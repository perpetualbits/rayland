# Incremental C→S blob sync — send only what changed — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop `rayland-c` re-shipping unchanged application blobs whole on every ring relay; ship only the byte-runs that changed since C last sent them, and nothing for an unchanged blob.

**Architecture:** Each application `LocalBlob` gains a **baseline** — C's copy of what S currently holds. On each relay, `messages_for_delta` diffs the live mapping against the baseline in one pass, emits a `C2S::BlobData` per changed run (reusing the existing `offset` field — no wire change), and updates the baseline. When S's own writes arrive over the return path, `apply_blob_data` folds them into the baseline so C never ships S's bytes back. This mirrors the proven S→C diff (Task 5b).

**Tech Stack:** Rust. `rayland-c` crate. `rayland_relay::{C2S, BlobRun}` (already a dependency). No new dependencies, no wire changes.

**Spec:** `docs/design/2026-07-25-c1-incremental-blob-sync.md`. Read it first.

## Global Constraints

- **Language: Rust for all code.** (CLAUDE.md locked decision.)
- **Build target (mandatory):** prefix every cargo invocation with `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target`. The default `/tmp` target is a tmpfs with a per-user quota; filling it makes the linker die with a bare `SIGBUS` (`collect2: ld terminated with signal 7`).
- **MSRV floor 1.85:** no let-chains or other >1.85 syntax. `cargo +1.85.0 check -p rayland-c` must stay green.
- **Doc-comment on every function, type, and method** (`///`); **intent comment on every non-trivial line** (the *why*, not the syntax). Stale comments are bugs — fix in the same edit. (CLAUDE.md.)
- **`rayland-c` must never link a GPU stack.** `tests/no_gpu_linkage.rs` guards it. This change adds no dependency (`BlobRun` is in `rayland-relay`, already linked), so the guard stays green — but do not add one.
- **No wire change.** Reuse `C2S::BlobData { res_id, offset, bytes }`. Do not touch `rayland-relay`.
- **Diary + ledger every working turn:** add an entry to `docs/DIARY.md` and update `.superpowers/sdd/progress.md` as tasks land.
- **Teeth-check every test:** watch each new test fail (or break the thing under test and confirm the test catches it) before trusting green.

---

### Task 1: The baseline and the diff primitive on `LocalBlob`

Add C's per-blob baseline and the single-pass diff-and-rebaseline that produces changed runs, plus the fold used by the return path. Pure data structure + logic, fully unit-testable with no GPU and no network.

**Files:**
- Modify: `crates/rayland-c/src/shm.rs` (the `LocalBlob` struct, `LocalBlob::create`, new methods, new tests in the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `rayland_relay::BlobRun { offset: u64, bytes: Vec<u8> }` (existing); `rayland_vtest::venus_ring::is_application_memory(blob_id: u64) -> bool` (already imported in `shm.rs`); `LocalBlob`'s existing private fields `mapping` (has `.as_ptr()` and `.len()`) and `blob_id`.
- Produces:
  - `LocalBlob::take_changed_runs(&mut self) -> Vec<rayland_relay::BlobRun>` — the changed runs vs the baseline, re-baselining as it goes. Empty for an unchanged blob or a non-application blob.
  - `LocalBlob::note_s_wrote(&mut self, offset: usize, bytes: &[u8])` — fold S's inbound bytes into the baseline so the next `take_changed_runs` does not re-ship them. No-op for a non-application blob.

- [ ] **Step 1: Add the `baseline` field to the struct**

In `crates/rayland-c/src/shm.rs`, add a field to `struct LocalBlob` (after `inode`):

```rust
    /// C's copy of the bytes S currently holds for this blob — the baseline the C→S diff ships against.
    ///
    /// Zero-length for a Venus-internal blob (the ring, reply arena, staging pool): only the
    /// application's own memory is shipped whole C→S, so only it needs a baseline (see
    /// [`crate::blob_sync`]). For an application blob it is the blob's size, initialised to zeros —
    /// which matches S's fresh, zero-filled memfd, so the first diff ships exactly the application's
    /// initial non-zero content and the two copies agree from the first relay onward.
    baseline: Vec<u8>,
```

- [ ] **Step 2: Initialise the baseline in `LocalBlob::create`**

In `LocalBlob::create`, between the `let mapping = ShmMapping::map(...)?;` line and the `Ok((LocalBlob { ... }, fd))` return, add the baseline and include it in the struct literal:

```rust
        // Only the application's own memory is shipped whole C→S and therefore needs a baseline; a
        // Venus-internal blob (the ring, the reply arena) gets an empty one and is never diffed. Zeros
        // match S's fresh memfd, so the first diff ships exactly the application's initial content.
        let baseline = if is_application_memory(blob_id) {
            vec![0u8; mapping.len()]
        } else {
            Vec::new()
        };
        Ok((
            LocalBlob {
                mapping,
                size,
                blob_id,
                inode,
                baseline,
            },
            fd,
        ))
```

(Replace the existing `Ok((LocalBlob { mapping, size, blob_id, inode }, fd))` with the above.)

- [ ] **Step 3: Add the import for `BlobRun`**

At the top of `crates/rayland-c/src/shm.rs`, add to the imports:

```rust
use rayland_relay::BlobRun;
```

- [ ] **Step 4: Write the failing tests for `take_changed_runs` and `note_s_wrote`**

Add to the `#[cfg(test)] mod tests` in `crates/rayland-c/src/shm.rs`:

```rust
    /// The app's vertex buffer id (ring-findings §6): a non-zero `blob_id` marks application memory.
    const APP_BLOB_ID: u64 = 16;
    /// A Venus-internal shmem id: `blob_id == 0` is the ring/arena/staging pool, never diffed C→S.
    const INTERNAL_BLOB_ID: u64 = 0;

    #[test]
    fn a_fresh_app_blob_diffs_its_whole_nonzero_content_as_one_run() {
        // A fresh application blob's baseline is zeros; writing non-zero content makes every byte
        // differ, so the first diff is exactly one run covering the whole blob.
        let (mut blob, _fd) = LocalBlob::create(APP_BLOB_ID, 64).expect("an app blob");
        blob.bytes_mut().fill(0x33);
        let runs = blob.take_changed_runs();
        assert_eq!(runs.len(), 1, "a wholly-written blob is one run");
        assert_eq!(runs[0].offset, 0);
        assert_eq!(runs[0].bytes, vec![0x33; 64]);
    }

    #[test]
    fn an_unchanged_app_blob_diffs_to_nothing_after_it_was_shipped() {
        // Once a diff has run it has re-baselined; a second diff of the untouched blob ships nothing.
        let (mut blob, _fd) = LocalBlob::create(APP_BLOB_ID, 64).expect("an app blob");
        blob.bytes_mut().fill(0x33);
        let _ = blob.take_changed_runs(); // ships and re-baselines
        assert!(
            blob.take_changed_runs().is_empty(),
            "an unchanged blob must ship nothing on the next relay"
        );
    }

    #[test]
    fn a_partial_change_diffs_only_the_changed_run() {
        // After the baseline holds 0x33, changing bytes [10..20] yields one run at offset 10.
        let (mut blob, _fd) = LocalBlob::create(APP_BLOB_ID, 64).expect("an app blob");
        blob.bytes_mut().fill(0x33);
        let _ = blob.take_changed_runs();
        blob.bytes_mut()[10..20].fill(0x44);
        let runs = blob.take_changed_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].offset, 10);
        assert_eq!(runs[0].bytes, vec![0x44; 10]);
    }

    #[test]
    fn two_disjoint_changes_diff_as_two_runs() {
        // Two separated changed regions, with unchanged bytes between them, are two runs — never one
        // coalesced run, which would re-ship the unchanged bytes the diff exists to skip.
        let (mut blob, _fd) = LocalBlob::create(APP_BLOB_ID, 64).expect("an app blob");
        blob.bytes_mut().fill(0x33);
        let _ = blob.take_changed_runs();
        blob.bytes_mut()[5..10].fill(0x44);
        blob.bytes_mut()[20..25].fill(0x55);
        let runs = blob.take_changed_runs();
        assert_eq!(runs.len(), 2, "disjoint changes must not coalesce across unchanged bytes");
        assert_eq!((runs[0].offset, runs[0].bytes.len()), (5, 5));
        assert_eq!((runs[1].offset, runs[1].bytes.len()), (20, 5));
    }

    #[test]
    fn an_internal_blob_has_no_baseline_and_diffs_to_nothing() {
        // Venus's own shmems are never shipped whole C→S; they carry no baseline and diff to nothing
        // even when written, so `messages_for_delta` never ships them by this path.
        let (mut blob, _fd) = LocalBlob::create(INTERNAL_BLOB_ID, 64).expect("an internal blob");
        blob.bytes_mut().fill(0x11);
        assert!(blob.take_changed_runs().is_empty());
    }

    #[test]
    fn note_s_wrote_keeps_the_baseline_so_c_does_not_ship_s_bytes_back() {
        // The return-path fold: when S writes a blob and sends it back, C applies it to the mapping AND
        // folds it into the baseline. The next C→S diff must then see no difference and ship nothing —
        // otherwise C would ship S's own bytes straight back to S (the last-writer-wins wobble).
        let (mut blob, _fd) = LocalBlob::create(APP_BLOB_ID, 64).expect("an app blob");
        // Simulate `apply_blob_data`: S's bytes land in the mapping and in the baseline together.
        blob.bytes_mut().fill(0x77);
        blob.note_s_wrote(0, &[0x77; 64]);
        assert!(
            blob.take_changed_runs().is_empty(),
            "C must not re-ship the bytes S itself wrote"
        );
    }
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-c --lib shm:: 2>&1 | tail -20`
Expected: FAIL to compile — `no method named take_changed_runs` / `note_s_wrote`.

- [ ] **Step 6: Implement `take_changed_runs` and `note_s_wrote`**

Add these two methods to `impl LocalBlob` in `crates/rayland-c/src/shm.rs` (after `bytes_mut`):

```rust
    /// The byte-runs of this blob that differ from the baseline, re-baselining as it goes — the C→S
    /// incremental sync. Empty for an unchanged blob, and empty for a Venus-internal blob (which has
    /// no baseline). See `docs/design/2026-07-25-c1-incremental-blob-sync.md`.
    ///
    /// # Why one pass, from a single read
    /// The application writes these pages with no call to intercept, so C must read the whole blob to
    /// learn what changed (there is no signal). Reading it once and using those same bytes for both the
    /// shipped run and the new baseline is what keeps C's baseline equal to what it actually sent — a
    /// second read for the baseline could see later writes and drift. A byte that matches the baseline
    /// ends the current run rather than being coalesced over, because coalescing would re-ship exactly
    /// the unchanged bytes this exists to skip (spec §"fragmentation").
    ///
    /// This is also why the shipped run is copied out of **the baseline**, not out of `live`, once the
    /// inner loop below has finished writing that range into `self.baseline`: at that point the two
    /// hold identical bytes for `[start..i)`, but `live` is still `Mesa`'s mapping and may have moved on
    /// by the time the run is materialised into a `Vec`. Reading `live` a second time here would be the
    /// exact "second read" the paragraph above warns against, just moved a few lines down — the baseline
    /// would then record bytes C never actually sent, and if the application's next write happened to
    /// revert to that earlier value, the two copies would disagree forever with nothing to notice it.
    ///
    /// # Pitfall: this read is racy against Mesa, by construction
    /// A torn read (the application mid-write) ships a torn intermediate; it is transient and the next
    /// relay's diff corrects it. This is the same inherent raciness [`Self::bytes`] documents and the
    /// remote-`vkMapMemory` problem (c)2 owns — not this method's to solve.
    pub fn take_changed_runs(&mut self) -> Vec<BlobRun> {
        let len = self.mapping.len();
        // Only application blobs carry a baseline sized to the mapping; a Venus-internal blob has an
        // empty one and is never diffed.
        if self.baseline.len() != len {
            return Vec::new();
        }
        // Take a raw pointer to the mapping so the live view below borrows nothing tracked, letting us
        // mutate `self.baseline` in the same loop. The mapping and the baseline are disjoint
        // allocations (an mmap and a `Vec`), so this cannot alias.
        let ptr = self.mapping.as_ptr() as *const u8;
        // SAFETY: `ptr` addresses `len` bytes of a live `MAP_SHARED` mapping that outlives this borrow;
        // `u8` has no invalid patterns; and `live` is disjoint from `self.baseline`, so mutating the
        // baseline below does not alias it. The concurrent-writer race is documented above.
        let live: &[u8] = unsafe { std::slice::from_raw_parts(ptr, len) };
        let mut runs = Vec::new();
        let mut i = 0;
        while i < len {
            // Skip bytes that already match what S has.
            if live[i] == self.baseline[i] {
                i += 1;
                continue;
            }
            // A changed run: extend it while the bytes differ, updating the baseline to the live bytes
            // as we go so the baseline ends equal to what this run ships.
            let start = i;
            while i < len && live[i] != self.baseline[i] {
                self.baseline[i] = live[i];
                i += 1;
            }
            runs.push(BlobRun {
                offset: start as u64,
                // From `self.baseline`, not `live`: the loop above just wrote `live[start..i]` into
                // `self.baseline[start..i]`, so the two agree right now. Reading `live` again here would
                // be a second, later look at memory Mesa may still be writing — see the doc comment
                // above for why that reopens the exact gap this method exists to close.
                bytes: self.baseline[start..i].to_vec(),
            });
        }
        runs
    }

    /// Fold bytes S wrote (arriving over the S→C return path) into the baseline, so the next
    /// [`Self::take_changed_runs`] does not turn around and ship S's own bytes back to S.
    ///
    /// No-op for a Venus-internal blob (no baseline). The caller [`crate::main`]'s `apply_blob_data`
    /// has already bounds-checked `offset + bytes.len()` against the blob size, and the baseline is
    /// exactly that size, so the slice below is in range.
    pub fn note_s_wrote(&mut self, offset: usize, bytes: &[u8]) {
        if self.baseline.len() != self.mapping.len() {
            return;
        }
        self.baseline[offset..offset + bytes.len()].copy_from_slice(bytes);
    }
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-c --lib shm:: 2>&1 | tail -20`
Expected: PASS — all six new tests plus the existing `shm::` tests.

- [ ] **Step 8: Teeth-check the return-path fold**

Temporarily delete the `blob.note_s_wrote(0, &[0x77; 64]);` line from `note_s_wrote_keeps_the_baseline_so_c_does_not_ship_s_bytes_back`, run that test, and confirm it now FAILS (the diff ships the `0x77` bytes — proving the test catches the bug). Then restore the line.

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-c --lib note_s_wrote 2>&1 | tail -8`
Expected: FAIL while the line is deleted; PASS after restoring.

- [ ] **Step 9: Commit**

```bash
git add crates/rayland-c/src/shm.rs
git commit -m "c1(blob-sync): LocalBlob baseline + take_changed_runs/note_s_wrote diff primitive"
```

---

### Task 2: `messages_for_delta` ships changed runs, not whole blobs

Replace the whole-blob push with a per-run push driven by Task 1's diff, so an unchanged application blob crosses nothing. Preserve the BlobData-before-RingDelta ordering exactly.

**Files:**
- Modify: `crates/rayland-c/src/blob_sync.rs` (`messages_for_delta` and its tests)

**Interfaces:**
- Consumes: `LocalBlob::take_changed_runs(&mut self) -> Vec<BlobRun>` (Task 1); `LocalBlob::is_application_memory(&self) -> bool` (existing); `rayland_relay::C2S::BlobData { res_id, offset, bytes }` (existing).
- Produces: unchanged public signature `messages_for_delta(blobs: &BlobTable, ring_res_id: u32, delta: RingDelta) -> Vec<C2S>`.

- [ ] **Step 1: Write the failing test for skip-on-unchanged**

Add to `#[cfg(test)] mod tests` in `crates/rayland-c/src/blob_sync.rs`:

```rust
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
```

- [ ] **Step 2: Run it to verify it fails**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-c --lib blob_sync::tests::an_unchanged_blob 2>&1 | tail -12`
Expected: FAIL — the current whole-blob code re-ships the blob on the second relay, so the second assertion fails.

- [ ] **Step 3: Rewrite the blob loop in `messages_for_delta`**

In `crates/rayland-c/src/blob_sync.rs`, replace the blob-shipping block (the `for (&res_id, blob) in table.iter()` loop and its body, currently pushing a whole-blob `C2S::BlobData`) with:

```rust
        // `iter_mut`, not `iter`: the diff re-baselines each blob it inspects.
        let mut table = blobs.lock().expect("the blob table lock is never poisoned");
        for (&res_id, blob) in table.iter_mut() {
            // Venus's own shmems — the ring, the reply arena, the staging pool — are not C's to
            // publish. Ring-findings §6's `blob_id` signal is the line between the application's memory
            // and the transport's plumbing; `take_changed_runs` also returns empty for them (no
            // baseline), so this filter is the intent and that is the backstop.
            if !blob.is_application_memory() {
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
```

Keep the surrounding lock scope `{ ... }` and the `out.push(C2S::RingDelta { ... })` after it exactly as they are — the ordering guarantee (every `BlobData` before the `RingDelta`) is unchanged.

- [ ] **Step 4: Run the new test and the existing `blob_sync` tests**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-c --lib blob_sync 2>&1 | tail -20`
Expected: PASS — the new `an_unchanged_blob_...` test and every existing `blob_sync::tests::*` (they fill blobs with non-zero bytes, so the first relay still ships them whole as one run: `the_app_s_blobs_are_shipped_before_the_ring_delta...`, `the_shipped_blob_carries_the_application_s_actual_bytes`, `venus_s_own_shmems_are_never_shipped_c_to_s`, `every_application_blob_is_shipped_not_merely_one`, `the_ring_delta_reaches_s_unaltered`, `a_delta_with_no_application_blobs_yet_is_still_relayed`).

If `the_shipped_blob_carries_the_application_s_actual_bytes` asserts `offset == 0` or the exact whole-blob bytes: a uniformly-filled blob against a zero baseline is exactly one run at offset 0 covering the whole blob, so it still holds. If any existing assertion is written against the *count* of messages in a way the diff changes, update it to the diff contract (one run per changed region) rather than weakening it.

- [ ] **Step 5: Commit**

```bash
git add crates/rayland-c/src/blob_sync.rs
git commit -m "c1(blob-sync): messages_for_delta ships changed runs, skips unchanged blobs"
```

---

### Task 3: Re-baseline on inbound S→C `BlobData`, and verify end to end

Fold S's return-path writes into the baseline where C applies them, and run the loopback e2e — which exercises S writing a readback and shipping it back — as the correctness gate for the whole change.

**Files:**
- Modify: `crates/rayland-c/src/main.rs` (`apply_blob_data`)

**Interfaces:**
- Consumes: `LocalBlob::note_s_wrote(&mut self, offset: usize, bytes: &[u8])` (Task 1).
- Produces: no signature change; `apply_blob_data` keeps C's baseline in step with S's writes.

- [ ] **Step 1: Add the re-baseline call in `apply_blob_data`**

In `crates/rayland-c/src/main.rs`, in `fn apply_blob_data`, immediately after the line that writes S's bytes into the mapping —

```rust
    blob.bytes_mut()[start..end as usize].copy_from_slice(bytes);
```

— add:

```rust
    // **Keep C's baseline in step with what S wrote.** S owns the readback buffers and the reply
    // arena; the bytes it just sent are now what S holds for this blob, so fold them into C's baseline.
    // Without this, the next C→S diff would see C's mapping (now carrying S's writes) differ from a
    // stale baseline and ship S's own bytes back to S — the last-writer-wins wobble (c)1 Task 5b fixed
    // in the S→C direction, here in its C→S twin. See `docs/design/2026-07-25-c1-incremental-blob-sync.md`.
    blob.note_s_wrote(start, bytes);
```

- [ ] **Step 2: Confirm it compiles and the whole rayland-c unit suite is green**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-c --lib 2>&1 | tail -6`
Expected: PASS. Also run `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-c --test no_gpu_linkage 2>&1 | tail -4` — Expected: PASS (no GPU dependency added).

- [ ] **Step 3: Run the loopback e2e — the correctness gate**

This is the real proof: `rayland-refapp` writes a vertex buffer once and reads a readback S writes; `rayland-icosa-*` rewrite a mapped blob every frame. Both must stay **bit-identical** through the diff — a dropped or mis-offset run would surface as a wrong pixel here. Requires the GPU (`/dev/dri/renderD128`) and takes several minutes.

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-s --test loopback_e2e 2>&1 | tail -25`
Expected: PASS — refapp bit-identical (0/16384 bytes differ) and the icosa e2e frames bit-identical. If any frame differs, STOP: the diff is losing bytes — do not weaken the e2e; return to Task 1's diff loop and Task 3's re-baseline.

- [ ] **Step 4: Verify the bandwidth win (the point of the change)**

Reconstruct the vkcube loopback smoke (see the WP0 handoff / `docs/design/2026-07-24-wp0-task4-next-session-prompt.md` §4 for the daemon recipe), run vkcube through it with `RAYLAND_C1_METRICS=1` on the C daemon, and confirm from the `C1METRICS` line that `c2s_blob_sync_bytes` has fallen by roughly two orders of magnitude versus the pre-change ~16.5 MB, while `c2s_ring_bytes` is unchanged (~23 KB). Process discipline: launch daemons with `setsid`, kill only the exact captured PIDs by group, never pattern-kill (user's global CLAUDE.md rule). This is a measurement, not an assertion — record the before/after numbers in the diary; do not gate the task on an exact figure.

- [ ] **Step 5: Commit, and write the diary + ledger entries**

Add a `docs/DIARY.md` entry (the honest story: the diff landed, the e2e stayed bit-identical, the measured C→S blob bytes before/after) and update `.superpowers/sdd/progress.md`. Then:

```bash
git add crates/rayland-c/src/main.rs docs/DIARY.md
git commit -m "c1(blob-sync): re-baseline on inbound S->C BlobData; e2e bit-identical, C->S blob bytes cut ~100x"
```

---

## Self-Review

**Spec coverage:**
- Baseline per application blob, zero-initialised → Task 1 Steps 1–2. ✓
- Single-pass diff-and-rebaseline, ship changed runs, skip unchanged → Task 1 Step 6; Task 2 Step 3. ✓
- No wire change (reuse `BlobData` offset) → Task 2 Step 3 (offset from the run). ✓
- Re-baseline on inbound `BlobData` (the load-bearing correctness point) → Task 1 `note_s_wrote` + Task 3 Step 1. ✓
- Ordering (BlobData before RingDelta) preserved → Task 2 Step 3 keeps the RingDelta push after the loop. ✓
- Trigger unchanged (the relay event) → `messages_for_delta` is still called per relay; untouched. ✓
- Fragmentation: exact runs, no coalescing → Task 1 diff ends a run on a matching byte; `two_disjoint_changes_diff_as_two_runs` pins it. ✓
- Metrics report bytes and messages separately → unchanged; each `BlobData` is still classified `blob_sync` by `link.rs::record_send`; Task 3 Step 4 reads them. ✓
- Scope excludes remote-`vkMapMemory` and dedup → nothing in the plan attempts either; the diff simply ships all changed bytes (icosa still ships its megabyte). ✓
- Regression gate (loopback e2e bit-identical) → Task 3 Step 3. ✓

**Placeholder scan:** No TBD/TODO. Every code step has literal code; every test step has literal assertions and a run command with an expected result. Task 3 Step 4 is a measurement, deliberately not gated on an exact number (stated).

**Type consistency:** `take_changed_runs(&mut self) -> Vec<BlobRun>` and `note_s_wrote(&mut self, offset: usize, bytes: &[u8])` are named identically in Task 1 (definition), Task 2 (call), and Task 3 (call). `BlobRun { offset: u64, bytes: Vec<u8> }` fields match their use (`run.offset`, `run.bytes`) in Task 2. `C2S::BlobData { res_id, offset, bytes }` matches the existing wire type. `is_application_memory` used as the existing `&self` method (Task 2) and the imported free function on `blob_id` (Task 1 Step 2) — both already exist in `shm.rs`.
