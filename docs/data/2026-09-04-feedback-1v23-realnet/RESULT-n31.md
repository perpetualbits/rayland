# CLOSED: the feedback speedup does not exist on a real network

**Primary result, n = 31 per arm**, pre-registered (`PREREGISTRATION-n31.md`) and run under a
protocol amendment made in the open before any of this data was collected
(`PREREGISTRATION-n31-AMENDMENT.md`).

| | |
|---|---|
| A, shipping (all five flags) | **18.90 ms** (range 14.88–33.15) |
| B, feedback on | **18.75 ms** (range 14.83–22.11) |
| **Ratio A/B** | **1.008×** |
| Mann–Whitney U, two-sided | U = 436, **p = 0.53** |
| Bootstrap 95% CI | **0.944× – 1.114×** |
| **Re-attempts** | **A = 0, B = 0** |

62 of 62 runs relay-verified, 120 frames, 0 stale. Nothing dropped, nothing retried.

## Against the outcome table fixed in advance

- **Excludes 1.23× — the loopback figure does not transfer.** This is now settled rather than
  suspected: two independent experiments (n = 10 and n = 31) both fail to find it, and this one
  excludes it outright.
- Does not exclude 1.00. The point estimate is **1.008×** — indistinguishable from no effect at all.
- Upper bound **1.114×**, so any benefit that exists is **at most ~11%**, and the interval equally
  admits a **6% penalty**.

By the pre-registered table this lands in the "still not separable" row, and the pre-commitment
attached to it applies: **the answer is that the effect is too small to matter, not that a larger n is
needed.** This was declared the last sizing before the data existed, and it is.

## The decision

**Do not enable `no_semaphore_feedback` / `no_event_feedback` / `no_query_feedback`.**

The case for enabling rested entirely on a 1.23× that was measured over loopback. On a real network:

- **Reliability:** identical. 400/400 clean on each arm, Fisher p = 1.0 (2026-09-02/03).
- **Speed:** 1.008×, p = 0.53, over 31 pairs. And 1.077×, p = 0.33, over an independent 10.
- **Reliability under this experiment:** zero re-attempts on either arm, so no arm-dependent failure
  signal — the amendment's guard against a retry mechanism hiding the answer found nothing to hide.

There is no measured benefit, and the configuration still carries one historically unexplained
session loss. **The question is closed. The shipping configuration stays.**

## Secondary, clearly labelled

Pooled with the earlier independent n = 10 (41 pairs total): **1.057×**. Reported for completeness
only; it was not the pre-registered primary analysis, and pooling was fixed as secondary *before* the
direction of either result was known.

## The prediction, for the record

Pre-registration 1 predicted **≥ 1.23×** and was wrong (1.077×). Pre-registration 2, revised on that
evidence, predicted **1.00–1.10×, most likely near 1.05×**, and was right (1.008×). Both are recorded
because a prediction that is only written down when it turns out well is not a prediction.
