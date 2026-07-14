//! End-to-end over a real QUIC connection on localhost: a client sends the triangle command
//! stream, the server accepts it over QUIC, replays it on the GPU, and we assert the pixels.
//! This is SP2's headline proof that the transport swap is correct.

// The client's command-stream builder and the server's stream handler.
use rayland_client::send_triangle;
use rayland_server::handle_connection;
// The QUIC transport.
use rayland_transport::{connect, listen};
// Networking types.
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[test]
fn client_to_server_over_quic_renders_the_triangle() {
    // Bind the server on an ephemeral localhost UDP port.
    let listener =
        listen(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).expect("listen must succeed");
    // Discover the bound address for the client to connect to.
    let addr = listener.local_addr().expect("listener has an address");

    // Server thread: accept one QUIC connection and render the streamed frame.
    let server = std::thread::spawn(move || {
        let (mut recv, _liveness) = listener.accept().expect("accept must succeed");
        handle_connection(&mut recv).expect("server must render the frame")
    });

    // Client: connect over QUIC and send the triangle.
    let mut stream = connect(addr).expect("client connects");
    send_triangle(&mut stream, 64, 64, [0.0, 0.0, 1.0, 1.0]).expect("client sends");

    // Recover the rendered frame from the server thread.
    let frame = server.join().expect("server thread must not panic");

    // Centre must be red (inside the triangle).
    let center_i = ((32 * 64 + 32) * 4) as usize;
    assert!(
        (frame.pixels[center_i] as i16 - 255).abs() <= 8,
        "centre red channel"
    );
    // Top-left corner must be blue (clear colour shows through).
    assert!(frame.pixels[2] >= 247, "corner blue channel");
}
