//! S-side presentation: turn a [`RenderedFrame`] into an on-screen Wayland window.
//!
//! SP0 rendered a triangle on the GPU and read it back into a `RenderedFrame` (tightly
//! packed RGBA8 in CPU memory). SP1's job is purely to *present* that frame: copy it into a
//! shared-memory (`wl_shm`) buffer and show it in a real `xdg_toplevel` window on the
//! compositor the user is sitting in front of. No GPU work changes; this module only adds
//! windowing.
//!
//! The pure conversion [`pack_xrgb8888`] is unit-tested here. The SCTK-driven window
//! ([`run_window`], added later) is integration code verified by building and by a manual
//! on-screen smoke check, because asserting real on-screen output needs a compositor.

// The rendered frame we present; its pixels are tightly-packed RGBA8.
use crate::render::RenderedFrame;

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
    use crate::render::RenderedFrame;

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
