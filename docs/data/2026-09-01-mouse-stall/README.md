# The ~1 s stall on mouse entry and exit — not a Rayland defect

## The question

The owner, watching vkcube run from milkv on dop561's real desktop: *"why does it still stall for over
a second at mouse entry and exit?"*

## The answer, in one line

**`vkcube` recreates its entire Vulkan swapchain on every `xdg_toplevel.configure`, COSMIC sends one on
every focus change, focus follows the pointer — and a swapchain recreation that costs ~1 ms locally
costs ~1 s across the relay.** It is an application behaviour that is free on one machine and expensive
on two. Rayland is not doing anything wrong; it is amplifying something that was already there.

## Evidence 1 — the relayed run (90 s, owner moving the mouse in and out)

| | count |
|---|---|
| `wl_pointer.enter` / `.leave` | 96 |
| `xdg_toplevel.configure` | 96 |
| ...of which carried **`[500,500]`** | **95** (the one other is the initial `[0,0]`) |
| `create_immed` (swapchain buffers) | 388 = **97 recreations** |

One configure per pointer crossing, one full swapchain recreation per configure, **and the size never
changes.** The eight stalls caught in a separate 7-minute run were 0.8–3.1 s each; the log shows two
distinct shapes — a silent gap with no Wayland traffic at all (the Vulkan teardown/setup), and a burst
of `zwp_linux_dmabuf_v1` destroy → `create_params` → `add` → `create_immed` (the four buffers being
rebuilt).

## Evidence 2 — native `vkcube` on the same compositor does exactly the same thing

Run natively on dop561 with `WAYLAND_DEBUG=1`, 60 s, same mousing:

| | count |
|---|---|
| `xdg_toplevel.configure` | **188** |
| `create_immed` | **752 = 188 recreations** — 1:1, same as relayed |
| `wl_pointer.enter` / `.leave` | 88 / 87 |
| frames (`wl_surface.attach`) | **2,403 (~40 fps)** |

And the arguments say precisely what is changing:

```
     64  (1437, 1384, array[0])     <- deactivated
     65  (1437, 1384, array[4])     <- activated  (one state, 4 bytes)
     21  (500, 500, array[0])
     21  (500, 500, array[4])
```

**Identical width and height; only the `states` array toggles.** The compositor is reporting a focus
change, not a resize, and the application rebuilds its swapchain anyway.

Natively that is invisible: 188 rebuilds and it still ran at 40 fps, because a local swapchain
recreation is on the order of a millisecond.

## Why it costs ~1 s over the relay

A swapchain recreation is a burst of **hundreds of synchronous Vulkan round trips** — destroy four
images, tear down and rebuild the swapchain, allocate and export four new dma-bufs, rebuild the
per-image command buffers. Each synchronous call is a full C→S→execute→reply→C cycle. At the measured
~11 ms per round trip on this board, a few hundred of them is the second the owner sees.

## What follows from this

- **Not a bug to fix in the relay.** Nothing here is being done incorrectly; there is no lost event, no
  spurious configure of our own making, no protocol error.
- **It is the strongest possible argument for the round-trip work**, and it puts a user-visible number
  on it: everything that reduces per-round-trip cost shortens this stall proportionally.
- **A mitigation exists and was deliberately NOT taken.** The proxy could withhold an
  `xdg_toplevel.configure` whose width and height match the previous one, delivering only the
  `xdg_surface.configure` the app must ack. That would very likely remove the stall for `vkcube` — and
  it would be the proxy deciding that an activation change is not worth telling the application about,
  which is false for any app that renders focus (a title bar, a caret, a hover state). Rayland is a
  transparent proxy; silently dropping protocol events to make a benchmark look better is exactly the
  kind of thing that would be discovered later as a bug. Recorded as an option, not applied.
- The honest framing for a user: **this is `vkcube` being naive, made visible by the network.** A
  well-behaved application that checks whether the size actually changed before recreating would not
  stall at all.

## Reproducing

```
scripts/milkv-demo.sh                    # relayed, on the live session; move the mouse in and out
WAYLAND_DEBUG=1 vkcube --gpu_number 0    # native, same compositor, same mousing
grep -c 'create_immed' <log>             # divide by 4 for swapchain recreations
```
