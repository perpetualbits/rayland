//! End-to-end: a real client on one thread sends over a real TCP socket to a server on
//! another thread, which renders and returns pixels. This is SP0's headline proof that the
//! client library, the wire framing, the server's stream handler, and the GPU renderer all
//! fit together across a genuine socket boundary rather than an in-memory buffer.

// The client's command-stream builder, exercised as the real sender in this test.
use rayland_client::send_triangle;
// The server's stream handler, exercised as the real receiver/renderer in this test.
use rayland_server::handle_connection;
// TcpListener accepts the connection server-side; TcpStream is the client-side socket.
use std::net::{TcpListener, TcpStream};

/// Drive a full client -> TCP -> server render and assert the returned pixels are correct.
///
/// A background thread plays the server: it binds an ephemeral port, accepts exactly one
/// connection, and renders the frame the client sends. The test thread plays the client:
/// it connects, sends the triangle stream (which ends with `EndFrame` — that is what makes
/// the server return, not the socket closing), and half-closes its write side as tidy
/// hygiene. Finally we join the server thread to recover the rendered frame and check two
/// pixels prove the triangle actually landed.
#[test]
fn client_to_server_over_tcp_renders_the_triangle() {
    // Bind to port 0 so the OS hands us a free ephemeral port, avoiding collisions.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind must succeed");
    // Read back the concrete address (with the chosen port) so the client can connect to it.
    let address = listener.local_addr().expect("listener has an address");
    // Run the server on its own thread; `move` gives it sole ownership of the listener.
    let server = std::thread::spawn(move || {
        // Accept the single connection this test makes; the peer address is unused here.
        let (mut stream, _peer) = listener.accept().expect("accept must succeed");
        // Replay the received command stream on the GPU and hand the frame back via join().
        handle_connection(&mut stream).expect("server must render the frame")
    });
    // Client side: open the socket to the server's ephemeral address.
    let mut stream = TcpStream::connect(address).expect("client connects");
    // Send a 64x64 frame on a blue clear colour so corners stay blue and the centre goes red.
    send_triangle(&mut stream, 64, 64, [0.0, 0.0, 1.0, 1.0]).expect("client sends");
    // Half-close the write side as hygiene: the server has already stopped at EndFrame, so
    // this only matters to a hypothetical server still reading — it would then see clean EOF.
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("shutdown write");
    // Block until the server finishes rendering and recover the frame it returned.
    let frame = server.join().expect("server thread must not panic");
    // Byte offset of the centre pixel's first channel: row 32, column 32, 4 bytes per pixel.
    let center_i = ((32 * 64 + 32) * 4) as usize;
    // The red triangle covers the centre, so its red channel must be near full (allow AA slop).
    assert!(
        (frame.pixels[center_i] as i16 - 255).abs() <= 8,
        "centre red channel"
    );
    // Top-left corner pixel; the triangle misses the corners, so the blue clear shows through.
    let corner_i = 0usize;
    // Blue is the third channel (index +2); it must be near full where the clear colour wins.
    assert!(frame.pixels[corner_i + 2] >= 247, "corner blue channel");
}
