//! Rayland client binary: connect to a server over TCP, send the triangle stream, and keep
//! the connection open so the server's window stays on screen until it (or we) closes.

// The library functions that do the actual work.
use rayland_client::{send_triangle, wait_until_closed};
// TcpStream is our byte sink (Write) and liveness channel (Read).
use std::net::TcpStream;

/// Connect to the server address given as the first CLI argument (default
/// `127.0.0.1:9000`), send one triangle at 256×256 on a blue background, then block until
/// the server closes the connection (which it does when its window is closed).
///
/// # Errors
/// Returns an error if the connection, the send, or the wait fails.
fn main() -> anyhow::Result<()> {
    // Read the server address from argv, or fall back to the localhost default.
    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9000".to_string());
    // Open the TCP connection to the server.
    let mut stream = TcpStream::connect(&address)?;
    // Send the triangle command stream.
    send_triangle(&mut stream, 256, 256, [0.0, 0.0, 1.0, 1.0])?;
    // Report that the frame is on its way and the window will stay until closed.
    println!("sent triangle to {address}; holding the connection until the window closes");
    // Hold the connection open as a liveness channel; returns when the server closes it
    // (i.e. the window was closed). Killing this process instead makes the server's socket
    // read hit EOF, which closes the window — the symmetric teardown.
    wait_until_closed(&mut stream)?;
    // The server closed the connection: the window is gone, so we are done.
    println!("server closed the connection; exiting");
    // Success.
    Ok(())
}
