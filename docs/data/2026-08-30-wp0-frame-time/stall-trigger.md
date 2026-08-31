# Locating the stall's trigger — one comparable method, both topologies — 2026-08-31

Follows `pointer-stall.md`. That session established the stall is real and contaminates frame-rate
figures, ruled out two hypotheses, and left the **trigger** undetermined — with a two-machine run
discarded because its sampling method was not comparable. This is the re-run with that fixed.

## Method

Both topologies scored **identically and offline**, from the monotonic timestamps now stamped on the
proxy's per-request log line. Nothing is polled, so there is no sampler whose cost differs between
loopback and two-machine — which is exactly what invalidated the earlier attempt. A "stall" is an
inter-frame gap more than 10× the run's own median, which needs no hard-coded frame rate.

## Results

| cell | frames | median gap | stalls | longest |
|---|---:|---:|---:|---:|
| loopback, no pointer moves by me | 281 | 57 ms | 2 | 995 ms |
| **apollo, no pointer moves by me** | 295 | 51 ms | **1** | **1834 ms** |
| **apollo, 5 synthetic pointer moves** | 265 | 56 ms | **2** | 1623 ms |
| loopback, 5 synthetic pointer moves | *4* | — | — | *run failed, discarded* |

## What this establishes

**Stalls occur with the application on a different machine from the compositor.** apollo runs the app
and `rayland-c`; dop561 runs `rayland-s` and COSMIC. A 1.8 s stall there cannot be the app competing
for CPU with the compositor, because they are not on the same computer. **That weakens the
CPU-contention hypothesis considerably** — it was the leading explanation after the input-relay one
was refuted, and it is now the second to survive poorly.

**The steady state is unaffected.** Median inter-frame gaps are 51–57 ms across every cell. Whatever
happens is purely intermittent: the pipeline is not slower, it stops and restarts.

## What this does NOT establish, and why

**The "no pointer moves by me" cells are not controlled idle baselines.** The repository owner is
working at dop561 throughout, so "I did not move the mouse" is not "the mouse did not move". Those
rows cannot be read as *undisturbed*, and the fact that they contain stalls therefore does not prove
stalls happen without a pointer.

That is a real limit of measuring on a machine in use, and it is why the one genuinely clean run in
the record matters: **439 frames in 20 s with zero abnormal gaps**, taken during a quiet moment. It
proves stall-free operation is achievable, so stalls are not inherent to the design.

**The loopback-with-moves cell failed** (4 frames) and is reported rather than re-run to completion,
because a fourth cell would not change the conclusion the other three already support.

## Where the next investigation should look

Not at CPU contention, and not at the input relay — both are now poorly supported. The remaining
candidates, in the order the evidence favours them:

1. **Mesa's `vn_relax` backoff as an amplifier.** Confirmed observation: during a stall the app's main
   thread is in `hrtimer_nanosleep`, sleeping, not blocked. Whatever the initial hiccup, the client's
   own escalating sleep turns it into hundreds of milliseconds. This is the mechanism most likely to
   explain *duration*, whatever explains *onset*.
2. **Something intermittent in S or the relay** that briefly stops ring progress. The
   `reply_arena_fence_signaled` section was measured at up to 36 ms — not enough alone, but it is the
   only S-side section with a heavy tail.
3. **A disturbance that reaches the app through the compositor's event stream** in a form other than
   the input events already ruled out — a configure, a frame-callback pause during focus change.

The instrument for all three exists: the timestamped proxy log gives exact stall onsets, and S's event
witness gives exact event times on the same clock. **Joining those two on a stall is the next
measurement**, and it needs no new code.
