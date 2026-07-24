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
//! # What this module does *not* do yet
//! - **Buffer tokens (4.3).** A request carrying a [`rayland_relay::WaylandArg::Buffer`] (the swapchain
//!   `wl_buffer`) cannot be replayed until the token is resolved to S's dma-buf; such a request is logged
//!   and skipped here.
//! - **Events back to the app (4.4).** Compositor events (`xdg_surface.configure`, dmabuf format events,
//!   frame callbacks) are received but not yet returned to the app. Without them a real client stalls
//!   before presenting; delivering them is the event-return path.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{OwnedFd, RawFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use rayland_relay::{WaylandArg, WaylandMessage};
use wayland_client::Proxy;
use wayland_client::backend::protocol::{Argument, Interface, Message};
use wayland_client::backend::{Backend, ObjectData, ObjectId};
use wayland_client::Connection;

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

/// One global S's compositor advertises: its registry `name` (S's numbering), interface, and max version.
struct GlobalEntry {
    /// S's registry numeric id for this global — the value `wl_registry.bind` takes.
    name: u32,
    /// The interface name (e.g. `"wl_compositor"`), matched against the app's forwarded binds.
    interface: String,
    /// The maximum version S's compositor advertises; a bind must not exceed it.
    version: u32,
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
/// It is inert for now: compositor events are the event-return path (Task 4.4). The one contract it must
/// honour is that an event carrying a `NewId` (the compositor creating an object) needs object data for
/// that new object — so it returns a fresh `ReplayObjectData` exactly then, mirroring the proxy's own
/// new-object handling on the server side.
struct ReplayObjectData;

impl ObjectData for ReplayObjectData {
    /// Handle one event on a replayed object. Events are not yet returned to the app (Task 4.4); this only
    /// honours the new-object contract so the backend has data for any object an event creates.
    fn event(
        self: Arc<Self>,
        _backend: &Backend,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData>> {
        // If the event creates an object, the backend needs data for it; otherwise none.
        let makes_object = msg
            .args
            .iter()
            .any(|arg| matches!(arg, Argument::NewId(_)));
        makes_object.then(|| Arc::new(ReplayObjectData) as Arc<dyn ObjectData>)
    }

    /// The object was destroyed; the id map cleanup is deferred to Task 4.4's event path.
    fn destroyed(&self, _object_id: ObjectId) {}
}

/// The S-side replay of one application's Wayland session.
///
/// Holds the connection to S's compositor, opened lazily on the first relayed message (bind or request),
/// so an offscreen session never touches a compositor. The `map` is the `app_id ↔ s_id` translation: keys
/// are ids in the app's id space (as they arrive on the wire), values are the S-side objects the replay
/// created on S's compositor.
pub struct WaylandReplay {
    /// The connection to S's compositor, `None` until the first relayed message opens it.
    conn: Option<Connection>,
    /// The compositor connection's backend, for `send_request`. Cloned from `conn` at connect.
    backend: Option<Backend>,
    /// S's `wl_registry` object, the sender of every `bind`. Set once at connect.
    registry: Option<ObjectId>,
    /// The globals S's compositor advertises, filled by [`RegistryData`] during the connect roundtrip.
    globals: Arc<Mutex<Vec<GlobalEntry>>>,
    /// The `app_id → S ObjectId` map: every object the app created (via bind or a request's `NewId`).
    map: HashMap<u32, ObjectId>,
}

impl WaylandReplay {
    /// Create an unconnected replay. No compositor connection is made until the first relayed message.
    pub fn new() -> Self {
        WaylandReplay {
            conn: None,
            backend: None,
            registry: None,
            globals: Arc::new(Mutex::new(Vec::new())),
            map: HashMap::new(),
        }
    }

    /// Ensure the compositor connection is up and the globals are enumerated. Returns `true` if connected.
    ///
    /// On first success it connects via `WAYLAND_DISPLAY`, sends `wl_display.get_registry`, and does a
    /// blocking roundtrip so [`Self::globals`] is populated before any bind is attempted. Idempotent: a
    /// later call is a cheap `is_some` check. A connection failure is logged and returns `false` — the
    /// caller drops the message, and the daemon's vtest/ring session is unaffected.
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
        // Block until the server has sent all its global advertisements, so binds can resolve.
        if let Err(e) = conn.roundtrip() {
            eprintln!("rayland-s: WP0 replay registry roundtrip failed: {e}");
            return false;
        }
        let count = self.globals.lock().unwrap().len();
        eprintln!("rayland-s: WP0 replay connected to S's compositor ({count} globals advertised)");
        self.conn = Some(conn);
        self.backend = Some(backend);
        self.registry = Some(registry);
        true
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
            Some(Arc::new(ReplayObjectData)),
            Some((iface, bind_version)),
        );
        match result {
            Ok(s_id) => {
                self.map.insert(app_object_id, s_id);
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
        let Some(sender) = self.map.get(&msg.object_id).cloned() else {
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
                    let obj = self.map.get(id).cloned().unwrap_or_else(ObjectId::null);
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
        // New objects need object data so the backend can route their future events.
        let data: Option<Arc<dyn ObjectData>> =
            new_app_id.map(|_| Arc::new(ReplayObjectData) as Arc<dyn ObjectData>);
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
                    self.map.insert(app_id, s_new_id);
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
        self.map.contains_key(&app_object_id)
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

impl Default for WaylandReplay {
    /// Same as [`WaylandReplay::new`]; provided because the type has an obvious empty state.
    fn default() -> Self {
        Self::new()
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
}
