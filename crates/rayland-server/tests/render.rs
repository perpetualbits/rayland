//! Integration test for the off-screen renderer.
//!
//! Renders one triangle into a 64×64 image and asserts on the pixels directly, so the
//! GPU path is verified by machine. Runs on a real GPU locally and on Mesa lavapipe in CI.

// The renderer under test.
use rayland_server::render::{FrameRequest, render_triangle};
// The vertex type carried by a request.
use rayland_wire::Vertex;

/// Fetch the RGBA of the pixel at (x, y) from a tightly-packed RGBA8 buffer.
fn pixel_at(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    // Each pixel is 4 bytes; compute the byte offset of (x, y).
    let index = ((y * width + x) * 4) as usize;
    // Copy the four channels out.
    [
        pixels[index],
        pixels[index + 1],
        pixels[index + 2],
        pixels[index + 3],
    ]
}

/// Assert two channel values are within a tolerance (software and hardware rasterisers
/// differ by a few least-significant bits at edges).
fn close(actual: u8, expected: u8) -> bool {
    // Absolute difference within 8/255 is "the same colour" for our purposes.
    (actual as i16 - expected as i16).abs() <= 8
}

#[test]
fn triangle_center_is_red_and_corners_are_blue() {
    // A centred red triangle that covers the middle but not the corners of the image.
    let request = FrameRequest {
        width: 64,
        height: 64,
        clear_color: [0.0, 0.0, 1.0, 1.0], // blue background
        vertices: vec![
            Vertex {
                position: [0.0, -0.5],
                color: [1.0, 0.0, 0.0],
            },
            Vertex {
                position: [0.5, 0.5],
                color: [1.0, 0.0, 0.0],
            },
            Vertex {
                position: [-0.5, 0.5],
                color: [1.0, 0.0, 0.0],
            },
        ],
    };

    // Render on the real (or software) GPU.
    let frame = render_triangle(&request).expect("rendering the triangle must succeed");

    // The output must be a tightly-packed 64×64 RGBA8 buffer.
    assert_eq!(frame.pixels.len(), (64 * 64 * 4) as usize);

    // The centre pixel is inside the triangle → red.
    let center = pixel_at(&frame.pixels, 64, 32, 32);
    assert!(
        close(center[0], 255) && close(center[1], 0) && close(center[2], 0),
        "centre should be red, was {center:?}"
    );

    // All four corners are outside the triangle → blue (the clear colour).
    for (x, y) in [(0, 0), (63, 0), (0, 63), (63, 63)] {
        let corner = pixel_at(&frame.pixels, 64, x, y);
        assert!(
            close(corner[0], 0) && close(corner[1], 0) && close(corner[2], 255),
            "corner ({x},{y}) should be blue, was {corner:?}"
        );
    }
}
