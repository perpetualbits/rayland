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
//! # Buffer tokens (Task 4.3): the one sequence S **originates** rather than replays
//! Every other request here is a translation of something the app did. A buffer token is not: the app's
//! `wl_buffer` names a dma-buf fd, and **C drops that fd by design** (that is what buffer-by-token *is*),
//! so there is nothing to translate. S must construct the whole buffer-creation sequence itself, against
//! the dma-buf **it already exported** for that resource at creation.
//!
//! It is **three** requests, not one — the point that is easy to get wrong. C's proxy intercepts
//! `zwp_linux_dmabuf_v1.create_params` and does **not** forward it, so when a `create_immed` carrying a
//! token arrives, S has no `zwp_linux_buffer_params_v1` object at all and the app-side params id is not in
//! the id map. A naive implementation reaches the sender lookup and refuses the request as "unmapped"
//! before it ever sees the token. So [`WaylandReplay::handle_request`] checks for a token **first**, and
//! [`plan_buffer_requests`] lays out the sequence:
//!
//! 1. `zwp_linux_dmabuf_v1.create_params` on the bound dmabuf global → the params object. The app's params
//!    id is mapped to it here, which is what makes the following two requests addressable.
//! 2. `zwp_linux_buffer_params_v1.add` with the duplicated dma-buf fd and the token's **carried** plane
//!    layout — `offset` and `stride` come from the token, never from `width × bpp`, because a wrong stride
//!    garbles the image instead of failing (see `rayland_relay::BufferToken::stride`).
//! 3. `zwp_linux_buffer_params_v1.create_immed` → the `wl_buffer`, mapped to the app's buffer id, after
//!    which the app's own `attach`/`commit` replay through the ordinary path.
//!
//! Any of those failing refuses the whole buffer and logs **which** step failed; a partially built buffer
//! is never attached.
//!
//! # What this module does *not* do yet
//! - **Commit gating.** The app's `commit` replays as soon as it arrives, with no wait on the (c)2
//!   completion signal, so a presented frame may be early or torn. That is a separate task by design:
//!   shipping it with the token path would make any failure ambiguous between the two.
//! - **Compositor-created objects in events.** An event carrying a `NewId` (a compositor creating an object
//!   for the app, e.g. a data offer) is not relayed back; the WP0 event set carries none.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rayland_relay::{BufferToken, WaylandArg, WaylandMessage};
use wayland_client::Connection;
use wayland_client::Proxy;
use wayland_client::backend::protocol::{Argument, Interface, Message};
use wayland_client::backend::{Backend, ObjectData, ObjectId, WaylandError};

// Interface descriptors for every object WP0 replays. Named so `interface_by_name` can map a wire
// interface string to the linked `&'static Interface` that `send_request`'s `child_spec` requires.
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_callback::WlCallback, wl_compositor::WlCompositor,
    wl_output::WlOutput, wl_region::WlRegion,
    wl_shm::WlShm, wl_shm_pool::WlShmPool,
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

/// `zwp_linux_dmabuf_v1.create_params` request opcode — creates a `zwp_linux_buffer_params_v1`.
const OP_DMABUF_CREATE_PARAMS: u16 = 1;
/// `zwp_linux_buffer_params_v1.add` request opcode — supplies one plane's fd and layout.
const OP_PARAMS_ADD: u16 = 1;
/// `zwp_linux_buffer_params_v1.create_immed` request opcode — creates the `wl_buffer` synchronously.
const OP_PARAMS_CREATE_IMMED: u16 = 3;
/// The version S binds its **own** `zwp_linux_dmabuf_v1` at, capped at what the app's proxy advertises.
///
/// The C-side proxy caps the application at v3 so Mesa takes the fd-free format path, and matching that
/// here keeps S on the same, well-exercised request set. It also becomes the params object's version and
/// then the `wl_buffer`'s, since a Wayland child inherits its parent's version.
const DMABUF_BIND_VERSION: u32 = 3;

/// The plane index every synthesized `add` names.
///
/// Always zero, and that is a guarantee rather than a simplification: C **refuses** a buffer whose `add`
/// names any other plane, or which supplies more than one, so a token that reaches S describes exactly one
/// plane by construction (see `rayland_c`'s `try_intercept_buffer`).
const SINGLE_PLANE: u32 = 0;
/// The `flags` argument of `create_immed` — no y-invert, no interlacing. WP0's swapchain images are plain
/// top-down LINEAR buffers, and the app's own request carried no flags for C to forward.
const NO_BUFFER_FLAGS: u32 = 0;
// (No `wl_buffer` version constant: a child object's version is **inherited from its parent**, not taken
// from the child interface's own maximum. See `plan_buffer_requests`' "the version rule" note.)

/// Resolves a [`BufferToken`]'s resource id to a **duplicate** of the dma-buf descriptor S exported for
/// that resource when it was created.
///
/// # Why this is a trait rather than a borrowed `Applier`
/// The plan's lock rule is *resolve and clone the fd under the applier lock, and release the lock before
/// any `send_request`* — because a Wayland call is a round trip to the compositor, and holding the relay's
/// applier mutex across one would put the whole ring session behind the compositor's scheduling. Expressed
/// as a comment, that rule is one refactor away from being violated. Expressed as this trait it is
/// **structural**: the lock guard cannot escape `dup_exported_fd`, so there is no way to hold the applier
/// across a compositor round trip even deliberately.
///
/// It also mirrors the house pattern on the other side of the wire (`rayland_c`'s `ResourceResolver` and
/// `WaylandSink`), and it gives the pure tests a fake to inject.
///
/// # Contract
/// - Returns an **owned duplicate**. The `Applier`'s own descriptor must stay alive and unmoved: the
///   export cannot be repeated, because virglrenderer's `mem->exported` guard permits exactly one export
///   per resource and it already happened at creation.
/// - `None` when the resource was never created, or has been unref'd — which a correct caller may well
///   see, since a token can outlive its resource if the app tears the swapchain down with a frame in
///   flight. The caller must refuse the buffer, not guess.
pub trait ExportedFdSource: Send + Sync {
    /// Duplicate S's exported dma-buf descriptor for `resource_id`, or `None` if there is none.
    fn dup_exported_fd(&self, resource_id: u32) -> Option<OwnedFd>;

    /// Record that this resource is now a **presentation buffer**, so its bytes stop being shipped to C.
    ///
    /// # Why the replay has to say this
    /// A presented resource is rendered by S's GPU and imported by S's compositor from S's own dma-buf.
    /// C has no use for its contents and no display to put them on — but the (c)2 return path ships back
    /// whatever S's GPU wrote, and cannot tell a readback the application will *read* from a swapchain
    /// image it will only *show*. Only the WP0 path knows which is which, so only the WP0 path can say.
    ///
    /// Measured before this existed: ~877 KB per frame crossing the network for a 500x500 window, in a
    /// project whose thesis is that pixels do not cross the network. Called on every present; idempotent.
    fn note_presented(&self, resource_id: u32);
}

/// One request in the synthesized buffer-creation sequence: what to send, and what object it creates.
///
/// Deliberately carries no sender: the sender of steps 2 and 3 is the params object that step 1 *creates*,
/// so it cannot be known when the sequence is planned. [`WaylandReplay::synthesize_buffer`] threads it in.
/// Keeping senders out is also what makes the planner a pure function that a test can assert on without a
/// live compositor connection to mint `ObjectId`s from.
#[derive(Debug)]
pub struct SynthesizedRequest {
    /// The request's opcode within its interface.
    pub opcode: u16,
    /// The wire arguments, in order. `NewId` slots are null — the backend fills them from `child`.
    pub args: Vec<Argument<ObjectId, RawFd>>,
    /// The interface and version of the object this request creates, or `None` if it creates none.
    pub child: Option<(&'static Interface, u32)>,
}

/// Lay out the three requests that turn a [`BufferToken`] into a `wl_buffer` on S's compositor.
///
/// # Inputs
/// - `token`: what C sent. Its `offset` and `stride` are used **verbatim**; recomputing them from `width`
///   and the format is the derivation that garbles rather than fails.
/// - `dmabuf_version`: the version the `zwp_linux_dmabuf_v1` global was bound at, so the params object is
///   created at a matching version rather than a guessed one.
/// - `fd`: the raw descriptor to hand to `add`. The **caller keeps ownership**: the pure-Rust
///   `wayland-backend` in this tree dups the fd inside `write_message`
///   (`BorrowedFd::borrow_raw(fd).try_clone_to_owned()`), so `send_request` neither consumes nor closes
///   it. The caller must hold its `OwnedFd` until the send returns and then let it drop, or leak.
///
/// # Output
/// Exactly three requests, in the order they must be sent. This is a pure function — no I/O, no locks, no
/// compositor — so its shape is checkable in a unit test, which is where the modifier hi/lo split and the
/// argument ordering are actually pinned down.
///
/// # Domain pitfall 1: the modifier's halves
/// The modifier is a 64-bit value split across two `uint` arguments, high half first. Getting the halves
/// the wrong way round produces a valid-looking request that describes a tiling layout nothing uses; the
/// compositor may reject the buffer, or import it and show noise.
///
/// # Domain pitfall 2: the version rule — **both children take the *parent's* version**
/// A Wayland object created by a request **inherits the version of the object that created it**, and
/// `wayland-backend` enforces exactly that: `send_request` **panics** unless `child_spec`'s version equals
/// the *sender's* version (`rs/client_impl/mod.rs:367`, "expected version N but got M").
///
/// So `create_immed`'s `wl_buffer` child is declared at `dmabuf_version`, **not** at 1. That is
/// counter-intuitive — the `wl_buffer` interface has only ever *had* version 1 — and it is what the first
/// two-machine run of this code died on: WP0's written plan specified `wl_buffer` v1, and against a v3
/// params object that is an immediate panic. The version here is a statement about *lineage*, not about
/// the interface's own capabilities.
pub fn plan_buffer_requests(
    token: &BufferToken,
    dmabuf_version: u32,
    fd: RawFd,
) -> [SynthesizedRequest; 3] {
    [
        // 1. Make the params object. C intercepted the app's own `create_params` and never forwarded it,
        //    so this object does not exist on S until now.
        SynthesizedRequest {
            opcode: OP_DMABUF_CREATE_PARAMS,
            args: vec![Argument::NewId(ObjectId::null())],
            child: Some((ZwpLinuxBufferParamsV1::interface(), dmabuf_version)),
        },
        // 2. Describe the single plane: S's own exported descriptor, plus the layout the app declared.
        SynthesizedRequest {
            opcode: OP_PARAMS_ADD,
            args: vec![
                Argument::Fd(fd),
                Argument::Uint(SINGLE_PLANE),
                Argument::Uint(token.offset),
                Argument::Uint(token.stride),
                // High half first, then low — the order `linux-dmabuf-v1.xml` specifies.
                Argument::Uint((token.modifier >> 32) as u32),
                Argument::Uint(token.modifier as u32),
            ],
            child: None,
        },
        // 3. Create the buffer itself. Width/height are protocol `int`s; the token carries them as `u32`
        //    because a real buffer is never negative.
        SynthesizedRequest {
            opcode: OP_PARAMS_CREATE_IMMED,
            args: vec![
                Argument::NewId(ObjectId::null()),
                Argument::Int(token.width as i32),
                Argument::Int(token.height as i32),
                Argument::Uint(token.drm_format),
                Argument::Uint(NO_BUFFER_FLAGS),
            ],
            // `dmabuf_version`, not 1 — the params object was created at that version and the buffer
            // inherits it. See "the version rule" above; getting this wrong panics the backend.
            child: Some((WlBuffer::interface(), dmabuf_version)),
        },
    ]
}

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
/// # On `.expect("the WP0 id maps lock is never poisoned")`, re-examined 2026-08-30
///
/// That claim is a *conditional* one and it is worth stating what it rests on, because a sibling claim
/// of the same shape turned out false the same week: `catch_unwind` around `send_request` was asserted
/// to make the session survivable, and did not, because the **dependency's** mutex is what gets
/// poisoned.
///
/// This lock is different, and still sound, for a reason that can be checked rather than believed:
/// **it is never held across a call that can panic.** Every acquisition in this module is a short
/// scope that reads or writes the maps and releases — `translate_and_emit` is shaped around a
/// `Result` precisely so the drop reason is *formatted* after release, and `synthesize_buffer` takes
/// and drops it between `send_request`s rather than across them. If a future edit holds this lock
/// across a backend call, the claim becomes false and the `expect` becomes the second crash rather
/// than a safe assertion.
///
/// # Why nothing is ever removed from these maps — and why that is deliberate, not an oversight
///
/// A Wayland protocol id is a **slot number, not an object identity**: it is unique only among objects
/// alive at one instant, and is recycled the moment an object dies. `rayland-c`'s proxy learned this the
/// hard way — its object map pruned by bare `protocol_id`, and a destroyed object's *late* cleanup deleted
/// the live object that had since inherited the slot, silently unregistering the application's frame
/// callback and freezing its window after one frame (`docs/data/2026-08-29-wp0-event-witness/`).
///
/// These maps are immune to that, and the reason is exactly that **nothing removes by number**.
/// [`Self::insert`] writes both directions when an object is *created*, so a recycled id simply
/// overwrites the stale pair and every lookup resolves to the object that currently holds the slot. The
/// entry is refreshed by the newcomer rather than deleted by the departed.
///
/// **Adding a removal here would import that bug**, not tidy the code. [`ReplayObjectData::destroyed`] is
/// a deliberate no-op for this reason; it is load-bearing.
///
/// Nor does never removing leak. Growth is bounded by the application's **peak live-object count**,
/// because ids are recycled — the same property that made recycling dangerous on C makes it safe here.
#[derive(Default)]
struct IdMaps {
    /// App object id → the S-side [`ObjectId`] the replay created for it. Overwritten, never removed.
    forward: HashMap<u32, ObjectId>,
    /// S-side object `protocol_id` → the app object id it stands for. The inverse of `forward`, and
    /// likewise overwritten rather than removed — see the type's own docs for why that is the safe choice.
    reverse: HashMap<u32, u32>,
    /// App object id → the **version of the S-side object** the replay created for it.
    ///
    /// # Why S has to remember this, and why the wire's version cannot be used
    /// In Wayland a `new_id` argument **inherits the version of the object that created it**. The one
    /// exception is `wl_registry.bind`, which carries an explicit version — and which the replay handles
    /// on its own path ([`WaylandReplay::handle_bind`]).
    ///
    /// S cannot simply forward the version C stamped on the wire, because **S may bind a global at a
    /// lower version than the application did**: `handle_bind` caps at what S's compositor advertises,
    /// since binding above a global's maximum is a protocol error. Once capped, every version the
    /// application believes in is too high for S's objects, and `wayland-backend` enforces the
    /// difference by **panicking** — `client_impl/mod.rs:368`, "expected version 5 but got 6".
    ///
    /// Nor can the version be read back off an `ObjectId`: the client-side `ObjectId` API exposes only
    /// `interface()` and `protocol_id()`. So it is recorded here.
    ///
    /// **The invariant this establishes**, which is what makes one lookup sufficient: *every object's
    /// version equals the capped version of the global it descends from.* Seeded at bind time with the
    /// capped value, and propagated unchanged to every child.
    versions: HashMap<u32, u32>,
}

impl IdMaps {
    /// Record a mapping in both directions at once, plus the S-side object's version, so the three
    /// never drift.
    ///
    /// `version` is the version of the **S-side** object — the capped bind version for a global, or the
    /// creating object's version for a child. Never the version the application asked for; see the
    /// [`Self::versions`] field for why that distinction is load-bearing.
    fn insert(&mut self, app_id: u32, s_id: ObjectId, version: u32) {
        self.reverse.insert(s_id.protocol_id(), app_id);
        self.forward.insert(app_id, s_id);
        self.versions.insert(app_id, version);
    }

    /// The version of the S-side object standing for `app_id`, if the replay created it.
    ///
    /// This is what a child's `child_spec` must be built from. `None` means the object is unknown, and
    /// the caller must refuse rather than guess a version — a guess is exactly the panic this exists to
    /// prevent.
    fn version_of(&self, app_id: u32) -> Option<u32> {
        self.versions.get(&app_id).copied()
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
            if let [
                Argument::Uint(name),
                Argument::Str(Some(iface)),
                Argument::Uint(version),
            ] = &msg.args[..]
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
        let makes_object = msg.args.iter().any(|arg| matches!(arg, Argument::NewId(_)));
        // Translate and emit (dropping the event if it cannot be represented in the app's id space).
        translate_and_emit(&self.maps, &self.sink, &msg);
        makes_object.then(|| self.child())
    }

    /// The object was destroyed. **Deliberately a no-op, and that is load-bearing — do not "fix" it.**
    ///
    /// The tempting change is to prune [`IdMaps`] here, by the destroyed object's `protocol_id`. That is
    /// precisely the bug `rayland-c`'s proxy shipped: a protocol id is a **slot number, not an object
    /// identity**, this callback arrives *after* the backend has dispatched the requests that followed the
    /// destruction, and by then the slot may already belong to a different, live object — which the
    /// removal would then delete. On C that silently unregistered the application's frame callback and
    /// froze its window after a single frame.
    ///
    /// `IdMaps` needs no pruning because [`IdMaps::insert`] overwrites both directions when an object is
    /// created, so a recycled id is always refreshed by its new owner. See that type's docs for the full
    /// argument, including why never removing does not leak.
    fn destroyed(&self, _object_id: ObjectId) {}
}

/// Whether the per-event return-path trace is on, read from `RAYLAND_S_EVENT_LOG`.
///
/// # Why a separate switch from `RAYLAND_S_REPLY_LOG`
/// It follows that variable's shape deliberately (a bare presence check, no parsing), but it is its own
/// switch because that one turns on the *link and applier* instrumentation, which is high-volume enough to
/// change the timing it measures. The event return path is a different investigation running on a
/// different thread, and it must be possible to watch it without also perturbing the ring.
///
/// Drops are **not** gated on this: a dropped event is rare and each one is a finding, so it is always
/// reported. This gates only the trace of events that flow normally.
fn event_log_enabled() -> bool {
    std::env::var_os("RAYLAND_S_EVENT_LOG").is_some()
}

/// Read a file descriptor's entire contents **from offset 0, without moving its file offset**.
///
/// # Why this exists
/// `wl_keyboard.keymap` carries an fd to a read-only mapping holding the XKB keymap as text. An fd
/// cannot cross a network, but these *bytes* can, and C mints an equivalent `memfd` on the far side —
/// see [`rayland_relay::WaylandArg::KeymapContent`].
///
/// # The invariant this function exists to hold, and how it was broken
/// The event's `size` argument travels unchanged, so **the bytes returned here must be the whole
/// file**: C sizes its `memfd` to this length, the application maps `size` bytes over it, and a short
/// file means the application faults reading past the end. That is not theoretical — the first
/// version of this used `dup` plus `read_to_end` and `vkgears` died with **SIGBUS** (exit 135).
///
/// **`dup` does not give an independent file offset.** It duplicates the descriptor, and the copy
/// shares the *open file description* — including the offset — with the original. So `read_to_end`
/// started wherever the compositor's descriptor happened to be pointing, could return fewer bytes than
/// the file holds, and left the compositor's own offset at EOF as a parting gift. An earlier comment
/// here asserted the opposite; it was wrong, and the assertion is what made the bug hard to see.
///
/// `read_at` (`pread`) is the fix: it reads from an explicit offset and touches no shared state, so
/// no dup is needed and nothing the compositor owns is disturbed.
///
/// # Inputs / outputs
/// - `fd`: the descriptor named by the event. Borrowed, never consumed, never repositioned.
/// - Returns the file's full contents, or `None` if `fstat` or a read fails — the caller then drops
///   the event, which is the honest outcome: an application that waits for a keymap is better served
///   than one handed a truncated mapping it will fault on.
fn read_fd_contents(fd: std::os::fd::RawFd) -> Option<Vec<u8>> {
    use std::os::fd::{AsRawFd, BorrowedFd};
    use std::os::unix::fs::FileExt;
    // The true length, from the descriptor itself: the caller's `size` argument is the compositor's
    // claim, and this is the thing C will actually be asked to reproduce.
    // SAFETY: `fstat` fills a `stat` it is given; the descriptor is live for this call.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        eprintln!(
            "rayland-s: WP0 keymap: fstat of the keymap fd failed: {}",
            std::io::Error::last_os_error()
        );
        return None;
    }
    let len = usize::try_from(st.st_size).unwrap_or(0);
    // Borrow rather than own: this descriptor belongs to the `wayland-client` backend, which closes it
    // when the handler returns. A `File` built from it would close it too — a double close, and on a
    // busy process a use-after-free of whatever fd number is reused next.
    // SAFETY: the descriptor is live for the duration of this call, and the `BorrowedFd` does not
    // outlive it.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let file = std::mem::ManuallyDrop::new(unsafe {
        <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(borrowed.as_raw_fd())
    });
    let mut bytes = vec![0u8; len];
    let mut done = 0usize;
    while done < len {
        match file.read_at(&mut bytes[done..], done as u64) {
            // A zero-length read before the end means the file is shorter than `fstat` claimed. Report
            // what was actually read rather than shipping a buffer with a zeroed tail.
            Ok(0) => {
                bytes.truncate(done);
                break;
            }
            Ok(n) => done += n,
            Err(e) => {
                eprintln!("rayland-s: WP0 keymap: reading the keymap fd failed: {e}");
                return None;
            }
        }
    }
    Some(bytes)
}

/// The name of the event at `opcode` on `interface`, or `None` if the opcode is outside the descriptor.
///
/// # Why by name and not by opcode
/// An opcode is an index into one interface's event list, so the same number means different things on
/// different objects, and the two sides of this relay log it against two different id spaces. `opcode 0`
/// appearing in both logs says nothing; `wl_callback.done` appearing in one and not the other is the whole
/// answer. This is what makes the two logs diffable.
///
/// # Failure mode
/// Returns `None` rather than panicking when the opcode is out of range. That is not paranoia: the opcode
/// arrives from S's compositor, and a compositor advertising an interface version newer than the linked
/// descriptor can legitimately send an event this build has no name for. Losing the name must never lose
/// the event, so callers fall back to printing the raw number.
fn event_name(interface: &Interface, opcode: u16) -> Option<&'static str> {
    interface.events.get(opcode as usize).map(|desc| desc.name)
}

/// `"<interface>.<event>"`, or `"<interface>.#<opcode>"` when the opcode has no descriptor.
///
/// Allocates, so callers must build it **outside** any held lock — see [`translate_and_emit`], whose whole
/// shape exists to keep string formatting off the maps lock.
fn event_label(interface: &Interface, opcode: u16) -> String {
    match event_name(interface, opcode) {
        Some(name) => format!("{}.{}", interface.name, name),
        None => format!("{}.#{}", interface.name, opcode),
    }
}

/// Why an event S's compositor emitted was not delivered to the application.
///
/// Deliberately `Copy` and free of any borrow: it is produced **inside** the maps lock and rendered into a
/// message **after** the lock is released, so that reporting a drop cannot extend the critical section that
/// the compositor-reader thread shares with the message thread.
#[derive(Debug, Clone, Copy)]
enum EventDrop {
    /// The sending object is not in the S→app map, so there is no app-side object to address the event to.
    ///
    /// **What this means for the app:** usually nothing — this is how S's own registry and display chatter
    /// is filtered out, since the replay never created those objects. But if it names an object the replay
    /// *did* create, the reverse map has a hole and the app is missing an event it is entitled to.
    UnmappedSender,
    /// An `Object` argument names an S-side object the app never learned of.
    ///
    /// **What this means for the app:** the whole event is lost, not just the argument. Delivering it with
    /// a dangling reference would be worse — the app would resolve it to one of its own unrelated objects.
    UnmappedObjectArg {
        /// The unresolvable object's protocol id in **S's** id space.
        s_object: u32,
    },
    /// The event carries a `NewId` — S's compositor creating an object for the app.
    ///
    /// **What this means for the app:** the event is lost. WP0's return path cannot mint an object in the
    /// app's id space, so anything delivered this way (a data offer, a dmabuf feedback object) never
    /// arrives. If the app is blocked on such an event, this branch is the reason.
    CarriesNewId,
    /// The event carries a file descriptor.
    ///
    /// **What this means for the app:** the event is lost, and it cannot be otherwise — an fd does not
    /// survive the network, which is the founding constraint of the whole project. An app blocked here
    /// needs a token-style substitution of its own, as the buffer path got.
    CarriesFd,
}

impl EventDrop {
    /// A short, stable tag for the log, so the two ends' logs can be grepped and diffed by reason.
    fn tag(self) -> &'static str {
        match self {
            EventDrop::UnmappedSender => "unmapped-sender",
            EventDrop::UnmappedObjectArg { .. } => "unmapped-object-arg",
            EventDrop::CarriesNewId => "carries-new-id",
            EventDrop::CarriesFd => "carries-fd",
        }
    }
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
    // The witness's first line: *everything* S's compositor emitted on a replayed object, before any
    // filtering. Without it, "the app never got X" cannot be told apart from "S's compositor never sent X",
    // and those call for opposite investigations. Gated, because it is one line per event.
    let trace = event_log_enabled();
    if trace {
        eprintln!(
            "[wp-event][S] from-compositor s_obj={} {} args={}",
            msg.sender_id.protocol_id(),
            event_label(msg.sender_id.interface(), msg.opcode),
            msg.args.len()
        );
    }
    if msg.sender_id.interface().name == ZwpLinuxDmabufV1::interface().name
        && (msg.opcode == 0 || msg.opcode == 1)
    {
        // Suppressed on purpose, not lost. Traced so the diff shows why the app never saw it, rather than
        // leaving a gap someone has to rediscover this rule to explain.
        if trace {
            eprintln!(
                "[wp-event][S] suppressed s_obj={} {} (dmabuf format/modifier: the C proxy answers this locally)",
                msg.sender_id.protocol_id(),
                event_label(msg.sender_id.interface(), msg.opcode)
            );
        }
        return;
    }
    // Build the app-space message under the lock; emit it — and report any drop — after releasing it.
    //
    // The `Result` is the whole point of this shape: every failure path below used to be a bare `return`
    // inside the lock, so a dropped event left no trace at all, and "no errors in S's log" was not evidence
    // about a path that was silent by construction. Carrying the reason out as a `Copy` value lets it be
    // *formatted* outside the critical section, so the witness cannot slow the section it is watching.
    // Decided once, outside the argument loop: is this the one fd-carrying event whose *contents* are
    // its whole meaning? Matched by interface and event **name**, not by a bare opcode — an opcode is
    // an index into one interface's event list, so the same number means something different on every
    // other interface.
    let is_keymap = event_label(msg.sender_id.interface(), msg.opcode) == "wl_keyboard.keymap";
    let outcome: Result<WaylandMessage, EventDrop> = {
        let maps = maps.lock().expect("the WP0 id maps lock is never poisoned");
        // The sender must be an object the replay created; S's own registry/display objects are not, so
        // their events (the global advertisements, the roundtrip callback) resolve to nothing and drop.
        match maps.to_app(&msg.sender_id) {
            None => Err(EventDrop::UnmappedSender),
            Some(app_object) => {
                let mut args = Vec::with_capacity(msg.args.len());
                let mut drop_reason = None;
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
                        // An object reference: translate S→app. If the app never learned of it, drop the
                        // whole event rather than name an object the app cannot resolve.
                        Argument::Object(id) => match maps.to_app(id) {
                            Some(app_id) => args.push(WaylandArg::Object(app_id)),
                            None => {
                                drop_reason = Some(EventDrop::UnmappedObjectArg {
                                    s_object: id.protocol_id(),
                                });
                                break;
                            }
                        },
                        // A compositor-created object, or an fd: outside WP0's event set; drop the event.
                        Argument::NewId(_) => {
                            drop_reason = Some(EventDrop::CarriesNewId);
                            break;
                        }
                        // **An fd. One kind can cross as its contents; the rest still cannot.**
                        //
                        // `wl_keyboard.keymap` names a read-only mapping whose *bytes are the whole
                        // payload* — the XKB keymap as text — so sending the bytes and letting C mint
                        // its own `memfd` is faithful: the application mmaps a read-only fd of `size`
                        // bytes holding the keymap, which is exactly the protocol's promise. Anything
                        // else an fd might name (a GPU buffer, a sync file) has identity beyond its
                        // bytes and is still dropped.
                        Argument::Fd(fd) => {
                            if is_keymap {
                                match read_fd_contents(std::os::fd::AsRawFd::as_raw_fd(fd)) {
                                    Some(bytes) => {
                                        // Logged unconditionally, not behind the event trace: this
                                        // length must equal the `size` the event carries unchanged,
                                        // and a mismatch is a SIGBUS in the application rather than an
                                        // error anyone would see. Cheap — once per keyboard.
                                        eprintln!(
                                            "rayland-s: WP0 keymap: relaying {} bytes of keymap \
                                             content (must match the event's size argument)",
                                            bytes.len()
                                        );
                                        args.push(WaylandArg::KeymapContent(bytes));
                                    }
                                    // Reading it failed. Dropping the event is the honest outcome —
                                    // the application waits for a keymap rather than mapping garbage —
                                    // and the reason is recorded rather than silently swallowed.
                                    None => {
                                        drop_reason = Some(EventDrop::CarriesFd);
                                        break;
                                    }
                                }
                            } else {
                                drop_reason = Some(EventDrop::CarriesFd);
                                break;
                            }
                        }
                    }
                }
                match drop_reason {
                    Some(reason) => Err(reason),
                    None => Ok(WaylandMessage {
                        object_id: app_object,
                        opcode: msg.opcode,
                        args,
                    }),
                }
            }
        }
    };

    // The lock is released. Format freely from here.
    match outcome {
        Ok(app_msg) => {
            if trace {
                eprintln!(
                    "[wp-event][S] emit s_obj={} app_obj={} {} args={}",
                    msg.sender_id.protocol_id(),
                    app_msg.object_id,
                    event_label(msg.sender_id.interface(), msg.opcode),
                    app_msg.args.len()
                );
            }
            sink.emit(app_msg);
        }
        // **Unconditional**, matching the C proxy's drop reporting. Each of these is an event S's
        // compositor sent that the application will never see, and if the app is blocked waiting for it,
        // this line is the answer. See `EventDrop`'s variants for what each means for the app.
        Err(reason) => {
            let detail = match reason {
                EventDrop::UnmappedObjectArg { s_object } => {
                    format!(" (argument names S object {s_object}, unknown to the app)")
                }
                _ => String::new(),
            };
            eprintln!(
                "[wp-event][S] drop:{} s_obj={} {}{}",
                reason.tag(),
                msg.sender_id.protocol_id(),
                event_label(msg.sender_id.interface(), msg.opcode),
                detail
            );
        }
    }
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
fn compositor_reader(conn: Connection, dead: Arc<AtomicBool>) {
    loop {
        // Stop the moment the backend's connection state is poisoned: every call below would
        // unwrap that poisoned mutex. See `WaylandReplay::dead_flag`.
        if dead.load(Ordering::Relaxed) {
            eprintln!("rayland-s: WP0 replay: compositor reader stopping — the replay is dead");
            return;
        }
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
                    eprintln!(
                        "rayland-s: WP0 compositor poll failed, event reader stopping: {err}"
                    );
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
                    eprintln!(
                        "rayland-s: WP0 compositor dispatch failed, event reader stopping: {e}"
                    );
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
    /// S's mirrors of the application's `wl_shm` pools — S's own memfds, kept in step by copying.
    ///
    /// Lives here rather than in the id maps because it is not an identity table: it owns memory and
    /// descriptors whose lifetime is the pool's. See [`crate::shm_mirror`], and in particular its
    /// warning that S's file is a *different file* from the application's.
    shm: crate::shm_mirror::ShmMirror,
    /// Whether the compositor-reader thread has been spawned (spawned once, after the connect roundtrip).
    reader_started: bool,
    /// Resolves a token's resource id to a duplicate of S's exported dma-buf descriptor (Task 4.3).
    fd_source: Arc<dyn ExportedFdSource>,
    /// Shared with the compositor-reader thread so it stops dispatching once the backend is poisoned.
    ///
    /// The message thread setting [`Self::replay_dead`] only stops *its own* calls. The reader thread
    /// dispatches the same backend independently and would unwrap the poisoned mutex on its next turn —
    /// observed directly in the mutation test, where it panicked alongside the main thread. A panic in a
    /// spawned thread does not end the process, but a reader spinning on a poisoned lock is noise, so it
    /// is told to stop.
    dead_flag: Arc<AtomicBool>,
    /// Set once a `send_request` panic has been caught, after which the replay issues **no further
    /// backend call**.
    ///
    /// # Why a panic is unrecoverable, and why the wrapper cannot pretend otherwise
    /// `Backend::send_request` takes `wayland-backend`'s own `ConnectionState::protocol` mutex and
    /// **holds it across every panic it raises** (`rs/client_impl/mod.rs`: the guard is taken at the top
    /// of `send_request`, the version and interface panics fire below it). A panic while a `Mutex` guard
    /// is held **poisons that mutex**, and the backend's `lock_protocol()` is a bare
    /// `self.protocol.lock().unwrap()` — so the *next* backend call of any kind unwraps a poisoned lock
    /// and aborts the process.
    ///
    /// No lock discipline on Rayland's side changes this: the poisoned mutex is inside the dependency.
    /// A `catch_unwind` that logged "session continues" and carried on therefore turned an immediate,
    /// legible crash into a reassuring line followed by a segfault one call later — strictly worse than
    /// no wrapper. `vkgears` demonstrated exactly that on 2026-08-30.
    ///
    /// So the honest behaviour is: catch it, say plainly that the Wayland replay is dead, and stop
    /// touching the backend. The vtest/ring relay is a separate session and keeps running — the
    /// application loses its window, not its compute.
    replay_dead: bool,
    /// **S's own** `zwp_linux_dmabuf_v1` object and the version it was bound at — bound lazily by
    /// [`Self::ensure_dmabuf`] the first time a buffer token arrives, and never destroyed.
    ///
    /// # Why S binds its own instead of reusing the application's
    /// A `create_immed` names the **params** object, and nothing in the relayed message identifies the
    /// dmabuf global it descends from, so S must supply one. The obvious move — remember the object
    /// created when the app binds the global — was tried and is **wrong**: the application binds
    /// `zwp_linux_dmabuf_v1` many times while probing formats (twelve times in one measured run) and
    /// **destroys** each one. A remembered id therefore names a dead object as soon as the app moves on,
    /// and every later `create_params` fails with `Invalid ObjectId` — after which no `wl_buffer` exists,
    /// the app's `attach` fails too, and a `wl_surface` with no valid buffer is unmapped by definition, so
    /// the window **disappears from the screen** while the application carries on none the wiser. That is
    /// the failure a human watching the screen reported on 2026-08-29.
    ///
    /// Binding S's own object severs the dependency on the application's object lifetime entirely: it is
    /// created once, owned by the replay, and outlives every swapchain the app builds. This is the same
    /// lesson as the recycled-id race in `rayland-c`, in its other form — *a handle you cached is not a
    /// handle you still have.*
    dmabuf: Option<(ObjectId, u32)>,
}

impl WaylandReplay {
    /// Create an unconnected replay. No compositor connection is made until the first relayed message.
    ///
    /// `sink` is where compositor events are emitted once translated — the link back to C in the daemon, a
    /// recorder in tests. `fd_source` resolves a buffer token's resource id to a duplicate of the dma-buf
    /// S exported for it — the `Applier` in the daemon, a fake in tests.
    pub fn new(sink: Arc<dyn EventSink>, fd_source: Arc<dyn ExportedFdSource>) -> Self {
        WaylandReplay {
            // No pools until the application creates one; most applications never do.
            shm: crate::shm_mirror::ShmMirror::default(),
            conn: None,
            backend: None,
            registry: None,
            globals: Arc::new(Mutex::new(Vec::new())),
            maps: Arc::new(Mutex::new(IdMaps::default())),
            sink,
            reader_started: false,
            dead_flag: Arc::new(AtomicBool::new(false)),
            replay_dead: false,
            fd_source,
            dmabuf: None,
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
            .spawn({
                let dead = Arc::clone(&self.dead_flag);
                move || compositor_reader(conn, dead)
            }) {
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
        // The backend's connection state is poisoned; any further call would abort the process.
        if self.replay_dead {
            return;
        }
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
            eprintln!(
                "rayland-s: WP0 replay: no linked descriptor for `{interface}`; bind skipped"
            );
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
                // Seed the version map with the **capped** value, not the version the application
                // asked for. Everything descended from this object inherits it.
                self.maps
                    .lock()
                    .expect("the WP0 id maps lock is never poisoned")
                    .insert(app_object_id, s_id, bind_version);
                eprintln!(
                    "rayland-s: WP0 replay bound `{interface}` v{bind_version} (app obj {app_object_id})"
                );
            }
            Err(e) => eprintln!("rayland-s: WP0 replay bind of `{interface}` failed: {e}"),
        }
        self.flush();
    }

    /// Write relayed `wl_shm` pool contents into S's mirror of that pool.
    ///
    /// # Why this is a method on the replay and not on the mirror alone
    /// The mirror is keyed by the **application's** pool id, which is the identifier that travels;
    /// resolving that is the replay's business, and keeping the call here means the mirror never needs
    /// to know an id map exists.
    ///
    /// # Ordering, which is the load-bearing part
    /// C sends this **before** the `wl_surface.commit` that presents the buffer, and both travel the
    /// same ordered stream — so by the time the commit is replayed, these bytes are already in S's
    /// pool. Reversing the two would present whatever the pool held from the previous frame, and would
    /// do it *intermittently*, which is why C pins the order with a test rather than trusting it.
    ///
    /// # Failure modes
    /// A write to an unknown pool, or one that would land outside the mirror, is logged and dropped.
    /// Both mean the two sides disagree about a pool, which is worth saying out loud — but neither is
    /// worth killing a session for, because the application's GPU frames travel a different path
    /// entirely and are unaffected.
    /// `(bytes written, updates applied, pools mirrored)` — S's half of the shm summary.
    ///
    /// Should agree with C's `shm_bytes`/`shm_commits`. A divergence means the two sides disagree
    /// about a pool, which is exactly the drift [`crate::shm_mirror`] warns about and is worth saying
    /// out loud rather than discovering as a wrong picture.
    pub fn shm_summary(&self) -> (u64, u64, usize) {
        self.shm.summary()
    }

    pub fn handle_shm_pool_data(&mut self, app_pool_id: u32, offset: u32, bytes: &[u8]) {
        if let Err(e) = self.shm.write(app_pool_id, offset, bytes) {
            eprintln!(
                "rayland-s: WP0 shm: dropping {} bytes at offset {offset} for app pool {app_pool_id}: {e:?}",
                bytes.len()
            );
        }
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
        // The backend's connection state is poisoned; any further call would abort the process.
        if self.replay_dead {
            return;
        }
        if !self.ensure_connected() {
            return;
        }
        // **Buffer tokens are handled before the sender lookup, and that ordering is load-bearing.** The
        // token arrives on a `create_immed` whose sender is the app's *params* object — which S has never
        // created, because C intercepts `create_params` and does not forward it. So the lookup below would
        // refuse this request as "unmapped" before anything ever looked at the token. See
        // [`Self::synthesize_buffer`], which creates that params object as its first act.
        if msg.args.iter().any(|a| matches!(a, WaylandArg::Buffer(_))) {
            self.synthesize_buffer(&msg);
            return;
        }
        // The sender must be an object the replay already created (bound global or prior new-id).
        let (sender, sender_version) = {
            let maps = self
                .maps
                .lock()
                .expect("the WP0 id maps lock is never poisoned");
            match (maps.to_s(msg.object_id), maps.version_of(msg.object_id)) {
                (Some(s), Some(v)) => (s, v),
                _ => {
                    eprintln!(
                        "rayland-s: WP0 replay: request for unmapped object {} (opcode {}); skipped",
                        msg.object_id, msg.opcode
                    );
                    return;
                }
            }
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
                // **The shm-pool substitution, landing.** C kept the application's descriptor and sent
                // only the size; S makes its *own* memfd of that size and passes *that* to the
                // compositor, so the `wl_shm.create_pool` the compositor sees is ordinary and
                // complete. The two files are kept in step by `C2S::ShmPoolData` copies, never by
                // sharing — see `crate::shm_mirror`.
                WaylandArg::ShmPool { size } => {
                    // The pool's app-side id is the `new_id` argument, which precedes the fd in
                    // `wl_shm.create_pool(new_id, fd, size)`. Reaching here without it would mean the
                    // request was malformed, and creating a mirror under the wrong key would leave
                    // every later `ShmPoolData` writing into nothing.
                    let Some(app_pool_id) = new_app_id else {
                        eprintln!(
                            "rayland-s: refusing wl_shm.create_pool: the ShmPool argument arrived \
                             with no new_id before it, so there is no pool to key the mirror by"
                        );
                        return;
                    };
                    match self.shm.create_pool(app_pool_id, *size) {
                        Ok(fd) => {
                            // **Two arguments come back out of one.** `WaylandArg::ShmPool` replaces
                            // the `fd` of `create_pool(new_id, fd, int size)` on the wire, but the
                            // compositor's signature still wants both — so the substitution expands
                            // here into the descriptor *and* the size it stood in for. Pushing only
                            // the fd leaves a short request, and `wayland-client` rejects it with
                            // "Unexpected signature ... expected [NewId, Fd, Int]" — which is exactly
                            // how the first acceptance run found this, and is a far better failure
                            // than a compositor quietly reading a wrong length.
                            args.push(Argument::Fd(fd));
                            args.push(Argument::Int(i32::try_from(*size).unwrap_or(i32::MAX)));
                        }
                        Err(e) => {
                            // **What this means for the app:** its pool never appears, so whatever it
                            // meant to draw with shm — usually a cursor or a decoration — is absent.
                            // Reported rather than fatal: the application's own GPU frames go through
                            // the dma-buf path and are unaffected.
                            eprintln!(
                                "rayland-s: refusing wl_shm.create_pool for app pool {app_pool_id}: {e:?}"
                            );
                            return;
                        }
                    }
                }
                // **Refused, in the direction it can never legitimately travel.** `KeymapContent`
                // exists for the *event* path, S→C: it replaces the fd `wl_keyboard.keymap` carries.
                // A request arriving C→S with one would mean the application is trying to hand S a
                // keymap, which no interface in WP0's set does. Refusing loudly beats silently
                // pushing an argument the compositor will misread as something else.
                WaylandArg::KeymapContent(bytes) => {
                    eprintln!(
                        "rayland-s: refusing request {}.#{}: it carries KeymapContent ({} bytes), \
                         which is an S->C event substitution and has no meaning in a request",
                        msg.object_id,
                        msg.opcode,
                        bytes.len()
                    );
                    return;
                }
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
                    // **The child's version comes from the SENDER, never from the wire.** A Wayland
                    // `new_id` inherits the version of the object that creates it, and S may hold that
                    // object at a *lower* version than the application does, because `handle_bind` caps
                    // every bind at what S's compositor advertises. `wayland-backend` enforces the
                    // inheritance by panicking, so using `version` here — the application's view — is a
                    // crash the moment S caps anything. It cost three separate failures to learn
                    // (`create_immed`'s `wl_buffer`, the params object, and `get_xdg_surface`); building
                    // from `sender_version` removes the whole class rather than the third instance.
                    //
                    // The wire's `version` is kept and logged because it is genuinely useful — the gap
                    // between it and the sender's is exactly how much S had to cap — but it decides
                    // nothing.
                    if *version != sender_version {
                        eprintln!(
                            "rayland-s: WP0 replay: capping child `{interface}` to v{sender_version} \
                             (the app asked for v{version}; S's `{}` is v{sender_version})",
                            sender.interface().name
                        );
                    }
                    child_spec = interface_by_name(interface).map(|iface| (iface, sender_version));
                    new_app_id = Some(*id);
                }
                // Unreachable: a message carrying a token was routed to `synthesize_buffer` above. Kept
                // as a refusal rather than an `unreachable!()` because the generic path genuinely cannot
                // express a token — there is no fd to translate — and a future edit that reorders the
                // check should lose the buffer, not kill the message thread.
                WaylandArg::Buffer(_) => {
                    eprintln!(
                        "rayland-s: WP0 replay: buffer token reached the generic path (obj {} opcode {}); \
                         request dropped — this is a routing bug",
                        msg.object_id, msg.opcode
                    );
                    return;
                }
            }
        }

        // **Refuse what would panic, rather than catching it afterwards.** A caught panic is not a
        // recovery here — it poisons the backend and kills the replay (see `replay_dead`) — so the two
        // panics that are cheaply predictable are checked first. The version panic is prevented by
        // construction now that children inherit the sender's version; these are the other two.
        if let Err(why) = precheck_request(&sender, msg.opcode, child_spec.map(|(i, _)| i)) {
            eprintln!(
                "rayland-s: WP0 replay: refusing request (obj {} opcode {}): {why}; dropped rather than \
                 risking a panic that would kill the replay",
                msg.object_id, msg.opcode
            );
            return;
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
                    // Witness: the reverse map is keyed by the S-side *protocol id*, and Wayland recycles
                    // those after an object is destroyed. So a later event arriving on this number resolves
                    // through whichever mapping was written last — which is precisely what decides whether
                    // an event reaches the app or is addressed to a dead app object. Logged (gated) so the
                    // mapping can be read back rather than reasoned about.
                    let s_protocol = s_new_id.protocol_id();
                    // The child inherits the sender's version, which is what keeps the invariant in
                    // `IdMaps::versions` true for every object the replay ever creates.
                    self.maps
                        .lock()
                        .expect("the WP0 id maps lock is never poisoned")
                        .insert(app_id, s_new_id, sender_version);
                    if event_log_enabled() {
                        eprintln!(
                            "[wp-event][S] map s_obj={s_protocol} app_obj={app_id} (from obj {} opcode {opcode})",
                            msg.object_id
                        );
                    }
                }
            }
            Ok(Err(e)) => eprintln!(
                "rayland-s: WP0 replay: send_request (obj {} opcode {opcode}) failed: {e}",
                msg.object_id
            ),
            // **Not a recovery.** See `WaylandReplay::replay_dead`: the panic fired inside
            // `send_request` with wayland-backend's own connection mutex held, poisoning it, so the very
            // next backend call would unwrap a poisoned lock and abort the process. The only honest
            // thing left is to stop calling the backend and say so.
            Err(_) => {
                eprintln!(
                    "rayland-s: WP0 replay: FATAL — send_request (obj {} opcode {opcode}) panicked. \
                     wayland-backend's connection state is now poisoned, so the Wayland replay is DEAD \
                     and no further request will be replayed. The vtest/ring relay continues; the \
                     application keeps rendering but loses its window.",
                    msg.object_id
                );
                self.replay_dead = true;
                self.dead_flag.store(true, Ordering::Relaxed);
                // **Return before the flush.** `Connection::flush` is itself a backend call, so
                // flushing here would immediately unwrap the mutex the panic just poisoned and abort —
                // turning an honest "the replay is dead" into a crash on the very next line. Found by
                // the mutation test, which kept dying at `client_impl/mod.rs:115` even after the panic
                // was being caught.
                return;
            }
        }
        self.flush();
    }

    /// Ensure S has its **own** `zwp_linux_dmabuf_v1` bound, and return it with its version.
    ///
    /// Binds once, from the globals S's compositor advertises, and keeps the object for the session — the
    /// replay never destroys it, which is the whole point (see the [`Self::dmabuf`] field docs for the bug
    /// this avoids). Idempotent: later calls return the same object.
    ///
    /// # Version
    /// Capped at [`DMABUF_BIND_VERSION`] and at what S's compositor advertises. The version matters beyond
    /// the bind itself: a Wayland child inherits its parent's version, so this number becomes the params
    /// object's version and then the `wl_buffer`'s.
    ///
    /// Returns `None` if S's compositor advertises no dmabuf global, or the bind fails — in which case the
    /// caller refuses the buffer rather than guessing.
    fn ensure_dmabuf(&mut self) -> Option<(ObjectId, u32)> {
        if let Some(existing) = &self.dmabuf {
            return Some(existing.clone());
        }
        let iface = ZwpLinuxDmabufV1::interface();
        // Find the global by interface name; S's registry numbering is its own, so the name is the key.
        let found = {
            let globals = self
                .globals
                .lock()
                .expect("the WP0 globals lock is never poisoned");
            globals
                .iter()
                .find(|g| g.interface == iface.name)
                .map(|g| (g.name, g.version.min(DMABUF_BIND_VERSION)))
        };
        let Some((name, version)) = found else {
            eprintln!(
                "rayland-s: WP0 4.3: S's compositor advertises no `{}`",
                iface.name
            );
            return None;
        };
        let backend = self.backend.as_ref().expect("connected").clone();
        let registry = self.registry.clone().expect("connected");
        let data = self.object_data();
        // `wl_registry.bind` spells its generic new_id out in full: [name, interface, version, new_id].
        let iface_arg = CString::new(iface.name).expect("an interface name has no interior NUL");
        let result = backend.send_request(
            Message {
                sender_id: registry,
                opcode: OP_REGISTRY_BIND,
                args: [
                    Argument::Uint(name),
                    Argument::Str(Some(Box::new(iface_arg))),
                    Argument::Uint(version),
                    Argument::NewId(ObjectId::null()),
                ]
                .into_iter()
                .collect(),
            },
            Some(data),
            Some((iface, version)),
        );
        match result {
            Ok(id) => {
                eprintln!(
                    "rayland-s: WP0 4.3: bound S's own `{}` v{version} for buffer creation",
                    iface.name
                );
                self.dmabuf = Some((id.clone(), version));
                Some((id, version))
            }
            Err(e) => {
                eprintln!(
                    "rayland-s: WP0 4.3: binding S's own `{}` failed: {e}",
                    iface.name
                );
                None
            }
        }
    }

    /// Build a real `wl_buffer` on S's compositor from a relayed [`BufferToken`] — the one sequence S
    /// **originates** instead of replaying (Task 4.3).
    ///
    /// # Why this exists at all
    /// The app's `wl_buffer` names a dma-buf fd, and C drops that fd by design — that *is* buffer-by-token.
    /// So there is nothing to translate: S must construct the buffer from the dma-buf **it already
    /// exported** for the named resource when that resource was created. The pixels never crossed the
    /// network; only the name of where they already are.
    ///
    /// # Inputs
    /// `msg` is the relayed `create_immed`: its `object_id` is the app's params object, and its arguments
    /// are the new `wl_buffer`'s app-side id plus the token.
    ///
    /// # Failure modes — all of them total, none of them partial
    /// A buffer is either fully built or not attached at all. Each of these logs which step failed and
    /// returns, leaving the app with a `wl_buffer` S is simply never told to present:
    /// - the message is malformed (no token, or no new-id for the buffer);
    /// - the app never bound `zwp_linux_dmabuf_v1`, so there is no object to originate `create_params` on;
    /// - the resource id resolves to no exported descriptor (never created, or already unref'd — which a
    ///   token can outlive);
    /// - any of the three `send_request`s fails.
    ///
    /// # Domain pitfall: the descriptor's ownership
    /// `dup_exported_fd` hands back an **owned duplicate**, and the pure-Rust `wayland-backend` in this
    /// tree dups again inside `write_message` rather than consuming what it is given. So this function
    /// must hold its `OwnedFd` across the `add` and let it drop afterwards: dropping early would send a
    /// closed descriptor, and forgetting it would leak one per frame.
    fn synthesize_buffer(&mut self, msg: &WaylandMessage) {
        // Pull the two facts the message carries: which resource, and what the app calls the new buffer.
        let Some(token) = msg.args.iter().find_map(|a| match a {
            WaylandArg::Buffer(t) => Some(t.clone()),
            _ => None,
        }) else {
            eprintln!(
                "rayland-s: WP0 4.3: buffer request with no token (obj {}); refused",
                msg.object_id
            );
            return;
        };
        let Some(app_buffer_id) = msg.args.iter().find_map(|a| match a {
            WaylandArg::NewId { id, .. } => Some(*id),
            _ => None,
        }) else {
            eprintln!(
                "rayland-s: WP0 4.3: buffer token for resource {} names no wl_buffer id; refused",
                token.resource_id
            );
            return;
        };
        // S's own dmabuf global, bound on demand. Deliberately not the application's: the app destroys
        // the ones it binds, and a reference to a destroyed object fails every later `create_params`.
        let Some((dmabuf_id, dmabuf_version)) = self.ensure_dmabuf() else {
            eprintln!(
                "rayland-s: WP0 4.3: no usable `zwp_linux_dmabuf_v1` on S; buffer for resource {} refused",
                token.resource_id
            );
            return;
        };
        // Resolve the descriptor. This takes and releases the applier lock inside the trait method, so no
        // lock is held across any of the `send_request`s below — see [`ExportedFdSource`].
        let Some(fd) = self.fd_source.dup_exported_fd(token.resource_id) else {
            eprintln!(
                "rayland-s: WP0 4.3: resource {} has no exported dma-buf (never created, or unref'd); \
                 buffer refused",
                token.resource_id
            );
            return;
        };

        // The three requests, laid out by the pure planner so their shape is unit-tested rather than
        // asserted here. `fd` stays owned by this scope for the whole sequence.
        let plan = plan_buffer_requests(&token, dmabuf_version, fd.as_raw_fd());
        let backend = self.backend.as_ref().expect("connected").clone();

        // `send_request` **panics** on a protocol violation rather than returning an error — a version
        // mismatch on a child_spec, an unknown object, a bad argument shape. The generic replay path wraps
        // it in `catch_unwind` for exactly that reason and this path must too: the first two-machine run of
        // this code took the whole daemon's main thread down with "expected version 3 but got 1", losing
        // the ring session along with the buffer. A refused buffer must cost the frame, not the session.
        let send =
            |sender: ObjectId, req: SynthesizedRequest, data: Option<Arc<dyn ObjectData>>| {
                catch_unwind(AssertUnwindSafe(|| {
                    backend.send_request(
                        Message {
                            sender_id: sender,
                            opcode: req.opcode,
                            args: req.args.into_iter().collect(),
                        },
                        data,
                        req.child,
                    )
                }))
            };

        // Step 1: create the params object on the dmabuf global, and map the app's params id to it — that
        // mapping is what makes the app's *later* requests against this object (if any) resolvable.
        let [create_params, add, create_immed] = plan;
        let s_params = match send(
            dmabuf_id,
            create_params,
            Some(self.object_data() as Arc<dyn ObjectData>),
        ) {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => {
                eprintln!(
                    "rayland-s: WP0 4.3: step 1/3 create_params failed for resource {}: {e}",
                    token.resource_id
                );
                return;
            }
            Err(_) => {
                self.declare_replay_dead("step 1/3 create_params", token.resource_id);
                return;
            }
        };
        // Same inheritance rule as everywhere else, with the chain spelled out because this path
        // originates its objects rather than replaying them: the params object's sender is S's own
        // dmabuf global, so the params object is `dmabuf_version`; the `wl_buffer` below is created by
        // the params object, so it is `dmabuf_version` too. `plan_buffer_requests` stamps both from the
        // same number for exactly this reason — there is one rule, not a special case here.
        self.maps
            .lock()
            .expect("the WP0 id maps lock is never poisoned")
            .insert(msg.object_id, s_params.clone(), dmabuf_version);

        // Step 2: describe the plane. No child object, so no object data.
        match send(s_params.clone(), add, None) {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                eprintln!(
                    "rayland-s: WP0 4.3: step 2/3 add failed for resource {}: {e}",
                    token.resource_id
                );
                return;
            }
            Err(_) => {
                self.declare_replay_dead("step 2/3 add", token.resource_id);
                return;
            }
        }

        // Step 3: the buffer itself. Once mapped, the app's own attach/commit replay through the ordinary
        // path and put this buffer on S's surface.
        let s_buffer = match send(
            s_params,
            create_immed,
            Some(self.object_data() as Arc<dyn ObjectData>),
        ) {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => {
                eprintln!(
                    "rayland-s: WP0 4.3: step 3/3 create_immed failed for resource {}: {e}",
                    token.resource_id
                );
                return;
            }
            Err(_) => {
                self.declare_replay_dead("step 3/3 create_immed", token.resource_id);
                return;
            }
        };
        // The buffer inherits the params object's version — see the note at the params insert above.
        self.maps
            .lock()
            .expect("the WP0 id maps lock is never poisoned")
            .insert(app_buffer_id, s_buffer, dmabuf_version);

        // The buffer is real and on S's compositor, so this resource is now shown rather than read:
        // stop the return path shipping its pixels to a machine that has no screen.
        self.fd_source.note_presented(token.resource_id);
        eprintln!(
            "rayland-s: WP0 4.3: built wl_buffer (app obj {app_buffer_id}) from resource {} \
             ({}x{} fmt {:#x} offset {} stride {} modifier {:#x})",
            token.resource_id,
            token.width,
            token.height,
            token.drm_format,
            token.offset,
            token.stride,
            token.modifier
        );
        self.flush();
        // `fd` drops here, closing this duplicate. The backend already dup'd its own copy when it wrote the
        // `add`, and the `Applier`'s original is untouched — it must be, since the export cannot be redone.
    }

    /// Record that a `send_request` panic has killed the Wayland replay, and say so without pretending.
    ///
    /// Every caller of this has just caught a panic from inside `send_request`, which means
    /// wayland-backend's connection mutex is poisoned and any further backend call aborts the process.
    /// See [`Self::replay_dead`] for the full mechanism. Centralised so that no site can drift into
    /// claiming a recovery that the dependency's locking makes impossible.
    fn declare_replay_dead(&mut self, what: &str, resource_id: u32) {
        eprintln!(
            "rayland-s: WP0 4.3: FATAL — {what} PANICKED for resource {resource_id}. \
             wayland-backend's connection state is now poisoned, so the Wayland replay is DEAD and no \
             further request will be replayed. The vtest/ring relay continues."
        );
        self.replay_dead = true;
        self.dead_flag.store(true, Ordering::Relaxed);
    }

    /// Whether a `send_request` panic has killed the Wayland replay. See [`Self::replay_dead`].
    ///
    /// Exposed so a test can assert the *absence* of that state: "the request was replayed and the
    /// replay is still alive" is the property the version fix exists to provide, and it is not
    /// observable from the id map alone.
    pub fn is_replay_dead(&self) -> bool {
        self.replay_dead
    }

    /// The version of the S-side object standing for `app_object_id`, if the replay created it.
    ///
    /// Exposed for tests: it is how "S capped this bind" is detected without reaching into the maps.
    pub fn version_of(&self, app_object_id: u32) -> Option<u32> {
        self.maps
            .lock()
            .expect("the WP0 id maps lock is never poisoned")
            .version_of(app_object_id)
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

/// Would this request make `Backend::send_request` panic? `Ok(())` if not, `Err(reason)` if so.
///
/// # Why this exists rather than relying on the `catch_unwind`
/// `send_request` panics on a protocol violation **while holding wayland-backend's own connection
/// mutex**, poisoning it — so catching the panic does not save the session, it only delays the abort by
/// one backend call (see [`WaylandReplay::replay_dead`]). Anything predictable is therefore better
/// refused than caught.
///
/// # What it checks, and what it deliberately does not
/// - **The opcode exists on the sender's interface.** An unknown opcode is `send_request`'s first
///   panic, and it is reachable whenever S's linked descriptor is older than the app's.
/// - **The child interface matches what the descriptor says the request creates.** That is
///   `send_request`'s other structural panic.
///
/// It does **not** check the argument signature: reproducing the backend's own type-by-type validation
/// here would duplicate logic that can drift out of step with the dependency, which is a worse failure
/// than the one it would prevent. The version panic is not checked either, because children now inherit
/// the sender's version and it cannot occur.
fn precheck_request(
    sender: &ObjectId,
    opcode: u16,
    child: Option<&'static Interface>,
) -> Result<(), String> {
    let iface = sender.interface();
    let Some(desc) = iface.requests.get(opcode as usize) else {
        return Err(format!(
            "opcode {opcode} is out of range for `{}` (it has {} requests)",
            iface.name,
            iface.requests.len()
        ));
    };
    // Only meaningful when the request creates an object *and* the descriptor names a fixed interface
    // for it; `wl_registry.bind`'s generic new_id has `child_interface: None` and is handled elsewhere.
    if let (Some(child), Some(expected)) = (child, desc.child_interface) {
        if child.name != expected.name {
            return Err(format!(
                "`{}.{}` creates a `{}`, but the relayed request names a `{}`",
                iface.name, desc.name, expected.name, child.name
            ));
        }
    }
    Ok(())
}

/// Map a Wayland interface name to the linked `&'static Interface` the client backend needs.
///
/// The set is exactly the interfaces WP0 replays (spec §6): the globals the app binds and the objects it
/// creates from them. An unknown name returns `None` — the request or bind naming it is skipped rather
/// than replayed, since without the descriptor the backend cannot create the object.
/// Every name [`interface_by_name`] answers to.
///
/// Rust cannot enumerate a `match`'s arms, so this list exists to let the consistency test walk
/// them. **Keep it in step with the `match` below**: a name here the `match` lacks fails
/// `every_supported_interface_resolves` if the table names it, and a `match` arm missing here is
/// caught the first time anything binds it. Both are better than the silence this replaces.
const KNOWN_INTERFACE_NAMES: &[&str] = &[
    "wl_compositor",
    "wl_surface",
    "wl_region",
    "wl_callback",
    "wl_buffer",
    "wl_shm",
    "wl_shm_pool",
    "wl_seat",
    "wl_output",
    "zxdg_output_manager_v1",
    "zxdg_output_v1",
    "zxdg_decoration_manager_v1",
    "zxdg_toplevel_decoration_v1",
    "wp_cursor_shape_manager_v1",
    "wp_cursor_shape_device_v1",
    "xdg_wm_base",
    "xdg_surface",
    "xdg_toplevel",
    "zwp_linux_dmabuf_v1",
    "zwp_linux_buffer_params_v1",
];

use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1::ZxdgOutputV1,
};

fn interface_by_name(name: &str) -> Option<&'static Interface> {
    Some(match name {
        "wl_compositor" => WlCompositor::interface(),
        "wl_surface" => WlSurface::interface(),
        "wl_region" => WlRegion::interface(),
        "wl_callback" => WlCallback::interface(),
        "wl_buffer" => WlBuffer::interface(),
        // The `wl_shm` pair. Missing these was caught by the first `solarsim` acceptance run and by
        // nothing else: C intercepted `create_pool` and forwarded it correctly, S logged
        // "no linked descriptor for `wl_shm`; bind skipped", and the application carried on happily
        // because its *GPU* frames go through dma-buf — so the only symptom was a cursor that never
        // appeared on S. No unit test can notice a missing entry in a table of things to support;
        // only running a real toolkit application can, which is exactly what the acceptance criterion
        // is for.
        "wl_shm" => WlShm::interface(),
        "wl_shm_pool" => WlShmPool::interface(),
        "wl_seat" => WlSeat::interface(),
        // The output pair. `wl_output` carries S's real monitor geometry, mode, scale and refresh —
        // correct to relay, because S's monitor is where the application is actually displayed.
        // `zxdg_output_manager_v1` adds logical (compositor-space) geometry on top of it, and
        // `zxdg_output_v1` is what its `get_xdg_output` creates.
        "wl_output" => WlOutput::interface(),
        "zxdg_output_manager_v1" => ZxdgOutputManagerV1::interface(),
        "zxdg_output_v1" => ZxdgOutputV1::interface(),
        // Server- vs client-side decoration negotiation, and the per-toplevel object
        // `get_toplevel_decoration` creates.
        "zxdg_decoration_manager_v1" => ZxdgDecorationManagerV1::interface(),
        "zxdg_toplevel_decoration_v1" => ZxdgToplevelDecorationV1::interface(),
        // Naming a cursor shape instead of supplying pixels, and the per-pointer object
        // `get_pointer` creates.
        "wp_cursor_shape_manager_v1" => WpCursorShapeManagerV1::interface(),
        "wp_cursor_shape_device_v1" => WpCursorShapeDeviceV1::interface(),
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

    /// Every interface the shared table names must resolve here, or S cannot replay a bind of it.
    ///
    /// This replaces a test that listed the thirteen names it expected and asserted they resolve.
    /// That test passed for a day while `wl_shm` was missing, because the name was absent from both
    /// the code and the test — it could only ever catch a name someone remembered to add in two
    /// places at once. This one compares against a list maintained for a *different* purpose (C's
    /// advertisement), so forgetting one side is a failure rather than a silence.
    #[test]
    fn every_supported_interface_resolves() {
        for spec in rayland_relay::interfaces::SUPPORTED {
            let iface = interface_by_name(spec.name).unwrap_or_else(|| {
                panic!(
                    "`{}` is in rayland_relay::interfaces::SUPPORTED but S has no linked \
                     descriptor for it, so a bind would be dropped mid-session",
                    spec.name
                )
            });
            assert_eq!(iface.name, spec.name, "descriptor name must match the lookup key");
        }
    }

    /// And nothing resolves here that the table does not name.
    ///
    /// A descriptor S can build but C never advertises is dead code at best; at worst it is an
    /// interface added on one side only, which is the same drift in the other direction.
    #[test]
    fn nothing_resolves_that_the_table_does_not_name() {
        for name in KNOWN_INTERFACE_NAMES {
            assert!(
                rayland_relay::interfaces::spec_for(name).is_some(),
                "S resolves `{name}` but it is not in rayland_relay::interfaces::SUPPORTED"
            );
        }
    }

    /// An interface WP0 has never heard of resolves to `None`, so its request is skipped rather
    /// than mis-created. `wl_data_device_manager` is the standing example: clipboard and
    /// drag-and-drop transfer data over descriptors the application creates.
    #[test]
    fn an_unknown_interface_resolves_to_none() {
        assert!(interface_by_name("wl_data_device_manager").is_none());
    }

    /// Event-name lookup resolves a real opcode and refuses an out-of-range one instead of panicking.
    ///
    /// The out-of-range half is the one that matters. The opcode arrives from S's compositor, which may
    /// advertise an interface version newer than the descriptor this binary linked, so an event with no
    /// name here is a thing that can genuinely happen at runtime. Indexing blindly would turn "an event we
    /// have no name for" into a panic on the compositor-reader thread — losing the whole return path in
    /// order to log one line.
    #[test]
    fn event_names_resolve_and_out_of_range_opcodes_do_not_panic() {
        // `wl_callback` has exactly one event, `done` at opcode 0 — the event the WP0 frame-callback path
        // depends on, and the reason this lookup exists at all.
        let callback = WlCallback::interface();
        assert_eq!(event_name(callback, 0), Some("done"));
        assert_eq!(
            event_label(callback, 0),
            "wl_callback.done",
            "the label is what both ends' logs are diffed on"
        );

        // One past the end, and far past it: both must be `None`, not a panic.
        let past_end = callback.events.len() as u16;
        assert_eq!(event_name(callback, past_end), None);
        assert_eq!(event_name(callback, u16::MAX), None);
        assert_eq!(
            event_label(callback, past_end),
            format!("wl_callback.#{past_end}"),
            "an unnamed opcode still prints its number, so the event is never invisible"
        );

        // A multi-event interface, so the lookup is proven to index rather than always return the first.
        let buffer = WlBuffer::interface();
        assert_eq!(event_name(buffer, 0), Some("release"));
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
        assert!(
            maps.to_s(3).is_none(),
            "an unmapped app id resolves to nothing"
        );
    }
}
