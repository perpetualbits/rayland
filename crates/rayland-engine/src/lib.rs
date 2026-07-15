//! `rayland-engine` — the S-side ("server") render engine for Rayland.
//!
//! Rayland renders an application's GPU work on a *different* machine (S) from the one running the
//! application (C). On S, an unmodified stream of Vulkan commands — captured on C by Mesa's Venus
//! ICD — must be **replayed** on the real GPU. Rather than reinvent a Vulkan capture/replay engine,
//! Rayland reuses the one the virtual-machine world already hardened for exactly this threat model
//! (an untrusted party driving the host GPU): Mesa's **Venus** on the capture side and
//! **virglrenderer** on the replay side. This crate is the replay side: it FFI-embeds
//! `libvirglrenderer` and drives a *Venus capset* context on S's GPU, behind a clean Rust trait so
//! the rest of Rayland never touches the C library.
//!
//! # What this crate proves (C0 Task 1, the de-risk)
//! A feasibility spike showed Venus-without-a-VM is real but hit flakiness in the throwaway
//! `virgl_test_server` harness: after one success, repeated venus init/teardown failed with
//! `INITIALIZATION_FAILED`. The open question was whether that flakiness lived in
//! `libvirglrenderer` itself or only in the test harness. **This crate answers it: the library is
//! reliable.** With the correct init flags and fd lifecycle, ≥50 init→context→teardown cycles and
//! dozens of simultaneous contexts succeed with no failures and no orphaned processes. The two
//! ingredients the harness got wrong:
//! - the **`VIRGL_RENDERER_RENDER_SERVER`** init flag is *required* for Venus context creation
//!   (without it, context creation returns `EINVAL`), and
//! - the **`get_drm_fd` callback must open a fresh render-node fd on every call**, because
//!   virglrenderer takes ownership of the fd and closes it.
//!
//! See `ffi.rs` for the pinned C ABI and `virgl.rs` for the lifecycle.
//!
//! # Layout
//! - [`RenderEngine`] — the trait the rest of Rayland programs against.
//! - [`VtestTransport`] — the transport seam the vtest protocol is served over (a byte stream
//!   **plus** SCM_RIGHTS fd passing; see its doc comment for why the fd half cannot be optional).
//! - [`VirglEngine`] — the concrete virglrenderer-backed implementation.
//! - [`EngineError`] — the typed error every C return code maps into.
//! - [`virgl_available`] — a cheap probe for gating GPU tests / runtime capability checks.

// The raw, hand-written C FFI surface. All `unsafe extern "C"` declarations and callbacks live here.
mod ffi;
// The typed error every failure in this crate maps into. Split out of `virgl.rs` in Task 4a: the
// enum had grown past 300 lines of (deliberately detailed) documentation, which is a file of its
// own by this repository's "small, focused files" rule.
mod error;
// The Unix-socket transport: the real `sendmsg`/SCM_RIGHTS `VtestTransport` impl, plus the POSIX
// primitives (memfd, mmap, eventfd) the vtest protocol needs and `std` does not expose.
mod transport;
// The engine, error type, and availability probe.
mod virgl;
// The vtest wire-protocol server: parses what Mesa's Venus ICD emits and drives a `RenderEngine`.
pub mod vtest;

// Re-export the public API so consumers use `rayland_engine::{VirglEngine, EngineError, ...}`.
// The module split behind these names is an implementation detail; the crate's surface is flat.
pub use error::EngineError;
pub use virgl::{BlobResource, EngineFrame, VirglEngine, virgl_available};

// The fd type `VtestTransport::send_fd` borrows. Re-exported through the trait's signature, so
// implementors need not guess which of `std`'s several fd types is meant.
use std::os::fd::BorrowedFd;

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
/// Task 1 pinned the two load-bearing replay methods (`create_venus_context`, `submit`); Task 2
/// added `venus_capset` for the vtest handshake; Task 3 adds the resource-creation and
/// fence-waited pixel-readback methods, so a replayed venus stream can be read back to CPU pixels.
pub trait RenderEngine {
    /// Create a Venus capset rendering context with the caller-chosen `ctx_id`.
    ///
    /// Later tasks map each connected Venus client (over the vtest wire protocol) to a context id.
    /// Returns an error if virglrenderer rejects the context (see [`EngineError`]).
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
    /// # Why this returns an fd (Task 4a; this is the whole point)
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
    ///   being asked for decides where the shared pages come from — see `VirglEngine::
    ///   create_blob_resource`'s doc comment for the two supported paths and their very different
    ///   mechanics.
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
    /// waiting could return a partially-rendered or stale frame. See `VirglEngine::read_back`'s
    /// doc comment for the full mechanism (how completion is tracked), the stride-honoring
    /// discipline (never assume a resource's row stride equals `width * bytes_per_pixel`), and the
    /// documented limitation that only resources created via [`Self::create_resource`] — not
    /// [`Self::create_blob_resource`] — can be read back with this virglrenderer version.
    ///
    /// # Inputs / outputs
    /// - `resource_id`: a resource id previously returned by [`Self::create_resource`].
    /// - Returns a tightly-packed [`EngineFrame`] on success, or an [`EngineError`].
    fn read_back(&mut self, resource_id: u32) -> Result<EngineFrame, EngineError>;
}
