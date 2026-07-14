//! The Rayland server (library half).
//!
//! This crate's job in SP0 is to take a stream of [`rayland_wire::Message`] commands and
//! replay them on a real GPU, producing pixels. The GPU work lives in [`render`]; the
//! stream-handling that drives it is [`handle_connection`]. Keeping this logic in a
//! library (rather than only in `main.rs`) is what lets the end-to-end test in Task 7
//! exercise it without going through a real TCP socket.

// The off-screen Vulkan renderer (Task 4).
pub mod render;

// The S-side presentation path: convert a rendered frame to a wl_shm buffer and
// show it in a live Wayland window (SP1).
pub mod window;

// The wire messages and framed reader.
use rayland_wire::{Message, PROTOCOL_VERSION, read_message};
// The renderer and its request/result types.
use render::{FrameRequest, RenderedFrame, render_triangle};

/// The largest render target dimension the server will accept, in pixels per side.
///
/// The client's requested width and height are untrusted (CLAUDE.md's threat model treats
/// C as an untrusted party driving the host GPU). This ceiling rejects an absurd
/// `BeginFrame` with a clear error *before* it reaches the driver, rather than relying on
/// the GPU's own `maxImageDimension2D` to surface an opaque failure. 16384 is the common
/// hardware limit and is vastly more than SP0's walking-skeleton triangle needs.
const MAX_DIMENSION: u32 = 16384;

/// Read a full SP0 command stream from `reader`, replay it, and return the rendered frame.
///
/// The handler is deliberately permissive about ordering: it accumulates state as messages
/// arrive (a `Hello` checks the protocol version, `BeginFrame` records the target size and
/// clear colour, `UploadVertices` records the geometry, `DrawTriangles` validates the draw
/// count against the uploaded vertices) and only actually renders when `EndFrame` arrives.
/// Messages may appear in any order and later ones of the same kind overwrite earlier ones;
/// SP0 does not police a strict message sequence. What it *does* reject is a stream that is
/// internally inconsistent or incomplete (see Errors).
///
/// # Errors
/// Returns an error on: a protocol-version mismatch (`Hello`); a `DrawTriangles` count that
/// disagrees with the number of uploaded vertices; an `EndFrame` reached without a valid
/// `BeginFrame` (zero width or height) or without any uploaded vertices; a stream that ends
/// before `EndFrame` (early end of stream, surfaced by [`read_message`]); or a failure in
/// the GPU render itself.
pub fn handle_connection<R: std::io::Read>(reader: &mut R) -> anyhow::Result<RenderedFrame> {
    // Frame parameters, filled in by BeginFrame.
    let mut width = 0u32;
    let mut height = 0u32;
    let mut clear_color = [0.0f32; 4];
    // The vertices, filled in by UploadVertices.
    let mut vertices = Vec::new();

    // Read and dispatch messages until EndFrame returns the rendered result.
    loop {
        // Read the next framed message; a stream that ends before EndFrame is an error.
        let message = read_message(reader)?;
        match message {
            Message::Hello { version } => {
                // Refuse to proceed if the client speaks a different protocol version.
                anyhow::ensure!(
                    version == PROTOCOL_VERSION,
                    "protocol version mismatch: client {version}, server {PROTOCOL_VERSION}"
                );
            }
            Message::BeginFrame {
                width: w,
                height: h,
                clear_color: c,
            } => {
                // Record the target size and background colour.
                width = w;
                height = h;
                clear_color = c;
            }
            Message::UploadVertices { vertices: v } => {
                // Store the geometry for the draw.
                vertices = v;
            }
            Message::DrawTriangles { vertex_count } => {
                // SP0 draws exactly the uploaded vertices; guard the invariant.
                anyhow::ensure!(
                    vertex_count as usize == vertices.len(),
                    "DrawTriangles count {vertex_count} != uploaded vertex count {}",
                    vertices.len()
                );
            }
            Message::EndFrame => {
                // Guard against a truncated or malformed stream reaching the GPU: an
                // EndFrame with no valid BeginFrame leaves width/height at zero, and
                // Vulkan rejects a zero-extent image (VkImageCreateInfo::extent must be
                // > 0), yielding an opaque driver error instead of a clear one.
                anyhow::ensure!(
                    width > 0 && height > 0,
                    "EndFrame before a valid BeginFrame (target is {width}x{height})"
                );
                // Reject an absurdly large target with a clear error rather than letting an
                // untrusted dimension drive GPU allocation (see MAX_DIMENSION).
                anyhow::ensure!(
                    width <= MAX_DIMENSION && height <= MAX_DIMENSION,
                    "requested target {width}x{height} exceeds the {MAX_DIMENSION}px limit"
                );
                // Likewise a zero-length vertex buffer is invalid (VkBufferCreateInfo::size
                // must be > 0) and there would be nothing to draw regardless.
                anyhow::ensure!(!vertices.is_empty(), "EndFrame with no uploaded vertices");
                // Everything is gathered and validated; render and return.
                let request = FrameRequest {
                    width,
                    height,
                    clear_color,
                    vertices,
                };
                return render_triangle(&request);
            }
        }
    }
}
