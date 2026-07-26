//! Compiles the shim against the vendored `venus-protocol` headers.
//!
//! # Why the include order matters
//! The generated headers `#include "vkr_cs.h"`, and this crate supplies its own replacement for that
//! file (see `csrc/vkr_cs.h` for why). `csrc/` is therefore placed **before** the vendored directory
//! on the include path, so ours wins. Today `vendor/` holds only `venus-protocol/` — virglrenderer's
//! own `vkr_cs.h` was never vendored, so this crate's copy is the *only* `vkr_cs.h` anywhere on the
//! include path, and the order is defensive rather than a tiebreak against a real competing header. It
//! stays load-bearing precedent for the day a virglrenderer header (or anything else declaring its own
//! `vkr_cs.h`) is added to `vendor/`: `csrc/` must still resolve first, or Mesa's util library (hash
//! tables, threads, os_file) would be dragged in through it — exactly what this crate exists to avoid.
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
