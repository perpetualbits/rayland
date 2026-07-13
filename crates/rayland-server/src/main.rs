//! Rayland server binary: accept one TCP connection, render it, and write a PNG.

// The connection handler from the library.
use rayland_server::handle_connection;
// TcpListener accepts incoming connections.
use std::net::TcpListener;

/// Listen on the address given as the first CLI argument (default `127.0.0.1:9000`),
/// handle exactly one connection, and write the rendered image to the path given as the
/// second argument (default `out.png`).
///
/// # Errors
/// Returns an error if binding, accepting, rendering, or writing the PNG fails.
fn main() -> anyhow::Result<()> {
    // Resolve the listen address and output path from argv, with localhost defaults.
    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9000".to_string());
    let out_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "out.png".to_string());

    // Bind and announce readiness.
    let listener = TcpListener::bind(&address)?;
    println!("rayland-server listening on {address}");

    // Accept exactly one connection (SP0 handles a single client then exits).
    let (mut stream, peer) = listener.accept()?;
    println!("connection from {peer}");

    // Replay the stream on the GPU.
    let frame = handle_connection(&mut stream)?;

    // Encode the tightly-packed RGBA8 pixels as a PNG and save them.
    image::save_buffer(
        &out_path,
        &frame.pixels,
        frame.width,
        frame.height,
        image::ColorType::Rgba8,
    )?;
    println!("wrote {out_path} ({}x{})", frame.width, frame.height);

    // Success.
    Ok(())
}
