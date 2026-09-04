# Pre-registration — does the 1.23× feedback speedup survive a real network?

**Written before any data was collected.** This project has been flattered by a small sample three
times (1.78× that was a null; 1.28× that was 1.03×; a win at n=3 that vanished at n=11), so the design
is fixed here first and not revisited after looking.

## The question

Venus's semaphore/event/query feedback measured **1.23×** on `icosa-gpu` — median `draw_readback`
48.7 ms → 39.5 ms — **over loopback**. Reliability is now settled (400/400 clean on each arm, Fisher
p = 1.0), so the speedup is the only remaining reason to enable the flags. It has never been measured
on a network, which is where it would be claimed.

**Directional prediction, stated in advance:** feedback removes *synchronous round trips*, so its
benefit should scale with per-trip cost. On a network with higher RTT than loopback the effect should
be **at least** 1.23×, not less. If it comes back at or below ~1.05× the mechanism does not pay here
and the thread closes.

## Design

- **Fixture:** `rayland-icosa-gpu` (the fixture the original 1.23× used).
- **Topology:** C = apollo (`172.16.20.10`), S = dop561 (`192.168.1.192`). Real network.
  milkv is unusable — on VLAN 70, its traffic to S does not arrive at either of S's addresses.
- **Arms:**
  - **A (shipping):** `no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback`
  - **B (feedback on):** `no_multi_ring,no_fence_feedback`
- **Interleaved A,B,A,B,…** — never all of one arm then all of the other. Machine state, thermals and
  link conditions drift, and a block design confounds them with the arm.
- **n = 10 per arm, decided now.** 120 frames each, so 1,200 frames per arm.
- **Profile: release.** Every duration ratio this project has taken on a debug build was wrong.
- **S's GPU pinned to Intel.** The NVIDIA card loses the device silently on ~half its runs.

## The metric, fixed in advance

Per run, the **median `draw_readback_us`** over its 120 frames (the fixture's own timer, column 4).
That gives 10 run-level medians per arm.

- **Point estimate:** median(A run-medians) ÷ median(B run-medians).
- **Significance:** Mann–Whitney U on the two sets of 10, two-sided.
- **Reported regardless of outcome**, including a null.

## What would invalidate the run

- Any arm losing a run to a failure or a setup retry — recorded, not silently dropped.
- Frames differing from the native baseline (the harness bit-compares); a torn run is not a timing
  sample.
- Fewer than 120 frames in any run.
