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
//! # Round two: the allowlist itself rotted, and the diagnostic module was a live side door
//! That fix still checked an explicit *allowlist* of relay-path files (`main.rs`, `blob_sync.rs`,
//! `ring.rs`, `relay_engine.rs`, `link.rs`) — five of the ten files under `src/`. That is exactly the
//! shape of bug that produced the original hole: `shm.rs`, `metrics.rs`, `proxy_link.rs` and
//! `wayland_proxy.rs` were never checked at all, and neither would any file added after this guard
//! was written, because nothing forces a new module onto an allowlist. Worse, the module this guard
//! carves out as the decoder's one legitimate home, [`ring_dump`], was excluded from the list on the
//! strength of a claim that lived only in prose — "it has no return value, so nothing downstream can
//! observe what it decided" — with nothing here checking that the function actually stayed
//! returnless. Both are fixed below:
//!
//! (a) **The list is inverted.** [`relay_path_files`] enumerates every `.rs` file directly under
//! `src/` at test time and subtracts [`EXCLUDED`], an explicit, commented, and now the *only* way a
//! file can escape this check. A new module is covered the moment it exists; escaping the guard now
//! requires a deliberate, reviewable edit to `EXCLUDED` rather than simply not being on a list nobody
//! updates.
//!
//! (b) **The property `ring_dump`'s exclusion rests on is now pinned, not asserted in prose.**
//! [`ring_dump_exposes_only_the_returnless_diagnostic_entry_point`] asserts that
//! `src/ring_dump.rs` has exactly one `pub fn`, and that it is the exact, returnless signature
//! `pub fn dump_if_enabled(pending: &RingDelta) {`. Give that function a return type — the one change
//! that would let its result reach a relay decision without ever spelling either needle at the call
//! site — and this assertion fails, not silently.
//!
//! # What it actually checks, and its honest limit
//! It asserts every relay-path file (every file under `src/` not named in [`EXCLUDED`]) does not
//! spell the decoder crate's name or its entry point's name, and it asserts the one excluded file's
//! only public function stays returnless. Together these catch the concrete situation that actually
//! existed in this codebase: the decoder is reachable from every relay-path file through
//! `rayland-vtest`'s public re-export, `ring_dump` is the one place calling it, and that call is safe
//! only because it is provably unable to return anything.
//!
//! What is left, now narrower than before: a future author could still route a decision through a
//! third module added to [`EXCLUDED`] alongside a false justification (this guard cannot audit the
//! *truth* of a comment, only the shape of the code), or rename the function locally with a
//! `use ... as` that leaves the original name in exactly one line rather than at the call site. Both
//! require a deliberate, reviewable act — adding a line to `EXCLUDED`, or writing a renaming `use` —
//! rather than the silent, no-list-update-required gap the allowlist version had.

use std::path::Path;

/// Files under `src/` that are deliberately exempted from the relay-path substring check, and why
/// each one earns that exemption. Every `.rs` file **not** listed here is checked automatically —
/// adding a new module to `src/` requires no edit to this test to be covered by it; only a
/// deliberate exclusion does.
const EXCLUDED: &[&str] = &[
    // The one legitimate, diagnostic-only call site for the decoder (see its own module docs). Safe
    // to exclude from the substring guard only because `dump_if_enabled` is mechanically pinned,
    // below, to a returnless signature — see `ring_dump_exposes_only_the_returnless_diagnostic_entry_point`.
    "ring_dump.rs",
];

/// Either of these appearing in a relay-path file is the violation this test exists to catch: the
/// decoder crate's own name (a direct dependency or `use`), or its entry point's name
/// (`decode_commands`, reachable today via `rayland-vtest`'s re-export without ever spelling the
/// crate name).
const FORBIDDEN_NEEDLES: &[&str] = &["rayland_venus_proto", "decode_commands"];

/// Every `.rs` file directly under `src/`, minus [`EXCLUDED`], as paths relative to the crate root —
/// the set this guard actually inspects.
///
/// # Why enumerate rather than list
/// A hand-maintained allowlist rots the moment a new file is added and nobody remembers to extend
/// it — that is exactly how `main.rs` was missing from the first version of this guard. Enumerating
/// `src/` and subtracting an explicit, commented exclusion set means a new module is covered by
/// construction; only a deliberate subtraction can opt one out.
///
/// # Inputs / outputs
/// - Reads `src/` relative to the crate root (cargo runs tests with the crate directory as the
///   working directory, so a relative path is correct here, matching the rest of this file).
/// - Returns a sorted list of `src/<name>.rs` paths, sorted so a failing assertion always names the
///   same file first regardless of the operating system's directory-listing order.
///
/// # Failure modes
/// Panics if `src/` cannot be read — that would mean this test is not running against a real
/// `rayland-c` checkout, which is a setup error this guard cannot meaningfully recover from.
fn relay_path_files() -> Vec<String> {
    let mut files: Vec<String> = std::fs::read_dir("src")
        .expect("crate's own src/ directory must exist and be readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        // Only plain `.rs` files: `src/` here has no subdirectories, but this guards against one
        // being added later and silently swallowed (or panicking) instead of being enumerated.
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rs"))
        .filter_map(|path: std::path::PathBuf| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !EXCLUDED.contains(&name.as_str()))
        .map(|name| format!("src/{name}"))
        .collect();
    // Deterministic order: `read_dir`'s order is filesystem-dependent, and a stable order keeps a
    // failing assertion's first-named file reproducible across machines and reruns.
    files.sort();
    files
}

#[test]
fn the_relay_path_does_not_consult_the_stream_decoder() {
    for file in relay_path_files() {
        let source = std::fs::read_to_string(&file)
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

/// Pins the one fact that makes excluding `ring_dump.rs` from the substring guard, above, actually
/// safe: it exposes exactly one public function, and that function is the exact, returnless
/// signature `pub fn dump_if_enabled(pending: &RingDelta) {`.
///
/// # Why this matters
/// The substring guard cannot see *what a function returns* — it only greps for names. Excluding
/// `ring_dump.rs` on the strength of "its call to the decoder never returns anything" was, before
/// this test existed, a claim made only in prose (this file's own module docs, and `ring_dump.rs`'s).
/// Give `dump_if_enabled` a return type — say, a `bool` that a caller starts threading into a relay
/// decision — and the substring guard above would stay green forever, because neither
/// `rayland_venus_proto` nor `decode_commands` needs to appear anywhere outside `ring_dump.rs` for
/// that to happen. This test is what turns "the diagnostic cannot influence the relay because it
/// returns nothing" from an assertion into a build failure the moment it stops being true.
///
/// # Inputs / outputs
/// Reads `src/ring_dump.rs` as text and checks two things: exactly one `pub fn` appears in the file
/// (the surface this module exposes is exactly one function, not a growing API), and the literal
/// string `pub fn dump_if_enabled(pending: &RingDelta) {` appears in it (the signature is exactly
/// this, not `-> bool` or any other non-void return).
///
/// # Failure modes / honest limit
/// A textual check, not a type-checked one: it would not catch a return type spelled with unusual
/// whitespace, nor a `pub(crate) fn` sneaking in a second entry point (`pub(crate)` does not match
/// the literal `pub fn` this test greps for — deliberately out of scope, since a crate-private
/// function cannot be called from outside `rayland-c` either way, and this guard's whole concern is
/// the *shape* `main.rs` sees). What it does catch, exactly, is the concrete mutation the review that
/// created this test specified: adding a return value to `dump_if_enabled`.
#[test]
fn ring_dump_exposes_only_the_returnless_diagnostic_entry_point() {
    let path = Path::new("src/ring_dump.rs");
    let source = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read {}, which this guard must inspect: {e}", path.display()));

    let pub_fn_count = source.matches("pub fn ").count();
    assert_eq!(
        pub_fn_count, 1,
        "src/ring_dump.rs must expose exactly one `pub fn` — it is excluded from the relay-path \
         substring guard on the strength of being a single, provably returnless diagnostic entry \
         point, and a second public function would not be covered by that reasoning."
    );

    assert!(
        source.contains("pub fn dump_if_enabled(pending: &RingDelta) {"),
        "src/ring_dump.rs's one public function must be exactly `pub fn dump_if_enabled(pending: \
         &RingDelta) {{` — returnless. A return value here (e.g. a `bool` or a `Result`) would let \
         `main.rs` thread this diagnostic's result into a relay decision without ever spelling \
         `rayland_venus_proto` or `decode_commands` itself, which is exactly the gap this test \
         exists to close."
    );
}
