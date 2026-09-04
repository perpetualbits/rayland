# Does the 1.23× feedback speedup survive a real network? — inconclusive, leaning no

**Design:** `PREREGISTRATION.md`, written before any data. `icosa-gpu`, apollo → dop561, release,
Intel-pinned, n = 10 per arm, interleaved ABAB, metric fixed in advance as the per-run median
`draw_readback_us`.

## Result

| | |
|---|---|
| A, shipping (all five flags) | median of run-medians **20.95 ms** |
| B, feedback on (`no_multi_ring,no_fence_feedback`) | median of run-medians **19.45 ms** |
| **Ratio A/B** | **1.077×** |
| Mann–Whitney U, two-sided | U = 37, **p = 0.33** |
| Bootstrap 95% CI (20,000 resamples) | **0.895× – 1.273×** |

Per-run medians, in the order they ran (ms):

```
A shipping   :  18.30 18.51 20.95 18.42 21.39 21.56 21.39 21.20 20.95 16.64
B feedback-on:  14.75 15.25 16.45 20.95 20.59 21.59 25.48 20.31 17.89 18.59
```

All 20 runs relay-verified and 120 frames; none dropped.

## What this supports

**The loopback 1.23× is not reproduced on a real network.** Observed run-to-run variability is a
pooled SD of 2.50 ms on a 19.6 ms mean (CV 12.8%), which means **n = 7 per arm suffices to detect a
23% effect at 80% power** — the design used 10 and found nothing. Failing to detect an effect the
design was powered for is evidence against that effect.

## What this does NOT support, and the distinction matters

**It does not refute a benefit.** The 95% CI runs to 1.273×, so a real ~1.2× effect is *not excluded*
— the data simply cannot separate it from no effect at all. Reporting this as "the 1.23× is refuted"
would overstate it in the same direction, and by the same amount, as reporting the point estimate as a
win would.

**Honest summary: the real-network effect is probably smaller than the loopback figure, plausibly
zero, and this experiment cannot rule out ~1.2×.**

## What it means for the decision

The **only** remaining reason to enable semaphore/event/query feedback was the 1.23×; reliability is
settled (400/400 clean on each arm, Fisher p = 1.0). That reason is now materially weaker: the best
estimate of the benefit where it would actually be claimed is **~8%, not significant**.

If anyone wants the question closed rather than weakened, **n = 31 per arm resolves a 10% effect at
80% power** (~1 hour on these machines). Whether an ~8% frame-time gain is worth adopting a
configuration with one historically unexplained session loss is a judgement, not a measurement.

## Two method notes worth more than the number

**The pre-registration earned its keep on the first pair.** Run 1 alone was **1.24×** — almost exactly
the loopback claim. Stopping there, or peeking and then deciding when to stop, would have "confirmed"
the hypothesis. This project has been flattered by a small sample three times before (1.78× that was a
null; 1.28× that was 1.03×; a win at n=3 that vanished at n=11); this is the first time the design
caught it in advance instead of a later re-run catching it after publication.

**The first attempt at this experiment measured nothing at all,** and is kept in the record rather than
quietly replaced. A comment placed inside a shell line continuation terminated the environment
assignments early, so the fixture ran with no Venus ICD and no vtest socket: it rendered on apollo's
own GPU and never touched Rayland, while still producing 120 well-formed frames and a plausible CSV.
Twenty runs were collected that way and reported a null at p = 0.88. The tells were all present —
3.43 ms where the record says 10.1, both arms identical to three digits, `VN_PERF` absent from the
logs — and the driver's own stale-frame check printed blank because its pattern did not match the
harness's wording, which was read as success. The harness now carries a **witness**: a run must show
in `rayland-c`'s own log that it relayed a session, or the sweep aborts rather than record a number.
