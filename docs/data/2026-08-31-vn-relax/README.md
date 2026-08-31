# The `vn_relax` test — hypothesis refuted, and the real target located

## The hypothesis

Four interventions on 2026-08-31 each collapsed a mechanism by a large factor and moved the frame rate
by nothing: S's largest lock-holder (8.3× less lock-held), C's forward message count (6.1× fewer), S's
`PROGRESS_POLL` (3× more polling, p = 0.94) and C's `PARK_SLEEP` (shorter is *worse*, longer nothing).

That signature suggested the frame time was **not ours to spend**: with fence feedback off, Mesa
implements `vkWaitForFences` by polling `vkGetFenceStatus`, and the gap between polls is chosen by
Mesa's own `vn_relax` back-off. If the application sleeps between polls, every microsecond Rayland
saves is absorbed by the app waiting longer before it next asks.

## The instrument

`crates/rayland-c/src/relaxstat.rs`, gated on `RAYLAND_C1_RELAXSTAT`. Records an ordered sequence of
three events, each costing one `CLOCK_MONOTONIC` read and one store into a preallocated array:

| event | meaning |
|---|---|
| `RingShipped` | C shipped a ring delta — the application's request is on its way |
| `ReplyApplied` | C applied bytes S wrote — the answer is now visible to the application |
| `FrameCallback` | C delivered `wl_callback.done` — the compositor released the app to draw |

A **sequence**, not a histogram, because `vn_relax`'s back-off grows *within* one wait: the signature
is an ordered run of increasing intervals, and a distribution cannot tell a constant 1 ms sleep from a
doubling 62 → 125 → 250 → 500 µs.

`FrameCallback` was added after the first capture: C's successive ring deltas were 25.0 ms apart and
the separately-measured native ceiling for this app and compositor is 25.4 ms/frame *of which 24.9 ms
is compositor pacing*. Without that event, time the app spends legitimately blocked on the compositor
is indistinguishable from time Rayland cost it, and would have been silently charged to us.

**Two instrument bugs were found and fixed before the data below was taken**, both of which would have
produced a confidently wrong answer:

1. The first version reported every 10 s and dumped the *whole* log each time. Two of three captures
   were **lost entirely** because the run ended before the first tick. Now it emits incremental chunks
   every 3 s (also removing an O(n²) re-dump).
2. The first analysis collapsed event bursts and computed a poll cycle from them — but one ring delta
   carries *many* application commands, so it was measuring batches, not polls. The attribution below
   charges every interval to whoever *ended* it, which needs no assumption about batching.

## Result — 5 clean runs, 312 frames, 26.8 s of wall clock attributed

Every interval between consecutive events is charged exactly once, to whoever ended it.

| owner | share | n | median | p90 | p99 |
|---|---|---|---|---|---|
| **RAYLAND** (ends: we delivered a reply) | **76.7%** | 9,800 | 61.5 µs | 8.55 ms | 26.7 ms |
| **COMPOSITOR** (ends: frame callback) | 13.7% | 317 | 12.7 ms | 14.6 ms | 20.7 ms |
| **APP** (ends: app wrote the ring) | **9.6%** | 1,355 | 1.03 ms | 3.48 ms | 18.9 ms |

**The hypothesis is refuted.** The application's own time — which is where *any* `vn_relax` sleep must
live — is **9.6% of the wall clock**. Eliminating all of it, sleeping and thinking alike, would buy at
most that. Frame time is ours: 76.7% of it.

The same data by frame phase, bounded by successive frame callbacks:

| phase | median | share |
|---|---|---|
| frame callback → app writes ring | 6.29 ms | 12.8% |
| ring out → last reply in | 38.19 ms | **66.8%** |
| last reply → next frame callback | 13.79 ms | 20.4% |
| **total frame** | **56.97 ms** | — |

(The 57 ms total matches the harness's independently-computed median inter-frame gap for these runs,
50–67 ms, which is the check that the decomposition is measuring the right thing.)

## Where our 76.7% actually sits

| intervals larger than | count | share of count | share of our time |
|---|---|---|---|
| 0.1 ms | 4,095 | 41.8% | 99.2% |
| 1 ms | 1,451 | 14.8% | 94.2% |
| 5 ms | 1,140 | 11.6% | **91.0%** |
| 10 ms | 876 | 8.9% | 80.9% |
| 20 ms | 233 | 2.4% | 33.7% |

Highly concentrated — and of the time in intervals over 5 ms, **90.5% is in intervals that begin with
a ring delta going out**. That is the C→S→execute→reply→C round trip, and there are about **3.1 of
them per frame at roughly 16 ms each**, which is essentially the whole 57 ms frame.

## A second hypothesis, tested in the same data and also refuted

If the delay were virglrenderer's *host-side* ring thread sitting in a grown back-off
(`vkr_ring_relax`: `thrd_yield()` ×16, then an exponentially growing sleep from 10 µs), then a longer
idle period before a delta should predict a longer wait for its reply.

| preceding idle | n | median wait | p90 |
|---|---|---|---|
| 0 – 0.2 ms | 4 | 11.49 ms | 12.15 |
| 0.2 – 1 ms | 622 | 15.86 ms | 25.72 |
| 1 – 5 ms | 652 | 16.83 ms | 30.94 |
| 5 – 20 ms | 61 | 16.37 ms | 36.78 |
| > 20 ms | 12 | 25.18 ms | 40.87 |

Spearman **ρ = +0.117** (z = +4.3, n = 1351): a real correlation, and far too small to be the
explanation. Even with essentially *no* preceding idle — where the ring thread should still be in its
yield phase — the wait is 11.5 ms. **There is a fixed ~11–16 ms floor per ring round trip that idle
history does not explain.**

## What this leaves

The target is now specific, countable and localised for the first time: **~3.1 ring-delta round trips
per frame, each costing a fixed ~16 ms on loopback**, where the network costs microseconds. Every
previously suspected term is excluded by measurement — network, S's lock contention, C's send path,
both poll intervals, Mesa's client-side back-off, and virglrenderer's host-side back-off.

What has *not* been looked at, and is where the ~16 ms must be: what S does between reading a ring
delta and the first reply reaching C. That crosses `virgl_render_server` (a separate process), the
real `vkQueueSubmit`, GPU execution, and the swapchain — and `vkAcquireNextImageKHR` blocking on the
compositor to release a buffer is a live candidate that this instrument cannot see from C.

## Reproducing

```
C_HOST= MODE=traffic RUNS=1 FRAMES=60 NO_BUILD=1 RELAXSTAT=1 OUT=/tmp/rx scripts/wp0-soak.sh
python3 analyse-ownership.py   /tmp/rx/run1/c.log      # who owns the wall clock
python3 analyse-frame-phases.py /tmp/rx/run1/c.log     # the frame, phase by phase
python3 analyse-concentration.py /tmp/rx/run1/c.log    # where our share is concentrated
python3 analyse-backoff-correlation.py /tmp/rx/run1/c.log
```

The `events-run*.txt.gz` files are the raw captures the tables above were computed from.
