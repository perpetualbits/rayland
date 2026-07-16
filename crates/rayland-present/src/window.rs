//! The live Wayland window: bind the globals, choose a path, draw once, and stay up.
//!
//! This is SP1's `wl_shm` presenter and SP3's zero-copy dmabuf presenter, moved verbatim out of
//! `rayland-server`'s `window.rs` by (c)1 Task 7 and given one new seam — [`FrameSource`] — so that
//! two unrelated producers can drive the identical code. See the crate docs for why the seam is
//! where it is.
//!
//! The SCTK-driven window ([`present`]) is integration code: it is verified by building, by
//! `tests/live_window.rs` (which runs it against a **real** compositor when one is reachable and
//! skips cleanly when one is not), and by a human looking at the screen — because no automated
//! test can assert what a compositor actually painted.

// The frame shapes this module presents, and the pure RGBA8 -> Xrgb8888 conversion the `wl_shm`
// path performs on every draw.
use crate::frame::{
    DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888, DmabufFrame, RenderedFrame, pack_xrgb8888,
};

// `Read` is used generically in `present`'s bound and callback (the concrete disconnect
// source — `TcpStream` in SP1, the QUIC `Liveness` in SP2/SP3, a `UnixStream` pair in (c)1's
// `rayland-s` — is supplied by the caller).
use std::io::Read;
// Borrowing our owned dmabuf fd for the `zwp_linux_buffer_params_v1::add` request, which takes
// a `BorrowedFd` (the request itself dup()s whatever it needs when the message is encoded; our
// `OwnedFd` in `DmabufFrame` keeps the underlying fd alive for as long as the `DmabufFrame`
// lives, which `present` ensures outlasts the attach+commit that consumes it).
use std::os::fd::AsFd;

// calloop and the Wayland event source, reexported by SCTK so versions always match.
use smithay_client_toolkit::reexports::calloop::{
    EventLoop,
    Interest,
    LoopHandle, // the event loop and a handle to register sources on it
    Mode,
    PostAction,       // how a file-descriptor source is polled and what to do after
    generic::Generic, // wraps an arbitrary fd (the disconnect source) as a calloop source
};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource; // Wayland fd -> calloop
// wl_output is imported as a module (not just its WlOutput type) because
// CompositorHandler::transform_changed below needs the sibling type wl_output::Transform.
// wl_buffer is imported the same way: SP3's dmabuf path builds a raw `WlBuffer` itself (SCTK's
// `Shm`/`SlotPool` only wraps buffers for the wl_shm path), so `RaylandWindow` needs its own
// `Dispatch` impl for it (see below).
use smithay_client_toolkit::reexports::client::{
    Connection,
    Dispatch,
    QueueHandle, // the compositor connection and per-state queue handle
    globals::{GlobalList, registry_queue_init}, // one-shot registry bootstrap + the bound-globals list
    protocol::{wl_buffer, wl_output, wl_shm, wl_surface::WlSurface}, // protocol objects we touch
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

// The raw `zwp_linux_dmabuf_v1`/`zwp_linux_buffer_params_v1` protocol objects (SP3 Task 4).
// `zv1` matches the module path `wayland-protocols` 0.32 generates for this protocol (it lives
// under the crate's "stable" XML category but the Rust module nesting still reads `zv1`,
// matching the interfaces' own `z`-prefixed, "unstable-naming-convention" names — see the
// task report for how this path was confirmed against the installed crate version).
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
};

/// Where [`present`] gets the frame it shows — the seam (c)1 Task 7 cut through SP3's presenter.
///
/// # Why a trait, and why it produces rather than provides
/// The obvious extraction would have been `present(frame: &RenderedFrame, ...)`: hand the
/// presenter finished pixels. **That cannot work**, and the reason is SP3's, not (c)1's. Whether
/// the zero-copy dmabuf path is usable depends on what *this specific compositor* advertises, which
/// is only knowable after connecting — i.e. from inside `present`. And the two paths need
/// *different renders*: the dmabuf path needs an exported LINEAR `B8G8R8A8` image, the `wl_shm`
/// path needs CPU-side RGBA8. So the frame cannot be produced before the decision, and the decision
/// cannot be made before the connection. A trait lets `present` make the call it alone can make and
/// then ask for the matching frame.
///
/// # The two implementors, and why they are so different
/// - **`rayland-server`** (SP-era) wraps a live Vulkan `Renderer`. It can answer
///   [`supports_dmabuf`](Self::supports_dmabuf) truthfully and produce either shape.
/// - **`rayland-s`** ((c)1) wraps bytes that were copied out of a blob whose real memory lives on
///   S's GPU. It answers `false`, permanently and structurally — **not** because the GPU cannot
///   export a dmabuf, but because S never sees the resource to export. Spec §7.1: the application's
///   `DEVICE_LOCAL` render target produces **no blob at all**, so there is nothing there to hand
///   the compositor. See `rayland_s::present` for the full account.
///
/// # Contract
/// [`produce_dmabuf`](Self::produce_dmabuf) is called **only** when
/// [`supports_dmabuf`](Self::supports_dmabuf) returned `true`; [`produce_pixels`](Self::produce_pixels)
/// only otherwise. Exactly one of the two is called, exactly once, per `present` call.
pub trait FrameSource {
    /// The frame's width in pixels. Read *before* any frame is produced, because the window is
    /// created (and its fixed size requested) before the render happens.
    fn width(&self) -> u32;

    /// The frame's height in pixels. Read before any frame is produced — see
    /// [`width`](Self::width).
    fn height(&self) -> u32;

    /// Can this source produce a dmabuf export at all?
    ///
    /// This is the **producer** half of the two-part capability check; the *compositor* half is
    /// [`present`]'s own probe. Both must hold, so answering `true` is a claim that
    /// [`produce_dmabuf`](Self::produce_dmabuf) will work, not a request for it.
    ///
    /// Defaults to `false`: a source that does not override this gets the `wl_shm` path and never
    /// has [`produce_dmabuf`](Self::produce_dmabuf) called. That default is the honest one — a
    /// source has to *do* something (export GPU memory) to earn a `true` here, so silence means no.
    fn supports_dmabuf(&self) -> bool {
        false
    }

    /// Produce the frame as a dmabuf export: an fd plus the layout the compositor needs.
    ///
    /// Called only if [`supports_dmabuf`](Self::supports_dmabuf) returned `true` **and** the
    /// compositor advertised `XRGB8888`/`LINEAR`. The returned frame's pixels must already be
    /// final — `present` attaches and commits it with no further synchronization, so any
    /// GPU fence-wait must have happened inside this method.
    ///
    /// # Errors
    /// Whatever the export failed with. `present` propagates it and shows nothing.
    ///
    /// The default implementation returns an error, which is unreachable for any implementor that
    /// honours the contract: the default `supports_dmabuf` is `false`, so a source that overrides
    /// neither is never asked. A source that overrides `supports_dmabuf` to `true` and forgets this
    /// method gets a named error rather than a wrong picture.
    fn produce_dmabuf(&mut self) -> anyhow::Result<DmabufFrame> {
        anyhow::bail!(
            "this frame source claimed supports_dmabuf() but did not implement produce_dmabuf(); \
             presenting is impossible and guessing would show the wrong thing"
        )
    }

    /// Produce the frame as CPU-side, tightly-packed **RGBA8** pixels.
    ///
    /// Called on the `wl_shm` path. The returned frame must satisfy [`RenderedFrame`]'s invariant
    /// (`pixels.len() == width * height * 4`, channel order R,G,B,A) — [`pack_xrgb8888`] asserts
    /// the length but can only *assume* the channel order.
    ///
    /// # Errors
    /// Whatever producing the pixels failed with. `present` propagates it and shows nothing.
    fn produce_pixels(&mut self) -> anyhow::Result<RenderedFrame>;
}

/// The knobs [`present`] takes that are about the *window* rather than the frame.
///
/// A struct rather than three positional parameters because two of them are `&str` and swapping
/// them would compile and produce a subtly mislabelled window — the kind of bug that survives
/// review.
pub struct WindowConfig<'a> {
    /// The window title the compositor shows in its decoration/taskbar. Human-facing.
    pub title: &'a str,
    /// The stable application id (reverse-DNS by convention), used by compositors to group windows
    /// and match desktop entries. Not human-facing.
    pub app_id: &'a str,
    /// Skip the dmabuf auto-detection entirely and always take the `wl_shm` path.
    ///
    /// This is what `rayland-server`'s `--force-shm` flag maps to: it lets the fallback be
    /// exercised deliberately, e.g. to verify by hand that it still works on a machine where the
    /// dmabuf path would otherwise win.
    pub force_shm: bool,
}

/// Which presentation backend is active for the on-screen buffer, decided once (in [`present`],
/// before the event loop starts) and never changed afterward — a presenter shows exactly one
/// static frame per call, so there is no notion of switching paths mid-run.
///
/// `draw()` matches on this to build and attach the right kind of `wl_buffer`. Bundling the
/// per-path state (the `wl_shm` pool + CPU frame, or the dmabuf global + exported frame) into
/// one enum — rather than a set of `Option` fields on [`RaylandWindow`] — makes "which path am
/// I on" a single match instead of several fields that could, by construction error, disagree.
enum Presentation {
    /// SP1's original path: a CPU-side [`RenderedFrame`] copied into a `wl_shm` `SlotPool`
    /// buffer on every draw (here, exactly once — the image is static).
    Shm {
        /// The pool the buffer for `frame` is allocated from.
        pool: SlotPool,
        /// The pixels to copy in, via [`pack_xrgb8888`].
        frame: RenderedFrame,
    },
    /// SP3's zero-copy path: a GPU-resident dmabuf imported by the compositor directly — no
    /// CPU pixel copy.
    Dmabuf {
        /// The bound `zwp_linux_dmabuf_v1` global, used to create the
        /// `zwp_linux_buffer_params_v1` that turns `frame`'s fd into a `wl_buffer`.
        dmabuf_global: ZwpLinuxDmabufV1,
        /// The exported dmabuf (fd + layout) [`FrameSource::produce_dmabuf`] produced. Its
        /// `OwnedFd` must stay alive until the `wl_buffer` built from it has been attached and
        /// the surface committed (see `draw`'s dmabuf branch) — keeping it here, owned by the
        /// long-lived window state rather than a temporary, guarantees that.
        frame: DmabufFrame,
    },
}

/// All state the window's event loop needs, threaded through SCTK's handler callbacks.
///
/// SCTK calls our handler methods with `&mut RaylandWindow`, so everything the window must
/// read or mutate in response to compositor events lives here: the SCTK sub-states, which
/// presentation path is active and its data, the shm global (bound unconditionally — see
/// `present` — since `ShmHandler` is required regardless of which path is actually drawn),
/// and the `exit` flag that ends the loop.
struct RaylandWindow {
    // SCTK's registry bookkeeping (which globals exist).
    registry_state: RegistryState,
    // SCTK's output (monitor) bookkeeping; required by CompositorHandler's blanket bound
    // even though this static, fixed-size window does not otherwise track outputs.
    output_state: OutputState,
    // SCTK's shared-memory manager; owns the wl_shm global. Bound unconditionally (even on the
    // dmabuf path) purely because `ShmHandler`/`delegate_shm!` require it to exist; its pool is
    // only actually allocated when `presentation` is `Presentation::Shm`.
    shm: Shm,
    // The xdg_toplevel window (surface + role).
    window: Window,
    // Which presentation path is active, and that path's data. See [`Presentation`].
    presentation: Presentation,
    // Set true to break the event loop: window closed, peer disconnected, or a draw failed.
    exit: bool,
    // True until the first configure, so we draw exactly once when the window is ready.
    first_configure: bool,
    // Why `draw` failed, if it did — carried out of the event loop so `present` can return it.
    //
    // SCTK's `configure` callback cannot return a `Result` (its signature is the trait's), so a
    // draw failure has nowhere to go but here. Before (c)1 Task 7 it went nowhere at all: the
    // error was discarded and `present` returned `Ok(())` having shown the user nothing, while
    // its own doc comment promised it errors when "buffer allocation/creation fails". See the
    // event loop at the bottom of `present`, which is where this is turned back into a return
    // value.
    draw_error: Option<anyhow::Error>,
}

impl RaylandWindow {
    /// Build the appropriate `wl_buffer` for the active [`Presentation`], attach it to the
    /// surface, mark the surface fully damaged, and commit — showing the frame.
    ///
    /// Called once, on first configure (see `WindowHandler::configure`); the static image then
    /// stays on screen with no further redraws. `qh` is needed only by the dmabuf branch
    /// (`create_params`/`create_immed` are proxy-constructing requests that must know which
    /// queue/state their new objects' events dispatch through).
    fn draw(&mut self, qh: &QueueHandle<Self>) -> anyhow::Result<()> {
        match &mut self.presentation {
            Presentation::Shm { pool, frame } => {
                // Fixed window size = the frame's size; stride is tight for a 32-bit format.
                let width = frame.width as i32;
                let height = frame.height as i32;
                let stride = width * 4;
                // Allocate a buffer and get writable access to its bytes in one step.
                let (buffer, canvas) = pool
                    .create_buffer(width, height, stride, wl_shm::Format::Xrgb8888)
                    .map_err(|e| anyhow::anyhow!("failed to create wl_shm buffer: {e}"))?;
                // Convert our RGBA8 frame into the Xrgb8888 buffer (the swizzle lives here).
                pack_xrgb8888(frame, canvas);
                // Attach the finished buffer to the window's surface.
                let surface = self.window.wl_surface();
                buffer
                    .attach_to(surface)
                    .map_err(|e| anyhow::anyhow!("failed to attach buffer: {e}"))?;
                // Mark the entire surface as changed so the compositor repaints it.
                surface.damage_buffer(0, 0, width, height);
                // Commit the surface state (buffer + damage) to make it visible.
                self.window.commit();
            }
            Presentation::Dmabuf {
                dmabuf_global,
                frame,
            } => {
                let width = frame.width as i32;
                let height = frame.height as i32;
                // Batch the single dmabuf plane into a temporary "params" object. LINEAR
                // XRGB8888 is single-plane, so exactly one `add` call (plane_idx 0) is needed;
                // offset/stride come from the driver's own subresource-layout query (Task 1),
                // NOT width*4, because a LINEAR image's row pitch may be padded beyond that.
                let params: ZwpLinuxBufferParamsV1 = dmabuf_global.create_params(qh, ());
                // The wire protocol splits the 64-bit modifier into two 32-bit halves.
                let modifier_hi = (frame.modifier >> 32) as u32;
                let modifier_lo = frame.modifier as u32;
                params.add(
                    frame.fd.as_fd(),
                    0, // plane_idx: the only plane a single-plane LINEAR image has
                    frame.offset,
                    frame.stride,
                    modifier_hi,
                    modifier_lo,
                );
                // `create_immed` (rather than `create`) asks the compositor to create the
                // `wl_buffer` SYNCHRONOUSLY from the client's point of view: no `created`/
                // `failed` event round trip to wait for (Task 4 brief's explicit reason for
                // choosing it) — either the server accepts it inline, or a bad batch is a
                // protocol error surfaced through normal Wayland error handling, not an event
                // this code has to poll for.
                //
                // The fence-wait for the *pixel data* already happened inside
                // [`FrameSource::produce_dmabuf`], whose contract requires it (for
                // `rayland-server` that is `Renderer::render_to_dmabuf`, which blocks the host
                // until the GPU blit that produced this dmabuf's contents has completed) — so it
                // is safe to attach and commit immediately below with no further synchronization
                // on our side.
                let buffer = params.create_immed(
                    width,
                    height,
                    frame.drm_format,
                    zwp_linux_buffer_params_v1::Flags::empty(),
                    qh,
                    (),
                );
                // The params object's one job (creating `buffer`) is done; the protocol
                // description says it "should be destroyed after a 'created' or 'failed' event
                // has been received" — `create_immed` has neither, so destroy it here instead,
                // as soon as it has served its purpose, rather than leaving it to linger.
                params.destroy();
                // Attach at surface-local (0, 0): this window is exactly the frame's size, so
                // the buffer covers the whole surface with no offset.
                let surface = self.window.wl_surface();
                surface.attach(Some(&buffer), 0, 0);
                surface.damage_buffer(0, 0, width, height);
                self.window.commit();
                // `buffer`'s Rust-side handle can be dropped here: once attached and committed,
                // the compositor holds its own protocol-level reference, and this window never
                // creates a second buffer to conflict with it (one static frame per `present`).
            }
        }
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
    // disconnect source and lets the peer exit.
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _window: &Window) {
        self.exit = true;
    }
    // The compositor is ready for us to draw. We draw exactly once, on the first configure.
    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _window: &Window,
        _configure: WindowConfigure,
        _serial: u32,
    ) {
        // Only the first configure triggers a draw; later configures (e.g. focus changes)
        // need no redraw for a static image.
        if self.first_configure {
            self.first_configure = false;
            // A draw failure here is unexpected; end the loop rather than hang with a blank window,
            // and **keep the error** so `present` can return it. Discarding it (which this line did
            // until (c)1 Task 7) made `present` answer `Ok(())` after showing nothing — the exact
            // shape of silent nothing this branch keeps shipping.
            if let Err(e) = self.draw(qh) {
                self.draw_error = Some(e);
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

// --- SP3 Task 4: zwp_linux_dmabuf_v1 Dispatch impls ---
//
// SCTK has no delegate macro for this protocol, so these three `Dispatch` impls are written by
// hand, following the same pattern SCTK's own generated code uses (a `state: &mut Self`
// method matching on the interface's `Event` enum).

impl Dispatch<ZwpLinuxDmabufV1, ()> for RaylandWindow {
    /// `zwp_linux_dmabuf_v1` events on the *real* (non-probe) binding used by the dmabuf
    /// presentation path itself.
    ///
    /// By the time `present` binds this "real" instance (see its doc comment), the
    /// dmabuf-vs-`wl_shm` decision has already been made using a SEPARATE, throwaway probe
    /// binding (`compositor_supports_dmabuf_xrgb8888_linear`'s own queue/state) — so this
    /// impl has nothing left to decide and only needs to exist to satisfy `Dispatch`'s trait
    /// bound (binding any object requires an impl for its interface on the binding state).
    /// Any `format`/`modifier` events the compositor resends to this second binding are
    /// therefore ignored.
    fn event(
        _state: &mut Self,
        _proxy: &ZwpLinuxDmabufV1,
        _event: zwp_linux_dmabuf_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for RaylandWindow {
    /// `created`/`failed` are only sent in response to the asynchronous `create` request;
    /// `draw`'s dmabuf branch always uses `create_immed` instead, specifically to avoid that
    /// round trip (see `draw`'s doc comment), so in practice neither event variant is ever
    /// dispatched here. The impl exists only because `Dispatch` must be implemented for every
    /// interface a proxy is created for, regardless of which requests that proxy actually uses.
    fn event(
        _state: &mut Self,
        _proxy: &ZwpLinuxBufferParamsV1,
        _event: zwp_linux_buffer_params_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for RaylandWindow {
    /// `release` tells us the compositor is done reading this buffer and it would be safe to
    /// reuse or free it. A presenter shows exactly one static frame per call and never recycles
    /// buffers, so there is nothing to do when it arrives; the buffer's backing dmabuf fd (and
    /// the GPU memory behind it) is instead kept alive by the `DmabufFrame` inside
    /// [`Presentation::Dmabuf`], whose own lifetime contract is on [`present`].
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

// --- SP3 Task 4: the compositor-capability probe ---

/// Accumulates every (DRM format, DRM modifier) pair `zwp_linux_dmabuf_v1` advertises, for
/// [`compositor_supports_dmabuf_xrgb8888_linear`]'s throwaway probe queue.
///
/// A dedicated, minimal state type (rather than reusing `RaylandWindow`) sidesteps an ordering
/// hazard: the probe must finish, and the dmabuf-vs-`wl_shm` decision must be made, BEFORE the
/// real window's surface exists and receives its first `configure` event. If the probe instead
/// used `RaylandWindow`'s own queue, a `RaylandWindow` would have to exist (with some
/// `presentation` already chosen) before the very capability check that chooses it — a
/// chicken-and-egg problem this separate, short-lived queue+type avoids entirely: it is built,
/// used once, and discarded before `RaylandWindow` is ever constructed.
#[derive(Default)]
struct DmabufProbe {
    /// Every `(format, modifier)` pair reported by a `modifier` event, in arrival order.
    formats: Vec<(u32, u64)>,
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for DmabufProbe {
    /// Record every advertised `(format, modifier)` pair.
    ///
    /// Only `modifier` events (added at protocol version 3, which
    /// [`compositor_supports_dmabuf_xrgb8888_linear`] deliberately binds — see its doc
    /// comment) carry the explicit format+modifier pairing this project checks for. A bare
    /// `format` event (sent since version 1) says a DRM fourcc is supported at all, but not
    /// with which tiling, so on its own it cannot confirm `DRM_FORMAT_MOD_LINEAR` support and
    /// is ignored here.
    fn event(
        state: &mut Self,
        _proxy: &ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if let zwp_linux_dmabuf_v1::Event::Modifier {
            format,
            modifier_hi,
            modifier_lo,
        } = event
        {
            // Reassemble the wire's two 32-bit halves into the single u64 DmabufFrame::modifier
            // uses, so this can be compared directly against DRM_FORMAT_MOD_LINEAR.
            let modifier = ((modifier_hi as u64) << 32) | modifier_lo as u64;
            state.formats.push((format, modifier));
        }
        // Any other event (including a bare `format`) is deliberately ignored; see above.
    }
}

/// Probe whether the compositor reachable via `conn`/`globals` advertises `XRGB8888` with
/// `DRM_FORMAT_MOD_LINEAR` on `zwp_linux_dmabuf_v1`.
///
/// This is the *compositor* half of SP3's two-part dmabuf capability check — the *producer* half
/// is [`FrameSource::supports_dmabuf`], checked independently by `present`. Both must hold before
/// the dmabuf path is used (see `present`'s doc comment).
///
/// Binds a **throwaway** instance of `zwp_linux_dmabuf_v1` on its own event queue (see
/// [`DmabufProbe`]'s doc comment for why a separate queue, rather than the real window's),
/// deliberately at **protocol version 3**: high enough that the `modifier` event (added at
/// version 3) is guaranteed to be sent, but *below* version 4, at which point the protocol XML
/// says compositors "must not" send `format`/`modifier` at all any more (superseded by the
/// heavier `zwp_linux_dmabuf_feedback_v1`/`format_table` mechanism, out of scope for SP3 — see
/// the module docs and the SP3 design doc's deferred-refinements list). A `roundtrip` after
/// binding is the protocol's own documented guarantee that every advertised format/modifier
/// pair has been delivered by the time this function returns.
///
/// # Errors
/// Returns `Err` with a human-readable reason (used directly in `present`'s fallback log line)
/// if the global cannot be bound at all — missing entirely, or the compositor's advertised
/// version is below 3 — or if the roundtrip itself fails (a connection-level problem).
fn compositor_supports_dmabuf_xrgb8888_linear(
    conn: &Connection,
    globals: &GlobalList,
) -> Result<bool, String> {
    // A queue+state that exists only for the duration of this probe; nothing outside this
    // function ever sees it.
    let mut probe_queue = conn.new_event_queue::<DmabufProbe>();
    let probe_qh = probe_queue.handle();
    let _dmabuf: ZwpLinuxDmabufV1 = globals
        .bind(&probe_qh, 3..=3, ())
        .map_err(|e| format!("compositor does not advertise zwp_linux_dmabuf_v1 v3+: {e}"))?;
    let mut probe = DmabufProbe::default();
    probe_queue
        .roundtrip(&mut probe)
        .map_err(|e| format!("Wayland roundtrip while probing dmabuf formats failed: {e}"))?;
    // `_dmabuf` (the probe-only binding) is dropped here along with `probe_queue`; if the
    // dmabuf path is chosen, `present` binds its OWN separate instance on the main queue —
    // rebinding the same global for a second, independent client-side object is valid Wayland
    // protocol (a global is not "used up" by one bind).
    Ok(probe
        .formats
        .contains(&(DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR)))
}

/// Open a Wayland window and present `source`'s frame, choosing at runtime between the
/// zero-copy `zwp_linux_dmabuf_v1` path (SP3) and SP1's `wl_shm` fallback, then keep it up
/// until the window is closed or `disconnect` reaches end-of-file — whichever comes first.
///
/// # The auto-detect / fallback split
/// Whether the dmabuf path is usable depends on two independent facts:
/// 1. **Can the producer export a dmabuf at all** — [`FrameSource::supports_dmabuf`]. For
///    `rayland-server` this is a GPU/driver-extension question decided when its `Renderer` was
///    constructed; for (c)1's `rayland-s` it is a structural `false` (spec §7.1 — S never sees the
///    application's render target, so there is nothing to export).
/// 2. **Does the compositor on the other end of THIS Wayland connection advertise `XRGB8888`
///    with `DRM_FORMAT_MOD_LINEAR`** on `zwp_linux_dmabuf_v1` — only knowable after connecting
///    and doing a protocol roundtrip (`compositor_supports_dmabuf_xrgb8888_linear`).
///
/// Because (2) can only be answered from inside this function, the *frame itself* — not just the
/// choice of buffer type — is produced here: this function calls whichever of
/// [`FrameSource::produce_dmabuf`] / [`FrameSource::produce_pixels`] matches the path it ends up
/// choosing. A caller cannot pre-produce a frame and hand it in, because which shape to produce is
/// itself part of the decision this function makes. See [`FrameSource`]'s doc comment.
///
/// `config.force_shm` skips the detection entirely and always takes the `wl_shm` path — this is
/// what the server's `--force-shm` flag maps to, letting the fallback be exercised deliberately
/// (e.g. for manual verification that it still works).
///
/// This function always prints exactly one line naming the chosen path and, on the fallback,
/// why: `presenting via dmabuf (zero-copy)` or `presenting via wl_shm (fallback: <reason>)`.
///
/// # `source`'s lifetime
/// `source` must stay alive (not be dropped) for as long as the window this function opens is on
/// screen — which, since this function blocks for exactly that long, is automatic for the duration
/// of the call. The reason it matters is the dmabuf path: the compositor is handed a live GPU
/// memory export whose *backing memory* the source owns (see [`FrameSource::produce_dmabuf`]), and
/// dropping the source while the compositor still holds that buffer would free memory out from
/// under it. Taking `&mut F` (rather than consuming it) keeps that the caller's responsibility to
/// arrange by simple scoping — see `rayland-server`'s `main.rs`.
///
/// # `disconnect`
/// Any source that (a) provides a file descriptor via [`AsFd`](std::os::fd::AsFd) and (b)
/// reaches end-of-file when the peer disconnects. SP1 passed a `TcpStream`; SP2/SP3 pass a
/// QUIC `Liveness`; (c)1's `rayland-s` passes one end of a `UnixStream` pair it keeps the other end
/// of forever, so that only closing the window ends the loop. **The source MUST already be
/// non-blocking** — this function does not set this itself, because `TcpStream::set_nonblocking`
/// is a TCP-specific call that does not exist on every possible source; callers are responsible
/// for constructing (or configuring) `disconnect` as non-blocking before passing it in. On return,
/// `disconnect` is dropped, which — for the QUIC `Liveness` — closes the connection so the client
/// also exits.
///
/// This runs one `calloop` event loop with two sources: the Wayland connection (window events)
/// and `disconnect` (liveness). Closing the window sets the exit flag; the peer disconnecting
/// is seen as end-of-stream on `disconnect` and also sets it. On return the loop and its
/// sources drop, dropping `disconnect`, which the peer observes as its side of the connection
/// closing. Unchanged from SP1/SP2's teardown contract.
///
/// # Errors
/// Returns an error if the compositor is unreachable (`WAYLAND_DISPLAY` unset or invalid), a
/// required global (`wl_compositor`, `wl_shm`, `xdg_wm_base`) is missing, producing the frame
/// fails, buffer allocation/creation fails, or the event loop errors.
pub fn present<F, S>(source: &mut F, config: &WindowConfig<'_>, disconnect: S) -> anyhow::Result<()>
where
    // Where the frame comes from, and the only thing that differs between this crate's callers.
    F: FrameSource,
    // `disconnect` must expose a raw fd so calloop's `Generic` source can register it for
    // readability polling — this is the trait that makes the source watchable at all.
    S: std::os::fd::AsFd,
    // calloop's `Generic` callback only ever hands the source back through a shared
    // reference (see the registration below), so reading must work through `&S`, not `S`
    // itself. `TcpStream`, `UnixStream` and the QUIC `Liveness` all implement `Read for &Self`.
    for<'a> &'a S: std::io::Read,
{
    // Connect to the compositor named by WAYLAND_DISPLAY.
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow::anyhow!("cannot connect to a Wayland compositor: {e}"))?;
    // Bootstrap the registry and get the initial event queue. `registry_queue_init` performs
    // its own internal roundtrip (via `conn.roundtrip()`, independent of our queue/state) to
    // populate `globals`, so `globals` is fully populated the moment this returns.
    let (globals, event_queue) = registry_queue_init(&conn)
        .map_err(|e| anyhow::anyhow!("Wayland registry initialization failed: {e}"))?;
    // A handle used to create protocol objects bound to our state type.
    let qh: QueueHandle<RaylandWindow> = event_queue.handle();

    // --- The dmabuf-vs-wl_shm decision (before anything else touches the main queue) ---
    //
    // The probe uses its OWN throwaway queue (see `compositor_supports_dmabuf_xrgb8888_linear`
    // and `DmabufProbe`'s doc comments) and so is safe to run here, before any surface exists
    // on the main queue — there is nothing yet for a stray `configure` event to race with.
    let (use_dmabuf, fallback_reason): (bool, Option<String>) = if config.force_shm {
        (false, Some("--force-shm was passed".to_string()))
    } else if !source.supports_dmabuf() {
        (
            false,
            Some("this frame source cannot export a dmabuf".to_string()),
        )
    } else {
        match compositor_supports_dmabuf_xrgb8888_linear(&conn, &globals) {
            Ok(true) => (true, None),
            Ok(false) => (
                false,
                Some(
                    "compositor does not advertise XRGB8888 with DRM_FORMAT_MOD_LINEAR on \
                     zwp_linux_dmabuf_v1"
                        .to_string(),
                ),
            ),
            Err(reason) => (false, Some(reason)),
        }
    };
    // Announce which path is active before producing the frame, so the log line is useful even
    // if the production step itself then fails.
    match &fallback_reason {
        None => println!("presenting via dmabuf (zero-copy)"),
        Some(reason) => println!("presenting via wl_shm (fallback: {reason})"),
    }

    // Create the calloop event loop that will drive everything.
    let mut event_loop: EventLoop<RaylandWindow> =
        EventLoop::try_new().map_err(|e| anyhow::anyhow!("failed to create event loop: {e}"))?;
    let loop_handle: LoopHandle<RaylandWindow> = event_loop.handle();

    // Feed Wayland events into the loop. Takes ownership of `event_queue`, so this happens
    // after the probe above (which used its own, separate queue) and after every other use of
    // `qh`-bound objects that needs `event_queue` to still be directly usable is done — in
    // practice here, since the remaining binds below only send requests, they need no further
    // direct queue access before the loop takes over dispatching.
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| anyhow::anyhow!("failed to insert the Wayland source: {e}"))?;

    // Bind the globals every path needs; a missing one is a clear, fatal error.
    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wl_compositor unavailable: {e}"))?;
    let xdg_shell = XdgShell::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("xdg_wm_base (window shell) unavailable: {e}"))?;
    // Bound unconditionally (even on the dmabuf path) — see `RaylandWindow::shm`'s doc comment
    // for why `ShmHandler` needs this regardless of which path actually draws.
    let shm = Shm::bind(&globals, &qh).map_err(|e| anyhow::anyhow!("wl_shm unavailable: {e}"))?;

    // Create the surface and give it the xdg_toplevel role (a normal window). Window creation
    // needs only the source's dimensions, not the produced frame, so it happens before the
    // production step below regardless of which path was chosen.
    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    // A human-readable title and a stable app id, both supplied by the caller: this crate serves
    // two different binaries and must not label one of them with the other's name.
    window.set_title(config.title);
    window.set_app_id(config.app_id);
    // Request a fixed size by pinning min == max to the frame's dimensions; compositors
    // commonly honour this by floating the window at exactly that size.
    window.set_min_size(Some((source.width(), source.height())));
    window.set_max_size(Some((source.width(), source.height())));
    // Initial commit with no buffer: the compositor replies with a configure, after which we
    // draw. This configure is only actually processed once the main loop starts dispatching
    // below (see the WaylandSource comment above) — by which time `presentation` is populated.
    window.commit();

    // --- Produce the frame via whichever path was chosen, and assemble that path's Presentation ---
    let presentation = if use_dmabuf {
        // The dmabuf export. `produce_dmabuf`'s contract requires the pixels to be final before it
        // returns, so the frame is safe to hand to the compositor with no further waiting.
        let frame = source
            .produce_dmabuf()
            .map_err(|e| anyhow::anyhow!("producing a dmabuf frame failed: {e}"))?;
        // Bind OUR OWN instance of zwp_linux_dmabuf_v1 on the main queue/state: the probe's
        // binding lived on a throwaway queue and is already gone (see
        // `compositor_supports_dmabuf_xrgb8888_linear`). A fresh bind here is required because
        // `draw` needs an object whose events dispatch through `RaylandWindow` specifically.
        let dmabuf_global: ZwpLinuxDmabufV1 = globals.bind(&qh, 3..=3, ()).map_err(|e| {
            anyhow::anyhow!(
                "zwp_linux_dmabuf_v1 disappeared between the capability probe and use: {e}"
            )
        })?;
        Presentation::Dmabuf {
            dmabuf_global,
            frame,
        }
    } else {
        // The CPU-pixel path (SP0/SP1's, and (c)1's only one), then a pool sized exactly for one
        // frame.
        let frame = source
            .produce_pixels()
            .map_err(|e| anyhow::anyhow!("producing a wl_shm frame failed: {e}"))?;
        let pool_size = frame.width as usize * frame.height as usize * 4;
        let pool = SlotPool::new(pool_size, &shm)
            .map_err(|e| anyhow::anyhow!("failed to create shm pool: {e}"))?;
        Presentation::Shm { pool, frame }
    };

    // Register `disconnect` as a liveness source: readable-then-zero-bytes means the peer
    // disconnected. The source must already be non-blocking (see the doc comment above) so
    // the callback never stalls the loop.
    loop_handle
        .insert_source(
            Generic::new(disconnect, Interest::READ, Mode::Level),
            |_readiness, source, state: &mut RaylandWindow| {
                // `source` is `&mut NoIoDrop<S>`, which derefs to `&S`; reading through `&S`
                // (rather than `S`) is exactly what the `for<'a> &'a S: Read` bound buys us,
                // and it avoids any unsafe access to the underlying fd.
                let mut reader: &S = source;
                let mut sink = [0u8; 256];
                loop {
                    match reader.read(&mut sink) {
                        // EOF: the peer is gone. Ask the loop to stop and remove this source.
                        Ok(0) => {
                            state.exit = true;
                            return Ok(PostAction::Remove);
                        }
                        // Unexpected bytes: no caller sends any, so ignore and keep draining.
                        Ok(_) => continue,
                        // Nothing more to read right now: leave the source in place.
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            return Ok(PostAction::Continue);
                        }
                        // A real error: treat like a disconnect and stop.
                        Err(_) => {
                            state.exit = true;
                            return Ok(PostAction::Remove);
                        }
                    }
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to watch the disconnect source: {e}"))?;

    // Assemble the state and run the loop until either trigger sets `exit`.
    let mut state = RaylandWindow {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        window,
        presentation,
        exit: false,
        first_configure: true,
        draw_error: None,
    };
    // Dispatch events, blocking until one arrives (`None` = no timeout); break out as soon
    // as `exit` is set. Blocking is fine because both teardown triggers — a window-close
    // event and readability on the disconnect source — wake the loop.
    while !state.exit {
        event_loop
            .dispatch(None, &mut state)
            .map_err(|e| anyhow::anyhow!("event loop dispatch failed: {e}"))?;
        // A compositor that refuses something we sent it does not reply "no" — it raises a
        // **protocol error** and destroys the connection. `dispatch` does not surface that as an
        // `Err` (the error arrives as an event, and the backend records it rather than failing the
        // dispatch that read it), so without this check the loop would go on waiting, on a dead
        // connection, for a close event that can now never arrive — and would then return `Ok(())`.
        //
        // (c)1 Task 7 found this by mutation: offering the compositor a format it will not take made
        // a real protocol error fire, the window stayed blank, and `present` still answered `Ok`.
        // That is the worst failure this crate can have — the caller believes it presented — and it
        // is why the check is here rather than left to a future reader to notice.
        if conn.protocol_error().is_some() {
            break;
        }
    }
    // The two failures that cannot be reported from inside the loop, now that it has ended. Order
    // matters only for which one a human sees first; a protocol error is the more fundamental of the
    // two (the connection is gone), so it wins.
    if let Some(e) = conn.protocol_error() {
        return Err(anyhow::anyhow!(
            "the compositor rejected what we sent and destroyed the connection: {} (object {}@{}, \
             code {}). Nothing was presented.",
            e.message,
            e.object_interface,
            e.object_id,
            e.code
        ));
    }
    // A local draw failure, stashed by `configure` because SCTK's callback cannot return one.
    if let Some(e) = state.draw_error.take() {
        return Err(e.context("presenting the frame failed while drawing it"));
    }
    // Returning drops the loop and its sources (closing `disconnect`, so the peer sees EOF
    // and exits) and drops `state` (including the dmabuf path's `DmabufFrame`, if any — its
    // `OwnedFd` closes here; the GPU memory it referred to is separately owned by `source`,
    // which the caller keeps alive independently — see this function's doc comment).
    Ok(())
}
