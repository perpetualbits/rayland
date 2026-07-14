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

// Re-export the public API so consumers use `rayland_engine::{VirglEngine, EngineError, ...}`.
pub use virgl::{EngineError, VirglEngine, virgl_available};

/// The abstraction the rest of Rayland renders through, so the borrowed C engine can later be
/// swapped or Rustified without touching callers (a locked design decision).
///
/// Task 1 pins the two load-bearing methods; the resource-creation and pixel-readback methods are
/// finalized in Task 3 once the vtest/venus data path is built.
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
}
