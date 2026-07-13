//! Test that handle_connection turns a command stream into correct pixels, without any
//! sockets — the stream is an in-memory buffer built with the client's own function.

// The function under test.
use rayland_server::handle_connection;
// Reuse the client library to build a real command stream.
use rayland_client::send_triangle;
// Low-level wire access, to hand-build a *deliberately malformed* stream.
use rayland_wire::{Message, PROTOCOL_VERSION, write_message};

#[test]
fn handle_connection_renders_the_triangle() {
    // Build the exact byte stream a client would send.
    let mut stream: Vec<u8> = Vec::new();
    send_triangle(&mut stream, 64, 64, [0.0, 0.0, 1.0, 1.0]).expect("client build cannot fail");

    // Feed it to the server's connection handler.
    let mut cursor = std::io::Cursor::new(stream);
    let frame = handle_connection(&mut cursor).expect("handling the stream must render a frame");

    // The centre must be red (inside the triangle).
    let center = {
        let i = ((32 * 64 + 32) * 4) as usize;
        [frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2]]
    };
    assert!(
        (center[0] as i16 - 255).abs() <= 8 && center[1] <= 8 && center[2] <= 8,
        "centre should be red, was {center:?}"
    );
}

#[test]
fn end_frame_without_begin_frame_is_a_clean_error() {
    // Build a truncated stream: a valid handshake, then straight to EndFrame — no
    // BeginFrame and no vertices ever arrive. This is the "malformed stream" the handler
    // must reject with a clear error rather than passing a 0x0 target to the GPU.
    let mut stream: Vec<u8> = Vec::new();
    write_message(
        &mut stream,
        &Message::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .expect("writing to a Vec cannot fail");
    write_message(&mut stream, &Message::EndFrame).expect("writing to a Vec cannot fail");

    // The handler must return an Err (from the width/height guard), not attempt to render.
    let mut cursor = std::io::Cursor::new(stream);
    // Discard any Ok frame down to `()` so the assert message needn't format RenderedFrame
    // (which holds a large pixel buffer and intentionally does not derive Debug).
    let outcome = handle_connection(&mut cursor).map(|_| ());
    assert!(
        outcome.is_err(),
        "EndFrame before a valid BeginFrame must be rejected, got {outcome:?}"
    );
}
