# (c)2 true-remote validation — the readback path holds, and the mapped-memory race bites

**Status:** findings, 2026-07-19. Records a two-machine (real-network) run of the (c)2 readback
completion barrier and the `rayland-icosa-cpu` fixture, and what it caught. Companion to
[`2026-07-19-c2-ringidx-decode.md`](2026-07-19-c2-ringidx-decode.md) (the readback barrier) and
[`../icosa-fixtures.md`](../icosa-fixtures.md) (why the fixtures exist).

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
  call**): **120/120 frames produced, ~118 bit-identical, ~2 stale, 0 corrupt** (the rate varies
  run to run — one run was 0/120, two runs were 2/120). No wedge, no `SIGABRT`, no `invalid ring_idx`.

## The finding: the stale frames are a *forward* mapped-memory race, not a readback-delivery lag

A stale relayed frame N equals native frame **N-1** — the *whole* previous frame, geometry and
fractal, not a torn hybrid. Two hypotheses fit that symptom, and they had to be separated:

- **Forward mapped-sync lag** — S renders frame N's draw commands (which arrive on the ring, in order)
  against frame **N-1's** mapped inputs (fractal + uniforms) because the mapped-blob relay lagged, so
  S produces frame N-1's image. S's own readback then holds N-1.
- **Readback-delivery lag** — S renders frame N correctly, but C receives the previous frame's pixels.

They make opposite predictions about **what S actually rendered**, so a throwaway spike dumped S's own
readback buffer per delivery (the `RAYLAND_C1_DUMP_S_FRAMES` hook, since reverted). In the run that
had 2 stale frames (5 and 95), S's per-delivery render sequence was:

```
around frame 5:   ... 3,3, 4,4,4,4, 6,6 ...     (S rendered frame 4 FOUR times, frame 5 ZERO times)
around frame 95:  ... 93,93, 94,94,94,94, 96,96 ...
frames S NEVER rendered  = exactly {5, 95}  (the stale relayed frames)
frames S rendered >twice  = exactly {4, 94}  (their predecessors)
```

**S itself never produced frames 5 or 95** — it drew frame 4/94 an extra time. So the readback path
(the (c)2 completion barrier) is faithful: it delivered exactly what S rendered. The defect is
**upstream, on the forward path**: the app's per-frame mapped writes reached S one frame behind the
ring's draw commands. The draw and the mapped-blob content are two separate channels over the network,
and when the mapped channel lags, S draws current geometry over stale mapped inputs.

This is precisely the **(c)2 mapped-memory coherence problem** the icosa fixtures were built to expose
(`../icosa-fixtures.md`). Refined over the old pessimistic framing: it is **not** "mapped writes cannot
cross a network" — they cross fine 118/120 of the time. It is a **coherence *race*** between the
mapped-blob relay and the ring command stream, landing the mapped data one frame late at ~2/120. It is
**invisible on loopback** (0/120 — same-machine shared memory / microsecond timing) and only appears
on a real link.

## Why the readback barrier is *not* implicated (and is confirmed correct here)

- Every stale relayed frame is a *complete real* prior frame, never torn or corrupt — so the readback
  was read after a real completion, i.e. the fence worked.
- The per-delivery dumps show S's readback always matched some real native frame (0 unmatched except
  the pre-first-frame init), and never a frame older than one already seen — so no readback was shipped
  before its render finished.
- No `invalid ring_idx` / `readback fence failed` / deadline bail fired over the network — the
  head-gate, destroy-close, and submit-dispatch trigger all held across a real link, including the
  teardown `vkDestroyDevice` close.

## Incidental: two submits per frame

`rayland-icosa-cpu` issues **2 `vkQueueSubmit`s per frame** (S delivers each readback twice; 240
deliveries for 120 frames, each the correct frame). Not the cause of the staleness, but it violates
the "single submit per readback frame" assumption the readback trigger documents — worth remembering
when that assumption is next relied on.

## Method notes (to reproduce)

- `scripts/c1-two-machine.sh` (refapp, headless PNG compare) is the committed baseline; the icosa
  variant and the per-frame-dump spike were run from the session scratchpad and are not committed.
- The spike added `Applier::debug_app_blob_of_size(len)` + a `RAYLAND_C1_DUMP_S_FRAMES`-gated dump of
  S's readback (256×256×4) and fractal (512×512×4) per delivery; comparison used the app's native PNGs
  decoded to RGBA (`PIL`). Both reverted after the finding.
- The rate is a race; judge it over several runs, never one.

## What this hands to the next (c)2 step

The readback return path is done and holds over a real network. **The open (c)2 problem is now
located precisely:** make the forward mapped-blob relay coherent with the ring command stream, so a
per-frame mapped write cannot land after the draw that consumes it. That is a `rayland-c` /
relay-ordering problem (when and how the uninterceptable mapped blob is snapshotted and shipped
relative to the submit that reads it), not a readback problem.
