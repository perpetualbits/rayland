//! Rayland server binary: accept one QUIC connection, render it on the GPU, and either show
//! the result in a live Wayland window (default) or write it to a PNG (`--png <path>`).

// The connection handler and the window presenter from the library.
use rayland_server::handle_connection;
use rayland_server::window::run_window;
// The QUIC transport listener.
use rayland_transport::listen;

/// Run the server: bind a QUIC endpoint, accept one connection, render the streamed frame,
/// then present it (window by default, or `--png <path>` to write a PNG and exit).
///
/// The first positional argument is the listen address (default `127.0.0.1:9000`).
///
/// # Errors
/// Returns an error if the address is invalid, or binding, accepting, rendering, PNG writing,
/// or window presentation fails.
fn main() -> anyhow::Result<()> {
    // Collect args, scanning for `--png <path>` and the positional address (same as SP1).
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Set once `--png <path>` is seen; presence switches the present-frame mode below.
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
                // Skip past both the flag and its value.
                i += 2;
            }
            // The first non-flag argument is the listen address; later ones are ignored.
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
    // Parse the UDP socket address QUIC binds to.
    let bind_addr: std::net::SocketAddr = address
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address {address:?}: {e}"))?;

    // Bind the QUIC endpoint and announce readiness.
    let listener = listen(bind_addr)?;
    println!("rayland-server listening on {address} (QUIC)");

    // Accept exactly one connection; get the sync command reader and the liveness handle.
    let (mut recv, liveness) = listener.accept()?;
    println!("connection accepted");

    // Replay the stream on the GPU into a CPU-side frame.
    let frame = handle_connection(&mut recv)?;

    // Present: PNG fallback, or a live window watching `liveness` for client disconnect.
    match png_path {
        Some(path) => {
            // Headless path: encode the RGBA8 pixels as a PNG. Dropping `liveness` closes the
            // connection so the client exits.
            image::save_buffer(
                &path,
                &frame.pixels,
                frame.width,
                frame.height,
                image::ColorType::Rgba8,
            )?;
            println!("wrote {path} ({}x{})", frame.width, frame.height);
            drop(liveness);
        }
        None => {
            // Default path: show the frame until the window or the client closes. `liveness`
            // is moved in BY VALUE; when the window loop ends, run_window drops it, which
            // closes the QUIC connection so the client also exits.
            println!("presenting in a window; close it (or stop the client) to exit");
            run_window(frame, liveness)?;
            println!("window closed; exiting");
        }
    }
    Ok(())
}
