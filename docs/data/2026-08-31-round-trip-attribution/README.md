# Round-trip attribution, 2026-08-31

Evidence for the diary entry *"Instrumenting the round trip: the two milliseconds were one function,
and it was ours"* and for the `project-map.js` parts it updates.

## What the question was

A swapchain rebuild does roughly 477 synchronous round trips and takes about 970 ms, so the stall the
owner sees on a focus change — and a large share of ordinary frame time — is a per-round-trip cost of
about two milliseconds. Nothing had ever put a clock on it.

## `linktrace-c.log.gz`, `linktrace-s.log.gz`

One loopback `vkcube` run (60 frames) with `LINK_LOG=1`, which arms `RAYLAND_C1_LINK_LOG` on C and
`RAYLAND_S_REPLY_LOG` on S. Both daemons stamp their link events with `t_ns=` on the **same**
`CLOCK_MONOTONIC` — they are on one machine — so the two files join directly and a round trip
decomposes into four points.

Markers: `s>` before a write, `s<` after the flush that makes it leave, `r<` on a read.

Measured medians from these two files:

| segment | median |
|---|---|
| C flushes a doorbell → S reads it | 3.9 ms |
| S reads it → S writes the reply blob | 3.2 ms |
| S writes the reply → C reads it | 0.67 ms |

Two further figures taken from the same pair, both of which matter:

- C sends **5,495 messages** in the run against **288 doorbells**, and **5,409 of them carry 1–3
  bytes each**.
- The measured count of C→S messages in flight at the moment a doorbell is flushed is a **median of
  2** — which is what rules out head-of-line blocking as the cause of the 3.9 ms, and points instead
  at S's message loop being blocked on the applier mutex.

**These files are perturbed and must not be quoted for frame rate.** The trace's two `eprintln`s per
message took the run from ~20 fps to ~10. The segment *ratios* and the message census survive that;
the absolute latencies are inflated.

## `ab-before.txt`, `ab-after.txt`

The interleaved A/B for the `memchr` rewrite of `Applier::reply_arena_fence_signaled`, 11 before-runs
and 10 after-runs, alternating arms, `scripts/wp0-soak.sh` in `MODE=traffic` on loopback with
`LOCKSTAT=1` (`RAYLAND_S_LOCKSTAT`) and `NO_BUILD=1` so each arm runs its own preserved binary.

Each block holds the run's `runs.tsv` row — including the contamination columns, since the owner was
using the machine throughout — and the last `S1LOCKSTAT` report from that run's S log.

Reading the `S1LOCKSTAT` histograms: each entry is `<bucket lower bound>us:<count>`, and the bucket
lower bounds are powers of two in **nanoseconds** printed as microseconds, so several early buckets
print as `0us`. Order, not the label, identifies the bucket.

| | before | after | |
|---|---|---|---|
| `reply_arena_fence_signaled` p50 | 1048 µs | 131 µs | 8× |
| the same, p99 | 134 ms | 8.4 ms | 16× |
| the same, worst bucket | 537 ms | 33.5 ms | 16× |
| total lock-held per run, median | 4.7 s | 0.57 s | 8.3× |
| message thread's total lock WAIT | 2.15 s | 0.73 s | 2.9× |
| median frame gap | 76 ms | 69 ms | **not significant** |

**The last row is not a result.** Mann-Whitney on the frame gap gives p = 0.46. Attaches in the fixed
window moved 67 → 79 in the same direction and are the same measurement. With three before-runs the
end-to-end difference looked convincing; with eleven it does not. The mechanism rows above are
results — ratios of 3× to 16× against run-to-run ranges that do not overlap.

## Reproducing

```
# the four-point link trace (perturbing; use for ratios and counts, not for fps)
C_HOST= MODE=traffic RUNS=1 FRAMES=60 LINK_LOG=1 OUT=/tmp/lt scripts/wp0-soak.sh

# the lock histograms (cheap; safe to quote for timing)
C_HOST= MODE=traffic RUNS=1 FRAMES=60 LOCKSTAT=1 OUT=/tmp/ls scripts/wp0-soak.sh
grep -A9 S1LOCKSTAT /tmp/ls/run1/s.log | tail -9
```
