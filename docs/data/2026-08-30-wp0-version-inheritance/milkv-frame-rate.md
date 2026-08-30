# Why milkv gets ~5 fps where apollo gets ~14 — measured, 2026-08-30

The owner watched `vkcube` from milkv (riscv64) on dop561's screen and observed the frame rate was
lower than the board ought to manage. It is, and the reason is **message rate, not the board's
general speed, not the network, and not the application.**

## What was ruled out

| Suspect | Measurement | Verdict |
|---|---|---|
| Network | RTT to dop561: **milkv 0.508 ms**, apollo 0.838 ms | **Not it** — milkv is *faster* |
| Board saturation | milkv load average **1.13 of 4 cores** | **Not it** — three cores idle |
| The application | `vkcube` CPU: **11%** (milkv), 4.3% (apollo) | **Not it** |
| More work per frame on milkv | see below | **Not it** — the work is the same |

## What it is

`rayland-c` is the hot process: **78% of one core on milkv**, 33.8% on apollo. Under one core on
both — so it is not out of CPU, it is **latency-bound on a fixed amount of per-frame work**.

Normalised per presented frame, C's own per-channel counters (`RAYLAND_C1_METRICS=1`), 30 s runs:

| | frames | C→S blob-sync msgs/frame | bytes/msg | C→S ring msgs/frame | S→C replies/frame | fps |
|---|---:|---:|---:|---:|---:|---:|
| apollo (x86_64, 16 cores) | 389 | **72.4** | 21 | 3.5 | 4.7 | 13 |
| milkv (riscv64, 4 cores) | 105 | **76** | 44 | 6.5 | 8.6 | 3.5 |

**The per-frame message counts are the same** (72 vs 76). Both machines push ~75–80 messages per
frame, almost all of them C→S blob-sync messages averaging **21–44 bytes each**. milkv's core is
roughly 3.7× slower at framing, serialising and writing them, and the frame rate falls by almost
exactly that factor.

## The lever

This is the **message-rate bound** (c)1 documented, in its forward direction. The project has already
attacked the equivalent problem once, on the *readback* path, with gap-threshold coalescing:
**~5000 → ~180 messages per frame, bit-identical output, no wall-clock change** on x86 — because on
a fast core the message rate was not what hurt.

**On a slow C it is exactly what hurts.** Coalescing the forward `c2s_blob_sync` path the same way is
the obvious next optimisation, and milkv is the machine that makes its value visible: on apollo the
same change would look like nothing.

Note the asymmetry that makes this attractive: 76 messages/frame at ~44 bytes is **~3.3 KB of payload
per frame** carried in ~76 syscalls' worth of framing. The bytes are already negligible; it is purely
per-message overhead.

## Not measured, and worth doing before optimising

- Where inside `rayland-c` the time actually goes (framing, postcard, the send lock, the syscall).
  78% of a core is a *profile* waiting to be taken, and guessing which of those dominates would be
  the same mistake this project has recorded several times.
- Whether the ~2× larger bytes/message on milkv (44 vs 21) reflects different write patterns from the
  application or a different coalescing outcome; it was not investigated.
