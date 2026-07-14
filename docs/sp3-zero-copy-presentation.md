# SP3 — Zero-Copy Presentation (how to run it)

SP3 adds a **zero-copy** presentation path alongside SP1's `wl_shm` one. The client (C) and
the wire protocol are unchanged; the server (S) still renders the triangle on its GPU. What
changes is how the finished pixels reach the compositor:

- **SP1's `wl_shm` path** (unchanged, still the fallback): the server reads the rendered image
  back into ordinary CPU memory (a `Vec<u8>`), copies it into a `wl_shm` shared-memory buffer,
  and hands that buffer to the compositor. Two copies happen: GPU → CPU (the readback) and
  CPU → the shared-memory buffer.
- **SP3's dmabuf path** (new, preferred when available): the server exports the rendered
  image as a **dmabuf** — a Linux kernel handle (file descriptor) to the GPU memory itself —
  and hands the compositor that *handle*, via the `zwp_linux_dmabuf_v1` Wayland protocol
  extension. No pixels are ever copied through CPU memory; the compositor imports and samples
  the same GPU memory the renderer wrote, directly.

Which path actually runs is decided **automatically, at startup**, by probing both the local
GPU/driver and the live compositor connection (see "Auto-detection" below). The startup log
prints exactly one line naming the chosen path.

## How to run it

Terminal A — the server:

    cargo run -p rayland-server            # listens on 127.0.0.1:9000 (QUIC)

Terminal B — the client:

    cargo run -p rayland-client            # connects to 127.0.0.1:9000 over QUIC

A window titled "Rayland — SP3" shows a red triangle on a blue background. The server's
startup log names the active presentation path, e.g.:

    rayland-server listening on 127.0.0.1:9000 (QUIC)
    connection accepted
    presenting in a window; close it (or stop the client) to exit
    presenting via dmabuf (zero-copy)

or, on a setup that cannot use dmabuf:

    presenting via wl_shm (fallback: compositor does not advertise XRGB8888 with DRM_FORMAT_MOD_LINEAR on zwp_linux_dmabuf_v1)

Teardown is unchanged from SP1/SP2: closing the window closes the QUIC connection (the client
exits immediately); stopping the client closes the window (within the QUIC idle timeout, ~5s).

### Forcing the fallback: `--force-shm`

    cargo run -p rayland-server -- --force-shm

Skips the dmabuf auto-detection entirely and always uses the `wl_shm` path, regardless of
what the GPU and compositor support. This exists to let the fallback be exercised
deliberately — e.g. to confirm it still works correctly, or to compare the two paths visually
(they must show pixel-identical output). The startup log will read:

    presenting via wl_shm (fallback: --force-shm was passed)

### Headless / PNG fallback: `--png`

    cargo run -p rayland-server -- --png out.png
    cargo run -p rayland-client

Unchanged from SP0/SP1/SP2: writes the rendered RGBA8 pixels straight to a PNG and exits, with
no Wayland connection at all. `--png` and `--force-shm` are mutually pointless together (the
PNG path never touches Wayland, so there is no `wl_shm`-vs-dmabuf choice to force), but
combining them is harmless — `--force-shm` is simply ignored in that case.

## Auto-detection: how the dmabuf-vs-`wl_shm` decision is made

Two independent facts must both hold before the dmabuf path is used:

1. **The local GPU + Vulkan driver can export a dmabuf** — checked once, at server startup,
   against the actual physical device chosen (`Renderer::supports_dmabuf`, backed by probing
   `VK_KHR_external_memory_fd` + `VK_EXT_external_memory_dma_buf` +
   `VK_EXT_image_drm_format_modifier`). This has nothing to do with Wayland; it is a pure
   Vulkan-driver capability.
2. **The compositor advertises `XRGB8888` with `DRM_FORMAT_MOD_LINEAR`** on
   `zwp_linux_dmabuf_v1` — checked once the server connects to the compositor, by binding the
   protocol at version 3 (the version at which the classic `format`/`modifier` events are
   still sent — see "Known SP3 limitations" below) and doing a protocol roundtrip to collect
   every `(format, modifier)` pair it advertises.

If (1) is false, the server does not even attempt to connect probing (2) meaningfully — the
export could never succeed regardless of what the compositor supports. If (1) is true but (2)
is false — the compositor exists but does not support this exact format+modifier combination
(some compositors only support NVIDIA-style vendor modifiers, or only expose the format
without the LINEAR modifier, or run at an older protocol version) — the server falls back to
`wl_shm`. Either way, the log line explains *why* the fallback happened, so a confusing
"nothing shows up" is never silent.

`--force-shm` short-circuits both checks and always chooses `wl_shm`.

### Why the render step itself is decided this late

Deciding *after* connecting to the compositor means the actual GPU render call
(`Renderer::render_to_frame` vs. `Renderer::render_to_dmabuf`) also has to happen that late —
there would be no point rendering into a dmabuf export if the compositor turns out not to
support importing it, or vice versa. The server therefore reads and validates the incoming
command stream into an unrendered `FrameRequest` first (`read_frame_request`), and only
renders once inside the presentation module, after the capability probe has run.

### CI / lavapipe

A software rasterizer (Mesa's `lvp`/lavapipe, `VK_ICD_FILENAMES`/`VK_LOADER_DRIVERS_SELECT`
pinned to `*lvp*`) may or may not advertise the dmabuf-export Vulkan extensions depending on
the Mesa version — some do (in which case the dmabuf-path GPU tests run and pass under
lavapipe too), some don't (in which case those specific tests print a skip message and pass
trivially). Either way, CI has no live Wayland compositor at all, so the real on-screen
`wl_shm`-vs-dmabuf choice is never exercised there — only the underlying Vulkan export
mechanics are (see the dmabuf test in `crates/rayland-server/src/dmabuf.rs` and the
`Renderer::render_to_dmabuf` test in `crates/rayland-server/src/render.rs`). No CI
configuration change was needed for SP3: `wayland-protocols` is pure Rust, so no new system
library is required to build.

## The `VK_QUEUE_FAMILY_FOREIGN_EXT` release barrier

Handing a dmabuf to the compositor is handing GPU memory to a consumer this process's Vulkan
device cannot see as a Vulkan queue at all (the compositor's own driver instance, or a
fixed-function scanout path). The Vulkan spec's mechanism for this is a queue-family
*ownership transfer*: a barrier releasing the image to the special
`VK_QUEUE_FAMILY_FOREIGN_EXT` pseudo-queue-family (via the `VK_EXT_queue_family_foreign`
extension), and switching it to the one layout the spec guarantees is valid for every kind of
access, `VK_IMAGE_LAYOUT_GENERAL`.

`Renderer::prepare_export_for_foreign_present` performs exactly this release, and only on the
actual presentation path (`window::present`) — never on the GPU-gated dmabuf tests. This
split exists because a queue-family *release* without a matching *acquire* makes the image
invalid to read again from the **same** device, which is precisely what those tests do (they
read the exported image back through Vulkan, on the same device, to assert its pixel colours
are correct). Folding the foreign-release barrier into the shared export path would have
broken that same-device readback; keeping it as a separate, presentation-only step keeps both
correct: the tests read back on the releasing device (no foreign transfer), and the real
presenter releases to the compositor (no same-device readback afterward).

This barrier is treated as **best-effort**: if `VK_EXT_queue_family_foreign` is not available
(probed and enabled independently of the three core dmabuf-export extensions), or the barrier
submission itself fails, the server logs a warning and presents anyway rather than aborting.
Several real Mesa/ANV-class compositor combinations tolerate the barrier's absence entirely
for LINEAR-modifier dmabufs under **implicit synchronization** (where the kernel's own
DRM/DMA-BUF fence — not an explicit Vulkan semaphore — is what actually orders GPU access), so
skipping it does not necessarily produce a visibly wrong result; it is the textually-correct
thing per spec, done as a courtesy, not a strict on-screen precondition on every setup.

## Known SP3 limitations

These are deliberate scope cuts (see the [SP3 design
spec](design/2026-07-14-sp3-zero-copy-presentation.md), §2), not oversights:

- **`DRM_FORMAT_MOD_LINEAR` only.** The export image always uses the trivial linear/row-major
  tiling, never a vendor-specific compaction modifier the compositor might prefer for display
  performance (e.g. Intel's `I915_FORMAT_MOD_X_TILED`). This is what makes the export portable
  across any dmabuf-capable driver with a single hard-coded format, at some cost in display
  efficiency versus a negotiated modifier. Modifier negotiation (matching what the compositor
  actually prefers, potentially per-output) is a deferred refinement.
- **The `format`/`modifier` events, not `zwp_linux_dmabuf_feedback_v1`.** SP3 binds
  `zwp_linux_dmabuf_v1` at protocol version 3 specifically so the classic, synchronous
  `format`/`modifier` events fire — from version 4 onward, compositors stop sending them and
  expect clients to use the heavier `get_default_feedback`/`get_surface_feedback` mechanism
  (a memory-mapped format table plus per-output "tranches"), which is out of scope for this
  sub-project. A compositor that has fully dropped support for version ≤3 semantics (rare, but
  possible on a very strict future compositor) would show as "compositor does not advertise
  zwp_linux_dmabuf_v1 v3+" and fall back to `wl_shm`.
- **CPU fence-wait, not async.** Every render step — the triangle draw, the export blit, and
  the presentation-path foreign-ownership release — blocks the host thread on a Vulkan fence
  until the GPU finishes, rather than using semaphores to let the CPU and GPU overlap. This is
  the same synchronization approach SP0/SP1/SP2 always used; correctness-first, simple to
  reason about, and adequate for a single static frame per process. A pipelined,
  multi-frame renderer would need real semaphore-based synchronization instead.
- **Single GPU, no negotiation of which device to use.** `Renderer::new` picks the first
  Vulkan physical device exposing a graphics queue; multi-GPU systems (e.g. a laptop with both
  an integrated and a discrete GPU) get whichever one enumerates first, not necessarily the
  one actually driving the display the window ends up on.
- **One buffer, no recycling.** SP3 presents exactly one static frame per process and never
  reuses or double-buffers a `wl_buffer`; the `wl_buffer::Event::release` event (which would
  signal "safe to reuse this buffer") is received and explicitly ignored. A renderer producing
  a live stream of frames would need real buffer recycling and (for the dmabuf path) probably
  multiple in-flight export images.
- **No modifier negotiation, no multi-plane formats.** Only single-plane `XRGB8888` is
  supported; YUV or other multi-plane pixel formats (relevant to a future video/media path)
  are unhandled.

These are the seams the **next arc — the real-engine pivot** (replacing the SP0-era hand-rolled
triangle renderer with the reused Venus/virglrenderer command-stream replay engine, per
`CLAUDE.md`'s locked architecture decision) will need to revisit, since a real application's
command stream implies genuine multi-frame, multi-buffer presentation rather than SP0–SP3's
single static image.

## Manual verification (the human operator's milestone)

The automated test suite (`cargo test --workspace`, GPU and lavapipe) covers the Vulkan export
mechanics and the CPU-readback path, but **cannot** exercise the actual on-screen result — that
needs a real compositor and a person looking at the screen. Two runs to check:

1. **Zero-copy path:** `cargo run -p rayland-server` then `cargo run -p rayland-client` on a
   machine with a real GPU (dmabuf-capable driver) and a compositor that supports
   `zwp_linux_dmabuf_v1` v3+ with `XRGB8888`+`DRM_FORMAT_MOD_LINEAR` (e.g. current Mesa
   ANV/RADV under a wlroots-based or GNOME/KDE Wayland session). Confirm: the startup log
   prints `presenting via dmabuf (zero-copy)`, and the window shows the same red-triangle
   on-blue image SP0/SP1 always produced.
2. **Fallback path:** `cargo run -p rayland-server -- --force-shm` then
   `cargo run -p rayland-client`. Confirm: the log prints
   `presenting via wl_shm (fallback: --force-shm was passed)`, and the window shows the
   **pixel-identical** triangle via the CPU round-trip path.

In both cases, confirm teardown still works: closing the window ends the client immediately;
Ctrl-C-ing the client closes the window within a few seconds.
