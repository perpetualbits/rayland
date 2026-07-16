//! S-side presentation for the SP-era server: hand `rayland-present` a live [`Renderer`].
//!
//! SP0 rendered a triangle on the GPU and read it back into a [`RenderedFrame`] (tightly packed
//! RGBA8 in CPU memory). SP1 presented that frame by copying it into a shared-memory (`wl_shm`)
//! buffer and showing it in a real `xdg_toplevel` window. SP3 added a second, **zero-copy** path:
//! the GPU's rendered image is exported as a Linux **dmabuf** (a kernel handle to live GPU memory)
//! and handed to the compositor directly via `zwp_linux_dmabuf_v1`.
//!
//! # What is left in this file, and what moved
//! **All of that machinery now lives in [`rayland_present`]**, extracted by (c)1 Task 7 because a
//! second binary — `rayland-s`, which presents a frame rendered on a GPU a network away — needs the
//! identical code, and two copies of ~700 lines of Wayland plumbing would have drifted.
//!
//! What is left here is the part that is genuinely `rayland-server`'s: the **adapter** that lets a
//! live Vulkan [`Renderer`] play the role of a [`FrameSource`]. That is the whole seam. The Vulkan
//! knowledge (which render method to call, and SP3's foreign-ownership release) stays on this side
//! of it; the Wayland knowledge (which *kind* of frame the compositor can take) stays on that side.
//! Neither crate needs the other's.
//!
//! Behaviour is unchanged from SP3: same two paths, same auto-detection, same `--force-shm`
//! override, same log lines, same teardown contract. The extraction is deliberately
//! behaviour-preserving, and `tests/e2e.rs`, `tests/quic_e2e.rs`, `tests/render.rs` and
//! `tests/handle.rs` are the regression net that says so.

// The rendered frame we present on the wl_shm path; its pixels are tightly-packed RGBA8.
use crate::render::RenderedFrame;
// The persistent renderer and its per-frame request type (SP3): `present` owns a `Renderer`
// across the whole window lifetime and decides, only once connected to the compositor, which
// of its two render methods to call.
use crate::render::{FrameRequest, Renderer};
// The dmabuf export description the zero-copy path produces.
use crate::dmabuf::DmabufFrame;
// The extracted presenter: the Wayland window, the capability probe, and the seam between them.
use rayland_present::{FrameSource, WindowConfig};

// The pure RGBA8 -> Xrgb8888 swizzle, which moved to `rayland-present` alongside the `wl_shm` path
// that is its only caller (and where its unit test moved with it). Re-exported so it stays part of
// this module's published surface, as it has been since SP1.
pub use rayland_present::pack_xrgb8888;

/// The window title the SP-era server labels its window with. Human-facing.
const WINDOW_TITLE: &str = "Rayland — SP3";

/// The stable application id the SP-era server claims. Not human-facing; compositors use it to
/// group windows and match desktop entries.
const WINDOW_APP_ID: &str = "nl.rayland.Sp3";

/// A live Vulkan [`Renderer`] plus the frame it has been asked to draw, presented as a
/// [`FrameSource`].
///
/// # Why this adapter exists rather than a `impl FrameSource for Renderer`
/// A `Renderer` alone cannot answer "how big is the frame?" or produce one: SP3's `Renderer`
/// deliberately takes no size at construction, and the size lives in the [`FrameRequest`]. So the
/// thing that is a frame source is the **pair**, and this struct is that pair — which is also why
/// it borrows both rather than owning either: the caller must keep the `Renderer` alive past the
/// window (see [`present`]), and the request is the caller's parsed stream.
struct RendererSource<'a> {
    /// The GPU. Borrowed mutably because both render methods need `&mut`.
    renderer: &'a mut Renderer,
    /// What to draw, and at what size.
    request: &'a FrameRequest,
}

impl FrameSource for RendererSource<'_> {
    /// The request's width — the size the window is fixed to, read before any render happens.
    fn width(&self) -> u32 {
        self.request.width
    }

    /// The request's height. See [`Self::width`].
    fn height(&self) -> u32 {
        self.request.height
    }

    /// The *GPU* half of SP3's two-part dmabuf capability check: does this device/driver have the
    /// three extensions the export path needs (see [`crate::dmabuf::required_device_extensions`])?
    ///
    /// Answered from state decided when `renderer` was constructed, so it costs nothing here. The
    /// *compositor* half is `rayland-present`'s own probe, and both must hold.
    fn supports_dmabuf(&self) -> bool {
        self.renderer.supports_dmabuf()
    }

    /// Render into an exportable LINEAR `B8G8R8A8` image and export it as a dmabuf fd.
    ///
    /// `Renderer::render_to_dmabuf` fence-waits internally, so the returned frame's pixels are
    /// already final — which is exactly what [`FrameSource::produce_dmabuf`]'s contract requires.
    ///
    /// # The best-effort ownership release
    /// SP3 Task 1 review finding #3: Vulkan queue-family ownership of the export image must be
    /// released to the compositor (a "foreign", non-Vulkan-to-us consumer) before it is handed the
    /// fd — see `Renderer::prepare_export_for_foreign_present`'s doc comment for the full spec
    /// rationale. It is treated as **non-fatal**, exactly as it was before the extraction: several
    /// real compositor combinations tolerate the barrier's absence for LINEAR dmabufs under
    /// implicit synchronization (documented on that method and in the SP3 doc's known limitations),
    /// and aborting presentation entirely over this would be worse than showing the frame anyway
    /// and letting the human operator's on-screen check be the real verification.
    ///
    /// It lives *here*, inside the adapter, rather than in the presenter: it is a Vulkan
    /// synchronization step about a Vulkan-owned image, and `rayland-present` neither can nor
    /// should know it exists. Folding it into this method is what let the presenter's trait stay
    /// free of it.
    ///
    /// # Errors
    /// Returns an error if the render or the export itself fails. A failed *ownership release* is
    /// not an error — see above.
    fn produce_dmabuf(&mut self) -> anyhow::Result<DmabufFrame> {
        let frame = self
            .renderer
            .render_to_dmabuf(self.request)
            .map_err(|e| anyhow::anyhow!("dmabuf render failed: {e}"))?;
        if let Err(e) = self.renderer.prepare_export_for_foreign_present() {
            eprintln!("warning: dmabuf foreign-ownership release failed ({e}); presenting anyway");
        }
        Ok(frame)
    }

    /// Render via the ordinary CPU-readback path (SP0/SP1's), yielding tightly-packed RGBA8.
    ///
    /// # Errors
    /// Returns an error if the GPU render or the readback fails.
    fn produce_pixels(&mut self) -> anyhow::Result<RenderedFrame> {
        self.renderer
            .render_to_frame(self.request)
            .map_err(|e| anyhow::anyhow!("wl_shm render failed: {e}"))
    }
}

/// Open a Wayland window and present `request`'s frame, choosing at runtime between the zero-copy
/// `zwp_linux_dmabuf_v1` path (SP3) and SP1's `wl_shm` fallback, then keep it up until the window is
/// closed or the remote peer disconnects — whichever comes first.
///
/// Since (c)1 Task 7 this is a thin adapter over [`rayland_present::present`], which holds every
/// line of the window, the probe, the two draw paths and the teardown loop. **The full account of
/// the auto-detect/fallback split, the log lines it prints, and `disconnect`'s contract is on that
/// function's doc comment** and is not duplicated here, because a second copy would go stale — which
/// is the same reason the code itself is not duplicated. What this wrapper adds is: SP3's window
/// title/app id, and the [`RendererSource`] adapter that lets a Vulkan `Renderer` be a frame source.
///
/// # `renderer`'s lifetime
/// `renderer` must be the SAME `Renderer` whose `supports_dmabuf()` answer decides this
/// function's path, and it must stay alive (not be dropped) for as long as the window this
/// function opens is on screen: the dmabuf path hands the compositor a live GPU memory export
/// owned by `renderer` (see `Renderer::render_to_dmabuf`'s own doc comment), and dropping
/// `renderer` while the compositor still holds that buffer would free memory out from under
/// it. Taking `&mut Renderer` (rather than consuming it) makes this the caller's
/// responsibility to arrange by simple scoping — see `main.rs`.
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
    // calloop's `Generic` callback only ever hands the source back through a shared reference, so
    // reading must work through `&S`, not `S` itself. Both `TcpStream` and the QUIC `Liveness`
    // implement `Read for &Self`.
    for<'a> &'a S: std::io::Read,
{
    // Pair the GPU with what it has been asked to draw; that pair is what the presenter can drive.
    let mut source = RendererSource { renderer, request };
    // The window's identity, and SP3's deliberate-fallback flag, are all this crate contributes
    // beyond the source itself.
    let config = WindowConfig {
        title: WINDOW_TITLE,
        app_id: WINDOW_APP_ID,
        force_shm,
    };
    rayland_present::present(&mut source, &config, disconnect)
}
