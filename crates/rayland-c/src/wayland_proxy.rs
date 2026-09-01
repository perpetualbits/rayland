//! WP0 — **the C-side Wayland proxy.**
//!
//! The application on C presents its window through Wayland. In Rayland's model that window is shown on
//! **S**, not C, so the application must not talk to a real compositor on C — it talks to *this* proxy,
//! which forwards its Wayland protocol to S (where a client replays it against S's real compositor) and
//! intercepts the one thing that cannot cross a network: the swapchain buffer's file descriptor, which is
//! replaced by a **buffer-by-token** naming the S-side resource the command relay already rendered.
//!
//! # Why the low-level `wayland_server::backend`, not the high-level typed API
//! A proxy forwards *whatever* the application does; it is not a compositor with its own object logic. So
//! it works at the structured-message layer: `wayland_backend` (re-exported as `wayland_server::backend`)
//! delivers each request as a `Message { sender_id, opcode, args }` whose `Argument`s the proxy translates
//! to a [`rayland_relay::WaylandMessage`] and forwards. The library owns all wire serialization and fd
//! plumbing; the proxy owns forwarding, the object-id bookkeeping, and the single fd→token interception.
//! See `docs/design/2026-07-22-wp0-wayland-proxy-first-light.md` §3 "Forwarding model".
//!
//! **Status: fd→token (WP0 Task 3b, sub-step 4 — the crux, complete).** The backend stands up, advertises
//! the minimal globals vkcube binds (§6 of the spec), accepts the application, **forwards each request** to
//! a [`WaylandSink`] after translating it (`Argument` → [`WaylandArg`]), and — the special case the whole
//! sub-project exists for — **intercepts buffer creation**: `create_params`/`add`/`create_immed` on the
//! `zwp_linux_dmabuf` path are consumed by [`try_intercept_buffer`], which resolves the passed dma-buf fd's
//! memfd inode to an S-side resource id (via [`ResourceResolver`]) and forwards a [`BufferToken`] in place
//! of the fd — no fd, no pixels crossing. Both the sink and the resolver are stubs in tests (a recorder and
//! a fixed inode map); Task 4 replaces them with the real link to S and the `shm.rs`-backed registry, and
//! adds the `app_id ↔ s_id` object-id mapping so S can replay the session against its real compositor.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use rayland_relay::{BufferToken, WaylandArg, WaylandMessage};
use wayland_server::Resource; // brings `interface()` into scope for the generated object types
use wayland_server::backend::protocol::{Argument, Message};
use wayland_server::backend::{
    Backend, ClientData, ClientId, GlobalHandler, GlobalId, Handle, ObjectData, ObjectId,
};

/// The write end of the compositor-event return path: the daemon's link **reader thread** hands S's
/// events to the proxy through this, from a *different* thread than the proxy's serve loop.
///
/// # Why an eventfd, not just a channel
/// The proxy's serve loop blocks in `poll(2)` on the listener and the backend fd (see [`serve`]). A channel
/// alone cannot wake that `poll`, so a third pollable fd — an `eventfd` — is the wakeup: [`post`](Self::post)
/// queues the message and then writes to the eventfd, which makes the proxy's `poll` return so it can drain
/// and deliver. This is the standard "self-pipe / eventfd" trick for waking a `poll` from another thread.
pub struct WaylandEventPoster {
    /// The queue the proxy drains on wakeup. Unbounded: compositor events are low-rate (configures, buffer
    /// releases), and dropping the reader (proxy gone) is handled by [`post`](Self::post) silently.
    tx: Sender<WaylandMessage>,
    /// The eventfd the proxy polls; writing 8 bytes makes it readable. A `dup` of the read end's fd, so both
    /// refer to the same kernel eventfd object.
    wake: OwnedFd,
}

impl WaylandEventPoster {
    /// Hand one compositor event to the proxy: queue it, then wake the proxy's `poll`.
    ///
    /// Fire-and-forget. If the proxy has gone (the channel's receiver is dropped), the send fails and this
    /// returns without waking — there is nothing to deliver to. An `EAGAIN` on the eventfd write means the
    /// counter is already non-zero, i.e. the proxy is already scheduled to wake, which is fine.
    pub fn post(&self, msg: WaylandMessage) {
        // Queue first, so the message is visible before the wakeup the proxy reacts to.
        if self.tx.send(msg).is_err() {
            return; // the proxy thread is gone; nothing to deliver to.
        }
        // Increment the eventfd counter by one to make it readable and return the proxy's `poll`.
        let one: u64 = 1;
        // SAFETY: writing 8 bytes of a `u64` to an eventfd is the eventfd write contract; the fd is valid
        // for the borrow, and a short/EAGAIN write only means "already signalled", which needs no retry.
        unsafe {
            libc::write(
                self.wake.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                8,
            );
        }
    }
}

/// The read end of the compositor-event return path, owned by the proxy's serve loop: the queue it drains
/// and the eventfd it polls. Constructed by [`wayland_event_channel`] together with its [`WaylandEventPoster`].
pub struct WaylandEventInbox {
    /// The queue of events posted by the reader thread, drained on each eventfd wakeup.
    rx: Receiver<WaylandMessage>,
    /// The eventfd the serve loop polls; readable (counter > 0) exactly when events are queued.
    wake: OwnedFd,
}

/// Create a linked [`WaylandEventPoster`] / [`WaylandEventInbox`] pair over one shared eventfd.
///
/// The poster goes to the daemon's link reader thread (which turns each `S2C::WaylandEvent` into a
/// [`WaylandEventPoster::post`]); the inbox goes to [`run_with_events`]. Both ends hold their own `OwnedFd`
/// duped from the same underlying eventfd, so a write on the poster's side is seen by a poll on the inbox's.
///
/// # Failure modes
/// Returns an `io::Error` if `eventfd(2)` or the `dup(2)` of it fails (fd exhaustion) — surfaced so the
/// daemon fails at wiring time rather than silently losing every compositor event.
pub fn wayland_event_channel() -> std::io::Result<(WaylandEventPoster, WaylandEventInbox)> {
    let (tx, rx) = channel();
    // The eventfd: counter starts at 0 (not readable), non-blocking so the drain read never blocks, and
    // close-on-exec so it is not leaked into any child (the daemon spawns none, but this is the safe default).
    // SAFETY: `eventfd` returns a fresh fd or -1; we check and take ownership via `OwnedFd`.
    let read_raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if read_raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `read_raw` is a valid, freshly-owned fd we have not otherwise registered.
    let read_fd = unsafe { OwnedFd::from_raw_fd(read_raw) };
    // The write end is a dup of the same eventfd — one kernel object, two owned fds, so each side drops
    // cleanly and neither closes the other's.
    // SAFETY: `dup` of a valid fd returns a fresh fd or -1; checked, then owned.
    let write_raw = unsafe { libc::dup(read_raw) };
    if write_raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `write_raw` is a valid, freshly-owned fd.
    let write_fd = unsafe { OwnedFd::from_raw_fd(write_raw) };
    Ok((
        WaylandEventPoster { tx, wake: write_fd },
        WaylandEventInbox { rx, wake: read_fd },
    ))
}

/// Why a Wayland `Message` argument could not be translated to a [`WaylandArg`] for forwarding to S.
///
/// The structured tunnel maps `wayland-backend`'s `Argument` cases 1:1 to [`WaylandArg`], with one
/// exception: a file descriptor never crosses the network. Every `Argument::Fd` must become a
/// [`rayland_relay::BufferToken`] naming the S-side resource — but that resolution needs the
/// resource→memfd correlation and the buffer geometry gathered across the `create_params`/`add`/
/// `create_immed` sequence, which is the *next* sub-step (fd→token). Until it lands, encountering an fd is
/// a defined, distinguishable outcome rather than a silent drop or a panic.
#[derive(Debug, PartialEq, Eq)]
enum TranslateError {
    /// An `Argument::Fd` was seen but the fd→token resolution is not yet wired (WP0 Task 3b sub-step 4).
    /// vkcube emits no fd until `zwp_linux_buffer_params_v1.add`, so the fd-free requests it sends before
    /// that forward cleanly; this marks the point where the token path must take over.
    UnresolvedFd,
    /// An `Argument::NewId` reached [`translate_arg`], which cannot translate it (it needs the backend
    /// `Handle` for the object's version). [`translate_message`] handles `NewId` before delegating, so
    /// this is only reachable as an internal error and never in normal operation.
    UnexpectedNewId,
}

/// Translate one `wayland-backend` request argument into the wire-forwardable [`WaylandArg`].
///
/// # Mapping
/// The scalar cases are a direct, lossless remap: `Int`/`Uint`/`Fixed` carry their raw values (`Fixed`
/// stays raw `wl_fixed_t` bits so re-encoding on S is bit-exact); `Array` is copied verbatim. `Str` carries
/// the string's bytes **without** the wire's trailing NUL (`CString::as_bytes` already excludes it), and
/// preserves the null-vs-present distinction (`None` is the wire's absent string, distinct from an empty
/// one). `Object`/`NewId` carry the object's id in the *sender's* id space via `ObjectId::protocol_id`;
/// this crate does not translate ids across the `app_id ↔ s_id` map — that is the S client's job (Task 4).
///
/// # Failure modes
/// - `Argument::Fd` returns [`TranslateError::UnresolvedFd`]: an fd must become a [`rayland_relay::BufferToken`],
///   which is the fd→token sub-step. This function never forwards a raw fd.
/// - `Argument::NewId` is **not** handled here — it needs the backend `Handle` to read the new object's
///   version, so [`translate_message`] handles it directly via [`translate_new_id`]. A `NewId` reaching
///   this function is an internal error and returns [`TranslateError::UnexpectedNewId`].
fn translate_arg(arg: &Argument<ObjectId, OwnedFd>) -> Result<WaylandArg, TranslateError> {
    match arg {
        // Direct scalar remaps — same value, wire-forwardable type.
        Argument::Int(v) => Ok(WaylandArg::Int(*v)),
        Argument::Uint(v) => Ok(WaylandArg::Uint(*v)),
        // Keep the raw fixed-point bits so S re-encodes the exact same wl_fixed_t.
        Argument::Fixed(v) => Ok(WaylandArg::Fixed(*v)),
        // Bytes without the trailing NUL; `None` stays the wire's absent-string case.
        Argument::Str(s) => Ok(WaylandArg::Str(
            s.as_ref().map(|cstr| cstr.as_bytes().to_vec()),
        )),
        // Object references carry the sender-space id; the S client remaps it against the id map.
        Argument::Object(id) => Ok(WaylandArg::Object(id.protocol_id())),
        // NewId needs the backend handle for the new object's version; handled by translate_message.
        Argument::NewId(_) => Err(TranslateError::UnexpectedNewId),
        // Opaque array copied through unchanged.
        Argument::Array(bytes) => Ok(WaylandArg::Array((**bytes).clone())),
        // The one thing that cannot cross a network: resolved to a token in the fd→token sub-step.
        Argument::Fd(_) => Err(TranslateError::UnresolvedFd),
    }
}

/// Translate a `NewId` argument into a wire [`WaylandArg::NewId`] carrying the new object's interface
/// and version, so S can create the corresponding object with the right `child_spec`.
///
/// The interface comes from the `ObjectId` itself — the server backend stamped the child object with its
/// statically-known interface before delivering the request (see the module note on why the one request
/// with a *dynamic* child interface, `wl_registry.bind`, never reaches this path). The version comes from
/// the backend's object info; if that lookup somehow fails, it falls back to the interface's maximum
/// version, which is the most permissive safe choice for S's `send_request`.
fn translate_new_id(handle: &Handle, id: &ObjectId) -> WaylandArg {
    // The interface the backend assigned the new object — authoritative, no protocol table needed.
    let iface = id.interface();
    // The object's actual version (its parent's, which Wayland children inherit); fall back to the
    // interface's max version if the info lookup fails (it should not, for a just-created object).
    let version = handle
        .object_info(id.clone())
        .map(|info| info.version)
        .unwrap_or(iface.version);
    WaylandArg::NewId {
        id: id.protocol_id(),
        interface: iface.name.to_string(),
        version,
    }
}

/// Translate a whole `wayland-backend` request into the [`WaylandMessage`] forwarded to S.
///
/// Carries the sender object's id in the app's id space (`sender_id.protocol_id()`), the opcode verbatim,
/// and each argument: scalars and object refs via [`translate_arg`], and each `NewId` via
/// [`translate_new_id`] (which needs `handle` for the new object's version). Fails with the first argument
/// that cannot be translated (an unresolved fd), so a request bearing an fd is never partially forwarded.
fn translate_message(
    handle: &Handle,
    msg: &Message<ObjectId, OwnedFd>,
) -> Result<WaylandMessage, TranslateError> {
    // Every argument in wire order; the first failure aborts the whole message (no partial forward).
    let mut args = Vec::with_capacity(msg.args.len());
    for arg in &msg.args {
        // NewId needs the handle for the object's version; every other case is handle-free.
        let translated = match arg {
            Argument::NewId(id) => translate_new_id(handle, id),
            other => translate_arg(other)?,
        };
        args.push(translated);
    }
    Ok(WaylandMessage {
        object_id: msg.sender_id.protocol_id(),
        opcode: msg.opcode,
        args,
    })
}

// The generated interface descriptors for the globals WP0 advertises. Each is only used to name the
// interface (its `&'static Interface`) when creating the global; the proxy never builds typed objects.
use wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;
use wayland_protocols::xdg::shell::server::xdg_wm_base::XdgWmBase;
use wayland_server::protocol::wl_compositor::WlCompositor;
use wayland_server::protocol::wl_seat::WlSeat;

/// Where the proxy sends the application's translated Wayland requests.
///
/// This is the seam between the proxy and S. In WP0 Task 3b it is satisfied by a **stub collector** (the
/// integration test's recorder); Task 4 replaces it with the real link that carries `C2S::WaylandRequest`
/// over the existing QUIC connection to S's Wayland client. Keeping it a trait lets the proxy be proven
/// end-to-end against a recorder without standing up the whole S side.
///
/// Must be `Send + Sync`: the proxy holds it inside the backend's dispatch state, which the backend may
/// touch from its own threads, and the future real link is shared with the network layer.
pub trait WaylandSink: Send + Sync {
    /// Forward one translated request to S. The proxy has already mapped the `wayland-backend` `Message`
    /// to a wire [`WaylandMessage`]; the sink's job is only to deliver it (record it, or send it over the
    /// link). Delivery is fire-and-forget here — WP0 does not yet acknowledge individual requests.
    fn forward_request(&self, msg: WaylandMessage);

    /// Forward a global **bind** to S: the app bound `interface` at `version`, creating the object
    /// `app_object_id` in its own id space.
    ///
    /// A bind never arrives as a request (C's backend handles `wl_registry.bind` as a built-in and routes
    /// it to the global's handler), so it cannot ride [`Self::forward_request`]. Yet S must learn of it to
    /// reconstruct the object graph — every later request targets an object descended from a bound global.
    /// The real sink sends this as a [`rayland_relay::C2S::WaylandBind`]; a test collector records it. See
    /// the design doc §3, "Object-id mapping".
    fn forward_bind(&self, interface: &str, version: u32, app_object_id: u32);
}

/// Resolves a passed file descriptor's memfd identity to the S-side resource it names.
///
/// This is the buffer-by-token correlation key, spiked in WP0 Task 1: the swapchain image's dma-buf fd is
/// the exact `memfd:rayland-blob` `rayland-c` allocated in `shm.rs`, so its inode (`st_dev`+`st_ino`) —
/// which `rayland-c` already owns — identifies the resource. It is a trait for the same reason as
/// [`WaylandSink`]: the proxy can be proven end-to-end against a stub mapping (a test that registers a
/// known inode) before the real, `shm.rs`-backed registry is wired in when the proxy joins the daemon.
///
/// Must be `Send + Sync`: it lives inside the backend's dispatch state.
pub trait ResourceResolver: Send + Sync {
    /// Given a memfd's device and inode numbers, return the S-side resource id that memfd backs, or `None`
    /// if this inode is not a tracked resource (a foreign fd the proxy must not turn into a buffer token).
    fn resolve_inode(&self, dev: u64, ino: u64) -> Option<u32>;
}

/// Buffer-creation state accumulated across one `zwp_linux_buffer_params_v1` object's request sequence.
///
/// The token the proxy must emit needs facts split across two requests: the resource id, modifier and
/// plane layout come from `params.add` (which carries the swapchain memfd, the DRM modifier, and the
/// offset/stride of one plane), while width, height, and format come from the later
/// `params.create_immed`. This holds the `add`-time facts until `create_immed` supplies the rest and the
/// full [`BufferToken`] can be assembled.
#[derive(Default)]
struct PendingParams {
    /// The `zwp_linux_buffer_params_v1` object this state belongs to, as an **identity** rather than a
    /// number.
    ///
    /// # Why the map key is not enough
    /// `pending` is keyed by the params object's `protocol_id`, and a protocol id is a **slot number**,
    /// not an object identity — it is unique only among objects alive at one instant, and Wayland reuses
    /// it the moment the object dies. The witness log of 2026-08-29 shows app id 24 living as a
    /// `zwp_linux_buffer_params_v1`, dying, and being reborn as a `wl_callback`, so reuse here crosses
    /// interfaces too. Without this field, a *late* destroy of an old params object would wipe the state
    /// a **new** one had just accumulated, and its `create_immed` would refuse the buffer as UNRESOLVED —
    /// a missing frame behind a plausible-looking log line. `None` only until the first request that
    /// identifies the object.
    owner: Option<ObjectId>,
    /// The S-side resource id the passed memfd resolved to (`None` until a successful `add`, or if the fd
    /// did not correspond to a tracked resource).
    resource_id: Option<u32>,
    /// The DRM format modifier assembled from `add`'s `modifier_hi`/`modifier_lo` (`hi << 32 | lo`).
    modifier: u64,
    /// The plane's row pitch in bytes, straight from `add`. Carried rather than derived from the geometry
    /// — see [`BufferToken::stride`] for why `width × bpp` is a garbling assumption rather than a shortcut.
    stride: u32,
    /// The plane's byte offset within the dma-buf, straight from `add`. Carried for the same reason.
    offset: u32,
    /// Whether an `add` has already been seen for this params object. A **second** `add` means a
    /// multi-plane buffer, which poisons the object (see `unsupported`); this flag is how that is noticed.
    add_seen: bool,
    /// Set when this params object described something WP0's assumptions do not cover — a non-zero
    /// `plane_idx`, or more than one plane. A poisoned object forwards **no** token at `create_immed`.
    ///
    /// # Why refuse rather than approximate
    /// The proxy advertises exactly two single-plane LINEAR formats ([`ADVERTISED_FORMATS`]), so a
    /// multi-plane `add` means an assumption underneath WP0 has broken. Keeping the last plane's stride
    /// and presenting anyway would produce a garbled image with nothing logged; refusing makes the broken
    /// assumption visible in the log at the moment it breaks, and leaves the app with a locally valid
    /// `wl_buffer` that S is simply never told to present.
    unsupported: bool,
}

/// The dispatch state the backend threads through every callback (`ObjectData::request`,
/// `GlobalHandler::bind`). It holds the [`WaylandSink`] requests are forwarded to, the
/// [`ResourceResolver`] that turns a passed memfd into a resource id, and the per-`params`-object buffer
/// state accumulated between `add` and `create_immed`. It will grow to also hold the `app_id ↔ s_id`
/// bookkeeping (Task 4).
struct ProxyState {
    /// Where translated requests go (a collector in tests, the real link to S in Task 4).
    sink: Arc<dyn WaylandSink>,
    /// Turns a `params.add` fd into the S-side resource id it names (a stub in tests, `shm.rs` in Task 4).
    resolver: Arc<dyn ResourceResolver>,
    /// In-flight buffer state, keyed by the `zwp_linux_buffer_params_v1` object's id (the app's id space).
    pending: HashMap<u32, PendingParams>,
    /// Every object the app has created, mapping its protocol id (the app's id space) to the live
    /// [`ObjectId`]. Populated as globals are bound and requests create objects; entries removed on
    /// `destroyed`. This is what the **event return path** (Task 4.4) resolves an inbound `S2C::WaylandEvent`
    /// against: the event arrives keyed by the app-side id (S translated it back from its own id space), and
    /// [`deliver_event`] looks the id up here to name the `send_event` sender and any object arguments.
    objects: HashMap<u32, ObjectId>,
}

// The interface names and request opcodes the fd→token interception keys on. Opcodes are the request's
// index in its interface (document order in `linux-dmabuf-v1.xml`), which is how the Wayland wire numbers
// them. Named here so the match on `(interface, opcode)` reads in domain terms, not magic numbers.
/// The `zwp_linux_dmabuf_v1` global's interface name.
const IFACE_DMABUF: &str = "zwp_linux_dmabuf_v1";
/// The `zwp_linux_buffer_params_v1` object's interface name.
const IFACE_PARAMS: &str = "zwp_linux_buffer_params_v1";
/// `zwp_linux_dmabuf_v1.create_params` — makes a new params object (opcode 1).
const OP_CREATE_PARAMS: u16 = 1;
/// `zwp_linux_buffer_params_v1.add` — supplies the dma-buf fd + modifier for one plane (opcode 1).
const OP_PARAMS_ADD: u16 = 1;
/// `zwp_linux_buffer_params_v1.create` — the *asynchronous* buffer-creation request (opcode 2). The
/// `wl_buffer` id is returned later via a `created`/`failed` event rather than in the request itself. WP0
/// does not support this path (it has no event-return channel yet); it is recognised only to be refused
/// cleanly rather than mis-forwarded — see [`try_intercept_buffer`].
const OP_PARAMS_CREATE: u16 = 2;
/// `zwp_linux_buffer_params_v1.create_immed` — creates the `wl_buffer` synchronously (opcode 3).
const OP_PARAMS_CREATE_IMMED: u16 = 3;

/// The version the proxy advertises `zwp_linux_dmabuf_v1` at — **capped at 3 on purpose**.
///
/// # Why the cap is load-bearing (WP0 Task 4.4)
/// The interface descriptor supports higher versions, but Mesa's Venus WSI opts into the **v4 feedback**
/// path (`get_default_feedback`) whenever the bound version is `>= 4`, and that path delivers its supported
/// formats through a `format_table` **file descriptor** the client `mmap`s (`wsi_common_wayland.c:917-928`).
/// A file descriptor cannot cross a network. At v3 Mesa falls back to the plain `modifier` event
/// (`:830-852`), which is three integers and no fd — a complete path in this Mesa. So the proxy advertises
/// exactly v3, forcing the fd-free path, and answers the format query itself (see [`advertise_dmabuf_formats`]).
const DMABUF_MAX_VERSION: u32 = 3;

/// `zwp_linux_dmabuf_v1.modifier` **event** opcode (event index 1). Wire signature
/// `[format: uint, modifier_hi: uint, modifier_lo: uint]`, valid since interface v3. This is the event the
/// proxy synthesizes to advertise a supported format to the app.
const EV_DMABUF_MODIFIER: u16 = 1;

/// The DRM format modifier the proxy advertises: `DRM_FORMAT_MOD_LINEAR` = 0. WP0's swapchain images are
/// LINEAR HOST3D resources (offset 0, stride = width·bpp — see the 4.0-bis measurement), so LINEAR is the
/// layout the app's buffer will actually have on S.
const MOD_LINEAR: u64 = 0;

/// DRM fourcc `XR24` (`DRM_FORMAT_XRGB8888`) — the standard opaque 32-bit swapchain format.
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
/// DRM fourcc `AR24` (`DRM_FORMAT_ARGB8888`) — the standard 32-bit swapchain format with alpha.
const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;

/// The `(format, modifier)` pairs the proxy advertises to the app on a dmabuf bind. Kept minimal and
/// LINEAR: these are what a Venus WSI swapchain negotiates and what the HOST3D dma-buf on S exports, so the
/// app never picks a format S cannot present. Mesa's `pick_surface_format` needs only that this set is
/// non-empty (`count >= 1`); one pair would do, and both opaque and alpha are offered for completeness.
const ADVERTISED_FORMATS: [(u32, u64); 2] = [
    (DRM_FORMAT_XRGB8888, MOD_LINEAR),
    (DRM_FORMAT_ARGB8888, MOD_LINEAR),
];

/// Read a passed fd's memfd identity (`st_dev`, `st_ino`) via `fstat`, for resource correlation.
///
/// Returns `None` if `fstat` fails (e.g. the fd was already closed). Uses `libc::fstat` directly because
/// `std` exposes no stable `st_dev`/`st_ino` accessor on a bare fd without going through a `File` that
/// would take ownership.
fn fd_inode(fd: &OwnedFd) -> Option<(u64, u64)> {
    // SAFETY: a zeroed `stat` is a valid initialization target; `fstat` fully populates it on success and
    // we only read fields after checking the return code. The fd is valid for the borrow's duration.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut st) == 0 {
            Some((st.st_dev as u64, st.st_ino as u64))
        } else {
            None
        }
    }
}

/// The first `NewId` argument's id (in the app's id space), or `None` if the request creates no object.
/// The dmabuf requests the proxy intercepts each carry exactly one `new_id` (the params object, or the
/// buffer), so "first" is unambiguous for them.
fn first_new_id(msg: &Message<ObjectId, OwnedFd>) -> Option<u32> {
    msg.args.iter().find_map(|arg| match arg {
        Argument::NewId(id) => Some(id.protocol_id()),
        _ => None,
    })
}

/// The `ObjectId` of the object a request creates, or `None` if it creates none.
///
/// The identity-carrying counterpart of [`first_new_id`]: that returns the protocol id, which is a slot
/// number and cannot distinguish two objects that occupied the slot at different times. Anything that must
/// still be correct after the slot is recycled needs this instead.
fn first_new_object(msg: &Message<ObjectId, OwnedFd>) -> Option<ObjectId> {
    msg.args.iter().find_map(|arg| match arg {
        Argument::NewId(id) => Some(id.clone()),
        _ => None,
    })
}

/// Reassemble the 64-bit DRM modifier from a `params.add` request's `modifier_hi`/`modifier_lo` args.
///
/// Per `linux-dmabuf-v1.xml`, `add`'s args are `[fd, plane_idx, offset, stride, modifier_hi, modifier_lo]`,
/// so the modifier halves are args 4 and 5. Returns 0 (the LINEAR/implicit modifier's neighbour, and a
/// safe default) if the shape is unexpected.
fn params_modifier(msg: &Message<ObjectId, OwnedFd>) -> u64 {
    // Read one Uint arg at `idx`, or 0 if it is missing or not a Uint.
    let uint_at = |idx: usize| match msg.args.get(idx) {
        Some(Argument::Uint(v)) => *v as u64,
        _ => 0,
    };
    (uint_at(4) << 32) | uint_at(5)
}

/// Read a `params.add` request's plane layout: `(plane_idx, offset, stride)`.
///
/// Per `linux-dmabuf-v1.xml`, `add`'s args are `[fd, plane_idx, offset, stride, modifier_hi, modifier_lo]`,
/// so these are args 1–3. Companion to [`params_modifier`], which reads args 4–5 of the same request.
///
/// Returns zeros for any arg with an unexpected shape, matching the sibling readers' convention. For
/// `plane_idx` that default is the *permissive* one (plane 0 is the supported case), which is deliberate:
/// a malformed `add` is not evidence of a multi-plane buffer, and the buffer will be refused anyway if its
/// fd does not resolve. The caller applies the single-plane rule; this function only reads.
fn params_plane_layout(msg: &Message<ObjectId, OwnedFd>) -> (u32, u32, u32) {
    // Read one Uint arg at `idx`, or 0 if it is missing or not a Uint — `add`'s layout args are all uint.
    let uint_at = |idx: usize| match msg.args.get(idx) {
        Some(Argument::Uint(v)) => *v,
        _ => 0,
    };
    (uint_at(1), uint_at(2), uint_at(3))
}

/// Extract `(width, height, drm_format)` from a `params.create_immed` request.
///
/// Per the protocol, `create_immed`'s args are `[buffer_id(new_id), width(int), height(int), format(uint),
/// flags(uint)]`, so geometry is args 1–3. Width/height are protocol `int`s but never negative for a real
/// buffer, so they are carried as `u32`. Returns zeros for any arg with an unexpected shape.
fn immed_geometry(msg: &Message<ObjectId, OwnedFd>) -> (u32, u32, u32) {
    let width = match msg.args.get(1) {
        Some(Argument::Int(v)) => *v as u32,
        _ => 0,
    };
    let height = match msg.args.get(2) {
        Some(Argument::Int(v)) => *v as u32,
        _ => 0,
    };
    let format = match msg.args.get(3) {
        Some(Argument::Uint(v)) => *v,
        _ => 0,
    };
    (width, height, format)
}

/// The buffer-by-token interception — the crux of WP0.
///
/// Handles the `zwp_linux_dmabuf` requests that create a buffer, keeping their state and, at
/// `create_immed`, forwarding a [`BufferToken`] instead of the passed dma-buf fd. Returns `true` when it
/// consumed the request (so the caller must **not** run the generic translate-and-forward path — a raw fd
/// must never cross), `false` for any other request (which the caller forwards generically).
///
/// The buffer-creation family has a **fourth** request, `params.create` (opcode 2) — the asynchronous
/// counterpart of `create_immed`, which returns the `wl_buffer` id via a later `created`/`failed` event.
/// WP0 has no event-return channel yet, so that path is recognised and refused cleanly rather than
/// mis-forwarded; only `create_immed` actually produces a token. See the `OP_PARAMS_CREATE` arm.
///
/// # The sequence (Mesa's Venus WSI, `wsi_common_wayland`)
/// 1. `zwp_linux_dmabuf_v1.create_params` → a new `params` object. Recorded; **not** forwarded — S builds
///    its own params object from the token in Task 4, so the app's is purely local bookkeeping here.
/// 2. `params.add(fd, plane_idx, offset, stride, modifier_hi, modifier_lo)` → the swapchain memfd, its
///    plane layout, and its modifier. The fd is `fstat`ed, its inode resolved to an S-side resource id,
///    and all of it stashed in [`PendingParams`]. The fd is **dropped** — it never crosses the network.
///    `offset` and `stride` are carried on the token rather than recomputed on S, because a wrong stride
///    garbles the image instead of failing (see [`BufferToken::stride`]).
/// 3. `params.create_immed(buffer_id, width, height, format, flags)` → the app names the `wl_buffer`. Now
///    all the token's facts are in hand: it is assembled and forwarded as the message
///    `{ object_id: params, opcode: create_immed, args: [NewId(buffer_id), Buffer(token)] }`, which names
///    the buffer and the resource it denotes without any pixels or fd crossing.
///
/// # Multi-plane buffers are refused, not approximated
/// A single [`BufferToken`] describes exactly one plane. If a params object receives a **second** `add`,
/// or one whose `plane_idx` is not `0`, it is poisoned and its `create_immed` forwards nothing. The proxy
/// advertises only single-plane LINEAR formats ([`ADVERTISED_FORMATS`]), so either of those means an
/// assumption underneath WP0 has broken; presenting the last plane's layout anyway would garble the image
/// silently, while refusing makes the break visible in the log where it happens.
///
/// A `create_immed` that forwards nothing — because its fd never resolved (no matching resource, or a
/// missing `add`), or because the params object was poisoned — still leaves the app with a locally valid
/// `wl_buffer` (the backend created it from the `NewId`); S is simply never told to present it. The log
/// names which of the two refusals occurred, since they call for different investigations.
fn try_intercept_buffer(data: &mut ProxyState, msg: &Message<ObjectId, OwnedFd>) -> bool {
    let iface = msg.sender_id.interface().name;
    // The params/buffer objects live in the app's id space, keyed by the sending object's protocol id.
    let obj = msg.sender_id.protocol_id();
    match (iface, msg.opcode) {
        // Step 1: a new params object. Start tracking it; do not forward.
        (IFACE_DMABUF, OP_CREATE_PARAMS) => {
            if let (Some(params_id), Some(params_obj)) = (first_new_id(msg), first_new_object(msg))
            {
                // Store the object's identity alongside its state, so a later destroy of a *different*
                // object that happened to wear this same number cannot discard it.
                data.pending.insert(
                    params_id,
                    PendingParams {
                        owner: Some(params_obj),
                        ..PendingParams::default()
                    },
                );
                wp_log(&format!("intercept create_params -> params {params_id}"));
            }
            true
        }
        // Step 2: the dma-buf fd + modifier for this params object. Resolve and stash; drop the fd.
        (IFACE_PARAMS, OP_PARAMS_ADD) => {
            // The fd is add's first argument; resolve its memfd inode to an S-side resource id.
            let resource_id = match msg.args.first() {
                Some(Argument::Fd(fd)) => {
                    fd_inode(fd).and_then(|(dev, ino)| data.resolver.resolve_inode(dev, ino))
                }
                _ => None,
            };
            let modifier = params_modifier(msg);
            // The plane's own layout, which the token carries verbatim rather than deriving on S.
            let (plane_idx, offset, stride) = params_plane_layout(msg);
            // Record against this params object; `create_immed` will complete the token.
            let entry = data.pending.entry(obj).or_default();
            // The `add` arm can be the first request to identify this params object (its `create_params`
            // may have been missed), so record the owner here too — the sender *is* the params object.
            entry.owner = Some(msg.sender_id.clone());
            // A second `add` on one params object means a multi-plane buffer (each plane gets its own
            // `add`), and a non-zero `plane_idx` says so explicitly. Either way WP0's single-plane
            // assumption has broken, so poison the object rather than silently keeping the last plane's
            // layout and presenting a garbled image. See `PendingParams::unsupported`.
            if entry.add_seen || plane_idx != 0 {
                entry.unsupported = true;
                wp_log(&format!(
                    "intercept params {obj}.add -> UNSUPPORTED multi-plane buffer (plane_idx {plane_idx}, \
                     add #{}); this params object will forward no token",
                    if entry.add_seen { 2 } else { 1 }
                ));
            }
            entry.add_seen = true;
            entry.resource_id = resource_id;
            entry.modifier = modifier;
            entry.stride = stride;
            entry.offset = offset;
            wp_log(&format!(
                "intercept params {obj}.add -> resource {resource_id:?}, modifier {modifier:#x}, \
                 offset {offset}, stride {stride} (fd dropped)"
            ));
            true
        }
        // Step 3: name the wl_buffer. Assemble and forward the token; the fd never crossed.
        (IFACE_PARAMS, OP_PARAMS_CREATE_IMMED) => {
            let buffer_id = first_new_id(msg);
            let (width, height, drm_format) = immed_geometry(msg);
            // Consume the accumulated state for this params object.
            let pending = data.pending.remove(&obj).unwrap_or_default();
            // A poisoned params object (multi-plane; see the `add` arm) is treated exactly like an
            // unresolved fd: no token crosses. Folding it into the same `None` keeps one refusal path
            // rather than two, which is what the caller and the app already cope with correctly.
            let resolved = if pending.unsupported {
                None
            } else {
                pending.resource_id
            };
            match (buffer_id, resolved) {
                (Some(buffer_id), Some(resource_id)) => {
                    let token = BufferToken {
                        resource_id,
                        width,
                        height,
                        drm_format,
                        modifier: pending.modifier,
                        // Straight from `add`. Deriving either of these from `width`/`drm_format` on S
                        // would garble rather than fail — see `BufferToken::stride`.
                        stride: pending.stride,
                        offset: pending.offset,
                    };
                    // The proxy-internal encoding of create_immed: the buffer's app-side id and the token
                    // that names its S-side resource. S resolves the token to a real dma-buf (Task 4).
                    // The NewId names `wl_buffer` (version 1 — the interface has never had another), so S
                    // creates the right kind of object for the token.
                    let wire = WaylandMessage {
                        object_id: obj,
                        opcode: msg.opcode,
                        args: vec![
                            WaylandArg::NewId {
                                id: buffer_id,
                                interface: "wl_buffer".to_string(),
                                version: 1,
                            },
                            WaylandArg::Buffer(token),
                        ],
                    };
                    wp_log(&format!(
                        "intercept params {obj}.create_immed -> buffer {buffer_id} = resource {resource_id} \
                         ({width}x{height} fmt {drm_format:#x} offset {} stride {})",
                        pending.offset, pending.stride
                    ));
                    data.sink.forward_request(wire);
                }
                _ => {
                    // No resolved resource (a missing/foreign fd): the app keeps its local buffer, but S
                    // is told nothing — a buffer it cannot identify must not be presented.
                    // Name which of the two refusals happened: an fd that named no tracked resource, or a
                    // buffer whose plane layout WP0 does not cover. They need different investigations.
                    let reason = if pending.unsupported {
                        "UNSUPPORTED (multi-plane)"
                    } else {
                        "UNRESOLVED (fd named no tracked resource)"
                    };
                    wp_log(&format!(
                        "intercept params {obj}.create_immed -> {reason} (buffer {buffer_id:?}, resource {:?}); not forwarded",
                        pending.resource_id
                    ));
                }
            }
            true
        }
        // The asynchronous sibling of create_immed. WP0 has no event-return channel yet (the `created`/
        // `failed` event that carries the buffer id back cannot be delivered — that is Task 4's S side), so
        // this path cannot complete. Consume and log it rather than let it fall through to the generic
        // forward, which would ship geometry with no token to S and silently misrepresent an unsupported
        // request as handled. The `pending` entry is cleared so it does not leak.
        (IFACE_PARAMS, OP_PARAMS_CREATE) => {
            data.pending.remove(&obj);
            wp_log(&format!(
                "intercept params {obj}.create (async) -> UNSUPPORTED in WP0 (no event-return channel); not forwarded"
            ));
            true
        }
        // Anything else is not a buffer-creation request; the caller forwards it generically.
        _ => false,
    }
}

/// Answer the dmabuf format capability locally, by emitting a `modifier` event per advertised format on the
/// just-bound `zwp_linux_dmabuf_v1` object.
///
/// # Why this is synthesized here rather than relayed from S
/// Mesa's WSI discovers formats with a **bounded** roundtrip (`wl_display.sync`, then read whatever formats
/// arrived — abort if none). The proxy's `wayland-backend` server answers `sync` **locally and immediately**
/// (`server_impl/client.rs`), with no knowledge of S, so a `modifier` event relayed from S's compositor
/// would race that sync callback and lose it over a real network (winning only on loopback timing). Format
/// advertisement is a capability handshake, and the proxy answers it from known truth — the LINEAR
/// XRGB8888/ARGB8888 the HOST3D swapchain export uses (see [`ADVERTISED_FORMATS`]).
///
/// # Inputs / failure modes
/// - `handle`: the backend handle, for `send_event`.
/// - `dmabuf`: the bound object's id, in the app's id space — the event's sender.
/// - A `send_event` failure (an `InvalidId`, i.e. the client vanished mid-bind) is logged and skipped; it is
///   not fatal, and the next event or the client's disconnect handles the teardown.
fn advertise_dmabuf_formats(handle: &Handle, dmabuf: &ObjectId) {
    for (format, modifier) in ADVERTISED_FORMATS {
        // Split the 64-bit modifier into the wire's hi/lo halves, as `add`/`modifier` both carry it.
        let modifier_hi = (modifier >> 32) as u32;
        let modifier_lo = modifier as u32;
        // The modifier event: `[format, modifier_hi, modifier_lo]` on the dmabuf object (event opcode 1).
        let event = Message {
            sender_id: dmabuf.clone(),
            opcode: EV_DMABUF_MODIFIER,
            args: [
                Argument::Uint(format),
                Argument::Uint(modifier_hi),
                Argument::Uint(modifier_lo),
            ]
            .into_iter()
            .collect(),
        };
        if let Err(e) = handle.send_event(event) {
            wp_log(&format!(
                "failed to advertise dmabuf format {format:#x} to the app: {e:?}"
            ));
            return;
        }
    }
    wp_log(&format!(
        "advertised {} LINEAR dmabuf format(s) to the app on object {}",
        ADVERTISED_FORMATS.len(),
        dmabuf.protocol_id()
    ));
}

/// Per-connection state the backend holds for the application's client. Empty for now — WP0 has exactly
/// one client (the app) and needs no per-client bookkeeping beyond the object map a later sub-step adds.
/// The `initialized`/`disconnected` notifications are left at their trait defaults.
struct ProxyClientData;
impl ClientData for ProxyClientData {}

/// The handler the backend invokes when a client binds one of our advertised globals.
///
/// A `wayland-backend` global is bound via `wl_registry.bind`; the backend then calls [`Self::bind`] to
/// obtain the [`ObjectData`] for the freshly created global object (the app's `wl_compositor`,
/// `xdg_wm_base`, etc.). One handler instance is created per advertised global; it carries the interface's
/// human name purely so the bring-up log can say *which* global was bound.
struct ProxyGlobal {
    /// The interface name (`"wl_compositor"`, …) — diagnostic only; the backend already knows the binding.
    iface_name: &'static str,
}

impl GlobalHandler<ProxyState> for ProxyGlobal {
    /// A client has bound this global, creating `object_id`. Return the object data that will receive the
    /// bound object's future requests. In this sub-step that data is inert — it does the new-object
    /// bookkeeping the backend contract requires but forwards nothing.
    fn bind(
        self: Arc<Self>,
        handle: &Handle,
        data: &mut ProxyState,
        _client_id: ClientId,
        _global_id: GlobalId,
        object_id: ObjectId,
    ) -> Arc<dyn ObjectData<ProxyState>> {
        // The version the app bound this global at, which S must bind at too so the object's request/event
        // set matches. The child object carries it; fall back to the interface's max version if the info
        // lookup fails (it should not, for a just-bound object).
        let version = handle
            .object_info(object_id.clone())
            .map(|info| info.version)
            .unwrap_or(object_id.interface().version);
        wp_log(&format!(
            "bound global {} v{version} -> object {}; forwarding bind to S",
            self.iface_name,
            object_id.protocol_id()
        ));
        // Forward the bind to S so it can reconstruct this global on its own compositor connection and map
        // the app's object id to the S-side one. Without this, S cannot replay any request the app makes
        // against this object (the app's `wl_registry.bind` itself never crosses — see `WaylandSink`).
        data.sink
            .forward_bind(self.iface_name, version, object_id.protocol_id());
        // WP0 Task 4.4: answer the dmabuf format capability locally, right here on bind. Mesa's WSI queries
        // supported formats with a bounded roundtrip the proxy's backend answers itself (see
        // `advertise_dmabuf_formats`), so the `modifier` events must be synthesized now rather than relayed
        // from S. Guarded on the bound version being >= 3, since the `modifier` event exists only from v3
        // (the proxy caps the advertisement at exactly v3, so a real client always binds v3 here).
        if self.iface_name == IFACE_DMABUF && version >= 3 {
            advertise_dmabuf_formats(handle, &object_id);
        }
        // Record the bound global in the app-id → ObjectId map so a compositor event targeting it (e.g. a
        // `wl_seat.capabilities`, or an `xdg_wm_base.ping` on a descendant) can be delivered back — see
        // [`deliver_event`] and [`ProxyState::objects`].
        data.objects
            .insert(object_id.protocol_id(), object_id.clone());
        // The bound object forwards its requests through the same inert data as any other object.
        Arc::new(ProxyObjectData)
    }
}

/// The per-object request hook. Every Wayland object the app creates — the globals it binds and every
/// object made from them — carries an instance of this. In the globals+connect sub-step it does not yet
/// forward to S; it only honours the backend's new-object contract (see [`Self::request`]).
struct ProxyObjectData;

impl ObjectData<ProxyState> for ProxyObjectData {
    /// Dispatch one request from the application.
    ///
    /// # The new-object contract
    /// If the request carries a `NewId` argument (the app is creating an object — `create_surface`,
    /// `get_xdg_surface`, `create_params`, …), this method **must** return the [`ObjectData`] for that new
    /// object, or the backend has no handler for it. We hand back another inert [`ProxyObjectData`] so the
    /// whole object graph the app builds is covered. A request with no `NewId` returns `None`.
    ///
    /// Forwarding the request to S, translating its arguments, and the `params.add` fd→token interception
    /// are the next sub-step; here the request is observed (optionally logged) and dropped.
    fn request(
        self: Arc<Self>,
        handle: &Handle,
        data: &mut ProxyState,
        _client_id: ClientId,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<ProxyState>>> {
        // Does this request create a new object? The backend needs data for it if so — decided *before*
        // `msg` is consumed by translation below.
        let makes_new_object = msg.args.iter().any(|arg| matches!(arg, Argument::NewId(_)));
        // Record every object this request creates in the app-id → ObjectId map, so a later compositor event
        // targeting it (an `xdg_surface.configure`, a `wl_buffer.release`) can be delivered back — see
        // [`deliver_event`]. The new object's `ObjectId` is carried in the request itself as an
        // `Argument::NewId`, so this needs no separate lookup and covers the intercepted buffer path too.
        for arg in &msg.args {
            if let Argument::NewId(id) = arg {
                data.objects.insert(id.protocol_id(), id.clone());
                // Part of the return-path witness: an event can only be delivered to an object that is in
                // this map, so its comings and goings are the ground truth behind every
                // `drop:unknown-object`. Short-lived objects (a `wl_callback`, destroyed by its own `done`)
                // make this a live question rather than bookkeeping.
                wp_log(&format!(
                    "objects+ app_obj={} {}",
                    id.protocol_id(),
                    id.interface().name
                ));
            }
        }
        // The one special case first: buffer-by-token interception on the dmabuf path. If it consumed the
        // request (a `create_params`/`add`/`create_immed`), the generic path must not also run — a raw fd
        // must never be forwarded.
        if !try_intercept_buffer(data, &msg) {
            // Generic path: translate the request to its wire form and forward it to S. A stray unresolved
            // fd outside the intercepted dmabuf path is not expected in WP0; log and drop rather than
            // forward a raw fd or panic.
            match translate_message(handle, &msg) {
                Ok(wire_msg) => {
                    wp_log(&format!(
                        "forward obj {} opcode {} ({} args)",
                        wire_msg.object_id,
                        wire_msg.opcode,
                        wire_msg.args.len()
                    ));
                    data.sink.forward_request(wire_msg);
                }
                // A NewId reaching translate_arg is an internal bug (translate_message handles it); log
                // and drop rather than panic, treating it like the other untranslatable case.
                Err(TranslateError::UnexpectedNewId) => {
                    wp_log(&format!(
                        "BUG obj {} opcode {} — NewId reached translate_arg; request dropped",
                        msg.sender_id.protocol_id(),
                        msg.opcode
                    ));
                }
                Err(TranslateError::UnresolvedFd) => {
                    // An fd on a non-dmabuf request: unhandled in WP0, dropped rather than forwarded raw.
                    wp_log(&format!(
                        "SKIP (unresolved fd) obj {} opcode {} — no token path for this request",
                        msg.sender_id.protocol_id(),
                        msg.opcode
                    ));
                }
            }
        }
        // Honour the contract: return handler data exactly when a new object was created, so it too
        // forwards its future requests.
        makes_new_object.then(|| Arc::new(ProxyObjectData) as Arc<dyn ObjectData<ProxyState>>)
    }

    /// The object was destroyed. Release any per-object buffer state so it cannot leak.
    ///
    /// If the destroyed object was a `zwp_linux_buffer_params_v1` that never reached `create_immed` (it was
    /// abandoned, or completed via the unsupported async `create` path), its [`PendingParams`] entry would
    /// otherwise linger for the process's life. Removing by the object's id is a cheap no-op for every other
    /// object (a surface, a toplevel), so it is done unconditionally. A later sub-step also drops the object
    /// from the `app_id ↔ s_id` map here.
    fn destroyed(
        self: Arc<Self>,
        _handle: &Handle,
        data: &mut ProxyState,
        _client_id: ClientId,
        object_id: ObjectId,
    ) {
        // **A protocol id is a slot number, not an object identity.**
        //
        // It is unique only among objects alive at one instant: the moment an object dies, libwayland
        // hands its number to the next object the application creates. And this cleanup runs *late* —
        // the backend reports a destruction after it has already dispatched the requests that followed
        // it — so by the time we get here, the slot may already belong to a **different, live** object.
        //
        // Removing by number therefore deletes whoever currently holds the slot, which is how the
        // application's second frame callback was silently unregistered and its `wl_callback.done`
        // dropped as `unknown-object`, freezing vkcube's cube after a single frame
        // (`docs/data/2026-08-29-wp0-event-witness/`). Every component involved was individually
        // correct; the bug lived entirely in this composition.
        //
        // So: remove only when the entry still holds *the object being destroyed*. A mismatch means the
        // slot has been refilled and the newcomer must survive.
        let slot = object_id.protocol_id();
        // Buffer state first (a no-op unless this was a params object). Its identity lives in the value's
        // `owner` field, since the value is not itself an `ObjectId` — see `PendingParams::owner`.
        if data
            .pending
            .get(&slot)
            .is_some_and(|p| p.owner.as_ref() == Some(&object_id))
        {
            data.pending.remove(&slot);
        }
        // Then the event-delivery map, whose values *are* `ObjectId`s and compare directly. `ObjectId`'s
        // `PartialEq` includes an internal per-client serial as well as the number, so two objects that
        // shared a slot at different times compare unequal — which is the property this rests on.
        let was_known = match data.objects.get(&slot) {
            Some(live) if *live == object_id => data.objects.remove(&slot).is_some(),
            // The slot is empty, or has already been refilled by a newer object: nothing of *this*
            // object's remains, and the newcomer's entry must be left exactly where it is.
            _ => false,
        };
        // The other half of the witness. A `wl_callback` is destroyed by delivering its own `done`, so this
        // fires *during* event delivery — and whether a later id of the same number is this object coming
        // back or a different object entirely is exactly what a `drop:unknown-object` turns on.
        wp_log(&format!(
            "objects- app_obj={} {} (was_known={was_known})",
            object_id.protocol_id(),
            object_id.interface().name
        ));
    }
}

/// Run the Wayland proxy: advertise the WP0 globals, bind the socket the application connects to, and
/// drive the backend so the application can connect and bind those globals.
///
/// # Inputs / outputs
/// - `socket_path`: the Unix socket the application's `WAYLAND_DISPLAY` names; where the app dials in.
///   Bound fresh — a stale socket file from a previous run is removed first so the bind cannot fail on
///   `EADDRINUSE` for a path nothing is listening on.
/// - Never returns under normal operation: it runs the accept-and-dispatch loop until the process exits or
///   an unrecoverable I/O error occurs (then returns that error).
///
/// # Failure modes
/// - The backend cannot be created (`wayland_server::backend::InitError`) — surfaced as an error.
/// - The socket cannot be bound (path unwritable, or a live listener already owns it) — surfaced as an
///   `io::Error`.
/// - `poll(2)` fails other than by interruption (`EINTR`, which is retried) — surfaced as an `io::Error`.
///
/// `sink` is where the app's translated requests are forwarded — a collector in tests, the real link to
/// S in Task 4. `resolver` turns a passed `params.add` dma-buf fd into the S-side resource id it names — a
/// stub map in tests, the `shm.rs`-backed registry in Task 4.
///
/// This is the no-return-path form: it runs with an [`WaylandEventInbox`] whose poster is dropped, so no
/// compositor events are ever delivered back to the app. The daemon uses [`run_with_events`] to wire the
/// real return path; this convenience keeps the many proxy tests that do not exercise events unchanged.
pub fn run(
    socket_path: PathBuf,
    sink: Arc<dyn WaylandSink>,
    resolver: Arc<dyn ResourceResolver>,
) -> anyhow::Result<()> {
    // A never-firing inbox: its poster is dropped immediately, so the eventfd is never written and the
    // serve loop only ever wakes for the listener and the backend.
    let (_poster, inbox) = wayland_event_channel()?;
    drop(_poster);
    run_with_events(socket_path, sink, resolver, inbox)
}

/// Run the Wayland proxy with a compositor-event return path (WP0 Task 4.4).
///
/// Identical to [`run`], but the `inbox` is fed by a [`WaylandEventPoster`] the daemon hands to its link
/// reader thread: each `S2C::WaylandEvent` from S is `post`ed, waking this loop, which translates the event's
/// ids back into the app's object graph and delivers it with `Handle::send_event` (see [`deliver_event`]).
/// This is how `xdg_surface.configure`, `wl_buffer.release`, and every other compositor event reaches the
/// application.
pub fn run_with_events(
    socket_path: PathBuf,
    sink: Arc<dyn WaylandSink>,
    resolver: Arc<dyn ResourceResolver>,
    inbox: WaylandEventInbox,
) -> anyhow::Result<()> {
    // A stale socket file left by a crashed run would make `bind` fail; remove it (ignore "not found").
    let _ = std::fs::remove_file(&socket_path);
    // The pure-Rust Wayland server backend. `ProxyState` is the dispatch state it threads to callbacks.
    let mut backend = Backend::<ProxyState>::new()?;
    // Advertise every global vkcube binds (spec §6). `wl_display`/`wl_registry` are backend built-ins; we
    // add only the application-visible globals. Each is advertised at the full version the interface
    // descriptor knows — except dmabuf, capped at v3 (see [`DMABUF_MAX_VERSION`]).
    let handle = backend.handle();
    // Advertise each global at the descriptor's max version (`u32::MAX` is clamped down to it), *except*
    // `zwp_linux_dmabuf_v1`, which is capped at v3 so Mesa takes the fd-free format path — see
    // [`DMABUF_MAX_VERSION`].
    create_global::<WlCompositor>(&handle, u32::MAX);
    create_global::<XdgWmBase>(&handle, u32::MAX);
    create_global::<ZwpLinuxDmabufV1>(&handle, DMABUF_MAX_VERSION);
    create_global::<WlSeat>(&handle, u32::MAX);
    // Where the application dials in. WP0 serves exactly one such socket.
    let listener = UnixListener::bind(&socket_path)?;
    // The dispatch loop owns this state; it is threaded to every callback by the backend.
    let mut state = ProxyState {
        sink,
        resolver,
        pending: HashMap::new(),
        objects: HashMap::new(),
    };
    wp_log(&format!("proxy listening at {}", socket_path.display()));
    serve(&mut backend, &listener, &mut state, &inbox)
}

/// Advertise one global for interface `R` at version `min(max_version, descriptor max)`, wiring it to a
/// fresh [`ProxyGlobal`] handler.
///
/// `R` is a generated object type (`WlCompositor`, `XdgWmBase`, …); `R::interface()` yields the
/// `&'static Interface` the backend needs, and its `.version` is the maximum the descriptor supports.
/// `max_version` caps the advertisement below that — pass `u32::MAX` to advertise the full descriptor
/// version, or a specific cap (as WP0 does for `zwp_linux_dmabuf_v1`; see [`DMABUF_MAX_VERSION`]). The
/// `.min` guarantees the proxy never advertises a version the descriptor cannot actually serve.
fn create_global<R: Resource>(handle: &Handle, max_version: u32) -> GlobalId {
    let iface = R::interface();
    let version = max_version.min(iface.version);
    handle.create_global::<ProxyState>(
        iface,
        version,
        Arc::new(ProxyGlobal {
            iface_name: iface.name,
        }),
    )
}

/// The accept-and-dispatch loop.
///
/// Polls three sources: the listening socket (a new application connecting), the backend's own poll fd
/// (a connected client has pending requests), and the event inbox's eventfd (S sent a compositor event to
/// deliver back). On a connection, insert the client; on backend readiness, dispatch and flush; on an
/// inbox wakeup, drain the queued events, deliver each with `send_event`, and flush. Runs until an
/// unrecoverable error.
///
/// `EINTR` from `poll` is retried rather than surfaced — a signal is not a failure of the loop.
fn serve(
    backend: &mut Backend<ProxyState>,
    listener: &UnixListener,
    state: &mut ProxyState,
    inbox: &WaylandEventInbox,
) -> anyhow::Result<()> {
    use std::os::fd::{AsFd, AsRawFd};
    loop {
        // Three fds to watch: 0 the listener, 1 the backend, 2 the event inbox's eventfd. `poll` fills
        // `revents` in place.
        let mut fds = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: backend.poll_fd().as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: inbox.wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid, initialized array of 3 pollfds for the duration of the call; a negative
        // timeout blocks until an fd is ready. `poll` only writes the `revents` fields.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            // A signal interrupted the wait; that is expected, not a failure. Re-poll.
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        // A new application is connecting: accept it and register it with the backend.
        if fds[0].revents & libc::POLLIN != 0 {
            accept_client(backend, listener)?;
        }
        // A connected client has pending requests (or the backend needs servicing): drain and flush.
        if fds[1].revents & libc::POLLIN != 0 {
            backend.dispatch_all_clients(state)?;
            backend.flush(None)?;
        }
        // S sent one or more compositor events to deliver back to the app.
        if fds[2].revents & libc::POLLIN != 0 {
            drain_events(backend, state, inbox)?;
        }
    }
}

/// Drain the event inbox and deliver every queued compositor event to the app, then flush.
///
/// Called when the inbox's eventfd polled readable. It first reads the eventfd to reset its counter (so the
/// next `poll` blocks until the *next* `post`), then drains the channel non-blocking and delivers each
/// event via [`deliver_event`]. A single wakeup can carry several events — the eventfd counts posts but the
/// channel holds them — so it drains until empty rather than one per wakeup.
fn drain_events(
    backend: &mut Backend<ProxyState>,
    state: &mut ProxyState,
    inbox: &WaylandEventInbox,
) -> anyhow::Result<()> {
    // Reset the eventfd counter: read the 8-byte counter value (discarded). `EFD_NONBLOCK` means a spurious
    // wakeup with the counter already 0 returns `EAGAIN`, which is not an error here.
    let mut buf = [0u8; 8];
    // SAFETY: reading 8 bytes into a valid local buffer from the eventfd; the return value is ignored
    // because any outcome (counter read, or EAGAIN) leaves us correctly proceeding to drain the channel.
    unsafe {
        libc::read(
            inbox.wake.as_raw_fd(),
            buf.as_mut_ptr() as *mut libc::c_void,
            8,
        );
    }
    let handle = backend.handle();
    // Drain everything queued. `try_recv` returns `Empty` when drained and `Disconnected` if the poster is
    // gone; both end the drain.
    while let Ok(msg) = inbox.rx.try_recv() {
        deliver_event(&handle, state, msg);
    }
    // Flush so the delivered events reach the app's socket promptly rather than on the next dispatch.
    backend.flush(None)?;
    Ok(())
}

/// Deliver one compositor event, arriving in the **app's** id space, to the application via `send_event`.
///
/// # Id translation
/// The event has already been translated S→app on S's side (the reverse of the request path's app→S map),
/// so `msg.object_id` and any `Object` argument name objects in the app's own id space. This function
/// resolves those numeric ids to live [`ObjectId`]s via [`ProxyState::objects`] and rebuilds the backend
/// `Message` `send_event` needs.
///
/// # What it forwards, and what it drops
/// Scalars (`Int`/`Uint`/`Fixed`/`Str`/`Array`) map straight across. An `Object` argument is resolved
/// through the map (dropping the event if unknown — a defensive branch, not a live one for WP0). A `NewId`
/// argument (a compositor *creating* an object for the app — e.g. a data offer) is **not** supported by
/// WP0's return path and drops the whole event with a log, as does an unexpected `Buffer` token (which is a
/// request-direction concern only). None of the events a minimal WSI client relies on
/// (`xdg_surface.configure`, `xdg_toplevel.configure`, `wl_buffer.release`, `xdg_wm_base.ping`,
/// `wl_callback.done`) carry either, so this covers the walking skeleton.
///
/// # Failure modes
/// An unknown target object (destroyed, or never created) drops the event with a log. A `send_event`
/// failure (the client vanished) is logged; it is not fatal to the proxy.
/// `"<interface>.<event>"` for an event on `object`, or `"<interface>.#<opcode>"` if the opcode has no
/// descriptor.
///
/// # Why by name
/// This is the C-side half of the WP0 return-path witness, and it exists so the two ends' logs diff
/// directly. An opcode is an index into one interface's event list, and the two ends log against two
/// different id spaces, so a bare `opcode 0` in each log says nothing about whether it is the *same* event.
/// `wl_callback.done` present on S and absent on C is an answer.
///
/// # Failure mode
/// Out-of-range opcodes return the `#n` form rather than panicking: the opcode originates on S's
/// compositor, so an interface version newer than this build's descriptor can legitimately produce one.
/// Losing the name must never cost the log line.
fn event_label(object: &ObjectId, opcode: u16) -> String {
    let interface = object.interface();
    match interface.events.get(opcode as usize) {
        Some(desc) => format!("{}.{}", interface.name, desc.name),
        None => format!("{}.#{}", interface.name, opcode),
    }
}

/// Report an event the proxy refused to deliver to the application.
///
/// **Unconditional, unlike [`wp_log`].** A dropped event is rare and each one is a finding — an event S's
/// compositor emitted that the application will never see — so it must not depend on a diagnostic switch
/// being set. This mirrors `rayland-s`'s drop reporting exactly, so the two ends' drops are comparable
/// without having to arrange for matching environments.
fn wp_drop(msg: &str) {
    eprintln!("[wp-event][C] {msg}");
}

fn deliver_event(handle: &Handle, state: &ProxyState, msg: WaylandMessage) {
    // The event's target object, in the app's id space, must be one the proxy created and still holds.
    let Some(sender) = state.objects.get(&msg.object_id).cloned() else {
        // No live proxy object with that app-side id. **What this means for the app:** the event is lost,
        // and if the app is blocked on it, this is the reason. The interface cannot be named here — the id
        // is precisely what could not be resolved — so the raw opcode is all there is to report.
        wp_drop(&format!(
            "drop:unknown-object app_obj={} opcode={}: no live proxy object",
            msg.object_id, msg.opcode
        ));
        return;
    };
    // A `wl_callback.done` (the only event `wl_callback` has, opcode 0) is the compositor telling the
    // application it may draw again. Recorded so the poll-cycle decomposition can charge time spent
    // waiting for the compositor to the compositor rather than to Rayland — see `relaxstat::Event`.
    if sender.interface().name == "wl_callback" && msg.opcode == 0 {
        crate::relaxstat::note(crate::relaxstat::Event::FrameCallback);
    }
    // Rebuild the argument list in the backend's `send_event` form (`Argument<ObjectId, RawFd>`).
    let mut args: Vec<Argument<ObjectId, RawFd>> = Vec::with_capacity(msg.args.len());
    for arg in &msg.args {
        match arg {
            WaylandArg::Int(v) => args.push(Argument::Int(*v)),
            WaylandArg::Uint(v) => args.push(Argument::Uint(*v)),
            WaylandArg::Fixed(v) => args.push(Argument::Fixed(*v)),
            // Re-add the wire NUL the tunnel stripped; a wayland string has no interior NUL.
            WaylandArg::Str(s) => args.push(Argument::Str(
                s.as_ref()
                    .and_then(|b| CString::new(b.clone()).ok())
                    .map(Box::new),
            )),
            WaylandArg::Array(b) => args.push(Argument::Array(Box::new(b.clone()))),
            // An object reference: resolve through the map. If the app never learned of the referenced
            // object, dropping the whole event is safer than delivering a dangling reference — but the WP0
            // event set carries no `Object` args, so this is a defensive branch, not a live one.
            WaylandArg::Object(id) => match state.objects.get(id).cloned() {
                Some(obj) => args.push(Argument::Object(obj)),
                None => {
                    // **What this means for the app:** the whole event is lost, not just the argument.
                    // Delivering a dangling reference would be worse — the app would resolve it against
                    // one of its own unrelated objects.
                    wp_drop(&format!(
                        "drop:unmapped-object-arg app_obj={} {}: references unknown object {id}",
                        msg.object_id,
                        event_label(&sender, msg.opcode)
                    ));
                    return;
                }
            },
            // A compositor-created object: unsupported in WP0's return path; drop the whole event.
            WaylandArg::NewId { id, interface, .. } => {
                // **What this means for the app:** lost. WP0's return path cannot mint an object in the
                // app's id space, so anything delivered this way never arrives. S drops these too, before
                // they reach the link — seeing one here would mean the two ends disagree about the rule.
                wp_drop(&format!(
                    "drop:carries-new-id app_obj={} {}: NewId ({interface} {id}) is not delivered back yet",
                    msg.object_id,
                    event_label(&sender, msg.opcode)
                ));
                return;
            }
            // A buffer token in a compositor→app event is never expected; drop rather than mis-deliver.
            WaylandArg::Buffer(_) => {
                // A buffer token travels app→S, never back. **What this means for the app:** lost, and it
                // also means something upstream is confused, since nothing constructs such an event.
                wp_drop(&format!(
                    "drop:unexpected-buffer-token app_obj={} {}",
                    msg.object_id,
                    event_label(&sender, msg.opcode)
                ));
                return;
            }
        }
    }
    // Built before `sender` is moved into the message below: success and failure must name the event
    // identically, and after the move the interface is no longer reachable.
    let label = event_label(&sender, msg.opcode);
    let arg_count = msg.args.len();
    let event = Message {
        sender_id: sender,
        opcode: msg.opcode,
        args: args.into_iter().collect(),
    };
    if let Err(e) = handle.send_event(event) {
        // The event was well-formed and still did not reach the app: a socket-level failure, not a
        // translation one. Unconditional for the same reason the drops are — the app is missing an event.
        wp_drop(&format!(
            "drop:send-event-failed app_obj={} {}: {e:?}",
            msg.object_id, label
        ));
    } else {
        // The witness's answer line: this event **reached the application**. Its counterpart is S's
        // `emit`; an S `emit` with no C `delivered` means the event was lost on the link between them.
        if std::env::var_os("RAYLAND_WP_LOG").is_some() {
            // **The scalar arguments, not just how many there are.** An event count answers "did it
            // arrive"; it cannot answer "what did it say", and on 2026-09-01 that was exactly the
            // question: a live run showed 21 `xdg_toplevel.configure` events producing 21 swapchain
            // recreations, each costing the application ~1 s over the relay, and deciding whether the
            // application was right to recreate needs the *size* the compositor sent. Only `Int` and
            // `Uint` are printed — a string or array argument could carry application content, and
            // this is a diagnostic, not a place to leak a title or a clipboard.
            let scalars: Vec<String> = msg
                .args
                .iter()
                .filter_map(|a| match a {
                    WaylandArg::Int(v) => Some(v.to_string()),
                    WaylandArg::Uint(v) => Some(v.to_string()),
                    _ => None,
                })
                .collect();
            eprintln!(
                "[wp-event][C] delivered app_obj={} {} args={} scalars=[{}]",
                msg.object_id,
                label,
                arg_count,
                scalars.join(",")
            );
        }
    }
}

/// Accept one pending connection on `listener` and hand its stream to the backend as a new client.
///
/// The stream is registered with a fresh [`ProxyClientData`]; from then on the backend delivers that
/// client's requests to the objects' [`ObjectData::request`]. After registering, flush so the client
/// immediately sees the initial protocol state (e.g. the display's globals).
fn accept_client(backend: &mut Backend<ProxyState>, listener: &UnixListener) -> anyhow::Result<()> {
    // `poll` reported the listener readable, so this accept does not block.
    let (stream, _addr): (UnixStream, _) = listener.accept()?;
    let mut handle = backend.handle();
    let client_id = handle.insert_client(stream, Arc::new(ProxyClientData))?;
    wp_log(&format!("application connected: client {:?}", client_id));
    // Push any pending events (the registry/globals) so the client can proceed without first sending.
    backend.flush(None)?;
    Ok(())
}

/// Emit a bring-up diagnostic line to stderr, gated on `RAYLAND_WP_LOG` so normal runs stay silent.
///
/// This is scaffolding for the globals+connect sub-step — it lets a human confirm which globals vkcube
/// binds and which requests arrive. It follows the repository's env-gated-stderr convention rather than a
/// logging framework; a later sub-step routes real events over the link instead.
fn wp_log(msg: &str) {
    // Only speak when explicitly asked, so the proxy is quiet in production and loud during bring-up.
    if std::env::var_os("RAYLAND_WP_LOG").is_some() {
        // **Timestamped, and that is what makes a frame timeline free.** Establishing whether a
        // frame-rate figure was disturbed needs to know *when* each frame was presented, and the
        // obvious way — sampling the presented-frame count on a timer — was tried twice and cost more
        // than it measured: a `grep -c` of the whole log ten times a second took the frame rate from
        // 213 attaches to 98, and an offset-carrying version that spawned four processes per sample was
        // no better. This line already exists for every forwarded request; stamping it turns the
        // timeline into something derived **offline** from the log, at the cost of one clock read on a
        // path that is already opt-in diagnostic. Exact inter-frame gaps, no sampler, and identical on
        // both topologies because nothing is polled.
        eprintln!(
            "[wp-proxy] t_ns={} {msg}",
            rayland_relay::trace::monotonic_ns()
        );
    }
}

#[cfg(test)]
mod tests {
    //! Pure translation tests for the scalar argument cases.
    //!
    //! `Object`/`NewId`/`Fd` need a real `ObjectId`/`OwnedFd`, which only the backend mints, so they are
    //! covered by the integration forward test (a real client sends real objects). The cases here are the
    //! ones with actual mapping logic worth pinning: the value scalars, and — the subtle one — `Str`'s
    //! NUL handling and its null-vs-present distinction.
    use super::*;
    use std::ffi::CString;

    #[test]
    fn scalar_args_map_one_to_one() {
        // Int/Uint/Fixed carry their raw values unchanged; Fixed stays raw wl_fixed_t bits.
        assert_eq!(
            translate_arg(&Argument::<ObjectId, OwnedFd>::Int(-7)),
            Ok(WaylandArg::Int(-7))
        );
        assert_eq!(
            translate_arg(&Argument::<ObjectId, OwnedFd>::Uint(42)),
            Ok(WaylandArg::Uint(42))
        );
        assert_eq!(
            translate_arg(&Argument::<ObjectId, OwnedFd>::Fixed(256)),
            Ok(WaylandArg::Fixed(256))
        );
    }

    #[test]
    fn array_arg_is_copied_verbatim() {
        let bytes = vec![0u8, 1, 2, 250, 255];
        assert_eq!(
            translate_arg(&Argument::<ObjectId, OwnedFd>::Array(Box::new(
                bytes.clone()
            ))),
            Ok(WaylandArg::Array(bytes))
        );
    }

    #[test]
    fn str_arg_drops_the_trailing_nul_and_keeps_the_bytes() {
        // The CString holds "wl_compositor\0"; the translated form must be the 13 content bytes, no NUL.
        let cstr = CString::new("wl_compositor").unwrap();
        assert_eq!(
            translate_arg(&Argument::<ObjectId, OwnedFd>::Str(Some(Box::new(cstr)))),
            Ok(WaylandArg::Str(Some(b"wl_compositor".to_vec())))
        );
    }

    #[test]
    fn str_arg_preserves_the_null_vs_empty_distinction() {
        // A null string (wire's absent case) is None...
        assert_eq!(
            translate_arg(&Argument::<ObjectId, OwnedFd>::Str(None)),
            Ok(WaylandArg::Str(None))
        );
        // ...distinct from a present-but-empty string, which is Some(empty).
        let empty = CString::new("").unwrap();
        assert_eq!(
            translate_arg(&Argument::<ObjectId, OwnedFd>::Str(Some(Box::new(empty)))),
            Ok(WaylandArg::Str(Some(Vec::new())))
        );
    }

    #[test]
    fn fd_arg_is_not_translatable_without_the_token_resolution() {
        // A borrowed-then-owned fd for the test: dup stdin so the OwnedFd is real and safely closeable.
        use std::os::fd::{AsFd, OwnedFd};
        let fd: OwnedFd = std::io::stdin().as_fd().try_clone_to_owned().unwrap();
        assert_eq!(
            translate_arg(&Argument::<ObjectId, OwnedFd>::Fd(fd)),
            Err(TranslateError::UnresolvedFd)
        );
    }
}
