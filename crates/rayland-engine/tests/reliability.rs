//! The C0 reliability spike — the load-bearing test of Task 1.
//!
//! These tests answer the open question the feasibility spike left: is repeated Venus
//! init/replay/teardown reliable when we drive `libvirglrenderer` directly (rather than through the
//! flaky `virgl_test_server` harness)? They do so by hammering the real lifecycle many times and
//! asserting every iteration succeeds.
//!
//! # Skip, don't fail, without a GPU
//! virglrenderer + Venus are Linux DRM/GPU features. On a host without a usable render node (a CI
//! runner, say), [`virgl_available`] returns `false` and each test prints a SKIP line and returns.
//! This keeps CI green while still exercising the full lifecycle on any host that has the GPU.
//!
//! Run on a GPU host with: `cargo test -p rayland-engine -- --nocapture`.

// The engine, trait, and probe under test.
use rayland_engine::{RenderEngine, VirglEngine, virgl_available};
// Path to the render node.
use std::path::Path;
// Serialize the GPU tests against each other (see `GPU_TEST_LOCK`).
use std::sync::Mutex;

/// Serializes the GPU-touching tests. `libvirglrenderer` is a process-global singleton, and so is
/// the engine's single-instance guard, so two engine-constructing tests running on separate
/// threads (cargo's default) would contend — one would see `AlreadyActive`. This lock makes the
/// tests run one at a time regardless of `--test-threads`, so the plain
/// `cargo test -p rayland-engine` invocation is deterministic. It is a *test-harness* concern only;
/// the singleton guard it works around is a real, desired production invariant.
static GPU_TEST_LOCK: Mutex<()> = Mutex::new(());

/// The DRM render node the C0 spike used. Rayland's target S machine has a real GPU here; a
/// headless CI runner does not, which is why the tests gate on [`virgl_available`].
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// Number of full new→context→drop cycles the reliability test performs. The feasibility spike
/// flaked on the *second* venus init; 20 is the brief's floor, and we run more to be convincing.
const RELIABILITY_ITERATIONS: u32 = 25;

/// Number of simultaneously-live Venus contexts the multi-context test creates within one engine.
const SIMULTANEOUS_CONTEXTS: u32 = 25;

/// Repeated full-lifecycle reliability: `new` → `create_venus_context` → `drop`, ≥20 times, each
/// iteration asserted to succeed. This is the exact pattern that flaked in the throwaway harness;
/// passing it here is the C0 gate that the flakiness was the harness, not the library.
///
/// A failure on any iteration (with the diagnostic it prints) is the signal to reconsider the
/// engine — but on a GPU host this test passes every iteration.
#[test]
fn repeated_init_context_teardown_is_reliable() {
    // Serialize against the other GPU tests (poison is irrelevant — we only need mutual exclusion).
    let _serialize = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Gate: skip cleanly where no usable Venus-capable render node exists.
    let node = Path::new(RENDER_NODE);
    if !virgl_available(node) {
        eprintln!(
            "SKIP repeated_init_context_teardown_is_reliable: no usable Venus render node at {RENDER_NODE}"
        );
        return;
    }

    // Hammer the lifecycle. Each iteration constructs a fresh engine (a fresh virgl_renderer_init +
    // forked render server), creates a Venus context, then drops the engine (context destroy +
    // virgl_renderer_cleanup). Any failure aborts with the iteration number for diagnosis.
    for iteration in 0..RELIABILITY_ITERATIONS {
        // (1) Bring up the renderer on the real GPU.
        let mut engine = VirglEngine::new(node)
            .unwrap_or_else(|e| panic!("iteration {iteration}: VirglEngine::new failed: {e}"));

        // (2) Create a Venus context. Context id 1 is reused each iteration — safe, because the
        // previous engine's `Drop` fully tore down the global renderer before this one initialized.
        engine
            .create_venus_context(1)
            .unwrap_or_else(|e| panic!("iteration {iteration}: create_venus_context failed: {e}"));

        // (3) Drop the engine explicitly to make the teardown point obvious in the test's flow.
        drop(engine);

        // Progress trace (visible under --nocapture) so a mid-run flake is easy to locate.
        eprintln!("iteration {iteration} OK");
    }

    // Reaching here means every cycle succeeded: the library is reliable across repeated
    // init/teardown.
    eprintln!("RELIABLE: {RELIABILITY_ITERATIONS} init/context/teardown cycles all succeeded");
}

/// Many simultaneously-live Venus contexts within a single engine: create N contexts, all held
/// live at once, then let one `Drop` destroy them all. Exercises the other axis of the lifecycle
/// (context churn within one renderer, not renderer churn).
#[test]
fn many_contexts_within_one_engine() {
    // Serialize against the other GPU tests.
    let _serialize = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Gate: skip cleanly without a usable Venus render node.
    let node = Path::new(RENDER_NODE);
    if !virgl_available(node) {
        eprintln!(
            "SKIP many_contexts_within_one_engine: no usable Venus render node at {RENDER_NODE}"
        );
        return;
    }

    // One engine for the whole test.
    let mut engine = VirglEngine::new(node).expect("VirglEngine::new should succeed on a GPU host");

    // Create N distinct contexts, all left live simultaneously (ids 1..=N). Each must succeed.
    for ctx_id in 1..=SIMULTANEOUS_CONTEXTS {
        engine
            .create_venus_context(ctx_id)
            .unwrap_or_else(|e| panic!("create_venus_context({ctx_id}) failed: {e}"));
    }

    // Drop destroys all N contexts, then cleans up the renderer. Reaching here (no panic) means N
    // simultaneous Venus contexts coexist and tear down cleanly.
    drop(engine);
    eprintln!(
        "RELIABLE: {SIMULTANEOUS_CONTEXTS} simultaneous Venus contexts created and torn down"
    );
}

/// The single-instance invariant: while one `VirglEngine` is live, a second `new` must fail with
/// `AlreadyActive` (virglrenderer is a process-global singleton) rather than corrupt global state.
/// This runs even without a GPU — but only meaningfully once the first engine constructs, so it
/// also gates on availability.
#[test]
fn second_engine_is_rejected_while_one_is_live() {
    // Serialize against the other GPU tests, so no *other* test's live engine is mistaken for the
    // second engine this test constructs.
    let _serialize = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Gate: needs a real renderer to construct the first engine.
    let node = Path::new(RENDER_NODE);
    if !virgl_available(node) {
        eprintln!(
            "SKIP second_engine_is_rejected_while_one_is_live: no usable Venus render node at {RENDER_NODE}"
        );
        return;
    }

    // First engine holds the global single-instance lock.
    let _engine = VirglEngine::new(node).expect("first VirglEngine::new should succeed");

    // A second construction must be rejected while the first is live.
    match VirglEngine::new(node) {
        // Correct: the singleton guard rejected the second engine.
        Err(rayland_engine::EngineError::AlreadyActive) => {
            eprintln!("OK: second engine correctly rejected with AlreadyActive");
        }
        // Any other outcome is a bug in the single-instance guard.
        Err(other) => panic!("second engine failed with the wrong error: {other}"),
        Ok(_) => panic!(
            "second engine was allowed while one was already live — singleton guard is broken"
        ),
    }
}
