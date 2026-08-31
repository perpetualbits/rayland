# The stall is swapchain recreation, triggered by a focus-change configure — 2026-08-31

The join the previous session said was needed, done. Both daemons now stamp their diagnostic lines
with the same `CLOCK_MONOTONIC`, so C's frame timeline and S's link activity line up directly.

## S is exonerated: the relay never stops

During a **2029 ms** stall in which the application presented no frames at all:

| what S did during the gap | count |
|---|---:|
| `RingProgress` sends (the message that releases the application) | **80** |
| largest `RingProgress`-to-`RingProgress` gap **inside** the stall | **43 ms** |
| `BlobData` sends | 672 |
| `SubmitCmd` **received from C** | 88 |
| `RingDelta` received from C | 73 |

The relay ran throughout, and **C was still sending submits** — the application was doing Vulkan work
the whole time. It was not blocked on Rayland, and it was not idle. It simply stopped *presenting*.

## What it was actually doing

Every line C logged inside the stall, from its own timestamps:

```
+   0ms  attach, damage, frame, commit                (the last frame before the stall)
+   2ms  bound global zwp_linux_dmabuf_v1 -> obj 18
+   3ms  advertised 2 LINEAR dmabuf format(s)
+   3ms  destroy obj 18, bind zwp_linux_dmabuf_v1 again
+   4ms  advertised 2 LINEAR dmabuf format(s)
           ← 975 ms with ZERO Wayland traffic in either direction
+ 979ms  create 4 params objects -> 4 new wl_buffers (500x500)
+ 995ms  destroy the 4 OLD wl_buffers
+ 997ms  xdg_surface.ack_configure
+ 997ms  bind zwp_linux_dmabuf_v1 again, advertise formats
           ← 964 ms with ZERO Wayland traffic
+1963ms  create 4 more params -> 4 more wl_buffers (500x500)
+1981ms  destroy the previous 4
+2029ms  attach                                        (the stall ends)
```

**Two complete swapchain recreations, back to back, at an unchanged 500×500.** The two silent windows
carry no Wayland traffic at all — but S logged ~477 link lines in each, including 43 and 42
`SubmitCmd`s. That is `vkCreateSwapchainKHR`/`vkDestroySwapchainKHR` and their image allocations
going over the vtest ring: **~477 round trips at roughly 2 ms each ≈ 970 ms.**

## Why it happens at all

Moving the pointer in or out changes the window's focus, so the compositor sends an
`xdg_toplevel.configure` with changed states. The application acks it and rebuilds its swapchain —
**normal, correct client behaviour**, and a native client does exactly the same. Natively it costs
milliseconds and nobody notices. Through Rayland the same sequence costs **~1 second per recreation**,
because every allocation is a synchronous round trip.

So the stall is not a defect in the sense of something behaving incorrectly. It is **the per-operation
round-trip cost of the relay, made visible by an operation that performs hundreds of them at once.**
That reframes it usefully: the fix for the stall and the fix for the frame rate are the same fix.

## A refuted fix

**Shortening the poll intervals does not help.** `PARK_SLEEP` 500 µs → 50 µs on C and `PROGRESS_POLL`
200 µs → 20 µs on S, two binaries, interleaved A/B with pointer disturbance:

| variant | fps | fps | stall total | stall total |
|---|---:|---:|---:|---:|
| baseline | 12.5 | 14.9 | 7427 ms | 3732 ms |
| fast poll | 11.2 | 11.3 | 4364 ms | 3875 ms |

Frame rate is **no better and probably worse** — plausibly the extra polling costs more CPU than the
latency it saves — and the stall totals are indistinguishable against this much run-to-run variance.
The ~2 ms per round trip is not simply poll latency.

## Where the fix has to come from

The target is now specific: **~2 ms per synchronous round trip on loopback**, and ~477 of them per
swapchain recreation. Reducing that number, or the count, fixes the stall *and* the frame rate
together. Not poll intervals — that has been tried and measured.

Note the measurement discipline this needs: the runs above vary from 12.5 to 14.9 fps and 3.7 s to
7.4 s of stall on the *same* binary, because a human is using the machine. Nothing here should be
concluded from a single run, and the contamination check now in `wp0-soak.sh` exists for exactly this.
