# SP3 — Zero-Copy Presentation (dmabuf) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace SP1's GPU→CPU-readback→`wl_shm` round-trip with a dmabuf handed to the compositor, keeping the rendered image on the GPU — with an automatic runtime fallback to the `wl_shm` path when dmabuf is unavailable.

**Architecture:** Entirely S-side. A capability spike first proves the Vulkan dmabuf-export chain works on this GPU and pins the `ash` API. `render.rs` is refactored into a persistent `Renderer` that owns the device and a LINEAR export image (a dmabuf fd references live GPU memory, so it must outlive the render call). `render_to_dmabuf` renders → blits OPTIMAL→LINEAR export image → fence-waits → exports an fd + layout. `window.rs` gains a `zwp_linux_dmabuf_v1` presenter beside the existing `wl_shm` one; the server auto-detects dmabuf support and falls back to `wl_shm` otherwise.

**Tech Stack:** Rust edition 2024; `ash` 0.38 Vulkan external-memory extensions (`VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf`, `VK_EXT_image_drm_format_modifier`); `wayland-protocols` `zwp_linux_dmabuf_v1` (already pulled by SCTK); existing SCTK/calloop window + SP2 transport unchanged.

## Global Constraints

Copied from the spec and `CLAUDE.md`; every task implicitly includes these.

- **Edition:** `edition = "2024"`, `rust-version = "1.85"`.
- **Comments:** doc-block on every fn/type/module; intent comment on every **non-trivial** line (the *why* / domain meaning), never restating syntax; trivial lines get none; code and comments must always agree. This is `unsafe` Vulkan — comment the *safety/domain reasoning*, not the syntax.
- **Errors:** binary (`rayland-server`) uses `anyhow` with contextual messages; no `unwrap`/`expect` on runtime-fallible non-test paths (asserts for documented caller-bug invariants OK; `expect` in tests OK). License `GPL-3.0-or-later` unchanged.
- **Pixel format:** the dmabuf uses DRM fourcc **`XRGB8888`** (little-endian `0x00RRGGBB` → memory `B,G,R,X`), which is Vulkan **`B8G8R8A8_UNORM`**. SP0's OPTIMAL render image stays **`R8G8B8A8_UNORM`**; the OPTIMAL→LINEAR step is a **`vkCmdBlitImage`** (component-semantic: R→R,G→G,B→B), which writes the export image's memory as `B,G,R,A` = correct XRGB8888. (A `vkCmdCopyImage` would byte-copy and NOT swizzle — must be a blit.) This keeps SP1's `XRGB8888` choice and leaves SP0's render + its test untouched.
- **Sync:** CPU **`vkWaitForFences`** after the blit, before exporting the fd and before `surface.commit` — the correctness point (compositor never samples a half-drawn image).
- **Layout:** the `zwp_linux_dmabuf_v1` `add` call uses the **`vkGetImageSubresourceLayout`** offset + rowPitch of the LINEAR export image, never `width*4` (LINEAR images may be padded).
- **Fallback is mandatory:** if the GPU lacks the extensions or the compositor doesn't advertise `XRGB8888`+`MOD_LINEAR`, use SP1's `wl_shm` presenter unchanged. `--force-shm` selects it deliberately. CI/lavapipe (no dmabuf) must stay green.
- **Verify against cargo/GPU, not the IDE.** rust-analyzer lags mid-edit. This host filters software Vulkan globally: for the real GPU run bare; to force lavapipe use `VK_LOADER_DRIVERS_SELECT='*lvp*'`. Vulkan dmabuf export works ONLY on the real GPU — the dmabuf test must skip on lavapipe.
- **ash API is authoritative from the compiler + the Task 1 spike.** Where this plan names an `ash` type/method (e.g. `ash::khr::external_memory_fd::Device::get_memory_fd`), verify the exact path/signature against what compiles; Task 1 pins them and later tasks reuse Task 1's confirmed forms.

---

## File Structure

- `crates/rayland-server/src/dmabuf.rs` — **new.** The Vulkan dmabuf export mechanics (create a LINEAR export image via the modifier extension, blit into it, export an fd, read the subresource layout), the `DmabufFrame` type, and the `dmabuf_available()` capability probe. Task 1 builds and GPU-tests this standalone; Task 3 wires it into the `Renderer`.
- `crates/rayland-server/src/render.rs` — **modify (Task 2).** Refactor the one-shot `render_triangle_inner` into a persistent `Renderer` owning the device + pipeline + OPTIMAL image; `render_to_frame` method (SP0 readback path); `render_triangle` free fn becomes a thin wrapper. Task 3 adds the persistent LINEAR export image + `render_to_dmabuf`.
- `crates/rayland-server/src/window.rs` — **modify (Task 4).** Add a `zwp_linux_dmabuf_v1` presenter path beside the `wl_shm` `SlotPool` path; choose at runtime.
- `crates/rayland-server/src/main.rs` — **modify (Task 4).** Probe dmabuf availability (unless `--force-shm`/`--png`), pick the presentation path, log which.
- `crates/rayland-server/src/lib.rs` — **modify.** `pub mod dmabuf;`.
- `crates/rayland-server/Cargo.toml` — **modify (Task 4).** Add `wayland-protocols` (with the `unstable`/`staging` feature that exposes `zwp_linux_dmabuf_v1`) and `wayland-client` if not already direct deps (both already transitive via SCTK; add as direct deps to use the protocol types).
- `docs/sp3-zero-copy-presentation.md` — **new (Task 4).**

---

## Task 1: dmabuf export mechanics spike + capability probe (DE-RISK FIRST)

Prove, on the real GPU, that Vulkan can create a LINEAR `B8G8R8A8_UNORM` export image via `VK_EXT_image_drm_format_modifier`, that we can render/fill it and export a dmabuf fd, and read it back correctly — pinning the exact `ash` API. This is standalone (its own minimal device + a known fill), independent of the triangle renderer, so it isolates the export mechanics. Its confirmed code + API are reused by Task 3.

**Files:**
- Create: `crates/rayland-server/src/dmabuf.rs`
- Modify: `crates/rayland-server/src/lib.rs` (add `pub mod dmabuf;`)

**Interfaces:**
- Produces (used by Tasks 3/4):
  - `pub struct DmabufFrame { pub fd: std::os::fd::OwnedFd, pub width: u32, pub height: u32, pub drm_format: u32, pub modifier: u64, pub offset: u32, pub stride: u32 }`
  - `pub fn device_supports_dmabuf_export(instance: &ash::Instance, physical_device: ash::vk::PhysicalDevice) -> bool` — true iff the three extensions are present.
  - `pub const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;` (fourcc `'XR24'`), `pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;`
  - The reusable export helper (exact name finalized here; Task 3 calls it): given a `&ash::Device`, the physical-device memory properties, a source OPTIMAL `VkImage` (R8G8B8A8_UNORM) + its extent + the graphics queue + command pool, create-or-reuse a LINEAR B8G8R8A8 export image, blit, fence-wait, and return a `DmabufFrame`.

- [ ] **Step 1: Declare the module**

In `crates/rayland-server/src/lib.rs`, after `pub mod window;`, add:
```rust
// The GPU dmabuf export path (SP3): render on the GPU, hand the compositor the image by
// dmabuf handle instead of copying it through CPU memory.
pub mod dmabuf;
```

- [ ] **Step 2: Write the capability probe + constants + `DmabufFrame`**

Create `crates/rayland-server/src/dmabuf.rs` starting with the types and the extension probe. Enable the needed device extensions when the device is created (Task 2's `Renderer` will enable them; the spike's own device enables them too):

```rust
//! Zero-copy presentation (SP3): export a Vulkan-rendered image as a Linux **dmabuf** so the
//! compositor samples GPU memory directly, with no CPU round-trip.
//!
//! A dmabuf is a kernel handle (a file descriptor) to a buffer of GPU memory. We render into a
//! LINEAR-tiled image whose memory is allocated *exportable*, export an fd with
//! `vkGetMemoryFdKHR`, and hand that fd (plus the pixel layout) to the compositor via
//! `zwp_linux_dmabuf_v1`. The image and its memory must stay alive as long as the compositor
//! holds the buffer — that lifetime is owned by [`crate::render::Renderer`] (added in Task 3).
//!
//! Pitfalls (see the code): the export image must be created with the *modifier* extension so
//! its tiling is well-defined for the compositor; the OPTIMAL→LINEAR step must be a
//! `vkCmdBlitImage` (component-semantic) not a copy, so R/B channel order lands as XRGB8888;
//! and the dmabuf layout must come from `vkGetImageSubresourceLayout`, not `width*4`.

// ash Vulkan bindings.
use ash::vk;

/// DRM fourcc for `XRGB8888` ('XR24'): a little-endian 0x00RRGGBB word (memory B,G,R,X). The
/// matching Vulkan format is `B8G8R8A8_UNORM`; the compositor must advertise this fourcc.
pub const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
/// The trivial "linear, row-major, no vendor tiling" DRM modifier — universally importable.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// A rendered frame exported as a dmabuf: the fd plus everything the compositor needs to
/// interpret the memory. The fd owns a dup of the exported handle; the *backing GPU memory* is
/// owned separately (by the `Renderer`) and outlives this struct's fd.
pub struct DmabufFrame {
    /// The exported dmabuf file descriptor.
    pub fd: std::os::fd::OwnedFd,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// DRM fourcc describing the pixel format (`DRM_FORMAT_XRGB8888`).
    pub drm_format: u32,
    /// DRM format modifier describing the tiling (`DRM_FORMAT_MOD_LINEAR`).
    pub modifier: u64,
    /// Byte offset of plane 0 within the buffer (from `vkGetImageSubresourceLayout`).
    pub offset: u32,
    /// Row stride in bytes of plane 0 (from `vkGetImageSubresourceLayout`; may exceed width*4).
    pub stride: u32,
}

/// The device extensions the dmabuf export path requires.
///
/// Returned as `&CStr` names so the `Renderer` can enable them and the probe can check them.
pub fn required_device_extensions() -> [&'static std::ffi::CStr; 3] {
    [
        ash::khr::external_memory_fd::NAME,
        ash::ext::external_memory_dma_buf::NAME,
        ash::ext::image_drm_format_modifier::NAME,
    ]
}

/// Return true iff `physical_device` exposes all [`required_device_extensions`].
///
/// This is the GPU half of the SP3 capability probe (the compositor half lives in `window.rs`).
/// A device missing any of these cannot export a dmabuf, so the server falls back to `wl_shm`.
pub fn device_supports_dmabuf_export(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> bool {
    // Enumerate the device's supported extensions once.
    let props = match unsafe { instance.enumerate_device_extension_properties(physical_device) } {
        Ok(p) => p,
        // If we cannot even query, treat dmabuf as unavailable (the fallback handles it).
        Err(_) => return false,
    };
    // Collect the supported extension names for membership testing.
    let have: std::collections::HashSet<&std::ffi::CStr> = props
        .iter()
        // Each `extension_name` is a fixed-size NUL-padded C string; parse it back to a CStr.
        .filter_map(|e| e.extension_name_as_c_str().ok())
        .collect();
    // Every required extension must be present.
    required_device_extensions().iter().all(|ext| have.contains(ext))
}
```

- [ ] **Step 3: Write the export helper (the risky core) + a GPU-gated test**

Add the export helper and a `#[cfg(test)]` test that stands up a minimal Vulkan device, creates a LINEAR export image, fills it with a known colour (a `vkCmdClearColorImage` to a distinctive value), exports an fd, reads the memory back through the export image's mapped memory (LINEAR + a HOST_VISIBLE export is not guaranteed — instead read back via `vkCmdCopyImageToBuffer` from the export image into a HOST_VISIBLE buffer), and asserts the colour and a valid fd. **This step is where you discover and pin the exact `ash` external-memory API** — the code below is the intended shape; adjust names/signatures to what compiles and works on the GPU, and record every deviation.

Intended export shape (adapt against the compiler + GPU):

```rust
/// Create a LINEAR `B8G8R8A8_UNORM` export image of `extent`, blit `src` (an OPTIMAL
/// `R8G8B8A8_UNORM` image already rendered) into it, wait for completion, and export a dmabuf.
///
/// Returns the `DmabufFrame` **and** the Vulkan image+memory handles the caller must keep alive
/// until the compositor releases the buffer (Task 3 stores them in the `Renderer`).
///
/// # Safety / lifetime
/// The returned `image`/`memory` back the dmabuf fd; destroying them while the compositor holds
/// the buffer would dangle. The caller owns their lifetime.
///
/// # Errors
/// Returns an error if any Vulkan step (image/memory creation, blit submit, fence wait, fd
/// export, layout query) fails.
pub unsafe fn export_as_dmabuf(
    device: &ash::Device,
    external_memory_fd: &ash::khr::external_memory_fd::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    src_optimal_rgba: vk::Image,
    extent: vk::Extent2D,
) -> anyhow::Result<(DmabufFrame, vk::Image, vk::DeviceMemory)> {
    // (1) Create a LINEAR B8G8R8A8_UNORM image whose tiling is the explicit modifier list
    //     [DRM_FORMAT_MOD_LINEAR], and whose memory will be exportable as a dmabuf.
    //     - vk::ImageDrmFormatModifierListCreateInfoEXT { drm_format_modifiers: &[MOD_LINEAR] }
    //     - vk::ExternalMemoryImageCreateInfo { handle_types: DMA_BUF_EXT }
    //     - ImageCreateInfo { format: B8G8R8A8_UNORM, tiling: DRM_FORMAT_MODIFIER_EXT,
    //       usage: TRANSFER_DST (blit dst) | TRANSFER_SRC (readback in the test),
    //       ...chain the two structs above via .push_next }
    // (2) Allocate memory with vk::ExportMemoryAllocateInfo { handle_types: DMA_BUF_EXT },
    //     device-local, bind it.
    // (3) Record: transition export image UNDEFINED→TRANSFER_DST; vkCmdBlitImage(src → export,
    //     one mip, whole extent, filter NEAREST); transition export→? ; submit; wait fence.
    // (4) Export the fd: external_memory_fd.get_memory_fd(&vk::MemoryGetFdInfoKHR {
    //       memory, handle_type: DMA_BUF_EXT }). Wrap the raw fd in OwnedFd.
    // (5) Query layout: device.get_image_subresource_layout(export_image,
    //       vk::ImageSubresource { aspect_mask: COLOR, mip_level:0, array_layer:0 })
    //       → SubresourceLayout { offset, row_pitch, .. }.
    // (6) Build DmabufFrame { fd, width, height, drm_format: DRM_FORMAT_XRGB8888,
    //       modifier: DRM_FORMAT_MOD_LINEAR, offset: layout.offset as u32, stride: layout.row_pitch as u32 }.
    unimplemented_placeholder_do_not_ship!()
}
```

> **Implementer note (not a placeholder in the deliverable):** the `unimplemented_placeholder_do_not_ship!()` marker above is a signal that YOU write this body against the real `ash`/GPU in this task — it is the spike's whole point. Replace it with the working implementation following the numbered comments; do not leave any macro/placeholder in the committed code. If a step is impossible on this GPU (e.g. blit-dst unsupported for the LINEAR modifier), STOP and report BLOCKED with the exact Vulkan error — that is a real finding about ANV support that changes the design.

The GPU-gated test:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmabuf_export_round_trips_a_known_colour() {
        // Stand up a minimal Vulkan instance/device that ENABLES required_device_extensions().
        // If no physical device supports them (e.g. running under lavapipe), skip cleanly.
        let Some(ctx) = minimal_dmabuf_context() else {
            eprintln!("skipping: no device supports dmabuf export (e.g. lavapipe)");
            return;
        };
        // Render/clear a source OPTIMAL R8G8B8A8 image to a known colour, export it, read back
        // the export image via a HOST_VISIBLE buffer, and assert the colour + a valid fd (>= 0).
        // ... (implementer writes the concrete body during the spike) ...
        assert!(ctx.exported_fd_raw >= 0, "exported dmabuf fd must be valid");
        assert_eq!(ctx.readback_bgra_pixel, [0, 0, 255, 0], "red XRGB8888 = B,G,R,X = 0,0,255,0");
    }
}
```

- [ ] **Step 4: Run the spike on the real GPU and record the outcome**

Run: `cargo test -p rayland-server --lib dmabuf::tests::dmabuf_export_round_trips_a_known_colour -- --nocapture`
Expected (real GPU): PASS — a LINEAR export image is created, filled, exported as a valid fd, and reads back the known colour.
Run under lavapipe: `VK_LOADER_DRIVERS_SELECT='*lvp*' cargo test -p rayland-server --lib dmabuf::tests -- --nocapture` → the test **skips cleanly** (prints the skip line, does not fail).

**Decision gate / record in the report:** which exact `ash` types/methods were used (the real names for `ImageDrmFormatModifierListCreateInfoEXT`, `ExternalMemoryImageCreateInfo`, `ExportMemoryAllocateInfo`, `external_memory_fd::Device::get_memory_fd`, `get_image_subresource_layout`); whether ANV supports LINEAR blit-dst for `B8G8R8A8_UNORM`; and any deviation from the shapes above. Tasks 3/4 rely on this.

- [ ] **Step 5: Lints, full suite, commit**

Run: `cargo clippy -p rayland-server -- -D warnings`, `cargo fmt`. Clean.
Run: `cargo test --workspace` (real GPU) and `VK_LOADER_DRIVERS_SELECT='*lvp*' cargo test --workspace` — all green (the dmabuf test passes on GPU, skips on lavapipe; everything else unchanged).

```bash
git add crates/rayland-server/src/dmabuf.rs crates/rayland-server/src/lib.rs
git commit -m "SP3 Task 1: dmabuf export mechanics spike + capability probe (GPU-gated test)"
```

---

## Task 2: refactor `render.rs` into a persistent `Renderer`

Restructure the one-shot renderer into a persistent object that owns the Vulkan device + pipeline + OPTIMAL image, exposing `render_to_frame` (SP0's readback path as a method). No dmabuf yet — this is a behavior-preserving refactor that keeps SP0's pixel test green. It makes Task 3's persistent export image possible.

**Files:**
- Modify: `crates/rayland-server/src/render.rs`

**Interfaces:**
- Produces: `pub struct Renderer` with `pub fn new() -> anyhow::Result<Renderer>` (creates instance/device/queue/pipeline/OPTIMAL image + command pool, enabling `dmabuf::required_device_extensions()` when the device supports them), and `pub fn render_to_frame(&mut self, request: &FrameRequest) -> anyhow::Result<RenderedFrame>`.
- Keeps: `pub fn render_triangle(request: &FrameRequest) -> anyhow::Result<RenderedFrame>` as a thin wrapper (`Renderer::new()?.render_to_frame(request)`), so the existing SP0 test (`tests`/`render.rs`) and the `--png` path are unchanged.

- [ ] **Step 1: Extract the persistent state into `Renderer`**

Refactor `render_triangle_inner` so the long-lived objects — `instance`, `device`, `queue`, `physical_device`, memory properties, render pass, pipeline + layout, shader modules (or destroy after pipeline creation), the OPTIMAL color image + view + framebuffer, and the command pool — become fields of a `Renderer` struct created in `Renderer::new()`. The per-render work (vertex buffer upload, command recording, submit, fence wait, copy-to-readback-buffer, read pixels) moves into `render_to_frame`. Implement `Drop for Renderer` to destroy the owned Vulkan objects in reverse creation order (device last). Keep every existing intent comment; add comments explaining the new ownership.

(The plan does not re-print all 550 lines; the refactor is mechanical — move creations into `new`, per-frame work into `render_to_frame`, destructions into `Drop`. The pixel-readback logic, the subpass dependency, the `offset_of!` vertex offsets, and the u64 readback-size arithmetic are all preserved exactly.)

- [ ] **Step 2: Add the `render_triangle` wrapper**

```rust
/// Render one frame and read it back to CPU memory (SP0/SP1/`--png` path).
///
/// A convenience wrapper that builds a throwaway [`Renderer`] for a single frame, so callers
/// that do not need a persistent renderer (the pixel test, the PNG dump, the `wl_shm` fallback)
/// keep working unchanged.
///
/// # Errors
/// Returns an error if renderer creation or rendering fails.
pub fn render_triangle(request: &FrameRequest) -> anyhow::Result<RenderedFrame> {
    // One-shot: create a renderer, render, drop it.
    Renderer::new()?.render_to_frame(request)
}
```

- [ ] **Step 3: Verify the refactor preserved behavior**

Run: `cargo test -p rayland-server --test render` (the SP0 pixel test) on GPU AND `VK_LOADER_DRIVERS_SELECT='*lvp*' cargo test -p rayland-server --test render` — both PASS unchanged (centre red, corners blue).
Run: `cargo test --workspace` (GPU + lavapipe) — all green.
Run: `cargo clippy --workspace -- -D warnings`, `cargo fmt --check` — clean.

- [ ] **Step 4: Commit**

```bash
git add crates/rayland-server/src/render.rs
git commit -m "SP3 Task 2: refactor render.rs into a persistent Renderer (render_triangle wrapper kept)"
```

---

## Task 3: `render_to_dmabuf` on the `Renderer` + gated export-correctness test

Give the `Renderer` a persistent LINEAR export image and a `render_to_dmabuf` method that renders the triangle and returns a `DmabufFrame`, reusing Task 1's confirmed export mechanics. Add a GPU-only test asserting the exported image's pixels.

**Files:**
- Modify: `crates/rayland-server/src/render.rs`, `crates/rayland-server/src/dmabuf.rs`

**Interfaces:**
- Consumes: `dmabuf::export_as_dmabuf` (or whatever Task 1 named it), `dmabuf::DmabufFrame`, `dmabuf::device_supports_dmabuf_export`.
- Produces: `Renderer::render_to_dmabuf(&mut self, request: &FrameRequest) -> anyhow::Result<dmabuf::DmabufFrame>`; `Renderer::supports_dmabuf(&self) -> bool`.

- [ ] **Step 1: Enable the extensions + own the export image**

Ensure `Renderer::new` enables `dmabuf::required_device_extensions()` when `device_supports_dmabuf_export` is true (store a `supports_dmabuf: bool`), and creates the `ash::khr::external_memory_fd::Device` loader. The persistent LINEAR export image + its exportable memory become `Option<...>` fields created lazily on first `render_to_dmabuf` (sized to the frame; SP3 is single-size) and destroyed in `Drop` **before** the device.

- [ ] **Step 2: Implement `render_to_dmabuf`**

```rust
/// Render one frame and export it as a dmabuf (SP3 zero-copy path).
///
/// Renders the triangle into the OPTIMAL image (same pipeline as `render_to_frame`), then blits
/// it into the persistent LINEAR export image and exports a dmabuf fd + layout. The export
/// image and its memory are owned by `self` and stay alive until the `Renderer` is dropped —
/// which must be after the compositor has released the buffer.
///
/// # Errors
/// Returns an error if the device does not support dmabuf export, or any Vulkan step fails.
pub fn render_to_dmabuf(&mut self, request: &FrameRequest) -> anyhow::Result<dmabuf::DmabufFrame> {
    // Refuse if the device can't export (the caller should have checked supports_dmabuf()).
    anyhow::ensure!(self.supports_dmabuf, "device does not support dmabuf export");
    // Render the triangle into the OPTIMAL colour image (reuse the shared record path).
    // ... record + submit the render (as render_to_frame does, minus the readback copy) ...
    // Create-or-reuse the persistent LINEAR export image, then export via Task 1's helper.
    // (unsafe block; reuse dmabuf::export_as_dmabuf with self's device/queue/pool/optimal image)
    // Store the returned (image, memory) in self so they outlive this call; return the frame.
}
```

- [ ] **Step 3: GPU-gated export-correctness test**

Add a test (in `dmabuf.rs` or `render.rs` tests) that: creates a `Renderer`; if `!renderer.supports_dmabuf()` skips cleanly; else calls `render_to_dmabuf` for the standard triangle, then reads the export image back (via a HOST_VISIBLE buffer copy, honoring the subresource layout stride) and asserts centre red / corner blue in XRGB8888 order, and `frame.fd` is valid, `frame.drm_format == DRM_FORMAT_XRGB8888`, `frame.modifier == DRM_FORMAT_MOD_LINEAR`.

- [ ] **Step 4: Verify (GPU + lavapipe) + commit**

Run: `cargo test -p rayland-server` on GPU (the new dmabuf render test PASSES) and under lavapipe (it SKIPS cleanly). `cargo test --workspace` green both ways. `cargo clippy --workspace -- -D warnings`, `cargo fmt --check` clean.

```bash
git add crates/rayland-server/src/render.rs crates/rayland-server/src/dmabuf.rs
git commit -m "SP3 Task 3: Renderer::render_to_dmabuf + persistent export image + gated GPU test"
```

---

## Task 4: dmabuf presenter + auto-detect + `wl_shm` fallback + docs

Add a `zwp_linux_dmabuf_v1` presenter to `window.rs`, make the server auto-detect dmabuf (GPU + compositor) and fall back to `wl_shm`, add `--force-shm`, and document it.

**Files:**
- Modify: `crates/rayland-server/src/window.rs`, `crates/rayland-server/src/main.rs`, `crates/rayland-server/Cargo.toml`
- Create: `docs/sp3-zero-copy-presentation.md`

**Interfaces:**
- Consumes: `Renderer` (Task 2/3), `DmabufFrame`, the SP2 `Liveness`/`run_window` machinery.
- Produces: a presentation entry point that, given a `Renderer` + `Liveness` + a `force_shm: bool`, runs the window using dmabuf when available else `wl_shm`.

- [ ] **Step 1: Add the protocol deps**

In `crates/rayland-server/Cargo.toml` `[dependencies]`, add (both already transitive via SCTK — declare them to use the types directly):
```toml
wayland-client = { workspace = true }                                   # raw Wayland client for zwp_linux_dmabuf
wayland-protocols = { version = "0.32", features = ["client", "staging", "unstable"] }  # zwp_linux_dmabuf_v1
```
Add `wayland-protocols` to `[workspace.dependencies]` too. (`wayland-client` is already a workspace dep from SP1.)

- [ ] **Step 2: The dmabuf presenter in `window.rs`**

Extend `RaylandWindow` so it can present via either path. Add a dmabuf branch: bind `zwp_linux_dmabuf_v1` from the registry; in `draw()`, when a `DmabufFrame` is present, build the `wl_buffer` — `dmabuf.create_params()` → `params.add(frame.fd, 0, frame.offset, frame.stride, (frame.modifier >> 32) as u32, frame.modifier as u32)` → `params.create_immed(width, height, frame.drm_format, Flags::empty())` → attach + damage + commit. The fence-wait already happened in `render_to_dmabuf`, so attach immediately. The `wl_shm` branch is SP1's existing `draw()` unchanged. Keep the `Renderer` (and thus the live export image) owned by the window state for the whole loop.

(Handler/registry plumbing: `zwp_linux_dmabuf_v1` needs a `Dispatch` impl on the state for its events — `format`/`modifier` advertisements and the params `created`/`failed`. Use `create_immed` to avoid the async created/failed roundtrip. Verify the exact `wayland-protocols` paths against the compiler.)

- [ ] **Step 3: Auto-detect + fallback in `main.rs`**

The server: create the `Renderer`; if `--png` → `render_to_frame` + save (unchanged). Else build/`accept` (SP2), `handle_connection`. Then choose the path: if `!force_shm && renderer.supports_dmabuf()` **and** the compositor advertises `XRGB8888`+`MOD_LINEAR` (discovered during window setup — if not, the presenter reports it and falls back), present via dmabuf (`render_to_dmabuf`); else present via `wl_shm` (`render_to_frame` → SP1 path). Log which path: `presenting via dmabuf (zero-copy)` or `presenting via wl_shm (fallback)`. Add `--force-shm` to the arg parser (alongside `--png`).

(Because the compositor-capability check needs a live Wayland connection, the cleanest structure is: the presenter attempts dmabuf when the GPU supports it and the compositor advertised the format, and returns/logs a clear fallback if the compositor didn't — the server passes the `Renderer` and lets the window module pick. Decide the exact split during implementation; document it.)

- [ ] **Step 4: Verify (GPU + lavapipe) + the automated checks**

Run: `cargo build --workspace`; `cargo clippy --workspace -- -D warnings`; `cargo fmt --check` — clean.
Run: `cargo test --workspace` (GPU) and under lavapipe — all green (window code compiled, not executed by tests; dmabuf test passes on GPU / skips on lavapipe).
Run the `--png` fallback check: server `--png <scratch>/sp3.png` + client → valid red-triangle/blue PNG (unchanged from SP2).
**Skip** the on-screen dmabuf + `--force-shm` window runs — those are the human's manual milestone (no display here). Note the deferral in the report.

- [ ] **Step 5: Docs**

Create `docs/sp3-zero-copy-presentation.md`: what SP3 does (dmabuf zero-copy vs the SP1 wl_shm round-trip); how to run it (server+client → triangle via dmabuf; the startup log line tells you which path is active); `--force-shm` to exercise the fallback; `--png` still works; the note that dmabuf needs a real GPU+compositor (CI/lavapipe uses the fallback/skip); and the "Known SP3 limitations" (LINEAR only, CPU fence-wait not async, single-GPU, one buffer) with pointers to the deferred refinements and the next arc (the real-engine pivot).

- [ ] **Step 6: Commit**

```bash
git add crates/rayland-server/src/window.rs crates/rayland-server/src/main.rs crates/rayland-server/Cargo.toml Cargo.toml docs/sp3-zero-copy-presentation.md
git commit -m "SP3 Task 4: zwp_linux_dmabuf presenter + auto-detect + wl_shm fallback + docs"
```

---

## Self-Review

**1. Spec coverage** — every SP3 spec section maps to a task:
- §1 success criterion (GPU-gated dmabuf test; manual dmabuf + fallback display) → Task 1/3 tests, Task 4 manual.
- §2 scope / non-goals (LINEAR, fence-wait, single-GPU, no recycling) → respected; documented in Task 4 doc.
- §4 persistent Renderer refactor → Task 2 (+ Task 3 export image).
- §5 Vulkan export path (LINEAR modifier image, exportable memory, blit swizzle, fd, layout) → Task 1 (mechanics) + Task 3 (integration).
- §6 dmabuf presenter (zwp_linux_dmabuf_v1) → Task 4.
- §7 auto-detect + wl_shm fallback + --force-shm → Task 4 (GPU probe: Task 1 `device_supports_dmabuf_export`).
- §8 teardown intact (SP2 Liveness) → Task 4 reuses run_window.
- §9 testing (gated GPU test, unchanged SP0/1/2, manual) → Tasks 1/3/4.
- §11 definition of done, §13 spike-first → Task 1.

**2. Placeholder scan** — the only intentional "write-it-here" markers are in Task 1 Step 3 (the spike body the implementer writes against the real GPU, with an explicit instruction to leave no macro/placeholder in the committed code) — this is a de-risking spike, not laziness, matching how SP2's crypto spike was structured. No other placeholders; every non-spike step names concrete files, calls, and test assertions.

**3. Type consistency** — `DmabufFrame { fd, width, height, drm_format, modifier, offset, stride }` defined in Task 1, consumed in Task 3 (`render_to_dmabuf`) and Task 4 (presenter `params.add`/`create_immed`). `Renderer` defined Task 2, extended Task 3 (`render_to_dmabuf`/`supports_dmabuf`), used Task 4. `render_triangle` wrapper (Task 2) keeps SP0's test + `--png` intact. `DRM_FORMAT_XRGB8888` (B8G8R8A8_UNORM) consistent across dmabuf.rs and the presenter.

**Note for the executor:** CI needs no change — the dmabuf test skips without the extensions (lavapipe), and `wayland-protocols` is pure Rust (already in the tree via SCTK), so no new system-lib. Confirm with `cargo tree` after Task 4. The real dmabuf display is the human's manual milestone on the GPU+compositor.
