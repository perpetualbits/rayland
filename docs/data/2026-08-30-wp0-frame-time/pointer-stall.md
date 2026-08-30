# Pointer motion stalls the application — and it contaminated every frame-rate number — 2026-08-30

**Reported by the repository owner**, who was working at the machine while measurements ran: *"vkcube
stops spinning for almost three seconds whenever the mouse enters or leaves the window. So your
measurements will be off."* Both halves are correct. This documents what is established and what is
not.

## 1. The contamination is real and large

Per-100 ms frame timeline, loopback, COSMIC, `vkcube`:

| condition | frames | fps | 100 ms buckets with **zero** frames | longest stall |
|---|---:|---:|---:|---:|
| **no pointer movement at all** | 439 / 20 s | **21.9** | **0 of 200** | **0 ms** |
| synthetic pointer moves (5 in 20 s) | 413 / 20 s | 20.6 | 5 of 200 | **500 ms** |

The undisturbed run is *perfectly smooth* — not one empty bucket in two hundred. The disturbed run has
a clean 500 ms hole immediately after the first move:

```
t=3.9s   3 2 3 2 2 | 0 0 0 0 0 | 1 2 2 2 2 …
```

**Consequence for the record: 21.9 fps is the clean figure, and every frame-rate number measured
before this — including the 14.33 vs 16.96 fps chunk-size A/B — was taken while a human was using the
machine.** The A/B's conclusion survives (it was interleaved, so contamination hits both arms alike,
and the distributions did not overlap), but its absolute values are low and should not be quoted.

## 2. What the application is doing during a stall

Sampled from `/proc/<pid>/task/*/wchan` inside a caught stall:

```
vkcube      wchan=hrtimer_nanosleep
vn_wsi[0,0] wchan=futex_do_wait
```

The main thread is **sleeping**, not blocked on I/O. That is Mesa's ring-wait backoff
(`vn_relax`): it spins, then sleeps for progressively longer intervals when `head` does not advance.
So a *brief* hiccup in ring progress is **amplified** into a long stall by the client's own backoff —
which is the most likely reason a momentary disturbance costs hundreds of milliseconds, and why the
owner sees ~3 s when moving a real mouse continuously rather than in five discrete jumps.

## 3. What has been ruled OUT

| Hypothesis | Test | Result |
|---|---|---|
| S holds the applier lock through the stall | `section_log` at 1 µs threshold across a stall | **No** — worst section 36 ms, and the progress thread kept looping (43,628 heartbeats) |
| Relaying the seat/pointer/keyboard events is the cost | env-gated suppression of all `wl_seat`/`wl_pointer`/`wl_keyboard` relay, A/B | **No — and it got worse**: 7 empty buckets with relay on, **17** with it off |

That second result is worth keeping. The obvious explanation — a burst of input events contending for
the single shared send link — is **wrong**, and would have been an entirely plausible thing to "fix".

## 4. What is NOT established

- **The trigger.** Whether the hiccup comes from compositor CPU contention (on loopback the app, C, S
  and COSMIC share one machine), from focus-change work in COSMIC, or from something in the relay, is
  **not determined**. An attempt to test it by moving the app to apollo produced a number that is not
  comparable, because the sampling loop used one `ssh` per sample and therefore paced at ~250 ms
  rather than 100 ms. That run is discarded rather than reported.
- **Whether it is a Rayland defect at all.** It may be that Rayland is merely *sensitive* to a hiccup
  that a native client would absorb, because `vn_relax` turns a millisecond into hundreds.

## 5. What to do next, in order

1. **Make every measurement report its own contamination.** The empty-bucket count is a cheap,
   decisive contamination check — 0/200 means the number is clean, anything else means it is not. Any
   harness quoting an fps figure should print it. This is worth doing before any further tuning,
   because it is what makes the numbers trustworthy.
2. **Establish the trigger** with a comparable method: same 100 ms sampling on both sides, app on
   another machine, pointer moved on the display machine. Loopback vs two-machine, same harness.
3. **Then** decide whether the fix is in Rayland (keep ring progress prompt enough that `vn_relax`
   never escalates) or is simply "do not measure on a machine someone is using".
