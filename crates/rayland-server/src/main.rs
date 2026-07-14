//! Rayland server binary: accept one QUIC connection, render it on the GPU, and either show
//! the result in a live Wayland window (default, auto-detecting zero-copy dmabuf vs. the
//! `wl_shm` fallback — SP3) or write it to a PNG (`--png <path>`).

// The stream parser (returns an unrendered FrameRequest — SP3) and the persistent Renderer.
use rayland_server::read_frame_request;
use rayland_server::render::Renderer;
// The window presenter: auto-detects dmabuf vs. wl_shm and runs the show/teardown loop.
use rayland_server::window::present;
// The QUIC transport listener.
use rayland_transport::listen;

/// Run the server: bind a QUIC endpoint, accept one connection, render the streamed frame,
/// then present it.
///
/// The first positional argument is the listen address (default `127.0.0.1:9000`).
/// `--png <path>` writes a PNG and exits instead of opening a window. `--force-shm` skips the
/// dmabuf auto-detection and always uses the `wl_shm` fallback presenter (useful to exercise
/// that path deliberately, e.g. for manual verification) — it has no effect together with
/// `--png`, since the PNG path never touches Wayland at all.
///
/// # Errors
/// Returns an error if the address is invalid, or binding, accepting, rendering, PNG writing,
/// or window presentation fails.
fn main() -> anyhow::Result<()> {
    // Collect args, scanning for `--png <path>`, `--force-shm`, and the positional address
    // (same parsing shape as SP1/SP2, extended with the new SP3 flag).
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Set once `--png <path>` is seen; presence switches the present-frame mode below.
    let mut png_path: Option<String> = None;
    // Set true by `--force-shm`; forces the wl_shm fallback regardless of dmabuf capability.
    let mut force_shm = false;
    // The listen address is the first argument that is not a flag or a flag's value.
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
            // `--force-shm` is a bare flag: no value follows it.
            "--force-shm" => {
                force_shm = true;
                i += 1;
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

    // Build the persistent Renderer up front (SP3): both the `--png` path and the window path
    // need one, and the window path specifically needs THIS SAME instance to still be alive
    // when it later checks `supports_dmabuf()` and (if that path is chosen) calls
    // `render_to_dmabuf` — see `Renderer`'s and `window::present`'s doc comments.
    let mut renderer = Renderer::new()
        .map_err(|e| anyhow::anyhow!("failed to initialize the GPU renderer: {e}"))?;

    // Bind the QUIC endpoint and announce readiness.
    let listener = listen(bind_addr)?;
    println!("rayland-server listening on {address} (QUIC)");

    // Accept exactly one connection; get the sync command reader and the liveness handle.
    let (mut recv, liveness) = listener.accept()?;
    println!("connection accepted");

    // Parse and validate the stream into a render-ready request WITHOUT rendering it yet (SP3):
    // whether to render via `render_to_frame` or `render_to_dmabuf` is decided below, and for
    // the window path that decision needs a live Wayland connection (see `window::present`).
    let request = read_frame_request(&mut recv)?;

    // Present: PNG fallback, or a live window watching `liveness` for client disconnect.
    match png_path {
        Some(path) => {
            // Headless path: render via the ordinary CPU-readback path (dmabuf has no meaning
            // for a PNG dump) and encode the RGBA8 pixels. Dropping `liveness` closes the
            // connection so the client exits.
            let frame = renderer.render_to_frame(&request)?;
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
            // Default path: `present` decides dmabuf vs. wl_shm itself (it needs the live
            // Wayland connection to check the compositor's advertised dmabuf formats — see its
            // doc comment), renders accordingly, and shows the frame until the window or the
            // client closes. `liveness` is moved in BY VALUE; when the window loop ends,
            // `present` drops it, which closes the QUIC connection so the client also exits.
            println!("presenting in a window; close it (or stop the client) to exit");
            present(&mut renderer, &request, liveness, force_shm)?;
            println!("window closed; exiting");
        }
    }
    Ok(())
}
