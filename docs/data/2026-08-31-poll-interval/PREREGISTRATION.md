# Poll-interval experiment — pre-registered before any data was collected

Written and committed **before the first run**, because twice on 2026-08-31 a small sample flattered
the result I was hoping for (the fence-scan fix looked like a win at n=3 and was nothing at n=11; the
starved-C test looked like 1.28× at n=7 and was 1.03× at n=13). Both times the bias ran towards the
answer I wanted. Deciding the design in advance is the only defence.

## What is being tested

Two poll intervals sit on the application's synchronous round trip:

- **`PARK_SLEEP`** (C, 500 µs) — how fast C's ring watcher notices the application wrote to the ring.
- **`PROGRESS_POLL`** (S, 200 µs) — how fast S's progress thread notices the ring moved, and so how
  fast the head-advance that *releases the application* goes out.

`PROGRESS_POLL`'s own doc comment marks its value **`[INFERENCE]` — "never measured"** while asserting
that on a loopback link "this becomes the dominant term". This settles that.

## Why it is being re-run now

It was run before and appeared refuted: 200→20 µs measured *worse* (11.2/11.3 fps against a 12.5–14.9
baseline). That run is not evidence about today's system, for two reasons.

1. It was taken when **every progress poll dragged a ~2 ms reply-arena scan behind it**. Polling ten
   times more often multiplied that scan's lock-held cost. That scan is now 131 µs at the median
   (2026-08-31, `memchr` rewrite), so the cost of polling harder has fallen by roughly 8×.
2. It changed **both** knobs at once, so it could not attribute its result to either — and it compared
   two separately-built binaries.

Both are fixed here: the intervals are read from the environment (`RAYLAND_S_PROGRESS_POLL_US`,
`RAYLAND_C1_PARK_SLEEP_US`), so **one binary serves every arm**, and the design is factorial.

## Design — fixed in advance

| arm | `PARK_SLEEP` (C) | `PROGRESS_POLL` (S) |
|---|---|---|
| **A** (control, shipping default) | 500 µs | 200 µs |
| **B** (S only) | 500 µs | 20 µs |
| **C** (C only) | 50 µs | 200 µs |
| **D** (both) | 50 µs | 20 µs |

- **n = 10 runs per arm**, 40 runs total. **This number is fixed now and will not be extended or
  truncated on the basis of the results.**
- Arms are **interleaved** (A,B,C,D repeated) so drift in machine load cannot align with an arm.
- Loopback, `scripts/wp0-soak.sh MODE=traffic FRAMES=60 NO_BUILD=1 LOCKSTAT=1`.

## Metrics

- **Primary: median inter-frame gap**, per run, as the harness already reports it. Not `attaches` —
  in `MODE=traffic` that overshoots by a poll-granularity-dependent amount and is not a rate.
- **Manipulation check: the progress thread's iteration count** from `S1LOCKSTAT`
  (`progress thread lock WAIT` n). If arm B does not show far more iterations than arm A, the
  environment override did not take effect and *no other number in the run means anything*.
- Secondary: contamination columns (stall count, longest gap), since the owner is using the machine.

## Analysis — fixed in advance

Medians and full ranges for every arm; two-tailed Mann-Whitney of A against each of B, C and D on the
frame gap. Significance is not the headline on its own: an effect that is significant but smaller than
a few percent is reported as small.

## Prediction, stated before looking

Per synchronous round trip the two polls add on average `PARK/2 + POLL/2` — **350 µs at control,
35 µs at arm D**, a saving of ~315 µs per round trip.

The number of round trips per frame decides whether that matters, and the two available estimates
disagree by 6×:

- `OVERVIEW.md` §5.3 says **≈4.4 synchronous round trips per frame** → saving ≈ **1.4 ms** of a ~61 ms
  frame ≈ **2%**, i.e. invisible.
- The 2026-08-31 link trace counted **~28 reply-arena `BlobData` per frame** → saving ≈ **8.8 ms**
  ≈ **14%**, i.e. clearly visible.

So this experiment also discriminates between those two counts, which is worth as much as the tuning
answer. **My expectation is the smaller effect** — the day's other two fixes each collapsed a
mechanism by 5–8× and moved the frame rate by nothing, which is the behaviour of a system whose frame
time is set by something not yet identified. I expect arm D to be within a few percent of arm A, and
I am writing that down so that a null result cannot be retold afterwards as "expected all along"
while a positive one gets claimed as a prediction.

---

# Addendum — a second, EXPLORATORY experiment, registered after the first was analysed

The pre-registered experiment above is complete and its result stands on its own. This addendum is a
**follow-up prompted by that result**, and it is labelled exploratory precisely because its hypothesis
was formed *after* seeing data. It must not be reported with the same weight.

**What prompted it.** Halving `PARK_SLEEP` (500 → 50 µs) made things ~1.3× *slower*, and the
mechanism is visible in C's own metrics: ring messages per run rose **323 → 456 (+41%)** while total
bytes were unchanged. A shorter park wakes the watcher more often, so the same ring bytes leave as
*more, smaller* deltas — and every delta costs C a full sweep of the blob table (an 8 MiB staging-pool
`memcmp` among others) and S an applier-lock acquisition on its message thread.

**Hypothesis.** If fragmenting the delta is what hurts, then *lengthening* the park should batch more
ring bytes per delta and help — until the added forward latency outweighs it. There should be an
optimum, and 500 µs was never chosen by measurement.

**Design — fixed before running.**

| arm | `PARK_SLEEP` (C) | `PROGRESS_POLL` (S) |
|---|---|---|
| **A′** (contemporaneous control) | 500 µs | 200 µs |
| **E** | 1000 µs | 200 µs |
| **F** | 2000 µs | 200 µs |

`A′` is re-run alongside rather than compared against the earlier arm A, because machine load drifts
across an afternoon and the owner is using the machine.

- **n = 10 per arm**, 30 runs, interleaved A′,E,F. Fixed now.
- Same harness, metrics and analysis as above. Primary metric is again the median inter-frame gap;
  ring message count is recorded as the mechanism check.

**Prediction.** If the fragmentation account is right, E and F beat A′, with F possibly turning back
up as forward latency starts to dominate. If E and F are flat, the fragmentation account is wrong and
arm C's damage has some other cause.
