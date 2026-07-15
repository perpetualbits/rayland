//! Enforces the structural constraint that makes Rayland's whole premise true: **`rayland-c` must
//! not link a GPU stack.**
//!
//! # Why this matters more here than anywhere else in the workspace
//! C is by design the *weak* machine. The application runs there; the GPU does not. CLAUDE.md names
//! it explicitly: possibly headless, possibly a different CPU architecture, **eventually RISC-V**.
//! `rayland-c` is the binary that actually has to run on that machine. If it links
//! `libvirglrenderer`, then "C needs no GPU" is quietly false — not as a design aspiration that
//! slipped, but as a claim the project makes and cannot keep.
//!
//! # Why the guard is on the *binary*, not on each library
//! `rayland-vtest` has its own copy of this test (`crates/rayland-vtest/tests/no_gpu_linkage.rs`),
//! and it is still worth having — it protects that crate for its own sake. But guarding the binary
//! is **strictly stronger**: `cargo tree -p rayland-c` covers `rayland-relay`, `rayland-vtest` and
//! everything either of them pulls in, transitively. A GPU dependency added to *any* of them, at
//! any depth, fails here. That is the whole dependency closure that would actually be installed on
//! machine C, which is the thing the claim is about.
//!
//! # Being honest about this test's teeth
//! For an ordinary `[dependencies]` edge this test is largely redundant, and pretending otherwise
//! would be the same vacuity it exists to prevent. `rayland-engine` already depends on
//! `rayland-vtest`, so cargo's own cycle check would refuse to build the workspace the moment anyone
//! added a normal dependency from `rayland-vtest` back to `rayland-engine` — with or without this
//! test. `rayland-c` is a leaf that nothing depends on, so a direct `rayland-c → rayland-engine`
//! edge is not a cycle at all and *would* build; that one is caught here and nowhere else.
//!
//! The guard's real value is three things cargo does not catch:
//! - **A direct dependency from this binary.** Nothing depends on `rayland-c`, so there is no cycle
//!   to protect it. Adding `rayland-engine` to this crate's `[dependencies]` compiles cleanly on a
//!   developer box that has virglrenderer installed, and fails only on the machine that matters.
//! - **Dev-dependency cycles, which cargo permits.** A `[dev-dependencies]` edge is not part of the
//!   graph cargo checks for cycles, so `rayland-vtest → rayland-engine` as a dev-dependency would be
//!   accepted — and would drag `libvirglrenderer` onto every machine that runs the test suite.
//! - **Future restructuring.** Cargo's cycle protection exists only *because* `rayland-engine`
//!   currently depends on `rayland-vtest`. Should that ever change, cargo would stop objecting and
//!   this would become the only thing standing between C and a linked GPU stack.
//!
//! # What this test does *not* guard
//! It greps `cargo tree` for the literal string `rayland-engine`. It has nothing to say about a new,
//! direct dependency on `ash`, a Vulkan-loader `*-sys` crate, or any other GPU-linking crate added
//! straight to a manifest — those would sail through unnoticed. `rayland-engine` is simply the only
//! GPU-linking crate in this workspace today, so it is the only needle this test can meaningfully
//! look for. The claim made here is exactly the claim the assertion checks, and no more.

/// The dependency tree of the `rayland-c` **binary** must not contain `rayland-engine` (which
/// FFI-links `libvirglrenderer`), nor any crate that does.
///
/// # How it works
/// `cargo tree -p rayland-c` prints the crate's full transitive dependency closure — the set that
/// would actually be built and installed on machine C. We assert `rayland-engine` is absent.
///
/// The check fails loudly if `cargo tree` itself could not run. That is not defensive noise: an
/// empty tree contains no needle, so a test that only asserted `!tree.contains(...)` would **pass
/// vacuously** the moment the command broke, and would then guarantee nothing while continuing to
/// look green. This repository has already shipped one linkage test with exactly that flaw.
#[test]
fn rayland_c_does_not_depend_on_the_gpu_engine() {
    // `env!("CARGO")` is the exact cargo binary running this test, so the check uses the same
    // toolchain as the build rather than whatever `cargo` happens to be on `$PATH`.
    let out = std::process::Command::new(env!("CARGO"))
        .args(["tree", "-p", "rayland-c", "--prefix", "none"])
        // Run inside this crate's directory so cargo resolves the enclosing workspace.
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree runs");
    let tree = String::from_utf8_lossy(&out.stdout);
    // Without this, a broken `cargo tree` would leave the assertion below trivially satisfied.
    assert!(
        out.status.success() && !tree.trim().is_empty(),
        "cargo tree -p rayland-c failed, so the no-GPU guarantee is unverified (status: {}). \
         stderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !tree.contains("rayland-engine"),
        "rayland-c must not depend on rayland-engine (it FFI-links libvirglrenderer). This binary \
         runs on machine C, which by design has no GPU stack at all — eventually a RISC-V box. If \
         it links one, the project's central claim is quietly false. cargo tree said:\n{tree}"
    );
}
