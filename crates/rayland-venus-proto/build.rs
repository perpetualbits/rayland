//! Compiles the shim against the vendored `venus-protocol` headers.
//!
//! # Why the include order matters
//! The generated headers `#include "vkr_cs.h"`, and this crate supplies its own replacement for that
//! file (see `csrc/vkr_cs.h` for why). `csrc/` is therefore placed **before** the vendored directory
//! on the include path, so ours wins. Reversing these two lines would pull in virglrenderer's header
//! and, through it, Mesa's util library — which is exactly what this crate exists to avoid.
fn main() {
    // Rebuild when anything we compile changes; without these, a header edit is silently ignored.
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-changed=vendor/venus-protocol");

    cc::Build::new()
        .file("csrc/shim.c")
        .include("csrc")
        .include("vendor/venus-protocol")
        // The vendored headers are generated code and are not ours to tidy; their warnings would
        // drown the shim's own. Rayland's C is only `csrc/`.
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-missing-field-initializers")
        .std("c99")
        .compile("rayland_venus_proto_shim");
}
