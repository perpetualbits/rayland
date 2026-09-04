# Pre-registration 2 — closing the feedback speed question at n = 31 per arm

**Written before any of this data was collected.** The n = 10 run gave 1.077× with a 95% CI of
0.895–1.273: powered for the 23% it went looking for, but not for the ~8% it found. This run is sized
from that run's own variance rather than from a guess.

## Sizing

Pooled SD of run-medians in the n = 10 experiment: **2.50 ms on a 19.6 ms mean (CV 12.8%)**. For a
two-sample test at α = 0.05 and 80% power that gives **n = 31 per arm to detect a 10% difference**.
31 is therefore the number, fixed here.

## Design — identical to the first, so the two are comparable

- `rayland-icosa-gpu`; C = apollo, S = dop561 `192.168.1.192`; release; S's GPU pinned to Intel.
- **A** = `no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback`
- **B** = `no_multi_ring,no_fence_feedback`
- **Interleaved A,B,A,B,…**, 31 repetitions, 120 frames each.
- Every run must be **relay-verified** (rayland-c's own log shows a relayed session) and produce
  exactly 120 frames and 0 stale frames. **Any run that is not is a hard stop for the experiment**,
  not a dropped sample — the first attempt at this measurement collected 20 runs in which the
  application never touched Rayland, and silently dropping bad runs is how that becomes invisible.

## Primary analysis, fixed now

The **new 31 pairs alone**. Metric: per-run median `draw_readback_us` → 31 run-medians per arm →
ratio of medians, Mann–Whitney two-sided, bootstrap 95% CI (20,000 resamples).

The earlier n = 10 is **not** pooled into the primary result: choosing to pool after seeing a
direction is exactly the freedom pre-registration exists to remove. A pooled estimate over all 41
pairs will be reported **as a clearly-labelled secondary figure**.

## What each outcome will mean — decided in advance

| Bootstrap 95% CI | Reading | Action |
|---|---|---|
| excludes 1.00, lower bound ≥ 1.10 | a real benefit of ≥ 10% | worth weighing against the unexplained session loss |
| excludes 1.00, lower bound < 1.10 | a real but small benefit | almost certainly not worth adopting for |
| **excludes 1.10** (upper bound < 1.10) | any benefit is under 10% | **close the question: do not enable** |
| contains both 1.00 and 1.10 | still underpowered | **stop anyway — see below** |

**Pre-commitment against escalation:** if this run is still inconclusive, the answer is *"the effect
is too small to matter and not worth more machine time"*, not n = 113. An effect that needs 226 runs
to see is not an effect worth changing a configuration for. This is the last sizing.

## The prediction that was wrong last time, restated

I predicted ≥ 1.23× and got 1.077×. My revised expectation is **1.00–1.10×, most likely near 1.05×**.
Recorded so it can be wrong again.
