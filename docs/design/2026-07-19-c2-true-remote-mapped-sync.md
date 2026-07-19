# (c)2 true-remote validation — the stale frame is a readback-completion lag, not a forward mapped race

**Status:** findings, 2026-07-19. Records a two-machine (real-network) run of the (c)2 readback
completion barrier and the `rayland-icosa-cpu` fixture, what it caught, and — importantly — the
**correction of this document's original conclusion.** Companion to
[`2026-07-19-c2-ringidx-decode.md`](2026-07-19-c2-ringidx-decode.md) (the readback barrier) and
[`../icosa-fixtures.md`](../icosa-fixtures.md) (why the fixtures exist).

> ## Correction (same day, after a second experiment)
>
> This document first concluded that the ~2/120 stale frames were a **forward mapped-memory relay
> race** — that the app's per-frame mapped writes reached S "one frame behind" the ring's draw
> commands. **That conclusion was wrong, and it was wrong for an instructive reason.** It rested on a
> spike that dumped *only* S's readback buffer per delivery, saw "S delivered frame N−1 repeatedly,
> never N," and inferred "S rendered against stale forward inputs."
>
> A follow-up experiment added an **independent forward-input witness** — the per-frame **uniform**
> (the MVP block the draw reads directly) — fingerprinted alongside the readback at every delivery on
> S. It inverts the finding: at every stale delivery the **uniform was already the new frame N while
> the delivered image was the old frame N−1**. The forward inputs were *fresh*; the **readback
> delivery** lagged. The defect is in S's own (c)2 completion barrier, not in the `rayland-c` mapped-blob
> relay. The corrected finding and its evidence are below; the original (mistaken) reasoning is kept
> under "Why the first spike misled" as a lesson about single-witness debugging.

## What was run

Topology from `scripts/c1-two-machine.sh`: **C = apollo** (x86_64, its GPU unused, stock Mesa Venus
ICD), **S = dop561** (Intel GPU, the build host). The unmodified app runs on C, its Venus command
stream is relayed by `rayland-c` over a real QUIC link to `rayland-s` on S, replayed on S's GPU, and
read back. Correctness is **the relayed frames vs the app run natively on S's *own* GPU** — both
render on the same Intel GPU, so only the transport differs and the result must be bit-identical.

Two apps:

- **`rayland-refapp`** (one triangle, no per-frame mapped writes): **bit-identical** across the
  network. On-screen presentation on S also works (a window on dop561 rendered from apollo).
- **`rayland-icosa-cpu`** (120 frames; a spinning icosahedron textured with a CPU-computed fractal
  written into **mapped `HOST_COHERENT` memory every frame, with no flush and so no interceptable
  call**): **120/120 frames produced, ~118 bit-identical, ~2 stale, 0 corrupt.** The rate is a race —
  it varies run to run (0/120, 1/120, 2/120, occasionally 4/120). No wedge, no `SIGABRT`, no
  `invalid ring_idx`.

A stale relayed frame N equals native frame **N−1** — the *whole* previous frame, geometry and
fractal together, never a torn hybrid.

## The decisive experiment: the uniform witness

Two hypotheses fit "stale frame N == whole frame N−1", and they make opposite predictions about what
S had in hand when it delivered:

- **Forward mapped-sync lag** — S executed frame N's draw against frame **N−1's** mapped inputs
  (fractal + uniforms), so S itself produced frame N−1's image; the readback path then faithfully
  returned it.
- **Readback-completion lag** — S's forward inputs were already frame N, but the readback S *delivered*
  for frame N's slot was still frame N−1's pixels.

To separate them, `rayland-s` was instrumented (env-gated `RAYLAND_C1_FPLOG`, throwaway) to
fingerprint, at **every** readback delivery, both the **delivered readback** blob and the resident
**uniform** blob — the per-frame MVP the draw reads directly, and therefore a clean, independent
witness to which frame's *forward* inputs S holds. Frame identity for each channel comes from a
deterministic fingerprint→frame map (the app writes uniforms and renders strictly in frame order, and
the readback blob is byte-identical to the native RGBA), so the two axes are directly comparable.

Across two independent stale runs, **every** stale frame showed the identical signature — resident
uniform is the new frame, delivered image is the old one:

| run | stale frame | deliveries | resident uniform | delivered image |
|-----|-------------|-----------|------------------|-----------------|
| 21  | 94          | 189, 190  | **94** (fresh)   | **93** (stale)  |
| 23  | 72          | 145, 146  | **72** (fresh)   | **71** (stale)  |
| 23  | 117         | 235, 236  | **117** (fresh)  | **116** (stale) |

Two further facts nail it:

- **S never delivered the stale frame at all.** Over a whole run the set of distinct delivered images
  was **119, not 120**: frame N's image never appeared in any of the 240 deliveries, and its
  predecessor N−1 was delivered **four** times (its own two slots plus N's two).
- **The forward inputs were provably fresh at those very deliveries** (`uniform = N`), so S was *not*
  rendering against stale mapped memory. What it *shipped* was the previous frame's readback.

That is `uniform = N > image = N−1`: **fresh forward inputs, stale delivered image = a readback-path
defect on S.** The forward mapped-blob relay in `rayland-c` is exonerated.

## Where the defect lives

It is in S's own **(c)2 readback completion barrier** (`rayland-s`'s `progress_thread`) interacting
with the fixture's **two `vkQueueSubmit`s per frame** (an upload copy, then the draw-and-readback).
The barrier's trigger — a newer submit position than last delivered, plus a drained ring, then
`wait_for_work_retired(ring_idx)` — ships the readback blob (`res6`) **without guaranteeing that
blob's *content* corresponds to the newest submitted draw.** Under real-network timing with two
submits per frame, it ships the previous frame's pixels for the current frame's submits and drops the
current frame's readback entirely. On loopback the timing never opens the window (0/120), which is why
the barrier looked correct when it was first landed.

The exact micro-mechanism (which of the two submits the barrier fires on, and why `res6` never shows
frame N in a stale run) is the first thing the fix design must pin down; this document establishes the
**layer** (readback delivery on S), which is what the next step targets.

## Why the first spike misled (kept as a lesson)

The original spike dumped **only** S's readback buffer per delivery. It correctly observed that in a
stale run "S delivered frame N−1 four times and frame N zero times," and concluded **S never
*produced* frame N** — i.e. S rendered frame N's draw against stale forward inputs (a forward race).
The reasoning was internally consistent but had **one witness**: with only the readback in view, "N−1
delivered repeatedly" is equally consistent with "the readback *delivery* repeated N−1 while the
forward inputs advanced correctly." The uniform witness is exactly the second variable needed to break
the tie, and it falls on the readback side. The lesson is the general one: **a single-channel dump
cannot distinguish a stale producer from a stale delivery of a fresh producer; add an independent
witness on the axis you are trying to exonerate.**

## Why the readback barrier's *other* guarantees still held

The barrier is not wholly broken — its structural guards all held across the real link, which is why
the failure is a narrow content/ordering slip rather than a crash:

- No `invalid ring_idx` / `readback fence failed` / deadline bail fired over the network — the
  head-gate, destroy-close, and submit-dispatch trigger all survived a real link, including the
  teardown `vkDestroyDevice` close.
- Every delivered frame was a *complete, real* frame (some prior frame's genuine pixels), never torn
  or corrupt — the fence did wait for a real completion; it simply released against the wrong frame's
  readback.

## Incidental: two submits per frame

`rayland-icosa-cpu` issues **2 `vkQueueSubmit`s per frame** (240 deliveries for 120 frames). This is
no longer an aside: it is **central** to the bug, because the readback trigger assumes one
readback-bearing submit per frame and the fixture violates that. The fix must be robust to N submits
per frame.

## Method notes (to reproduce)

- `scripts/c1-two-machine.sh` (refapp, headless PNG compare) is the committed baseline; the icosa
  variant, the `RAYLAND_C1_FPLOG` instrumentation, and the correlation script were run from the
  session scratchpad and are not committed (throwaway).
- The FPLOG spike logs, at each delivery, a fingerprint of every application blob (`res5`/128 =
  uniform, `res6`/262144 = readback, `res2`/1 MiB = fractal staging, plus Venus-internal blobs).
  Offline, fingerprints are numbered into frames by first-appearance order (uniform/staging) or by
  matching the native RGBA (readback), and the delivered-image frame is compared against the resident
  uniform frame per delivery.
- The rate is a race; judge it over several runs, never one. Catching a stale run took ~1 in 3–5
  runs.

## What this hands to the next (c)2 step

The problem is **located and its layer proven**: a **readback-completion delivery lag on S**, with the
forward mapped-blob relay verified fresh. The fix is a **`rayland-s` readback-barrier change** — make
the delivered readback provably correspond to the frame whose completion released it, robust to N
submits per frame — **not** a `rayland-c` mapped-blob relay change.
