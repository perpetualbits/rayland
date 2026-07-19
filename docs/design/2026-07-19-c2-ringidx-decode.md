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

**The ring wraps — so positions are tracked free-running, not by buffer offset.** An earlier draft
assumed the whole session fits in the 128 KiB buffer without wrapping (an old (c)1 finding measured a
7.58% peak on a *shorter* capture). That is **false** for the full 120-frame icosa run: the buffer
wraps around frame ~82, after which a byte's *buffer offset* (`pos & mask`) no longer equals its
*free-running* ring position. Two consequences shape the rest of this design:

- **`vkGetDeviceQueue2` is safe to place by buffer offset** only because it is emitted during device
  init, at a tiny `tail` far before the first wrap — so there its offset does equal its free-running
  position (`end_offset`), and the head-gate below compares it against a free-running `head`.
- **Everything that must stay correct *after* the wrap uses free-running counters, never a buffer
  offset.** `head`, `applied_tail`, and the head-gate/drained comparisons are all free-running (they
  grow monotonically past the buffer size and never approach the 2³² counter wrap in a session, so a
  plain `>=`/`==` is exact). And the readback-fence trigger (§8) records each `vkQueueSubmit`'s
  **free-running** position from the *delta stream* rather than scanning the circular buffer — precisely
  because a buffer scan's offsets wrap and its "newer than last delivered" compare would break (it did:
  a buffer-scan version wedged reproducibly at the wrap point).

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
   scan the ring's linear buffer `[0, applied_tail)` for the command; on first match store a
   `QueueRegistration { ring_res_id, ring_idx, end_offset }` in `Applier::queue: Option<…>` (the
   `end_offset` is the free-running position of the command's end). Cost is bounded — a few KiB, and
   the scan stops permanently after the first match. Single-queue scope (matches (c)1's pinned
   `no_multi_ring`): the first `vkGetDeviceQueue2` is the app's one queue; a second is out of scope and
   left for a later task.

3. **Gate** (`Applier`): `retirement_ring_idx(&self) -> Option<u32>` returns `Some(ring_idx)` only
   when latched **and** `RingMirror::head(blob) >= end_offset`. Needs a `RingMirror::head(&self, blob)
   -> u32` peek (a plain acquire load, unlike `take_progress`'s move-only report). A plain `>=` is
   correct because both are **free-running** counters (they keep growing past the buffer size when the
   ring wraps, and never approach the 2³² counter boundary in a session), and `end_offset` is a fixed
   early position `head` only ever grows past — so the gate opens once and stays open.

4. **Fence** (`progress_thread`, after the Task 4 wiring is applied): replace the hardcoded
   `engine.wait_for_work_retired(ctx, 0)` with the decoded value — fetch `retirement_ring_idx` under
   the short applier lock (released before the wait, preserving the no-deadlock invariant); if
   `Some(idx)`, fence with `idx`; if `None` (not yet registered, or already destroyed), do **not**
   fence. **The fence is triggered when a new `vkQueueSubmit` has been dispatched** — `Applier::
   latest_queue_submit_start` reports a submit position newer than the one last delivered, and
   `queue_ring_drained` proves the host ring thread has dispatched it (see §8) — never on the first ring
   quiet, which lets the fence overtake the app's submit and ship torn pixels. To keep a stuck delivery
   (a queue never decoded, or a wedged ring thread) from hanging the app silently, it is bounded by
   `QUEUE_REGISTER_DEADLINE` (5 s), after which the session ends with a diagnostic rather than spinning.

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

## 7. The teardown race, and closing the gate on `vkDestroyDevice`

The head-gate (§3) prevents a fence *before* the queue is registered. It does **not**, by itself,
prevent a fence *after* the queue is **destroyed** — and that turned out to be a real, non-benign
race, not a cosmetic one:

- When the application finishes its last frame it destroys its device, which frees the host queue
  (`sync_queues[ring_idx]` → NULL). Teardown ring traffic sets `delivery_pending` one last time and,
  because a one-way `head`-gate stays open forever, `progress_thread` fires a final fence on the freed
  queue. That fence is **render-server-fatal** exactly as the `finish-report` documented: it kills the
  context worker (`invalid ring_idx 1` → `failed to dispatch context op`), and the application's
  remaining teardown commands then `EPERM`. Whether the app survives is a race with its own teardown:
  measured at **1 SIGABRT in 3 icosa runs** — enough to fail the "must exit cleanly" gate
  non-deterministically. It is not benign.

The fix is to **close the gate the moment the application destroys the device**, decoded from the ring
by the same signature-scan machinery: [`find_destroy_device`] matches `vkDestroyDevice` (type 12,
async flags 0, and the **same `VkDevice` handle** the `vkGetDeviceQueue2` carried), and the
`C2S::RingDelta` arm clears `Applier::queue` on it. The reason this is race-free — not merely
less-likely — rests on two facts:

1. **The application is synchronous**, so `vkDestroyDevice` is emitted strictly *after* the last
   frame's readback has been delivered and the app released — and the fence trigger (§8) only ever
   fires on a *new* `vkQueueSubmit`, which the destroy delta does not carry. So the destroy never
   arms a fence, whatever the host does with the queue.
2. **`Applier::queue` is cleared in the message thread, under the applier lock, as the destroy delta
   is applied.** The progress thread must take that same lock to read `retirement_ring_idx`, so it
   observes the clear atomically: it never reads `queue = Some` in the same critical section in which
   the host could already have freed the queue. (Note the weaker fact that this is *not* strictly
   "before the doorbell": `apply_delta` publishes `tail` with `Release` before the clear runs, so a
   ring thread already busy-polling could dispatch the destroy before the explicit `vkNotifyRingMESA`.
   The safety rests on the under-lock clear and fact #1, not on doorbell ordering.)

A false *positive* on the destroy signature would close the gate early and wedge the next readback.
The type + async-flags + exact-device-handle triple is not high-entropy on its own (Venus object ids
are small integers), so the real protection is that the scan reads **only each delta's new bytes,
once** (never the whole wrapped buffer repeatedly) and only while a queue is latched — a tiny aliasing
surface — and that a stuck delivery ends the session loudly via `QUEUE_REGISTER_DEADLINE` rather than
hanging. A false *negative* (missing a real destroy) is the dangerous direction — it re-admits the
fatal fence — so the signature is kept minimal, and the destroy sits in one delta (deltas end at
command boundaries, so it is never split).

## 8. The tearing race, and gating the fence on the GPU write

With the fatal fence removed (§7) and the correct `ring_idx` in hand, the icosa fixture stopped
`SIGABRT`-ing — but a *different* defect surfaced: **1–4 of 120 frames, non-deterministically, came
back torn** ("matches no native frame: corruption"), a different set each run. This is a real race,
and it is the reason the fence must be triggered carefully rather than fired on ring quiet.

**The mechanism — the fence can overtake the application's own submit.** S issues the readback fence
via `virgl_renderer_context_create_fence`, which reaches `vkr_context_submit_fence` over the
render-server's context-op socket — a path *independent of, and able to overtake,* the ring thread
(the same property the `finish-report` noted). If S fires the fence while the ring is only
*transiently* quiet — e.g. during the icosa fixture's per-frame CPU fractal computation, or in any gap
before the application's `vkQueueSubmit` is dispatched — S's empty `vkQueueSubmit(queue, 0, NULL,
fence)` can land on the shared queue **ahead** of the application's real submit. By FIFO it then
retires against the *previous* frame's work, S reads a half-written readback, and ships torn pixels.
Ring retirement alone cannot tell S the readback submit has been dispatched; the ring genuinely goes
quiet mid-frame.

**A content-based trigger was tried first, and abandoned.** The natural idea — S's GPU writes the
readback *only while executing the app's submit, so fence when the readback bytes change* — works but
is a nest of races: a strided hash collides between two spinning frames and wedges; a full hash fixes
that but must be baselined against the *last delivered* frame and re-baselined *before* the ship (which
releases the app to run the next frame); and even then it wedges on any two byte-identical frames.
Each was fixed in turn and it still hung ~2 runs in 5. The lesson: **the readback content is the wrong
thing to watch.** (The commit history and `.superpowers/sdd/ringidx-decode-findings.md` record the
dead ends.)

**The fix — fence when a new `vkQueueSubmit` has been dispatched.** What S actually needs to know is
structural, not content: that the app's own `vkQueueSubmit` for *this* frame has been **dispatched on
the host**, so a fence issued now lands strictly after it. Two decoded facts pin that exactly:

- **The submit is present.** `find_queue_submit` scans the ring for the latest `vkQueueSubmit` /
  `vkQueueSubmit2` on the app's queue (matched by type ∈ {18, 206}, async flags 0, the queue handle
  from the latched `vkGetDeviceQueue2`, and a `submitCount`/array-marker self-consistency check that
  stray bytes will not satisfy). `Applier::latest_queue_submit_start` returns its ring position; the
  fence fires only when that position is **newer than the one last delivered** — i.e. this frame's
  submit, not a stale earlier one, and not a between-deltas transient drain before the submit delta
  arrived.
- **The submit is dispatched.** `head == applied_tail` (`Applier::queue_ring_drained`) means the host
  ring thread has consumed everything S relayed, including that submit — so the fence cannot overtake
  it. The readback **DMA** may still be in flight (dispatched ≠ GPU-complete); the fence S then issues
  on the real `ring_idx` is what waits for that completion.

This trigger is **content-independent** (immune to identical frames and to sampling a buffer a
cross-process GPU is writing) **and needs no timing heuristic**. An intermediate version gated the
fence on the ring staying drained for a 2 ms settle — a proxy for "the submit has been relayed" — but a
settle is only probabilistic: it still tore ~1 frame in 5 when a between-deltas gap ran long. Decoding
the submit removes the guess: S waits for the actual submit, however the deltas fall. (The commit
history and `.superpowers/sdd/ringidx-decode-findings.md` record the content and settle dead ends.)

The one remaining assumption is the single-queue Venus configuration (c)1 pins: one queue, one submit
stream. Multiple queues — matching each readback to the queue that produced it — is (c)2's next step.

**Result (2026-07-19):** with the head-gate (§3), the destroy-close (§7), and the submit-dispatch
trigger (§8) together, `icosa_cpu_renders` passed **5 of 5 completing runs at 0/120 frames differing**
(the gate asks for 3 consecutive), no `invalid ring_idx`, clean shutdown; `refapp_renders` stays green.
The four defects this task closed — fence before registration (fatal), fence after destruction (fatal
at teardown), fence overtaking the submit (torn), and a buffer-offset trigger breaking when the ring
wraps mid-run (wedged at ~frame 82) — are each closed by one of the mechanisms above: the head-gate,
the destroy-scan, the submit-dispatch trigger, and free-running position tracking respectively.
