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
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;

use rayland_relay::{BufferToken, WaylandArg, WaylandMessage};
use wayland_server::Resource; // brings `interface()` into scope for the generated object types
use wayland_server::backend::protocol::{Argument, Message};
use wayland_server::backend::{
    Backend, ClientData, ClientId, GlobalHandler, GlobalId, Handle, ObjectData, ObjectId,
};

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
        Argument::NewId(id) => Ok(WaylandArg::NewId(id.protocol_id())),
        // Opaque array copied through unchanged.
        Argument::Array(bytes) => Ok(WaylandArg::Array((**bytes).clone())),
        // The one thing that cannot cross a network: resolved to a token in the fd→token sub-step.
        Argument::Fd(_) => Err(TranslateError::UnresolvedFd),
    }
}

/// Translate a whole `wayland-backend` request into the [`WaylandMessage`] forwarded to S.
///
/// Carries the sender object's id in the app's id space (`sender_id.protocol_id()`), the opcode verbatim,
/// and each argument via [`translate_arg`]. Fails with the first argument that cannot be translated (an
/// unresolved fd), so a request bearing an fd is never partially forwarded.
fn translate_message(msg: &Message<ObjectId, OwnedFd>) -> Result<WaylandMessage, TranslateError> {
    // Every argument in wire order; the first failure aborts the whole message (no partial forward).
    let args = msg
        .args
        .iter()
        .map(translate_arg)
        .collect::<Result<Vec<_>, _>>()?;
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
/// The token the proxy must emit needs facts split across two requests: the resource id and modifier come
/// from `params.add` (which carries the swapchain memfd and the DRM modifier), while width, height, and
/// format come from the later `params.create_immed`. This holds the `add`-time facts until `create_immed`
/// supplies the rest and the full [`BufferToken`] can be assembled.
#[derive(Default)]
struct PendingParams {
    /// The S-side resource id the passed memfd resolved to (`None` until a successful `add`, or if the fd
    /// did not correspond to a tracked resource).
    resource_id: Option<u32>,
    /// The DRM format modifier assembled from `add`'s `modifier_hi`/`modifier_lo` (`hi << 32 | lo`).
    modifier: u64,
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
/// 2. `params.add(fd, …, modifier_hi, modifier_lo)` → the swapchain memfd and its modifier. The fd is
///    `fstat`ed, its inode resolved to an S-side resource id, and both stashed in [`PendingParams`]. The
///    fd is **dropped** — it never crosses the network.
/// 3. `params.create_immed(buffer_id, width, height, format, flags)` → the app names the `wl_buffer`. Now
///    all the token's facts are in hand: it is assembled and forwarded as the message
///    `{ object_id: params, opcode: create_immed, args: [NewId(buffer_id), Buffer(token)] }`, which names
///    the buffer and the resource it denotes without any pixels or fd crossing.
///
/// A `create_immed` whose fd never resolved (no matching resource, or a missing `add`) forwards nothing and
/// logs — the app still gets its `wl_buffer` object locally (the backend created it from the `NewId`), but
/// S is not told to present a resource it cannot identify.
fn try_intercept_buffer(data: &mut ProxyState, msg: &Message<ObjectId, OwnedFd>) -> bool {
    let iface = msg.sender_id.interface().name;
    // The params/buffer objects live in the app's id space, keyed by the sending object's protocol id.
    let obj = msg.sender_id.protocol_id();
    match (iface, msg.opcode) {
        // Step 1: a new params object. Start tracking it; do not forward.
        (IFACE_DMABUF, OP_CREATE_PARAMS) => {
            if let Some(params_id) = first_new_id(msg) {
                data.pending.insert(params_id, PendingParams::default());
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
            // Record against this params object; `create_immed` will complete the token.
            let entry = data.pending.entry(obj).or_default();
            entry.resource_id = resource_id;
            entry.modifier = modifier;
            wp_log(&format!(
                "intercept params {obj}.add -> resource {resource_id:?}, modifier {modifier:#x} (fd dropped)"
            ));
            true
        }
        // Step 3: name the wl_buffer. Assemble and forward the token; the fd never crossed.
        (IFACE_PARAMS, OP_PARAMS_CREATE_IMMED) => {
            let buffer_id = first_new_id(msg);
            let (width, height, drm_format) = immed_geometry(msg);
            // Consume the accumulated state for this params object.
            let pending = data.pending.remove(&obj).unwrap_or_default();
            match (buffer_id, pending.resource_id) {
                (Some(buffer_id), Some(resource_id)) => {
                    let token = BufferToken {
                        resource_id,
                        width,
                        height,
                        drm_format,
                        modifier: pending.modifier,
                    };
                    // The proxy-internal encoding of create_immed: the buffer's app-side id and the token
                    // that names its S-side resource. S resolves the token to a real dma-buf (Task 4).
                    let wire = WaylandMessage {
                        object_id: obj,
                        opcode: msg.opcode,
                        args: vec![WaylandArg::NewId(buffer_id), WaylandArg::Buffer(token)],
                    };
                    wp_log(&format!(
                        "intercept params {obj}.create_immed -> buffer {buffer_id} = resource {resource_id} ({width}x{height} fmt {drm_format:#x})"
                    ));
                    data.sink.forward_request(wire);
                }
                _ => {
                    // No resolved resource (a missing/foreign fd): the app keeps its local buffer, but S
                    // is told nothing — a buffer it cannot identify must not be presented.
                    wp_log(&format!(
                        "intercept params {obj}.create_immed -> UNRESOLVED (buffer {buffer_id:?}, resource {:?}); not forwarded",
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
        _handle: &Handle,
        _data: &mut ProxyState,
        _client_id: ClientId,
        _global_id: GlobalId,
        object_id: ObjectId,
    ) -> Arc<dyn ObjectData<ProxyState>> {
        // Record the bind so bring-up can confirm vkcube reaches every WP0 global. Gated so a normal run
        // is silent; `RAYLAND_WP_LOG=1` turns it on.
        wp_log(&format!(
            "bound global {} -> object {}",
            self.iface_name,
            object_id.protocol_id()
        ));
        // The bound object forwards its requests through the same inert data as any other object for now.
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
        _handle: &Handle,
        data: &mut ProxyState,
        _client_id: ClientId,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<ProxyState>>> {
        // Does this request create a new object? The backend needs data for it if so — decided *before*
        // `msg` is consumed by translation below.
        let makes_new_object = msg
            .args
            .iter()
            .any(|arg| matches!(arg, Argument::NewId(_)));
        // The one special case first: buffer-by-token interception on the dmabuf path. If it consumed the
        // request (a `create_params`/`add`/`create_immed`), the generic path must not also run — a raw fd
        // must never be forwarded.
        if !try_intercept_buffer(data, &msg) {
            // Generic path: translate the request to its wire form and forward it to S. A stray unresolved
            // fd outside the intercepted dmabuf path is not expected in WP0; log and drop rather than
            // forward a raw fd or panic.
            match translate_message(&msg) {
                Ok(wire_msg) => {
                    wp_log(&format!(
                        "forward obj {} opcode {} ({} args)",
                        wire_msg.object_id,
                        wire_msg.opcode,
                        wire_msg.args.len()
                    ));
                    data.sink.forward_request(wire_msg);
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
        // Drop any accumulated buffer state keyed by this object's id (no-op unless it was a params object).
        data.pending.remove(&object_id.protocol_id());
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
pub fn run(
    socket_path: PathBuf,
    sink: Arc<dyn WaylandSink>,
    resolver: Arc<dyn ResourceResolver>,
) -> anyhow::Result<()> {
    // A stale socket file left by a crashed run would make `bind` fail; remove it (ignore "not found").
    let _ = std::fs::remove_file(&socket_path);
    // The pure-Rust Wayland server backend. `ProxyState` is the dispatch state it threads to callbacks.
    let mut backend = Backend::<ProxyState>::new()?;
    // Advertise every global vkcube binds (spec §6). `wl_display`/`wl_registry` are backend built-ins; we
    // add only the application-visible globals. Each is advertised at the full version the interface
    // descriptor knows — WP0 negotiates no version subset yet.
    let handle = backend.handle();
    create_global::<WlCompositor>(&handle);
    create_global::<XdgWmBase>(&handle);
    create_global::<ZwpLinuxDmabufV1>(&handle);
    create_global::<WlSeat>(&handle);
    // Where the application dials in. WP0 serves exactly one such socket.
    let listener = UnixListener::bind(&socket_path)?;
    // The dispatch loop owns this state; it is threaded to every callback by the backend.
    let mut state = ProxyState {
        sink,
        resolver,
        pending: HashMap::new(),
    };
    wp_log(&format!("proxy listening at {}", socket_path.display()));
    serve(&mut backend, &listener, &mut state)
}

/// Advertise one global for interface `R`, wiring it to a fresh [`ProxyGlobal`] handler.
///
/// `R` is a generated object type (`WlCompositor`, `XdgWmBase`, …); `R::interface()` yields the
/// `&'static Interface` the backend needs, and its `.version` is the maximum version the descriptor
/// supports, which is what we advertise.
fn create_global<R: Resource>(handle: &Handle) -> GlobalId {
    let iface = R::interface();
    handle.create_global::<ProxyState>(
        iface,
        iface.version,
        Arc::new(ProxyGlobal {
            iface_name: iface.name,
        }),
    )
}

/// The accept-and-dispatch loop.
///
/// Polls two sources: the listening socket (a new application connecting) and the backend's own poll fd
/// (a connected client has pending requests). On a connection, insert the client into the backend; on
/// backend readiness, dispatch all pending client requests and flush any queued events back. Runs until an
/// unrecoverable error.
///
/// `EINTR` from `poll` is retried rather than surfaced — a signal is not a failure of the loop.
fn serve(
    backend: &mut Backend<ProxyState>,
    listener: &UnixListener,
    state: &mut ProxyState,
) -> anyhow::Result<()> {
    use std::os::fd::{AsFd, AsRawFd};
    loop {
        // Two fds to watch: index 0 the listener, index 1 the backend. `poll` fills `revents` in place.
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
        ];
        // SAFETY: `fds` is a valid, initialized array of 2 pollfds for the duration of the call; a negative
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
    }
}

/// Accept one pending connection on `listener` and hand its stream to the backend as a new client.
///
/// The stream is registered with a fresh [`ProxyClientData`]; from then on the backend delivers that
/// client's requests to the objects' [`ObjectData::request`]. After registering, flush so the client
/// immediately sees the initial protocol state (e.g. the display's globals).
fn accept_client(
    backend: &mut Backend<ProxyState>,
    listener: &UnixListener,
) -> anyhow::Result<()> {
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
        eprintln!("[wp-proxy] {msg}");
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
            translate_arg(&Argument::<ObjectId, OwnedFd>::Array(Box::new(bytes.clone()))),
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
