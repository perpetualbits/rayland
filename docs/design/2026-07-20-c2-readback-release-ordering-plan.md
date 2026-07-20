# Readback release ordering — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the ~1/11 residual stale frame by holding the ring head-advance that releases a readback-bearing draw until that frame's readback pixels have shipped.

**Architecture:** `RingMirror::take_progress` and `Applier::take_ring_progress` gain an optional cap so S can report the consumed frontier only up to a given ring position, shipping the held-back remainder later. `progress_thread` caps the step-1 head-advance at the latest undelivered submit, and in step 2 releases the full head *after* the readback ships (draw) or immediately (copy/identical — safe because the fence already retired any readback DMA). All in `rayland-s`; no wire or `rayland-c` change.

**Tech Stack:** Rust, the `rayland-s` crate (lib + bin), `cargo test` (integration tests in `tests/apply.rs`), and a committed two-machine bash oracle (apollo + dop561) for the network proof.

## Global Constraints

- **Language: Rust for all code Rayland writes.**
- **Doc-comment on every function/type/module**; **intent comment on every non-trivial line** (the *why*, never a syntax restatement); genuinely trivial lines get none.
- **Code and comments must always agree** — fix a stale comment in the same edit.
- **`rayland-s` is GPL, `publish = false`, `v0.0.x`** — do not change its manifest.
- **Do not add `VN_DEBUG=no_abort`** to any run (Mesa's stall-abort is the detector).
- This implements `docs/design/2026-07-20-c2-readback-release-ordering.md`; the residual it fixes and the confirmed cause are in `docs/design/2026-07-19-c2-true-remote-mapped-sync.md`. Read both before starting.
- **Free-running counters:** `head`, `reported_head`, and `latest_submit_pos` are free-running ring byte counters that wrap mid-run (~frame 82). All distance/comparison arithmetic on them must be **wrapping** (`wrapping_sub`), never masked buffer offsets.

---

### Task 1: Add an optional cap to the ring-progress mechanism

Teach `RingMirror::take_progress` to report the consumed head only up to a cap (advancing its internal `reported_head` no further than what it reported, so the remainder ships on a later uncapped call), thread the cap through `Applier::take_ring_progress` (applied to every ring — single-ring in (c)1), update the callers to preserve today's behavior with `None`, and cover the cap with integration tests.

**Files:**
- Modify: `crates/rayland-s/src/ring_mirror.rs` (`take_progress`, ~line 399)
- Modify: `crates/rayland-s/src/apply.rs` (`take_ring_progress`, ~line 1065)
- Modify: `crates/rayland-s/src/main.rs` (the one call at ~line 276 → pass `None` for now; Task 2 replaces it)
- Modify: `crates/rayland-s/tests/apply.rs` (`poll_progress` helper at ~line 55 → pass `None`; add the new tests)

**Interfaces:**
- Produces: `RingMirror::take_progress(&mut self, blob: &HostBlob, cap: Option<u32>) -> Option<u32>` and `Applier::take_ring_progress(&mut self, cap: Option<u32>) -> Vec<S2C>`. `cap == None` reproduces today's behavior exactly.

- [ ] **Step 1: Write the failing tests**

In `crates/rayland-s/tests/apply.rs`, first update the existing `poll_progress` helper (~line 55) so it still compiles under the new signature — change `let progress = applier.take_ring_progress();` to:

```rust
    let progress = applier.take_ring_progress(None);
```

Then add these three tests to the file (near the other `head`/progress tests, after `progress_is_reported_once_per_movement_not_on_every_poll`). `RING_HEAD_OFFSET`, `session_with_ring`, and `RecordingEngine::write_control` are already in scope/used by neighboring tests.

```rust
/// **(c)2 release ordering.** A cap makes S report the consumed head only up to the cap, and the
/// held-back remainder ships on a later uncapped call. This is what lets `progress_thread` hold a
/// readback-bearing draw's release until its pixels have shipped, without losing the rest of the
/// head-advance.
#[test]
fn take_ring_progress_caps_the_head_and_ships_the_remainder_when_uncapped() {
    let (mut applier, mut engine, ring) = session_with_ring();
    applier.apply(
        &mut engine,
        C2S::RingDelta { ring_res_id: ring, tail: 64, bytes: vec![0u8; 64] },
    );
    // The engine consumed all 64 bytes.
    engine.write_control(ring, RING_HEAD_OFFSET, 64);

    // Capped at 32: S reports only 32, holding the release of everything past it.
    let capped = applier.take_ring_progress(Some(32));
    assert!(
        matches!(
            capped.as_slice(),
            [S2C::RingProgress { ring_res_id, consumed_tail: 32 }] if *ring_res_id == ring
        ),
        "cap must hold the reported head at 32; got {capped:?}"
    );

    // Uncapped: the held-back remainder (up to the full head, 64) must now ship.
    let remainder = applier.take_ring_progress(None);
    assert!(
        matches!(
            remainder.as_slice(),
            [S2C::RingProgress { ring_res_id, consumed_tail: 64 }] if *ring_res_id == ring
        ),
        "lifting the cap must ship the remainder up to the full head; got {remainder:?}"
    );
}

/// A cap **ahead** of where the engine has actually consumed is inert: S reports the true head, not
/// the cap. The cap only ever *holds back*, never invents progress the engine has not made.
#[test]
fn a_cap_ahead_of_the_head_is_inert() {
    let (mut applier, mut engine, ring) = session_with_ring();
    applier.apply(
        &mut engine,
        C2S::RingDelta { ring_res_id: ring, tail: 64, bytes: vec![0u8; 64] },
    );
    // The engine has consumed only 32.
    engine.write_control(ring, RING_HEAD_OFFSET, 32);

    let out = applier.take_ring_progress(Some(64));
    assert!(
        matches!(
            out.as_slice(),
            [S2C::RingProgress { ring_res_id, consumed_tail: 32 }] if *ring_res_id == ring
        ),
        "a cap ahead of the head must be inert; got {out:?}"
    );
}

/// A cap at (or below) what was already reported yields no progress — nothing new is releasable
/// within it, and progress is reported on movement only, never on repetition.
#[test]
fn a_cap_at_the_already_reported_head_yields_no_progress() {
    let (mut applier, mut engine, ring) = session_with_ring();
    applier.apply(
        &mut engine,
        C2S::RingDelta { ring_res_id: ring, tail: 64, bytes: vec![0u8; 64] },
    );
    engine.write_control(ring, RING_HEAD_OFFSET, 32);
    // Report up to 32 first.
    let _ = applier.take_ring_progress(None);

    // Head unchanged; a cap of 32 (== already reported) has no new room to report.
    assert!(
        applier.take_ring_progress(Some(32)).is_empty(),
        "a cap at the already-reported head must report nothing"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo test -p rayland-s --test apply take_ring_progress 2>&1 | tail -20`
Expected: **compile error** — `take_ring_progress`/`take_progress` do not yet accept a `cap` argument. (This is the RED: the new signature does not exist.)

- [ ] **Step 3: Add the cap to `RingMirror::take_progress`**

In `crates/rayland-s/src/ring_mirror.rs`, replace the body of `take_progress` (~line 399) — currently:

```rust
    pub fn take_progress(&mut self, blob: &HostBlob) -> Option<u32> {
        let head = self.head_word(blob).load(Ordering::Acquire);
        if head == self.reported_head {
            return None;
        }
        self.reported_head = head;
        Some(head)
    }
```

with (keep the existing `Acquire`-ordering doc comment above the function; extend it to mention the cap):

```rust
    pub fn take_progress(&mut self, blob: &HostBlob, cap: Option<u32>) -> Option<u32> {
        // `Acquire`, pairing with virglrenderer's `Release` store on `head` (its own comment asks a
        // renderer to load with acquire, forming release-acquire ordering).
        let head = self.head_word(blob).load(Ordering::Acquire);
        // Report only up to `cap` in the ring's free-running counter space. `reported_head` advances
        // to exactly what is reported and never past the cap, so a capped-off remainder is shipped by
        // a later uncapped call — which is how `progress_thread` holds a readback draw's release until
        // its pixels ship without losing the rest of the head-advance.
        let target = match cap {
            // No cap: report the whole head, exactly as before.
            None => head,
            // Capped: report whichever of `head` and `cap` is the *nearer forward* from what we last
            // reported. Forward distance is wrapping because the counter wraps mid-run. When the cap
            // is nearer (the head has already passed it), the cap wins and holds the release; when the
            // head has not yet reached the cap, the head wins and the cap is inert.
            Some(c) if c.wrapping_sub(self.reported_head) < head.wrapping_sub(self.reported_head) => c,
            Some(_) => head,
        };
        // Report on movement only: `target == reported_head` means nothing new is releasable here.
        if target == self.reported_head {
            return None;
        }
        self.reported_head = target;
        Some(target)
    }
```

- [ ] **Step 4: Thread the cap through `Applier::take_ring_progress`**

In `crates/rayland-s/src/apply.rs`, change `take_ring_progress` (~line 1065). Update its signature to accept `cap: Option<u32>` and pass it to `mirror.take_progress(blob, cap)`. Add a doc-comment sentence explaining that the cap holds every ring's head-advance at `cap` (correct because (c)1 runs a single ring). The call inside the loop changes from `mirror.take_progress(blob)` to:

```rust
            if let Some(consumed_tail) = mirror.take_progress(blob, cap) {
```

Keep the surrounding `T2` trace emit and the `S2C::RingProgress` push unchanged.

- [ ] **Step 5: Update the remaining caller in `main.rs` to preserve behavior**

In `crates/rayland-s/src/main.rs` (~line 276), change `session.take_ring_progress()` to `session.take_ring_progress(None)` so it compiles with unchanged behavior. (Task 2 replaces this with the real cap.)

- [ ] **Step 6: Run the tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo test -p rayland-s 2>&1 | tail -25`
Expected: the whole `rayland-s` suite PASSES, including the three new `take_ring_progress_*` / `a_cap_*` tests and all pre-existing progress tests (proving `None` preserved behavior).

- [ ] **Step 7: Commit**

```bash
cd /home/roland/git/rayland
git add crates/rayland-s/src/ring_mirror.rs crates/rayland-s/src/apply.rs crates/rayland-s/src/main.rs crates/rayland-s/tests/apply.rs
git commit -m "rayland-s: optional cap on ring-progress reporting

RingMirror::take_progress and Applier::take_ring_progress gain a cap so S can
report the consumed head only up to a given ring position and ship the held-back
remainder later (wrapping-aware; advances reported_head no further than reported).
cap=None reproduces prior behavior; the one existing caller passes None.
Groundwork for holding a readback draw's release until its pixels ship."
```

---

### Task 2: Cap the head-advance in `progress_thread` and release after the readback

Use the cap: hold the step-1 head-advance at the latest undelivered submit so the readback-bearing draw is not released early; in step 2 release the full head *after* the readback ships (draw), or immediately (copy/identical frame — safe because the fence has already retired any readback DMA, so an empty result means no new pixels exist).

**Files:**
- Modify: `crates/rayland-s/src/main.rs` (`progress_thread`: the lock-read block ~273–282, and the step-2 completion block ~329–351)

**Interfaces:**
- Consumes: `Applier::take_ring_progress(Option<u32>)` from Task 1; `Applier::latest_submit_pos()` (existing); `rayland_s::delivery::readback_delivery_ready` (existing).

- [ ] **Step 1: Cap the step-1 head-advance**

In `progress_thread`, replace the lock-read block (currently):

```rust
        let (progress, ctx_id, ring_idx, drained, latest_submit) = {
            let mut session = applier.lock().expect("the applier lock is never poisoned");
            (
                session.take_ring_progress(None),
                session.ctx_id(),
                session.retirement_ring_idx(),
                session.queue_ring_drained(),
                session.latest_submit_pos(),
            )
        };
```

with:

```rust
        let (progress, ctx_id, ring_idx, drained, latest_submit) = {
            let mut session = applier.lock().expect("the applier lock is never poisoned");
            let latest = session.latest_submit_pos();
            // Cap the head-advance at the latest submit not yet delivered, so the draw that owes a
            // readback is not released until step 2 ships its pixels. Everything earlier — the upload
            // copy and its fence-wait — is below the cap and still released promptly. `None` once
            // nothing is undelivered, so a quiescent session reports its head unheld.
            let cap = if latest.is_some() && latest > last_delivered_submit {
                latest
            } else {
                None
            };
            (
                session.take_ring_progress(cap),
                session.ctx_id(),
                session.retirement_ring_idx(),
                session.queue_ring_drained(),
                latest,
            )
        };
```

- [ ] **Step 2: Release the full head in step 2, after the readback**

In `progress_thread`, replace the step-2 body — from `let app = { ... session.take_app_blob_writes() };` through the closing of the `if rayland_s::delivery::readback_delivery_ready(...) { ... }` block and its trailing `// else:` comment (the current code reads `app`, computes `readback_advanced`/`pending_elapsed`, and on ready ships `app` then clears `delivery_pending` / advances `last_delivered_submit`) — with:

```rust
                // Read S's new app-blob writes AND the now-releasable head together under one lock.
                // The pending submit's GPU work — including any readback DMA into the readback blob —
                // has retired (the fence above waited for it), so releasing the full head now is safe:
                // an empty write set means no new pixels exist (a copy submit, which the app does not
                // read a readback after; or an identical frame the app reads correctly from its own
                // unchanged local blob). See docs/design/2026-07-20-c2-readback-release-ordering.md §5.
                let (app, released) = {
                    let mut session = applier.lock().expect("the applier lock is never poisoned");
                    let app = session.take_app_blob_writes();
                    let released = session.take_ring_progress(None);
                    (app, released)
                };
                // A non-empty write set means the readback advanced past the last delivered frame.
                let readback_advanced = !app.is_empty();
                // How long this delivery has been pending — only the identical-frame fallback reads it.
                let pending_elapsed = pending_since.map(|t| t.elapsed()).unwrap_or_default();
                if rayland_s::delivery::readback_delivery_ready(readback_advanced, pending_elapsed) {
                    // Draw (or the bounded identical-frame fallback): ship the pixels, THEN the head
                    // that releases the application onto them — the ordering the residual was about.
                    if ship(&tx, &app).is_err() {
                        return;
                    }
                    if ship(&tx, &released).is_err() {
                        return;
                    }
                    delivery_pending = false;
                    pending_since = None;
                    // Only a submit newer than this one may trigger the next delivery.
                    last_delivered_submit = latest_submit;
                } else {
                    // Copy submit (owes no readback), resolved empty within the bound: release the head
                    // so the application proceeds past its upload fence-wait to submit the draw — else
                    // a two-submits-per-frame app deadlocks — but keep the delivery pending so the
                    // real readback (the draw) is still awaited.
                    if ship(&tx, &released).is_err() {
                        return;
                    }
                }
```

- [ ] **Step 3: Verify it builds**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo build -p rayland-s 2>&1 | tail -5`
Expected: `Finished`, no unused-variable/borrow errors (both `app` and `released` are used; `latest_submit` is used for `last_delivered_submit`).

- [ ] **Step 4: Verify the existing suite still passes**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo test -p rayland-s 2>&1 | tail -20`
Expected: all tests PASS (the apply/blob/ring-mirror/delivery/cap suites and both loopback e2e tests — `refapp` and `icosa_cpu_renders` — the latter proving the cap is inert on loopback where there is no residual).

- [ ] **Step 5: Commit**

```bash
cd /home/roland/git/rayland
git add crates/rayland-s/src/main.rs
git commit -m "rayland-s: hold a readback draw's head-advance until its pixels ship

progress_thread caps the step-1 head-advance at the latest undelivered submit,
and in step 2 releases the full head AFTER shipping the readback (draw) or
immediately (copy/identical, safe post-fence). This closes the C-side release
race: the app's vkWaitForFences, released by the head, no longer fires before the
frame's readback lands on C. Fixes the ~1/11 residual from the completion gate
(docs/design/2026-07-20-c2-readback-release-ordering.md)."
```

---

### Task 3: Two-machine proof and status docs

Prove over the real network that the residual is gone, then record the fix honestly.

**Files:**
- Use: `scripts/c2-icosa-two-machine.sh` (existing oracle; run it, do not modify)
- Modify: `CLAUDE.md` (the `(c)2` bullet), `docs/icosa-fixtures.md` (§8), `docs/design/2026-07-20-c2-readback-release-ordering.md` (append an outcome line)

- [ ] **Step 1: Run the two-machine oracle at high count**

The residual was ~1/11, so a convincing proof needs many runs. Run in batches of ≤5 (a long single invocation gets wall-clock-killed in this environment — a lesson from the gate's e2e):

Run: `cd /home/roland/git/rayland && for b in 1 2 3 4; do ./scripts/c2-icosa-two-machine.sh 5 2>&1 | grep -E "run [0-9]+/|TOTAL|PASS|FAIL"; done`
Expected: every run prints `0 stale frame(s)`; **≥ 20 runs total with 0 stale**. No `SIGABRT` / `QUEUE_REGISTER_DEADLINE` / `died`.

If any run shows stale frames OR a deadlock, STOP: the release logic is wrong (a deadlock means the copy-submit head-release of Task 2 Step 2 failed; a stale frame means the cap is not holding). Do not paper over it — return to the design's §5/§8.

- [ ] **Step 2: Update `CLAUDE.md` `(c)2` bullet**

In `CLAUDE.md`, in the `(c)2` bullet, replace the sentence describing the residual and "the next (c)2 step is ordering the head-advance..." with a statement that it is **fixed**: the readback release-ordering change (cap the head-advance at the readback draw, release after the pixels) closed the ~1/11 residual — `scripts/c2-icosa-two-machine.sh` now runs **0 stale across ≥20 runs**. Cross-reference `docs/design/2026-07-20-c2-readback-release-ordering.md`. Keep the note that **Direction B** (fence feedback over a real network) and **multi-queue** remain open.

- [ ] **Step 3: Update `docs/icosa-fixtures.md` §8**

Change the §8 closing (which currently says the release race "is where (c)2's remaining subject matter now lives") to record that the release-ordering fix landed and the two-machine oracle is now 0 stale across ≥20 runs; cross-reference the design doc.

- [ ] **Step 4: Append the outcome to the design doc**

At the end of `docs/design/2026-07-20-c2-readback-release-ordering.md`, append a short `## Outcome` section stating the fix landed and `scripts/c2-icosa-two-machine.sh` measured 0 stale across ≥20 real-network runs (give the actual run count and total from Step 1), with no deadlock or abort.

- [ ] **Step 5: Commit**

```bash
cd /home/roland/git/rayland
git add CLAUDE.md docs/icosa-fixtures.md docs/design/2026-07-20-c2-readback-release-ordering.md
git commit -m "c2: readback release-ordering fix landed — residual closed over the real network

scripts/c2-icosa-two-machine.sh: 0 stale across <N> runs (was ~1/11). Records
the fix in CLAUDE.md, icosa-fixtures §8, and the design doc's Outcome section.
Direction B (fence feedback over the network) and multi-queue remain open."
```

---

## Self-Review

**Spec coverage:**
- §2 invariant (head must not pass a readback draw until its pixels ship): Task 2 Step 1 (cap) + Step 2 (release after readback). ✓
- §3 wrong alternatives avoided: the cap releases everything below the draw (upload copy), so no deadlock; the two-step split is preserved (no fence-before-head-advance). ✓
- §4 cap at `latest_submit_pos`, `min(head, Pb)` in free-running space: Task 1 Step 3 (wrapping `take_progress`) + Task 2 Step 1 (`cap = latest`). ✓
- §5 post-fence-empty is safe to release; release vs complete are distinct events: Task 2 Step 2 (empty → release head, keep pending; non-empty → ship + release + complete). ✓
- §6 no wire/`rayland-c` change: only `rayland-s` files touched. ✓
- §7 testing (e2e ≥20 runs; unit on the cap; loopback regression): Task 3 Step 1; Task 1 tests; Task 2 Step 4. ✓
- §8 risks (upload fence-wait via head → loud deadlock if wrong; wrapping): Task 3 Step 1 stop-condition; Task 1 wrapping `take_progress` + tests. ✓

**Placeholder scan:** no TBD/TODO; every code step shows complete code; `<N>`/`<run count>` in Task 3 are filled from the measured result, not shipped as placeholders.

**Type consistency:** `take_progress(&HostBlob, Option<u32>) -> Option<u32>` and `take_ring_progress(Option<u32>) -> Vec<S2C>` are defined in Task 1 and consumed with those exact signatures in Task 2. `latest_submit_pos() -> Option<u32>` and `readback_delivery_ready(bool, Duration) -> bool` used per their real signatures.
