# Where a WP0 frame's time goes — measured 2026-08-30

**Topology: LOOPBACK on dop561** (C, S and the compositor all on one host), headless weston
(`--renderer=gl`, Mesa/Intel, `--idle-time=0`), `vkcube --gpu_number 0`. One `CLOCK_MONOTONIC`, so
S-side and C-side stamps compare directly. **No figure here is a two-machine figure.**

## 1. Round trips per frame — the number that transfers to a real network

From C's own per-channel counters (`RAYLAND_C1_METRICS=1`), 362 presented frames:

| channel | per frame |
|---|---:|
| **S→C replies** (answers to blocking requests) | **4.43** |
| S→C control | 2.00 |
| C→S ring | 3.24 |
| C→S inline (vtest request/reply) | 3.24 |
| **C→S blob sync** | **72.50** |
| S→C blob sync | 9.49 |

**≈4.4 synchronous round trips per frame** (the replies), or ≈6.4 counting control.

What that costs on a real link, as an *n × RTT floor* that no bandwidth saving can remove:

| link | RTT | added per frame | effect on a 65.8 ms frame |
|---|---:|---:|---|
| this LAN | 0.5 ms | 2.2 ms | invisible |
| same city | 5 ms | 22 ms | +33% |
| another country | 30 ms | 132 ms | **3× worse — 5 fps** |
| transatlantic | 80 ms | 352 ms | **fatal — under 2.5 fps** |

**4.4 is a good number.** It is small enough that a LAN is unaffected and a metropolitan link is
usable; it is not small enough for an intercontinental one. If Rayland is to work over a WAN, this is
the number to attack, and it is far more tractable than the 72.5 forward messages, which are
*asynchronous* and cost bandwidth and CPU rather than latency.

## 2. Baselines — measured before attributing anything

`vkcube` natively against the same headless weston, steady state extracted by slope (300 vs 900
frames, so startup is removed; startup measured 0.12 s):

| configuration | ms/frame | fps |
|---|---:|---:|
| native, `--present_mode 0` (IMMEDIATE, no vsync) | **0.49** | 2037 |
| native, `--present_mode 2` (FIFO, vsync — the default) | **25.37** | 39.4 |
| **through WP0, FIFO** | **65.8** | **15.2** |

Three native FIFO runs agreed to within 0.1 fps.

**The ceiling is 39.4 fps, not 60**, and it is *entirely compositor pacing*: the GPU work is
0.49 ms/frame, so 24.9 ms of every native frame is the client waiting for weston's frame callback.
Any comparison against 60 fps would have been wrong.

**On `vkgears` — CORRECTED the same day, and the original claim was wrong.** This section first said
`vkgears` "is a fragile binary" whose numbers should not be used. That is not right, and the root cause
matters:

- `vkgears` segfaults against a **seatless** compositor because mesa-demos 9.0.0 dereferences the
  `wl_seat` global unconditionally (`src/vulkan/wsi/wayland.c:236`) — an upstream bug found by the
  solsim session with a backtrace. **The headless weston launched here advertises no `wl_seat`**
  (S's own log: *"S's compositor advertises no `wl_seat`; bind skipped"*), which is exactly that case.
- **Natively against COSMIC, which has a seat, `vkgears` runs at 60.8–61.1 FPS.** A native baseline
  *is* obtainable; the earlier claim that it is not was wrong.
- Its failure *through Rayland against COSMIC* is a **separate and much more serious defect, and it is
  ours** — see `keymap-drop-crashes-applications.md`.

Every figure in this document is still `vkcube`'s, which is unaffected by both issues.

`--present_mode 0` through WP0 produces **no frames**: *"Present mode specified is not supported"* —
Venus does not expose IMMEDIATE. So pacing cannot be subtracted on the Rayland side the way it can
natively; that is a limitation of this measurement, not a defect.

## 3. The budget

| component | ms/frame | how it was established |
|---|---:|---|
| GPU render + present | **0.49** | native IMMEDIATE |
| Compositor pacing (weston's callback) | **~24.9** | native FIFO minus native IMMEDIATE |
| **Everything Rayland adds** | **~40.4** | WP0 FIFO (65.8) minus native FIFO (25.4) |
| **Total observed** | **65.8** | WP0 FIFO |

So **38% of a WP0 frame is compositor pacing a native client pays too**, and **61% is Rayland's**.

### Inside Rayland's ~40 ms — stage intervals, same clock

Paired by `(res, tail)` / `(res, off)` across both daemons' `RLTRACE` output, 291 frames:

| interval | what it is | n | median | p10 | p90 | max |
|---|---|---:|---:|---:|---:|---:|
| `T5→T7` | S ships a blob write → C has it | 932 | **2.13 ms** | 1.49 | 3.51 | 33.1 |
| `T0→T2` | S receives a ring delta → engine consumes it | 1004 | **3.96 ms** | 2.32 | 13.98 | 141.2 |
| `T2→T8` | S consumed → C is told | 1319 | **7.90 ms** | 5.09 | 14.51 | 149.8 |

Each occurs 3–5 times per frame, and they overlap, so these do **not** sum to 40 ms — they locate it.
The shape matters as much as the medians: p90 is 2–4× the median and the maxima are 30–150 ms, so
this is **not** a uniformly slow pipeline. It is a mostly-fast one with a heavy tail.

**Unaccounted for:** the budget attributes ~40 ms to Rayland but does not yet decompose it fully. The
app's submission and the `wl_buffer` commit have no stations, so the segment from the application's
`vkQueueSubmit` to S's apply, and from the commit to the callback, is not instrumented. That is the
gap the next session should close, and it is stated rather than papered over.

## 4. Suspects ruled OUT

| Suspect | Ruled out by |
|---|---|
| **GPU render time** | 0.49 ms/frame native IMMEDIATE — 0.7% of the frame |
| **The network** | Loopback throughout, and `T5→T7` is still 2.13 ms with no network at all |
| **Bandwidth** | ~3.6 KB/frame of commands; the return path is 219 B/frame since the presented-buffer exclusion |
| **Compositor pacing as the whole story** | It accounts for 25.4 of 65.8 ms; 40 ms remains |
| **Polling granularity alone** | `PARK_SLEEP` 500 µs, `PROGRESS_POLL` 200 µs. Intervals of 4–8 ms are 8–40× those, so either many sync points per interval or real blocking — a bare poll interval does not explain it |

**Still live:** forward blob-sync volume (72.5 msgs/frame, and milkv's 3.7×-slower core gave almost
exactly 3.7× fewer frames — the signature of per-message cost), and whatever produces the heavy tail.
