# The pixel stream that should not have been there — measured, 2026-08-29

Same workload both times: vkcube on apollo, 500×500, presented on dop561 through WP0,
`RAYLAND_C1_METRICS=1` on C, ~60 s per run. Counters are C's own per-channel framed-byte totals.

| | Before | After |
|---|---:|---:|
| Frames (wl_buffers built) | ~120 | 96 |
| C→S total (commands) | 804,814 | 1,626,138 |
| **S→C total** | **105,254,034** | **184,311** |
| S→C blob sync | 105,222,306 | 126,212 |
| **S→C per frame** | **~877 KB** | **~1.9 KB** |

A 500×500×4 frame is 1,000,000 bytes. The before figure is a **frame-sized payload per frame**: S was
shipping every rendered frame back to C, a machine with no display, where nothing consumed it.

**Why it happened.** The (c)2 return path ships back whatever S's GPU wrote into any blob. That is
right for a *readback* — an application that maps a GPU-written buffer and reads it — and it cannot
distinguish that from a *swapchain image*, which the application only ever shows. Only the WP0 token
path knows which is which, so only it can say.

**The fix.** When WP0 builds a real `wl_buffer` from a resource, S marks it presented and stops
shipping its bytes. Narrow by construction: an offscreen fixture never populates that set, so the
(c)2 readback path is untouched.

**Why nobody noticed.** The display was already correct and genuinely zero-copy — S's compositor
imports S's own dma-buf. The bytes were pure waste with no visible symptom. It took the repository
owner asking "we are cheating now, if I understand correctly; because pixels are now crossing the
wire, true?" to prompt the measurement.
