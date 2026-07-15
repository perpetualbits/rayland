//! Enforces (c)1's load-bearing structural constraint: `rayland-vtest` must never pull in a GPU
//! stack. The C side of Rayland depends on this crate, and C is by design the *weak* machine — a
//! headless box, eventually a RISC-V one, with no GPU libraries at all. If this crate ever links
//! `libvirglrenderer`, the project's central claim ("C needs no GPU") becomes quietly false.
//!
//! This is a test rather than a code review note because the failure is silent: adding
//! `rayland-engine` to `[dependencies]` compiles fine on a developer box that happens to have
//! virglrenderer installed, and only fails much later on the machine that matters.

/// The dependency tree of `rayland-vtest` must not contain `rayland-engine` (which FFI-links
/// `libvirglrenderer`), nor any crate that does.
///
/// # How it works
/// `cargo tree -p rayland-vtest` prints this crate's transitive dependencies. We assert
/// `rayland-engine` is absent. Failure mode: someone adds a convenience dependency and does not
/// realize it drags a GPU stack onto a machine that has none.
///
/// # Why the assertion is on the *whole* tree, not just direct dependencies
/// The dangerous case is the indirect one. A direct `rayland-engine` dependency is visible in this
/// crate's manifest and would be caught in review; a dependency that *itself* pulls in
/// `rayland-engine` several levels down is not visible anywhere a human reliably looks. `cargo
/// tree`'s transitive output is what makes the check cover both.
#[test]
fn rayland_vtest_does_not_depend_on_the_gpu_engine() {
    // `env!("CARGO")` is the exact cargo binary running this test, so the check uses the same
    // toolchain as the build rather than whatever `cargo` happens to be on `$PATH`.
    let out = std::process::Command::new(env!("CARGO"))
        .args(["tree", "-p", "rayland-vtest", "--prefix", "none"])
        // Run inside this crate's directory so cargo resolves the enclosing workspace.
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree runs");
    let tree = String::from_utf8_lossy(&out.stdout);
    // A `cargo tree` that failed prints nothing useful on stdout; asserting on its emptiness would
    // otherwise *pass* the test below vacuously — the check must fail loudly if it could not run.
    assert!(
        out.status.success() && !tree.trim().is_empty(),
        "cargo tree -p rayland-vtest failed, so the no-GPU guarantee is unverified (status: {}). \
         stderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !tree.contains("rayland-engine"),
        "rayland-vtest must not depend on rayland-engine (it FFI-links libvirglrenderer, and \
         Rayland's C side must run on a machine with no GPU stack). cargo tree said:\n{tree}"
    );
}
