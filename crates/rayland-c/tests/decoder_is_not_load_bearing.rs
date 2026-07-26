//! **The decoder may never make a correctness decision.**
//!
//! (c)1 spec §7 relays the ring as opaque bytes precisely so that a decoding bug cannot become a
//! corruption bug. `rayland-venus-proto` exists to *report* what a stream contains; the moment
//! anything on the relay path decides something from it, a wrong answer stops being a wrong
//! diagnosis and starts being a wrong frame.
//!
//! This test is the mechanical half of that rule. The prose half is in the design spec and in the
//! decoder crate's own docs, and prose does not fail a build.
//!
//! # What it actually checks, and its honest limit
//! It asserts the relay's own modules do not name the decoder crate. It cannot prove absence of
//! influence in general — a future author could route a decision through a third module — but it
//! catches the direct, obvious version, which is the one that would happen by accident.

/// The relay path's source files: the modules that decide what crosses the wire and when.
const RELAY_PATH: &[&str] = &[
    "src/blob_sync.rs",
    "src/ring.rs",
    "src/relay_engine.rs",
    "src/link.rs",
];

#[test]
fn the_relay_path_does_not_consult_the_stream_decoder() {
    for file in RELAY_PATH {
        let source = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("cannot read {file}, which this guard must inspect: {e}"));
        // The crate name in any form — a `use`, a path call, or a re-export.
        assert!(
            !source.contains("rayland_venus_proto"),
            "{file} references the Venus stream decoder. That decoder is DIAGNOSTIC ONLY: it may \
             report what a stream contains, never decide what to relay. See (c)1 spec §7 and \
             docs/superpowers/specs/2026-07-26-venus-stream-decoder-design.md."
        );
    }
}
