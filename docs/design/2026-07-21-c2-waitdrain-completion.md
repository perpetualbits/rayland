# (c)2 — the wait-drain completion signal: use the application's own `vkWaitForFences`, not S's proxy fence

**Status:** design/spec, 2026-07-21. Fixes the `N == N−1` stale-frame residual left by the readback-completion
gate. It rests on the evidence in
[`2026-07-20-c2-fence-empty-submit-finding.md`](2026-07-20-c2-fence-empty-submit-finding.md) — read that
first — and it supersedes the retired Direction A/B of
[`2026-07-20-c2-readback-release-ordering.md`](2026-07-20-c2-readback-release-ordering.md) and the handoff
[`2026-07-20-c2-handoff.md`](2026-07-20-c2-handoff.md).

## 1. The problem, stated exactly

An unmodified Vulkan app (`rayland-icosa-cpu`) renders across the real network and reads its frames back;
a residual fraction come back as the whole **previous** frame. The landed readback-completion gate took
this from *most runs losing frames* to ~10/11 clean, but a residual remains.

The 2026-07-20 measurement pinned why, and it inverted a plausible belief. S's return path uses an
**empty-submit completion fence** (`virgl_renderer_context_create_fence` →
`vkr_queue_sync_submit` → `vk->QueueSubmit(queue, 0, NULL, fence)`) as its "is the readback done?" barrier.
An empty submission's fence waits only for *its own* (zero) work, not for the application's prior real
submit to complete — so it retires **before** the readback DMA lands. Measured: on **~60 % of every
120-frame run**, the readback buffer changes **1.7–16 ms after** the fence retired, at a *constant*
submit. The fence is the wrong primitive, and virglrenderer's **public API exposes no queue-completion
barrier** to replace it with (all 60 exports are fences, `context_poll`, transfer, and resource
management; the only fence path is the empty-submit one). A `vkQueueWaitIdle`-class fix would require
patching virglrenderer — an engine fork, against the project's "borrow, don't fork" stance.

## 2. The key realization: the application already carries a real barrier

The application does **not** rely on S's proxy fence. It waits on **its own** `VkFence`, attached to its
**real** submit, with a blocking `vkWaitForFences`:

- `rayland-icosa-vk/src/scene.rs:540–598` — submit B records `draw` → `cmd_copy_image_to_buffer` (into
  the readback buffer, `res6`) → **one `vkQueueSubmit` carrying `self.fence`** → **`wait_for_fences(self.fence)`**
  → `read_pixels()`. The code's own comment: *"the fence wait above proves the copy has completed, so the
  buffer now holds the finished pixels."*
- `rayland-icosa-cpu/src/texture.rs:461–477` — submit A (the upload copy) has the same
  `reset → submit(fence) → wait_for_fences(fence)` shape.

On S, that `vkWaitForFences` is dispatched **synchronously and blocking** on the ring thread
(`virglrenderer 1.3.0 vkr_queue.c:461–471`, `vkr_dispatch_vkWaitForFences` → `vk->WaitForFences(...)`).
The ring thread advances the ring **`head`** only *after* each command's dispatch returns
(`vkr_ring.c:224–236`). Therefore:

> **The instant the ring `head` drains past a `vkWaitForFences` command, the fence it waited on has
> signalled — the corresponding submit, its readback copy included, is complete, and `res6` is
> host-visible and tear-free.**

This is a *true* completion barrier, and it is already in the stream. It is strictly stronger than S's
empty-submit fence, needs no engine change, and needs no timing heuristic.

### Why the gate misses it today
The gate triggers on `queue_ring_drained() && latest_submit_pos > last_delivered`. But between B's submit
delta and B's wait delta the ring is **transiently drained** (the "between-deltas drain" of
[`2026-07-19-c2-ringidx-decode.md`](2026-07-19-c2-ringidx-decode.md) §8) — `head` has passed B's submit
but the `wait` has not yet arrived to block the ring thread. The gate fires *there*, where `res6` is still
the previous frame, and the empty-submit fence does not save it. The reliable point — `head` past the
`wait` — comes a beat later, and the gate does not key on it.

## 3. The design

Replace the trigger and the barrier; keep and reuse the head-cap ordering machinery.

### 3.1 Decode `vkWaitForFences` inline in the ring
Mirror `find_queue_submit` (`rayland-vtest/src/venus_ring/decode.rs`), which byte-scans each `C2S::RingDelta`
for the app's `vkQueueSubmit` — proving the app's commands are carried **inline in the ring**, not only via
out-of-line streams. Add `find_wait_for_fences(stream)`:

- Match `VkCommandTypeEXT == VK_COMMAND_TYPE_vkWaitForFences_EXT` (**39**; cf. submit 18/206, queue2 155),
  async flags `0`, and an internal-consistency check on the argument layout (`device` handle, `fenceCount`,
  the `pFences` array marker, `waitAll`, `timeout`) pinned from `vn_protocol_renderer_fence.h`, exactly as
  the submit scan checks `submitCount`/array-marker agreement so stray bytes cannot satisfy it.
- Track `latest_wait_end_pos` (the free-running ring position of the **end** of the latest wait command),
  recorded from the linear delta bytes at the delta's frontier — **wrap-safe**, never a masked buffer
  offset — the identical discipline `latest_submit_pos` already uses (design-doc §2 of the ringidx work).
  The *end* position (not the start) is what `head` must reach, because `head` passes it only once the
  wait has **returned**.

### 3.2 Trigger the delivery on the wait-drain
In `progress_thread`, deliver when `head ≥ latest_wait_end_pos` for a wait newer than the last delivered
(replacing `queue_ring_drained() && latest_submit_pos > last_delivered`). At that moment the corresponding
submit is provably complete.

### 3.3 Disambiguate copy vs draw by content — now reliably
Read `res6` (via the existing `take_app_blob_writes` diff) at the wait-drain:

- **Changed** ⇒ this was the draw wait (B); `res6` holds the new, complete, tear-free frame. Ship it.
- **Unchanged** ⇒ this was the upload-copy wait (A), which never writes `res6`; nothing to ship.

This is the discriminator Direction A lacked. It is sound **only** because the wait-drain proves
completion: unlike the post-empty-fence state (ambiguous between a copy and a draw whose DMA had not
landed), a post-wait-drain `res6` is definitively final. There is no `T2 < T4` window here to make
"unchanged" mean "not yet".

### 3.4 Order pixels before the release, via the existing cap
The head-advance past the `wait` is what releases the application on C to read `res6` (with feedback off,
`vn_ring_wait_seqno` on the head). So the readback `BlobData` must reach C **before** that head-advance:

- **Cap** the reported head at `latest_wait_end_pos` while its delivery is pending.
- On the **changed** (draw) branch: ship `BlobData(res6)` first, then release the full head, then complete.
- On the **unchanged** (copy) branch: release the full head immediately (no ship) — the application then
  proceeds from its upload wait to record and submit the draw. Never holding a copy's head is what avoids
  the §3 deadlock of the release-ordering doc.

**Reuse Direction A's cap machinery.** Branch `c2-readback-release-ordering` already built and
code-reviewed (two reviewers, incl. opus) a **wrap-safe head cap** (Tasks 1–2); its regression was the
*trigger/release signal* (submit-pos + weak fence), not the cap. This design swaps that signal for
(wait-end-pos + reliable wait-drain content check) and keeps the cap. That is the crux of why G is
expected to succeed where A failed, and why the risky part (the wrapping cap) is already done.

### 3.5 Retire the empty-submit fence from the return path
`wait_for_work_retired` / the empty-submit fence is removed from `progress_thread`'s gate. **Keep the
method** — `read_back` and presentation still use it for resources *S itself* submits (where S controls the
submit and the fence is issued in-band, so it is a valid barrier there). Only the (c)2 return-path caller
goes away.

## 4. What does not change

- No `rayland-c`, wire-format, or forward-path change. `RingProgress { consumed_tail }` already carries a
  cappable frontier; this changes only *what value S reports when*, and adds one ring-scan on S.
- The `rayland-relay` protocol is untouched.
- Multi-queue / async submits remain out of scope — this rests on the single-queue, synchronous
  `submit → wait_for_fences` pattern (c)1 pins, exactly as the current gate does.

## 5. Testing (the proof)

- **End-to-end (the proof):** `scripts/c2-icosa-two-machine.sh` over the real link must go from the current
  residual to **0 stale across ≥ 20 runs** (several batches of ≤ 5 — the residual is intermittent, so the
  run count must make a survivor statistically visible). No new deadlock / `SIGABRT` /
  `QUEUE_REGISTER_DEADLINE`.
- **Unit (the wait scan):** `find_wait_for_fences` on captured/synthetic delta bytes — finds the latest
  wait, rejects stray bytes via the consistency check, returns the correct free-running end position across
  a ring wrap.
- **Unit (the cap decision):** the head-cap logic tested pure — given (head, `latest_wait_end_pos`,
  delivery resolved as draw / copy), assert `min(head, wait_end)` while pending and the full head once
  resolved, both branches.
- **Regression:** the loopback `icosa_cpu_renders` and `refapp_renders` e2e must stay green, and the cap
  must be inert when nothing is pending.

## 6. Risks to confirm during implementation (read the code — venus, virglrenderer)

1. **Is `vkWaitForFences` carried inline in the ring, or in an out-of-line execute stream?** `find_queue_submit`
   proves *submit* is inline; the *wait* must be confirmed. This is a **Mesa venus encoder** question —
   read Mesa's `vn_ring` / `vn_cs_encoder` (which chooses inline vs `vkExecuteCommandStreamsMESA`,
   command type 180) and confirm against virglrenderer's `vkr_ring_submit_cmd` dispatch. **Fallback if it
   is out-of-line:** key instead on the *return* of the wait as observed by the ring draining and
   *staying* drained past the execute boundary, or decode the out-of-line stream — to be settled by the
   read, before building. A quick throwaway probe (scan each delta for command type 39, log hits) settles
   it empirically in one run.
2. **Exact `vkWaitForFences` argument layout** for the consistency check — pin from
   `vn_protocol_renderer_fence.h` (type 39), the same source the submit scan used for its layout.
3. **Wrap-safety of `latest_wait_end_pos`** — reuse the free-running discipline of `latest_submit_pos`; the
   ring wraps ~frame 82, so this must never be a masked buffer offset.
4. **No regression of the fences-on loopback path** — the loopback e2e runs with feedback enabled; confirm
   removing the empty-submit fence from the (c)2 gate does not disturb it (it should not: that path is
   released by the feedback word, and `read_back`'s own fence use is untouched).
5. **cosmic-comp / presentation** — unaffected by this return-path change, but confirm the presented-frame
   path (`rayland-present`, `wl_shm`) still receives the same `BlobData` it does today.

## 7. Why this is the right fix (summary)

It is **deterministic** (the application's own real fence, not a proxy), needs **no engine fork** (the
signal is already in the ring), uses **no timing heuristic** (the class the diary records failing), and it
**dissolves** the copy-vs-draw ambiguity that regressed Direction A — because the wait-drain makes the
content check reliable. The one real unknown (inline vs out-of-line encoding of the wait) is a bounded
code-reading question answered before building.
