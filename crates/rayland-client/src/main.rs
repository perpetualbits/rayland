//! Rayland client binary: connect to a server over TCP and send the triangle stream.

// The library function that does the actual work.
use rayland_client::send_triangle;
// TcpStream is our byte sink; it implements Write.
use std::net::TcpStream;

/// Connect to the server address given as the first CLI argument (default
/// `127.0.0.1:9000`) and send one triangle at 256×256 on a blue background.
///
/// # Errors
/// Returns an error if the connection or any send fails.
fn main() -> anyhow::Result<()> {
    // Read the server address from argv, or fall back to the localhost default.
    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9000".to_string());
    // Open the TCP connection to the server.
    let mut stream = TcpStream::connect(&address)?;
    // Send the triangle command stream.
    send_triangle(&mut stream, 256, 256, [0.0, 0.0, 1.0, 1.0])?;
    // Tell the user where the result will appear (the server writes the PNG).
    println!("sent triangle to {address}; the server writes the PNG");
    // Success.
    Ok(())
}
