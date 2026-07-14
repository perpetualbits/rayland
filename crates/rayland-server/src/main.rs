//! Rayland server binary: accept one TCP connection, render it on the GPU, and either show
//! the result in a live Wayland window (default) or write it to a PNG (`--png <path>`).

// The connection handler and the window presenter from the library.
use rayland_server::handle_connection;
use rayland_server::window::run_window;
// TcpListener accepts the incoming connection.
use std::net::TcpListener;

/// Run the server: bind, accept one connection, render the streamed frame, then present it.
///
/// Arguments (all optional, order-independent for the flag):
/// - the first positional argument is the listen address (default `127.0.0.1:9000`);
/// - `--png <path>` writes the frame to `<path>` and exits instead of opening a window
///   (the SP0 behaviour, kept for headless machines and for reproducing the PNG).
///
/// # Errors
/// Returns an error if binding, accepting, rendering, PNG writing, or window presentation
/// fails.
fn main() -> anyhow::Result<()> {
    // Collect args once so we can scan for the flag and the positional address.
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Look for `--png <path>`; if present, remember the path and treat it as the mode.
    let mut png_path: Option<String> = None;
    // The listen address is the first argument that is not the flag or its value.
    let mut address: Option<String> = None;
    // Walk the arguments, consuming the value after `--png`.
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // `--png` takes the next argument as its output path.
            "--png" => {
                let path = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--png requires a path argument"))?;
                png_path = Some(path.clone());
                // Skip the flag and its value.
                i += 2;
            }
            // The first non-flag argument is the listen address.
            other => {
                if address.is_none() {
                    address = Some(other.to_string());
                }
                i += 1;
            }
        }
    }
    // Fall back to the localhost default if no address was given.
    let address = address.unwrap_or_else(|| "127.0.0.1:9000".to_string());

    // Bind and announce readiness.
    let listener = TcpListener::bind(&address)?;
    println!("rayland-server listening on {address}");

    // Accept exactly one connection (SP1 still handles a single client).
    let (mut stream, peer) = listener.accept()?;
    println!("connection from {peer}");

    // Replay the stream on the GPU into a CPU-side frame.
    let frame = handle_connection(&mut stream)?;

    // Present the frame: PNG if requested, otherwise a live window.
    match png_path {
        // Headless/fallback path: encode the tightly-packed RGBA8 pixels as a PNG.
        Some(path) => {
            image::save_buffer(
                &path,
                &frame.pixels,
                frame.width,
                frame.height,
                image::ColorType::Rgba8,
            )?;
            println!("wrote {path} ({}x{})", frame.width, frame.height);
        }
        // Default path: show the frame in a window until it or the client closes.
        None => {
            println!("presenting in a window; close it (or stop the client) to exit");
            // Hand the socket to the window so it can watch for client disconnect.
            run_window(frame, stream)?;
            println!("window closed; exiting");
        }
    }

    // Success.
    Ok(())
}
