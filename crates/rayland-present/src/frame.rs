//! The two shapes a presentable frame can take, and the one pure conversion between a frame and
//! what Wayland wants.
//!
//! Presentation consumes a frame in exactly one of two forms, and this module defines both:
//!
//! - [`RenderedFrame`] — **CPU pixels**, tightly-packed RGBA8. This is what the `wl_shm` path
//!   copies into a shared-memory buffer.
//! - [`DmabufFrame`] — a **kernel handle to GPU memory** plus the layout needed to interpret it.
//!   This is what the zero-copy `zwp_linux_dmabuf_v1` path hands the compositor.
//!
//! Both are deliberately *descriptions*, not producers: nothing here knows or cares whether the
//! pixels came from a local Vulkan renderer (`rayland-server`), from a readback buffer whose real
//! memory lives on a GPU a network away ((c)1's `rayland-s`, spec §7.1), or from a test's
//! `vec![]`. That is the whole reason this crate can exist without linking a GPU stack — see the
//! crate docs.

// Wrap the exported raw fd in an owning handle so it is closed exactly once, on drop.
use std::os::fd::OwnedFd;

/// A finished frame as **CPU-side pixels**: a tightly-packed RGBA8 image.
///
/// Moved here from `rayland-server`'s `render.rs` by (c)1 Task 7 (which is why the name still says
/// "rendered"): it is the input to [`pack_xrgb8888`] and therefore to the whole `wl_shm`
/// presentation path, so it has to live on the presentation side of the split rather than the GPU
/// side. `rayland_server::render` re-exports it, so every SP-era caller still names it where it
/// always did.
///
/// # The invariant every consumer relies on
/// `pixels.len()` must equal `width * height * 4`, and the bytes must be **RGBA8** in memory order
/// (red, green, blue, alpha) with no per-row padding. [`pack_xrgb8888`] asserts the length and
/// silently assumes the channel order — a frame whose pixels are actually BGRA will present with
/// red and blue swapped and no error anywhere. That is the pitfall this type's whole doc block
/// exists to name.
pub struct RenderedFrame {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes of RGBA8, row-major, no padding.
    pub pixels: Vec<u8>,
}

/// DRM fourcc for `XRGB8888` ('XR24'): a little-endian 0x00RRGGBB word (memory B,G,R,X). The
/// matching Vulkan format is `B8G8R8A8_UNORM`; the compositor must advertise this fourcc.
///
/// Lives here rather than with the Vulkan export code because it is what the *compositor* is told
/// (in [`DmabufFrame::drm_format`] and in the capability probe). `rayland_server::dmabuf`
/// re-exports it and keeps the matching `EXPORT_VK_FORMAT` next to its own export code, since that
/// half is what the *GPU* is told.
pub const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;

/// The trivial "linear, row-major, no vendor tiling" DRM modifier — universally importable.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// A finished frame exported as a **dmabuf**: the fd plus everything the compositor needs to
/// interpret the memory.
///
/// The fd owns a dup of the exported handle; the *backing GPU memory* is owned separately (by
/// whatever produced the export — for `rayland-server`, its `Renderer`) and must outlive this
/// struct's fd. See [`crate::present`]'s doc comment for the lifetime contract that enforces this.
///
/// Moved here from `rayland-server`'s `dmabuf.rs` by (c)1 Task 7. Only the *description* moved:
/// the `ash` code that produces one (`export_as_dmabuf` and the extension checks around it) stayed
/// behind with the renderer, because this crate must not link a GPU stack.
pub struct DmabufFrame {
    /// The exported dmabuf file descriptor.
    pub fd: OwnedFd,
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

/// Copy a rendered frame's pixels into a `wl_shm` `Xrgb8888` buffer.
///
/// `RenderedFrame::pixels` is tightly-packed **RGBA8**: the four bytes of each pixel are, in
/// memory order, red, green, blue, alpha. A `wl_shm` `Xrgb8888` buffer stores each pixel as
/// a 32-bit value `0x00RRGGBB` interpreted **little-endian**, so its four bytes in memory
/// order are blue, green, red, unused. This function performs that reordering — the classic
/// channel-swizzle pitfall of feeding GPU pixels to a Wayland buffer. Getting it wrong makes
/// the red triangle appear blue on screen.
///
/// # Inputs
/// - `frame`: the source; `frame.pixels.len()` must equal `frame.width * frame.height * 4`
///   (which `render_triangle` guarantees).
/// - `dst`: the destination buffer, which must be exactly the same length as `frame.pixels`
///   (i.e. `width * height * 4`). The `wl_shm` pool row stride for a 32-bit format is
///   `width * 4`, which is already tight, so no per-row padding arises.
///
/// # Panics
/// Panics (via `assert_eq!`) if `dst` is not exactly `frame.pixels.len()` bytes — a caller
/// bug, since the caller sizes the buffer from the same dimensions.
pub fn pack_xrgb8888(frame: &RenderedFrame, dst: &mut [u8]) {
    // The destination must match the source exactly; a mismatch is a caller error, not a
    // recoverable runtime condition, so we assert rather than return a Result.
    assert_eq!(
        dst.len(),
        frame.pixels.len(),
        "destination buffer must be width*height*4 bytes"
    );
    // Walk source and destination four bytes (one pixel) at a time in lockstep.
    for (rgba, out) in frame.pixels.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        // Read the source channels by their documented RGBA positions.
        let r = rgba[0] as u32;
        let g = rgba[1] as u32;
        let b = rgba[2] as u32;
        // Assemble the 32-bit 0x00RRGGBB word; the unused top byte stays 0 (opaque window).
        let word = (r << 16) | (g << 8) | b;
        // Writing the word little-endian lays the bytes out as B, G, R, 0 — exactly the
        // Xrgb8888 memory order the compositor expects.
        out.copy_from_slice(&word.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_xrgb8888_swizzles_rgba_into_little_endian_xrgb() {
        // Two known pixels: pure red then pure green, both fully opaque in the source.
        let frame = RenderedFrame {
            width: 2,
            height: 1,
            pixels: vec![
                255, 0, 0, 255, // red   (R,G,B,A)
                0, 255, 0, 255, // green (R,G,B,A)
            ],
        };
        // Destination sized exactly width*height*4.
        let mut dst = vec![0u8; frame.pixels.len()];
        pack_xrgb8888(&frame, &mut dst);
        // Red -> Xrgb8888 little-endian bytes B,G,R,X = 0,0,255,0.
        // Green -> 0,255,0,0.
        assert_eq!(dst, vec![0, 0, 255, 0, 0, 255, 0, 0]);
    }
}
