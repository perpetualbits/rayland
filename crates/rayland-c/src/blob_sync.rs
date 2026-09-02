//! **Blob synchronisation, C→S**: deciding what must cross the wire alongside a ring delta, and in
//! what order.
//!
//! # The problem, stated as C0 measured it
//! Ring-findings §6 is blunt that this — not the ring — is the genuinely hard part of remote Vulkan.
//! An application calls `vkMapMemory` **once**, gets a raw pointer, and then writes vertices,
//! uniforms and texture data straight into it for the rest of its life, **with no API call at all**:
//!
//! > *There is no command to intercept. There is no event. There is nothing on any wire.*
//!
//! On one machine the GPU simply sees those writes. Across a network there is no shared page, so
//! unless something ships the bytes, S's GPU renders from memory the application never wrote. C0
//! Task 4b caught exactly this in the reference app: `res=3`, 64 bytes, `blob_id = 16`, decoding
//! float-for-float into the triangle's three vertices. Without this module those 64 bytes never
//! leave C, and the "first light" triangle is drawn from uninitialized memory.
//!
//! # The strategy: incremental diff against a per-blob baseline, and why the trigger is what it is
//! Spec §7 originally pinned v1's answer as **ship the full contents of every mapped blob, whole, in
//! the direction it is needed — no dirty tracking, no cleverness.** For a 64-byte vertex buffer and a
//! 16 KiB readback that was trivially cheap. It was not cheap for a real application: measured on
//! vkcube (spec §8), whole-blob resync cost **16.5 MB** of blob traffic against **23 KB** of actual
//! ring commands, which buries the link and looks like a hang. That measurement is what retired v1.
//!
//! The replacement, described in `docs/design/2026-07-25-c1-incremental-blob-sync.md`,
//! keeps the same trigger and the same ownership filter, but changes *what* crosses once a blob is
//! judged eligible: each application blob carries a baseline (its last-agreed state with S, held by
//! [`crate::shm::LocalBlob`]), and [`LocalBlob::take_changed_runs`](crate::shm::LocalBlob::take_changed_runs)
//! diffs the live bytes against that baseline in one pass, re-baselining as it goes. An unchanged blob
//! yields no runs and crosses nothing. A changed blob yields one [`rayland_relay::BlobRun`] per
//! contiguous run of differing bytes — each reusing `BlobData`'s existing `offset` field, so there is
//! no wire change — and runs are never coalesced across unchanged bytes in between, because doing so
//! would re-ship exactly the bytes the diff exists to skip. Byte-granular diffs can therefore
//! fragment into many small runs; that is why bytes and message count are reported as separate
//! metrics rather than assumed to move together.
//!
//! This does **not** solve remote `vkMapMemory` in general. A blob the application genuinely rewrites
//! every frame — `rayland-icosa-cpu`'s megabyte of fractal texture is the standing example — still
//! ships nearly whole, because nearly every byte really did change; making *that* case cheap is
//! (c)2's problem, not this one's. Deduplicating identical bytes across different blobs, or across
//! time within one blob's own history, is (c)3's. What this module buys is narrower and still real:
//! the common case of a blob written once and read many times (the reference app's vertex buffer)
//! now crosses exactly once, not on every relay.
//!
//! The decoding avoidance v1 also cared about still holds: this diff works on raw bytes, never on the
//! ring's decoded meaning, so *decoding the ring to make a correctness decision means a decoding bug
//! becomes a corruption bug* remains just as true as it was under v1 — the diff adds no parsing of
//! Vulkan structures, only a byte comparison against a baseline.
//!
//! The trigger deserves its own paragraph, because the phrase that sounds right is wrong. "Sync at
//! every submission boundary" is not implementable here: **`vkQueueSubmit` is invisible to us.** It
//! is encoded *inside* the ring, and v1 relays the ring as opaque bytes without parsing them. The
//! only boundary C can actually observe is **its own relay event** — "we are about to ship ring
//! bytes to S". So that is the trigger, and it is deliberately over-eager: it **inspects** every
//! application blob on every relay, on relays that may contain no submit at all, whether or not that
//! blob actually changed. Under the diff that over-eagerness is no longer a bandwidth cost — an
//! unchanged blob costs one comparison pass and ships nothing — it only means the trigger cannot tell
//! in advance which blobs are worth looking at, so it looks at all of them.
//!
//! # Ordering is the correctness property this module exists to guarantee
//! **Blobs must reach S before the ring delta whose commands may read them.** The ring bytes are
//! opaque to us, so any delta must be assumed to contain a draw that reads every mapped blob. Ship
//! the delta first and S's ring thread — which polls, and runs asynchronously the instant `tail`
//! moves (`vkr_ring.c:262-266`) — may dispatch a draw against vertex memory that is still zeros.
//! That failure is timing-dependent: it would appear as an intermittently wrong or empty frame, with
//! nothing anywhere naming the cause.
//!
//! Returning the messages **in order, as a list**, rather than sending them from the middle of this
//! logic, is what makes that guarantee testable without a network, a GPU or an S. The ordering is
//! the whole point of the module, so it is asserted directly rather than inferred from a live run.
//!
//! # Why this is not simply "ship every blob"
//! Each blob has an owner, and the conservative-looking choice of shipping all of them is a
//! corruption bug. C's copies of Venus's *internal* shmems are not C's to publish: S's reply arena
//! is written by S, and overwriting it with C's stale copy would destroy replies the application is
//! blocked on. So this ships the application's memory only, on ring-findings §6's `blob_id` signal —
//! see [`rayland_vtest::venus_ring::is_application_memory`], which holds the evidence.
//!
//! # Why C still routes on `blob_id` when S no longer does
//! Spec §7.2 retracted the ownership predicate for **S→C** and replaced it with "S ships back
//! exactly the bytes S wrote". The natural question is why C does not mirror that, and the answer is
//! that the mirror image is not available to C and would not be an improvement if it were.
//!
//! The rule works on S because S's *own* writes are the thing to be detected, and everything else
//! that touches S's pages arrives through one function ([`copy_in`]) that can record it. **C is not
//! in that position.** C's peer across these mappings is Mesa — which is to say the application —
//! writing with plain stores, from another process, announcing nothing. A blob "C wrote" and a blob
//! "the application wrote" are the same blob, so the symmetric predicate on C would collapse to "did
//! anything other than S's replies change?", which is what shipping the application's memory already
//! means. It would also start shipping the 8 MiB staging pool C's Mesa records into — writes C
//! genuinely made, harmless but pure waste — where `blob_id` correctly declines to.
//!
//! So `blob_id` survives as a **C→S** routing rule for the reason it was always sound in that
//! direction: it keeps C from publishing memory S owns. The direction where it was a *guess* at
//! authorship, and therefore wrong, is the one §7.2 fixed.
//!
//! [`copy_in`]: https://docs.rs/rayland-s

// The ring's identity and the delta the watcher drained.
use crate::relay_engine::BlobTable;
use crate::ring::RingDelta;
// The messages this module decides to send.
use rayland_relay::C2S;

// =================================================================================================
// `blobscan` — attributing the per-delta blob scan to individual blobs.
// =================================================================================================
//
// # The question it was built for, and the answer it gave
// The per-delta scan costs **8.92 ms on the riscv64 board even for a fixture that changes ~80 bytes
// a frame**, so 82% of it is `memcmp` over memory that did not change. Dirty-page tracking would
// remove that and riscv64 cannot do dirty-page tracking, so the open question was whether a
// **protocol-level** filter could skip blobs C knows cannot have changed. This instrument was
// written to size that idea before building it, and it **refuted it**. Measured on the board,
// 2026-09-02, per delta:
//
// | | `icosa-gpu` | `icosa-cpu` |
// |---|---|---|
// | the 8 MiB Venus staging pool | 8.06 ms — **85%** | 7.91 ms — **68%** |
// | the application's own 1 MiB buffer | — | 2.20 ms — 19% |
// | a 1 MiB non-application blob that **never changes** | 1.09 ms — 11% | 1.12 ms — 10% |
// | a 256 KiB application blob that **never changes** | 0.34 ms — 3% | 0.34 ms — 3% |
//
// Three facts kill the filter, and they are worth keeping so nobody re-proposes it:
//
//  1. **The dominant blob cannot be skipped.** The staging pool genuinely changes on 41–47% of
//     deltas — but ships only ~500–600 bytes when it does. We scan 8 MiB to find 600 bytes.
//     Narrowing that requires knowing *which region* changed, which is either the decoder (banned
//     by (c)1 §7, enforced by `tests/decoder_is_not_load_bearing.rs`) or dirty-page tracking
//     (absent on riscv64). There is no third signal.
//  2. **The filterable remainder is 13–15%, and skipping it is unsound.** "Has not changed yet"
//     does not imply "will not change", and establishing that it has not changed *is* the scan.
//  3. **No static property discriminates.** `is_application_memory` is useless here: application
//     memory both never-changes (the 256 KiB blob) and changes on every frame (`icosa-cpu`'s 1 MiB
//     staging buffer). Resource ids move between workloads — the pool is `res 6` in one fixture and
//     `res 7` in the other.
//
// Scan bandwidth measured **0.93–1.02 GB/s** in both fixtures, so the cost is simply
// `bytes ÷ 1 GB/s`: the scan already runs at the board's memory bandwidth and no chunk-size or
// constant tuning can help it. **The only lever is scanning fewer bytes.**
//
// # Why it is kept rather than deleted
// It began as a throwaway probe. It is kept because the question recurs — every future attempt on
// frame time will want to know where the scan time went on *that* workload, and re-deriving it cost
// a cross-compile and two board runs. Keeping it also keeps the table above honest: it is
// reproducible with `BLOBSCAN=1 scripts/c2-icosa-milkv.sh` rather than being a number in a comment.
//
// # Why it is shaped like this
// Four instruments in this project have been caught changing their own measurement. This one does,
// per blob per delta, two `Instant` reads and one map update — and the update happens while the
// caller **already holds the blob table lock**, so the mutex below is uncontended by construction
// and adds no new synchronisation to the relay path. It prints once every `REPORT_EVERY` deltas,
// and does nothing whatsoever when the gate is off. Both fixtures stayed 120/120 bit-identical with
// it armed, which is the check that says the instrument did not become the experiment.
//
// # What it may never become
// **Nothing here may ever inform what is relayed.** It counts and it prints. It has no return value
// that reaches a relay decision, and it must not acquire one: the moment a measurement of this kind
// starts *deciding*, it is on the wrong side of (c)1 §7 for exactly the reason the decoder is.
pub(crate) mod blobscan {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock};

    /// Deltas between reports. Large enough that printing is not in the measurement, small enough
    /// that a run that ends early still yields something.
    const REPORT_EVERY: u64 = 200;

    /// What is accumulated per resource id.
    #[derive(Default, Clone, Copy)]
    struct Acc {
        /// How many times this blob was scanned.
        scans: u64,
        /// Total nanoseconds spent inside `take_changed_runs` for it.
        nanos: u64,
        /// The blob's length, last seen (constant in practice; recorded to size the scan).
        len: u64,
        /// How many scans found at least one changed run — the number this spike turns on.
        scans_with_change: u64,
        /// Total bytes actually shipped for it.
        changed_bytes: u64,
        /// Whether C classes this blob as application memory (ring-findings §6's `blob_id` signal).
        /// A **non-decode** property, so unlike anything the decoder could tell us it is legitimate
        /// input to a filter — which is precisely what this spike is trying to find.
        app_mem: bool,
    }

    static STATE: OnceLock<Mutex<(BTreeMap<u32, Acc>, u64)>> = OnceLock::new();

    fn state() -> &'static Mutex<(BTreeMap<u32, Acc>, u64)> {
        STATE.get_or_init(|| Mutex::new((BTreeMap::new(), 0)))
    }

    /// Whether recording is on, from `RAYLAND_C1_BLOBSCAN`. Read once.
    pub fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("RAYLAND_C1_BLOBSCAN").is_some())
    }

    /// Record one blob's scan.
    pub fn record(
        res_id: u32,
        len: usize,
        nanos: u64,
        runs: usize,
        changed_bytes: usize,
        app_mem: bool,
    ) {
        let mut g = state().lock().expect("throwaway instrument mutex");
        let e = g.0.entry(res_id).or_default();
        e.scans += 1;
        e.nanos += nanos;
        e.len = len as u64;
        if runs > 0 {
            e.scans_with_change += 1;
        }
        e.changed_bytes += changed_bytes as u64;
        e.app_mem = app_mem;
    }

    /// Called once per delta; prints a cumulative report every `REPORT_EVERY` deltas.
    pub fn end_of_delta() {
        let mut g = state().lock().expect("throwaway instrument mutex");
        g.1 += 1;
        if g.1 % REPORT_EVERY != 0 {
            return;
        }
        let deltas = g.1;
        let mut out = String::new();
        for (res_id, a) in &g.0 {
            out.push_str(&format!(
                "BLOBSCAN deltas={deltas} res={res_id} len={} scans={} ms_total={:.1} \
us_per_scan={:.1} scans_with_change={} changed_bytes={} app_mem={}\n",
                a.len,
                a.scans,
                a.nanos as f64 / 1e6,
                if a.scans > 0 { a.nanos as f64 / 1e3 / a.scans as f64 } else { 0.0 },
                a.scans_with_change,
                a.changed_bytes,
                a.app_mem,
            ));
        }
        eprint!("{out}");
    }
}

/// Decide everything C must send S for one drained ring delta, in the order it must be sent.
///
/// **This function's return order is a correctness contract, not a convenience.** See the module
/// docs: every [`C2S::BlobData`] must precede the [`C2S::RingDelta`], because S's ring thread
/// dispatches the delta's commands asynchronously the moment its `tail` moves, and those commands
/// may read the very memory the blob messages carry.
///
/// # Why this copies rather than sends
/// The blob table is polled continuously by the ring watcher and written by the reader thread as S's
/// replies arrive, and [`BlobTable`]'s lock discipline is that it must **never** be held across a
/// network send. Returning a list means the lock is held only for the `memcpy` out of each mapping
/// and is released before the caller touches the link. The discipline is thereby structural rather
/// than something a caller has to remember.
///
/// # Inputs / outputs
/// - `blobs`: the local blob shadows. Locked briefly, per the discipline above.
/// - `ring_res_id`: the S-side resource id of the command ring, stamped on the delta.
/// - `delta`: the bytes Mesa produced, already un-wrapped by
///   [`RingWatcher::take_delta`](crate::ring::RingWatcher::take_delta). Consumed, because its
///   `Vec<u8>` is moved onto the wire rather than copied again.
/// - Returns the messages to send, in order: each application blob's *changed* runs (none at all for
///   an unchanged blob), then the ring delta.
///
/// # Failure modes
/// Cannot fail. A blob table that does not contain the ring is not this function's problem — the
/// caller drained the delta from it and would have noticed. Nothing here validates the delta;
/// [`rayland_vtest::venus_ring::scan_for_out_of_line_stream`] is the check that governs whether the
/// delta may be relayed at all, and the caller runs it.
///
/// # Pitfalls
/// - **This ships only what changed since C's last relay of a given blob, not whole blobs.** An
///   unchanged blob emits nothing at all; a changed one emits one [`rayland_relay::BlobRun`] per
///   contiguous run of differing bytes, via
///   [`LocalBlob::take_changed_runs`](crate::shm::LocalBlob::take_changed_runs). Do **not** read this
///   as "this module is now cheap in general": a blob the application genuinely rewrites every frame
///   (`rayland-icosa-cpu`'s fractal texture is the standing example) still ships nearly whole, because
///   nearly every byte really did change — that is (c)2's problem, not this one's, and this diff must
///   not be mistaken for having solved it. Cross-blob or cross-time deduplication (the same content
///   appearing twice) is (c)3's. What this diff buys is narrower and still real: a blob written once
///   and then only read — the reference app's vertex buffer, and vkcube's setup blobs generally —
///   crosses exactly once instead of on every relay. Byte-granular diffs can also **fragment**: where
///   changed bytes are interleaved with unchanged ones, one logical change can surface as many small
///   runs, so relay bytes and relay *message count* are reported as separate metrics rather than
///   assumed to move together — a fragmented blob can trade fewer bytes for more messages. See
///   `docs/design/2026-07-25-c1-incremental-blob-sync.md` for the measurement that retired the old
///   whole-blob strategy (vkcube: 16.5 MB of blob resends against 23 KB of actual ring commands) and
///   the fragmentation caveat in full.
/// - **The lost-write race this used to describe is gone, and it was never fixable here.** Until
///   (c)1 Task 5b, S's return path shipped back *every application blob its GPU might have written*,
///   which meant S sent C stale copies of blobs S never wrote at all — vertex and uniform buffers,
///   the common case — and C's reader laid them over whatever the application had written since.
///   This function then faithfully relayed the stale bytes back to S. Spec §7.2 retracted that rule:
///   **S now ships back exactly the bytes S is observed to have written**, so nothing arrives here
///   to overwrite a blob the GPU never touched, and shipping a blob's changed runs C→S is no longer
///   racing anything on the return leg. The repair had to happen on S because only S can see which
///   bytes S wrote; there was no version of this function that could have avoided it.
/// - **What remains is narrower than it once was, but it is *two* hazards, not one — an earlier draft
///   of this list undercounted them, and this is the correction.**
///   - **Tearing.** A blob the application writes *while* this copies it is torn, and nothing here
///     can prevent that — it is the `vkMapMemory` problem itself, since the application is not
///     obliged to tell anyone when it stops writing and v1 has no flush hook to wait on. For a
///     *correctly synchronized* application it does not fire on memory the GPU actually wrote,
///     because S's own ordering guarantees the bytes land before the `head` update that releases the
///     app's fence wait.
///   - **C relaying its own stale copy of a blob that S — not C — currently owns the live contents
///     of, clobbering S's authoritative copy with old news.** Under the old whole-blob strategy this
///     was a standing hazard on `res=6`, the readback buffer S's GPU writes and C never touches:
///     C shipped its (stale) copy of `res=6` on *every* relay regardless of whether it had changed,
///     which could clobber pixels S's GPU had just written with C's zeros.
///
///     **This function lands the C-side half of the fix — the symmetric, observed-write rule
///     `docs/design/2026-07-25-c1-incremental-blob-sync.md` describes — and the fix's other half is
///     wired in too, at both places S's bytes enter a C mapping.** The diff means C no longer ships
///     `res=6` on every relay unconditionally; it ships a run only when
///     [`LocalBlob::take_changed_runs`](crate::shm::LocalBlob::take_changed_runs) sees the live bytes
///     differ from C's baseline. That baseline is only correct if it is kept in step with S's own
///     writes as they arrive back over the wire, and [`LocalBlob::note_s_wrote`](crate::shm::LocalBlob::note_s_wrote)
///     is that fold. It is **not** called from this module — this module only ever ships C→S — so it
///     must be called everywhere S's bytes are written into a C-side mapping. There are exactly two
///     such places today, and both call it:
///     - `apply_blob_data` in `crates/rayland-c/src/main.rs`, on every inbound `S2C::BlobData` — the
///       steady-state return path (readback buffers, the reply arena) for a blob already registered.
///     - `commit_pending_blob` in `crates/rayland-c/src/relay_engine.rs`, on the `initial` runs carried
///       inside `S2C::BlobCreated` — because a readback buffer is routinely born with the finished
///       frame already in it (Mesa's `vkMapMemory` is lazy, so the blob comes into existence *after*
///       S's GPU has already rendered into it), which makes creation itself a return-path event, not
///       merely the steady state.
///
///     **If a third place is ever added where S's bytes land in a C mapping, it must call
///     `note_s_wrote` too, or this hazard resurfaces there.** The mechanism of the resurfacing is
///     mechanical and worth stating once, since it is easy to reintroduce by omission rather than by a
///     visible bug: C's reader applies S's fresh bytes into the live mapping, the stale baseline does
///     not reflect them, and the next call to `take_changed_runs` reads the now-fresh bytes as a
///     *change C made* and ships them straight back to S as `BlobData` — overwriting S's own
///     authoritative pixels with a copy of what S itself just sent. Byte diffing alone cannot tell
///     "S just wrote this" apart from "the application just wrote this"; only folding S's writes into
///     the baseline, at every site they arrive, can.
///
///     **This is not tearing.** C's copy is whole and un-torn in this scenario; the (now-closed) defect
///     was that it could be stale-relative-to-freshly-applied in a way plain byte diffing cannot see.
///
///     **Why the reference app never triggers it, either way.** An application blocked in
///     `vkWaitForFences` issues no further ring traffic of its own — `vn_ring_wait_seqno` only polls
///     `head` in shared memory, it writes nothing — so there is no *second* relay event for this
///     function to clobber the readback with. A real application's frame N+1 command stream is
///     exactly that second relay, carrying frame N's readback back over frame N's genuine pixels; the
///     reference app never issues one.
/// - **A further hazard was recorded here until the §7.2 amendment removed it: false sharing at S's
///   page grain.** S's returned run used to be rounded out to a 4096-byte page, so when S's engine
///   wrote one region of a page and the application wrote another region of the same page — legal, and
///   needing no Vulkan synchronization between them — the run carried S's stale copy of the
///   application's bytes alongside S's own fresh ones, and this side laid the lot down. S now diffs
///   **byte-granular**, so every byte arriving from S is a byte S actually wrote. See
///   `rayland_s::blob::HostBlob::take_bytes_s_wrote`.
/// - **The reference app reaches none of this**, and that is a property of *this one workload*
///   rather than of the algorithm: it writes its vertex buffer exactly once, before its first draw,
///   and never again. Which is exactly why every test here passed while the S→C rule was a race, and
///   why the spec calls this narrow slice v1's answer rather than the answer.
pub fn messages_for_delta(blobs: &BlobTable, ring_res_id: u32, delta: RingDelta) -> Vec<C2S> {
    let mut out = Vec::new();

    // Scope the lock tightly: it is released before this function returns, so the caller physically
    // cannot hold it across the sends. See the note above on why that matters.
    {
        // `iter_mut`, not `iter`: the diff re-baselines each blob it inspects.
        let mut table = blobs.lock().expect("the blob table lock is never poisoned");
        // DIAGNOSTIC (`RAYLAND_C1_BLOB_FP`), throttled: fingerprint **every** blob C holds, including
        // the Venus-internal ones this loop is about to skip. The ring relay is now proven byte-exact
        // (253/253 matching digests), so S's failure to complete the application's `vkQueueSubmit`
        // must be about state the submit *references*. This is the same instrument one level down:
        // if S's copy of some resource disagrees with C's, this and its S-side twin say which.
        blob_fingerprints(&table, delta.tail);

        for (&res_id, blob) in table.iter_mut() {
            // **The ring is the one blob this loop must never touch.** It has its own message,
            // `C2S::RingDelta`, which carries the `tail` that makes the bytes meaningful and is
            // deliberately sent last. Shipping the same pages here as well would publish ring bytes
            // ahead of the `tail` that validates them, and S's ring thread reads whatever is below
            // `tail` the instant it lands.
            if res_id == ring_res_id {
                continue;
            }
            // **Everything else C holds is synchronised, including Venus's own shmems.**
            //
            // This used to ship only application memory, on ring-findings §6's `blob_id` signal, and
            // that is why `vkExecuteCommandStreamsMESA` could not be relayed: when a submission
            // exceeds Mesa's `direct_size` (`buffer_size >> 4`, so 8 KiB for the 128 KiB ring), the
            // commands do not go in the ring at all — they go into the **staging pool**, which is
            // `blob_id == 0`, which this loop skipped. S then held a pool of zeros and the referenced
            // streams were simply absent, so `rayland-c` refused the delta rather than let S execute
            // whatever its copy happened to contain.
            //
            // # Why publishing a region S also writes is safe now, when it was not before
            // The reply arena is `blob_id == 0` too, and S writes it. The old comment here was right
            // that publishing C's stale copy of such a region is a clobber — and right that the
            // answer was "a design for synchronising a region *both* sides write". That design now
            // exists and is what this rests on: every blob carries a **baseline**, `take_changed_runs`
            // ships only bytes that differ from it, and `LocalBlob::note_s_wrote` folds each S→C write
            // into the baseline as it arrives (`main.rs`'s `apply_blob_data` and
            // `relay_engine.rs`'s `commit_pending_blob`, both teeth-checked). So S's own writes never
            // look like C-side changes and are never echoed back. What C publishes is exactly what
            // C's Mesa wrote, which for the arena is nothing.
            //
            // # The cost, and why it is affordable
            // A baseline for the 8 MiB pool, and a chunked compare of it per relay. The compare is
            // `memcmp`-shaped (see `LocalBlob::take_changed_runs`) and the steady state is "nothing
            // changed", so it costs a vectorised scan and no traffic. Shipping *whole* blobs here,
            // which an earlier experiment did, tripled C's message count and slowed the relay enough
            // that the application never reached the submit under test.
            //
            // **MEASURED 2026-09-01, and the sentence above turned out to be optimistic.** "A
            // vectorised scan" is 13.2 MiB for `vkcube`, walked on *every* ring delta, about three
            // times a frame. On the riscv64 board that is **8.78 ms per delta — 30.8 ms of a 50 ms
            // frame, 62% of it.** It is the single largest term in frame time on a weak C, and the
            // skip immediately below is the largest safe reduction available without kernel support.

            // **A presented blob is not diffed at all**, which is a correctness statement first and a
            // saving second.
            //
            // Correctness: a presented blob is one C published to S as a `BufferToken`, so S's GPU
            // renders into it and — since the 2026-08-29 presented-buffer exclusion — never reports
            // those writes back. C's baseline for it is therefore stale **by design**, and anything C
            // shipped for it would lay C's old news over S's freshly rendered pixels. C must never
            // ship one, so there is nothing to learn by diffing one.
            //
            // Saving: for `vkcube` the four swapchain images are **4 MiB of the 13.2 MiB** walked per
            // delta — about a third of the walk spent re-establishing, three times a frame, that the
            // application has not CPU-written pixels only the other machine's GPU ever writes.
            //
            // This changes no output. A full-run census on 2026-08-31 recorded C→S `BlobData` for
            // resources 3, 4, 5 and 6 only, never for 7–10, the four swapchain images. It removes
            // work, not messages — and `a_presented_blob_is_never_shipped_even_when_its_bytes_changed`
            // pins the correctness half against a blob whose bytes really did change.
            if blob.is_presented() {
                continue;
            }
            blob.ensure_baseline();
            // **How near two changed runs must be for this direction to merge them.**
            //
            // Measured 2026-08-31: one loopback `vkcube` run sent 5,495 `C2S::BlobData` of which
            // **5,409 carried one to three bytes each**, almost all of them the 8 MiB staging pool
            // fragmenting under a byte-granular diff — about a second of wall clock inside C's
            // `send()`, per run, to move a few kilobytes. The forward path is message-rate bound for
            // the same reason the return path was, and this is the same remedy.
            //
            // 256 matches the value S's readback path already uses, so the two directions do not
            // acquire different magic numbers for the same trade.
            const FORWARD_COALESCE_GAP: usize = 256;
            // **A presented blob keeps the strict byte grain, and this is the safety condition.**
            //
            // Coalescing re-ships the unchanged bytes in a gap, taken from C's baseline — safe only
            // while that baseline is a faithful model of S's copy. It is faithful because S reports
            // every byte it writes and `note_s_wrote` folds it in, with exactly one exception: S
            // excludes **presented** resources from its return path (added 2026-08-29, when S was
            // found shipping ~877 KB of rendered frame per second back to a machine with no display).
            // For those, S's GPU writes and never tells C, so C's baseline is stale by design and a
            // re-shipped gap byte would overwrite S's freshly rendered pixels with C's old news.
            //
            // **The presented exception that used to live here is now handled above, by not diffing
            // such a blob at all.** Until 2026-09-01 this chose a zero gap for a presented blob, so
            // that coalescing could not re-ship C's stale copy of bytes S had rendered. Skipping the
            // blob outright is strictly stronger — nothing is shipped rather than nothing extra — so
            // this branch became unreachable and is gone rather than left as reassuring dead code.
            // The safety argument itself is unchanged and now lives at the skip.
            let gap = FORWARD_COALESCE_GAP;
            // Attribute this one blob's scan (see `blobscan`). Two clock reads, gated.
            let scan_start = blobscan::enabled().then(std::time::Instant::now);
            let runs = blob.take_changed_runs(gap);
            if let Some(t0) = scan_start {
                // Stop the clock BEFORE the probe's own accounting. Until 2026-09-02 `elapsed()`
                // was evaluated after the `sum()` below, so the attributed scan time included the
                // instrument -- immaterial against a 10.85 ms scan, but this module's own doc
                // claims it costs "two Instant reads and one map update", and it sits three lines
                // from a comment recording four earlier instruments caught changing what they
                // measured. An instrument that quietly contradicts its own cost note is the thing
                // that comment exists to warn about.
                let elapsed_ns = t0.elapsed().as_nanos() as u64;
                let changed: usize = runs.iter().map(|r| r.bytes.len()).sum();
                blobscan::record(
                    res_id,
                    blob.size() as usize,
                    elapsed_ns,
                    runs.len(),
                    changed,
                    blob.is_application_memory(),
                );
            }
            // Each run reuses `BlobData`'s offset field; v1 only ever used offset 0 (the whole blob).
            for run in runs {
                out.push(C2S::BlobData {
                    res_id,
                    offset: run.offset,
                    bytes: run.bytes,
                });
            }
        }
    }

    // One delta's worth of per-blob scans is now recorded (see `blobscan`).
    if blobscan::enabled() {
        blobscan::end_of_delta();
    }

    // **Last, always.** Everything above must be on S before the commands that may read it. See the
    // module docs: S's ring thread runs asynchronously the instant this delta's `tail` lands.
    out.push(C2S::RingDelta {
        ring_res_id,
        tail: delta.tail,
        bytes: delta.bytes,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::LocalBlob;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// The live capture's blob ids (ring-findings §6), so the tests speak in the real session's
    /// terms rather than invented ones.
    ///
    /// `blob_id == 0` marks Venus's internal shmems; the rest are the application's `VkDeviceMemory`.
    const RING_BLOB_ID: u64 = 0;
    /// The reply arena's `blob_id`: Venus-internal, like every one of its own shmems.
    const REPLY_ARENA_BLOB_ID: u64 = 0;
    /// The app's vertex buffer (`res=3`, 64 bytes) — the one that decodes float-for-float.
    const VERTEX_BUFFER_BLOB_ID: u64 = 16;
    /// The app's readback buffer (`res=6`, 16384 bytes) — the one that carries the picture back.
    const READBACK_BLOB_ID: u64 = 18;

    /// Build a blob table from `(res_id, blob_id, size, fill)` 4-tuples, filling each blob with a
    /// recognizable byte so a test can tell whose bytes arrived.
    fn table_of(blobs: &[(u32, u64, u64, u8)]) -> BlobTable {
        let mut map = HashMap::new();
        for &(res_id, blob_id, size, fill) in blobs {
            let (mut blob, _fd) = LocalBlob::create(blob_id, size).expect("a local blob");
            blob.bytes_mut().fill(fill);
            map.insert(res_id, blob);
        }
        Arc::new(Mutex::new(map))
    }

    /// A ring delta standing in for one the watcher drained.
    fn a_delta() -> RingDelta {
        RingDelta {
            // The reference session's first frontier: 4024 bytes carried its whole Vulkan
            // initialization (ring-findings §2).
            tail: 4024,
            bytes: vec![0xaa; 4024],
        }
    }

    /// **The task's central assertion: the app's memory must reach S before the commands that read
    /// it.**
    ///
    /// C0 Task 4b caught the reference app's vertex buffer (`res=3`, 64 bytes) decoding
    /// float-for-float out of a mapped blob. The app writes it with a plain `memcpy` and **no API
    /// call to intercept**, so if it is not on S before S's GPU reads, the triangle renders from
    /// uninitialized memory.
    ///
    /// The ordering — not merely the presence — is the property. S's ring thread polls, and
    /// dispatches the delta's commands the instant `tail` moves (`vkr_ring.c:262-266`), so a delta
    /// that arrives first can be executed against vertex memory that is still zeros. That failure is
    /// timing-dependent and would present as an intermittently wrong frame with nothing naming the
    /// cause.
    #[test]
    fn the_app_s_blobs_are_shipped_before_the_ring_delta_that_may_read_them() {
        let blobs = table_of(&[
            (1, RING_BLOB_ID, 131268, 0x11),
            (3, VERTEX_BUFFER_BLOB_ID, 64, 0x33),
        ]);

        let msgs = messages_for_delta(&blobs, 1, a_delta());

        // The vertex buffer must be there at all — without it the triangle is undefined.
        let vertex_at = msgs
            .iter()
            .position(|m| matches!(m, C2S::BlobData { res_id: 3, .. }))
            .expect("the app's vertex buffer must be shipped; without it S renders from zeros");
        let delta_at = msgs
            .iter()
            .position(|m| matches!(m, C2S::RingDelta { .. }))
            .expect("the delta itself must still be sent");

        assert!(
            vertex_at < delta_at,
            "the vertex buffer must be on S before the delta whose commands may read it, but the \
             delta was sent first (blob at {vertex_at}, delta at {delta_at}); S's ring thread \
             dispatches the moment `tail` moves, so it would draw from memory C had not yet shipped"
        );
    }

    /// The blob's **contents** must actually cross, not just its name. A message carrying the right
    /// id and the wrong bytes would pass an ordering test and still render the wrong triangle.
    #[test]
    fn the_shipped_blob_carries_the_application_s_actual_bytes() {
        let blobs = table_of(&[(3, VERTEX_BUFFER_BLOB_ID, 64, 0x33)]);

        let msgs = messages_for_delta(&blobs, 1, a_delta());

        let blob = msgs
            .iter()
            .find_map(|m| match m {
                C2S::BlobData {
                    res_id: 3,
                    offset,
                    bytes,
                } => Some((offset, bytes)),
                _ => None,
            })
            .expect("the vertex buffer");
        assert_eq!(
            *blob.0, 0,
            "a uniformly-filled blob diffed against a zero baseline is exactly one run \
             covering the whole blob, so it still starts at offset 0"
        );
        assert_eq!(
            blob.1,
            &vec![0x33u8; 64],
            "the bytes Mesa wrote into the mapping are what must reach S's GPU"
        );
    }

    /// **The ring must never be shipped as `BlobData`**, and this is not tidiness.
    ///
    /// The ring has its own message, [`C2S::RingDelta`], which carries the `tail` that makes its
    /// bytes meaningful and is deliberately sent **last**. A copy sent as `BlobData` would fight the
    /// delta for the same bytes, publish ring contents ahead of the `tail` that validates them — S's
    /// ring thread reads whatever is below `tail` the instant it lands — and overwrite the `head` and
    /// `status` words S's virglrenderer owns.
    ///
    /// Everything *else* C holds now crosses, Venus's own shmems included. That is a deliberate
    /// widening (see this module's docs): the staging pool is `blob_id == 0`, and it is where Mesa
    /// puts a submission too large to inline, so a relay that skipped it could not carry
    /// `vkExecuteCommandStreamsMESA` at all. What makes publishing a region **S also writes** safe is
    /// the baseline, not the `blob_id` — see [`the_reply_arena_is_not_echoed_back_to_s`].
    #[test]
    fn the_ring_is_never_shipped_as_blob_data() {
        let blobs = table_of(&[
            (1, RING_BLOB_ID, 131268, 0x11),
            (2, REPLY_ARENA_BLOB_ID, 1048576, 0x22),
            (3, VERTEX_BUFFER_BLOB_ID, 64, 0x33),
        ]);

        let msgs = messages_for_delta(&blobs, 1, a_delta());

        let shipped: Vec<u32> = msgs
            .iter()
            .filter_map(|m| match m {
                C2S::BlobData { res_id, .. } => Some(*res_id),
                _ => None,
            })
            .collect();
        assert!(
            !shipped.contains(&1),
            "the ring must never cross as BlobData: RingDelta carries it, and a BlobData copy would \
             publish ring bytes ahead of the tail that validates them and clobber the head/status \
             words S's virglrenderer owns. shipped: {shipped:?}"
        );
        assert!(
            shipped.contains(&3),
            "the application's own memory must still cross. shipped: {shipped:?}"
        );
        assert!(
            shipped.contains(&2),
            "Venus's non-ring shmems must now cross too — the staging pool is blob_id == 0 and is \
             where an over-sized submission's commands actually live. shipped: {shipped:?}"
        );
    }

    /// **The property that makes publishing a region S also writes safe: S's own bytes are never
    /// echoed back to it.**
    ///
    /// This is the guard that replaced `blob_id` routing for C→S, and it is the whole reason the
    /// reply arena can now be in the sync at all. The arena is written by **S** and read by C — it is
    /// how every synchronous Vulkan call gets its answer (ring-findings §7 measured it at ~12x the
    /// command traffic). Shipping C's copy of it back would clobber replies the application is
    /// blocked on, which is a corruption bug, not a wasted byte.
    ///
    /// What prevents that is not the filter but the **baseline**: [`LocalBlob::note_s_wrote`] folds
    /// each S→C write into it as the write is applied, so those bytes are already "what S has" by the
    /// time the next diff runs and never appear as a C-side change. This test states that directly —
    /// write into the mapping exactly as S's reply path does, and require the next relay to carry
    /// nothing for it.
    #[test]
    fn the_reply_arena_is_not_echoed_back_to_s() {
        const ARENA_RES: u32 = 2;
        const ARENA_SIZE: u64 = 4096;
        let blobs = table_of(&[(ARENA_RES, REPLY_ARENA_BLOB_ID, ARENA_SIZE, 0x00)]);

        // First relay establishes the baseline and drains whatever the initial state was.
        let _ = messages_for_delta(&blobs, 1, a_delta());

        // Now S answers a synchronous call: its reply lands in the arena, through the same path
        // `apply_blob_data` and `commit_pending_blob` use — write the bytes, then record them.
        {
            let mut table = blobs.lock().expect("the blob table lock is never poisoned");
            let blob = table
                .get_mut(&ARENA_RES)
                .expect("the arena is in the table");
            blob.bytes_mut()[64..96].copy_from_slice(&[0xEE; 32]);
            blob.note_s_wrote(64, &[0xEE; 32]);
        }

        let msgs = messages_for_delta(&blobs, 1, a_delta());
        let arena_runs: Vec<&C2S> = msgs
            .iter()
            .filter(|m| matches!(m, C2S::BlobData { res_id, .. } if *res_id == ARENA_RES))
            .collect();
        assert!(
            arena_runs.is_empty(),
            "S's own reply must never be shipped back to S — note_s_wrote folds it into the \
             baseline so it is not a C-side change. Got: {arena_runs:?}"
        );
    }

    /// Every application blob crosses, not just the first one found. The readback buffer matters as
    /// much as the vertex buffer: v1 has no way to know which of the app's blobs a given delta's
    /// commands touch, which is exactly why the sync is conservative (spec §7). This holds here because
    /// both blobs are freshly filled against a zero baseline, so both are "changed"; under the diff, a
    /// blob that had not changed since its last relay would rightly be absent, which is a different
    /// test (`an_unchanged_blob_is_not_reshipped_on_the_next_relay`) below.
    #[test]
    fn every_application_blob_is_shipped_not_merely_one() {
        let blobs = table_of(&[
            (1, RING_BLOB_ID, 131268, 0x11),
            (3, VERTEX_BUFFER_BLOB_ID, 64, 0x33),
            (6, READBACK_BLOB_ID, 16384, 0x66),
        ]);

        let msgs = messages_for_delta(&blobs, 1, a_delta());

        let mut shipped: Vec<u32> = msgs
            .iter()
            .filter_map(|m| match m {
                C2S::BlobData { res_id, .. } => Some(*res_id),
                _ => None,
            })
            .collect();
        // The table is a HashMap, so iteration order is arbitrary; only the *set* is specified, and
        // only the blobs-before-delta boundary is ordered.
        shipped.sort_unstable();
        assert_eq!(shipped, vec![3, 6]);
    }

    /// The delta itself must survive intact — same `tail`, same bytes, same ring. It is the payload
    /// the whole sub-project exists to move, and a blob sync that mangled it would be worse than no
    /// blob sync at all.
    #[test]
    fn the_ring_delta_reaches_s_unaltered() {
        let blobs = table_of(&[(3, VERTEX_BUFFER_BLOB_ID, 64, 0x33)]);

        let msgs = messages_for_delta(&blobs, 7, a_delta());

        assert_eq!(
            msgs.last(),
            Some(&C2S::RingDelta {
                ring_res_id: 7,
                tail: 4024,
                bytes: vec![0xaa; 4024],
            }),
            "the delta must be last, and must carry exactly what the watcher drained"
        );
    }

    /// A session with no application blobs yet — everything before the app's first `vkAllocateMemory`
    /// — must still relay its delta. The whole Vulkan initialization happens in this state, and a
    /// sync that swallowed those deltas would hang the application before it ever drew anything.
    #[test]
    fn a_delta_with_no_application_blobs_yet_is_still_relayed() {
        let blobs = table_of(&[(1, RING_BLOB_ID, 131268, 0x11)]);

        let msgs = messages_for_delta(&blobs, 1, a_delta());

        assert_eq!(
            msgs.len(),
            1,
            "nothing to sync, so the delta alone: {msgs:?}"
        );
        assert!(matches!(msgs[0], C2S::RingDelta { .. }));
    }

    /// **A presented blob is never diffed, even when its bytes have changed.**
    ///
    /// This is the stronger half of the presented rule. `nearby_runs_coalesce_except_on_a_presented_blob`
    /// asserts that a presented blob keeps the byte grain; this asserts it is not shipped *at all*,
    /// which is what makes skipping the diff for it correct rather than merely faster.
    ///
    /// The scenario is the one that would corrupt a frame: S has rendered into the blob (so C's
    /// baseline is stale by design and C's copy is meaningless), and C's mapping then differs from
    /// that baseline. A diff would see a change and ship it, overwriting S's pixels with C's copy.
    #[test]
    fn a_presented_blob_is_never_shipped_even_when_its_bytes_changed() {
        let blobs = table_of(&[(7, VERTEX_BUFFER_BLOB_ID, 4096, 0x00)]);
        {
            let mut table = blobs.lock().expect("the blob table lock");
            let blob = table.get_mut(&7).expect("res 7");
            blob.ensure_baseline();
            let _ = blob.take_changed_runs(0);
            // Now mark it presented and change every byte. A blob that is diffed would ship 4096
            // bytes; a blob that is skipped ships nothing.
            blob.note_presented();
            blob.bytes_mut().fill(0xEE);
        }
        let out = messages_for_delta(&blobs, 1, a_delta());
        let shipped: Vec<&C2S> = out
            .iter()
            .filter(|m| matches!(m, C2S::BlobData { res_id: 7, .. }))
            .collect();
        assert!(
            shipped.is_empty(),
            "a presented blob must never be shipped: S renders into it and never reports those \
             writes, so C's copy is stale by design and shipping it would overwrite S's pixels"
        );
        // The ring delta itself must still be relayed — skipping a blob must not skip the frame.
        assert!(
            out.iter().any(|m| matches!(m, C2S::RingDelta { .. })),
            "the ring delta must still cross"
        );
    }

    /// **The forward path coalesces nearby changed runs — and a presented blob ships nothing.**
    ///
    /// Both halves are asserted in one test on purpose: the interesting claim is not "coalescing
    /// works" but that it is *withheld* exactly where it would be unsafe, and that is only visible as
    /// a difference between two blobs in the same relay.
    ///
    /// Each blob gets two changed bytes separated by a short unchanged gap. For an ordinary blob the
    /// gap rides along and one `BlobData` crosses, because S reports every byte it writes and C's
    /// baseline is a true model of S's copy — so re-shipping a gap byte writes what S already holds.
    /// A **presented** blob is not diffed at all: S renders into it and never reports those writes,
    /// so C's baseline is stale by design and *any* byte C shipped would overwrite S's pixels.
    ///
    /// Until 2026-09-01 the presented blob was diffed with a zero coalescing gap, and this test
    /// asserted two byte-granular runs. Skipping it outright is strictly stronger, so the expectation
    /// here moved with it — recorded because a test that is merely *edited* to match new behaviour is
    /// how a weakened guard slips through, and this one was strengthened.
    #[test]
    fn nearby_runs_coalesce_except_on_a_presented_blob() {
        // Two application blobs, identical in every way but the mark.
        let blobs = table_of(&[
            (5, VERTEX_BUFFER_BLOB_ID, 64, 0x00),
            (6, VERTEX_BUFFER_BLOB_ID, 64, 0x00),
        ]);
        {
            let mut table = blobs.lock().expect("the blob table lock");
            // The same two-changes-with-a-gap pattern in each: bytes 0 and 4 change, 1..4 do not.
            for id in [5u32, 6u32] {
                let blob = table.get_mut(&id).expect("the blob");
                blob.ensure_baseline();
                // Drain the birth diff first, so what the relay below sees is only this edit.
                let _ = blob.take_changed_runs(0);
                blob.bytes_mut()[0] = 0x11;
                blob.bytes_mut()[4] = 0x22;
            }
            // `res=6` is a swapchain image C has published as a BufferToken. Marked *after* its bytes
            // changed, so the change is genuinely present and genuinely withheld.
            table.get_mut(&6).expect("res 6").note_presented();
        }

        let out = messages_for_delta(&blobs, 1, a_delta());
        let runs_for = |res: u32| -> Vec<(u64, usize)> {
            out.iter()
                .filter_map(|m| match m {
                    C2S::BlobData {
                        res_id,
                        offset,
                        bytes,
                    } if *res_id == res => Some((*offset, bytes.len())),
                    _ => None,
                })
                .collect()
        };

        // The ordinary blob: one run covering 0..5, the 3-byte gap re-shipped.
        assert_eq!(
            runs_for(5),
            vec![(0, 5)],
            "an ordinary blob's two nearby changes must coalesce into one BlobData"
        );
        // The presented blob: nothing at all. THIS is the safety property; if it ever fails,
        // presented frames corrupt.
        assert_eq!(
            runs_for(6),
            Vec::new(),
            "a presented blob must not be shipped at all — S renders into it and never reports \
             those writes, so C's copy is stale by design"
        );
    }

    /// **A gap wider than the threshold is not merged, on any blob.**
    ///
    /// The companion to the test above: coalescing is bounded, so a blob with two genuinely distant
    /// changes still ships two runs rather than the megabyte between them. Without this, "coalescing
    /// works" could be satisfied by a broken implementation that merges everything.
    #[test]
    fn changes_farther_apart_than_the_threshold_stay_separate() {
        let blobs = table_of(&[(5, VERTEX_BUFFER_BLOB_ID, 4096, 0x00)]);
        {
            let mut table = blobs.lock().expect("the blob table lock");
            let blob = table.get_mut(&5).expect("the blob");
            blob.ensure_baseline();
            let _ = blob.take_changed_runs(0);
            // 0 and 1000: a 999-byte gap, far past the 256-byte threshold.
            blob.bytes_mut()[0] = 0x11;
            blob.bytes_mut()[1000] = 0x22;
        }
        let out = messages_for_delta(&blobs, 1, a_delta());
        let runs: Vec<(u64, usize)> = out
            .iter()
            .filter_map(|m| match m {
                C2S::BlobData { offset, bytes, .. } => Some((*offset, bytes.len())),
                _ => None,
            })
            .collect();
        assert_eq!(
            runs,
            vec![(0, 1), (1000, 1)],
            "a 999-byte unchanged gap is past the threshold and must not be re-shipped"
        );
    }

    /// The point of the whole change: a blob that has not changed since the last relay must not be
    /// re-shipped. Relaying twice, the second relay carries only the ring delta.
    #[test]
    fn an_unchanged_blob_is_not_reshipped_on_the_next_relay() {
        let blobs = table_of(&[(3, VERTEX_BUFFER_BLOB_ID, 64, 0x33)]);

        // First relay: the vertex buffer's content crosses (baseline was zeros).
        let first = messages_for_delta(&blobs, 1, a_delta());
        assert!(
            first
                .iter()
                .any(|m| matches!(m, C2S::BlobData { res_id: 3, .. })),
            "the first relay must ship the app blob's content"
        );

        // Second relay, nothing written in between: the blob must not cross again.
        let second = messages_for_delta(&blobs, 1, a_delta());
        assert!(
            !second.iter().any(|m| matches!(m, C2S::BlobData { .. })),
            "an unchanged blob must not be re-shipped; only the ring delta should cross"
        );
        assert!(
            second.iter().any(|m| matches!(m, C2S::RingDelta { .. })),
            "the ring delta must still cross on every relay"
        );
    }
}

/// Fingerprint every blob C holds, for comparison against S's copy of the same resources.
///
/// # Why this exists
/// The ring relay is measured byte-exact — 253 deltas, 253 matching digests — yet S either refuses
/// the application's `vkQueueSubmit` with a decoder error or executes it into a fence that never
/// signals, and it does so **intermittently**. Since the command bytes provably arrive intact, what
/// the submit *references* is the remaining suspect: the swapchain images S builds from WP0 buffer
/// tokens, and the staging pool this very module declines to publish. Fingerprinting both sides'
/// blobs and joining on `tail` says which resource, if any, they disagree about.
///
/// # Inputs / outputs
/// - `table`: C's blob shadows, already locked by the caller.
/// - `tail`: the ring frontier of the delta being relayed — the join key against S's log, exactly as
///   the ring fingerprints used.
/// - Prints one line per blob per interval. **No-op unless `RAYLAND_C1_BLOB_FP` is set.**
///
/// # Pitfall: the two sides sample at different instants, and that is expected
/// C and S cannot sample simultaneously, and the application writes its mapped memory continuously
/// with no call to intercept — so a *transient* disagreement on an application blob means only that
/// a write landed between the samples. **A persistent disagreement is the signal**, and a
/// disagreement on a blob the application never writes is a stronger one still.
///
/// # Pitfall: this is deliberately its own switch
/// It hashes several megabytes per sample, so it is throttled and gated separately from
/// `RAYLAND_C1_METRICS` and `RAYLAND_RING_DUMP`. An instrument that changes the timing of the thing
/// it measures is how the previous wall stayed misread for two days.
fn blob_fingerprints(table: &std::collections::HashMap<u32, crate::shm::LocalBlob>, tail: u32) {
    if std::env::var_os("RAYLAND_C1_BLOB_FP").is_none() {
        return;
    }
    // Throttled: hashing megabytes on every relay would dominate the relay itself.
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    static LAST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    /// Twice a second — dense enough to bracket a submit, sparse enough not to distort the relay.
    const INTERVAL_MS: u64 = 500;
    let now_ms = BASE
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64;
    if now_ms < LAST_MS.load(std::sync::atomic::Ordering::Relaxed) + INTERVAL_MS {
        return;
    }
    LAST_MS.store(now_ms, std::sync::atomic::Ordering::Relaxed);

    for (&res_id, blob) in table.iter() {
        let bytes = blob.bytes();
        // **Cheap by construction, and it has to be.** The first version hashed every byte of every
        // blob (~11 MiB) twice a second while holding this table's lock, and measurably stopped the
        // application from reaching its swapchain buffers at all — 36 proxy-trace lines against 52
        // with the instrument off, 5 runs to 3. The relay path is that latency-sensitive. So the
        // zero regions, which are the overwhelming majority, are skipped with chunk compares that
        // lower to `memcmp`, and only non-zero content is hashed.
        let (nonzero, digest) = sparse_fingerprint(bytes);
        eprintln!(
            "[c-blobfp] tail={tail} res={res_id} len={} nonzero={nonzero} fnv={:016x}",
            bytes.len(),
            digest
        );
    }
}

/// A blob fingerprint that skips zero regions: `(non-zero byte count, digest of the non-zero bytes)`.
///
/// # Why not simply hash the blob
/// Because hashing 11 MiB of mostly-zero memory twice a second, under a lock the relay needs, changed
/// the behaviour of the system being measured — the application stopped reaching its swapchain
/// buffers entirely. Zero regions carry no information here: S's copy of a blob starts as a
/// zero-filled memfd, so agreement on the zeros is the default rather than evidence. Comparing a
/// chunk against zeros with slice equality lowers to `memcmp`, so the common case costs a vectorised
/// scan and no hashing at all.
///
/// # Inputs / outputs
/// - `bytes`: the blob's live contents.
/// - Returns the count of non-zero bytes and an FNV-1a digest **of only those bytes, in order**. Two
///   blobs agreeing on both agree on their content, since the zeros are implied by the length.
fn sparse_fingerprint(bytes: &[u8]) -> (usize, u64) {
    const CHUNK: usize = 64;
    const ZEROS: [u8; CHUNK] = [0u8; CHUNK];
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut nonzero = 0usize;
    let mut hash = OFFSET_BASIS;
    let mut i = 0usize;
    while i < bytes.len() {
        let end = (i + CHUNK).min(bytes.len());
        // The whole point: an all-zero chunk is dismissed by one vectorised compare.
        if bytes[i..end] == ZEROS[..end - i] {
            i = end;
            continue;
        }
        for &b in &bytes[i..end] {
            if b != 0 {
                nonzero += 1;
                hash ^= b as u64;
                hash = hash.wrapping_mul(PRIME);
            }
        }
        i = end;
    }
    (nonzero, hash)
}
