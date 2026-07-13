//! The Rayland server (library half).
//!
//! This crate's job in SP0 is to take a stream of [`rayland_wire::Message`] commands and
//! replay them on a real GPU, producing pixels. The GPU work lives in [`render`]; the
//! stream-handling that drives it is [`handle_connection`]. Keeping this logic in a
//! library (rather than only in `main.rs`) is what lets the end-to-end test in Task 7
//! exercise it without going through a real TCP socket.

// The off-screen Vulkan renderer (Task 4).
pub mod render;

// The wire messages and framed reader.
use rayland_wire::{Message, PROTOCOL_VERSION, read_message};
// The renderer and its request/result types.
use render::{FrameRequest, RenderedFrame, render_triangle};

/// Read a full SP0 command stream from `reader`, replay it, and return the rendered frame.
///
/// Processes messages in order: verifies the `Hello` version, accumulates the frame
/// parameters and vertices, and renders when `EndFrame` arrives. Any message arriving out
/// of the expected order, or a version mismatch, is an error.
///
/// # Errors
/// Returns an error on a protocol-version mismatch, a malformed/out-of-order stream, an
/// early end of stream, or a rendering failure.
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
                // Everything is gathered; render and return.
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
