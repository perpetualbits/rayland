//! The Rayland client (library half): builds and sends a triangle command stream.

// The message types, framing writer, and version constant.
use rayland_wire::{Message, PROTOCOL_VERSION, Vertex, WireError, write_message};
// Read and Write are the traits for anything we can read/send bytes to (a Vec in tests, a TcpStream in main).
use std::io::{Read, Write};

/// Build the SP0 triangle command stream and write it to `w`.
///
/// Emits the fixed sequence the server expects — `Hello`, `BeginFrame`, `UploadVertices`,
/// `DrawTriangles`, `EndFrame` — for a single centred red triangle on the given clear
/// colour. The triangle geometry is hardcoded in SP0; later sub-projects derive it from a
/// real application instead.
///
/// # Errors
/// Returns a [`WireError`] if any message fails to serialize or the write fails.
pub fn send_triangle<W: Write>(
    w: &mut W,
    width: u32,
    height: u32,
    clear_color: [f32; 4],
) -> Result<(), WireError> {
    // Handshake first so the server can reject a mismatched protocol version.
    write_message(
        w,
        &Message::Hello {
            version: PROTOCOL_VERSION,
        },
    )?;
    // Ask for an off-screen target of the requested size and background colour.
    write_message(
        w,
        &Message::BeginFrame {
            width,
            height,
            clear_color,
        },
    )?;
    // The three vertices of a centred triangle, all red; it covers the image centre but
    // not the corners, which is what the server-side pixel test relies on.
    let vertices = vec![
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
    ];
    // Upload the geometry.
    write_message(w, &Message::UploadVertices { vertices })?;
    // Draw the three vertices as one triangle.
    write_message(w, &Message::DrawTriangles { vertex_count: 3 })?;
    // End the frame, prompting the server to read back and save the image.
    write_message(w, &Message::EndFrame)?;
    // All messages sent.
    Ok(())
}

/// Block until the peer closes the connection, discarding anything it sends.
///
/// In SP1 the client sends one frame and then holds the connection open purely as a
/// *liveness channel*: as long as the socket is open, the server keeps the window on screen.
/// When the user closes the window, the server drops the socket; the read here then returns
/// end-of-stream and the client exits. The SP1 server sends no bytes back, so any received
/// data is unexpected and simply discarded rather than interpreted.
///
/// # Errors
/// Returns any I/O error other than a clean end of stream (which is the normal, successful
/// termination and yields `Ok(())`).
pub fn wait_until_closed<R: Read>(reader: &mut R) -> std::io::Result<()> {
    // A small scratch buffer; we never keep what we read — only watch for the stream to end.
    let mut sink = [0u8; 256];
    loop {
        match reader.read(&mut sink) {
            // Zero bytes read means the peer closed the connection: the window was closed.
            Ok(0) => return Ok(()),
            // Any bytes are unexpected in SP1; discard them and keep watching for the close.
            Ok(_) => continue,
            // A genuine I/O failure propagates to the caller.
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayland_wire::{Message, PROTOCOL_VERSION, read_message};

    #[test]
    fn send_triangle_emits_the_expected_sequence() {
        // Send into an in-memory buffer instead of a socket.
        let mut buffer: Vec<u8> = Vec::new();
        send_triangle(&mut buffer, 64, 64, [0.0, 0.0, 1.0, 1.0])
            .expect("writing to a Vec cannot fail");

        // Read the framed messages back out.
        let mut cursor = std::io::Cursor::new(buffer);
        let mut messages = Vec::new();
        while let Ok(m) = read_message(&mut cursor) {
            messages.push(m);
        }

        // The sequence must be exactly Hello, BeginFrame, UploadVertices(3), Draw(3), End.
        assert_eq!(
            messages.len(),
            5,
            "expected five messages, got {}",
            messages.len()
        );
        assert_eq!(
            messages[0],
            Message::Hello {
                version: PROTOCOL_VERSION
            }
        );
        assert!(matches!(
            messages[1],
            Message::BeginFrame {
                width: 64,
                height: 64,
                ..
            }
        ));
        assert!(
            matches!(&messages[2], Message::UploadVertices { vertices } if vertices.len() == 3)
        );
        assert_eq!(messages[3], Message::DrawTriangles { vertex_count: 3 });
        assert_eq!(messages[4], Message::EndFrame);
    }

    #[test]
    fn wait_until_closed_returns_at_end_of_stream() {
        // A reader that yields a few bytes and then EOF models a server that sends nothing
        // and later closes the connection. wait_until_closed must drain and return Ok.
        let mut reader = std::io::Cursor::new(vec![1u8, 2, 3]);
        wait_until_closed(&mut reader).expect("reaching end of stream is success, not error");
    }
}
