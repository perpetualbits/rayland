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
            // Venus's own shmems — the ring, the reply arena, the staging pool — are not C's to
            // publish. Ring-findings §6's `blob_id` signal is the line between the application's memory
            // and the transport's plumbing; `take_changed_runs` also returns empty for them (no
            // baseline), so this filter is the intent and that is the backstop.
            if !blob.is_application_memory() {
                // DIAGNOSTIC (`RAYLAND_C1_SHIP_BLOB=<res_id>`), and **emphatically not a fix.** The
                // blob fingerprints showed exactly one structural divergence between the two
                // machines when a submit fails to complete: the staging pool holds content on C and
                // is all zeros on S, because this filter declines to publish `blob_id == 0` by
                // design. Naming the resource explicitly — rather than inferring "the staging pool"
                // from its size or its id — keeps this a stated experiment rather than a guess.
                //
                // **Why this must not become the fix:** S writes the reply arena, which is
                // `blob_id == 0` too, and C's copy of it is stale by construction. Publishing such a
                // region from C is the clobber `blob_id` routing exists to prevent. If this
                // experiment shows the submit completing, the answer is a design for synchronising a
                // region *both* sides write — not this switch left on.
                if diagnostic_ship_blob() != Some(res_id) {
                    continue;
                }
                // Only the non-zero runs: the pool is 8 MiB of which a handful of bytes are set, and
                // re-shipping megabytes per relay would resurrect the bandwidth wall this session
                // just removed and make the experiment unreadable.
                for (start, end) in nonzero_runs(blob.bytes()) {
                    out.push(C2S::BlobData {
                        res_id,
                        offset: start as u64,
                        bytes: blob.bytes()[start..end].to_vec(),
                    });
                }
                continue;
            }
            // Ship only what changed since C last sent this blob — nothing at all if it is unchanged.
            // Each run reuses `BlobData`'s offset field; v1 only ever used offset 0 (the whole blob).
            for run in blob.take_changed_runs() {
                out.push(C2S::BlobData {
                    res_id,
                    offset: run.offset,
                    bytes: run.bytes,
                });
            }
        }
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
            *blob.0,
            0,
            "a uniformly-filled blob diffed against a zero baseline is exactly one run \
             covering the whole blob, so it still starts at offset 0"
        );
        assert_eq!(
            blob.1,
            &vec![0x33u8; 64],
            "the bytes Mesa wrote into the mapping are what must reach S's GPU"
        );
    }

    /// **Venus's internal shmems must never be shipped C→S**, and this is not tidiness.
    ///
    /// The reply arena is written by **S** and read by C — it is how every synchronous Vulkan call
    /// gets its answer (ring-findings §7 measured it at ~12x the command traffic). Shipping C's
    /// stale copy of it to S would clobber replies S had already written and the application is
    /// blocked on. The ring likewise: the delta carries it, and a copy of it sent as `BlobData`
    /// would fight the delta for the same bytes while overwriting the `head` and `status` words
    /// S's virglrenderer owns.
    ///
    /// So "sync the application's memory" cannot mean "everything, both ways" — that is not what
    /// the filter is for, and getting it wrong here is a corruption bug, not merely a wasted byte.
    /// `blob_id` (ring-findings §6) is the line.
    #[test]
    fn venus_s_own_shmems_are_never_shipped_c_to_s() {
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
        assert_eq!(
            shipped,
            vec![3],
            "only the application's own memory may cross C->S; shipping C's stale reply arena would \
             destroy the replies S wrote and the application is blocked on, and shipping the ring \
             would clobber the head/status words S's virglrenderer owns"
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

    /// The point of the whole change: a blob that has not changed since the last relay must not be
    /// re-shipped. Relaying twice, the second relay carries only the ring delta.
    #[test]
    fn an_unchanged_blob_is_not_reshipped_on_the_next_relay() {
        let blobs = table_of(&[(3, VERTEX_BUFFER_BLOB_ID, 64, 0x33)]);

        // First relay: the vertex buffer's content crosses (baseline was zeros).
        let first = messages_for_delta(&blobs, 1, a_delta());
        assert!(
            first.iter().any(|m| matches!(m, C2S::BlobData { res_id: 3, .. })),
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
        // The digest answers "are these the same bytes"; the non-zero count answers "is this blob
        // populated at all", which distinguishes *diverged* from *never filled in* — and the latter
        // is what a resource S built from a token but never received contents for would look like.
        let nonzero = bytes.iter().filter(|&&b| b != 0).count();
        eprintln!(
            "[c-blobfp] tail={tail} res={res_id} len={} nonzero={nonzero} fnv={:016x}",
            bytes.len(),
            fnv1a_blob(bytes)
        );
    }
}

/// FNV-1a over a blob's bytes. Duplicated from the ring fingerprint on purpose — see the S-side
/// twin's docs: agreement between two independent implementations over two independent buffers is
/// the property being tested, and a shared helper would weaken what a match proves.
fn fnv1a_blob(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// The resource id the staging-pool experiment names, from `RAYLAND_C1_SHIP_BLOB`.
///
/// # Why an explicit id rather than "the staging pool"
/// C cannot identify the staging pool from anything it knows: `blob_id == 0` covers the ring, the
/// reply arena **and** the pool alike, and picking it out by size would be exactly the kind of guess
/// that has cost this investigation days. Making the operator name the resource turns the experiment
/// into a stated hypothesis with a stated subject.
///
/// Returns `None` — the default, and the only safe standing value — unless the variable parses as a
/// resource id. A malformed value is treated as unset rather than refused: this is a diagnostic, and
/// failing a render session over a typo in a debug switch would be the wrong trade.
fn diagnostic_ship_blob() -> Option<u32> {
    std::env::var("RAYLAND_C1_SHIP_BLOB").ok()?.parse().ok()
}

/// The half-open ranges of every run of non-zero bytes in `bytes`.
///
/// # Why non-zero runs rather than the whole blob
/// The blob this exists for is 8 MiB with a few dozen bytes set. Shipping it whole on every relay
/// would re-create the resend flood that made vkcube look like a hang, and would swamp the very
/// measurement the experiment is trying to read. Shipping the non-zero runs sends the same
/// information: S's copy starts as a zero-filled memfd, so zero regions already agree.
///
/// # Pitfall: this is *not* a diff, and is only sound because S's copy starts empty
/// It ships what is non-zero, not what changed, so it cannot un-set a byte S already holds. That is
/// acceptable for a one-run experiment against a blob S never writes and never had content in, and
/// it would **not** be acceptable as a synchronisation mechanism — another reason this path is not a
/// candidate fix.
///
/// # Inputs / outputs
/// - `bytes`: the blob's live contents.
/// - Returns ascending, non-empty, non-adjacent `(start, end)` ranges; empty when all bytes are zero.
fn nonzero_runs(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    // Where the run currently being accumulated began, if one is open.
    let mut open: Option<usize> = None;
    // **Chunked, for the same reason the S-side diff is.** The first version of this walked all
    // 8 MiB a byte at a time on every relay, and measurably slowed C's relay enough that the
    // application got *less* far than without the experiment — an instrument distorting the thing it
    // measures, which is exactly the failure this session spent two days on at the other end of the
    // wire. Comparing a chunk against zeros with slice equality lowers to `memcmp`; the per-byte loop
    // then runs only inside a chunk that is not all zeros. The runs produced are identical.
    const CHUNK: usize = 64;
    const ZEROS: [u8; CHUNK] = [0u8; CHUNK];
    let mut i = 0usize;
    while i < bytes.len() {
        let chunk_end = (i + CHUNK).min(bytes.len());
        if bytes[i..chunk_end] == ZEROS[..chunk_end - i] {
            // An all-zero chunk closes any open run at the chunk's start, exactly as the first zero
            // byte would have.
            if let Some(start) = open.take() {
                runs.push((start, i));
            }
            i = chunk_end;
            continue;
        }
        for j in i..chunk_end {
            if bytes[j] != 0 {
                open.get_or_insert(j);
            } else if let Some(start) = open.take() {
                runs.push((start, j));
            }
        }
        i = chunk_end;
    }
    // A run reaching the final byte is closed by the blob's end, not by a zero after it.
    if let Some(start) = open {
        runs.push((start, bytes.len()));
    }
    runs
}
