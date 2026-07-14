//! Rayland client binary: connect to a server over QUIC, send the triangle stream, and hold
//! the connection open so the server's window stays on screen until it (or we) closes.

// The library functions that build and drain the command stream.
use rayland_client::{send_triangle, wait_until_closed};
// The QUIC transport connect entry point.
use rayland_transport::connect;

/// Connect to the server address given as the first CLI argument (default `127.0.0.1:9000`),
/// send one triangle at 256×256 on a blue background, then block until the server closes the
/// connection (which it does when its window is closed).
///
/// # Errors
/// Returns an error if the address is invalid, or the connection, send, or wait fails.
fn main() -> anyhow::Result<()> {
    // Resolve and parse the server address (a UDP socket address for QUIC).
    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9000".to_string());
    let server_addr: std::net::SocketAddr = address
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid server address {address:?}: {e}"))?;

    // Open the QUIC connection (returns a synchronous Read+Write stream).
    let mut stream = connect(server_addr)?;
    // Send the triangle command stream, exactly as over TCP.
    send_triangle(&mut stream, 256, 256, [0.0, 0.0, 1.0, 1.0])?;
    // Report and hold the connection open as a liveness channel.
    println!(
        "sent triangle to {address} over QUIC; holding the connection until the window closes"
    );
    // Returns when the server closes the connection (its window was closed).
    wait_until_closed(&mut stream)?;
    // The server closed the connection: we are done.
    println!("server closed the connection; exiting");
    Ok(())
}
