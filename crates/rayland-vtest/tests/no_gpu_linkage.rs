//! Enforces one narrow, load-bearing structural constraint: `rayland-vtest`'s dependency tree
//! must not contain `rayland-engine` — the one crate in this workspace that FFI-links
//! `libvirglrenderer`. The C side of Rayland depends on `rayland-vtest`, and C is by design the
//! *weak* machine — a headless box, eventually a RISC-V one, with no GPU libraries at all. If
//! `rayland-vtest` ever pulled in `rayland-engine`, the project's central claim ("C needs no GPU")
//! becomes quietly false.
//!
//! **What this test does *not* guard:** anything else that could drag GPU code onto C. It greps
//! `cargo tree` for the literal string `rayland-engine`; it has nothing to say about a *new,
//! direct* dependency on `ash`, a Vulkan-loader `*-sys` crate, or any other GPU-linking crate
//! added straight to `rayland-vtest`'s manifest — those would sail straight through unnoticed.
//! `rayland-engine` is simply the only GPU-linking crate that exists in this workspace today, so
//! it is the only needle this test can meaningfully look for.
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
/// # Why this guard has real teeth despite cargo's own cycle rejection
/// For an ordinary `[dependencies]` edge, this test is mostly redundant: `rayland-engine` already
/// depends on `rayland-vtest` (that is the whole point of the Task 1 split), so cargo's own
/// dependency-cycle check would refuse to build the workspace the moment anyone added a normal
/// dependency from `rayland-vtest` back to `rayland-engine`, at any depth — the manifest simply
/// would not compile, with or without this test.
///
/// The guard's real value is two things cargo does *not* catch on its own:
/// - **Dev-dependency cycles, which cargo permits.** A `[dev-dependencies]` edge from
///   `rayland-vtest` to `rayland-engine` is not a build cycle (dev-dependencies are not part of
///   the normal dependency graph cargo checks for cycles), so cargo would happily accept it —
///   and it would still drag `libvirglrenderer` onto every machine that runs this crate's test
///   suite, including C. Confirmed by mutation: adding such a dev-dependency compiles fine, and
///   this test is what catches it (fails with the assertion message above, naming
///   `rayland-engine` in the printed tree).
/// - **Future restructuring.** Cargo's automatic cycle protection only exists *because*
///   `rayland-engine` currently depends on `rayland-vtest`. If that relationship ever changed —
///   `rayland-engine` stopped depending on `rayland-vtest`, e.g. after some further split — cargo
///   would no longer reject a `rayland-vtest → rayland-engine` dependency at all, and this test
///   would become the *only* thing standing between C and a linked GPU stack.
///
/// `cargo tree`'s transitive output (rather than a check of only the direct `[dependencies]`
/// table) is what makes this cover an indirect route too: a crate that does not depend on
/// `rayland-engine` itself but pulls in something that does would otherwise be invisible to
/// anyone reading `rayland-vtest`'s own manifest.
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
