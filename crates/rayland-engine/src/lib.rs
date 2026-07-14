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
//! - [`VirglEngine`] — the concrete virglrenderer-backed implementation.
//! - [`EngineError`] — the typed error every C return code maps into.
//! - [`virgl_available`] — a cheap probe for gating GPU tests / runtime capability checks.

// The raw, hand-written C FFI surface. All `unsafe extern "C"` declarations and callbacks live here.
mod ffi;
// The engine, error type, and availability probe.
mod virgl;
// The vtest wire-protocol server: parses what Mesa's Venus ICD emits and drives a `RenderEngine`.
pub mod vtest;

// Re-export the public API so consumers use `rayland_engine::{VirglEngine, EngineError, ...}`.
pub use virgl::{EngineError, EngineFrame, VirglEngine, virgl_available};

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

    /// Create a *blob* resource — opaque host/guest-shareable memory, the resource kind Venus's
    /// real wire protocol (`VCMD_RESOURCE_CREATE_BLOB`) allocates for device memory — attach it to
    /// `ctx_id`, and track it.
    ///
    /// The vtest server ([`vtest::serve_vtest`]) routes `VCMD_RESOURCE_CREATE_BLOB` here, so the
    /// resource id it reports in [`vtest::VtestOutcome::rendered_resource_id`] names a resource
    /// that genuinely exists in the engine (Task 3's Step 2 requirement) — not a vtest-local
    /// counter. Turning this resource's bytes into an [`EngineFrame`] is Task 4's concern; see
    /// [`Self::read_back`]'s doc comment for exactly what is and is not possible here yet.
    ///
    /// # Inputs / outputs
    /// - `ctx_id`: the context this resource is created for (must already exist).
    /// - `blob_mem`: `VIRGL_RENDERER_BLOB_MEM_*` from the wire message.
    /// - `blob_flags`: `VIRGL_RENDERER_BLOB_FLAG_*` from the wire message.
    /// - `blob_id`: the client-chosen blob id from the wire message.
    /// - `size`: requested size in bytes.
    /// - Returns the engine-assigned resource id on success, or an [`EngineError`].
    fn create_blob_resource(
        &mut self,
        ctx_id: u32,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
    ) -> Result<u32, EngineError>;

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
