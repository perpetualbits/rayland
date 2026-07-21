# (c)2 — the return-path fix that worked: gate the readback on the app's `vkGetFenceStatus` completion

**Status:** design + result, 2026-07-21. This is the fix that took the `N == N−1` readback residual to
**0 stale over the real network**. It supersedes the *premise* of
[`2026-07-21-c2-waitdrain-completion.md`](2026-07-21-c2-waitdrain-completion.md) (whose wait-drain
mechanism was disproven — see §2) and builds on the evidence in
[`2026-07-20-c2-fence-empty-submit-finding.md`](2026-07-20-c2-fence-empty-submit-finding.md).

## 1. The problem

An unmodified Vulkan app (`rayland-icosa-cpu`) renders across the real network and reads its frames back;
a residual fraction came back as the whole **previous** frame. The landed readback-completion gate reached
~10/11 clean; the residual was a **C-side release-ordering race**: S shipped the head-advance that releases
the application before it shipped that frame's readback pixels, so C released the app, which read its own
local `res6` before S's `BlobData` for it had landed.

Fixing it needs two things at once, learned the hard way (§2): the readback pixels must ship **before** the
release, **and** only a *whole* (never mid-DMA) `res6` may ship. The missing piece both times was a
**real completion barrier** — a signal that says "this frame's readback copy is finished on S's GPU."

## 2. Three disproven approaches (do not retry)

- **The empty-submit context fence** (`virgl_renderer_context_create_fence`) retires *before* the readback
  DMA is host-visible (`T2 < T4`), pervasively — it is the wrong primitive
  ([`2026-07-20-c2-fence-empty-submit-finding.md`](2026-07-20-c2-fence-empty-submit-finding.md)).
- **The wait-drain** (key on the application's `vkWaitForFences` blocking on S) — **false premise.** With
  fence feedback off, Mesa's `vn_WaitForFences` does *not* send a blocking wait; it **polls
  `vkGetFenceStatus`** in a relax loop (`vn_queue.c`). The Task-1 spike caught this before it was built.
- **G-lite** (ship `res6` first, gated by a cheap fingerprint, no barrier) — fixed the `N−1` ordering
  (0 whole-previous) but introduced **~4 torn frames/run**: with no completion barrier the fingerprint
  fires mid-DMA and ships a partial buffer. Confirmed by classifying the failures (all "match no native").

## 3. The signal that works: a `vkGetFenceStatus` reply of `VK_SUCCESS`

With feedback off the application releases itself by polling `vkGetFenceStatus` over the ring until the
reply reads `VK_SUCCESS`. virglrenderer writes each reply into the **reply arena** (a Venus-internal blob)
as `[VkCommandTypeEXT][VkResult]`. A live `[38][0]` — type `vkGetFenceStatus`, result `VK_SUCCESS` — means
the polled fence has signalled: the application's submit **and its readback copy** are complete on S's GPU,
so `res6` holds a whole, finished frame. This is the real completion barrier the empty-submit fence never
was.

### Why the **live** arena, not the shipped diff
`take_venus_blob_writes` fragments the reply into **one run per changed byte** (the result byte is often
unchanged from the previous reply), so the contiguous `[38][0]` pattern is *not visible* in what S ships —
the first implementation scanned the diff, never matched, and shipped no `res6` at all (all 120 frames came
back identical). The **live** arena holds the whole reply. `Applier::reply_arena_fence_signaled` scans it.

### Why only the reply arena (the scan is narrowed to `s_written`)
Three shmems share Venus's `blob_id == 0` marker: the ring, the ~1 MiB reply arena, and the ~8 MiB
command-buffer **staging pool**. The staging pool holds the *application's own* command-buffer bytes
(forward-relayed from C), where a coincidental `[38][0]` word could stick the signal `true`. So the scan is
restricted to `Applier::s_written` — blobs S has actually written — which contains the reply arena (S's ring
thread writes replies into it) but **not** the staging pool (S never writes it). This is sound across pool
growth: when the reply pool fills Mesa mints a new `res_id`, and S writes replies into that one too, so it
joins `s_written` on its first reply — whereas decoding `vkSetReplyCommandStreamMESA` for the arena's
`res_id` would go stale at exactly that moment (that decode is documented-unsound; see `apply.rs`'s
`take_blob_writes` docs).

### Why an early or stale match cannot cause a wrong frame (the real safety property)
The signal is **not** treated as a precise per-frame barrier, and it does not need to be. A stale `[38][0]`
*can* linger — reply streams **chain** at advancing offsets rather than overwriting in place. Correctness
comes from the **ship order**: `res6` and the reply arena are shipped *before* the `RingProgress`
head-advance, and the application is released **only** by that head-advance (`vn_ring_wait_seqno` on `head`),
which S ships **last** and only once the application's own `vkGetFenceStatus` poll actually succeeded.
Because `take_bytes_s_wrote` is consuming and per-byte, any early/partial `res6` shipped on a mid-DMA poll
is completed by later polls, so the union on the wire is the whole frame *before* the releasing head-advance.
The signal therefore only controls *when* `res6` ships (ideally once, at completion), never whether C reads a
torn or stale frame. The `take_app_blob_writes`-non-empty gate keeps it to a draw with fresh pixels (an
upload copy or a re-poll ships nothing). This is why the fix is robust rather than merely empirically clean:
a future change that reordered the ship (progress before `res6`) is the one thing that would reintroduce the
defect.

## 4. The mechanism

In `progress_thread`, per poll, **only when the ring moved** (`take_ring_progress` non-empty — the same
progress-gated, venus-before-progress lockstep the working gate used, which **initialization depends on**;
a wholesale rewrite of this cadence broke device init):

1. Take the reply-arena delta (`take_venus_blob_writes`) **and** read `reply_arena_fence_signaled()` in one
   lock.
2. If a fence signalled: `take_app_blob_writes()`; if non-empty (a draw's fresh, complete `res6`), **ship
   it first**.
3. Then ship the reply arena (which carries the `VK_SUCCESS` that ends the poll loop), then the
   head-advance. C therefore always applies the finished frame before it is released onto it.

No empty-submit fence, no engine call from the progress thread, no timing heuristic, no content-stability
guess. The completion signal is the application's own real fence, observed through the reply it is polling
for.

## 5. Result

`scripts/c2-icosa-two-machine.sh` over the real link (apollo → dop561), feedback off: **0 stale frames
across 20 runs** (four batches of 5, 2026-07-21), where the committed gate was ~10/11 and every other
approach this session either tore (~4/run) or stuck (all frames identical). First fully-clean result.

## 6. Scope, cost, and follow-ups

- **Feedback-off only.** G' keys on `vkGetFenceStatus` polling, which exists only with fence feedback
  disabled — the only configuration that renders over a real network anyway (feedback-on SIGABRTs over a
  real link; it was always loopback-only). The loopback `icosa_cpu` e2e, which used feedback-on, is
  switched to feedback-off so it guards the actual shipping path.
- **Bandwidth: the readback still fragments** into ~5000 one-byte `BlobData` runs per frame (a flat or
  slowly-changing region diffs into one run per changed byte). This is the *same* fragmentation the
  committed gate had — a real performance issue, not a correctness one — and is left for a follow-up
  (run-coalescing in `emit_blob_writes`, or shipping the readback whole).
- **Single-queue, synchronous pattern only**, exactly as the prior gate — multi-queue remains (c)2's later
  work.
- **`take_app_blob_writes` emptiness is the copy-vs-draw discriminator**, now reliable because `VK_SUCCESS`
  proves the copy done (so a non-empty diff is a *complete* frame, not a torn one).
