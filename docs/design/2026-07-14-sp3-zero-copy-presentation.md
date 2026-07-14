# SP3 — Zero-Copy Presentation (dmabuf)

**Date:** 2026-07-14
**Status:** Sub-project design spec (awaiting owner review)
**Parent design:** [`2026-07-13-native-remote-wayland-gpu.md`](2026-07-13-native-remote-wayland-gpu.md)
**Predecessors:** [`2026-07-13-sp0-first-light.md`](2026-07-13-sp0-first-light.md), [`2026-07-14-sp1-onto-the-screen.md`](2026-07-14-sp1-onto-the-screen.md), [`2026-07-14-sp2-real-transport.md`](2026-07-14-sp2-real-transport.md) (all complete, merged)

---

## 1. Purpose and the single success criterion

SP1 put the rendered triangle on screen, but by a wasteful route: the GPU renders the image,
the server **copies it down to CPU memory**, and hands those bytes to the compositor via
`wl_shm` — which **re-uploads them to the GPU** to composite. That GPU→CPU→GPU round-trip is
exactly the "second best" the project exists to avoid.

SP3 removes it. The server keeps the rendered image **on the GPU** and hands the compositor a
**dmabuf** — a kernel handle to that GPU memory — so the compositor samples it directly, with
no copy. This is the S-side efficiency the parent design calls for (§"Anatomy" steps 4–5:
"S-side renderer replays on the real GPU → **S-local dmabuf** … the Wayland proxy attaches the
dmabuf").

SP3's **one new hard thing** is *exporting a Vulkan-rendered image as a dmabuf and presenting
it through `zwp_linux_dmabuf_v1`*, with a robust runtime fallback to SP1's `wl_shm` path. The
client (C) is **untouched** — it still hand-emits the triangle; rendering (SP0) and the QUIC
transport (SP2) are reused.

**Success criterion (measurable + observable):**

1. *Machine-verified:* a new **GPU-only** test renders the triangle, copies it into a
   LINEAR-tiled export image, exports a dmabuf fd, reads that image back, and asserts the
   pixels (centre red, corners blue). It is **skipped cleanly** where the required Vulkan
   external-memory/modifier extensions are absent (so lavapipe/CI stays green). All SP0/SP1/SP2
   tests remain green.
2. *Human-observed (documented manual milestones):*
   - The triangle appears in a window **via dmabuf** on the real compositor (verified, e.g., by
     a log line and/or absence of the `wl_shm` fallback).
   - The **`wl_shm` fallback** still shows the triangle when dmabuf is unavailable (forced via a
     flag or on a non-dmabuf setup).

## 2. Scope — what SP3 is, and is not

SP3 **is**: render on the GPU (as SP0), copy into a LINEAR-tiled **exportable** image, export it
as a dmabuf, and attach it to the `xdg_toplevel` surface via `zwp_linux_dmabuf_v1`; with a
runtime probe that falls back to SP1's `wl_shm` presenter when dmabuf is not available. It
includes the **renderer-lifetime refactor** that makes this possible (§4).

SP3 is deliberately **NOT** (each deferred to a named later sub-project):

- **No negotiated/optimal modifiers** — SP3 uses `DRM_FORMAT_MOD_LINEAR` only (universally
  importable). True no-copy via the GPU's native tiling modifier is a later refinement.
- **No async GPU→compositor sync** — SP3 uses a CPU **fence-wait** before attach (correct, not
  asynchronous). `linux-drm-syncobj-v1` / timeline-semaphore export is deferred.
- **No cross-GPU dmabuf** — assumes S renders and composites on the **same** GPU (the laptop's
  single GPU). Render-on-iGPU / display-on-dGPU is out of scope.
- **No buffer-release recycling / multi-frame buffer pools** — one frame, one buffer, held for
  the window's life.
- **No mapped-memory coherence, no content-addressed assets, no real Vulkan interception** —
  the rest of the original "sibling protocol," and the real-engine pivot, are later arcs (the
  agreed decomposition: SP3 = dmabuf presentation only; the engine pivot is the next arc).

## 3. Architecture: two S-side changes + a lifetime refactor

```
   C side (client)                 S side (server) — the change is entirely here
 ┌──────────────┐   QUIC (SP2)   ┌──────────────────────────────────────────────────────────┐
 │ rayland-     │ ─────────────► │ handle_connection → Renderer (persistent, owns the GPU    │
 │ client       │                │   device + LINEAR export image)                            │
 │ (unchanged)  │                │   render → OPTIMAL image → GPU copy → LINEAR export image   │
 └──────────────┘                │   ── dmabuf available? ──┬── yes ── export fd + layout ──┐ │
                                 │                          │                                │ │
                                 │                          └── no ── wl_shm (SP1 path) ──┐  │ │
                                 │   present via calloop window (SP1/SP2 teardown intact)  ▼  ▼ │
                                 │      zwp_linux_dmabuf_v1  OR  wl_shm SlotPool  → surface     │
                                 └──────────────────────────────────────────────────────────┘
```

The transport, the `handle_connection` state machine, and the calloop window loop with its SP2
`Liveness` teardown are all **reused unchanged**. SP3 changes only *how the rendered image
reaches the surface*.

## 4. The lifetime refactor: renderer becomes a persistent object

This is the structural heart of SP3. SP0/SP1 create the Vulkan instance/device/pipeline,
render, copy the result to CPU, and **destroy every Vulkan object before returning**. That is
fine when the deliverable is a `Vec<u8>` of pixels — but a **dmabuf fd references live GPU
memory**. The exported image and its memory (and the device) **must stay alive for as long as
the compositor holds the buffer** — i.e., the whole window lifetime. Tearing them down after
the render call (as today) would hand the compositor a dangling handle.

So the renderer is refactored from a one-shot free function into a **persistent object**:

- A **`Renderer`** owns the Vulkan instance, device, queue, pipeline, and the **persistent
  LINEAR export image + its exportable memory**. It is created once and lives through the
  presentation.
- It exposes two output methods over the same render:
  - `render_to_frame(&self, request) -> RenderedFrame` — the SP0 CPU-readback path, reused by
    the `wl_shm` fallback, the `--png` path, and the pixel test.
  - `render_to_dmabuf(&mut self, request) -> DmabufFrame` — renders, copies into the persistent
    LINEAR export image, fence-waits, and returns a `DmabufFrame { fd, width, height, drm_format,
    modifier, stride, offset }` whose backing GPU resources are **owned by the `Renderer`** and
    stay alive.
- The existing free function `render_triangle(request) -> RenderedFrame` is retained as a thin
  convenience wrapper (create a temporary `Renderer`, call `render_to_frame`) so **SP0's render
  test and the `--png` path keep working unchanged**.

The window loop holds the `Renderer` (and thus the live export image) for its whole run, and
drops it — releasing the Vulkan resources in the correct order — only after the window is gone
and the compositor has released the buffer.

## 5. The dmabuf export path (Vulkan)

On S's GPU, `render_to_dmabuf`:

1. Renders the triangle into the OPTIMAL color image (SP0's pipeline, unchanged).
2. Creates (once, in the `Renderer`) a **LINEAR-tiled export image** of the frame size:
   - tiling described by `VkImageDrmFormatModifierListCreateInfoEXT` with the single modifier
     `DRM_FORMAT_MOD_LINEAR` (`VK_EXT_image_drm_format_modifier`);
   - memory allocated with `VkExportMemoryAllocateInfo` for the `DMA_BUF` external handle type
     (`VK_KHR_external_memory` + `VK_KHR_external_memory_fd` + `VK_EXT_external_memory_dma_buf`).
3. `vkCmdCopyImage` (or blit) OPTIMAL → LINEAR export image; submits; **`vkWaitForFences`** until
   the copy completes (the fence-wait of §Q1 — the correctness point).
4. Exports the dmabuf fd with `vkGetMemoryFdKHR`, and reads the plane **offset + row stride** via
   `vkGetImageSubresourceLayout` on the LINEAR image.
5. Returns the `DmabufFrame`; the fd is dup'd/owned so the compositor import and the Vulkan
   resource lifetime are decoupled correctly.

**Pitfalls (documented in code):** the export image *must* be LINEAR and created via the
modifier extension (not plain `VK_IMAGE_TILING_LINEAR`) so its modifier is well-defined for the
compositor; the `zwp_linux_dmabuf_v1` `add` call must use the **subresource-layout** offset/stride,
not `width*4` (LINEAR images can still be padded); and the fence-wait must precede both the fd
export and the surface commit.

## 6. The dmabuf presenter (Wayland)

`window.rs` gains a dmabuf presenter alongside the existing `wl_shm` one:

- Bind `zwp_linux_dmabuf_v1` (from `wayland-protocols`; SCTK 0.20 has no first-class helper, so
  it is driven directly with `wayland-client`, beside the existing SCTK compositor/xdg/registry
  plumbing).
- Build a `wl_buffer`: `zwp_linux_dmabuf_v1.create_params()` → `add(fd, plane=0, offset, stride,
  modifier_hi, modifier_lo)` → `create_immed(width, height, drm_format, flags)`.
- **Fence-wait already done** in the renderer (§5.3), so attach immediately: `surface.attach(wl_buffer)`,
  `damage_buffer`, `commit`.
- Opaque `XRGB8888`, fixed-size window at the frame dimensions — same choices as SP1.

## 7. Runtime detection and the `wl_shm` fallback

At startup the server probes two things and picks the path:

1. **GPU capability:** the physical device exposes `VK_KHR_external_memory_fd`,
   `VK_EXT_external_memory_dma_buf`, and `VK_EXT_image_drm_format_modifier`, and can create a
   LINEAR export image of the target format.
2. **Compositor capability:** `zwp_linux_dmabuf_v1` is advertised **and** lists our format
   (`XRGB8888`/`ARGB8888`) with `DRM_FORMAT_MOD_LINEAR`.

If **both** hold → the dmabuf path. Otherwise → **SP1's `wl_shm` presenter, unchanged**
(`render_to_frame` → `pack_xrgb8888` → `SlotPool`). This keeps SP1 working on every setup,
keeps CI/lavapipe (no dmabuf) green, and is honest engineering rather than an assumption. A
`--force-shm` flag (and the existing `--png`) let the fallback be exercised deliberately.

## 8. Teardown (contract unchanged from SP1/SP2)

"Close on either" is preserved verbatim: the calloop loop watches the SP2 `Liveness` fd;
window-close or client-disconnect ends the loop. The only addition is ordered Vulkan teardown:
the `Renderer` (owning the export image/memory/device) is dropped after the window is gone, so
the dmabuf's backing memory outlives the compositor's use of it.

## 9. Testing strategy (light CI)

- **Unchanged & green:** SP0 render pixel test, SP1 `pack_xrgb8888`, SP2 QUIC e2e + transport
  tests — all headless, GPU-or-lavapipe.
- **New GPU-only dmabuf export test:** render → copy to LINEAR export image → `vkGetMemoryFdKHR`
  (assert a valid fd ≥ 0) → read the export image back (honoring the subresource layout) →
  assert the triangle pixels. **Gated:** if the required extensions are not present on the
  active device (e.g., lavapipe), the test **skips cleanly** (returns early with a logged
  reason), so CI passes without a dmabuf-capable GPU.
- **Manual (documented in `docs/sp3-zero-copy-presentation.md`):** (i) run server+client → the
  triangle shows via **dmabuf** on the real compositor (confirmed by a startup log line naming
  the chosen path); (ii) `--force-shm` → the same triangle via the `wl_shm` fallback.

## 10. Error handling and dependencies

- **Binaries** use `anyhow`; the renderer's fallible Vulkan steps surface clear, contextual
  errors. No `unwrap`/`expect` on runtime-fallible paths (`expect` in tests OK; asserts for
  documented caller-bug invariants OK).
- **No new crates:** `ash` already exposes the external-memory/modifier extension entry points;
  `wayland-protocols` (already pulled by SCTK) provides `zwp_linux_dmabuf_v1`; `rustix` (already
  present) covers fd handling. Licenses unchanged (`rayland-server` GPL, libraries LGPL).

## 11. Definition of done

- The capability probe (§13) has run and its result (dmabuf available on this S, or not) is
  recorded.
- `cargo test` passes locally (real GPU, including the new dmabuf test) and in CI (lavapipe, with
  the dmabuf test skipped), plus all inherited SP0/SP1/SP2 tests.
- `cargo clippy --workspace -- -D warnings` clean; `cargo fmt` applied.
- Every function has a doc-block; every non-trivial line a value-adding comment; code and
  comments agree.
- Running server+client shows the triangle **via dmabuf** on S's display; `--force-shm` shows it
  via `wl_shm`; closing the window exits the client and vice-versa (SP1/SP2 teardown intact).
- `docs/sp3-zero-copy-presentation.md` documents both paths and how to tell which is active.

## 12. Refinements to confirm at review

1. **Persistent `Renderer` owning the device + export image** (vs. isolating a separate dmabuf
   renderer and leaving `render_triangle` untouched). Chosen: the persistent object, because the
   dmabuf lifetime *requires* it and it sets up later multi-frame work; `render_triangle` stays
   as a wrapper so SP0's test is undisturbed.
2. **dmabuf-primary with automatic `wl_shm` fallback** (vs. an explicit path flag). Chosen:
   auto-detect (with `--force-shm` as an override), so it "just works" and CI/non-dmabuf setups
   stay green.
3. **LINEAR modifier + CPU fence-wait** (vs. negotiated modifiers / async syncobj). Chosen for
   correctness-first simplicity; both are localized later refinements.

## 13. Assumption to verify first (the SP3 spike)

The plan's **first task** probes the S-side GPU and compositor: does the physical device expose
`VK_KHR_external_memory_fd` + `VK_EXT_external_memory_dma_buf` + `VK_EXT_image_drm_format_modifier`,
and does the compositor advertise `XRGB8888`+`MOD_LINEAR` on `zwp_linux_dmabuf_v1`? On the target
laptop (Intel Iris Xe / Mesa ANV) all three Vulkan extensions and dmabuf import are expected, but
this quick probe de-risks the slice up front (as SP2's crypto spike did). If the GPU lacks them,
SP3 degrades to "the `wl_shm` path plus a documented dmabuf path that this machine can't exercise"
— which the fallback already handles.

Everything else follows the parent design and `CLAUDE.md`.
