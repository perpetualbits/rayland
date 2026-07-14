//! S-side presentation: turn a [`RenderedFrame`] into an on-screen Wayland window.
//!
//! SP0 rendered a triangle on the GPU and read it back into a `RenderedFrame` (tightly
//! packed RGBA8 in CPU memory). SP1's job is purely to *present* that frame: copy it into a
//! shared-memory (`wl_shm`) buffer and show it in a real `xdg_toplevel` window on the
//! compositor the user is sitting in front of. No GPU work changes; this module only adds
//! windowing.
//!
//! The pure conversion [`pack_xrgb8888`] is unit-tested here. The SCTK-driven window
//! ([`run_window`], added later) is integration code verified by building and by a manual
//! on-screen smoke check, because asserting real on-screen output needs a compositor.

// The rendered frame we present; its pixels are tightly-packed RGBA8.
use crate::render::RenderedFrame;

/// Copy a rendered frame's pixels into a `wl_shm` `Xrgb8888` buffer.
///
/// `RenderedFrame::pixels` is tightly-packed **RGBA8**: the four bytes of each pixel are, in
/// memory order, red, green, blue, alpha. A `wl_shm` `Xrgb8888` buffer stores each pixel as
/// a 32-bit value `0x00RRGGBB` interpreted **little-endian**, so its four bytes in memory
/// order are blue, green, red, unused. This function performs that reordering — the classic
/// channel-swizzle pitfall of feeding GPU pixels to a Wayland buffer. Getting it wrong makes
/// the red triangle appear blue on screen.
///
/// # Inputs
/// - `frame`: the source; `frame.pixels.len()` must equal `frame.width * frame.height * 4`
///   (which `render_triangle` guarantees).
/// - `dst`: the destination buffer, which must be exactly the same length as `frame.pixels`
///   (i.e. `width * height * 4`). The `wl_shm` pool row stride for a 32-bit format is
///   `width * 4`, which is already tight, so no per-row padding arises.
///
/// # Panics
/// Panics (via `assert_eq!`) if `dst` is not exactly `frame.pixels.len()` bytes — a caller
/// bug, since the caller sizes the buffer from the same dimensions.
pub fn pack_xrgb8888(frame: &RenderedFrame, dst: &mut [u8]) {
    // The destination must match the source exactly; a mismatch is a caller error, not a
    // recoverable runtime condition, so we assert rather than return a Result.
    assert_eq!(
        dst.len(),
        frame.pixels.len(),
        "destination buffer must be width*height*4 bytes"
    );
    // Walk source and destination four bytes (one pixel) at a time in lockstep.
    for (rgba, out) in frame.pixels.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        // Read the source channels by their documented RGBA positions.
        let r = rgba[0] as u32;
        let g = rgba[1] as u32;
        let b = rgba[2] as u32;
        // Assemble the 32-bit 0x00RRGGBB word; the unused top byte stays 0 (opaque window).
        let word = (r << 16) | (g << 8) | b;
        // Writing the word little-endian lays the bytes out as B, G, R, 0 — exactly the
        // Xrgb8888 memory order the compositor expects.
        out.copy_from_slice(&word.to_le_bytes());
    }
}

// --- Live Wayland window (SCTK + calloop) ---
//
// The types below come from smithay-client-toolkit (SCTK). SCTK wraps the raw Wayland
// registry/xdg-shell/wl_shm handshake in handler traits: we implement the few our window
// needs (compositor, shm, xdg window) and leave the rest to SCTK's delegate macros.

// Standard networking + io for the liveness socket.
use std::io::Read;
use std::net::TcpStream;

// calloop and the Wayland event source, reexported by SCTK so versions always match.
use smithay_client_toolkit::reexports::calloop::{
    EventLoop,
    Interest,
    LoopHandle, // the event loop and a handle to register sources on it
    Mode,
    PostAction,       // how a file-descriptor source is polled and what to do after
    generic::Generic, // wraps an arbitrary fd (our TCP socket) as a calloop source
};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource; // Wayland fd -> calloop
// wl_output is imported as a module (not just its WlOutput type) because
// CompositorHandler::transform_changed below needs the sibling type wl_output::Transform.
use smithay_client_toolkit::reexports::client::{
    Connection,
    QueueHandle,                  // the compositor connection and per-state queue handle
    globals::registry_queue_init, // one-shot registry bootstrap
    protocol::{wl_output, wl_shm, wl_surface::WlSurface}, // protocol objects we touch
};

// SCTK building blocks: registry, compositor, output tracking, shm pool, and xdg window.
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
// CompositorHandler's surface_enter/leave report a wl_output; SCTK 0.20 also requires any
// CompositorHandler to implement OutputHandler (an output-add/update/remove callback set) so
// hotplugged monitors can be tracked, even though this static window ignores them.
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_output, delegate_registry, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window, registry_handlers,
};

/// All state the window's event loop needs, threaded through SCTK's handler callbacks.
///
/// SCTK calls our handler methods with `&mut RaylandWindow`, so everything the window must
/// read or mutate in response to compositor events lives here: the SCTK sub-states, the
/// frame we are presenting, the shm pool, and the `exit` flag that ends the loop.
struct RaylandWindow {
    // SCTK's registry bookkeeping (which globals exist).
    registry_state: RegistryState,
    // SCTK's output (monitor) bookkeeping; required by CompositorHandler's blanket bound
    // even though this static, fixed-size window does not otherwise track outputs.
    output_state: OutputState,
    // SCTK's shared-memory manager; owns the wl_shm global.
    shm: Shm,
    // The pool we allocate the pixel buffer from.
    pool: SlotPool,
    // The xdg_toplevel window (surface + role).
    window: Window,
    // The frame we present; its RGBA8 pixels feed pack_xrgb8888.
    frame: RenderedFrame,
    // Set true to break the event loop: window closed or client disconnected.
    exit: bool,
    // True until the first configure, so we draw exactly once when the window is ready.
    first_configure: bool,
}

impl RaylandWindow {
    /// Fill the shm buffer from the frame and commit it to the surface.
    ///
    /// Allocates a buffer sized to the frame (the window is fixed at the frame's
    /// dimensions), converts the pixels with [`pack_xrgb8888`], attaches the buffer, marks
    /// the whole surface damaged, and commits. Called once on first configure; the static
    /// image then stays on screen with no further redraws.
    fn draw(&mut self) -> anyhow::Result<()> {
        // Fixed window size = the frame's size; stride is tight for a 32-bit format.
        let width = self.frame.width as i32;
        let height = self.frame.height as i32;
        let stride = width * 4;
        // Allocate a buffer and get writable access to its bytes in one step.
        let (buffer, canvas) = self
            .pool
            .create_buffer(width, height, stride, wl_shm::Format::Xrgb8888)
            .map_err(|e| anyhow::anyhow!("failed to create wl_shm buffer: {e}"))?;
        // Convert our RGBA8 frame into the Xrgb8888 buffer (the swizzle lives here).
        pack_xrgb8888(&self.frame, canvas);
        // Attach the finished buffer to the window's surface.
        let surface = self.window.wl_surface();
        buffer
            .attach_to(surface)
            .map_err(|e| anyhow::anyhow!("failed to attach buffer: {e}"))?;
        // Mark the entire surface as changed so the compositor repaints it.
        surface.damage_buffer(0, 0, width, height);
        // Commit the surface state (buffer + damage) to make it visible.
        self.window.commit();
        Ok(())
    }
}

impl CompositorHandler for RaylandWindow {
    // Output scale changes need no action: our buffer is a fixed-size static image.
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_factor: i32,
    ) {
    }
    // Output transform changes likewise need no action for a static frame.
    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }
    // We never request frame callbacks (the image is static), so this is a no-op.
    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _time: u32,
    ) {
    }
    // Which output the surface entered is irrelevant to a fixed-size static window.
    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
    // Same for leaving an output.
    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for RaylandWindow {
    // Hand SCTK our output bookkeeping.
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    // A monitor appearing needs no action: the window's fixed size does not depend on it.
    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
    // Likewise a monitor's mode/geometry changing.
    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
    // Likewise a monitor disappearing.
    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for RaylandWindow {
    // The user closed the window (e.g. the close button): end the loop, which drops the
    // socket and lets the client exit.
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _window: &Window) {
        self.exit = true;
    }
    // The compositor is ready for us to draw. We draw exactly once, on the first configure.
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
        _configure: WindowConfigure,
        _serial: u32,
    ) {
        // Only the first configure triggers a draw; later configures (e.g. focus changes)
        // need no redraw for a static image.
        if self.first_configure {
            self.first_configure = false;
            // A draw failure here is unexpected; surface it by requesting exit so the
            // process ends rather than hanging with a blank window.
            if self.draw().is_err() {
                self.exit = true;
            }
        }
    }
}

impl ShmHandler for RaylandWindow {
    // SCTK asks for our Shm state to service wl_shm events.
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for RaylandWindow {
    // Hand SCTK our registry bookkeeping.
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    // OutputState needs registry notifications to discover/track wl_output globals; we
    // register no other extra global handlers (no seats tracked).
    registry_handlers![OutputState];
}

// Wire SCTK's protocol dispatch to our handler impls above.
delegate_compositor!(RaylandWindow);
delegate_output!(RaylandWindow);
delegate_shm!(RaylandWindow);
delegate_xdg_shell!(RaylandWindow);
delegate_xdg_window!(RaylandWindow);
delegate_registry!(RaylandWindow);

/// Open a Wayland window showing `frame`, and keep it up until the window is closed or the
/// client on `stream` disconnects — whichever comes first.
///
/// This runs one `calloop` event loop with two sources: the Wayland connection (window
/// events) and the TCP socket (liveness). Closing the window sets the exit flag; the client
/// disconnecting is seen as end-of-stream on the socket and also sets it. On return the loop
/// and its sources drop, closing the socket, which the client observes as EOF.
///
/// # Errors
/// Returns an error if the compositor is unreachable (`WAYLAND_DISPLAY` unset or invalid), a
/// required global (`wl_compositor`, `wl_shm`, `xdg_wm_base`) is missing, buffer allocation
/// fails, or the event loop errors.
pub fn run_window(frame: RenderedFrame, stream: TcpStream) -> anyhow::Result<()> {
    // Connect to the compositor named by WAYLAND_DISPLAY.
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow::anyhow!("cannot connect to a Wayland compositor: {e}"))?;
    // Bootstrap the registry and get the initial event queue.
    let (globals, event_queue) = registry_queue_init(&conn)
        .map_err(|e| anyhow::anyhow!("Wayland registry initialization failed: {e}"))?;
    // A handle used to create protocol objects bound to our state type.
    let qh: QueueHandle<RaylandWindow> = event_queue.handle();

    // Create the calloop event loop that will drive everything.
    let mut event_loop: EventLoop<RaylandWindow> =
        EventLoop::try_new().map_err(|e| anyhow::anyhow!("failed to create event loop: {e}"))?;
    let loop_handle: LoopHandle<RaylandWindow> = event_loop.handle();

    // Feed Wayland events into the loop.
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| anyhow::anyhow!("failed to insert the Wayland source: {e}"))?;

    // Bind the globals we need; a missing one is a clear, fatal error.
    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wl_compositor unavailable: {e}"))?;
    let xdg_shell = XdgShell::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("xdg_wm_base (window shell) unavailable: {e}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| anyhow::anyhow!("wl_shm unavailable: {e}"))?;

    // Allocate a shm pool large enough for one frame-sized buffer.
    let pool_size = frame.width as usize * frame.height as usize * 4;
    let pool = SlotPool::new(pool_size, &shm)
        .map_err(|e| anyhow::anyhow!("failed to create shm pool: {e}"))?;

    // Create the surface and give it the xdg_toplevel role (a normal window).
    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    // A human-readable title and a stable app id (provisional).
    window.set_title("Rayland — SP1");
    window.set_app_id("nl.rayland.Sp1");
    // Request a fixed size by pinning min == max to the frame's dimensions; compositors
    // commonly honour this by floating the window at exactly that size.
    window.set_min_size(Some((frame.width, frame.height)));
    window.set_max_size(Some((frame.width, frame.height)));
    // Initial commit with no buffer: the compositor replies with a configure, after which
    // we draw.
    window.commit();

    // Register the TCP socket as a liveness source: readable-then-zero-bytes means the
    // client disconnected. Non-blocking so the callback never stalls the loop.
    stream
        .set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("failed to set the socket non-blocking: {e}"))?;
    loop_handle
        .insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            |_readiness, socket, state: &mut RaylandWindow| {
                // Drain whatever is readable; we only care whether the stream has ended.
                // NoIoDrop derefs to the wrapped TcpStream; std's `impl Read for &TcpStream` lets us
                // read through a shared reference, so no unsafe access to the fd is needed here.
                let mut socket: &std::net::TcpStream = socket;
                let mut sink = [0u8; 256];
                loop {
                    match socket.read(&mut sink) {
                        // EOF: the client is gone. Ask the loop to stop and remove this source.
                        Ok(0) => {
                            state.exit = true;
                            return Ok(PostAction::Remove);
                        }
                        // Unexpected bytes in SP1: ignore and keep draining.
                        Ok(_) => continue,
                        // Nothing more to read right now: leave the source in place.
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            return Ok(PostAction::Continue);
                        }
                        // A real socket error: treat like a disconnect and stop.
                        Err(_) => {
                            state.exit = true;
                            return Ok(PostAction::Remove);
                        }
                    }
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to watch the client socket: {e}"))?;

    // Assemble the state and run the loop until either trigger sets `exit`.
    let mut state = RaylandWindow {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        window,
        frame,
        exit: false,
        first_configure: true,
    };
    // Dispatch events, blocking until one arrives (`None` = no timeout); break out as soon
    // as `exit` is set. Blocking is fine because both teardown triggers — a window-close
    // event and socket readability on client disconnect — wake the loop.
    while !state.exit {
        event_loop
            .dispatch(None, &mut state)
            .map_err(|e| anyhow::anyhow!("event loop dispatch failed: {e}"))?;
    }
    // Returning drops the loop and its Generic source, closing the socket; the client then
    // sees EOF and exits.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::RenderedFrame;

    #[test]
    fn pack_xrgb8888_swizzles_rgba_into_little_endian_xrgb() {
        // Two known pixels: pure red then pure green, both fully opaque in the source.
        let frame = RenderedFrame {
            width: 2,
            height: 1,
            pixels: vec![
                255, 0, 0, 255, // red   (R,G,B,A)
                0, 255, 0, 255, // green (R,G,B,A)
            ],
        };
        // Destination sized exactly width*height*4.
        let mut dst = vec![0u8; frame.pixels.len()];
        pack_xrgb8888(&frame, &mut dst);
        // Red -> Xrgb8888 little-endian bytes B,G,R,X = 0,0,255,0.
        // Green -> 0,255,0,0.
        assert_eq!(dst, vec![0, 0, 255, 0, 0, 255, 0, 0]);
    }
}
