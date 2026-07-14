//! Build script for `rayland-engine`.
//!
//! Its sole job is to tell Cargo how to link the system `libvirglrenderer` (the C library
//! whose `virgl_renderer_*` symbols `src/ffi.rs` declares). We do NOT generate bindings here:
//! the FFI surface is hand-written in `src/ffi.rs` (see that file and `Cargo.toml` for why).
//!
//! We use `pkg-config` to locate the library rather than hard-coding `-lvirglrenderer`, so the
//! build follows whatever prefix the distribution installed the library under and fails with a
//! clear "install libvirglrenderer-dev" style message on a host that lacks it.

// Bring the pkg-config probe API into scope.
use std::error::Error;

/// Entry point run by Cargo before compiling the crate.
///
/// Probes for `virglrenderer` via `pkg-config`, which (on success) prints the
/// `cargo:rustc-link-lib=virglrenderer` and `cargo:rustc-link-search=...` directives that make
/// the linker find and link the shared object. Returns an error (failing the build with a
/// readable message) if the library is not installed.
///
/// # Failure modes
/// - `libvirglrenderer` is not installed / not discoverable by `pkg-config`: the build fails
///   here with pkg-config's own diagnostic, which names the missing `.pc` file.
fn main() -> Result<(), Box<dyn Error>> {
    // Re-run this script only if it changes; nothing else here depends on source files.
    println!("cargo:rerun-if-changed=build.rs");

    // Ask pkg-config for `virglrenderer`. `.probe()` emits the `cargo:rustc-link-lib` and
    // `cargo:rustc-link-search` lines Cargo needs to link the `.so`, and errors out (with a
    // message pointing at the missing package) when the library is absent. We do not require a
    // minimum version: the C0 surface we use has been stable across virglrenderer releases, and
    // the reliability spike pins behaviour against whatever version the host provides.
    pkg_config::Config::new().probe("virglrenderer")?;

    // Success: pkg-config has printed all required link directives.
    Ok(())
}
