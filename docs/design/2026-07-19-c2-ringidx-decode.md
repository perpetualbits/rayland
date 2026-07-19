# (c)2 completion barrier — decoding the app's real per-queue `ring_idx`

**Status:** design spec, 2026-07-19. Implements the remaining half of the engine-actor walking
skeleton ([`2026-07-18-c2-engine-actor.md`](2026-07-18-c2-engine-actor.md)): the actor already
removed the fence-vs-doorbell deadlock; what is left is issuing the **T4 GPU-completion fence** on
the application's **real per-queue `ring_idx`**, not the hardcoded `ring_idx = 0` that ties to no GPU
work. Read the actor spec's §8–§9 and its §9 correction first — they are why this task exists.

The full source trace behind every number here is
[`.superpowers/sdd/ringidx-decode-findings.md`](../../.superpowers/sdd/ringidx-decode-findings.md).

## 1. The one-sentence problem

`virgl_renderer_context_create_fence(ctx, 0, ring_idx, id)` is a real GPU-completion fence **iff**
`ring_idx` is the app's actual per-queue timeline index (≥1) **and** that queue is already registered
on the host; `ring_idx = 0` retires instantly (no GPU tie — the stale/torn-readback bug), and a wrong
or premature `ring_idx ≥ 1` is **render-server-FATAL** (`sync_queues[ring_idx] == NULL` → the context
worker dies → the app `SIGABRT`s). So S must (a) learn the real `ring_idx`, and (b) never fence until
that queue is registered. Neither may be guessed — a miss kills the session.

## 2. Where `ring_idx` lives, and why we can read it safely

Mesa's guest driver assigns every `VkQueue` a `ring_idx ≥ 1` and hands it to the host at
`vkGetDeviceQueue2` time, inside `VkDeviceQueueTimelineInfoMESA.ringIdx` on the `pNext` chain of
`VkDeviceQueueInfo2`. That command crosses S in the **ring** (not the inline vtest socket) — it is one
of the app's ordinary Venus commands, which S already relays byte-for-byte into its ring mirror.

The command is **fixed at 80 bytes** with `ringIdx` at **offset 48** (full table and citations in the
findings doc). A candidate at ring-buffer offset `X` is unambiguously `vkGetDeviceQueue2` when all four
hold:

```
u32@X      == 155          // VkCommandTypeEXT vkGetDeviceQueue2_EXT
u32@X+4    == 0            // VkCommandFlagsEXT — async, so 0
u32@X+24   == 1000145003   // VkDeviceQueueInfo2.sType
u32@X+36   == 1000384005   // VkDeviceQueueTimelineInfoMESA.sType
```

Two of these are 32-bit structure-type constants, so a false positive is astronomically unlikely.
This is why a **signature scan** is honest here even though the repository's existing linear decoder
([`venus_ring::decode`](../../crates/rayland-vtest/src/venus_ring/decode.rs)) cannot *walk* to this
command: variable-size commands (`vkCreateInstance`, `vkCreateDevice`, …) precede it and correctly
stop the walk. Scanning past them for a fully-cross-checked fixed signature is not the "guess a size
and desynchronize" failure that decoder is built to refuse — it reads a self-verifying constant.

**No wrap.** The whole session never wraps the ring (findings: peak tail 7.58% of the 128 KiB
buffer), and `vkGetDeviceQueue2` is emitted during device init, so its byte offset in S's linear ring
buffer equals its free-running ring position. `head`, `tail`, and the command-end offset are all
directly comparable with `wrapping_sub`.

## 3. The registration gate — the head signal

`sync_queues[ring_idx]` is populated by `vkr_queue_assign_ring_idx`, called by virglrenderer's ring
thread **while it dispatches** `vkGetDeviceQueue2` (`vkr_queue.c:263-287`, dispatched `:321-356`). That
thread stores the ring's `head` **after** each command's dispatch, with release ordering
(`vkr_ring.c:232-233`, `store_head` release at `:61-66`). Therefore:

> Once S observes `head ≥ (vkGetDeviceQueue2's end offset)`, that command has been dispatched and the
> queue is registered — and S reads `head` with acquire ordering, which pairs with that release store,
> so the registration is visible before S acts.

This is the gate the fence waits on. It needs no new host round-trip: S already reads `head` in
`RingMirror::take_progress`.

## 4. Design

Four small pieces; nothing about the actor, the delivery ordering, or the trait boundary changes.

1. **Decoder** (`venus_ring::decode`): a pure function
   `find_get_device_queue2(stream: &[u8]) -> Option<GetDeviceQueue2>` returning
   `{ ring_idx: u32, end_offset: usize }` — the validated signature scan of §2. Unit-tested against
   **real captured bytes** (see §5), never synthetic ones (the `captured.rs` rule: a decoder must not
   be tested against bytes its own encoder produced).

2. **Latch** (`Applier`, in the `C2S::RingDelta` arm, after `apply_delta`): while not yet latched,
   scan the ring's linear buffer `[0, applied_tail)` for the command; on first match store
   `readback_ring_idx: Option<u32>` and `queue_registered_at: Option<u32>` (the free-running
   `end_offset`). Cost is bounded — a few KiB, and the scan stops permanently after the first match.
   Single-queue scope (matches (c)1's pinned `no_multi_ring`): the first `vkGetDeviceQueue2` is the
   app's one queue; a second is out of scope and left for a later task.

3. **Gate** (`Applier`): `retirement_ring_idx(&self, blob) -> Option<u32>` returns `Some(ring_idx)`
   only when latched **and** `RingMirror::head(blob) ≥ queue_registered_at` (wrapping). Needs a
   `RingMirror::head(&self, blob) -> u32` peek (a plain acquire load, unlike `take_progress`'s
   move-only report).

4. **Fence** (`progress_thread`, after the Task 4 wiring is applied): replace the hardcoded
   `engine.wait_for_work_retired(ctx, 0)` with the decoded value — fetch `retirement_ring_idx` under
   the short applier lock; if `Some(idx)`, fence with `idx`; if `None` (not yet registered), do **not**
   fence this poll — leave `delivery_pending` set and retry next poll. In practice the queue is
   registered during device init, long before frame 0's readback, so the gate is already open at the
   first delivery; the `None` branch is a safety net, not a normal path.

## 5. Test strategy

- **Decoder unit tests** use real `vkGetDeviceQueue2` bytes captured from a live icosa run (dumped by
  a throwaway spike — see [`ringidx-decode-findings.md`](../../.superpowers/sdd/ringidx-decode-findings.md)),
  transcribed into a fixture the way `captured.rs` preserves its ring prefix. Tests: the signature
  matches at the right offset; `ring_idx` reads `1` (the real value, confirmed by the spike — the
  finish-report's `ring_idx=1` was right, it just fenced before registration); a stream without the command
  yields `None`; a truncated command (< 80 bytes) yields `None`; a near-miss (one magic word wrong)
  yields `None`.
- **No-GPU linkage** is unaffected: the decoder is pure bytes in `rayland-vtest`, which by
  construction has no GPU dependency; `rayland-c`'s guard still holds.
- **The gate** (the whole point): `icosa_cpu_renders` passes 120/120 on **three consecutive
  completing runs** (no SIGABRT, no `vkWaitForFences` timeout, no hang); `refapp_renders` stays green;
  `clippy --workspace --all-targets` clean; non-GPU unit/lib tests pass.

## 6. What this is not

- Not a general Venus decoder. It recognizes exactly one command by a self-verifying signature; every
  other command still stops the linear walk, as before.
- Not multi-queue. One queue, one `ring_idx`, one ring — (c)1's pinned configuration. Multiple queues
  (matching each readback submit to its queue's `ring_idx`) is deferred.
- Not a change to the actor, the venus/app blob-write split, or the delivery ordering — those are
  already correct (actor spec §8, Task 4). This task only supplies the correct fence argument and the
  gate that makes issuing it safe.
