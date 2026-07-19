//! The Mesa **Venus vtest** wire protocol, and this repository's knowledge of Venus's command ring.
//!
//! # Why this crate exists separately from `rayland-engine`
//! `rayland-engine` FFI-links `libvirglrenderer` — a GPU stack. Rayland's **C** side (where the
//! *application* runs) speaks this protocol but must never need a GPU: C is by design the weak
//! machine, possibly a different CPU architecture, possibly headless. Splitting the protocol from
//! the GPU implementation is what lets `rayland-c` run there. `tests/no_gpu_linkage.rs` enforces it.
//!
//! The [`RenderEngine`] trait is the seam: `rayland-engine`'s `VirglEngine` implements it by
//! driving a real GPU, while (c)1's `RelayEngine` implements it by forwarding to another machine.
//!
//! # What lives here, and the one thing that does not
//! Everything in this crate is pure Rust over `std` plus four POSIX syscalls (`sendmsg`,
//! `memfd_create`/`mmap`, `eventfd`) — no GPU, no C library, no `build.rs`. What stays behind in
//! `rayland-engine` is precisely the code that *touches virglrenderer*: the `ffi` declarations and
//! the `VirglEngine` that drives them.
//!
//! # Layout
//! - [`RenderEngine`] — the trait the rest of Rayland programs against, and the seam a network can
//!   be slid into (see [`vtest::serve_vtest`], which drives the *trait*, never a concrete engine).
//! - [`VtestTransport`] — the transport seam the vtest protocol is served over (a byte stream
//!   **plus** SCM_RIGHTS fd passing; see its doc comment for why the fd half cannot be optional).
//! - [`EngineError`] — the typed error every failure maps into.
//! - [`BlobResource`] / [`EngineFrame`] — the two data types that cross the [`RenderEngine`]
//!   boundary. Both are plain data (ids, descriptors, pixel bytes) with no GPU types in them, which
//!   is what allows the trait to be implemented by something that is not a GPU at all.
//! - [`venus_ring`] — where the application's Vulkan commands *actually* travel. Read its module
//!   docs before assuming [`RenderEngine::submit`] sees real commands; it does not.

// The typed error every failure in this crate maps into. `pub` (it was private in `rayland-engine`,
// which re-exported only `EngineError`) because `rayland-engine`'s `virgl.rs` now lives across a
// crate boundary and still needs the module's `errno_name` helper for its C return codes.
pub mod error;
// The Unix-socket transport: the real `sendmsg`/SCM_RIGHTS `VtestTransport` impl, plus the POSIX
// primitives (memfd, mmap, eventfd) the vtest protocol needs and `std` does not expose. `pub` for
// the same reason as `error`: `VirglEngine`'s blob path maps shared memory with `ShmMapping` and
// `create_memfd`, and those calls are now cross-crate.
pub mod transport;
// Mesa's Venus command ring: the shared-memory layout a live client declares, a decoder for the
// Vulkan command stream it carries, and the live diagnostic that discovered both. This is where the
// application's Vulkan commands actually travel — *not* the vtest socket. Read its module docs
// before building anything that assumes `RenderEngine::submit` sees real commands; it does not.
pub mod venus_ring;
// The vtest wire-protocol server: parses what Mesa's Venus ICD emits and drives a `RenderEngine`.
pub mod vtest;

// Re-export the public API so consumers use `rayland_vtest::{EngineError, ...}`. The module split
// behind these names is an implementation detail; the crate's surface is flat.
pub use error::EngineError;

// The fd type `VtestTransport::send_fd` borrows. Re-exported through the trait's signature, so
// implementors need not guess which of `std`'s several fd types is meant.
use std::os::fd::BorrowedFd;
// A blob resource's client-facing fd is owned by whoever holds the `BlobResource` (Task 4a).
use std::os::fd::OwnedFd;

/// A blob resource that was just created, and the file descriptor the vtest client must receive
/// for it — the result of [`RenderEngine::create_blob_resource`].
///
/// # Why the fd is part of the result (C0 Task 4a)
/// Tasks 2/3 returned a bare `u32` resource id, which was not merely incomplete but *unusable*: a
/// blob is shared memory, and the client cannot use memory it has no descriptor for. It blocks in
/// `recvmsg` until the descriptor arrives. This type is what lets the engine express "here is the
/// resource, and here is the descriptor that makes it real to the client".
///
/// # Ownership contract (read this before touching the fd)
/// The `fd` is **owned by the receiver of this struct** and is closed when it drops. The intended
/// lifecycle, which mirrors virglrenderer's own vtest server exactly, is:
/// 1. write the in-band `[len=1][VCMD_RESOURCE_CREATE_BLOB][res_id]` reply,
/// 2. `send_fd(fd.as_fd())` — the kernel *duplicates* the descriptor into the client, so this
///    borrows rather than consumes it,
/// 3. drop it (the C server's `close(fd)` at the same point).
///
/// Dropping the fd does **not** release the resource, and does not unmap anything: for a
/// `GUEST`-family blob the pages stay alive because the *engine* holds a mapping of them for the
/// resource's lifetime (see `rayland_engine::VirglEngine::create_blob_resource`), and for a
/// `HOST3D` blob the memory belongs to the 3D driver. The resource itself is released only by
/// [`RenderEngine::unref_resource`] (or `Drop` of the engine). "Closing the file descriptor does
/// not unmap the region", as virglrenderer's own comment at this exact step puts it.
#[derive(Debug)]
pub struct BlobResource {
    /// The engine-assigned resource id, which the in-band reply reports to the client.
    pub resource_id: u32,
    /// The descriptor the client must receive over `SCM_RIGHTS` in order to `mmap` this blob's
    /// memory.
    ///
    /// `None` means "this blob has no client-visible descriptor". `rayland-engine`'s `VirglEngine`
    /// never produces that — both blob paths it serves always yield one — but the type admits it so
    /// that an engine implementation which genuinely cannot supply an fd is *forced* to say so, and
    /// the vtest layer can turn it into a typed [`EngineError::BlobFdMissing`] instead of leaving a
    /// live client hanging on a descriptor that is never coming.
    ///
    /// That escape hatch stops being hypothetical in (c)1: a descriptor names memory shared with
    /// *this kernel*, and a relay engine forwarding to another machine has no such thing to offer.
    pub fd: Option<OwnedFd>,
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

/// The transport the vtest wire protocol is served over: a byte stream **plus** the ability to pass
/// a file descriptor to the peer.
///
/// # Why this is a trait, and why `send_fd` is not optional (the Task 4a correction)
/// Tasks 2 and 3 served the protocol over a generic `S: Read + Write`, on the stated assumption
/// that SP2's QUIC transport could then "swap in unchanged". **That assumption was false**, and a
/// live Mesa Venus client is what proves it: two of the protocol's replies —
/// `VCMD_RESOURCE_CREATE_BLOB` and `VCMD_SYNC_WAIT` — must hand the client a real file descriptor
/// over an `SCM_RIGHTS` ancillary message, and the client blocks in `recvmsg` forever without it. A
/// byte stream cannot carry a descriptor. The old bound could therefore never have served a real
/// client at all; it only ever satisfied unit tests over an in-memory `Cursor`.
///
/// Making the fd a **required** trait method is the deliberate design choice. QUIC has no fd
/// passing and no shared memory, so a future QUIC transport cannot implement this by delegation —
/// it must confront the question head-on (what the descriptor *means* on the far side of a network,
/// and how the memory it names gets there), which is exactly the (c)1/(c)2 design work. A trait
/// with a mandatory `send_fd` forces that confrontation at compile time. An optional/defaulted
/// method, or a `Read + Write` bound with the fd bolted on elsewhere, would let it be silently
/// skipped again — reproducing the very bug this task exists to fix, just later and further from
/// its cause.
pub trait VtestTransport: std::io::Read + std::io::Write {
    /// Send `fd` to the client as `SCM_RIGHTS` ancillary data, **after** the in-band reply this fd
    /// belongs to has already been written.
    ///
    /// # The ordering is part of the contract, not an implementation detail
    /// virglrenderer's C server writes the in-band reply and *then* calls `vtest_send_fd`, and
    /// Mesa's client reads them in exactly that order (a plain `read()` of the reply, then a
    /// `recvmsg` for the descriptor). Sending the fd first, or merging it into the reply's bytes,
    /// mis-frames the protocol for the client. Callers must therefore write the reply first; this
    /// method never writes in-band bytes of its own beyond the single carrier byte the kernel
    /// requires (see `transport.rs`'s module docs).
    ///
    /// # Inputs / outputs
    /// - `fd`: the descriptor to duplicate into the client's process. **Borrowed, not consumed:**
    ///   the kernel duplicates it, so the caller still owns its own copy and remains responsible
    ///   for closing it — normally immediately after this returns, as the C server does.
    /// - Returns `Ok(())` once the kernel has accepted the message.
    ///
    /// # Failure modes
    /// Returns an [`EngineError`] if the underlying send fails (peer gone, or — for a transport
    /// that has no fd-passing mechanism at all — a clear, typed refusal rather than a silent no-op
    /// that would hang the client).
    fn send_fd(&mut self, fd: BorrowedFd<'_>) -> Result<(), EngineError>;
}

/// The abstraction the rest of Rayland renders through, so the borrowed C engine can later be
/// swapped or Rustified without touching callers (a locked design decision).
///
/// C0 Task 1 pinned the two load-bearing replay methods (`create_venus_context`, `submit`); Task 2
/// added `venus_capset` for the vtest handshake; Task 3 added the resource-creation and
/// fence-waited pixel-readback methods, so a replayed venus stream can be read back to CPU pixels.
///
/// # Why this trait is (c)1's foundation
/// [`vtest::serve_vtest`] drives *this trait*, never a concrete engine. The locked decision that the
/// boundary stay clean enough to swap the engine was written with "Rustify it later" in mind; (c)1
/// cashes it in for something nobody anticipated — the implementation being swapped in is a
/// **network**. A `RelayEngine` that forwards these calls to another machine is a `RenderEngine`,
/// and the vtest server cannot tell the difference. Keep GPU concepts out of this signature.
pub trait RenderEngine {
    /// Create a Venus capset rendering context with the caller-chosen `ctx_id`.
    ///
    /// Later tasks map each connected Venus client (over the vtest wire protocol) to a context id.
    /// Returns an error if the engine rejects the context (see [`EngineError`]).
    fn create_venus_context(&mut self, ctx_id: u32) -> Result<(), EngineError>;

    /// Submit a raw command buffer to a context for execution on the GPU.
    ///
    /// `cmd` is the byte stream to replay; its length must be a multiple of 4 (virglrenderer
    /// counts commands in 4-byte words). Returns an error if the buffer is malformed or the
    /// renderer rejects it (see [`EngineError`]).
    fn submit(&mut self, ctx_id: u32, cmd: &[u8]) -> Result<(), EngineError>;

    /// Return the raw Venus capability-set blob the vtest handshake must hand back to the client.
    ///
    /// Mesa's Venus ICD, during connection setup, sends `VCMD_GET_CAPSET` and refuses to proceed
    /// until it receives a valid Venus capset (a `struct virgl_renderer_capset_venus`) carrying the
    /// wire-format and protocol-spec versions it will negotiate against. The vtest server
    /// ([`vtest::serve_vtest`]) routes that request here so the answer comes from the *real*
    /// renderer rather than a guess.
    ///
    /// # Inputs / outputs
    /// - `version`: the capset version the client asked for (`VCMD_GET_CAPSET`'s version field).
    /// - Returns the capset bytes (length is a multiple of 4, as the wire framing requires), or an
    ///   [`EngineError`] if the renderer reports no Venus capset (e.g. no GPU / Venus unsupported).
    fn venus_capset(&mut self, version: u32) -> Result<Vec<u8>, EngineError>;

    /// Create a *classic* (non-blob) 2D resource — a GPU-backed texture with a real
    /// format/width/height/row-stride — attach it to `ctx_id`, and track it so [`Self::read_back`]
    /// can later read pixels out of it.
    ///
    /// This is the resource kind [`Self::read_back`] can actually read back (see that method's
    /// doc comment for the empirically-discovered reason blob resources — [`Self::
    /// create_blob_resource`]'s kind, the one Venus's real wire protocol allocates — cannot).
    ///
    /// # Inputs / outputs
    /// - `ctx_id`: the context this resource is created for (must already exist).
    /// - `width`, `height`: resource dimensions in pixels.
    /// - `format`: a `VIRGL_FORMAT_*` code (e.g. `1` = `VIRGL_FORMAT_B8G8R8A8_UNORM`).
    /// - Returns the engine-assigned resource id on success, or an [`EngineError`].
    fn create_resource(
        &mut self,
        ctx_id: u32,
        width: u32,
        height: u32,
        format: u32,
    ) -> Result<u32, EngineError>;

    /// Create a *blob* resource — host/guest-shareable memory, the resource kind Venus's real wire
    /// protocol (`VCMD_RESOURCE_CREATE_BLOB`) allocates for both its command ring and its device
    /// memory — attach it to `ctx_id`, track it, and yield **the descriptor the client must
    /// receive** alongside the resource id.
    ///
    /// # Why this returns an fd (C0 Task 4a; this is the whole point)
    /// A blob is not merely a host-side allocation the client is told the id of: it is **memory the
    /// client and the host genuinely share**. The client `mmap`s the returned descriptor and writes
    /// its Venus command stream directly into those pages, which the host then reads. Returning a
    /// bare `u32` (as Tasks 2/3 did) cannot express that, which is exactly why a live Mesa client
    /// blocked forever on the first blob: it was waiting on a descriptor the engine had no way to
    /// produce. See [`BlobResource`] for the ownership contract of the returned fd, and
    /// [`VtestTransport::send_fd`] for how it reaches the client.
    ///
    /// # Inputs / outputs
    /// - `ctx_id`: the context this resource is created for (must already exist).
    /// - `blob_mem`: `VIRGL_RENDERER_BLOB_MEM_*` from the wire message. Which memory *kind* is
    ///   being asked for decides where the shared pages come from — see
    ///   `rayland_engine::VirglEngine::create_blob_resource`'s doc comment for the two supported
    ///   paths and their very different mechanics.
    /// - `blob_flags`: `VIRGL_RENDERER_BLOB_FLAG_*` from the wire message.
    /// - `blob_id`: the client-chosen blob id from the wire message.
    /// - `size`: requested size in bytes.
    /// - Returns a [`BlobResource`] (id + the client's fd) on success, or an [`EngineError`].
    fn create_blob_resource(
        &mut self,
        ctx_id: u32,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
    ) -> Result<BlobResource, EngineError>;

    /// Release a resource created by [`Self::create_resource`] or [`Self::create_blob_resource`].
    ///
    /// Mirrors the wire protocol's `VCMD_RESOURCE_UNREF`, which has no reply and cannot fail from
    /// the caller's perspective; an id this engine never created (or already released) is silently
    /// ignored rather than erroring.
    fn unref_resource(&mut self, resource_id: u32);

    /// Fence-wait for every command submitted to a resource's context to retire, then read the
    /// resource's pixels back to CPU memory as a tightly-packed [`EngineFrame`].
    ///
    /// The fence-wait is the correctness point this method exists for: `submit` only proves the
    /// GPU *accepted* a command stream, not that it *finished* it, so reading pixels without
    /// waiting could return a partially-rendered or stale frame. See
    /// `rayland_engine::VirglEngine::read_back`'s doc comment for the full mechanism (how
    /// completion is tracked), the stride-honoring discipline (never assume a resource's row stride
    /// equals `width * bytes_per_pixel`), and the documented limitation that only resources created
    /// via [`Self::create_resource`] — not [`Self::create_blob_resource`] — can be read back with
    /// this virglrenderer version.
    ///
    /// # Inputs / outputs
    /// - `resource_id`: a resource id previously returned by [`Self::create_resource`].
    /// - Returns a tightly-packed [`EngineFrame`] on success, or an [`EngineError`].
    fn read_back(&mut self, resource_id: u32) -> Result<EngineFrame, EngineError>;

    /// Block until every command already submitted on `(ctx_id, ring_idx)` has **retired on the
    /// GPU**, so that whatever those commands wrote is visible in memory to whoever looks next.
    ///
    /// # Why this exists on the trait, and the bug that put it here
    /// [`Self::read_back`] already states the principle this method generalises: *"`submit` only
    /// proves the GPU accepted a command stream, not that it finished it, so reading pixels without
    /// waiting could return a partially-rendered or stale frame."* `read_back` obeys that by
    /// fence-waiting internally — but `read_back` is C0's offscreen path, and **(c)1 does not use
    /// it**. (c)1's return path is `rayland-s`'s poll loop, which discovers what S's GPU wrote by
    /// *comparing bytes against a baseline* and had no way to ask this question at all.
    ///
    /// It therefore inferred completion from a `memcmp` — and a `memcmp` answers *"did these bytes
    /// change?"*, never *"has the GPU finished?"*. Those coincide until they don't: (c)1 Task 9
    /// measured a 120-frame workload receiving **the previous frame, whole and intact, in 22 of 120
    /// frames, and a torn mix in 16 more** — silently, with the application exiting 0
    /// (`docs/c1-the-network.md` §3.1).
    ///
    /// # ⚠️ This is NOT sufficient, and the measurement says so
    /// Calling this before the diff **does not fix that defect**. Measured on `rayland-s`'s poll
    /// loop: the barrier is genuinely doing work — 684 calls averaging **1.1 ms of real waiting** in
    /// a 120-frame run, so it is not a no-op — and the run still delivered **24 of 120 frames wrong**
    /// (18 stale, 6 torn), indistinguishable from no barrier at all (the unfixed range is 3–39).
    ///
    /// The inference to draw is about `VirglEngine`, not about this trait: **a virglrenderer context
    /// fence does not order against the work Venus's own ring thread dispatches.** `read_back` gets a
    /// correct frame from the same primitive because it fences resources made by
    /// [`Self::create_resource`] — C0's offscreen path — never the application's Venus queue. So this
    /// method asks a real question and gets a real answer, and it is **still the wrong question** for
    /// the caller that needs it. Whoever resumes this: start there, and do not assume the barrier is
    /// the missing piece merely because it is the obvious one. It was tried.
    ///
    /// # Why a barrier rather than a fence handle
    /// The caller has no fence to name. (c)1's S never submits the application's work — Venus's own
    /// ring thread inside virglrenderer consumes the ring and dispatches, so no code here observes a
    /// submission it could attach an id to. What the caller *can* say is "everything up to now", and
    /// per-context fences signal in creation order within a context, so a fence created now retires
    /// only after all earlier work on that ring. That makes "wait for a fence created now" exactly
    /// the barrier the caller needs and the only one it can express.
    ///
    /// # Inputs / outputs
    /// - `ctx_id`: the context whose work must retire — the one [`Self::create_venus_context`] made.
    /// - `ring_idx`: the ring within that context. (c)1 passes `0`, which is legitimate rather than
    ///   lucky only because spec §6's crutch table sets `VN_PERF=no_multi_ring`; if that crutch is
    ///   ever bought back, this argument must start carrying a real ring index.
    /// - Returns `Ok(())` once the work has retired, or an [`EngineError`] if the fence could not be
    ///   created or did not retire within the implementation's timeout.
    ///
    /// # Failure modes, and why a timeout is an error rather than a shrug
    /// A fence that never retires means the GPU is wedged or the context is gone. Returning `Ok(())`
    /// on timeout would resurrect exactly the defect this method exists to kill — the caller would
    /// proceed to ship whatever bytes happen to be in memory — so an implementation that cannot
    /// prove retirement must say so.
    ///
    /// # Default: a no-op, and when that is honest
    /// An engine that does not drive a real GPU asynchronously has nothing to retire: whatever its
    /// `submit` did, it did before returning. The default therefore returns `Ok(())`, which is the
    /// truth for the in-process mocks the tests use. **A real GPU engine must override it** — and
    /// `VirglEngine` does.
    fn wait_for_work_retired(&mut self, ctx_id: u32, ring_idx: u32) -> Result<(), EngineError> {
        // Named with leading underscores in the default body: an engine with no asynchronous GPU
        // behind it has no work in flight to wait for, so it needs neither argument.
        let (_ctx_id, _ring_idx) = (ctx_id, ring_idx);
        Ok(())
    }
}
