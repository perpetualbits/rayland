# WP0 — Wayland proxy first light: a real Vulkan app presenting on S via buffer-by-token

**Status:** design/spec, 2026-07-22. First sub-project of **arc B — the Wayland proxy** (the design's
long-term presentation model, [`2026-07-13-native-remote-wayland-gpu.md`](2026-07-13-native-remote-wayland-gpu.md)
§185–189). Motivated by the vkcube finding (`docs/DIARY.md`, 2026-07-21): a real WSI app's swapchain
`wl_buffer` is an invalid virtio-gpu dma-buf because Rayland has no virtio-gpu display path.

## 1. What WP0 is (and is not)

WP0 is the **walking skeleton** of the Wayland proxy: the thinnest end-to-end path that puts **vkcube's
spinning cube on S's screen** through the proxy, driving the *risky core* — buffer-by-token — first,
exactly as SP0 drove the whole render loop before hardening any of it.

**In scope:** one Vulkan app (vkcube), one `xdg_toplevel` surface, the minimal Wayland interface set
vkcube binds, buffer-by-token wired to the swapchain images the relay already renders on S, and enough
present coordination to show moving frames. **Ugly is fine; end-to-end is the point.**

**Explicitly out of scope** (later WP sub-projects): input forwarding (WP2), sync/timing polish —
`presentation-time`, `drm-syncobj`, frame pacing (WP3), the Wayland interface long tail, subsurfaces,
damage, multiple surfaces, robust teardown and error paths (WP1). WP0 may hard-code, stub, or ignore
anything vkcube does not exercise.

## 2. The one invariant everything rests on

**The app's swapchain `wl_buffer` on C must denote the same pixels as the S-side resource the command
relay rendered into.** Nothing about the frame's *pixels* crosses the network for presentation: the frame
is rendered on S (the existing forward path already does this) and displayed on S. Only the **Wayland
protocol** and **buffer tokens** cross. Keeping that identity consistent is the whole problem; everything
else is plumbing that waypipe and Sommelier prove is tractable.

## 3. Architecture

Two new roles on the two existing processes, plus a new message channel on the existing QUIC link.

- **C-side Wayland proxy** (in or beside `rayland-c`). The application connects here — `WAYLAND_DISPLAY`
  names the proxy's socket, not the real compositor. The proxy is a Wayland **server** to the app: it
  advertises the minimal globals, dispatches the app's requests, forwards them to S, and — the one
  special case — intercepts buffer creation to produce a **token** instead of relaying a dma-buf.
- **S-side Wayland client** (in or beside `rayland-s`). It connects to S's **real** compositor as a
  Wayland **client** and replays the forwarded session: binds the same globals, creates the surface and
  `xdg_toplevel`, resolves each buffer token to an S-side `wl_buffer`, attaches, and commits.
- **Transport.** Wayland requests/events cross the **existing QUIC link** as new `rayland-relay` message
  types (opaque Wayland wire bytes plus the small typed side-band the proxy adds for tokens). No second
  connection in WP0. The vtest/ring relay is unchanged and runs alongside it.

## 4. Buffer-by-token — the crux, mechanism-grounded

The mechanism is pinned from Mesa's Venus WSI (`vn_wsi.c` + Mesa's common `wsi_common_wayland`):

- `vkCreateSwapchainKHR` makes N swapchain images, each a **Venus resource** with dedicated memory,
  exported as a **dma-buf**. That dma-buf fd is the Venus memory resource's exported fd — **the same
  resource the vtest relay already tracks** (it crossed as a `CreateBlob`, and S holds the S-side copy).
- `wsi_common_wayland` wraps that dma-buf as a `wl_buffer` via `zwp_linux_dmabuf.create_immed`, passing
  the fd + format + modifier over the app's Wayland socket. This is the request the C proxy intercepts.

The flow WP0 builds:

1. **C-side correlate → token.** On `create_immed`, the proxy does **not** forward the fd or any pixels.
   It correlates the passed dma-buf fd to a **Venus resource id** the relay already knows (fd identity —
   the two fds name the same underlying resource; confirmed by the Task-1 spike, §7). It emits a token
   (the resource id + the format/modifier/size the compositor needs) to S and hands the app back a proxy
   `wl_buffer` object standing in for it.
2. **S-side resolve → real `wl_buffer`.** S maps the token to the S-side resource, **re-exports** it as a
   dma-buf (`rayland-s` already does this for presentation — `virgl_renderer_resource_export_blob`), and
   creates a `wl_buffer` from it via S's compositor's `zwp_linux_dmabuf`.
3. **Render.** The app's draws into the swapchain image are relayed by the existing forward path and
   executed on S's GPU **into that same resource**, so when S attaches it, it holds the finished frame.
4. **Present.** The app's `wl_surface.attach(buffer)` + `commit` (driven by `vkQueuePresentKHR` inside
   Mesa's WSI) forward to S, which attaches the resolved `wl_buffer` and commits to S's compositor —
   gated on the frame's completion so the compositor never composites a half-rendered image (WP0 reuses
   the existing return-path completion signal; precise fence/`drm-syncobj` integration is WP3).

## 5. Why this is the right shape

- **No pixel path for presentation** — pixels are rendered on S and shown on S; the network carries
  protocol and tokens only. This is the design doc's buffer-by-token, not waypipe's copy-the-buffer.
- **No Mesa fork** — the proxy sits *above* Mesa, intercepting the app's Wayland wire protocol, exactly
  as the ring-watching insight sat beside Mesa without patching it.
- **Reuses the proven pipeline** — the command relay renders the swapchain image on S; `rayland-s`
  already exports resources as dma-bufs and drives a Wayland surface (`rayland-present`). WP0 adds the
  proxy transport and the token identity, not a new renderer.

## 6. Scope of Wayland coverage (WP0 minimum)

Only what vkcube binds and uses: `wl_display`/`wl_registry` (globals), `wl_compositor` (surface),
`xdg_wm_base` + `xdg_surface` + `xdg_toplevel` (window), `zwp_linux_dmabuf_v1` (the buffer path),
`wl_callback` (frame throttling — may be stubbed to fire promptly in WP0), and `wl_seat` **advertised but
inert** (vkcube binds it; input is WP2). Anything vkcube does not touch is not implemented.

## 7. Plan shape (spike first)

- **Task 1 — mechanism spike (gate).** Instrument the path to confirm the §4 correlation *before* building
  the proxy: run vkcube through the vtest relay with the C side also observing the app's Wayland socket,
  and verify (a) the `create_immed` dma-buf fd matches a resource the relay tracked, (b) S can re-export
  that resource as a dma-buf, and (c) the resource actually receives the app's render on S. If any leg
  fails, revise §4 before writing plumbing. This is the load-bearing assumption; it is proven first.
- **Task 2 — the transport:** the `rayland-relay` Wayland message channel and its framing (Wayland wire
  bytes + the token side-band), with C↔S round-tripping a trivial exchange.
- **Task 3 — the C proxy:** advertise the §6 globals, dispatch and forward vkcube's requests, intercept
  `create_immed` into a token.
- **Task 4 — the S client:** replay the session against S's compositor; resolve tokens to `wl_buffer`s;
  surface + `xdg_toplevel` + attach + commit.
- **Task 5 — present coordination:** wire `vkQueuePresentKHR`/commit to S's commit, gated on frame
  completion; a window with the moving cube appears on S.

## 8. Success criteria

- **The proof:** `vkcube` launched on C against the proxy shows its **spinning cube in a window on S's
  compositor**, animating, for many frames, with no crash, no `create_immed` assert, no wedge. On the
  two-machine setup (apollo → dop561) the window is on S (dop561) and vkcube runs on C (apollo).
- **The pixels are S-rendered, not forwarded:** verified structurally (no readback/pixel BlobData on the
  presentation path — only tokens and Wayland protocol cross) and by the frame being present-correct on S.
- **Regression:** the existing offscreen fixtures (`icosa_cpu`, `refapp`) and their e2e are untouched and
  still pass — WP0 adds a parallel Wayland channel, it does not change the vtest/ring relay.

## 9. Risks and unknowns (beyond the Task-1 gate)

- **fd-identity correlation robustness.** If two swapchain images or a resize mint fds the correlation
  cannot disambiguate, the token map breaks. The spike measures the real fd/resource relationship; the
  design may need the relay's own resource metadata rather than raw fd identity.
- **Modifier/format agreement.** S's compositor must accept the resource's dma-buf format+modifier. Venus
  already negotiates modifiers the host supports (`vn_wsi.c` §143 notes LINEAR for wlroots/KWin); WP0
  confirms S's compositor accepts what S re-exports, and falls back to LINEAR if needed.
- **Present timing.** WP0 gates the commit on the existing completion signal; if that is too coarse the
  cube may tear or stutter. Correctness (no torn frame) is a WP0 bar; smoothness is WP3.
- **`wayland-rs` vs hand-rolled.** WP0 leans on `wayland-rs` for the server (to the app) and client (to
  S's compositor) halves, to reach first light fast; the dependency and any minimal-proxy alternative are
  revisited in WP1, not litigated here.
