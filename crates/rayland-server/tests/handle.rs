//! Test that handle_connection turns a command stream into correct pixels, without any
//! sockets — the stream is an in-memory buffer built with the client's own function.

// The function under test.
use rayland_server::handle_connection;
// Reuse the client library to build a real command stream.
use rayland_client::send_triangle;

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
