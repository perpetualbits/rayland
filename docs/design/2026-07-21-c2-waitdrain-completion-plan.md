# (c)2 wait-drain completion — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the `N == N−1` readback-stale residual by keying S's return path on the application's own `vkWaitForFences` completion (a real barrier already in the ring) instead of the weak empty-submit fence, and ordering the readback pixels ahead of the head-advance that releases the application.

**Architecture:** S decodes the app's `vkWaitForFences` inline in the ring (like it already decodes `vkQueueSubmit`), tracks the free-running end position of the latest one, and delivers the readback when `head` drains past it — the moment the app's submit (readback copy included) is provably complete. At that point a `res6` content diff reliably tells a draw (ship the pixels, then release the head) from an upload copy (release the head only). A capped head-advance holds the releasing frontier just below the wait until the pixels are on the wire.

**Tech Stack:** Rust; `rayland-vtest` (ring decode, no GPU deps), `rayland-s` (the daemon), `rayland-engine` (virglrenderer FFI). Proof harness: `scripts/c2-icosa-two-machine.sh` (C = apollo, S = dop561).

**Spec:** [`2026-07-21-c2-waitdrain-completion.md`](2026-07-21-c2-waitdrain-completion.md). **Evidence:** [`2026-07-20-c2-fence-empty-submit-finding.md`](2026-07-20-c2-fence-empty-submit-finding.md).

## Global Constraints

- **All Rust.** Our code is 100% Rust; virglrenderer is a linked C dependency behind the `RenderEngine` trait.
- **Doc-comment (`///`/`//!`) every function, type, trait, module** — what it does, inputs/outputs, failure modes, domain pitfalls. **Intent comment on every non-trivial line** (the *why*, never a syntax restatement). **Code and comments must always agree**; fix a stale comment in the same edit.
- **`rayland-vtest` has no GPU dependencies** (only `libc`, `thiserror`); `tests/no_gpu_linkage.rs` guards it. The decode work in Task 2 must not add any.
- **S/C vocabulary:** S = the strong machine (GPU, display, where the user sits); C = where the app runs. Do not invert.
- **Never `pkill`/pattern-kill.** Kill only by an exact captured PID (`cmd & PID=$!`). The oracle already does PID-based cleanup.
- **Licenses unchanged:** `rayland-vtest` LGPL, `rayland-s` GPL.
- **Build/test target dir:** `CARGO_TARGET_DIR=/tmp/rayland-c1-target` (matches the oracle, avoids a cold rebuild).

## Wire facts (pinned from `vn_protocol_renderer_fence.h`, virglrenderer 1.3.0)

`vkWaitForFences` encodes as `[type u32][flags u32]` header then, in decode order (`vn_decode_vkWaitForFences_args_temp`): `device` (u64 handle), `fenceCount` (u32), array-size marker (u64, equals `fenceCount` when non-NULL), `pFences[fenceCount]` (u64 handles), `waitAll` (u32 `VkBool32`), `timeout` (u64). For the single-fence pattern (`fenceCount == 1`) the byte layout is therefore:

| field | offset | size |
|-------|-------:|-----:|
| command type (== 39) | 0 | 4 |
| flags (== 0) | 4 | 4 |
| device handle | 8 | 8 |
| fenceCount (== 1) | 16 | 4 |
| array marker (u64, == fenceCount) | 20 | 8 |
| pFences[0] | 28 | 8 |
| waitAll | 36 | 4 |
| timeout | 40 | 8 |
| **end** | **48** | — |

Command type `VK_COMMAND_TYPE_vkWaitForFences_EXT = 39`. This mirrors `vkQueueSubmit`'s existing layout constants (`QUEUE_SUBMIT_ARRAY_MARKER_OFFSET = 20`, handles at offset 8), so the array-marker consistency check is identical in spirit.

---

### Task 1: Spike — is `vkWaitForFences` carried *inline* in the ring? (decision gate)

The whole design assumes S can byte-scan a `RingDelta` for the wait, as it does for the submit. Mesa's venus encoder may instead pack app commands into an out-of-line stream referenced by `vkExecuteCommandStreamsMESA` (command type 180). Settle this **before** building. This task is investigation + a throwaway probe, not TDD.

**Files:**
- Read (no edit): Mesa venus source — `vn_ring.c` / `vn_cs_encoder.c` (whichever chooses inline vs out-of-line command emission). If no local Mesa checkout, read via the system Mesa or clone; also cross-read virglrenderer `vkr_ring.c:207-240` (`vkr_ring_submit_cmd`) which dispatches whatever the ring carries.
- Modify (throwaway): `crates/rayland-s/src/apply.rs` — in the `C2S::RingDelta` arm, under the existing `RAYLAND_C2_FENCEPROBE` env gate, scan `&bytes` for command type `39` and `eprintln!` a one-line hit count per delta.

- [ ] **Step 1: Read the encoder.** Determine whether `vkWaitForFences` is emitted inline in the ring buffer or bundled into an out-of-line execute stream. Record the finding (file:line) in the plan's progress ledger.

- [ ] **Step 2: Empirical confirmation.** Add a throwaway scan (env-gated) that counts, per delta, offsets where `read_u32_le(&bytes, off) == 39` with `flags == 0` and the fenceCount/marker consistency (offset+16 == 1, offset+20 u64 == 1). Build and run one two-machine run:

```bash
CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo build --release -p rayland-s
RAYLAND_C2_FENCEPROBE=/tmp/waitscan.txt scripts/c2-icosa-two-machine.sh 1
grep -c "waitscan hit" /tmp/rayland-s-c2-1.log   # expect ~2 per frame (upload wait + draw wait)
```

- [ ] **Step 3: Decision gate.**
  - **Inline (hits ≈ 2/frame):** proceed to Task 2 as written. Revert the throwaway scan.
  - **Out-of-line (no inline hits):** STOP. The design needs revision — either decode the out-of-line stream, or key on the ring draining and *staying* drained past the execute boundary. Do not build Tasks 2–5 against the wrong assumption; return to brainstorming with the finding.

- [ ] **Step 4: Commit** (only the finding is durable; the throwaway scan is reverted):

```bash
git add docs/design/2026-07-21-c2-waitdrain-completion-plan.md   # ledger note of the finding
git commit -m "c2(waitdrain): spike — confirm vkWaitForFences is inline in the ring"
```

---

### Task 2: `find_wait_for_fences` ring decoder

**Files:**
- Modify: `crates/rayland-vtest/src/venus_ring/decode.rs`
- Test: same file's `#[cfg(test)] mod tests` (this crate keeps decode tests inline).

**Interfaces:**
- Produces: `pub fn find_wait_for_fences(stream: &[u8], device_handle: u64) -> Option<usize>` — the **start** offset of the *latest* single-fence `vkWaitForFences` for `device_handle` in `stream`, or `None`. Also `pub const WAIT_FOR_FENCES_SPAN: usize = 48;` and `pub const VK_COMMAND_TYPE_VK_WAIT_FOR_FENCES: u32 = 39;`.

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn find_wait_for_fences_finds_the_latest_single_fence_wait_for_the_device() {
    // Two waits for device 0xD; the scanner must return the LATER one's start offset.
    let dev: u64 = 0xD;
    let mut s = Vec::new();
    let mut push_wait = |s: &mut Vec<u8>, fence: u64| {
        s.extend_from_slice(&39u32.to_le_bytes());   // command type
        s.extend_from_slice(&0u32.to_le_bytes());    // flags
        s.extend_from_slice(&dev.to_le_bytes());     // device handle
        s.extend_from_slice(&1u32.to_le_bytes());    // fenceCount
        s.extend_from_slice(&1u64.to_le_bytes());    // array marker == fenceCount
        s.extend_from_slice(&fence.to_le_bytes());   // pFences[0]
        s.extend_from_slice(&1u32.to_le_bytes());    // waitAll
        s.extend_from_slice(&u64::MAX.to_le_bytes()); // timeout
    };
    push_wait(&mut s, 0xF1);
    let second = s.len();
    push_wait(&mut s, 0xF2);
    assert_eq!(find_wait_for_fences(&s, dev), Some(second));
}

#[test]
fn find_wait_for_fences_rejects_a_wait_for_a_different_device_and_stray_bytes() {
    let mut s = vec![0u8; 200];               // stray bytes: no valid command
    assert_eq!(find_wait_for_fences(&s, 0xD), None);
    // A wait for a different device must not match.
    s.clear();
    s.extend_from_slice(&39u32.to_le_bytes());
    s.extend_from_slice(&0u32.to_le_bytes());
    s.extend_from_slice(&0xBEEFu64.to_le_bytes()); // wrong device
    s.extend_from_slice(&1u32.to_le_bytes());
    s.extend_from_slice(&1u64.to_le_bytes());
    s.extend_from_slice(&0xF1u64.to_le_bytes());
    s.extend_from_slice(&1u32.to_le_bytes());
    s.extend_from_slice(&0u64.to_le_bytes());
    assert_eq!(find_wait_for_fences(&s, 0xD), None);
}
```

- [ ] **Step 2: Run it, verify it fails.**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo test -p rayland-vtest find_wait_for_fences`
Expected: FAIL — `cannot find function find_wait_for_fences`.

- [ ] **Step 3: Implement.** Add near `find_queue_submit`, with full doc-comment and intent comments matching the crate's style:

```rust
/// The `VkCommandTypeEXT` for `vkWaitForFences` (venus-protocol, `= 39`). See the plan's wire table.
pub const VK_COMMAND_TYPE_VK_WAIT_FOR_FENCES: u32 = 39;
/// Bytes of a single-fence (`fenceCount == 1`) `vkWaitForFences` command — through `timeout`.
/// Multi-fence waits are out of scope (the single-queue synchronous pattern (c)1 pins), so this is fixed.
pub const WAIT_FOR_FENCES_SPAN: usize = 48;

/// Find the **latest** single-fence `vkWaitForFences` for `device_handle` in a Venus command stream.
///
/// Mirrors [`find_queue_submit`]: a linear byte scan (the app's commands are inline in the ring), taking
/// the *latest* match so a stale earlier wait is never mistaken for this frame's. The consistency checks —
/// command type, zero async flags, the device handle, `fenceCount == 1`, and the array-size marker equal
/// to the count — are strong enough that stray argument bytes cannot satisfy them.
///
/// # Inputs / outputs
/// - `stream`: one delta's bytes (linear, un-wrapped).
/// - `device_handle`: the app's `VkDevice`, latched from its `vkGetDeviceQueue2`.
/// - Returns the **start** offset of the latest matching wait, or `None`. The caller adds
///   [`WAIT_FOR_FENCES_SPAN`] to get the command's end — the position `head` must reach for the wait to
///   have *returned* (i.e. the fence signalled, the submit complete).
pub fn find_wait_for_fences(stream: &[u8], device_handle: u64) -> Option<usize> {
    let mut latest = None;
    let mut offset = 0usize;
    while offset + WAIT_FOR_FENCES_SPAN <= stream.len() {
        let is_match = read_u32_le(stream, offset) == Some(VK_COMMAND_TYPE_VK_WAIT_FOR_FENCES)
            // Synchronous wait: async flags are 0.
            && read_u32_le(stream, offset + 4) == Some(0)
            // This device, latched from vkGetDeviceQueue2.
            && read_u64_le(stream, offset + 8) == Some(device_handle)
            // Exactly one fence — the pattern we support — and the array marker must agree with it.
            && read_u32_le(stream, offset + 16) == Some(1)
            && read_u64_le(stream, offset + 20) == Some(1);
        if is_match {
            // Keep scanning: we want the latest wait, not the first.
            latest = Some(offset);
        }
        offset += 4;
    }
    latest
}
```

- [ ] **Step 4: Run tests, verify pass.**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo test -p rayland-vtest find_wait_for_fences`
Expected: PASS (both tests).

- [ ] **Step 5: Guard the no-GPU invariant.**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo test -p rayland-vtest --test no_gpu_linkage`
Expected: PASS.

- [ ] **Step 6: Commit.**

```bash
git add crates/rayland-vtest/src/venus_ring/decode.rs
git commit -m "c2(waitdrain): find_wait_for_fences — decode the app's wait inline in the ring"
```

---

### Task 3: Track `latest_wait_end_pos` in the `Applier`

**Files:**
- Modify: `crates/rayland-s/src/apply.rs` — add the field, track it in the `C2S::RingDelta` arm beside `latest_submit_pos`, add an accessor.
- Modify: `crates/rayland-s/src/apply.rs` imports — add `find_wait_for_fences` to the `use rayland_vtest::venus_ring::{...}` line (currently imports `find_queue_submit`).

**Interfaces:**
- Consumes: `find_wait_for_fences` (Task 2), the latched `q.device_handle` (already tracked on the queue record).
- Produces: `pub fn latest_wait_end_pos(&self) -> Option<u32>` — the free-running ring position of the **end** of the latest decoded `vkWaitForFences`, or `None`.

- [ ] **Step 1: Add the field.** Beside `latest_submit_pos` (apply.rs:312), with a doc-comment explaining it is the free-running **end** position (start + span), wrap-safe, tracked from the linear delta stream exactly like `latest_submit_pos`:

```rust
/// Free-running ring position of the **end** of the latest `vkWaitForFences` the app issued on its
/// device. `head >= this` means that wait has returned — the fence it waited on signalled, so the
/// submit (readback copy included) is complete and `res6` is host-visible and tear-free. This is the
/// (c)2 return-path trigger; see [`Self::latest_wait_end_pos`] and the wait-drain design doc.
/// Tracked from the linear delta bytes (never a masked buffer offset), so it survives the ring wrap —
/// the same discipline as [`Self::latest_submit_pos`].
latest_wait_end_pos: Option<u32>,
```
Initialise it to `None` wherever `latest_submit_pos` is initialised.

- [ ] **Step 2: Track it in the delta arm.** In the `Some(q) => { ... }` block (apply.rs:728-747), after the `find_queue_submit` branch, add (the two are not mutually exclusive — a delta may carry both, so use a separate `if`, not `else if`):

```rust
// A wait for this frame's fence: record the **free-running end** position. `progress_thread` fires
// the readback delivery when `head` reaches it — the moment the app's submit is provably complete.
// `bytes` spans free-running `[tail - bytes.len(), tail)`, identical to the submit tracking above.
if let Some(off) = find_wait_for_fences(&bytes, q.device_handle) {
    let frontier_before = tail.wrapping_sub(bytes.len() as u32);
    self.latest_wait_end_pos =
        Some(frontier_before.wrapping_add((off + WAIT_FOR_FENCES_SPAN) as u32));
}
```
(Import `WAIT_FOR_FENCES_SPAN` alongside `find_wait_for_fences`.)

- [ ] **Step 3: Add the accessor**, beside `latest_submit_pos()` (apply.rs:1038), fully doc-commented:

```rust
/// See [`Self::latest_wait_end_pos`] the field. Returns the end position of the latest decoded
/// `vkWaitForFences`, or `None` before the app has issued one.
pub fn latest_wait_end_pos(&self) -> Option<u32> {
    self.latest_wait_end_pos
}
```

- [ ] **Step 4: Build (no behavior change yet; the field is not read by `progress_thread` until Task 5).**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo build -p rayland-s`
Expected: compiles clean.

- [ ] **Step 5: Commit.**

```bash
git add crates/rayland-s/src/apply.rs
git commit -m "c2(waitdrain): track latest_wait_end_pos from the ring deltas"
```

---

### Task 4: `take_progress_capped` on `RingMirror`

The head-advance that releases the app must be held just below the wait until the readback ships. `take_progress` today reports the full moved `head` and advances `reported_head` to it (ring_mirror.rs:399-409). Add a capped variant that reports `min(head, cap)` and advances `reported_head` only to what it reported, so the held-back remainder ships on a later uncapped call.

**Files:**
- Modify: `crates/rayland-s/src/ring_mirror.rs`
- Test: same file's `#[cfg(test)] mod tests`.

**Interfaces:**
- Produces: `pub fn take_progress_capped(&mut self, blob: &HostBlob, cap: u32) -> Option<u32>` — reports `min(head, cap)` if it exceeds `reported_head`, advancing `reported_head` to that reported value; else `None`. `take_progress(blob)` (uncapped) is unchanged and is the release call.

- [ ] **Step 1: Write the failing test.** (Uses the existing test helper that builds a `HostBlob` with a settable head word — mirror whatever `take_progress`'s existing tests use; if none, construct via the module's test constructor.)

```rust
#[test]
fn take_progress_capped_holds_at_the_cap_then_releases_the_remainder() {
    let (mut mirror, blob) = test_mirror_with_head(0);
    set_head(&blob, 100);
    // Capped at 60: report only up to the cap, hold the rest.
    assert_eq!(mirror.take_progress_capped(&blob, 60), Some(60));
    // Still capped at 60, head unchanged: nothing new to report.
    assert_eq!(mirror.take_progress_capped(&blob, 60), None);
    // Uncapped release: the held-back remainder (61..=100) ships now.
    assert_eq!(mirror.take_progress(&blob), Some(100));
}

#[test]
fn take_progress_capped_reports_full_head_when_cap_is_beyond_it() {
    let (mut mirror, blob) = test_mirror_with_head(0);
    set_head(&blob, 40);
    // Cap above head: the cap does not bite; report the whole head.
    assert_eq!(mirror.take_progress_capped(&blob, 1000), Some(40));
}
```
(If `test_mirror_with_head`/`set_head` do not exist, add minimal test-only helpers in the same `mod tests`, matching how `head`/`take_progress` are exercised today.)

- [ ] **Step 2: Run it, verify it fails.**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo test -p rayland-s take_progress_capped`
Expected: FAIL — `no method named take_progress_capped`.

- [ ] **Step 3: Implement**, beside `take_progress`, fully doc-commented (explain the free-running `min`, why `reported_head` advances only to the reported value, and the wrap-safety assumption the module already documents):

```rust
/// Like [`Self::take_progress`], but never reports past `cap`: returns `Some(min(head, cap))` when that
/// exceeds the last reported value, advancing `reported_head` only to what it reported so the held-back
/// remainder is emitted by a later (uncapped) [`Self::take_progress`]. Used by the (c)2 return path to
/// hold the head just below the app's `vkWaitForFences` until that frame's readback pixels are on the
/// wire, so C never releases the app onto stale local `res6`.
///
/// `head`, `cap`, and `reported_head` are free-running `u32` counters in one coordinate space (the wait
/// end position the caller passes as `cap` is computed the same way), and never approach the 2^32 wrap in
/// a session, so the `min` and the `>` are direct — the same wrap-safety the module relies on throughout.
///
/// # Inputs / outputs
/// - `blob`: the ring blob's mapping. `cap`: the free-running frontier not to report past.
/// - Returns `Some(reported)` the first time a new capped value is reachable, else `None`.
pub fn take_progress_capped(&mut self, blob: &HostBlob, cap: u32) -> Option<u32> {
    let head = self.head_word(blob).load(Ordering::Acquire);
    // Report no further than the cap; the remainder ships when the caller later calls take_progress.
    let reported = head.min(cap);
    if reported <= self.reported_head {
        return None;
    }
    self.reported_head = reported;
    Some(reported)
}
```

- [ ] **Step 4: Run tests, verify pass.**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo test -p rayland-s take_progress_capped`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add crates/rayland-s/src/ring_mirror.rs
git commit -m "c2(waitdrain): take_progress_capped — hold the head at the readback wait"
```

---

### Task 5: Rewire `progress_thread` — wait-drain trigger, content discriminate, cap + order, retire the empty-submit fence

This is the integration. Its correctness proof is the loopback e2e (regression) and the two-machine oracle (Task 6); the pieces it composes were unit-tested in Tasks 2–4.

**Files:**
- Modify: `crates/rayland-s/src/main.rs` — `progress_thread` (main.rs:256-384) and the per-ring progress step it calls.
- Modify: `crates/rayland-s/src/apply.rs` — `take_ring_progress` needs a capped mode (or a sibling) so the head report can be capped at a frontier; thread `latest_wait_end_pos` through. (`take_ring_progress` calls `RingMirror::take_progress` per ring; add a capped path using `take_progress_capped`.)

**Interfaces:**
- Consumes: `Applier::latest_wait_end_pos()` (Task 3), `Applier::take_app_blob_writes()` (existing, the res6 diff), `RingMirror::take_progress_capped` (Task 4), `take_progress` (existing, the release).

- [ ] **Step 1: Add a capped ring-progress path in `Applier`.** Add `pub fn take_ring_progress_capped(&mut self, cap: u32) -> Vec<S2C>` mirroring `take_ring_progress` (apply.rs:1065) but calling `mirror.take_progress_capped(blob, cap)` for the app's ring (the ring carrying the queue; other rings, if any, use the uncapped path). Doc-comment it as the (c)2 hold path. Keep `take_ring_progress` for the uncapped release.

- [ ] **Step 2: Rewrite the delivery logic in `progress_thread`.** Replace the submit-drain trigger + `wait_for_work_retired` fence (main.rs:284-354) with the wait-drain algorithm. The new per-iteration logic:

```text
read under one lock: latest_wait_end = session.latest_wait_end_pos()
                     head           = session.app_ring_head()          // uncapped peek
1. If a delivery is pending (we are holding the head at `pending_wait_end`):
     ship capped ring progress: session.take_ring_progress_capped(pending_wait_end)   // releases everything up to the wait
     if head >= pending_wait_end:                                    // the wait has RETURNED — submit complete
         app = session.take_app_blob_writes()                        // res6 diff, now reliable (tear-free)
         if !app.is_empty():                                         // DRAW: fresh pixels
             ship(app)                                               // pixels first...
             ship(session.take_ring_progress())                      // ...then the full head releases the app
             pending = None
         else:                                                       // UPLOAD COPY: nothing to ship
             ship(session.take_ring_progress())                      // release the full head so the app proceeds to the draw
             pending = None
2. Else (no delivery pending):
     ship(session.take_venus_blob_writes())                          // reply arena, as today
     ship(session.take_ring_progress_capped(latest_wait_end or head))// normal progress, capped at the next wait
     if latest_wait_end is a NEW wait (> last_handled_wait_end):
         pending = Some(latest_wait_end); last_handled_wait_end = latest_wait_end
```
Notes for the implementer:
- Track `pending_wait_end: Option<u32>` and `last_handled_wait_end: Option<u32>` (replacing `delivery_pending`/`last_delivered_submit`/`pending_since` semantics; keep a `pending_since` deadline as the loud-failure backstop — Step 4).
- **Ship order on the draw branch is load-bearing:** `BlobData(res6)` before the releasing `RingProgress`, so C applies pixels before releasing the app (the whole point). This matches the existing "largest-first, feedback last" discipline.
- **The empty (upload copy) branch releases the full head** and does NOT complete a delivery — never hold a copy's head (that is the §3 deadlock of the release-ordering doc).
- Remove the `engine.wait_for_work_retired(...)` call and the `EngineClient` fence usage from `progress_thread`. `progress_thread` no longer needs `engine`; drop the parameter if nothing else uses it. **Keep** `VirglEngine::wait_for_work_retired`/`wait_for_context_fence` — `read_back`/presentation still use them.

- [ ] **Step 3: Keep the queue-teardown and deadline guards.** Preserve the `retirement_ring_idx().is_none()` drop (device destroyed → drop any pending) and the `QUEUE_REGISTER_DEADLINE` loud-failure backstop, re-expressed against `pending_wait_end` (a pending wait-drain that never arrives within the deadline ends the session with the existing message).

- [ ] **Step 4: Build.**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo build -p rayland-s`
Expected: compiles clean (fix any now-unused `engine`/imports).

- [ ] **Step 5: Loopback regression — the fast correctness gate.**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo test -p rayland-s --test loopback_e2e -- --nocapture`
Expected: `refapp_renders` and `icosa_cpu_renders` PASS (the gate asks 3 consecutive 0/120). If `icosa_cpu` regresses, STOP and debug before the two-machine run — loopback is the cheap oracle.

- [ ] **Step 6: Commit.**

```bash
git add crates/rayland-s/src/main.rs crates/rayland-s/src/apply.rs
git commit -m "c2(waitdrain): key the return path on the app's wait_for_fences drain, order pixels before release"
```

---

### Task 6: The proof — two-machine, ≥ 20 runs clean, plus regression

**Files:** none (runs the committed oracle).

- [ ] **Step 1: Batches over the real network.** Run four batches of 5 (the residual is intermittent; ≥ 20 runs makes a survivor visible). A single long run can be wall-clock-killed — keep batches ≤ 5.

```bash
for b in 1 2 3 4; do scripts/c2-icosa-two-machine.sh 5; done
```
Expected: **0 stale frames across all 20 runs.** No `SIGABRT`, no `QUEUE_REGISTER_DEADLINE`, no `invalid ring_idx`, clean shutdown.

- [ ] **Step 2: If any run is stale**, do not paper over it. Re-enable the throwaway fence-probe (still present until Task 7) to capture whether the stale frame is a wait-drain miss (a wait not decoded → head advanced uncapped) or a genuine ordering hole, and root-cause per systematic-debugging before continuing. Judge over many runs, never one; watch for the Heisenbug (heavy per-poll logging hides it).

- [ ] **Step 3: Refapp regression over the network** (bit-identical apollo→dop561, must still hold):

```bash
scripts/c2-refapp-two-machine.sh 2>/dev/null || echo "(use the refapp two-machine script if present; else confirm refapp via loopback_e2e)"
```
Expected: bit-identical, presents on S's screen, no wedge.

- [ ] **Step 4: Record the result** in the diary (`docs/DIARY.md`) and the progress ledger — honestly, including the run count and any residual. If clean over ≥ 20 runs, state the confidence and the remaining caveat (single-queue only).

---

### Task 7: Remove the throwaway fence-probe

Once G is proven (Task 6 clean), delete the measurement scaffolding — it was for the finding and the verification, and the codebase discipline is no dead instrumentation.

**Files:**
- Modify: `crates/rayland-s/src/apply.rs` — remove `readback_probe` and `sampled_fp`.
- Modify: `crates/rayland-s/src/main.rs` — remove the `RAYLAND_C2_FENCEPROBE` block and the per-fence-poll record in `progress_thread` (and any residue of the Task 1 spike scan if not already reverted).

- [ ] **Step 1: Delete the probe code and its env gate.**

- [ ] **Step 2: Build + full pure tests.**

Run: `CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo test -p rayland-s --lib && CARGO_TARGET_DIR=/tmp/rayland-c1-target cargo build -p rayland-s`
Expected: PASS, clean build.

- [ ] **Step 3: One more two-machine batch** to confirm removing the probe changed nothing:

```bash
scripts/c2-icosa-two-machine.sh 5
```
Expected: 0 stale.

- [ ] **Step 4: Commit.**

```bash
git add crates/rayland-s/src/apply.rs crates/rayland-s/src/main.rs
git commit -m "c2(waitdrain): remove the throwaway fence-probe now the fix is proven"
```

---

## Task-by-task update of the durable record

After Task 6 (and again after Task 7), update: `CLAUDE.md`'s `(c)2` bullet (the current-truth summary), `docs/DIARY.md` (the story, honestly), and `.superpowers/sdd/progress.md` (the ledger). The spec and finding docs already exist; link them.

## Self-review notes (author)

- **Spec coverage:** §3.1 → Tasks 2–3; §3.2 → Task 5 Step 2; §3.3 → Task 5 Step 2 (content branch); §3.4 → Tasks 4 + 5; §3.5 (retire the fence) → Task 5 Step 2; §5 testing → Tasks 5–6; §6 risks → Task 1 (inline), Wire-facts table (layout), Task 4 (wrap), Task 5 (loopback regression). cosmic-comp/presentation: unaffected; confirmed via `loopback_e2e`/refapp.
- **Open dependency:** Task 1 is a hard gate — if `vkWaitForFences` is out-of-line, Tasks 2–5 change. That is deliberate: the one unverified premise is settled first, cheaply.
- **Naming consistency:** `find_wait_for_fences`, `WAIT_FOR_FENCES_SPAN`, `latest_wait_end_pos`, `take_progress_capped`, `take_ring_progress_capped`, `pending_wait_end`, `last_handled_wait_end` — used identically across tasks.
