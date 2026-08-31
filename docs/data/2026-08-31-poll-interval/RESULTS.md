# Poll-interval experiment — results

Design, sample size and prediction were fixed in `PREREGISTRATION.md` **before any data was
collected**. Nothing below deviates from it. The park-sleep follow-up at the end is labelled
exploratory because its hypothesis was formed after seeing the first result.

## Manipulation check (pre-registered) — passed

Progress-thread iterations per run, median: **A 36,958 · B 115,804 · C 36,078 · D 110,753**. The
`POLL=20 µs` arms really did poll ~3× harder (not 10×, because the loop's own work dominates at
20 µs). The C-side knob has no counter, so its plumbing was verified directly instead: a daemon
launched with `RAYLAND_C1_PARK_SLEEP_US=50` shows it in `/proc/PID/environ`, and the harness line that
sets it is `scripts/wp0-soak.sh:231`.

## Primary result — median inter-frame gap, n = 10 per arm

| arm | `PARK_SLEEP` | `PROGRESS_POLL` | median gap | range | vs control |
|---|---|---|---|---|---|
| **A** control | 500 µs | 200 µs | **59.0 ms** | 43–75 | — |
| **B** | 500 µs | **20 µs** | **59.0 ms** | 43–81 | 1.000× · **p = 0.94** |
| **C** | **50 µs** | 200 µs | **77.0 ms** | 57–89 | 0.77× · **p = 0.004** |
| **D** | **50 µs** | **20 µs** | **74.5 ms** | 57–89 | 0.79× · **p = 0.017** |

Zero stalls in all 40 runs; the contamination check was clean throughout.

### 1. `PROGRESS_POLL` does nothing, and its doc comment's `[INFERENCE]` is refuted

Arm B is a null of unusual quality: **medians identical to one decimal place, p = 0.94**, with the
manipulation check proving the poll really ran 3× more often. `PROGRESS_POLL`'s own documentation
asserts that "on a loopback link, where the RTT is microseconds, this becomes the dominant term". It
is not the dominant term. It is not a measurable term at all. That comment has been corrected.

### 2. The earlier experiment was right about the outcome and wrong about the cause

The previous attempt reported 200→20 µs as **worse** (11.2 fps against 12.5–14.9) and that was taken
as refuting a shorter poll. It changed **both** intervals together. The factorial separates them: the
damage is entirely `PARK_SLEEP`'s (arm C, p = 0.004, without touching `PROGRESS_POLL` at all), and
arm D — both knobs shortened — is statistically indistinguishable from arm C. `PROGRESS_POLL`
contributed nothing to that earlier result in either direction.

### 3. Why a *shorter* park sleep hurts — the mechanism, from C's own metrics

Ring messages per run, median: **A 323 · C 456 (+41%)**, with total C→S bytes unchanged. A shorter
park wakes the ring watcher *between* Mesa's doorbell kicks, so the same ring bytes leave as more,
smaller deltas. Every delta costs C a full sweep of the blob table (an 8 MiB staging-pool `memcmp`
among others) and S an applier-lock acquisition on its message thread. Fragmentation, again — the same
shape as the one-to-three-byte `BlobData` flood fixed earlier the same day.

## Exploratory follow-up — is a *longer* park sleep better?

Registered in the addendum before running; n = 10 per arm, contemporaneous control.

| arm | `PARK_SLEEP` | median gap | range | vs control | ring msgs |
|---|---|---|---|---|---|
| **A′** control | 500 µs | 64.5 ms | 54–153 | — | 318 |
| **E** | 1000 µs | 66.0 ms | 51–85 | 0.98× · p = 0.91 | 300 |
| **F** | 2000 µs | 63.0 ms | 57–140 | 1.02× · p = 0.88 | 310 |

A clean null — **and the mechanism check is null too**, which is the informative part: ring messages
stay at ~300–318 no matter how long the park is. Lengthening it batches nothing, because at 500 µs the
watcher is **already not timer-driven** — it is woken by Mesa's doorbell kick (throttled to at most
one per millisecond, `vn_ring.c:475-483`). The timer only starts to bind below that, which is exactly
where arm C found the damage.

So `PARK_SLEEP` sits on a **flat optimum at or above 500 µs**: shortening it costs ~1.3×, lengthening
it buys nothing. The shipping default is correct and should be left alone.

## What this closes, and what it leaves open

Both defaults are vindicated and **neither poll interval is the frame-time term**. Together with the
day's other results, four candidate explanations have now been eliminated by measurement:

| candidate | verdict |
|---|---|
| S's lock contention (`reply_arena_fence_signaled`) | 8.3× less lock-held → **no frame-rate change** |
| C's forward message rate | 6.1× fewer messages, 5.4× less `send()` → **no frame-rate change** |
| S's `PROGRESS_POLL` | 3× more polling → **no change, p = 0.94** |
| C's `PARK_SLEEP` | shorter is 1.3× **worse**; longer does nothing |

**The leading remaining hypothesis is that frame time is set by Mesa's own back-off, not by Rayland.**
With fence feedback off the application implements `vkWaitForFences` by polling `vkGetFenceStatus`,
and the interval between polls is chosen by Mesa's `vn_relax` (yield for 16 iterations, then an
exponentially growing sleep from 10 µs — `vkr_ring.c:190-210`). If the application sleeps in
`vn_relax` between polls, then every microsecond Rayland saves is absorbed by the app waiting longer
before it next asks — which is precisely the pattern of four independent large mechanism wins moving
nothing. This is a hypothesis, not a finding; it has not been tested. The test is to timestamp the
application's successive ring writes and compare the distribution against `vn_relax`'s schedule.

## Reproducing

```
PARK_US=500 POLL_US=200 C_HOST= MODE=traffic RUNS=1 FRAMES=60 NO_BUILD=1 LOCKSTAT=1 \
  OUT=/tmp/poll scripts/wp0-soak.sh
grep -A9 S1LOCKSTAT /tmp/poll/run1/s.log | grep 'progress thread lock WAIT'   # manipulation check
```
