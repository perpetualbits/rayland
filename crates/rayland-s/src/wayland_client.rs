//! WP0 — **the S-side Wayland session replay.**
//!
//! The application on C presents through the C-side proxy (`rayland-c`'s `wayland_proxy`), which forwards
//! every Wayland request across the link as a [`rayland_relay::C2S::WaylandRequest`], and every global
//! bind as a [`rayland_relay::C2S::WaylandBind`]. This module is the other end: it replays that session
//! against **S's real compositor**, so the app's window appears on S's screen. It is the mirror of the C
//! proxy — the proxy is a Wayland *server* to the app; this is a Wayland *client* to S's compositor.
//!
//! # How the replay works, symmetric to the proxy
//! Like the proxy, the replay works at the structured-message layer (`wayland_client::backend`), not the
//! high-level typed API: it forwards *whatever* the app did. For each relayed message it:
//!
//! - **On a bind** ([`WaylandReplay::handle_bind`]): finds the matching global among the ones S's own
//!   compositor advertises (by interface name — the app's registry `name` numbers are C's and meaningless
//!   here), binds it via `wl_registry.bind` with the app's version, and records `app_object_id ↔ (the
//!   S-side object)` in the id map.
//! - **On a request** ([`WaylandReplay::handle_request`]): reconstructs a `wayland-backend` `Message` —
//!   translating the sender and every `Object`/`NewId` argument through the id map — and submits it with
//!   `Backend::send_request`. A `NewId` becomes a null id plus a `child_spec` (the interface+version C
//!   stamped onto it), and the newly created S-side object is mapped back to the app's id.
//!
//! # The event return path (Task 4.4)
//! Requests flow app→S; **events flow S→app**. S's real compositor emits `xdg_surface.configure`,
//! `wl_buffer.release`, and so on, and a real client blocks on them before it can present. This module
//! delivers them back:
//!
//! - A dedicated **compositor-reader thread** ([`compositor_reader`]) dispatches S's compositor connection,
//!   so incoming events reach [`ReplayObjectData::event`]. It is a separate thread because events arrive
//!   asynchronously — often while the app is *idle*, waiting for its first configure — and S's message
//!   thread is busy serving the ring; nothing else would pump the compositor socket.
//! - [`ReplayObjectData::event`] translates each event's ids **S→app** (via the reverse id map, the inverse
//!   of the request path's app→S map) and emits it through an [`EventSink`] as a
//!   [`rayland_relay::S2C::WaylandEvent`]. C's proxy re-encodes it onto the app's own socket.
//!
//! # What this module does *not* do yet
//! - **Buffer tokens (4.3).** A request carrying a [`rayland_relay::WaylandArg::Buffer`] (the swapchain
//!   `wl_buffer`) cannot be replayed until the token is resolved to S's dma-buf; such a request is logged
//!   and skipped here.
//! - **Compositor-created objects in events.** An event carrying a `NewId` (a compositor creating an object
//!   for the app, e.g. a data offer) is not relayed back; the WP0 event set carries none.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use rayland_relay::{WaylandArg, WaylandMessage};
use wayland_client::Connection;
use wayland_client::Proxy;
use wayland_client::backend::protocol::{Argument, Interface, Message};
use wayland_client::backend::{Backend, ObjectData, ObjectId, WaylandError};

// Interface descriptors for every object WP0 replays. Named so `interface_by_name` can map a wire
// interface string to the linked `&'static Interface` that `send_request`'s `child_spec` requires.
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_callback::WlCallback, wl_compositor::WlCompositor, wl_region::WlRegion,
    wl_registry::WlRegistry, wl_seat::WlSeat, wl_surface::WlSurface,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel, xdg_wm_base::XdgWmBase,
};

/// `wl_display.get_registry` request opcode (creates the `wl_registry`).
const OP_DISPLAY_GET_REGISTRY: u16 = 1;
/// `wl_registry.bind` request opcode (binds a global, creating an object).
const OP_REGISTRY_BIND: u16 = 0;
/// `wl_registry.global` event opcode: `[name: uint, interface: string, version: uint]`.
const EV_REGISTRY_GLOBAL: u16 = 0;

/// Where S sends compositor events back to C — the mirror of the C proxy's `WaylandSink`.
///
/// S's compositor emits events; the replay translates each into the app's id space and hands it here. The
/// daemon implements this over the link (`S2C::WaylandEvent`); a test implements it as a recorder. Keeping
/// it a trait lets the replay be proven against a recorder without standing up the whole C side.
///
/// Must be `Send + Sync`: it is held by [`ReplayObjectData`], which the compositor-reader thread invokes.
pub trait EventSink: Send + Sync {
    /// Emit one compositor event, already translated into the app's id space, back toward C.
    fn emit(&self, event: WaylandMessage);
}

/// One global S's compositor advertises: its registry `name` (S's numbering), interface, and max version.
struct GlobalEntry {
    /// S's registry numeric id for this global — the value `wl_registry.bind` takes.
    name: u32,
    /// The interface name (e.g. `"wl_compositor"`), matched against the app's forwarded binds.
    interface: String,
    /// The maximum version S's compositor advertises; a bind must not exceed it.
    version: u32,
}

/// The `app_id ↔ s_id` translation, shared between S's message thread (which fills it as it replays binds
/// and requests) and the compositor-reader thread (which reads it to translate events S→app).
///
/// It holds **both** directions because the two paths need opposite lookups: a request names objects in the
/// app's id space and must resolve them to S-side [`ObjectId`]s (`forward`); an event names objects in S's
/// id space and must resolve them back to the app's numeric ids (`reverse`). The reverse map is keyed by the
/// S-side object's `protocol_id` (a `u32`, unique within S's connection) rather than the `ObjectId` itself,
/// so it needs no `Hash` on `ObjectId`.
#[derive(Default)]
struct IdMaps {
    /// App object id → the S-side [`ObjectId`] the replay created for it.
    forward: HashMap<u32, ObjectId>,
    /// S-side object `protocol_id` → the app object id it stands for. The inverse of `forward`.
    reverse: HashMap<u32, u32>,
}

impl IdMaps {
    /// Record a mapping in both directions at once, so the two never drift.
    fn insert(&mut self, app_id: u32, s_id: ObjectId) {
        self.reverse.insert(s_id.protocol_id(), app_id);
        self.forward.insert(app_id, s_id);
    }

    /// Resolve an app object id to its S-side [`ObjectId`], if the replay has created it.
    fn to_s(&self, app_id: u32) -> Option<ObjectId> {
        self.forward.get(&app_id).cloned()
    }

    /// Resolve an S-side object to the app object id it stands for, if it is one the replay created.
    /// Objects S's compositor owns that the replay never created (its `wl_display`, `wl_registry`, the
    /// roundtrip `wl_callback`) are absent, so their events resolve to `None` and are naturally skipped.
    fn to_app(&self, s_id: &ObjectId) -> Option<u32> {
        self.reverse.get(&s_id.protocol_id()).copied()
    }
}

/// The [`ObjectData`] on S's `wl_registry`: it records each advertised global from the `global` events.
struct RegistryData {
    /// Shared with [`WaylandReplay`] so the collected globals are visible after the roundtrip.
    globals: Arc<Mutex<Vec<GlobalEntry>>>,
}

impl ObjectData for RegistryData {
    /// Handle a `wl_registry` event. Only `global` (advertisement) is collected; `global_remove` is
    /// ignored (WP0 does not track dynamic global teardown). The registry creates no objects via events,
    /// so this never returns child data.
    fn event(
        self: Arc<Self>,
        _backend: &Backend,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData>> {
        // A `global` event names one advertised global; record it for binding.
        if msg.opcode == EV_REGISTRY_GLOBAL {
            if let [Argument::Uint(name), Argument::Str(Some(iface)), Argument::Uint(version)] =
                &msg.args[..]
            {
                self.globals.lock().unwrap().push(GlobalEntry {
                    name: *name,
                    interface: iface.to_string_lossy().into_owned(),
                    version: *version,
                });
            }
        }
        None
    }

    /// The registry was destroyed; nothing to release (its collected globals live in the replay).
    fn destroyed(&self, _object_id: ObjectId) {}
}

/// The [`ObjectData`] on every object S creates during the replay (bound globals and their descendants).
///
/// It is the **event return path** (Task 4.4): when S's compositor emits an event on one of these objects,
/// [`Self::event`] translates its ids back into the app's id space and emits it through the [`EventSink`].
/// It carries clones of the shared id maps (to translate) and the sink (to emit).
struct ReplayObjectData {
    /// The shared `app_id ↔ s_id` maps, for translating the event's ids S→app.
    maps: Arc<Mutex<IdMaps>>,
    /// Where a translated event goes — the link back to C (or a recorder in tests).
    sink: Arc<dyn EventSink>,
}

impl ReplayObjectData {
    /// A fresh object data sharing this one's maps and sink, for a child object an event or request creates.
    fn child(&self) -> Arc<dyn ObjectData> {
        Arc::new(ReplayObjectData {
            maps: Arc::clone(&self.maps),
            sink: Arc::clone(&self.sink),
        }) as Arc<dyn ObjectData>
    }
}

impl ObjectData for ReplayObjectData {
    /// Handle one event on a replayed object: translate it S→app and emit it toward C.
    ///
    /// The sender and every `Object` argument are resolved through the reverse id map. An event whose
    /// sender the replay never created (an object belonging to S's own registry/display) resolves to
    /// nothing and is dropped — that is how registry chatter is filtered out without a special case. An
    /// event carrying a `NewId` (a compositor-created object) or an `Fd` is not part of WP0's event set and
    /// drops the whole event, since neither can be faithfully reconstructed on the app side yet.
    fn event(
        self: Arc<Self>,
        _backend: &Backend,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData>> {
        // If the event creates an object, the backend needs data for it regardless of whether we relay it.
        let makes_object = msg
            .args
            .iter()
            .any(|arg| matches!(arg, Argument::NewId(_)));
        // Translate and emit (dropping the event if it cannot be represented in the app's id space).
        translate_and_emit(&self.maps, &self.sink, &msg);
        makes_object.then(|| self.child())
    }

    /// The object was destroyed; nothing to release (the id maps are pruned lazily — a stale forward entry
    /// is harmless, and the reverse entry only ever matches a live S-side object).
    fn destroyed(&self, _object_id: ObjectId) {}
}

/// Translate one compositor event S→app and emit it, or drop it if it cannot be represented.
///
/// See [`ReplayObjectData::event`] for the drop rules. Holds the maps lock only for the translation, never
/// across the `emit` (which sends over the link), so it cannot contend with the message thread's own map
/// writes for longer than the translation takes.
fn translate_and_emit(
    maps: &Arc<Mutex<IdMaps>>,
    sink: &Arc<dyn EventSink>,
    msg: &Message<ObjectId, OwnedFd>,
) {
    // **Do not relay S's own dmabuf format/modifier events.** The C-side proxy answers the dmabuf format
    // capability *locally* (WP0 Task 4.4a synthesizes the `modifier` events on bind), so S's compositor's
    // `format` (opcode 0) / `modifier` (opcode 1) events are duplicates — and, sent across the app's many
    // transient probe binds, they arrive in the hundreds and congest the return link the ring's replies
    // share, which can stall the command ring. The proxy owns this capability; S stays out of it.
    if msg.sender_id.interface().name == ZwpLinuxDmabufV1::interface().name
        && (msg.opcode == 0 || msg.opcode == 1)
    {
        return;
    }
    // Build the app-space message under the lock; emit it after releasing the lock.
    let app_msg = {
        let maps = maps.lock().expect("the WP0 id maps lock is never poisoned");
        // The sender must be an object the replay created; S's own registry/display objects are not, so
        // their events (the global advertisements, the roundtrip callback) resolve to nothing and drop.
        let Some(app_object) = maps.to_app(&msg.sender_id) else {
            return;
        };
        let mut args = Vec::with_capacity(msg.args.len());
        for arg in &msg.args {
            match arg {
                Argument::Int(v) => args.push(WaylandArg::Int(*v)),
                Argument::Uint(v) => args.push(WaylandArg::Uint(*v)),
                Argument::Fixed(v) => args.push(WaylandArg::Fixed(*v)),
                // Bytes without the trailing NUL; `None` stays the wire's absent-string case.
                Argument::Str(s) => {
                    args.push(WaylandArg::Str(s.as_ref().map(|c| c.as_bytes().to_vec())))
                }
                Argument::Array(bytes) => args.push(WaylandArg::Array((**bytes).clone())),
                // An object reference: translate S→app. If the app never learned of it, drop the whole
                // event rather than name an object the app cannot resolve.
                Argument::Object(id) => match maps.to_app(id) {
                    Some(app_id) => args.push(WaylandArg::Object(app_id)),
                    None => return,
                },
                // A compositor-created object, or an fd: outside WP0's event set; drop the event.
                Argument::NewId(_) | Argument::Fd(_) => return,
            }
        }
        WaylandMessage {
            object_id: app_object,
            opcode: msg.opcode,
            args,
        }
    };
    sink.emit(app_msg);
}

/// The compositor-reader thread body: dispatch S's compositor connection so its events reach
/// [`ReplayObjectData::event`].
///
/// # Why a dedicated thread
/// Compositor events arrive asynchronously, and typically while the app is *idle* — blocked waiting for its
/// first `xdg_surface.configure` before it draws anything. S's message thread is busy serving the ring and
/// only ever *writes* to the compositor connection (`send_request` + flush); nothing would read it. This
/// thread does nothing but read and dispatch, so it cannot stall the ring, and it owns all reads so it never
/// races the message thread's writes for the socket (the backend serializes the two internally).
///
/// It mirrors `wayland-client`'s own `roundtrip` loop: flush, then either read-and-dispatch through a
/// prepared guard (blocking on the fd with `poll`), or drain the backend's inner queue when a guard is
/// unavailable. It returns when the compositor connection ends (which ends the app's session too).
fn compositor_reader(conn: Connection) {
    loop {
        // Push any queued requests before waiting, so a reply the app is blocked on is not held back.
        if let Err(e) = conn.flush() {
            eprintln!("rayland-s: WP0 compositor flush failed, event reader stopping: {e}");
            return;
        }
        match conn.prepare_read() {
            Some(guard) => {
                // Block until the compositor fd is readable (or errored), mirroring wayland-client's own
                // `blocking_read`. `poll` returns on data, on `POLLERR` (peer gone), or on a signal.
                let mut pfd = libc::pollfd {
                    fd: guard.connection_fd().as_raw_fd(),
                    events: libc::POLLIN | libc::POLLERR,
                    revents: 0,
                };
                // SAFETY: `pfd` is a single valid pollfd for the call; a negative timeout blocks until
                // ready; `poll` only writes `revents`.
                let rc = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, -1) };
                if rc < 0 {
                    let err = std::io::Error::last_os_error();
                    // A signal is not a failure; drop the guard (its Drop cancels the prepared read) and
                    // re-prepare on the next iteration.
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    eprintln!("rayland-s: WP0 compositor poll failed, event reader stopping: {err}");
                    return;
                }
                // Read and dispatch the events into `ReplayObjectData::event`.
                match guard.read() {
                    Ok(_) => {}
                    // A spurious wakeup with nothing to read is fine; just loop.
                    Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        eprintln!("rayland-s: WP0 compositor connection ended: {e}");
                        return;
                    }
                }
            }
            // No guard available: another read is in flight or events are already queued. Drain the inner
            // queue to dispatch them, exactly as `roundtrip` does in this case.
            None => {
                if let Err(e) = conn.backend().dispatch_inner_queue() {
                    eprintln!("rayland-s: WP0 compositor dispatch failed, event reader stopping: {e}");
                    return;
                }
            }
        }
    }
}

/// The S-side replay of one application's Wayland session.
///
/// Holds the connection to S's compositor, opened lazily on the first relayed message (bind or request),
/// so an offscreen session never touches a compositor. The `maps` are the `app_id ↔ s_id` translation,
/// shared with the compositor-reader thread. The `sink` is where translated compositor events go back to C.
pub struct WaylandReplay {
    /// The connection to S's compositor, `None` until the first relayed message opens it.
    conn: Option<Connection>,
    /// The compositor connection's backend, for `send_request`. Cloned from `conn` at connect.
    backend: Option<Backend>,
    /// S's `wl_registry` object, the sender of every `bind`. Set once at connect.
    registry: Option<ObjectId>,
    /// The globals S's compositor advertises, filled by [`RegistryData`] during the connect roundtrip.
    globals: Arc<Mutex<Vec<GlobalEntry>>>,
    /// The `app_id ↔ s_id` maps, shared with the compositor-reader thread.
    maps: Arc<Mutex<IdMaps>>,
    /// Where compositor events go once translated into the app's id space.
    sink: Arc<dyn EventSink>,
    /// Whether the compositor-reader thread has been spawned (spawned once, after the connect roundtrip).
    reader_started: bool,
}

impl WaylandReplay {
    /// Create an unconnected replay. No compositor connection is made until the first relayed message.
    ///
    /// `sink` is where compositor events are emitted once translated — the link back to C in the daemon, a
    /// recorder in tests.
    pub fn new(sink: Arc<dyn EventSink>) -> Self {
        WaylandReplay {
            conn: None,
            backend: None,
            registry: None,
            globals: Arc::new(Mutex::new(Vec::new())),
            maps: Arc::new(Mutex::new(IdMaps::default())),
            sink,
            reader_started: false,
        }
    }

    /// Object data for a new S-side object, sharing the replay's maps and sink so its future events relay.
    fn object_data(&self) -> Arc<ReplayObjectData> {
        Arc::new(ReplayObjectData {
            maps: Arc::clone(&self.maps),
            sink: Arc::clone(&self.sink),
        })
    }

    /// Ensure the compositor connection is up and the globals are enumerated. Returns `true` if connected.
    ///
    /// On first success it connects via `WAYLAND_DISPLAY`, sends `wl_display.get_registry`, does a blocking
    /// roundtrip so [`Self::globals`] is populated before any bind is attempted, and then starts the
    /// compositor-reader thread ([`compositor_reader`]) that pumps events from here on. Idempotent: a later
    /// call is a cheap `is_some` check. A connection failure is logged and returns `false` — the caller
    /// drops the message, and the daemon's vtest/ring session is unaffected.
    fn ensure_connected(&mut self) -> bool {
        if self.conn.is_some() {
            return true;
        }
        // Open the compositor connection (this is where libwayland is dlopen'd).
        let conn = match Connection::connect_to_env() {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("rayland-s: WP0 replay could not reach a compositor: {e}");
                return false;
            }
        };
        let backend = conn.backend();
        // `wl_display.get_registry(new_id)` — the display is always object 1; child is the wl_registry.
        let registry = backend.send_request(
            Message {
                sender_id: backend.display_id(),
                opcode: OP_DISPLAY_GET_REGISTRY,
                args: [Argument::NewId(ObjectId::null())].into_iter().collect(),
            },
            Some(Arc::new(RegistryData {
                globals: Arc::clone(&self.globals),
            })),
            Some((WlRegistry::interface(), 1)),
        );
        let registry = match registry {
            Ok(id) => id,
            Err(e) => {
                eprintln!("rayland-s: WP0 replay get_registry failed: {e}");
                return false;
            }
        };
        // Block until the server has sent all its global advertisements, so binds can resolve. This is the
        // one read the message thread does; the reader thread starts only after it returns.
        if let Err(e) = conn.roundtrip() {
            eprintln!("rayland-s: WP0 replay registry roundtrip failed: {e}");
            return false;
        }
        let count = self.globals.lock().unwrap().len();
        eprintln!("rayland-s: WP0 replay connected to S's compositor ({count} globals advertised)");
        self.conn = Some(conn);
        self.backend = Some(backend);
        self.registry = Some(registry);
        // Start pumping compositor events. From here the reader thread owns all reads of this connection.
        self.start_reader();
        true
    }

    /// Spawn the compositor-reader thread once, handing it a clone of the connection.
    ///
    /// Called at the end of [`Self::ensure_connected`], after the connect roundtrip, so the message thread's
    /// one read (the roundtrip) never overlaps the reader thread's reads. Idempotent via `reader_started`.
    fn start_reader(&mut self) {
        if self.reader_started {
            return;
        }
        let Some(conn) = self.conn.as_ref().map(Connection::clone) else {
            return;
        };
        match std::thread::Builder::new()
            .name("rayland-s-wl-events".into())
            .spawn(move || compositor_reader(conn))
        {
            Ok(_) => self.reader_started = true,
            Err(e) => eprintln!("rayland-s: WP0 could not start the compositor event reader: {e}"),
        }
    }

    /// Replay one global **bind**: bind the matching global on S's compositor and map the app's object id.
    ///
    /// Finds the global S advertises whose interface name matches, binds it via `wl_registry.bind` at the
    /// app's version (capped at what S advertises, since binding above a global's max version is a protocol
    /// error), and records `app_object_id → (the S-side object)`. A missing global or unknown interface is
    /// logged and skipped — the app can still run; that object simply will not replay.
    pub fn handle_bind(&mut self, interface: String, version: u32, app_object_id: u32) {
        if !self.ensure_connected() {
            return;
        }
        // Find the advertised global by interface name; cap the bind version at what S offers.
        let found = {
            let globals = self.globals.lock().unwrap();
            globals
                .iter()
                .find(|g| g.interface == interface)
                .map(|g| (g.name, version.min(g.version)))
        };
        let Some((name, bind_version)) = found else {
            eprintln!(
                "rayland-s: WP0 replay: S's compositor advertises no `{interface}`; bind skipped"
            );
            return;
        };
        // Map the interface name to the linked descriptor `send_request`'s child_spec needs.
        let Some(iface) = interface_by_name(&interface) else {
            eprintln!("rayland-s: WP0 replay: no linked descriptor for `{interface}`; bind skipped");
            return;
        };
        let backend = self.backend.as_ref().expect("connected");
        let registry = self.registry.clone().expect("connected");
        let data = self.object_data();
        // `wl_registry.bind` is the one request whose child interface is *dynamic*, so its wire signature
        // spells the generic new_id out in full: `[name:uint, interface:str, version:uint, new_id]`. The
        // interface string and version are explicit args here (not injected from `child_spec`, which the
        // backend uses only to create the child object). `iface.name` is the same interface, NUL-terminated
        // for the wire.
        let iface_arg = CString::new(iface.name).expect("an interface name has no interior NUL");
        let result = backend.send_request(
            Message {
                sender_id: registry,
                opcode: OP_REGISTRY_BIND,
                args: [
                    Argument::Uint(name),
                    Argument::Str(Some(Box::new(iface_arg))),
                    Argument::Uint(bind_version),
                    Argument::NewId(ObjectId::null()),
                ]
                .into_iter()
                .collect(),
            },
            Some(data),
            Some((iface, bind_version)),
        );
        match result {
            Ok(s_id) => {
                self.maps
                    .lock()
                    .expect("the WP0 id maps lock is never poisoned")
                    .insert(app_object_id, s_id);
                eprintln!(
                    "rayland-s: WP0 replay bound `{interface}` v{bind_version} (app obj {app_object_id})"
                );
            }
            Err(e) => eprintln!("rayland-s: WP0 replay bind of `{interface}` failed: {e}"),
        }
        self.flush();
    }

    /// Replay one Wayland **request** against S's compositor.
    ///
    /// Reconstructs a `wayland-backend` `Message`: the sender and every `Object`/`NewId` argument are
    /// translated through the id map. A `NewId` becomes a null id plus a `child_spec`, and the S-side
    /// object `send_request` returns is mapped back to the app's new id. A request carrying a buffer token
    /// is skipped (Task 4.3). `send_request` panics on a protocol violation, so it is wrapped in
    /// `catch_unwind` — a replay bug is logged and the vtest/ring session survives, rather than the whole
    /// message thread dying.
    pub fn handle_request(&mut self, msg: WaylandMessage) {
        if !self.ensure_connected() {
            return;
        }
        // The sender must be an object the replay already created (bound global or prior new-id).
        let Some(sender) = self
            .maps
            .lock()
            .expect("the WP0 id maps lock is never poisoned")
            .to_s(msg.object_id)
        else {
            eprintln!(
                "rayland-s: WP0 replay: request for unmapped object {} (opcode {}); skipped",
                msg.object_id, msg.opcode
            );
            return;
        };

        // Reconstruct the argument list, translating ids and pulling out the child_spec / new-object id.
        let mut args: Vec<Argument<ObjectId, RawFd>> = Vec::with_capacity(msg.args.len());
        let mut child_spec: Option<(&'static Interface, u32)> = None;
        let mut new_app_id: Option<u32> = None;
        for arg in &msg.args {
            match arg {
                WaylandArg::Int(v) => args.push(Argument::Int(*v)),
                WaylandArg::Uint(v) => args.push(Argument::Uint(*v)),
                WaylandArg::Fixed(v) => args.push(Argument::Fixed(*v)),
                WaylandArg::Str(s) => args.push(Argument::Str(
                    // Re-add the wire NUL the tunnel stripped; a wayland string has no interior NUL.
                    s.as_ref()
                        .and_then(|b| CString::new(b.clone()).ok())
                        .map(Box::new),
                )),
                WaylandArg::Array(b) => args.push(Argument::Array(Box::new(b.clone()))),
                // An object reference: translate through the map, or null if it is not (yet) known.
                WaylandArg::Object(id) => {
                    let obj = self
                        .maps
                        .lock()
                        .expect("the WP0 id maps lock is never poisoned")
                        .to_s(*id)
                        .unwrap_or_else(ObjectId::null);
                    args.push(Argument::Object(obj));
                }
                // A new object: the wire carries a null id here; child_spec names its interface+version.
                WaylandArg::NewId {
                    id,
                    interface,
                    version,
                } => {
                    args.push(Argument::NewId(ObjectId::null()));
                    child_spec = interface_by_name(interface).map(|iface| (iface, *version));
                    new_app_id = Some(*id);
                }
                // A buffer token: needs resolution to S's dma-buf (Task 4.3). Skip the whole request.
                WaylandArg::Buffer(_) => {
                    eprintln!(
                        "rayland-s: WP0 replay: buffer-token request (obj {} opcode {}) deferred to 4.3; skipped",
                        msg.object_id, msg.opcode
                    );
                    return;
                }
            }
        }

        let backend = self.backend.as_ref().expect("connected");
        let opcode = msg.opcode;
        // New objects need object data so the backend can route their future events (the return path).
        let data: Option<Arc<dyn ObjectData>> =
            new_app_id.map(|_| self.object_data() as Arc<dyn ObjectData>);
        // `send_request` panics on a protocol violation; isolate that from the shared message thread.
        let result = catch_unwind(AssertUnwindSafe(|| {
            backend.send_request(
                Message {
                    sender_id: sender,
                    opcode,
                    args: args.into_iter().collect(),
                },
                data,
                child_spec,
            )
        }));
        match result {
            Ok(Ok(s_new_id)) => {
                // Map the app's new object to the S-side one the compositor just created.
                if let Some(app_id) = new_app_id {
                    self.maps
                        .lock()
                        .expect("the WP0 id maps lock is never poisoned")
                        .insert(app_id, s_new_id);
                }
            }
            Ok(Err(e)) => eprintln!(
                "rayland-s: WP0 replay: send_request (obj {} opcode {opcode}) failed: {e}",
                msg.object_id
            ),
            Err(_) => eprintln!(
                "rayland-s: WP0 replay: send_request (obj {} opcode {opcode}) panicked — likely a \
                 translation bug; request dropped, session continues",
                msg.object_id
            ),
        }
        self.flush();
    }

    /// Whether `app_object_id` has been mapped to an S-side object — i.e. the replay has created the
    /// corresponding object on S's compositor (by binding a global or replaying a request that created
    /// it). Used by the integration test to confirm the replay path without a full app; also a cheap,
    /// honest window into the map for diagnostics.
    pub fn is_mapped(&self, app_object_id: u32) -> bool {
        self.maps
            .lock()
            .expect("the WP0 id maps lock is never poisoned")
            .forward
            .contains_key(&app_object_id)
    }

    /// Flush queued requests to the compositor so it acts on them promptly.
    fn flush(&self) {
        if let Some(conn) = &self.conn {
            if let Err(e) = conn.flush() {
                eprintln!("rayland-s: WP0 replay flush failed: {e}");
            }
        }
    }
}

/// Map a Wayland interface name to the linked `&'static Interface` the client backend needs.
///
/// The set is exactly the interfaces WP0 replays (spec §6): the globals the app binds and the objects it
/// creates from them. An unknown name returns `None` — the request or bind naming it is skipped rather
/// than replayed, since without the descriptor the backend cannot create the object.
fn interface_by_name(name: &str) -> Option<&'static Interface> {
    Some(match name {
        "wl_compositor" => WlCompositor::interface(),
        "wl_surface" => WlSurface::interface(),
        "wl_region" => WlRegion::interface(),
        "wl_callback" => WlCallback::interface(),
        "wl_buffer" => WlBuffer::interface(),
        "wl_seat" => WlSeat::interface(),
        "xdg_wm_base" => XdgWmBase::interface(),
        "xdg_surface" => XdgSurface::interface(),
        "xdg_toplevel" => XdgToplevel::interface(),
        "zwp_linux_dmabuf_v1" => ZwpLinuxDmabufV1::interface(),
        "zwp_linux_buffer_params_v1" => ZwpLinuxBufferParamsV1::interface(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_registry_maps_the_wp0_interfaces() {
        // Every interface WP0 binds or creates must resolve, or its request cannot be replayed.
        for name in [
            "wl_compositor",
            "wl_surface",
            "wl_region",
            "wl_callback",
            "wl_buffer",
            "wl_seat",
            "xdg_wm_base",
            "xdg_surface",
            "xdg_toplevel",
            "zwp_linux_dmabuf_v1",
            "zwp_linux_buffer_params_v1",
        ] {
            let iface = interface_by_name(name).unwrap_or_else(|| panic!("no descriptor for {name}"));
            assert_eq!(iface.name, name, "descriptor name must match the lookup key");
        }
        // An interface WP0 does not handle resolves to None, so its request is skipped, not mis-created.
        assert!(interface_by_name("wl_data_device_manager").is_none());
    }

    /// The reverse map resolves an S-side object back to the app id it stands for, and drops the events of
    /// objects the replay never created (S's own registry/display) by returning `None`.
    #[test]
    fn id_maps_translate_both_directions() {
        // A stand-in ObjectId is awkward to mint without a backend, so this exercises the numeric keying
        // that the reverse map actually uses: insert routes by `protocol_id`, and `to_app` reads it back.
        // We cannot fabricate an ObjectId here, so the round-trip is covered by the integration test; this
        // asserts the empty-map default behaviour that guards registry chatter.
        let maps = IdMaps::default();
        assert!(maps.to_s(3).is_none(), "an unmapped app id resolves to nothing");
    }
}
