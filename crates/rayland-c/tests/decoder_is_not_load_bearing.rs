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
//! # Two needles, not one, and `main.rs` in the file list
//! A first version of this guard checked only for the decoder crate's own name
//! (`rayland_venus_proto`) and did not list `main.rs` among the relay-path files. Both gaps were
//! real, not hypothetical, and review caught them before this landed: `main.rs`'s
//! `ring_watcher_thread` is where the relay decisions actually happen (`messages_for_delta`,
//! `scan_for_out_of_line_stream`, and `link.send` are all there — `ring.rs` itself never sends
//! anything), and that same function held a live, reachable call to
//! `rayland_vtest::venus_ring::decode::decode_commands` — the `RAYLAND_RING_DUMP` diagnostic, safely
//! inert because its result only ever reached `eprintln!`. A crate-name-only grep would never have
//! caught someone making *that already-present call* load-bearing (say, using its `stop` result to
//! skip relaying a truncated command), because the call goes through `rayland-vtest`'s re-export and
//! never spells `rayland_venus_proto` anywhere.
//!
//! The fix was to move that diagnostic into its own module, [`ring_dump`](../src/ring_dump.rs), so
//! `main.rs` itself now contains no decoder call of any kind, and to check for **both** the crate
//! name and the function name (`decode_commands`) here. Either needle alone would catch a direct
//! reference; together they catch the specific reachable path this repository already had, not just
//! a hypothetical one.
//!
//! # What it actually checks, and its honest limit
//! It asserts the relay path's own files do not spell the decoder crate's name or its entry point's
//! name. It cannot prove absence of influence in general — a future author could route a decision
//! through a third module that itself never spells either needle, or rename the function locally
//! with a `use ... as` that only leaves the original name in one line rather than at the call site —
//! but it does catch the concrete situation that actually exists in this codebase: the decoder is
//! already reachable from every relay-path file through `rayland-vtest`'s public re-export, and
//! today the only call anywhere in this crate is `ring_dump`'s diagnostic one. This guard is what
//! stands between that call staying diagnostic and someone quietly wiring its result into a relay
//! decision.

/// The relay path's source files: the modules that decide what crosses the wire and when.
///
/// `main.rs` is here because it — not `ring.rs` — is where the decision actually happens:
/// `ring_watcher_thread` calls `scan_for_out_of_line_stream`, `messages_for_delta`, and `link.send`.
/// `ring_dump.rs` is deliberately **not** in this list: it is where the one legitimate,
/// diagnostic-only call to the decoder lives, and nothing there can reach back into any of these.
const RELAY_PATH: &[&str] = &[
    "src/main.rs",
    "src/blob_sync.rs",
    "src/ring.rs",
    "src/relay_engine.rs",
    "src/link.rs",
];

/// Either of these appearing in a relay-path file is the violation this test exists to catch: the
/// decoder crate's own name (a direct dependency or `use`), or its entry point's name
/// (`decode_commands`, reachable today via `rayland-vtest`'s re-export without ever spelling the
/// crate name).
const FORBIDDEN_NEEDLES: &[&str] = &["rayland_venus_proto", "decode_commands"];

#[test]
fn the_relay_path_does_not_consult_the_stream_decoder() {
    for file in RELAY_PATH {
        let source = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("cannot read {file}, which this guard must inspect: {e}"));
        for needle in FORBIDDEN_NEEDLES {
            assert!(
                !source.contains(needle),
                "{file} references the Venus stream decoder (found {needle:?}). That decoder is \
                 DIAGNOSTIC ONLY: it may report what a stream contains, never decide what to relay. \
                 If you need a diagnostic, put the decode call in `ring_dump.rs` (not on the relay \
                 path) the way `RAYLAND_RING_DUMP` already does, and never let its result reach \
                 `messages_for_delta`, `scan_for_out_of_line_stream`, or the network link. See (c)1 \
                 spec §7 and docs/superpowers/specs/2026-07-26-venus-stream-decoder-design.md."
            );
        }
    }
}
