//! S-side presentation: turn a rendered frame into an on-screen Wayland window.
//!
//! SP0 rendered a triangle on the GPU and read it back into a [`RenderedFrame`] (tightly
//! packed RGBA8 in CPU memory). SP1 presented that frame by copying it into a shared-memory
//! (`wl_shm`) buffer and showing it in a real `xdg_toplevel` window. SP3 adds a second,
//! **zero-copy** presentation path: instead of a CPU round-trip through `wl_shm`, the GPU's
//! rendered image is exported as a Linux **dmabuf** (a kernel handle to live GPU memory) and
//! handed to the compositor directly via `zwp_linux_dmabuf_v1`. Both paths end up showing the
//! same triangle in the same kind of window; which one runs is decided at runtime by
//! [`present`] (see its doc comment for the full auto-detect/fallback story).
//!
//! The pure conversion [`pack_xrgb8888`] (used by the `wl_shm` path) is unit-tested here. The
//! SCTK-driven window ([`present`]) is integration code verified by building and by a manual
//! on-screen smoke check, because asserting real on-screen output needs a compositor.

// The rendered frame we present on the wl_shm path; its pixels are tightly-packed RGBA8.
use crate::render::RenderedFrame;
// The persistent renderer and its per-frame request type (SP3): `present` owns a `Renderer`
// across the whole window lifetime and decides, only once connected to the compositor, which
// of its two render methods to call.
use crate::render::{FrameRequest, Renderer};
// The dmabuf export types and the DRM format/modifier constants `present`'s capability probe
// checks the compositor against.
use crate::dmabuf::{self, DmabufFrame};

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
// The types below come from smithay-client-toolkit (SCTK) for the compositor/xdg/shm/output
// plumbing, and from `wayland-protocols` + raw `wayland-client` for `zwp_linux_dmabuf_v1`,
// which SCTK 0.20 has no first-class helper for (SP3 Task 4) — it is driven directly,
// alongside the existing SCTK delegate macros, on the same `RaylandWindow` state and the same
// event queue.

// `Read` is used generically in `present`'s bound and callback (the concrete disconnect
// source — `TcpStream` in SP1, the QUIC `Liveness` in SP2 — is supplied by the caller).
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

/// Which presentation backend is active for the on-screen buffer, decided once (in [`present`],
/// before the event loop starts) and never changed afterward — SP3 shows exactly one static
/// frame per process, so there is no notion of switching paths mid-run.
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
        /// The exported dmabuf (fd + layout) `Renderer::render_to_dmabuf` produced. Its
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
    // Set true to break the event loop: window closed or client disconnected.
    exit: bool,
    // True until the first configure, so we draw exactly once when the window is ready.
    first_configure: bool,
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
                // `Renderer::render_to_dmabuf` (it blocks the host until the GPU blit that
                // produced this dmabuf's contents has completed — see that function's doc
                // comment) — so it is safe to attach and commit immediately below with no
                // further synchronization on our side.
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
                // creates a second buffer to conflict with it (SP3 presents one static frame).
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
    // socket and lets the client exit.
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
            // A draw failure here is unexpected; surface it by requesting exit so the
            // process ends rather than hanging with a blank window.
            if self.draw(qh).is_err() {
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
    /// reuse or free it. SP3 presents exactly one static frame per process and never recycles
    /// buffers, so there is nothing to do when it arrives; the buffer's backing dmabuf fd (and
    /// the GPU memory behind it) is instead kept alive for the whole process lifetime by the
    /// `Renderer`/`DmabufFrame` `present` holds, and is only actually freed at process exit.
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
/// This is the *compositor* half of SP3's two-part dmabuf capability check — the *GPU* half is
/// [`crate::render::Renderer::supports_dmabuf`], checked independently by `present`. Both must
/// hold before the dmabuf path is used (see `present`'s doc comment).
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
        .contains(&(dmabuf::DRM_FORMAT_XRGB8888, dmabuf::DRM_FORMAT_MOD_LINEAR)))
}

/// Open a Wayland window and present `request`'s frame, choosing at runtime between the
/// zero-copy `zwp_linux_dmabuf_v1` path (SP3) and SP1's `wl_shm` fallback, then keep it up
/// until the window is closed or the remote peer disconnects — whichever comes first.
///
/// # The auto-detect / fallback split
/// Whether the dmabuf path is usable depends on two independent facts:
/// 1. **Does the local GPU + Vulkan driver support exporting a dmabuf** —
///    `renderer.supports_dmabuf()`. This is known before any Wayland connection exists (it was
///    decided when `renderer` was constructed).
/// 2. **Does the compositor on the other end of THIS Wayland connection advertise `XRGB8888`
///    with `DRM_FORMAT_MOD_LINEAR`** on `zwp_linux_dmabuf_v1` — only knowable after connecting
///    and doing a protocol roundtrip (`compositor_supports_dmabuf_xrgb8888_linear`).
///
/// Because (2) can only be answered from inside this function, the *rendering itself* — not
/// just the choice of buffer type — happens here: `request` is the not-yet-rendered frame
/// description, and this function calls whichever of `Renderer::render_to_dmabuf` /
/// `Renderer::render_to_frame` matches the path it ends up choosing. A caller cannot
/// pre-render and hand this function a finished frame, because which render method to call is
/// itself part of the decision this function makes.
///
/// `force_shm` skips the detection entirely and always takes the `wl_shm` path — this is what
/// the server's `--force-shm` flag maps to, letting the fallback be exercised deliberately
/// (e.g. for manual verification that it still works).
///
/// This function always prints exactly one line naming the chosen path and, on the fallback,
/// why: `presenting via dmabuf (zero-copy)` or `presenting via wl_shm (fallback: <reason>)`.
///
/// # `renderer`'s lifetime
/// `renderer` must be the SAME `Renderer` whose `supports_dmabuf()` answer decided this
/// function's path, and it must stay alive (not be dropped) for as long as the window this
/// function opens is on screen: the dmabuf path hands the compositor a live GPU memory export
/// owned by `renderer` (see `Renderer::render_to_dmabuf`'s own doc comment), and dropping
/// `renderer` while the compositor still holds that buffer would free memory out from under
/// it. Taking `&mut Renderer` (rather than consuming it) makes this the caller's
/// responsibility to arrange by simple scoping — see `main.rs`.
///
/// # `disconnect`
/// Any source that (a) provides a file descriptor via [`AsFd`](std::os::fd::AsFd) and (b)
/// reaches end-of-file when the peer disconnects. SP1 passed a `TcpStream`; SP2/SP3 pass a
/// QUIC `Liveness`. **The source MUST already be non-blocking** — this function does not set
/// this itself, because `TcpStream::set_nonblocking` is a TCP-specific call that does not
/// exist on every possible source; callers are responsible for constructing (or configuring)
/// `disconnect` as non-blocking before passing it in. On return, `disconnect` is dropped,
/// which — for the QUIC `Liveness` — closes the connection so the client also exits.
///
/// This runs one `calloop` event loop with two sources: the Wayland connection (window events)
/// and `disconnect` (liveness). Closing the window sets the exit flag; the peer disconnecting
/// is seen as end-of-stream on `disconnect` and also sets it. On return the loop and its
/// sources drop, dropping `disconnect`, which the peer observes as its side of the connection
/// closing. Unchanged from SP1/SP2's teardown contract.
///
/// # Errors
/// Returns an error if the compositor is unreachable (`WAYLAND_DISPLAY` unset or invalid), a
/// required global (`wl_compositor`, `wl_shm`, `xdg_wm_base`) is missing, the render step
/// fails, buffer allocation/creation fails, or the event loop errors.
pub fn present<S>(
    renderer: &mut Renderer,
    request: &FrameRequest,
    disconnect: S,
    force_shm: bool,
) -> anyhow::Result<()>
where
    // `disconnect` must expose a raw fd so calloop's `Generic` source can register it for
    // readability polling — this is the trait that makes the source watchable at all.
    S: std::os::fd::AsFd,
    // calloop's `Generic` callback only ever hands the source back through a shared
    // reference (see the registration below), so reading must work through `&S`, not `S`
    // itself. Both `TcpStream` and the QUIC `Liveness` implement `Read for &Self`.
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
    let (use_dmabuf, fallback_reason): (bool, Option<String>) = if force_shm {
        (false, Some("--force-shm was passed".to_string()))
    } else if !renderer.supports_dmabuf() {
        (
            false,
            Some(
                "this GPU/driver does not support the Vulkan dmabuf-export extensions".to_string(),
            ),
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
    // Announce which path is active before doing any GPU work, so the log line is useful even
    // if the render step itself then fails.
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
    // needs only `request`'s dimensions, not the rendered/exported frame, so it happens before
    // the render step below regardless of which path was chosen.
    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    // A human-readable title and a stable app id (provisional).
    window.set_title("Rayland — SP3");
    window.set_app_id("nl.rayland.Sp3");
    // Request a fixed size by pinning min == max to the frame's dimensions; compositors
    // commonly honour this by floating the window at exactly that size.
    window.set_min_size(Some((request.width, request.height)));
    window.set_max_size(Some((request.width, request.height)));
    // Initial commit with no buffer: the compositor replies with a configure, after which we
    // draw. This configure is only actually processed once the main loop starts dispatching
    // below (see the WaylandSource comment above) — by which time `presentation` is populated.
    window.commit();

    // --- Render via whichever path was chosen, and assemble that path's Presentation ---
    let presentation = if use_dmabuf {
        // The GPU render + dmabuf export. Renderer::render_to_dmabuf fence-waits internally, so
        // the returned frame's pixels are already final and safe to hand to the compositor.
        let frame = renderer
            .render_to_dmabuf(request)
            .map_err(|e| anyhow::anyhow!("dmabuf render failed: {e}"))?;
        // SP3 Task 1 review finding #3: release Vulkan queue-family ownership of the export
        // image to the compositor (a "foreign", non-Vulkan-to-us consumer) before handing it
        // the fd — see `Renderer::prepare_export_for_foreign_present`'s doc comment for the
        // full spec rationale. Treated as best-effort/non-fatal: several real compositor
        // combinations tolerate the barrier's absence for LINEAR dmabufs under implicit
        // synchronization (documented on that method and in the SP3 doc's known limitations),
        // and aborting presentation entirely over this would be worse than showing the frame
        // anyway and letting the human operator's on-screen check be the real verification.
        if let Err(e) = renderer.prepare_export_for_foreign_present() {
            eprintln!("warning: dmabuf foreign-ownership release failed ({e}); presenting anyway");
        }
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
        // The CPU-readback render (SP0/SP1 path), then a pool sized exactly for one frame.
        let frame = renderer
            .render_to_frame(request)
            .map_err(|e| anyhow::anyhow!("wl_shm render failed: {e}"))?;
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
                        // Unexpected bytes in SP1/SP2/SP3: ignore and keep draining.
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
    };
    // Dispatch events, blocking until one arrives (`None` = no timeout); break out as soon
    // as `exit` is set. Blocking is fine because both teardown triggers — a window-close
    // event and socket readability on client disconnect — wake the loop.
    while !state.exit {
        event_loop
            .dispatch(None, &mut state)
            .map_err(|e| anyhow::anyhow!("event loop dispatch failed: {e}"))?;
    }
    // Returning drops the loop and its sources (closing `disconnect`, so the client sees EOF
    // and exits) and drops `state` (including the dmabuf path's `DmabufFrame`, if any — its
    // `OwnedFd` closes here; the GPU memory it referred to is separately owned by `renderer`,
    // which the caller keeps alive independently — see this function's doc comment).
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
