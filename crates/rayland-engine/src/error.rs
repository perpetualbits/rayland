//! [`EngineError`]: the one typed error every failure in this crate maps into.
//!
//! # Why this crate has an error variant for nearly every C call
//! `rayland-engine` drives `libvirglrenderer`, a C library whose functions report failure by
//! returning an errno. The single rule this module exists to enforce is: **a C error must never
//! become a silent success.** Every `virgl_renderer_*` return code is checked at its call site and
//! mapped to exactly one variant here, carrying both the raw code and a human-readable errno name
//! (see [`errno_name`]) — so a failure that surfaces to a user names the C call that produced it,
//! the arguments it was given, and what the kernel/library said about it. The same applies to the
//! POSIX calls in `transport.rs` (`sendmsg`, `memfd_create`, `mmap`, `eventfd`), where a silently
//! dropped failure would present to a live Mesa client as an unexplainable hang rather than an
//! error at all.
//!
//! This module was split out of `virgl.rs` in Task 4a. It is only an enum and one helper, but it
//! had grown past 300 lines — the documentation on each variant is where the domain's pitfalls are
//! recorded — and this repository's convention is that a file should stay small enough to hold in
//! your head.

// The C `int` every virglrenderer return code arrives as.
use std::ffi::c_int;

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
    /// real blob readback would need (Task 4b's concern).
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

    // ---------------------------------------------------------------------------------------
    // Task 4a: SCM_RIGHTS fd passing and the blob resources whose memory it shares.
    //
    // Every variant below exists because the alternative — dropping the failure — would not
    // surface as an error at all, but as a live Mesa Venus client blocked forever in `recvmsg`
    // waiting for a descriptor that will never arrive. "Hangs mysteriously" is the single worst
    // failure mode in this protocol, so these paths are checked with unusual pedantry.
    // ---------------------------------------------------------------------------------------
    /// Sending a file descriptor to the client over `SCM_RIGHTS` failed. Either the underlying
    /// `sendmsg` failed (peer gone, socket not Unix-domain, ...), or the transport has no
    /// fd-passing mechanism at all — the honest, typed refusal a future QUIC transport must give
    /// rather than silently skipping the fd (see [`crate::VtestTransport::send_fd`]).
    #[error("failed to send a file descriptor to the vtest client over SCM_RIGHTS")]
    FdSendFailed {
        /// The underlying OS error.
        source: std::io::Error,
    },

    /// Allocating the anonymous shared memory backing a `GUEST`-family blob resource failed
    /// (`memfd_create` or the `ftruncate` that gives it its length).
    #[error("failed to allocate {size} bytes of shared memory for a blob resource: {source}")]
    ShmCreateFailed {
        /// The blob size that was requested (from the client's wire message).
        size: u64,
        /// The underlying OS error.
        source: std::io::Error,
    },

    /// `mmap`ing a blob resource's shared memory failed. Without the mapping there is no iovec to
    /// hand virglrenderer, so the resource cannot be created at all.
    #[error("failed to map {size} bytes of a blob resource's shared memory: {source}")]
    ShmMapFailed {
        /// The blob size that was requested (from the client's wire message).
        size: u64,
        /// The underlying OS error.
        source: std::io::Error,
    },

    /// Creating the pollable `eventfd` that `VCMD_SYNC_WAIT`'s reply hands the client failed.
    #[error("failed to create the eventfd for a VCMD_SYNC_WAIT reply")]
    EventFdFailed {
        /// The underlying OS error.
        source: std::io::Error,
    },

    /// The client asked for a `blob_mem` kind this server does not implement. virglrenderer's own
    /// vtest server answers `-EINVAL` for anything outside `{GUEST, HOST3D, HOST3D_GUEST}`, and so
    /// do we — notably `VIRGL_RENDERER_BLOB_MEM_GUEST_VRAM` (4), which has no meaning without a
    /// real virtio-gpu guest. Refused rather than guessed: picking a plausible-looking allocation
    /// strategy for a memory kind the client meant differently would corrupt its command ring.
    #[error(
        "VCMD_RESOURCE_CREATE_BLOB requested unsupported blob_mem {blob_mem} (only GUEST/1, HOST3D/2 and HOST3D_GUEST/3 are served)"
    )]
    UnsupportedBlobMem {
        /// The `VIRGL_RENDERER_BLOB_MEM_*` value the client asked for.
        blob_mem: u32,
    },

    /// `virgl_renderer_resource_export_blob` returned a non-zero code, so a host-allocated
    /// (`HOST3D`) blob produced no descriptor for the client to map.
    #[error(
        "virgl_renderer_resource_export_blob(resource_id={resource_id}) failed (rc={rc}: {reason})"
    )]
    BlobExportFailed {
        /// The resource that could not be exported.
        resource_id: u32,
        /// The raw return code.
        rc: c_int,
        /// Human-readable errno name for `rc`.
        reason: String,
    },

    /// `virgl_renderer_resource_export_blob` succeeded but reported an `fd_type` the client cannot
    /// use. The client's entire use of the descriptor is to `mmap` it, so only
    /// `VIRGL_RENDERER_BLOB_FD_TYPE_DMABUF` and `..._SHM` are acceptable; an `..._OPAQUE` handle
    /// would be accepted here and then fail in the client's `mmap`, far from the cause.
    /// virglrenderer's own vtest server makes exactly this check, and we mirror it.
    #[error(
        "virgl_renderer_resource_export_blob(resource_id={resource_id}) returned unusable fd_type {fd_type} (only DMABUF/1 and SHM/3 can be mapped by the client)"
    )]
    BlobExportUnusableFdType {
        /// The resource that was exported.
        resource_id: u32,
        /// The `VIRGL_RENDERER_BLOB_FD_TYPE_*` value that was reported.
        fd_type: u32,
    },

    /// A `RenderEngine` implementation returned a [`crate::BlobResource`] with no file descriptor,
    /// but the client is waiting for one. Unreachable for [`crate::VirglEngine`] (both of its blob
    /// paths always produce a descriptor); this variant is what turns a *different* engine
    /// implementation's mistake into a clear error instead of a client hang, since writing the
    /// in-band reply and then simply not sending an fd is indistinguishable, from the client's
    /// side, from the server having crashed.
    #[error(
        "the engine created blob resource {resource_id} without a file descriptor, but the vtest client requires one"
    )]
    BlobFdMissing {
        /// The resource whose descriptor is missing.
        resource_id: u32,
    },
}

/// Maps a C return code (an errno, possibly returned as a positive value) to a short readable
/// name, so `EngineError` messages say `EINVAL` rather than a bare `22`. Falls back to the numeric
/// code for anything `std` does not recognize.
///
/// # Inputs / outputs
/// - `rc`: the raw return code from a `virgl_renderer_*` call (treated by absolute value, since
///   these functions variously return positive or negative errnos).
/// - Returns an owned `String` like `"EINVAL"` or `"os error 22"`.
pub(crate) fn errno_name(rc: c_int) -> String {
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
