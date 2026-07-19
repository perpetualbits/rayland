# Readback-completion gate — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop `rayland-s` from delivering a stale (previous-frame) readback when an application issues more than one submit per frame, by completing a readback delivery only once the readback blob's contents have actually advanced.

**Architecture:** The entire change is in `rayland-s`. A new pure decision function in the `rayland_s` library (`delivery.rs`) expresses "may this pending delivery complete now?" and is unit-tested in isolation; `main.rs`'s `progress_thread` calls it instead of completing unconditionally. `take_app_blob_writes` (which already returns S's newly-written app-blob bytes, empty when none) is the "readback advanced" signal. No `rayland-c`, wire, or forward-path change.

**Tech Stack:** Rust, the existing `rayland-s` crate (lib + bin), `cargo test` for the unit test, a committed bash script + apollo/dop561 for the two-machine end-to-end check.

## Global Constraints

- **Language: Rust for all code Rayland writes.** (CLAUDE.md, Locked decisions.)
- **A doc-comment block (`///`/`//!`) on every function, type, and module**, describing what it does, inputs/outputs, failure modes, domain pitfalls. (CLAUDE.md, Code conventions.)
- **An intent comment on every non-trivial line** — the *why*/domain meaning, never a restatement of syntax; genuinely trivial lines get none. (CLAUDE.md.)
- **Code and comments must always agree**; fix a stale comment in the same edit as the code. (CLAUDE.md.)
- **`rayland-s` is GPL, `publish = false`, `v0.0.x`.** Do not change its manifest license/publish fields.
- The design this implements is `docs/design/2026-07-19-c2-readback-completion-gate.md`; the confirmed root cause is `docs/design/2026-07-19-c2-true-remote-mapped-sync.md`. Read both before starting.
- **Do not add `VN_DEBUG=no_abort`** to any run (the Mesa stall-abort is the stall detector). (scripts/c1-two-machine.sh.)

---

### Task 1: Revert the throwaway diagnostic instrumentation

The root-cause investigation left an env-gated `RAYLAND_C1_FPLOG` probe uncommitted in `apply.rs` and `main.rs`. Start from a clean committed tree so the fix is not entangled with dead diagnostics. (The fix is verified by the frame-diff oracle in Task 4, which does not need the probe.)

**Files:**
- Modify (revert): `crates/rayland-s/src/apply.rs`
- Modify (revert): `crates/rayland-s/src/main.rs`

- [ ] **Step 1: Confirm the only uncommitted changes are the probe**

Run: `git -C /home/roland/git/rayland status --short`
Expected: exactly ` M crates/rayland-s/src/apply.rs` and ` M crates/rayland-s/src/main.rs` (and nothing else uncommitted).

- [ ] **Step 2: Revert both files to HEAD**

```bash
cd /home/roland/git/rayland
git checkout -- crates/rayland-s/src/apply.rs crates/rayland-s/src/main.rs
```

- [ ] **Step 3: Verify the probe is gone and the tree is clean**

Run: `git -C /home/roland/git/rayland status --short && grep -rn "FPLOG\|debug_selected_blob_fps\|fnv1a" crates/rayland-s/src`
Expected: empty output (clean tree, no probe symbols).

- [ ] **Step 4: Verify it still builds**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo build -p rayland-s`
Expected: `Finished`.

(No commit — this only removes uncommitted changes.)

---

### Task 2: The pure delivery-gate decision, unit-tested

Add a small library module holding the decision "may a pending readback delivery complete now?" and the bound it uses. This is the fix's logic, isolated so it can be tested without an engine, a socket, or a GPU.

**Files:**
- Create: `crates/rayland-s/src/delivery.rs`
- Modify: `crates/rayland-s/src/lib.rs` (add `pub mod delivery;`)

**Interfaces:**
- Produces: `pub const rayland_s::delivery::READBACK_ADVANCE_BOUND: std::time::Duration` and `pub fn rayland_s::delivery::readback_delivery_ready(readback_advanced: bool, pending_elapsed: std::time::Duration) -> bool`.

- [ ] **Step 1: Add the module declaration to the library**

In `crates/rayland-s/src/lib.rs`, add the module declaration next to the other `pub mod` lines (match their alphabetical/grouped placement):

```rust
pub mod delivery;
```

- [ ] **Step 2: Write the module with the failing test first**

Create `crates/rayland-s/src/delivery.rs`:

```rust
//! **The readback-completion gate.** Deciding *when* a pending readback delivery on S may complete.
//!
//! # The defect this exists to fix
//! S delivers an application's readback (the finished pixels) once its (c)2 completion barrier fires.
//! That barrier's trigger is content-independent — it fires on a newer `vkQueueSubmit` position plus a
//! drained ring — which is correct only when every submit produces a readback. An application that
//! issues **more than one submit per frame** (the `rayland-icosa-cpu` fixture issues two: a fractal
//! upload copy, then the draw-and-readback) breaks that assumption: S cannot tell the copy submit from
//! the draw submit without parsing the opaque ring, so the barrier can fire on the copy — whose fence
//! retires with the readback blob still holding the *previous* frame — and ship those stale pixels.
//! Over a real network this loses ~2/120 frames; on loopback the timing window never opens. Full
//! evidence: `docs/design/2026-07-19-c2-true-remote-mapped-sync.md`.
//!
//! # The signal this gate keys on
//! With an opaque ring and N submits per frame, the *only* thing that distinguishes a readback-bearing
//! submit is that **S's own write into an application blob advanced** — the copy submit writes a
//! texture image, not an application blob, so it produces no such advance. `Applier::take_app_blob_writes`
//! already reports exactly S's newly-written app-blob bytes and is empty when there are none, so
//! "readback advanced" is "that call returned something". This module holds the pure decision built on
//! that boolean, kept separate from `main.rs`'s `progress_thread` so it can be tested without an engine
//! or a socket.

use std::time::Duration;

/// How long a pending readback delivery will wait for the readback to advance before completing anyway.
///
/// # Why a bound exists at all
/// The gate below waits for S's readback write to advance past the last delivered frame. For two
/// *byte-identical* consecutive frames the readback never advances (the fixture never produces such a
/// pair — its fractal zooms and its model-view rotates every frame — but a real application could), and
/// an unbounded wait would hang the application in `vkWaitForFences` until Mesa's ~3.5 s stall-abort.
/// Completing after this bound with the unchanged — and therefore still correct — bytes avoids that.
///
/// # Why this value
/// It must sit comfortably **above** the inter-submit round trip (a draw submit's readback lands within
/// a single network RTT of its copy submit, single-digit milliseconds on a LAN) so a normal frame never
/// waits it out, and comfortably **below** both Mesa's ~3.5 s abort and S's own `QUEUE_REGISTER_DEADLINE`
/// (5 s) so it is the *first* backstop to act. 250 ms satisfies both with wide margin.
pub const READBACK_ADVANCE_BOUND: Duration = Duration::from_millis(250);

/// Decide whether a pending readback delivery may complete now.
///
/// # Inputs / outputs
/// - `readback_advanced`: whether S has written new bytes into an application blob since the last
///   delivery — i.e. whether `Applier::take_app_blob_writes` returned a non-empty set this poll. This is
///   the proof that a readback-bearing submit (not a bare copy submit) has retired.
/// - `pending_elapsed`: how long the current delivery has been pending. Only consulted for the
///   identical-frame fallback described on [`READBACK_ADVANCE_BOUND`].
/// - Returns `true` to complete the delivery now, `false` to keep it pending and poll again.
///
/// # Failure modes
/// None; it is a total function of its two arguments.
pub fn readback_delivery_ready(readback_advanced: bool, pending_elapsed: Duration) -> bool {
    // A fresh readback is the normal, immediate completion; the elapsed-time fallback only rescues the
    // pathological identical-frame case, where the unchanged bytes are already correct.
    readback_advanced || pending_elapsed >= READBACK_ADVANCE_BOUND
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The normal path: the readback advanced, so the delivery completes at once regardless of how
    /// little time has passed. This is the every-frame case for any well-behaved application.
    #[test]
    fn a_fresh_readback_completes_immediately() {
        assert!(readback_delivery_ready(true, Duration::ZERO));
    }

    /// The defect's fix: the readback has not advanced (a copy submit fired the trigger, or the draw's
    /// readback DMA has not landed) and we are still within the bound, so the delivery must NOT complete
    /// — completing here is exactly what shipped the previous frame's pixels.
    #[test]
    fn an_unadvanced_readback_waits_within_the_bound() {
        assert!(!readback_delivery_ready(false, READBACK_ADVANCE_BOUND / 2));
    }

    /// The identical-frame fallback: the readback never advances, so past the bound the delivery
    /// completes anyway rather than hanging the application until Mesa's stall-abort.
    #[test]
    fn an_unadvanced_readback_completes_past_the_bound() {
        assert!(readback_delivery_ready(false, READBACK_ADVANCE_BOUND));
        assert!(readback_delivery_ready(false, READBACK_ADVANCE_BOUND + Duration::from_millis(1)));
    }
}
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo test -p rayland-s delivery`
Expected: the three `delivery::tests::*` tests PASS.

- [ ] **Step 4: Commit**

```bash
cd /home/roland/git/rayland
git add crates/rayland-s/src/delivery.rs crates/rayland-s/src/lib.rs
git commit -m "rayland-s: pure readback-delivery-gate decision + bound, unit-tested

The gate that decides when a pending readback delivery may complete: a fresh
readback completes at once; an unadvanced one waits within a 250ms bound and
then completes anyway (the identical-frame fallback). Kept in the lib so it is
tested without an engine or a socket. Wired into progress_thread in the next
commit."
```

---

### Task 3: Wire the gate into `progress_thread`

Replace the unconditional delivery completion with the gate. Today, after the fence, `progress_thread` ships `take_app_blob_writes()` (possibly empty/stale) and always clears `delivery_pending` / advances `last_delivered_submit`. The gate makes completion conditional on the readback having advanced (or the bound expiring).

**Files:**
- Modify: `crates/rayland-s/src/main.rs` (the `new_submit_dispatched` completion block inside `progress_thread`)

**Interfaces:**
- Consumes: `rayland_s::delivery::readback_delivery_ready` from Task 2.

- [ ] **Step 1: Read the current completion block**

Run: `grep -n "new_submit_dispatched\|take_app_blob_writes\|delivery_pending = false\|last_delivered_submit = latest_submit" crates/rayland-s/src/main.rs`
Expected: the `if delivery_pending && new_submit_dispatched {` block and, inside it, `take_app_blob_writes()`, `delivery_pending = false;`, `last_delivered_submit = latest_submit;`.

- [ ] **Step 2: Replace the completion body with the gated version**

Find this block in `progress_thread` (the body of `if delivery_pending && new_submit_dispatched { if let (Some(ctx), Some(ring_idx)) = (ctx_id, ring_idx) { ... } }`), from the `let app = { ... take_app_blob_writes() };` line through `last_delivered_submit = latest_submit;`, and replace it with:

```rust
                // Read what S has written into the application's blobs since the last delivery. This is
                // empty when nothing new landed — a bare copy submit writes a texture image, not an
                // application blob, and an in-flight readback DMA has not changed the blob yet — and
                // non-empty exactly when a readback-bearing submit has produced a new frame. That
                // emptiness is the signal the gate keys on; see `crate::delivery`.
                let app = {
                    let mut session = applier.lock().expect("the applier lock is never poisoned");
                    session.take_app_blob_writes()
                };
                // A non-empty write set means the readback advanced past the last delivered frame.
                let readback_advanced = !app.is_empty();
                // How long this delivery has been pending — only the identical-frame fallback reads it.
                let pending_elapsed = pending_since.map(|t| t.elapsed()).unwrap_or_default();
                if rayland_s::delivery::readback_delivery_ready(readback_advanced, pending_elapsed) {
                    // The frame is ready (fresh readback, or the bounded fallback for an identical
                    // frame whose unchanged bytes are already correct on C). Ship largest-first so the
                    // feedback word lands after the pixels it reports on, then complete the delivery.
                    if ship(&tx, &app).is_err() {
                        return;
                    }
                    delivery_pending = false;
                    pending_since = None;
                    // Only a submit newer than this one may trigger the next delivery.
                    last_delivered_submit = latest_submit;
                }
                // else: the readback has not advanced and we are still within the bound — this was a
                // copy submit or the draw's DMA has not landed. Leave `delivery_pending` set and fall
                // through; the next poll re-fences and re-checks, delivering once the readback advances.
```

- [ ] **Step 3: Verify it builds**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo build -p rayland-s`
Expected: `Finished` (no unused-variable or borrow errors).

- [ ] **Step 4: Verify the existing rayland-s tests still pass**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo test -p rayland-s`
Expected: all tests PASS (the apply/blob/ring-mirror suites plus the new `delivery` tests).

- [ ] **Step 5: Commit**

```bash
cd /home/roland/git/rayland
git add crates/rayland-s/src/main.rs
git commit -m "rayland-s: gate readback delivery on the readback actually advancing

progress_thread no longer completes a delivery the instant a fence returns; it
completes only when take_app_blob_writes reports S wrote a new frame's readback
(or the identical-frame bound expires). This stops the copy submit of a
two-submits-per-frame app from shipping the previous frame's pixels, and
collapses the two deliveries per frame to the one that carries a readback.
Fixes the true-remote stale frame located in
docs/design/2026-07-19-c2-true-remote-mapped-sync.md."
```

---

### Task 4: Two-machine end-to-end proof + committed harness

Prove the fix over a real network: the committed refapp harness already exists; add an icosa variant that runs many times and asserts zero stale frames, mirroring `scripts/c1-two-machine.sh`. This is the oracle the design (§8) calls for and is the only test that can exercise the true-network timing.

**Files:**
- Create: `scripts/c2-icosa-two-machine.sh`

**Interfaces:**
- Consumes: the built `rayland-c`, `rayland-s`, `rayland-icosa-cpu` binaries and ssh access to `apollo` (C); this host is `dop561` (S).

- [ ] **Step 1: Write the committed harness**

Create `scripts/c2-icosa-two-machine.sh` (mark executable):

```bash
#!/usr/bin/env bash
#
# (c)2 — the readback-completion gate, proven over a real network.
# ============================================================================================
#
# WHAT THIS PROVES
#   `rayland-icosa-cpu` (120 frames, a spinning icosahedron textured with a per-frame CPU fractal
#   written into mapped HOST_COHERENT memory) runs on C (apollo) through `rayland-c`, is replayed
#   on S (dop561) by `rayland-s`, and read back. Before the readback-completion gate, ~2/120 frames
#   came back as the WHOLE PREVIOUS frame over a real link (0/120 on loopback) — a readback-delivery
#   lag on S, not a forward relay race (docs/design/2026-07-19-c2-true-remote-mapped-sync.md). After
#   the gate, every frame must match native-on-S across many runs.
#
# CORRECTNESS ASSERTION
#   Compare each relayed frame against `rayland-icosa-cpu` run NATIVELY ON S (same Intel GPU), so
#   only the transport differs and every frame must be bit-identical. Do NOT compare against the app
#   run on C (AMD GPU, a different rasteriser).
#
# WHY no VN_DEBUG=no_abort (do not add it): Mesa's ~3.5s stall-abort is the stall detector.
#
# Usage:  scripts/c2-icosa-two-machine.sh [RUNS]     # default 10 runs; exits non-zero on any stale frame
set -euo pipefail

C_HOST="${C_HOST:-apollo}"
S_IP="${S_IP:-192.168.1.192}"
PORT="${PORT:-9402}"
RUNS="${1:-10}"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rayland-c1-target}"
BIN="$TARGET_DIR/release"
SOCK="/tmp/rl-c2-icosa.sock"

echo "### building rayland-c, rayland-s, rayland-icosa-cpu (release; the app must be fast) ###"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release -p rayland-c -p rayland-s -p rayland-icosa-cpu

echo "### native baseline on S (Intel GPU, no Venus) ###"
rm -rf /tmp/icosa-native && mkdir -p /tmp/icosa-native
"$BIN/rayland-icosa-cpu" /tmp/icosa-native >/dev/null
echo "native frames: $(ls /tmp/icosa-native/frame_*.png | wc -l)"

echo "### deploy C-side binaries to $C_HOST ###"
scp -q "$BIN/rayland-c" "$BIN/rayland-icosa-cpu" "$C_HOST:/tmp/"
ssh "$C_HOST" 'chmod +x /tmp/rayland-c /tmp/rayland-icosa-cpu'

S_PID=""
cleanup() { [ -n "$S_PID" ] && kill "$S_PID" 2>/dev/null || true; ssh "$C_HOST" 'pkill -f /tmp/rayland-c; pkill -f /tmp/rayland-icosa-cpu' 2>/dev/null || true; }
trap cleanup EXIT

total_stale=0
for run in $(seq 1 "$RUNS"); do
  ssh "$C_HOST" 'rm -rf /tmp/icosa-relay; mkdir -p /tmp/icosa-relay'
  RAYLAND_C1_NO_PRESENT=1 RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" "$BIN/rayland-s" >"/tmp/rayland-s-c2-$run.log" 2>&1 &
  S_PID=$!; sleep 3
  kill -0 "$S_PID" 2>/dev/null || { echo "rayland-s died:"; cat "/tmp/rayland-s-c2-$run.log"; exit 1; }
  ssh "$C_HOST" "
    RAYLAND_C1_S_ADDR=$S_IP:$PORT RAYLAND_C1_SOCKET=$SOCK nohup /tmp/rayland-c >/tmp/rayland-c-icosa.log 2>&1 &
    sleep 3
    VN_DEBUG=vtest VN_PERF=no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback \
    VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json VTEST_SOCKET_NAME=$SOCK \
    env -u VK_LOADER_DRIVERS_SELECT /tmp/rayland-icosa-cpu /tmp/icosa-relay >/dev/null 2>&1 || echo APP_EXIT_NONZERO
  "
  sleep 1
  rm -rf /tmp/icosa-relay && scp -q -r "$C_HOST:/tmp/icosa-relay" /tmp/icosa-relay
  kill "$S_PID" 2>/dev/null || true; S_PID=""
  stale=0
  for f in /tmp/icosa-native/frame_*.png; do
    b=$(basename "$f")
    cmp -s "$f" "/tmp/icosa-relay/$b" 2>/dev/null || stale=$((stale + 1))
  done
  echo "run $run/$RUNS: $stale stale frame(s)"
  total_stale=$((total_stale + stale))
done

echo "TOTAL stale frames over $RUNS runs: $total_stale"
[ "$total_stale" -eq 0 ] || { echo "FAIL: stale frames remain — the gate did not fix it (see docs/design §9)"; exit 1; }
echo "PASS: 0 stale frames over $RUNS runs"
```

- [ ] **Step 2: Make it executable**

```bash
chmod +x /home/roland/git/rayland/scripts/c2-icosa-two-machine.sh
```

- [ ] **Step 3: Run it (10 runs) and confirm zero stale frames**

Run: `cd /home/roland/git/rayland && ./scripts/c2-icosa-two-machine.sh 10`
Expected: each run prints `0 stale frame(s)`, and the script ends `PASS: 0 stale frames over 10 runs`.

If any run shows stale frames, STOP and re-open the design's §9 checkpoint: the application's readback-wait may be released by the head-advance rather than the readback delivery, which would require a separate C-side change. Do not paper over it.

- [ ] **Step 4: Commit**

```bash
cd /home/roland/git/rayland
git add scripts/c2-icosa-two-machine.sh
git commit -m "scripts: committed two-machine icosa oracle for the readback gate

Runs rayland-icosa-cpu through the relay N times and asserts every frame is
bit-identical to native-on-S. This is the design's end-to-end oracle: before the
gate it caught ~2/120 stale frames over the real link; after the gate it passes
0/N. Network-gated (needs apollo + dop561), not part of default cargo test."
```

---

### Task 5: Update the status docs to "fixed"

With the fix landed and proven, update the running record so the next reader sees a solved problem, not an open one.

**Files:**
- Modify: `CLAUDE.md` (the `(c)2` bullet)
- Modify: `docs/icosa-fixtures.md` (§8)
- Modify: `docs/design/2026-07-19-c2-true-remote-mapped-sync.md` (add a closing "Fixed by" line)

- [ ] **Step 1: Update the CLAUDE.md `(c)2` bullet**

In `CLAUDE.md`, in the `(c)2` bullet, change the sentence that currently reads "The next (c)2 step is a **`rayland-s` readback-barrier fix** ..." to state that the fix has landed:

```
  **Fixed (2026-07-19):** the `rayland-s` readback-completion gate — a delivery completes only once
  `take_app_blob_writes` reports the readback advanced past the last delivered frame (or a 250 ms
  identical-frame bound expires), so a two-submits-per-frame app's copy submit can no longer ship the
  previous frame's pixels. Proven over the real network by `scripts/c2-icosa-two-machine.sh` (0 stale
  frames across 10 runs where ~2/120 appeared before). Design and plan:
  [`docs/design/2026-07-19-c2-readback-completion-gate.md`](docs/design/2026-07-19-c2-readback-completion-gate.md).
  Still open: multi-queue support.
```

- [ ] **Step 2: Update `docs/icosa-fixtures.md` §8**

In §8, change the closing sentence "making it a repeatable fixture-vs-native comparison, and then fixing S's readback barrier, is where (c)2's remaining subject matter now lives." to:

```
That repeatable fixture-vs-native comparison now exists as `scripts/c2-icosa-two-machine.sh`, and
S's readback barrier is fixed: the readback-completion gate delivers a frame only once S's readback
write advances, and the script confirms 0 stale frames over 10 real-network runs. See
`design/2026-07-19-c2-readback-completion-gate.md`.
```

- [ ] **Step 3: Add a closing line to the findings doc**

At the end of `docs/design/2026-07-19-c2-true-remote-mapped-sync.md`, append:

```markdown

## Fixed by

`docs/design/2026-07-19-c2-readback-completion-gate.md` (the readback-completion gate) — landed and
proven 0 stale frames over 10 real-network runs via `scripts/c2-icosa-two-machine.sh`.
```

- [ ] **Step 4: Verify the docs reference real paths**

Run: `cd /home/roland/git/rayland && ls docs/design/2026-07-19-c2-readback-completion-gate.md scripts/c2-icosa-two-machine.sh`
Expected: both paths exist.

- [ ] **Step 5: Commit**

```bash
cd /home/roland/git/rayland
git add CLAUDE.md docs/icosa-fixtures.md docs/design/2026-07-19-c2-true-remote-mapped-sync.md
git commit -m "c2: record the readback-completion gate as landed and proven"
```

---

## Self-Review

**Spec coverage:**
- §1 (defect) / §2 (constraint): motivation carried into `delivery.rs` module docs (Task 2). ✓
- §3 (existing machinery): Task 3 uses `take_app_blob_writes` as the advance signal. ✓
- §4 (the gate): Task 2 (decision) + Task 3 (wiring). ✓
- §5 (correctness/generality): the gate keys on S-observed writes, no ring parse — Task 2/3. ✓
- §6 (edge cases): identical-frame bound in `READBACK_ADVANCE_BOUND` (Task 2); teardown/deadline untouched by Task 3. ✓
- §7 (no forward/wire change): only `rayland-s` files touched. ✓
- §8 (testing): unit test (Task 2) + committed e2e oracle (Task 4). ✓
- §9 (release-path checkpoint): Task 4 Step 3 escalation note. ✓
- §10 (instrumentation cleanup): Task 1 reverts the probe. ✓

**Placeholder scan:** no TBD/TODO; every code step shows complete code; the bound value is concrete (250 ms) with justification. ✓

**Type consistency:** `readback_delivery_ready(bool, Duration) -> bool` and `READBACK_ADVANCE_BOUND: Duration` are defined in Task 2 and consumed with those exact names/types in Task 3. `take_app_blob_writes` used per its real signature (`-> Vec<S2C>`, `.is_empty()`). ✓
