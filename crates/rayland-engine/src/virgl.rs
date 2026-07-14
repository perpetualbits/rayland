//! `VirglEngine`: the concrete `RenderEngine` backed by an FFI-embedded `libvirglrenderer`.
//!
//! This module owns all interaction with the C library beyond the raw declarations in `ffi.rs`.
//! It brings up a Venus-capable renderer on a DRM render node, creates Venus contexts, submits
//! command buffers, creates/reads back GPU resources (Task 3), and tears everything down in the
//! correct order on `Drop`.
//!
//! # The global-singleton rule
//! virglrenderer keeps its entire state in process-global variables (none of its functions take a
//! handle). Therefore at most one initialized `VirglEngine` may exist per process at any time.
//! We enforce this with a process-global flag: `VirglEngine::new` fails with
//! `EngineError::AlreadyActive` if another engine is live, and `Drop` releases the flag only after
//! `virgl_renderer_cleanup` returns. This is what makes *repeated* new→use→drop cycles safe, and
//! is the core of the C0 reliability result.
//!
//! # Task 3: resource creation + fence-waited readback
//! `create_resource`/`create_blob_resource` bring up a GPU resource; `read_back` fence-waits for
//! outstanding GPU work on its context, then copies its pixels to CPU memory as an [`EngineFrame`].
//! Two non-obvious, empirically-discovered constraints shape this design — read `TrackedResource`'s
//! and `read_back`'s doc comments for the full story before touching either method:
//! 1. `virgl_renderer_resource_get_info` must be called **at most once per resource, immediately
//!    after creation** — calling it again after content exists silently resets that content.
//! 2. `virgl_renderer_transfer_read_iov`/`transfer_write_iov` must be called with `ctx_id = 0`, not
//!    the resource's real (Venus) owning context — the render-server proxy does not support them.

// The raw FFI surface (constants, structs, C functions, callbacks).
use crate::ffi;
// The trait this engine implements.
use crate::RenderEngine;
// Raw C string / char for the context debug label.
use std::ffi::{CString, c_char, c_int, c_void};
// Resource-id -> context-id tracking (Task 3): which context a resource was attached to, so
// `read_back` knows which context's fences to wait on.
use std::collections::HashMap;
// Global single-instance guard.
use std::sync::atomic::{AtomicBool, Ordering};
// Bounding the fence-wait poll loop so a wedged render server cannot hang readback forever.
use std::time::{Duration, Instant};

// Path handling for the render node.
use std::path::Path;

/// Process-global "an engine is initialized" flag. virglrenderer is a global singleton, so this
/// serializes the whole init→use→cleanup lifecycle to one engine at a time. `false` = no engine.
static ENGINE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Errors from the render engine. Every fallible C call maps into exactly one variant here — a C
/// error can never silently become success. Variants that wrap a C return code carry both the raw
/// code and a human-readable errno name (see [`errno_name`]).
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// Another `VirglEngine` is already initialized in this process. virglrenderer is a global
    /// singleton; only one engine may be live at a time.
    #[error("a virglrenderer engine is already active in this process (it is a global singleton)")]
    AlreadyActive,

    /// The DRM render node could not be opened (absent, or permission denied). This is the
    /// expected "no usable GPU" condition on a CI runner and the reason tests skip rather than fail.
    #[error("render node {path} could not be opened: {source}")]
    RenderNodeUnavailable {
        /// The render-node path we tried to open.
        path: String,
        /// The underlying OS error.
        source: std::io::Error,
    },

    /// `virgl_renderer_init` returned a non-zero code. The renderer's EGL/Venus winsys failed to
    /// come up (e.g. the render node does not support the required EGL/Vulkan features).
    #[error("virgl_renderer_init failed for {path} (rc={rc}: {reason})")]
    InitFailed {
        /// The render-node path passed to the engine.
        path: String,
        /// The raw return code from `virgl_renderer_init`.
        rc: c_int,
        /// Human-readable errno name for `rc`.
        reason: String,
    },

    /// `virgl_renderer_context_create_with_flags` returned a non-zero code for a Venus context.
    /// The most common cause (`EINVAL`) is Venus being unavailable or the render server not
    /// running — but with `RAYLAND_INIT_FLAGS` the render server is enabled, so a failure here is
    /// a genuine capability problem, not the missing-flag artifact the spike diagnosed.
    #[error(
        "virgl_renderer_context_create_with_flags(ctx_id={ctx_id}, capset=venus) failed (rc={rc}: {reason})"
    )]
    ContextCreateFailed {
        /// The context id we tried to create.
        ctx_id: u32,
        /// The raw return code.
        rc: c_int,
        /// Human-readable errno name for `rc`.
        reason: String,
    },

    /// A command buffer's length is not a whole number of 4-byte words. virglrenderer measures
    /// command buffers in `ndw` (number of dwords), so any byte length must be a multiple of 4.
    #[error("command buffer length {len} bytes is not a multiple of 4")]
    UnalignedCommand {
        /// The offending byte length.
        len: usize,
    },

    /// A command buffer is too long to express as `ndw` in the C API's `int` word count.
    #[error("command buffer of {len} bytes is too large to submit")]
    CommandTooLarge {
        /// The offending byte length.
        len: usize,
    },

    /// `virgl_renderer_submit_cmd` returned a non-zero code. The command stream was rejected
    /// (e.g. malformed, or the context is gone).
    #[error("virgl_renderer_submit_cmd(ctx_id={ctx_id}) failed (rc={rc}: {reason})")]
    SubmitFailed {
        /// The context id the command targeted.
        ctx_id: u32,
        /// The raw return code.
        rc: c_int,
        /// Human-readable errno name for `rc`.
        reason: String,
    },

    /// A context id does not fit in the C API's `int`. `virgl_renderer_submit_cmd` takes `ctx_id`
    /// as an `int`; ids come from the (untrusted) vtest client, so an id above `INT_MAX` would
    /// wrap to a negative context under a bare `as c_int` cast. We reject it up front instead.
    #[error("context id {ctx_id} does not fit in a C int and cannot be submitted to")]
    SubmitCtxIdOutOfRange {
        /// The offending context id.
        ctx_id: u32,
    },

    /// The renderer reports no Venus capset, so the vtest handshake cannot answer `VCMD_GET_CAPSET`
    /// with real capability data (typically: no GPU, or this build/host lacks Venus support).
    #[error("no Venus capset available to answer the vtest handshake (Venus unsupported here)")]
    NoVenusCapset,

    /// The stream carrying the vtest protocol failed (peer closed mid-message, socket error, ...).
    #[error("vtest stream I/O failed")]
    VtestIo(#[from] std::io::Error),

    /// A vtest length prefix exceeded [`crate::vtest::MAX_VTEST_PAYLOAD_BYTES`]; the message is
    /// refused before any buffer is allocated (an untrusted peer must not force a huge allocation).
    #[error("vtest message payload of {len} bytes exceeds the maximum allowed")]
    VtestFrameTooLarge {
        /// The offending declared payload length in bytes.
        len: u64,
    },

    /// A vtest message was malformed or an opcode arrived that this server does not implement.
    /// Carries a human-readable description; the server never silently drops an unhandled message.
    #[error("vtest protocol error: {detail}")]
    VtestProtocol {
        /// What was wrong (bad length, unknown/unsupported opcode, out-of-range offset, ...).
        detail: String,
    },

    // ---------------------------------------------------------------------------------------
    // Task 3: resource creation + fence-waited readback.
    // ---------------------------------------------------------------------------------------
    /// A context id supplied to a resource-creation call does not fit in the C API's `int`
    /// (`ctx_attach_resource` takes `ctx_id` as `c_int`). Ids ultimately come from the untrusted
    /// vtest client, so — exactly like `SubmitCtxIdOutOfRange` — we reject rather than wrap.
    #[error("context id {ctx_id} does not fit in a C int and cannot own a resource")]
    ResourceCtxIdOutOfRange {
        /// The offending context id.
        ctx_id: u32,
    },

    /// `create_resource`/`create_blob_resource` were asked to attach a resource to a `ctx_id`
    /// this engine never created (or already destroyed). Only a context this engine created and
    /// tracks in `self.contexts` is a safe `ctx_attach_resource` target — an untracked id might
    /// name nothing, or might collide with something virglrenderer interprets differently. This
    /// makes "`ctx_id` names a live context" (the invariant `wait_for_context_fence`'s doc comment
    /// already assumes) an enforced check rather than caller discipline.
    #[error("context id {ctx_id} is not tracked by this engine (no such Venus context)")]
    UnknownContext {
        /// The offending context id.
        ctx_id: u32,
    },

    /// An engine-assigned resource id does not fit in the C API's `int` (several resource calls
    /// take `res_handle` as `c_int`). `next_resource_id` is a `u32` counter starting at 1; this is
    /// unreachable in any real session (it would require ~2^31 resources in one process lifetime)
    /// but is guarded rather than assumed, per this crate's no-silent-wrap discipline.
    #[error("resource id {resource_id} does not fit in a C int")]
    ResourceIdOverflow {
        /// The offending resource id.
        resource_id: u32,
    },

    /// `virgl_renderer_resource_create` returned a non-zero code for a classic 2D resource.
    #[error(
        "virgl_renderer_resource_create(ctx_id={ctx_id}, resource_id={resource_id}) failed (rc={rc}: {reason})"
    )]
    ResourceCreateFailed {
        /// The context the resource was being created for.
        ctx_id: u32,
        /// The engine-assigned id that was about to be attached to it.
        resource_id: u32,
        /// The raw return code.
        rc: c_int,
        /// Human-readable errno name for `rc`.
        reason: String,
    },

    /// `virgl_renderer_resource_create_blob` returned a non-zero code for a blob resource (the
    /// resource type Venus's real wire protocol, `VCMD_RESOURCE_CREATE_BLOB`, allocates).
    #[error(
        "virgl_renderer_resource_create_blob(ctx_id={ctx_id}, resource_id={resource_id}) failed (rc={rc}: {reason})"
    )]
    BlobResourceCreateFailed {
        /// The context the blob resource was being created for.
        ctx_id: u32,
        /// The engine-assigned id that was about to be attached to it.
        resource_id: u32,
        /// The raw return code.
        rc: c_int,
        /// Human-readable errno name for `rc`.
        reason: String,
    },

    /// `virgl_renderer_context_create_fence` returned a non-zero code — the fence-wait
    /// `read_back` performs before every readback could not even be created.
    #[error(
        "virgl_renderer_context_create_fence(ctx_id={ctx_id}, ring_idx={ring_idx}) failed (rc={rc}: {reason})"
    )]
    FenceCreateFailed {
        /// The context the fence targeted.
        ctx_id: u32,
        /// The ring index the fence targeted.
        ring_idx: u32,
        /// The raw return code.
        rc: c_int,
        /// Human-readable errno name for `rc`.
        reason: String,
    },

    /// A fence created by `read_back`'s wait loop never retired within `FENCE_WAIT_TIMEOUT`.
    /// Either the render server wedged, or
    /// (more likely in Task 3/4's early integration) the context never actually had the submitted
    /// work retire because nothing was ever submitted to it. Distinguishing those is future work;
    /// today this is a clear, typed "readback did not complete in time" rather than a silent hang.
    #[error(
        "fence wait timed out: ctx_id={ctx_id} ring_idx={ring_idx} fence_id={fence_id} never retired"
    )]
    FenceTimeout {
        /// The context the fence targeted.
        ctx_id: u32,
        /// The ring index the fence targeted.
        ring_idx: u32,
        /// The fence id that never retired.
        fence_id: u64,
    },

    /// `read_back` (or `unref_resource`) was asked about a resource id this engine never created
    /// (or already unref'd). The engine only knows how to fence-wait/read back resources it
    /// created itself, since only it knows which context they were attached to.
    #[error("resource id {resource_id} is not tracked by this engine")]
    UnknownResource {
        /// The offending resource id.
        resource_id: u32,
    },

    /// `virgl_renderer_resource_get_info` returned a non-zero code when `create_resource` queried
    /// it immediately after creation (the *only* time this engine ever calls it — see
    /// `TrackedResource`'s doc comment for why calling it again later, e.g. from `read_back`, is
    /// actively dangerous, not just redundant).
    #[error(
        "virgl_renderer_resource_get_info(resource_id={resource_id}) failed (rc={rc}: {reason})"
    )]
    ResourceInfoFailed {
        /// The resource that was queried.
        resource_id: u32,
        /// The raw return code.
        rc: c_int,
        /// Human-readable errno name for `rc`.
        reason: String,
    },

    /// `create_resource`'s immediate post-creation `resource_get_info` succeeded but reported a
    /// zero width, height, or stride — not a valid 2D image to ever read back.
    #[error(
        "resource {resource_id} has no valid image info (width={width} height={height} stride={stride})"
    )]
    InvalidResourceInfo {
        /// The resource that was queried.
        resource_id: u32,
        /// The reported width (0 here is the problem).
        width: u32,
        /// The reported height (0 here is the problem).
        height: u32,
        /// The reported row stride (0 here is the problem).
        stride: u32,
    },

    /// `create_resource`'s immediate post-creation `resource_get_info` succeeded, reported
    /// nonzero width/height/stride, but the stride is narrower than a single tightly-packed row
    /// (`width * bytes_per_pixel`) for a format `read_back`/`repack_tight` know how to unpad.
    /// Refused here — while the resource can still be cleanly detached+unref'd — rather than
    /// letting `read_back` discover it later via an out-of-bounds slice panic in `repack_tight`.
    #[error(
        "resource {resource_id} has an implausible stride (width={width} stride={stride} bytes_per_pixel={bytes_per_pixel}: stride is narrower than one packed row)"
    )]
    InvalidResourceStride {
        /// The resource that was queried.
        resource_id: u32,
        /// The reported width.
        width: u32,
        /// The reported row stride (too small for `width` here is the problem).
        stride: u32,
        /// The format's bytes-per-pixel, from `bytes_per_pixel`.
        bytes_per_pixel: u32,
    },

    /// `read_back` was asked to read a resource with no cached image layout — i.e. one created via
    /// `create_blob_resource`, not `create_resource`. Blob resources have no format/dimension
    /// concept (`virgl_renderer_resource_get_info` fails on one, confirmed empirically), so there
    /// is nothing cached to read back with; see `RenderEngine::read_back`'s doc comment for what a
    /// real blob readback would need (Task 4's concern).
    #[error(
        "resource {resource_id} has no cached image layout (it is a blob resource, not a classic one — read_back only supports resources created via create_resource)"
    )]
    ResourceNotReadable {
        /// The resource that was asked for.
        resource_id: u32,
    },

    /// The resource's `virgl_format` is not one of the small set of 32-bit-per-pixel formats
    /// `read_back` knows how to repack into a tightly-packed `EngineFrame` (see
    /// `bytes_per_pixel`). Refused rather than guessed — inventing a byte width for an unknown
    /// format would silently corrupt every pixel.
    #[error("resource {resource_id} has unsupported virgl_format {format} for readback")]
    UnsupportedReadbackFormat {
        /// The resource that was queried.
        resource_id: u32,
        /// The unrecognized `VIRGL_FORMAT_*` code.
        format: u32,
    },

    /// `virgl_renderer_transfer_read_iov` returned a non-zero code.
    #[error(
        "virgl_renderer_transfer_read_iov(resource_id={resource_id}) failed (rc={rc}: {reason})"
    )]
    TransferReadFailed {
        /// The resource that was being read.
        resource_id: u32,
        /// The raw return code.
        rc: c_int,
        /// Human-readable errno name for `rc`.
        reason: String,
    },
}

/// A CPU-side copy of a rendered resource's pixels, produced by [`RenderEngine::read_back`].
///
/// `pixels` is always **tightly packed**: exactly `width * height * bytes_per_pixel(format)`
/// bytes, one row immediately after another with no padding, regardless of the GPU resource's
/// real row stride (`read_back` strips that padding — see its doc comment). `format` is the raw
/// `VIRGL_FORMAT_*` code virglrenderer reported for the resource (pinned from
/// `virgl_renderer_resource_get_info`, never guessed), so callers can interpret the byte layout
/// correctly (e.g. `VIRGL_FORMAT_B8G8R8A8_UNORM = 1` is `[B, G, R, A]` per pixel, little-endian
/// byte order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFrame {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Tightly-packed pixel bytes, `width * height * bytes_per_pixel(format)` long.
    pub pixels: Vec<u8>,
    /// The raw `VIRGL_FORMAT_*` code (from `virgl_renderer_resource_get_info`) describing how to
    /// interpret `pixels`.
    pub format: u32,
}

/// Maps a C return code (an errno, possibly returned as a positive value) to a short readable
/// name, so `EngineError` messages say `EINVAL` rather than a bare `22`. Falls back to the numeric
/// code for anything `std` does not recognize.
///
/// # Inputs / outputs
/// - `rc`: the raw return code from a `virgl_renderer_*` call (treated by absolute value, since
///   these functions variously return positive or negative errnos).
/// - Returns an owned `String` like `"EINVAL"` or `"os error 22"`.
fn errno_name(rc: c_int) -> String {
    // Normalize sign: virglrenderer returns errnos both as +22 (context create) and, elsewhere,
    // as negatives; `std::io::Error` wants the positive errno.
    let errno = rc.unsigned_abs() as i32;
    // `from_raw_os_error(0)` is not a real error, so guard it to avoid a misleading "success".
    if errno == 0 {
        return "unknown error (rc=0)".to_string();
    }
    // Let std translate the errno to its platform description (e.g. "Invalid argument (os error 22)").
    std::io::Error::from_raw_os_error(errno).to_string()
}

/// How long [`VirglEngine`]'s fence-wait poll loop (used by `read_back`) waits for a fence to
/// retire before giving up. Generous for varied/loaded hardware while still bounding a hang if
/// the render server wedges. Chosen conservatively, not measured against real Venus render
/// workloads — no live Venus client exists yet (that is Task 4); revisit if Task 4 finds it too
/// tight for real GPU work.
const FENCE_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the fence-wait poll loop sleeps between calls to `virgl_renderer_context_poll`.
/// `THREAD_SYNC` is deliberately not enabled (Task 1's choice, for simpler teardown), so nothing
/// pumps fence completion in the background — this loop must call `context_poll` itself,
/// repeatedly. Short enough to add negligible latency to a real readback, long enough not to spin
/// the CPU while waiting.
const FENCE_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// A classic resource's image layout — exactly what `read_back` needs to interpret its bytes —
/// cached at resource-creation time. See [`TrackedResource`]'s doc comment for **why** it must be
/// cached rather than looked up on demand: this is the single most important, least obvious
/// finding of Task 3.
#[derive(Debug, Clone, Copy)]
struct CachedImageInfo {
    /// Width in pixels, from `virgl_renderer_resource_get_info` at creation time.
    width: u32,
    /// Height in pixels, from `virgl_renderer_resource_get_info` at creation time.
    height: u32,
    /// Row stride in bytes, from `virgl_renderer_resource_get_info` at creation time. May exceed
    /// `width * bytes_per_pixel(format)` — the stride-honoring discipline `read_back` depends on
    /// this cached value for.
    stride: u32,
    /// The `VIRGL_FORMAT_*` code, from `virgl_renderer_resource_get_info` at creation time.
    format: u32,
}

/// A resource this engine created and has not yet released, and everything `read_back`/`Drop`
/// need to know about it without calling back into virglrenderer.
///
/// # Why `image` is cached at creation time, not queried by `read_back` (the key Task 3 finding)
/// The obvious design calls `virgl_renderer_resource_get_info` *inside* `read_back`, right before
/// the pixel transfer, so the answer is as fresh as possible. **That design is wrong, and silently
/// so**: a scratch experiment on real hardware (`libvirglrenderer` 1.2.0) found that calling
/// `virgl_renderer_resource_get_info` on a classic resource *after* content has been written into
/// it (via `transfer_write_iov`, standing in for a real render) — even when the call's own return
/// code is 0 and it reports perfectly plausible width/height/stride — **resets that resource's
/// content back to all-zero** for every subsequent `transfer_read_iov`. This was confirmed
/// independently in both a standalone C reproduction and this crate's own Rust code: removing the
/// second `get_info` call (calling it exactly once, immediately after `resource_create` +
/// `ctx_attach_resource`, before anything is ever written to the resource) makes the round trip
/// correct every time; adding a second call anywhere after a write makes it silently wrong every
/// time. virglrenderer's public documentation does not mention this, and the mechanism (almost
/// certainly some lazy-realization or state-revalidation path inside vrend's classic GL backend)
/// is not visible from the public API — this is exactly the kind of non-obvious pitfall this
/// codebase's documentation discipline exists to record for the next reader, so **`read_back` must
/// never call `virgl_renderer_resource_get_info` itself** — it uses the `image` cached here,
/// queried exactly once, at the earliest safe moment (creation, before any content exists to
/// lose).
struct TrackedResource {
    /// The context this resource was attached to (`read_back`'s fence-wait target).
    ctx_id: u32,
    /// Cached image layout for a classic resource (`Some`), queried exactly once at creation. A
    /// blob resource (`create_blob_resource`) has no format/dimension concept — confirmed
    /// empirically, `resource_get_info` fails on one — so its entry is `None`; `read_back` refuses
    /// such a resource with `EngineError::ResourceNotReadable` rather than trying.
    image: Option<CachedImageInfo>,
}

/// The Venus render engine: an initialized `libvirglrenderer` bound to one DRM render node.
///
/// Construct with [`VirglEngine::new`]; it holds the single-instance lock for its lifetime and
/// releases it on `Drop` after cleaning up. All contexts created through it are destroyed on
/// `Drop`, so callers cannot leak GPU contexts by forgetting to tear down.
pub struct VirglEngine {
    /// The cookie handed to virglrenderer, heap-boxed so its address is stable for the whole
    /// lifetime (virglrenderer passes this pointer back to `get_drm_fd` and `write_context_fence`).
    /// Kept alive until after `virgl_renderer_cleanup` in `Drop`.
    cookie: Box<ffi::Cookie>,
    /// Ids of the Venus contexts we created and have not destroyed, so `Drop` can destroy them
    /// before `virgl_renderer_cleanup`.
    contexts: Vec<u32>,
    /// Resources we created (via `create_resource`/`create_blob_resource`) and have not unref'd,
    /// keyed by resource id. `read_back` uses each entry's `ctx_id` to know which context's fences
    /// to wait on and its cached `image` (see `TrackedResource`'s doc comment for why it must be
    /// *cached*, not re-queried) to know how to read it; `Drop` uses the map to release every
    /// resource before the contexts that hold them (Task 3's lifetime-order caution: resources
    /// before contexts).
    resources: HashMap<u32, TrackedResource>,
    /// Monotonic source of resource ids for `create_resource`/`create_blob_resource`. virglrenderer
    /// requires the *caller* to choose a resource handle (unlike context ids, which come from the
    /// vtest client, resource ids in this design are entirely engine-assigned — see their doc
    /// comments). Starts at 1 so 0 stays a sentinel, matching the vtest layer's existing convention.
    next_resource_id: u32,
    /// Monotonic source of fence ids for the `read_back` wait loop's
    /// `virgl_renderer_context_create_fence` calls. A fresh id per wait keeps concurrent/successive
    /// waits on the same context unambiguous (each is asking "has *this specific* fence retired",
    /// not "has *some* fence retired").
    next_fence_id: u64,
}

impl VirglEngine {
    /// Initializes virglrenderer against `render_node` and brings up its Venus-capable EGL winsys.
    ///
    /// Steps:
    /// 1. Acquire the process-global single-instance lock (fail with `AlreadyActive` if taken).
    /// 2. Verify the render node can be opened (fail with `RenderNodeUnavailable` otherwise) — a
    ///    fast, clear failure before we touch the C library, and the condition tests skip on.
    /// 3. Box a `Cookie` carrying the render-node path and call `virgl_renderer_init` with
    ///    `RAYLAND_INIT_FLAGS` (`USE_EGL | USE_SURFACELESS | VENUS | RENDER_SERVER`) and the
    ///    `'static` callbacks. virglrenderer calls our `get_drm_fd` during this call to open its
    ///    winsys fd.
    ///
    /// # Inputs / outputs
    /// - `render_node`: path to a DRM render node (e.g. `/dev/dri/renderD128`).
    /// - Returns the initialized engine, or an `EngineError` (releasing the lock on any failure).
    ///
    /// # Failure modes
    /// - `AlreadyActive`: another engine is live in this process.
    /// - `RenderNodeUnavailable`: the node is absent or not permitted (the CI-skip condition).
    /// - `InitFailed`: virglrenderer's winsys/Venus init failed on an otherwise-openable node.
    pub fn new(render_node: &Path) -> Result<Self, EngineError> {
        // Render-node path as a string, reused in error messages.
        let render_node_str = render_node.display().to_string();

        // (1) Take the single-instance lock. `compare_exchange` succeeds only if it was `false`.
        // On failure another engine is live and we must not touch the global renderer.
        if ENGINE_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(EngineError::AlreadyActive);
        }
        // From here on, any early return MUST release the lock (via the `guard` closure below).

        // (2) Prove the node is openable before initializing the C library. This gives a clean,
        // specific error (and the tests' skip condition) instead of an opaque init failure, and
        // matches exactly what `get_drm_fd` will do repeatedly during init.
        if let Err(source) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(render_node)
        {
            // Release the lock we just took before returning.
            ENGINE_ACTIVE.store(false, Ordering::Release);
            return Err(EngineError::RenderNodeUnavailable {
                path: render_node_str,
                source,
            });
        }

        // (3) Box the cookie so its heap address is stable; virglrenderer stores the pointer and
        // hands it back to `get_drm_fd` for the engine's whole life.
        let cookie = Box::new(ffi::Cookie {
            render_node: render_node.to_path_buf(),
            // Empty mailbox: no fences created yet. `write_context_fence` and `read_back`'s wait
            // loop share this through the cookie pointer for the engine's whole lifetime.
            fence_state: ffi::FenceState::default(),
        });
        // Raw pointer into the boxed cookie, valid as long as `cookie` (a field of `self`) lives.
        let cookie_ptr = (&*cookie as *const ffi::Cookie) as *mut c_void;

        // The callbacks struct is a `'static`; cast away const because the C prototype takes a
        // non-const pointer, though virglrenderer only reads it.
        let cb_ptr = (&ffi::RAYLAND_CALLBACKS as *const ffi::VirglRendererCallbacks)
            as *mut ffi::VirglRendererCallbacks;

        // Call into C. SAFETY: `cookie_ptr` points at a live boxed `Cookie` that outlives this
        // renderer (we move the box into `self` on success, and cleanup runs before it drops);
        // `cb_ptr` points at a `'static`; the flags are the spike-proven Venus set. virglrenderer
        // may invoke `get_drm_fd` before returning.
        let rc = unsafe { ffi::virgl_renderer_init(cookie_ptr, ffi::RAYLAND_INIT_FLAGS, cb_ptr) };
        if rc != 0 {
            // Init failed: release the lock and report. `cookie` drops here, which is safe because
            // a failed init means virglrenderer is not holding onto its pointer.
            ENGINE_ACTIVE.store(false, Ordering::Release);
            return Err(EngineError::InitFailed {
                path: render_node_str,
                rc,
                reason: errno_name(rc),
            });
        }

        // Success: hand the cookie to the engine, which now owns the lifecycle.
        Ok(VirglEngine {
            cookie,
            contexts: Vec::new(),
            resources: HashMap::new(),
            // Start at 1: Mesa/Venus asserts resource/res ids are > 0, matching the vtest layer's
            // existing `next_res_id` convention (see `vtest::Session`).
            next_resource_id: 1,
            next_fence_id: 1,
        })
    }
}

impl RenderEngine for VirglEngine {
    /// Creates a Venus capset context with the given id.
    ///
    /// Calls `virgl_renderer_context_create_with_flags(ctx_id, VENUS, ..)`. The context is
    /// recorded so `Drop` will destroy it. Reusing an id that is already live is a caller error
    /// that virglrenderer will reject; ids are chosen by the caller (Task 2 maps vtest client
    /// connections to ids).
    ///
    /// # Failure modes
    /// - `ContextCreateFailed`: virglrenderer rejected the context (with `RAYLAND_INIT_FLAGS` this
    ///   indicates a real Venus capability problem, since the render server is enabled).
    fn create_venus_context(&mut self, ctx_id: u32) -> Result<(), EngineError> {
        // A short debug label virglrenderer attaches to the context (aids its own logging).
        // `CString::new` cannot fail here: the literal has no interior NUL.
        let name = CString::new("rayland-venus").unwrap_or_default();
        // Length virglrenderer expects (bytes, not counting the NUL it does not require).
        let nlen = name.as_bytes().len() as u32;

        // Create the context. SAFETY: `name` is a valid C string living for the call; `nlen`
        // matches its length; the capset id selects Venus (low 8 bits of ctx_flags). The renderer
        // is initialized (we hold an initialized engine).
        let rc = unsafe {
            ffi::virgl_renderer_context_create_with_flags(
                ctx_id,
                ffi::VIRGL_RENDERER_CAPSET_VENUS,
                nlen,
                name.as_ptr() as *const c_char,
            )
        };
        if rc != 0 {
            // Map the non-zero code to a typed error — never treat it as success.
            return Err(EngineError::ContextCreateFailed {
                ctx_id,
                rc,
                reason: errno_name(rc),
            });
        }

        // Record the id so `Drop` destroys the context even if the caller never does.
        self.contexts.push(ctx_id);
        Ok(())
    }

    /// Submits a raw command buffer to a context.
    ///
    /// virglrenderer measures command buffers in 4-byte words (`ndw`). We validate the byte length
    /// is a multiple of 4 and copy it into a `u32`-aligned buffer (a `Vec<u32>`), which guarantees
    /// the ≥4-byte alignment `virgl_renderer_submit_cmd` requires (a bare `&[u8]` is not
    /// guaranteed aligned, and a misaligned buffer returns `EFAULT`).
    ///
    /// # Failure modes
    /// - `UnalignedCommand`: `cmd.len()` is not a multiple of 4.
    /// - `CommandTooLarge`: the word count would overflow the C API's `int`.
    /// - `SubmitFailed`: virglrenderer rejected the command stream.
    fn submit(&mut self, ctx_id: u32, cmd: &[u8]) -> Result<(), EngineError> {
        // Command buffers are dword-counted; a non-multiple-of-4 length is meaningless.
        // (`% 4` rather than `usize::is_multiple_of`, which is only stable since Rust 1.87 > MSRV.)
        if cmd.len() % 4 != 0 {
            return Err(EngineError::UnalignedCommand { len: cmd.len() });
        }
        // Number of 4-byte words; must fit in the C API's `int`.
        let ndw = cmd.len() / 4;
        let ndw =
            c_int::try_from(ndw).map_err(|_| EngineError::CommandTooLarge { len: cmd.len() })?;

        // Guard the context id the same way. `virgl_renderer_submit_cmd` takes `ctx_id` as a C
        // `int`, and the id originates from the untrusted vtest client; a bare `as c_int` cast on
        // a value above `INT_MAX` would silently wrap to a negative (wrong) context. Convert
        // fallibly and reject out-of-range ids instead of wrapping (Task 1 review follow-up).
        let ctx_id_c =
            c_int::try_from(ctx_id).map_err(|_| EngineError::SubmitCtxIdOutOfRange { ctx_id })?;

        // Copy into a `u32` buffer to guarantee 4-byte alignment regardless of the caller's slice.
        // `Vec<u32>`'s allocation is 4-byte aligned by construction.
        let mut words: Vec<u32> = vec![0; cmd.len() / 4];
        // Reinterpret the word buffer as bytes and fill it from the command slice.
        // SAFETY: `words` holds exactly `cmd.len()` bytes; the pointer is writable and non-null.
        let byte_view =
            unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, cmd.len()) };
        byte_view.copy_from_slice(cmd);

        // Submit. SAFETY: `words` is a live, 4-byte-aligned buffer of `ndw` dwords; virglrenderer
        // never mutates it; `ctx_id_c` was range-checked above to fit a C `int`.
        let rc = unsafe {
            ffi::virgl_renderer_submit_cmd(words.as_mut_ptr() as *mut c_void, ctx_id_c, ndw)
        };
        if rc != 0 {
            return Err(EngineError::SubmitFailed {
                ctx_id,
                rc,
                reason: errno_name(rc),
            });
        }
        Ok(())
    }

    /// Returns the real Venus capset blob for the vtest `VCMD_GET_CAPSET` handshake step.
    ///
    /// Two C calls: `virgl_renderer_get_cap_set(VENUS, &max_ver, &max_size)` learns the blob size,
    /// then `virgl_renderer_fill_caps(VENUS, version, buf)` writes exactly `max_size` bytes into a
    /// buffer we allocate. A `max_size` of 0 means Venus is unsupported here, which we surface as
    /// `NoVenusCapset` (the client would then fail its own init — the honest outcome on a host
    /// without Venus).
    ///
    /// # Failure modes
    /// - `NoVenusCapset`: the renderer reports no Venus capset (no GPU / Venus unavailable).
    fn venus_capset(&mut self, version: u32) -> Result<Vec<u8>, EngineError> {
        // Learn the capset blob size (and max version) from the renderer. `get_cap_set` writes both
        // out-params; a zero `max_size` means the capset is unsupported on this host.
        let mut max_ver: u32 = 0;
        let mut max_size: u32 = 0;
        // SAFETY: both out-params are valid, writable locals; the call only writes through them.
        unsafe {
            ffi::virgl_renderer_get_cap_set(
                ffi::VIRGL_RENDERER_CAPSET_VENUS,
                &mut max_ver,
                &mut max_size,
            )
        };
        // No Venus capability advertised → we cannot answer the handshake with real data.
        if max_size == 0 {
            return Err(EngineError::NoVenusCapset);
        }

        // Allocate exactly the blob the renderer will fill. The vtest wire framing counts the
        // capset in dwords, and `max_size` from virglrenderer is always a multiple of 4, so this
        // buffer frames cleanly; we do not need to pad.
        let mut caps = vec![0u8; max_size as usize];
        // Fill the buffer with the capset for the requested version. SAFETY: `caps` is a live,
        // writable allocation of exactly `max_size` bytes, which is what `fill_caps` writes for
        // this `(set, version)` per the size just returned by `get_cap_set`.
        unsafe {
            ffi::virgl_renderer_fill_caps(
                ffi::VIRGL_RENDERER_CAPSET_VENUS,
                version,
                caps.as_mut_ptr() as *mut c_void,
            )
        };
        Ok(caps)
    }

    /// Creates a *classic* (non-blob) 2D resource — a GPU-backed texture with a real
    /// format/width/height/stride — attaches it to `ctx_id`, queries its image layout exactly
    /// once, and tracks it (id, context, and cached layout) for later `read_back`.
    ///
    /// This is the resource kind `read_back` can actually read pixels out of (see that method's
    /// doc comment for why blob resources, `create_blob_resource`'s kind, cannot be read back the
    /// same way). `bind` is fixed to render-target + sampler-view, which covers both "the GPU
    /// draws into this" and "a shader samples from this" — the two ways Task 4's live drive might
    /// plausibly use a readback target.
    ///
    /// # Why this queries `resource_get_info` immediately (not later, in `read_back`)
    /// See [`TrackedResource`]'s doc comment for the full story: calling
    /// `virgl_renderer_resource_get_info` *after* content has been written/rendered into a
    /// resource has been found, empirically, to reset that resource back to blank. Querying it
    /// here — immediately after creation, before anything has ever been written — is the one point
    /// in this resource's lifetime where doing so is safe (there is no content yet to lose), and
    /// `read_back` reuses the cached answer forever after rather than ever asking again.
    ///
    /// # Inputs / outputs
    /// - `ctx_id`: the context this resource is created for (must already exist).
    /// - `width`, `height`: resource dimensions in pixels.
    /// - `format`: a `VIRGL_FORMAT_*` code (e.g. `1` = `VIRGL_FORMAT_B8G8R8A8_UNORM`).
    /// - Returns the engine-assigned resource id on success.
    ///
    /// # Failure modes
    /// - `ResourceCtxIdOutOfRange`: `ctx_id` does not fit a C `int`.
    /// - `UnknownContext`: `ctx_id` does not name a context this engine created.
    /// - `ResourceIdOverflow`: the engine's own id counter overflowed (practically unreachable).
    /// - `ResourceCreateFailed`: virglrenderer rejected the resource.
    /// - `ResourceInfoFailed`: the immediate post-creation `resource_get_info` failed.
    /// - `InvalidResourceInfo`: `resource_get_info` succeeded but reported a zero
    ///   width/height/stride.
    /// - `InvalidResourceStride`: `resource_get_info` succeeded but reported a `stride` narrower
    ///   than one tightly-packed row (`width * bytes_per_pixel`) for a format `read_back` knows how
    ///   to unpad — reading it back would slice out of bounds.
    ///
    /// # No leaked resource on any error path
    /// Once `virgl_renderer_resource_create` and `virgl_renderer_ctx_attach_resource` have both
    /// succeeded, the resource genuinely exists inside virglrenderer. Every error branch that can
    /// still occur after that point (`ResourceInfoFailed`, `InvalidResourceInfo`,
    /// `InvalidResourceStride`) therefore detaches then unrefs the resource — mirroring
    /// `unref_resource`/`Drop`'s teardown order — before returning `Err`, so a failed call never
    /// leaves an orphaned GPU resource that nothing will ever unref (it was never inserted into
    /// `self.resources`, so `unref_resource`/`Drop` would never otherwise see it).
    fn create_resource(
        &mut self,
        ctx_id: u32,
        width: u32,
        height: u32,
        format: u32,
    ) -> Result<u32, EngineError> {
        // `ctx_attach_resource` takes `ctx_id` as a C `int`; guard the untrusted-origin id rather
        // than let a value above `INT_MAX` wrap to a different (wrong) context.
        let ctx_id_c =
            c_int::try_from(ctx_id).map_err(|_| EngineError::ResourceCtxIdOutOfRange { ctx_id })?;

        // Reject a `ctx_id` this engine never created (or already destroyed). Attaching a
        // resource to an unknown context would either fail deep inside virglrenderer with an
        // opaque code, or — worse — silently attach to whatever that context number happens to
        // mean to virglrenderer. This makes `ctx_id` a checked invariant from here on, not
        // caller discipline (see `wait_for_context_fence`'s doc comment, which relies on the same
        // "ctx_id names a live context" property).
        if !self.contexts.contains(&ctx_id) {
            return Err(EngineError::UnknownContext { ctx_id });
        }

        // Allocate the next engine-assigned id and pre-validate it fits `c_int` (required by
        // `ctx_attach_resource`) *before* calling into C, so a failure *up to this point* never
        // leaves a half-created resource behind. (A failure *after* `resource_create` +
        // `ctx_attach_resource` succeed is a different case — see "No leaked resource on any
        // error path" above; those branches explicitly detach+unref before returning.)
        let resource_id = self.next_resource_id;
        let resource_id_c = c_int::try_from(resource_id)
            .map_err(|_| EngineError::ResourceIdOverflow { resource_id })?;
        self.next_resource_id = self.next_resource_id.saturating_add(1);

        // `VIRGL_RES_BIND_RENDER_TARGET (1<<1) | VIRGL_RES_BIND_SAMPLER_VIEW (1<<3)`: this
        // resource can be drawn into and sampled from, covering both plausible Task 4 uses.
        const PIPE_TEXTURE_2D: u32 = 2; // gallium's `pipe_texture_target` enum, stable ABI value.
        const VIRGL_RES_BIND_RENDER_TARGET: u32 = 1 << 1;
        const VIRGL_RES_BIND_SAMPLER_VIEW: u32 = 1 << 3;
        let mut args = ffi::VirglRendererResourceCreateArgs {
            handle: resource_id,
            target: PIPE_TEXTURE_2D,
            format,
            bind: VIRGL_RES_BIND_RENDER_TARGET | VIRGL_RES_BIND_SAMPLER_VIEW,
            width,
            height,
            depth: 1,      // a 2D resource is one layer deep.
            array_size: 1, // not a texture array.
            last_level: 0, // no mipmaps.
            nr_samples: 0, // not multisampled.
            flags: 0,
        };
        // SAFETY: `args` is a fully-initialized, live struct for the call's duration; no iovecs
        // (`num_iovs = 0`, null pointer) since this is a render target the host GPU backs with
        // its own texture storage, not host memory we supply up front.
        let rc = unsafe { ffi::virgl_renderer_resource_create(&mut args, std::ptr::null_mut(), 0) };
        if rc != 0 {
            return Err(EngineError::ResourceCreateFailed {
                ctx_id,
                resource_id,
                rc,
                reason: errno_name(rc),
            });
        }

        // Make the resource visible to the context. SAFETY: both ids were just validated to fit
        // `c_int`, `ctx_id` names a live context, and `resource_id` names the resource just
        // created above.
        unsafe { ffi::virgl_renderer_ctx_attach_resource(ctx_id_c, resource_id_c) };

        // Query the image layout NOW, while it is still safe to do so (see this method's and
        // `TrackedResource`'s doc comments) — this is the *only* `resource_get_info` call this
        // resource will ever get.
        let mut info = ffi::VirglRendererResourceInfo::default();
        // SAFETY: `info` is a valid, writable local; `resource_id_c` names the resource just
        // created and attached above.
        let rc = unsafe { ffi::virgl_renderer_resource_get_info(resource_id_c, &mut info) };
        if rc != 0 {
            // The resource exists in virglrenderer (created + attached above) but will never be
            // inserted into `self.resources`, so nothing will ever unref it unless we do so here.
            // Detach then unref — the same order `unref_resource`/`Drop` use. SAFETY: both ids
            // were validated to fit `c_int` above; `resource_id_c`/`resource_id` name the
            // resource just created and attached above, not yet unref'd.
            unsafe { ffi::virgl_renderer_ctx_detach_resource(ctx_id_c, resource_id_c) };
            unsafe { ffi::virgl_renderer_resource_unref(resource_id) };
            return Err(EngineError::ResourceInfoFailed {
                resource_id,
                rc,
                reason: errno_name(rc),
            });
        }
        if info.width == 0 || info.height == 0 || info.stride == 0 {
            // Same leak concern as the branch above: release the resource before reporting the
            // error, rather than abandoning it un-tracked and un-releasable.
            // SAFETY: as above.
            unsafe { ffi::virgl_renderer_ctx_detach_resource(ctx_id_c, resource_id_c) };
            unsafe { ffi::virgl_renderer_resource_unref(resource_id) };
            return Err(EngineError::InvalidResourceInfo {
                resource_id,
                width: info.width,
                height: info.height,
                stride: info.stride,
            });
        }

        // Guard against a stride narrower than a single tightly-packed row for a format
        // `read_back`/`repack_tight` know how to unpad. Unknown formats have no `bytes_per_pixel`
        // to compare against yet — `read_back`'s `UnsupportedReadbackFormat` check refuses those
        // before ever reaching `repack_tight`, so they need no guard here. But for a *known*
        // format, a too-small stride would make `repack_tight`'s `raw[start..start + row_bytes]`
        // slice run past the row it actually has — an out-of-bounds panic, not a typed error.
        // Converting that into `InvalidResourceStride` here (while the resource can still be
        // cleanly released) closes that gap.
        if let Some(bpp) = bytes_per_pixel(info.virgl_format) {
            let min_stride = info.width.saturating_mul(bpp);
            if info.stride < min_stride {
                // SAFETY: as above.
                unsafe { ffi::virgl_renderer_ctx_detach_resource(ctx_id_c, resource_id_c) };
                unsafe { ffi::virgl_renderer_resource_unref(resource_id) };
                return Err(EngineError::InvalidResourceStride {
                    resource_id,
                    width: info.width,
                    stride: info.stride,
                    bytes_per_pixel: bpp,
                });
            }
        }

        // Track ownership + the cached layout so `read_back` knows which context's fences to wait
        // on and how to interpret this resource's bytes without ever asking virglrenderer again.
        self.resources.insert(
            resource_id,
            TrackedResource {
                ctx_id,
                image: Some(CachedImageInfo {
                    width: info.width,
                    height: info.height,
                    stride: info.stride,
                    format: info.virgl_format,
                }),
            },
        );
        Ok(resource_id)
    }

    /// Creates a *blob* resource — opaque host/guest-shareable memory, the resource type Venus's
    /// real wire protocol (`VCMD_RESOURCE_CREATE_BLOB`) allocates for device memory — attaches it
    /// to `ctx_id`, and tracks it.
    ///
    /// # Empirical limitation (see `read_back`'s doc comment for the full story)
    /// A blob resource has no format/dimension concept of its own; `virgl_renderer_resource_get_info`
    /// fails on one (confirmed on real hardware in this task). `read_back` therefore cannot read
    /// pixels out of a resource created here — only out of one created via `create_resource`. This
    /// method exists so `VCMD_RESOURCE_CREATE_BLOB` routes through a *real* engine resource path
    /// (Task 3's Step 2 requirement) rather than a vtest-local counter; turning its output into
    /// readable pixels is Task 4's concern once a live Venus client's actual object graph is known.
    ///
    /// # Inputs / outputs
    /// - `ctx_id`: the context this resource is created for (must already exist).
    /// - `blob_mem`: `VIRGL_RENDERER_BLOB_MEM_*` (Venus typically uses `HOST3D` for device memory
    ///   in this no-VM vtest setup, since there is no real guest to back `GUEST`/`HOST3D_GUEST`
    ///   memory with pages).
    /// - `blob_flags`: `VIRGL_RENDERER_BLOB_FLAG_*` (e.g. `USE_MAPPABLE`).
    /// - `blob_id`: the client-chosen blob id from the wire message (0 is valid for Venus).
    /// - `size`: requested size in bytes.
    /// - Returns the engine-assigned resource id on success.
    ///
    /// # Failure modes
    /// - `ResourceCtxIdOutOfRange`: `ctx_id` does not fit a C `int`.
    /// - `UnknownContext`: `ctx_id` does not name a context this engine created.
    /// - `ResourceIdOverflow`: the engine's own id counter overflowed (practically unreachable).
    /// - `BlobResourceCreateFailed`: virglrenderer rejected the resource (e.g. a guest-backed
    ///   `blob_mem` was requested, which this no-VM setup cannot satisfy — see above).
    fn create_blob_resource(
        &mut self,
        ctx_id: u32,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
    ) -> Result<u32, EngineError> {
        let ctx_id_c =
            c_int::try_from(ctx_id).map_err(|_| EngineError::ResourceCtxIdOutOfRange { ctx_id })?;
        // Same check `create_resource` performs: refuse a `ctx_id` this engine did not itself
        // create, rather than let `ctx_attach_resource` (below) target an unowned/unknown context.
        if !self.contexts.contains(&ctx_id) {
            return Err(EngineError::UnknownContext { ctx_id });
        }
        let resource_id = self.next_resource_id;
        let resource_id_c = c_int::try_from(resource_id)
            .map_err(|_| EngineError::ResourceIdOverflow { resource_id })?;
        self.next_resource_id = self.next_resource_id.saturating_add(1);

        let args = ffi::VirglRendererResourceCreateBlobArgs {
            res_handle: resource_id,
            ctx_id,
            blob_mem,
            blob_flags,
            blob_id,
            size,
            // No guest pages to describe: this no-VM vtest server has no real guest memory, so
            // only host-resident blob types (HOST3D) are actually satisfiable here.
            iovecs: std::ptr::null(),
            num_iovs: 0,
        };
        // SAFETY: `args` is fully initialized and valid for the call's duration.
        let rc = unsafe { ffi::virgl_renderer_resource_create_blob(&args) };
        if rc != 0 {
            return Err(EngineError::BlobResourceCreateFailed {
                ctx_id,
                resource_id,
                rc,
                reason: errno_name(rc),
            });
        }

        // SAFETY: both ids were just validated to fit `c_int`; `ctx_id` names a live context and
        // `resource_id` names the resource just created above.
        unsafe { ffi::virgl_renderer_ctx_attach_resource(ctx_id_c, resource_id_c) };

        // No `resource_get_info` call here — deliberately: it is known to fail for a blob resource
        // (confirmed empirically), so there is nothing to cache. `image: None` is what makes
        // `read_back` refuse this resource with a clear `ResourceNotReadable` instead of guessing.
        self.resources.insert(
            resource_id,
            TrackedResource {
                ctx_id,
                image: None,
            },
        );
        Ok(resource_id)
    }

    /// Releases a resource created by `create_resource` or `create_blob_resource`.
    ///
    /// Mirrors the wire protocol's `VCMD_RESOURCE_UNREF`, which has no reply and cannot fail from
    /// the caller's perspective — so, like `virgl_renderer_resource_unref` itself, this returns
    /// nothing. An id this engine never created (or already released) is silently ignored, the
    /// same "fire and forget, idempotent" semantics the vtest layer previously implemented as a
    /// local stub — the difference here is that a *tracked* id now actually reaches virglrenderer
    /// (detach, then unref, the documented teardown order) instead of only being forgotten
    /// locally.
    fn unref_resource(&mut self, resource_id: u32) {
        // Only resources we created are safe to hand to virglrenderer's unref — an untracked id
        // might not correspond to any resource we own.
        let Some(tracked) = self.resources.remove(&resource_id) else {
            return;
        };
        // Detach before unref (the documented order). Both ids were validated to fit `c_int` when
        // this resource was created and inserted into `self.resources`, so re-validating here is
        // defensive, not expected to ever fail; on the (unreachable) failure we skip the detach
        // call rather than panic — the following unref still releases the resource either way.
        if let (Ok(ctx_id_c), Ok(resource_id_c)) = (
            c_int::try_from(tracked.ctx_id),
            c_int::try_from(resource_id),
        ) {
            // SAFETY: both ids are validated and were live at creation time.
            unsafe { ffi::virgl_renderer_ctx_detach_resource(ctx_id_c, resource_id_c) };
        }
        // SAFETY: `resource_id` names a resource we created and have not yet unref'd (we just
        // removed it from `self.resources`, so a second call is a no-op — the tracking prevents
        // a double-unref).
        unsafe { ffi::virgl_renderer_resource_unref(resource_id) };
    }

    /// Fence-waits for every command submitted to a resource's context to retire, then reads the
    /// resource's pixels back to CPU memory as a tightly-packed [`EngineFrame`].
    ///
    /// # Why fence-wait first (the correctness point this task exists for)
    /// Command submission (`submit`) is asynchronous: `virgl_renderer_submit_cmd` returning 0
    /// means the GPU *accepted* the command stream, not that it *finished executing* it. Reading
    /// pixels before the GPU finishes would return a partially-rendered (or entirely stale) frame
    /// — a real, silent correctness bug, not a hypothetical one. So before touching the resource's
    /// contents, this creates a per-context fence (`virgl_renderer_context_create_fence`) and
    /// polls (`virgl_renderer_context_poll`) until `write_context_fence` reports it retired — see
    /// `wait_for_context_fence`. Only *then* is it safe to read.
    ///
    /// # This never calls `virgl_renderer_resource_get_info` — the single most important finding
    /// The obvious implementation queries the resource's format/dimensions/stride right here, just
    /// before reading. **That is wrong, and silently so**: a scratch experiment on real hardware
    /// (`libvirglrenderer` 1.2.0), reproduced independently in both a standalone C program and this
    /// crate's own code, found that calling `virgl_renderer_resource_get_info` on a classic
    /// resource *after* content has been written into it resets that resource's content back to
    /// all-zero for every subsequent read — even though the `get_info` call itself reports success
    /// and perfectly plausible values. There is no mention of this in virglrenderer's public
    /// documentation. The fix is architectural, not a workaround: `create_resource` queries
    /// `resource_get_info` exactly **once**, immediately after creation (before anything could ever
    /// have been written), and caches the answer in [`TrackedResource::image`]; `read_back` uses
    /// that cache and never calls `resource_get_info` itself. See [`TrackedResource`]'s doc comment
    /// for the full experimental evidence.
    ///
    /// # Why `ctx_id = 0` for the actual transfer (an empirically-discovered constraint)
    /// A resource attached to a Venus context (created via `create_venus_context`, which passes
    /// `VIRGL_RENDERER_RENDER_SERVER`) is served by the render-server *proxy*, and the proxy does
    /// **not** support the classic transfer path: the same scratch experiment, calling
    /// `transfer_read_iov`/`transfer_write_iov` with the resource's own Venus `ctx_id`, logged
    /// `"proxy: no transfer support for ctx 1 and res 1"` and returned a nonzero rc every time.
    /// Calling the *same* function with `ctx_id = 0` instead (bypassing proxy routing; ctx 0 is
    /// vrend's own classic/legacy path) succeeded and round-tripped known pixel content correctly —
    /// confirmed by the GPU-gated test in this module. So `read_back` always passes `0`, regardless
    /// of which Venus context actually owns the resource; the fence-wait above (against the *real*
    /// owning context) is what still proves the GPU work is done before this transfer reads the
    /// result.
    ///
    /// # Only classic resources are supported (documented limitation, not silently papered over)
    /// This only works for resources created via `create_resource`. A blob resource (from
    /// `create_blob_resource` — the kind Venus's real wire protocol actually allocates) has no
    /// format/dimension info (`virgl_renderer_resource_get_info` fails on one, confirmed
    /// empirically) and is not consumable by `transfer_read_iov` on this virglrenderer version —
    /// such a resource's `TrackedResource::image` is `None`, and this method refuses it up front
    /// with `ResourceNotReadable`. Turning a real Venus-rendered blob's bytes into an `EngineFrame`
    /// needs either `virgl_renderer_resource_map` plus externally-known image layout (Venus does
    /// not expose `VkSubresourceLayout` over the vtest wire), or a companion classic "swapchain"
    /// resource a live client's WSI copies into — both are Task 4 (live drive) design decisions,
    /// deliberately out of this task's scope (see the module/task docs).
    ///
    /// # Stride-honoring discipline (the SP0/SP3 precedent this task follows)
    /// The cached `stride` is frequently **larger** than `width * bytes_per_pixel` (GPU drivers
    /// commonly pad rows to an alignment boundary for DMA efficiency). This reads exactly
    /// `stride * height` bytes from the GPU (never assuming `stride == width * bpp`), then strips
    /// the per-row padding down to a tightly-packed `width * bpp` per row for the returned
    /// `EngineFrame` (see `repack_tight`).
    ///
    /// # Inputs / outputs
    /// - `resource_id`: a resource id previously returned by `create_resource` on this engine.
    /// - Returns a tightly-packed [`EngineFrame`] on success.
    ///
    /// # Failure modes
    /// - `UnknownResource`: this engine never created (or already unref'd) `resource_id`.
    /// - `ResourceNotReadable`: `resource_id` names a blob resource (`create_blob_resource`), which
    ///   has no cached image layout to read back with.
    /// - `FenceCreateFailed` / `FenceTimeout`: the completion fence could not be created, or never
    ///   retired within `FENCE_WAIT_TIMEOUT`.
    /// - `UnsupportedReadbackFormat`: the resource's cached `virgl_format` is not one of the
    ///   handful of 32-bit-per-pixel formats this function knows how to repack (see
    ///   `bytes_per_pixel`).
    /// - `TransferReadFailed`: `virgl_renderer_transfer_read_iov` failed.
    fn read_back(&mut self, resource_id: u32) -> Result<EngineFrame, EngineError> {
        // Look up this resource's tracked state. An untracked id cannot be fenced or read
        // meaningfully.
        let tracked = self
            .resources
            .get(&resource_id)
            .ok_or(EngineError::UnknownResource { resource_id })?;
        let ctx_id = tracked.ctx_id;
        // A blob resource has no cached layout (see this method's and `TrackedResource`'s doc
        // comments) — refuse it clearly rather than attempt a transfer that cannot succeed.
        let image = tracked
            .image
            .ok_or(EngineError::ResourceNotReadable { resource_id })?;

        // Ring 0: the only ring this C0 skeleton's vtest layer uses (`VTEST_CONTEXT_ID`'s one
        // context, one ring — see `vtest.rs`). A multi-ring readback target is future scope.
        const RING_IDX: u32 = 0;
        self.wait_for_context_fence(ctx_id, RING_IDX)?;

        let bpp = bytes_per_pixel(image.format).ok_or(EngineError::UnsupportedReadbackFormat {
            resource_id,
            format: image.format,
        })?;

        // Allocate exactly what the GPU will fill: `stride` bytes per row (its real, possibly
        // padded, row pitch) times `height` rows — the stride-honoring discipline this task exists
        // to enforce (never `width * bpp * height`, which would under-read a padded resource).
        let raw_len = (image.stride as usize).saturating_mul(image.height as usize);
        let mut raw = vec![0u8; raw_len];
        let mut iov = ffi::IoVec {
            iov_base: raw.as_mut_ptr() as *mut c_void,
            iov_len: raw.len(),
        };
        let mut region = ffi::VirglBox {
            x: 0,
            y: 0,
            z: 0,
            w: image.width,
            h: image.height,
            d: 1,
        };
        // SAFETY: `raw`/`iov` describe a live, writable buffer of exactly `raw_len` bytes; `region`
        // covers the whole resource (`level 0`, the only mip level `create_resource` allocates);
        // `ctx_id = 0` is the empirically-required routing documented above, not the resource's
        // real owning context. Crucially, this is the ONLY virglrenderer call this function makes
        // to actually touch the resource — no `resource_get_info` call, per the doc comment above.
        let rc = unsafe {
            ffi::virgl_renderer_transfer_read_iov(
                resource_id,
                0, // deliberately NOT `ctx_id` — see the doc comment above.
                0, // level 0: the only mip level this resource has.
                image.stride,
                0, // layer_stride: irrelevant for a single-layer 2D resource.
                &mut region,
                0, // offset: read from the start of the resource.
                &mut iov,
                1,
            )
        };
        if rc != 0 {
            return Err(EngineError::TransferReadFailed {
                resource_id,
                rc,
                reason: errno_name(rc),
            });
        }

        let pixels = repack_tight(&raw, image.width, image.height, image.stride, bpp);
        Ok(EngineFrame {
            width: image.width,
            height: image.height,
            pixels,
            format: image.format,
        })
    }
}

impl VirglEngine {
    /// Blocks until every command submitted to `(ctx_id, ring_idx)` so far has retired on the GPU,
    /// or [`FENCE_WAIT_TIMEOUT`] elapses.
    ///
    /// Creates a fresh per-context fence (`virgl_renderer_context_create_fence`) carrying a
    /// monotonically-increasing `fence_id`, then repeatedly calls `virgl_renderer_context_poll`
    /// (which drives the `write_context_fence` callback for any fence that has retired) and checks
    /// the shared `FenceState` mailbox on the cookie, sleeping `FENCE_POLL_INTERVAL` between
    /// attempts. See `write_context_fence`'s and `virgl_renderer_context_create_fence`'s doc
    /// comments in `ffi.rs` for how this pairing was confirmed empirically (as opposed to the
    /// legacy `virgl_renderer_create_fence`/`write_fence` pairing, deliberately not used here).
    ///
    /// # Inputs / outputs
    /// - `ctx_id`, `ring_idx`: which context/ring to wait on.
    /// - Returns `Ok(())` once the fence retires.
    ///
    /// # Failure modes
    /// - `FenceCreateFailed`: virglrenderer rejected the fence creation itself.
    /// - `FenceTimeout`: the fence never retired within `FENCE_WAIT_TIMEOUT`.
    fn wait_for_context_fence(&mut self, ctx_id: u32, ring_idx: u32) -> Result<(), EngineError> {
        // A fresh id per wait, so this call is unambiguously asking about *this* fence rather than
        // one an earlier, possibly-still-outstanding wait created.
        let fence_id = self.next_fence_id;
        self.next_fence_id = self.next_fence_id.wrapping_add(1);

        // Create the fence. `flags = 0`: we do not set `VIRGL_RENDERER_FENCE_FLAG_MERGEABLE`,
        // since we want this specific fence to reliably invoke `write_context_fence`, never be
        // silently coalesced into another one. SAFETY: `ctx_id` names a live context (the caller
        // looked it up from `self.resources`, populated only with contexts that existed at
        // resource-creation time and are only removed by `Drop`, which cannot run concurrently
        // with this call since it takes `&mut self`).
        let rc = unsafe { ffi::virgl_renderer_context_create_fence(ctx_id, 0, ring_idx, fence_id) };
        if rc != 0 {
            return Err(EngineError::FenceCreateFailed {
                ctx_id,
                ring_idx,
                rc,
                reason: errno_name(rc),
            });
        }

        // Poll until the fence retires or we time out. Nothing pumps fence completion in the
        // background (see `FENCE_POLL_INTERVAL`'s doc comment), so this loop must drive it itself.
        let deadline = Instant::now() + FENCE_WAIT_TIMEOUT;
        loop {
            // SAFETY: `ctx_id` names a live context; forcing retirement is always safe to call.
            unsafe { ffi::virgl_renderer_context_poll(ctx_id) };
            if self
                .cookie
                .fence_state
                .is_retired(ctx_id, ring_idx, fence_id)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(EngineError::FenceTimeout {
                    ctx_id,
                    ring_idx,
                    fence_id,
                });
            }
            // Yield the CPU briefly rather than busy-spinning; see `FENCE_POLL_INTERVAL`'s doc.
            std::thread::sleep(FENCE_POLL_INTERVAL);
        }
    }
}

/// Bytes per pixel for the small set of 32-bit-per-pixel `VIRGL_FORMAT_*` codes a Venus/Vulkan
/// swapchain plausibly renders into (BGRA/RGBA/ARGB/XRGB channel order, UNORM or SRGB encoding —
/// the encoding changes how shaders interpret the bytes, not their count or layout). Any other
/// format is refused by `read_back` rather than guessed, per this crate's discipline of never
/// silently inventing domain data. Pinned from virglrenderer's `virgl_hw.h` (`enum
/// virgl_formats`), the same source Task 1 used to pin the Venus capset id.
///
/// # Inputs / outputs
/// - `virgl_format`: the raw `VIRGL_FORMAT_*` code from `virgl_renderer_resource_get_info`.
/// - Returns `Some(4)` for a recognized 32-bit format, `None` otherwise.
fn bytes_per_pixel(virgl_format: u32) -> Option<u32> {
    /// `VIRGL_FORMAT_B8G8R8A8_UNORM` — the format this task's GPU test creates resources with,
    /// and a plausible default Venus/Vulkan swapchain format.
    const B8G8R8A8_UNORM: u32 = 1;
    const B8G8R8X8_UNORM: u32 = 2;
    const A8R8G8B8_UNORM: u32 = 3;
    const X8R8G8B8_UNORM: u32 = 4;
    const R8G8B8A8_UNORM: u32 = 67;
    const R8G8B8X8_UNORM: u32 = 134;
    const A8B8G8R8_SRGB: u32 = 98;
    const X8B8G8R8_SRGB: u32 = 99;
    const B8G8R8A8_SRGB: u32 = 100;
    const B8G8R8X8_SRGB: u32 = 101;
    const A8R8G8B8_SRGB: u32 = 102;
    const X8R8G8B8_SRGB: u32 = 103;
    const R8G8B8A8_SRGB: u32 = 104;
    match virgl_format {
        B8G8R8A8_UNORM | B8G8R8X8_UNORM | A8R8G8B8_UNORM | X8R8G8B8_UNORM | R8G8B8A8_UNORM
        | R8G8B8X8_UNORM | A8B8G8R8_SRGB | X8B8G8R8_SRGB | B8G8R8A8_SRGB | B8G8R8X8_SRGB
        | A8R8G8B8_SRGB | X8R8G8B8_SRGB | R8G8B8A8_SRGB => Some(4),
        _ => None,
    }
}

/// Strips a resource's row padding so pixel data is tightly packed, honoring the GPU-reported row
/// `stride` rather than assuming `stride == width * bpp` (the stride-honoring discipline this task
/// exists to enforce — see `read_back`'s doc comment). Pure and GPU-independent, so it is
/// unit-testable with synthetic data with no render node required (see the tests below).
///
/// # Inputs / outputs
/// - `raw`: at least `stride * height` bytes, as filled by `virgl_renderer_transfer_read_iov`.
/// - `width`, `height`: image dimensions in pixels.
/// - `stride`: the real row pitch in bytes (`raw`'s row spacing); may exceed `width * bpp`.
/// - `bpp`: bytes per pixel for the image's format.
/// - Returns exactly `width * bpp * height` bytes: each row's real pixels, back to back, with the
///   padding between `width * bpp` and `stride` dropped.
///
/// # Panics
/// If `raw` is shorter than `stride * height` (a caller bug — `read_back` always allocates exactly
/// that much before calling this), the out-of-bounds slice indexing below panics rather than
/// silently reading garbage or wrapping. This is only reachable from a bug in this crate, never
/// from untrusted input (the "how much to allocate" decision is entirely ours).
fn repack_tight(raw: &[u8], width: u32, height: u32, stride: u32, bpp: u32) -> Vec<u8> {
    let row_bytes = (width as usize) * (bpp as usize); // the real, unpadded row length to keep.
    let stride = stride as usize;
    let mut out = Vec::with_capacity(row_bytes * height as usize);
    for row in 0..height as usize {
        let start = row * stride; // this row's start offset in the padded GPU buffer.
        out.extend_from_slice(&raw[start..start + row_bytes]); // keep only the real pixel bytes.
    }
    out
}

impl Drop for VirglEngine {
    /// Tears the engine down in the correct order and releases the single-instance lock.
    ///
    /// Order matters: release every resource we created, then destroy every context we created,
    /// then `virgl_renderer_cleanup` (which releases the EGL winsys and reaps the render-server
    /// subprocess), and only then release the global lock so a subsequent `VirglEngine::new` can
    /// safely re-initialize. Resources before contexts because a resource can reference its
    /// context (releasing the context first would leave a dangling reference); the boxed cookie is
    /// a field, so it is dropped *after* this method returns — i.e. it stays valid through
    /// `virgl_renderer_cleanup`, which is the last C call that could touch it.
    fn drop(&mut self) {
        // Release every resource we created. SAFETY: these (ctx_id, resource_id) pairs were
        // recorded by successful `create_resource`/`create_blob_resource` calls on the
        // still-initialized renderer; detach/unref are safe to call once per resource, which this
        // loop does (each id appears once in the map).
        for (&resource_id, tracked) in &self.resources {
            if let (Ok(ctx_id_c), Ok(resource_id_c)) = (
                c_int::try_from(tracked.ctx_id),
                c_int::try_from(resource_id),
            ) {
                unsafe { ffi::virgl_renderer_ctx_detach_resource(ctx_id_c, resource_id_c) };
            }
            unsafe { ffi::virgl_renderer_resource_unref(resource_id) };
        }

        // Destroy each live context. SAFETY: these ids were returned by successful context
        // creation on the still-initialized renderer; destroy is idempotent-safe here.
        for &ctx_id in &self.contexts {
            unsafe { ffi::virgl_renderer_context_destroy(ctx_id) };
        }

        // Cookie pointer for cleanup — the same one we registered at init.
        let cookie_ptr = (&*self.cookie as *const ffi::Cookie) as *mut c_void;
        // Tear down the global renderer. SAFETY: we hold an initialized engine, so exactly one
        // `virgl_renderer_init` is outstanding; `cookie_ptr` is still valid (the box lives until
        // after this returns).
        unsafe { ffi::virgl_renderer_cleanup(cookie_ptr) };

        // Release the lock last, so the global renderer is fully torn down before another engine
        // may initialize.
        ENGINE_ACTIVE.store(false, Ordering::Release);
    }
}

/// Cheap probe for whether a usable Venus-capable renderer is available on `render_node`, for
/// gating tests (and, later, runtime capability checks). Returns `true` only if BOTH hold:
/// 1. the render node can be opened read/write (a real, permitted GPU render node exists), and
/// 2. virglrenderer reports the Venus capset (4) as supported (non-zero capset size).
///
/// It does not initialize the renderer, so it is safe to call without holding the single-instance
/// lock and cheap enough to call at the top of every GPU test. On a CI runner without a render
/// node (or without Venus), it returns `false` and the tests skip cleanly rather than failing.
///
/// # Inputs / outputs
/// - `render_node`: path to a DRM render node.
/// - Returns `true` if a Venus context is plausibly creatable there, `false` otherwise.
pub fn virgl_available(render_node: &Path) -> bool {
    // (1) Must be able to open the node read/write — the same access `get_drm_fd` needs.
    if std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(render_node)
        .is_err()
    {
        return false;
    }

    // (2) Ask virglrenderer whether Venus (capset 4) is supported. `get_cap_set` reads static
    // capability tables and is safe to call without init. A zero `max_size` means unsupported.
    let mut max_ver: u32 = 0;
    let mut max_size: u32 = 0;
    // SAFETY: both out-params are valid, writable locals; the call only writes through them.
    unsafe {
        ffi::virgl_renderer_get_cap_set(
            ffi::VIRGL_RENDERER_CAPSET_VENUS,
            &mut max_ver,
            &mut max_size,
        )
    };

    // Venus is usable only if the library reports a non-zero capset size for it.
    max_size > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only this module's tests need explicit mutual exclusion between GPU-touching tests within
    // this binary (see `GPU_TEST_LOCK` below); `c_int`, `c_void`, and `Path` are already in scope
    // via `use super::*`.
    use std::sync::Mutex;

    /// Serializes this module's GPU-touching tests against each other, mirroring
    /// `tests/reliability.rs`'s `GPU_TEST_LOCK`. This is a *separate* lock (and, because
    /// `#[cfg(test)]` unit tests and `tests/*.rs` integration tests build into separate binaries /
    /// separate OS processes, a separate process) from that one — no cross-binary contention is
    /// possible for a process-global C singleton, so each test binary only needs to serialize its
    /// own GPU tests against each other.
    static GPU_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// The DRM render node used throughout this crate's GPU tests.
    const RENDER_NODE: &str = "/dev/dri/renderD128";

    /// `VIRGL_FORMAT_B8G8R8A8_UNORM` (from virglrenderer's `virgl_hw.h`) — a plausible Venus
    /// swapchain pixel format, and the one this task's round-trip test creates resources with.
    const B8G8R8A8_UNORM: u32 = 1;

    /// The real end-to-end capability this task builds, proven against the real GPU: create a
    /// classic resource, seed it with known pixel content (standing in for a live Venus client's
    /// rendering — Task 4), fence-wait, and read it back — asserting the bytes round-trip exactly.
    ///
    /// This is the strongest test this task can run without a live Venus client (see the module
    /// docs on `read_back` for exactly why a live client is Task 4's concern): it exercises the
    /// real `virgl_renderer_resource_create`, `ctx_attach_resource`, the *one-shot*
    /// `virgl_renderer_resource_get_info` (see `TrackedResource`'s doc comment for why it must be
    /// one-shot), `virgl_renderer_context_create_fence` + `write_context_fence` + `context_poll`,
    /// and `transfer_read_iov` calls on real hardware, through the actual public `RenderEngine`
    /// trait methods (`create_resource`, `read_back`) — only the pixel-seeding step uses a
    /// test-only raw FFI call in place of a real Venus render.
    #[test]
    fn create_seed_fence_wait_and_read_back_round_trips_known_pixels() {
        let _serialize = GPU_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let node = Path::new(RENDER_NODE);
        if !virgl_available(node) {
            eprintln!(
                "SKIP create_seed_fence_wait_and_read_back_round_trips_known_pixels: no usable Venus render node at {RENDER_NODE}"
            );
            return;
        }

        // (1) Bring up the engine and a Venus context — the resource's owning context, and the one
        // `read_back`'s fence-wait will wait on.
        let mut engine =
            VirglEngine::new(node).expect("VirglEngine::new should succeed on a GPU host");
        engine
            .create_venus_context(1)
            .expect("create_venus_context should succeed on a GPU host");

        // A bad ctx_id is rejected before any FFI call, not wrapped into the wrong context.
        match engine.create_resource(u32::MAX, 4, 4, B8G8R8A8_UNORM) {
            Err(EngineError::ResourceCtxIdOutOfRange { ctx_id }) => {
                assert_eq!(ctx_id, u32::MAX);
            }
            other => panic!("expected ResourceCtxIdOutOfRange, got {other:?}"),
        }

        // A ctx_id that fits a C `int` but was never created by this engine (context 99 was
        // never passed to `create_venus_context`) is rejected too — this is the Fix 3 guard: a
        // resource cannot be attached to a context the engine does not itself own. Checked for
        // both `create_resource` and `create_blob_resource`, since both take this same guard.
        match engine.create_resource(99, 4, 4, B8G8R8A8_UNORM) {
            Err(EngineError::UnknownContext { ctx_id }) => {
                assert_eq!(ctx_id, 99);
            }
            other => panic!("expected UnknownContext, got {other:?}"),
        }
        match engine.create_blob_resource(
            99, 0x0002, /* HOST3D */
            0x0001, /* MAPPABLE */
            0, 64,
        ) {
            Err(EngineError::UnknownContext { ctx_id }) => {
                assert_eq!(ctx_id, 99);
            }
            other => panic!("expected UnknownContext, got {other:?}"),
        }

        // (2) Create a real classic 2D resource: 4x4, BGRA8 (4 bytes/pixel), attached to ctx 1.
        // `create_resource` queries and caches this resource's image layout right here, once —
        // see `TrackedResource`'s doc comment for why that one-shot timing is load-bearing.
        const WIDTH: u32 = 4;
        const HEIGHT: u32 = 4;
        let resource_id = engine
            .create_resource(1, WIDTH, HEIGHT, B8G8R8A8_UNORM)
            .expect("create_resource should succeed on a GPU host");

        // (3) Seed known, distinct-per-pixel content via the test-only transfer_write_iov path —
        // standing in for "a live Venus client rendered something here" (Task 4). The buffer we
        // supply is itself tightly packed (stride = width * 4), which we tell vrend explicitly.
        // Crucially, nothing between here and `read_back` below calls `resource_get_info` again —
        // see `TrackedResource`'s doc comment for why a second call would silently zero this out.
        let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
        for i in 0..(WIDTH * HEIGHT) as usize {
            pixels[i * 4] = (i as u8).wrapping_mul(7);
            pixels[i * 4 + 1] = (i as u8).wrapping_mul(13);
            pixels[i * 4 + 2] = (i as u8).wrapping_mul(19);
            pixels[i * 4 + 3] = 0xff;
        }
        let mut write_iov = ffi::IoVec {
            iov_base: pixels.as_mut_ptr() as *mut c_void,
            iov_len: pixels.len(),
        };
        let mut region = ffi::VirglBox {
            x: 0,
            y: 0,
            z: 0,
            w: WIDTH,
            h: HEIGHT,
            d: 1,
        };
        // SAFETY: `write_iov` describes the live `pixels` buffer for the call's duration; `region`
        // covers the whole resource; `ctx_id = 0` bypasses Venus-proxy routing exactly as
        // `read_back` does (see its doc comment) — proven necessary by the scratch experiment this
        // task's report documents (a Venus-ctx transfer logs "no transfer support").
        let rc = unsafe {
            ffi::virgl_renderer_transfer_write_iov(
                resource_id,
                0,
                0,
                WIDTH * 4, // our own buffer's stride: tightly packed, so exactly width * bpp.
                0,
                &mut region,
                0,
                &mut write_iov,
                1,
            )
        };
        assert_eq!(rc, 0, "transfer_write_iov (test seeding) should succeed");

        // (4) The real capability under test: fence-wait, then read back through the public trait.
        let frame = engine
            .read_back(resource_id)
            .expect("read_back should succeed on a GPU host");

        assert_eq!(frame.width, WIDTH);
        assert_eq!(frame.height, HEIGHT);
        assert_eq!(frame.format, B8G8R8A8_UNORM);
        assert_eq!(
            frame.pixels, pixels,
            "read-back pixels must exactly match what was written"
        );

        // An unknown resource id is rejected cleanly, not treated as resource 0 or panicking.
        match engine.read_back(resource_id + 1000) {
            Err(EngineError::UnknownResource { .. }) => {}
            other => panic!("expected UnknownResource, got {other:?}"),
        }

        // (5) A blob resource (the kind Venus's real wire protocol allocates) has no cached image
        // layout, so `read_back` refuses it clearly instead of guessing — the documented
        // limitation this task hands to Task 4 (see `read_back`'s doc comment).
        let blob_id = engine
            .create_blob_resource(
                1, 0x0002, /* HOST3D */
                0x0001, /* MAPPABLE */
                0, 64,
            )
            .expect("create_blob_resource should succeed on a GPU host");
        match engine.read_back(blob_id) {
            Err(EngineError::ResourceNotReadable { resource_id }) => {
                assert_eq!(resource_id, blob_id);
            }
            other => panic!("expected ResourceNotReadable, got {other:?}"),
        }

        eprintln!(
            "OK: created a {WIDTH}x{HEIGHT} BGRA8 resource, fence-waited, and read back {} bytes matching the seeded pixels exactly; blob resource correctly refused",
            frame.pixels.len()
        );
    }

    /// The fence-wait mechanism in isolation: `wait_for_context_fence` must return `Ok(())` for a
    /// live context within `FENCE_WAIT_TIMEOUT`, proving that the pairing of
    /// `virgl_renderer_context_create_fence`, `virgl_renderer_context_poll`, and
    /// `write_context_fence` this task relies on actually works end-to-end on real hardware (not
    /// just that the FFI calls return 0 — that `write_context_fence` really fires and this crate's
    /// `FenceState` really observes it).
    #[test]
    fn fence_wait_completes_on_a_live_context() {
        let _serialize = GPU_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let node = Path::new(RENDER_NODE);
        if !virgl_available(node) {
            eprintln!(
                "SKIP fence_wait_completes_on_a_live_context: no usable Venus render node at {RENDER_NODE}"
            );
            return;
        }

        let mut engine =
            VirglEngine::new(node).expect("VirglEngine::new should succeed on a GPU host");
        engine
            .create_venus_context(1)
            .expect("create_venus_context should succeed on a GPU host");

        // No commands were ever submitted to context 1, so this fence should retire almost
        // immediately (there is nothing outstanding for the GPU to finish first).
        engine
            .wait_for_context_fence(1, 0)
            .expect("a fence on a live, idle context should retire well within the timeout");
        eprintln!("OK: fence created and retired on a live Venus context");
    }

    /// `repack_tight` in isolation, with synthetic (non-GPU) data: proves the stride-honoring
    /// discipline itself — the part of this task's brief explicitly allowing a no-GPU unit test —
    /// independent of whether any real GPU resource on this host actually exhibits row padding
    /// (the 4x4 BGRA8 resource the GPU test above uses happens not to: stride == width * bpp there,
    /// confirmed on this host, so it alone would not catch a stride-handling bug).
    #[test]
    fn repack_tight_strips_padding_between_rows() {
        // A synthetic 2-wide, 3-tall, 4-bytes-per-pixel image with a padded stride of 12 bytes/row
        // (real content is 2*4=8 bytes/row; 4 bytes of padding per row, as a GPU driver might add
        // for DMA alignment). Row `r`'s real pixel bytes are `[r*10, r*10+1, ..]` for easy
        // identification; padding bytes are `0xEE` (a value real pixel data will never coincidentally
        // equal, so any padding leaking into the output is obvious).
        let width = 2u32;
        let height = 3u32;
        let stride = 12u32;
        let bpp = 4u32;
        let mut raw = vec![0xEEu8; (stride * height) as usize];
        for row in 0..height as usize {
            let row_start = row * stride as usize;
            for col in 0..(width * bpp) as usize {
                raw[row_start + col] = (row * 10 + col) as u8;
            }
        }

        let tight = repack_tight(&raw, width, height, stride, bpp);

        // Exactly `width * bpp * height` bytes: no padding survives.
        assert_eq!(tight.len(), (width * bpp * height) as usize);
        // No padding byte (0xEE) survives anywhere in the tightly-packed output.
        assert!(
            !tight.contains(&0xEE),
            "padding bytes must not leak into the tightly-packed output"
        );
        // Row-by-row content is exactly what was written, just with the gaps removed.
        for row in 0..height as usize {
            let expected: Vec<u8> = (0..(width * bpp) as usize)
                .map(|col| (row * 10 + col) as u8)
                .collect();
            let got = &tight[row * (width * bpp) as usize..(row + 1) * (width * bpp) as usize];
            assert_eq!(got, expected.as_slice(), "row {row} mismatch");
        }
    }

    /// `bytes_per_pixel` refuses formats it does not recognize rather than guessing a byte width —
    /// `read_back` depends on this to never silently misinterpret an unexpected format.
    #[test]
    fn bytes_per_pixel_rejects_unrecognized_formats() {
        assert_eq!(bytes_per_pixel(B8G8R8A8_UNORM), Some(4));
        // VIRGL_FORMAT_NONE (0) and an arbitrary unassigned-looking code are both refused.
        assert_eq!(bytes_per_pixel(0), None);
        assert_eq!(bytes_per_pixel(9999), None);
    }
}
