# Instrumenting the S-side span — and finding the 16 ms was never on S

## The brief

The 2026-08-31 measurement localised frame time to **~3.1 ring-delta round trips per frame at ~16 ms
each** and said the remaining unmeasured span was "what S does between reading a delta and the first
reply reaching C". So: instrument S and find it.

## What was built

`crates/rayland-s/src/stages.rs`, gated on `RAYLAND_S_STAGES` — seven stages of S's handling of one
relayed ring delta. The recording mechanism moved into `rayland_relay::stagelog`, **shared with C's
`relaxstat`**, so the two sides' numbers stay comparable and one instrument cannot drift from the
other. On loopback both daemons share `CLOCK_MONOTONIC`, so the two records join directly.

Two events were also *added* to C (`SyncPrepared`, `SyncSent`) rather than repositioning the existing
`RingShipped` — moving it would have silently invalidated comparison with the previous day's data.

## Result 1 — S is not where the time is

Per relayed ring delta, S's whole span from reading it to putting the reply on the link:

| stage | median | share of S's span |
|---|---|---|
| read → delta applied to ring memory | 1.90 ms | 41% |
| **applied → `head` moved (virglrenderer executing)** | **0.20 ms** | **1%** |
| `head` moved → reply on the link | 2.76 ms | 54% |
| **all of S** | **4.96 ms** | — |

**virglrenderer is not the problem: 0.20 ms.** And all of S is 4.96 ms of a ~19 ms round trip.

## Result 2 — the joined round trip, and the answer

Joining C's `RELAXSTAT` and S's `SSTAGE` on the shared clock, 630 round trips over 3 runs:

| stage | median | share |
|---|---|---|
| **1a C: diff every blob + serialize** | **9.99 ms** | **51.1%** |
| 1b C: write + flush the batch | 0.16 ms | 1.6% |
| 1c transit + S works through the batch | 1.34 ms | 7.2% |
| 2a S: read → delta applied | 1.90 ms | 9.1% |
| 2b S: applied → `head` moved (virglrenderer) | 0.20 ms | 1.1% |
| 2c S: `head` moved → reply on the link | 2.76 ms | 17.4% |
| 3 transit back + C applies | 1.13 ms | 12.6% |
| **TOTAL round trip** | **19.27 ms** | — |

**Over half the round trip is C diffing blobs**, and it is on **C** — the machine that may be the weak
one — not on S at all.

## The cause, and the fix

`blob_sync::messages_for_delta` diffs **every** blob against its baseline on **every** ring delta.
For `vkcube` that is **13.2 MiB** — an 8 MiB Venus staging pool, a 1 MiB reply arena, four 1 MB
swapchain images and the rest — roughly three times per frame.

`LocalBlob::take_changed_runs` compared it **64 bytes at a time**. S's equivalent
(`rayland_s::blob`) was raised from 64 to 4096 in August; **C's was simply never changed with it**, on
the side where it costs the most.

Raising C's to 4096 (`shm::DIFF_CHUNK`) is a **speed change only** — a differing chunk is still walked
byte by byte and a run straddling a boundary is still emitted whole, which is exactly what the guard
test asserts against a naive reference.

## Measured

Mechanism, from the stage record itself:

| | before | after | |
|---|---|---|---|
| stage 1a, median | 9.13 ms | **2.17 ms** | **4.2×** |
| stage 1a, p90 | 24.75 ms | 4.75 ms | 5.2× |

End to end, 11 interleaved pairs:

| | before | after |
|---|---|---|
| median inter-frame gap | **80.0 ms** | **45.0 ms** |
| range | 46–140 | 28–67 |

1.78× faster, Mann-Whitney p = 0.0025 — **on DEBUG binaries, which is what this harness built by
default, and that qualification turns out to matter more than the number.**

> **CORRECTION, same day.** Re-run on **release** binaries on this same laptop, the difference
> disappears: **35 → 37 ms, p = 0.38, n = 13 per arm — a clean null.** Debug Rust keeps the bounds
> checks and per-iteration overhead of a byte loop, so it exaggerates exactly the cost this change is
> about. The 1.78× above is a true statement about a debug build and was wrong to publish as a
> headline.
>
> **The change is still right, and the evidence for it is the riscv64 board**, where it measures
> **105 → 71 ms, 1.48×, complete separation, p = 0.0015** in *release* — see
> `docs/data/2026-09-01-milkv-ab/`. That is the shape this project should expect: C is the weak
> machine by design, and a fixed per-delta scan costs it far more than it costs a fast x86_64 laptop
> whose optimiser can hide it.

## The guard test had two blind spots, both found by mutation

The test that makes "speed only, not meaning" safe to claim was written for a 64-byte chunk with a
512-byte buffer and hand-written offsets around 64.

1. **At `CHUNK = 4096` the whole 512-byte buffer becomes one chunk**, and every "straddles a boundary"
   case would have tested nothing while still passing. The chunk size is now a module constant the
   test derives its buffer size *and* its case offsets from, so the coupling is structural rather than
   remembered.
2. **`SIZE` was an exact multiple of the chunk**, so no case ever had a trailing partial chunk — an
   implementation that simply *dropped* one passed the whole suite. `SIZE` is now `CHUNK * 5 + 37`.

Mutations run after both fixes: skipping the first byte of each differing chunk — **caught**; dropping
a trailing partial chunk — **caught**; not re-baselining inside a differing chunk — **caught**; forcing
every run closed at a chunk boundary — **not caught, and correctly so**: `coalesce_ranges(_, 0)` merges
*adjacent* ranges, so a boundary split is repaired before it reaches the wire and the mutation is
behaviour-preserving by construction. That last one is worth knowing: gap 0 is inert against the
correct diff's output but is *not* a no-op against a split one.

## What is left

Stage 1a is still 2.17 ms and still runs ~3 times per frame — roughly 6.5 ms of a 45 ms frame, and now
close to the memory-bandwidth floor for a 13.2 MiB `memcmp`. Making it smaller means **not walking
13.2 MiB per delta at all**: only the blobs that could have changed, or real dirty-page tracking
(`/proc/PID/clear_refs` soft-dirty, or `userfaultfd`). That is a design change, not a constant.

Also note the swapchain images are 4 MB of those 13.2 MiB and the application never CPU-writes them —
they are diffed on every delta to discover, every time, that nothing changed.

## Reproducing

```
C_HOST= MODE=traffic RUNS=1 FRAMES=60 NO_BUILD=1 STAGES=1 RELAXSTAT=1 OUT=/tmp/st scripts/wp0-soak.sh
python3 analyse-s-stages.py    /tmp/st/run1/s.log
python3 analyse-round-trip.py  /tmp/st/run1/c.log /tmp/st/run1/s.log
```
