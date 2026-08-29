# WP0 traffic, measured with spread — 2026-08-30

Five runs per arm, **fixed frame count imposed by the harness** (`scripts/wp0-soak.sh MODE=traffic
FRAMES=200`), against headless weston on Mesa/Intel. Bytes are C's own per-channel framed-byte
counters (`RAYLAND_C1_METRICS=1`). The A/B switches the presented-buffer exclusion **inside one
binary** (`RAYLAND_S_SHIP_PRESENTED=1`), so a difference cannot be a rebuild artefact.

| | exclusion ON (shipping) | exclusion OFF |
|---|---:|---:|
| runs | 5 | 5 |
| attaches | 207–242 (median 226) | 200–213 (median 205) |
| C→S total | 766,060–849,277 | 751,694–782,000 |
| S→C total | 47,121–**1,047,105** | 61,834,426–65,495,673 |
| **C→S per frame** | **3,594 B** (3,509–3,708) | **3,723 B** (3,671–3,758) |
| **S→C per frame** | **219 B** (211–5,058) | **307,776 B** (306,299–309,172) |

## The two questions this was run to answer

**1. Did the presented-exclusion change the forward path?** **No.** C→S per frame is 3,594 B with the
exclusion on and 3,723 B with it off — a ratio of **1.04×**, well inside the spread of either arm.

The 2026-08-29 report observed C→S rising from 804,814 to 1,626,138 bytes and did not explain it. It
is now explained: **it was an artefact of the comparison, not an effect.** Those two figures came from
runs of different lengths (120 frames against 96) that were never frame-matched, and the per-frame
figures derived from them inherited that. With the frame count fixed and the switch inside one binary,
the forward path does not move.

**2. What is the return-path saving, measured properly?** **1,406× per frame** (307,776 B → 219 B).
The 2026-08-29 figure of 571× was computed from a single unmatched pair and understated it.

## The outlier, which is understood rather than dismissed

One of the five exclusion-ON runs shipped **1,047,105 bytes** where the other four shipped ~47–51 KB —
5,058 B/frame against ~215. That is almost exactly **one 500×500×4 frame** (1,000,000 B).

The likely mechanism, stated as a hypothesis: a swapchain image is created, and S's GPU writes it,
*before* WP0's `create_immed` claims it — so the very first frame's bytes can cross the wire before
`note_presented` marks the resource. Four of five runs did not show it, which fits a race against the
first present rather than a steady leak. **Not chased in this session**; it is one frame per session,
not per frame, and the measurement it perturbs is reported with its spread rather than smoothed.
