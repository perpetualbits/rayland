//! **Framing for Venus command streams** — how long is the command at the start of these bytes?
//!
//! # Why this crate exists
//! Rayland relays Venus command streams byte-exactly and, until this crate, could not read them:
//! `rayland_vtest::venus_ring::decode::encoded_size` can express only *fixed* encodings, so a walk
//! halts at the application's first real command. Most Vulkan commands are variable-length — arrays,
//! `pNext` chains, optional pointers — so no table of sizes can ever walk a real stream.
//!
//! # Why it borrows rather than reimplements
//! Mesa already generates a complete, exact decoder for its own protocol. Reimplementing it would
//! create a second source of truth for a format Rayland does not own, and when the two diverge the
//! symptom is a decoder confidently reporting the wrong thing. This mirrors `CLAUDE.md`'s locked
//! decision to reuse the Venus/virglrenderer engine rather than write one.
//!
//! # THE BINDING CONSTRAINT
//! **This crate is diagnostic and structural only.** It may report, log, measure and assert. It may
//! never decide what to relay, when to relay it, or which blobs a delta reads. (c)1 spec §7 relays
//! the ring as opaque bytes precisely so that a decoding bug cannot become a corruption bug, and
//! that reasoning is untouched here.
//!
//! See `docs/superpowers/specs/2026-07-26-venus-stream-decoder-design.md`.

// The decoding entry point arrives in Task 2; Task 1 proves the vendored protocol compiles.
unsafe extern "C" {
    /// Links the shim's self-test, proving the vendored headers compiled and linked.
    fn rayland_venus_proto_selftest() -> core::ffi::c_int;
}

/// The `VkCommandTypeEXT` of `vkGetFenceStatus`, as reported by the compiled shim.
///
/// Exists only so Task 1 has something testable: it proves the vendored protocol headers are on the
/// include path, compiled, and linked. Removed in Task 2 when the real entry point lands.
///
/// # Failure modes
/// None; the shim's implementation is a constant.
pub fn selftest_command_type() -> i32 {
    // SAFETY: the shim function takes no arguments, touches no memory, and returns a constant.
    unsafe { rayland_venus_proto_selftest() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vendored protocol compiles, links, and reports the value Mesa's headers define — which is
    /// the whole of Task 1's deliverable. `38` is `VK_COMMAND_TYPE_vkGetFenceStatus_EXT`, the command
    /// vkcube polls while waiting for the submit that motivated this crate.
    #[test]
    fn the_vendored_protocol_compiles_and_links() {
        assert_eq!(selftest_command_type(), 38);
    }
}
