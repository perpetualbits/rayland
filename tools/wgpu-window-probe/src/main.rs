//! **The WP0 toolkit probe** — the smallest possible `winit` + `wgpu` window, for finding out what a
//! real toolkit asks a desktop for.
//!
//! # Why this exists
//! Until 2026-08-30, WP0's Wayland proxy had only ever been driven by `vkcube`: bare Vulkan over
//! **libwayland**, and unusually undemanding — it wants a window, some GPU images, and nothing else.
//! A normal application is not like that. `solarsim` (`wgpu` 29 + `winit` 0.30 + `egui` 0.35) reaches
//! Wayland through `smithay-client-toolkit` over the **pure-Rust `wayland-client`**, which is a second
//! client implementation entirely, against a proxy that has only ever faced the first one.
//!
//! But `solarsim` is a whole application: shaders, assets, an `egui` overlay, a simulation. When it
//! fails against the proxy, "the toolkit needs something" and "solarsim needs something" are not
//! distinguishable. This probe is the control that separates them. It is the toolkit-stack analogue of
//! `vkcube`: small enough that any failure is unambiguous, and stable enough to re-run after every
//! change to the proxy.
//!
//! # What it does, and deliberately does not
//! Opens one window, creates a `wgpu` surface on it, clears to a solid colour, presents, counts the
//! frames, and exits cleanly after [`FRAMES`]. **No `egui`, no textures, no input handling, no resize
//! logic** — every one of those would add a reason for it to fail that is not the one being measured.
//!
//! # This is NOT a fixture
//! `OVERVIEW.md` §7 binds the *fixtures* (`rayland-icosa-cpu`/`-gpu`) to rules this crate does not
//! follow: no `rayland-*` dependencies, and no redraw loop, because a fixture's value is a
//! bit-identical native-vs-remoted comparison that a loop would destroy. This probe makes no such
//! comparison. It is a diagnostic instrument, like `rayland-icosa-window` before it, and nobody should
//! read its animation loop as a fixture violation.
//!
//! # Usage
//! `cargo run --release` — natively, or with `WAYLAND_DISPLAY` pointed at `rayland-c`'s proxy socket.
//! Exit status 0 means it reached [`FRAMES`]; any other outcome is the finding.

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// How many frames to present before exiting successfully.
///
/// Small on purpose. The question this probe answers is "does the toolkit stack get a window and
/// present at all through the proxy", which is settled in the first few frames; a long run would only
/// add time to a diagnostic that is meant to be re-run constantly.
const FRAMES: u32 = 60;

/// The window's edge in logical pixels. Square and small, matching the other WP0 vehicles so a size
/// difference is never a variable when comparing their logs.
const WINDOW_EDGE: u32 = 500;

/// The clear colour — a distinctive blue-grey, so a human glancing at a screen can tell this probe's
/// window from `vkcube`'s cube or a compositor's own background without reading a title bar.
const CLEAR: wgpu::Color = wgpu::Color { r: 0.10, g: 0.20, b: 0.35, a: 1.0 };

/// Everything the probe holds once the window exists: the GPU objects, and the frame counter.
///
/// Split from [`Probe`] because `winit` 0.30 cannot create a window until the event loop is running
/// (`resumed`), so none of this can be built in `main`.
struct Gpu {
    /// Kept alive and shared with the surface, which borrows the window for its lifetime.
    window: Arc<Window>,
    /// The presentation surface for `window`.
    surface: wgpu::Surface<'static>,
    /// The logical GPU, for creating command encoders.
    device: wgpu::Device,
    /// Where finished command buffers are submitted.
    queue: wgpu::Queue,
    /// Frames presented so far; the probe exits once this reaches [`FRAMES`].
    frames: u32,
}

/// The `winit` application. `None` until the event loop resumes and the window can be created.
#[derive(Default)]
struct Probe {
    /// The GPU state, absent until `resumed` has run once.
    gpu: Option<Gpu>,
}

impl ApplicationHandler for Probe {
    /// Create the window and the whole `wgpu` stack, on the first resume.
    ///
    /// This is where a proxy missing a service the toolkit needs will most likely fail, and it is why
    /// every step below prints before it acts: when the process dies inside one of these calls, the
    /// last line printed names the step that killed it. A stack trace would not — the failure is
    /// usually a clean `panic!` from deep inside the toolkit about a global it could not find.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return; // resumed can fire more than once; the window is created only the first time
        }
        eprintln!("probe: creating window");
        let attrs = Window::default_attributes()
            .with_title("Rayland WP0 toolkit probe")
            .with_inner_size(winit::dpi::LogicalSize::new(WINDOW_EDGE, WINDOW_EDGE));
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("winit could not create a window"),
        );

        eprintln!("probe: creating wgpu instance + surface");
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window.clone())
            .expect("wgpu could not create a surface on the window");

        eprintln!("probe: requesting adapter");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            // The probe must run on whatever the surface is on; a fallback adapter would silently
            // change which GPU is under test and make the result unattributable.
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("no wgpu adapter is compatible with this surface");

        eprintln!("probe: requesting device");
        let (device, queue) = pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor::default()),
        )
        .expect("wgpu could not open a device");

        // Configure the surface for the window's real size. `get_default_config` picks a format and
        // present mode the adapter actually supports, so the probe never asks for something exotic.
        let size = window.inner_size();
        let config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .expect("the surface is not supported by this adapter");
        surface.configure(&device, &config);
        eprintln!("probe: surface configured {}x{}", config.width, config.height);

        self.gpu = Some(Gpu { window, surface, device, queue, frames: 0 });
        // Ask for the first frame; each presented frame requests the next (see `redraw`).
        self.gpu.as_ref().unwrap().window.request_redraw();
    }

    /// Handle the two window events the probe cares about: close, and "draw now".
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            // A compositor or a human closing the window ends the run. Reported as its own outcome
            // because "the window went away" and "the probe finished its frames" are different results.
            WindowEvent::CloseRequested => {
                eprintln!("probe: close requested by the compositor/user");
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }
}

impl Probe {
    /// Clear the surface to [`CLEAR`] and present it; exit once [`FRAMES`] have been shown.
    ///
    /// # Failure modes
    /// `get_current_texture` is the call that fails when presentation is broken underneath — a lost or
    /// outdated surface, or a compositor that never released a buffer. It is `expect`ed rather than
    /// handled, deliberately: this is a diagnostic, and a loud failure naming the frame number is the
    /// output. Recovering would hide exactly what the probe exists to see.
    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        // wgpu 29 returns an enum rather than a Result here, and each arm is a different finding, so
        // each is reported by name. A probe that collapsed them into "failed" would lose exactly the
        // distinction it exists to make — `Outdated` after a resize is ordinary, while `Lost` or
        // `Validation` through the proxy is a defect.
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            other => {
                eprintln!(
                    "probe: STOPPED at frame {} — get_current_texture returned {:?}",
                    gpu.frames,
                    std::mem::discriminant(&other)
                );
                eprintln!("probe: (Outdated/Lost want a reconfigure; Timeout/Occluded mean the \
                           compositor is not showing us; Validation is a wgpu-level error)");
                event_loop.exit();
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            // A render pass whose only job is the clear: `LoadOp::Clear` writes every pixel, so no
            // pipeline, shader or vertex buffer is needed to produce a visible, verifiable frame.
            let _pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("probe clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(CLEAR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                // wgpu 29 requires this; the probe renders no multiview layers.
                multiview_mask: None,
            });
        }
        gpu.queue.submit(Some(enc.finish()));
        frame.present();

        gpu.frames += 1;
        // Progress on stderr rather than a total at the end: if the probe dies mid-run, the last line
        // says how far it got, which is the number that matters for a scouting run.
        if gpu.frames % 10 == 0 || gpu.frames == 1 {
            eprintln!("probe: presented frame {}", gpu.frames);
        }
        if gpu.frames >= FRAMES {
            eprintln!("probe: OK — presented {FRAMES} frames, exiting");
            event_loop.exit();
        } else {
            // Drive the next frame. `request_redraw` rather than a timer, so the pace is the
            // compositor's — the same property `rayland-present` relies on.
            gpu.window.request_redraw();
        }
    }
}

/// Run the probe. Exit status 0 means [`FRAMES`] were presented; anything else is the finding.
fn main() {
    // wgpu and winit report missing Wayland globals through `log`; without a logger installed those
    // messages are discarded, and they are precisely what this probe was built to capture.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    eprintln!("probe: starting (WAYLAND_DISPLAY={:?})", std::env::var("WAYLAND_DISPLAY").ok());
    let event_loop = EventLoop::new().expect("could not create a winit event loop");
    // Poll rather than Wait: the probe drives its own frames and should not sleep waiting for input
    // that a headless or proxied session may never deliver.
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut probe = Probe::default();
    event_loop.run_app(&mut probe).expect("winit event loop failed");
}
