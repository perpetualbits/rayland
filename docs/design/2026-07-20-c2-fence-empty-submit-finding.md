# (c)2 — the completion fence retires before the readback DMA, and *why*: the empty-submit primitive

**Status:** investigation finding, 2026-07-20. Evidence-backed, throwaway instrumentation. It **confirms**
the handoff's `T2 < T4` hypothesis, **refutes** a competing "the fence is already sound" reading, and —
the new part — **pins the mechanism** to the *empty-submit* form of virglrenderer's context fence, not to
the `ring_idx`. Read the handoff first: [`2026-07-20-c2-handoff.md`](2026-07-20-c2-handoff.md).

## 1. What was in question

The ~1/11 (and, under load, much worse) residual `N == N−1` stale frame is enabled by the readback
completion fence "not reliably guaranteeing the readback is host-visible when it retires" (`T2 < T4`). But
the evidence for that claim in the record was measured on **2026-07-17**, before the real-`ring_idx` fence
existed — when the fence fired on `ring_idx = 0`, which retires immediately and waits on *no* GPU work.
That measurement characterised the *old broken* fence. Reading today's code
(`crates/rayland-engine/src/virgl.rs`, virglrenderer 1.3.0 `vkr_ring.c`/`vkr_queue.c`) suggested the
*current* fence should FIFO-follow the app's submit and cover its readback copy — i.e. `T2 < T4` might no
longer bite, and the residual might be pure C-side release ordering. Two hypotheses, resolved only by
measurement:

- **H1 (recorded):** today's real-`ring_idx` fence still retires before the readback DMA.
- **H2 (a fresh code reading):** the fence is sound; the residual lives entirely in C-side release ordering.

## 2. The measurement

Env-gated, in-memory, dump-at-session-end instrumentation in `rayland-s`'s `progress_thread`
(`RAYLAND_C2_FENCEPROBE=<file>`; the probe and its `Applier::readback_probe`/`sampled_fp` helpers are
throwaway and marked for deletion). On each **fence-poll** it appends one line:

```
seq, t_ms_since_pending, latest_submit, drained, readback_advanced, res6_id, res6_size, res6_fp
```

`res6_fp` is a cheap content fingerprint of the readback buffer (the largest non-ring, non-Venus-internal
blob — the same one `take_app_blob_writes` ships first). It changes exactly when the GPU's readback DMA
lands a new frame. The **discriminating signal** is a `res6_fp` change between two adjacent fence-polls:

- at a **constant** `latest_submit` (no new `vkQueueSubmit` crossed the ring) ⇒ the DMA for a submit S had
  *already fenced* landed *after* the fence retired ⇒ **H1**;
- only when `latest_submit` **advances** ⇒ `res6` moves only as new submits cross ⇒ **H2**.

### The Heisenbug, met head-on
The first probe used the full `trace::fingerprint` (one byte per 64, ~16k iterations over 1 MiB) **under
the applier lock, ~20×/frame**. It detected all 120 frames but *starved the message thread*: run 5
collapsed to 109/120 stale and 11 delivered frames — the probe inflated the very defect it measured.
Dropping to ~64 samples removed the perturbation but went **blind** (1 distinct fingerprint per run — 64
sparse samples miss a small spinning object on a mostly-constant background). The usable point is
**~4096 word-samples**: it detects all ~120 frames *and* leaves the residual in its normal range (7 stale
across 5 runs, no collapse). All three regimes agree on the finding below; only the ~4096 run is quoted as
the clean one.

## 3. The result — H1, decisively, and pervasive

Clean run (4096-sample probe, real network apollo→dop561), per 120-frame run:

| run | stale frames | distinct res6 frames | **H1 events** (fp change at constant submit) | H2 events (fp change on advanced submit) |
|----:|-------------:|---------------------:|---------------------------------------------:|-----------------------------------------:|
| 1 | 0 | 121 | **72** | 48 |
| 2 | 0 | 120 | **78** | 41 |
| 3 | 7 | 121 | **70** | 50 |
| 4 | 0 | 121 | **64** | 56 |
| 5 | 0 | 120 | **73** | 46 |

**~60% of every run's frames** exhibit the H1 signature: `res6` changes **1.7–16 ms after** the fence has
retired, with **no new submit crossing the ring**. It happens in the runs with **zero** stale frames just
as much as in the stale one — so it is not itself the stale frame; it is the *window*. The gate's re-poll
loop (fence again a few ms later, `res6` has landed, ship fresh) absorbs almost all of it. A stale frame is
the rare case that escapes the window on the **C side** before the gate ships the fresh readback.

H2 is refuted. `T2 < T4` is real with today's real-`ring_idx` fence, it is the common case, and the earlier
"the fence is sound now" reading was wrong.

## 4. Why — the mechanism (the new part)

The fence is `virgl_renderer_context_create_fence(ctx, flags=0, ring_idx, fence_id)` →
`vkr_context_submit_fence` → `vkr_queue_sync_submit`, whose body is (virglrenderer 1.3.0
`vkr_queue.c:93`):

```c
mtx_lock(&queue->vk_mutex);
vk->QueueSubmit(queue->base.handle.queue, 0, NULL, sync->fence);  /* an EMPTY submit — 0 batches */
mtx_unlock(&queue->vk_mutex);
```

The naive FIFO argument says: the ring thread calls the app's real `vk->QueueSubmit(B)` **synchronously,
inline** (`vkr_dispatch_vkQueueSubmit`, `vkr_queue.c:378–381`) and only *then* advances `head`
(`vkr_ring.c:225–236`); S fences only once it sees `head == applied_tail` (drained), so the app's submit B
is already enqueued and the empty submit — same queue, same `vk_mutex` — is enqueued strictly after it.
True, but it proves the wrong thing: **an empty submission's fence waits only for that submission's own
(zero) work, not for prior submissions to complete.** Enqueue order is not completion order. So the empty
fence can signal as soon as the queue *reaches* the workless submit — before B's readback-copy DMA drains.

**Why this does not break venus for everyone else.** The application's *own* `VkFence` is passed to the
*real* submit (`vkr_dispatch_vkQueueSubmit` forwards `args->fence`), so it correctly waits for B — that is
how `vkWaitForFences` works natively. The empty-submit `create_fence` is a *separate*, ring-timeline
mechanism that ordinary venus does **not** use for application-visible completion (and with fence feedback
disabled — our only real-network config — the app does not use it at all). **We repurpose it** as a
"has the readback landed?" barrier, and for that purpose it is the wrong primitive. Normal venus is fine
because it never leans on `create_fence` this way; we see `T2 < T4` because we do.

Ruled out, so P2 stands by elimination as well as by code:
- **Different queue (no FIFO):** single-queue config (`no_multi_ring`); the fence's `sync_queues[ring_idx]`
  is the queue the app registered via `vkGetDeviceQueue2` and submits on. Same `VkQueue`.
- **A coherency delay after completion:** `DMA_BUF_IOCTL_SYNC` on the readback dma-buf was a measured no-op
  (6561/6561 byte-identical) — once written, the bytes are immediately CPU-visible; and 16 ms is far too
  long for a cache flush. The lag is *completion*, not *visibility*.

## 5. What this means for the fix (to brainstorm next, not decided here)

The `ring_idx` decode was necessary but not sufficient: it aimed the fence at the right queue; it did not
make the empty-submit form a real completion barrier. A correct fix needs the barrier to actually wait for
the app's already-dispatched submit B to *complete*, which the current public virglrenderer fence API does
not express (its only knobs are `flags` and `ring_idx`; there is no public per-queue "wait idle"). Three
directions worth weighing:

- **A real engine-level completion barrier** — reach the app's `VkQueue` and `vkQueueWaitIdle` it (or chain
  an empty submit behind B via a real dependency). This is the *correct* barrier but likely needs a
  virglrenderer-level addition, in tension with "no fork."
- **Tolerate the weak fence; fix only the C-side release** — the gate *already* re-polls until `res6`
  actually changes, so S always ships fresh pixels; the residual is purely the C-side head-advance
  releasing the app before that ship. Hold the head for a readback-bearing submit and disambiguate copy vs
  draw by the gate's **resolution outcome** (`res6` changed within a bound = draw → ship+release; bound
  expired unchanged = copy → release) rather than by the instantaneous post-fence-empty state that
  Direction A keyed on and that `T2 < T4` makes ambiguous.
- **A race-free content-stability signal** — previously tried and abandoned as a nest of races
  (`2026-07-19-c2-ringidx-decode.md` §8); start from *why* it failed, not from scratch.

## 6. Reproduce

```
cargo build --release -p rayland-s          # the probe builds behind the RAYLAND_C2_FENCEPROBE gate
RAYLAND_C2_FENCEPROBE=/tmp/fp.csv scripts/c2-icosa-two-machine.sh 5
python3 <analyze_probe.py> /tmp/fp.csv       # counts H1 vs H2 per run; runs split on '# ' header lines
```
(The analysis script lives in the session scratchpad; it is throwaway like the probe.) Judge over many
runs. Keep the probe at ~4096 samples: denser starves the message thread (Heisenbug), sparser goes blind.
