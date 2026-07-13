//! The SP0 command messages and the geometry they carry.

// serde's derive macros generate the (de)serialization code for our types.
use serde::{Deserialize, Serialize};

/// The protocol version the server and client must agree on.
///
/// SP0 is pre-1.0 throwaway protocol, so this is simply `0`. The server rejects any
/// `Hello` whose version it does not recognise, which is how future incompatible
/// changes will be caught instead of silently misinterpreting bytes.
pub const PROTOCOL_VERSION: u32 = 0;

/// One vertex of the triangle: a 2-D position and an RGB colour.
///
/// Positions are in Vulkan normalised device coordinates (each axis roughly -1.0..=1.0).
/// This is the *data* that crosses the wire in SP0 — the humble ancestor of the
/// mapped-memory and asset-residence machinery that arrives in SP3.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Vertex {
    /// Position in normalised device coordinates: `[x, y]`.
    pub position: [f32; 2],
    /// Linear RGB colour in `0.0..=1.0`: `[r, g, b]`.
    pub color: [f32; 3],
}

/// A single command from client to server.
///
/// The client sends these in order — `Hello`, `BeginFrame`, `UploadVertices`,
/// `DrawTriangles`, `EndFrame` — and the server replays them against a real GPU. Each
/// variant is documented with the server-side effect it triggers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// Handshake. The server checks `version` against [`PROTOCOL_VERSION`] and refuses
    /// to proceed if they differ.
    Hello {
        /// The protocol version the client speaks.
        version: u32,
    },
    /// Begin a frame: allocate an off-screen render target of `width`×`height` pixels
    /// and clear it to `clear_color` (RGBA, each channel `0.0..=1.0`).
    BeginFrame {
        /// Target width in pixels.
        width: u32,
        /// Target height in pixels.
        height: u32,
        /// Background colour the target is cleared to before drawing.
        clear_color: [f32; 4],
    },
    /// Upload the triangle's vertices into a GPU vertex buffer on the server.
    UploadVertices {
        /// The vertices, in draw order.
        vertices: Vec<Vertex>,
    },
    /// Draw `vertex_count` vertices as a triangle list using the uploaded vertices.
    DrawTriangles {
        /// How many vertices to draw (3 for one triangle).
        vertex_count: u32,
    },
    /// End the frame: the server reads the rendered image back and writes the PNG.
    EndFrame,
}

#[cfg(test)]
mod tests {
    // Bring the message types into scope for the tests.
    use super::*;

    // A round-trip helper: serialize a message with postcard, deserialize it, and
    // return the result, so each test can assert "what went in comes back out".
    fn round_trip(message: &Message) -> Message {
        // Serialize to a byte vector using postcard's std-backed helper.
        let bytes =
            postcard::to_stdvec(message).expect("serialization must succeed for a valid message");
        // Deserialize those same bytes back into a Message.
        postcard::from_bytes(&bytes)
            .expect("deserialization must succeed for bytes we just produced")
    }

    #[test]
    fn hello_round_trips() {
        // A Hello carrying the current protocol version must survive a round trip unchanged.
        let original = Message::Hello {
            version: PROTOCOL_VERSION,
        };
        assert_eq!(round_trip(&original), original);
    }

    #[test]
    fn upload_vertices_round_trips() {
        // Three coloured vertices (the triangle) must survive a round trip unchanged.
        let original = Message::UploadVertices {
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
        assert_eq!(round_trip(&original), original);
    }
}
