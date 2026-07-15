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
//! # What is here, and what moved out ((c)1 Task 1)
//! This crate was the whole of C0's engine: the protocol *and* the GPU. (c)1 split the protocol out
//! into [`rayland_vtest`], leaving only what genuinely touches `libvirglrenderer` here. The reason
//! is structural, not tidiness: Rayland's **C** side (where the application runs) must speak the
//! vtest protocol while linking **no GPU code at all** — C is by design the weak machine, headless
//! and possibly a different CPU architecture. C depends on `rayland-vtest`; it must never depend on
//! this crate. `rayland-vtest`'s `tests/no_gpu_linkage.rs` enforces that direction mechanically.
//!
//! Everything `rayland-vtest` owns is re-exported below, so this crate's public paths are unchanged
//! from C0.
//!
//! # Layout
//! - [`VirglEngine`] — the concrete virglrenderer-backed implementation. **This, plus `ffi`, is all
//!   that is really left in this crate**; every other name below is a re-export.
//! - [`virgl_available`] — a cheap probe for gating GPU tests / runtime capability checks.
//! - [`RenderEngine`] — the trait the rest of Rayland programs against (from [`rayland_vtest`]).
//! - [`VtestTransport`] — the transport seam the vtest protocol is served over (a byte stream
//!   **plus** SCM_RIGHTS fd passing; see its doc comment for why the fd half cannot be optional).
//! - [`EngineError`] — the typed error every C return code maps into.

// The raw, hand-written C FFI surface. All `unsafe extern "C"` declarations and callbacks live here.
mod ffi;
// The engine and its availability probe: the only module left in this crate that drives
// virglrenderer, which is precisely why it is the only one that could not move to `rayland-vtest`.
mod virgl;

// `rayland-engine` was the whole of C0's engine; (c)1 split the protocol out into `rayland-vtest`
// so the C side can link this crate's *absence*. Re-exported so existing dependents (this crate's
// own tests, `examples/vtest_serve.rs`) keep their import paths.
pub use rayland_vtest::{
    BlobResource, EngineError, EngineFrame, RenderEngine, VtestTransport, error, transport,
    venus_ring, vtest,
};

// The concrete engine this crate exists to provide. Not a re-export: this is the GPU.
pub use virgl::{VirglEngine, virgl_available};
