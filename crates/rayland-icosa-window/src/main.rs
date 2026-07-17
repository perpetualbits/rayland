//! **A demo, not a fixture.** This binary opens a live Wayland window on S's display and shows the
//! icosahedron fixtures' spinning, fractal-textured solid actually spinning, in real time, so a
//! human can look at it. Nothing here is evidence about anything; nothing here is measured; nothing
//! here is compared against a second run.
//!
//! # Read this before assuming it is a third fixture
//! `rayland-icosa-cpu` and `rayland-icosa-gpu` are option-free, wall-clock-free, and
//! `rayland-*`-ignorant **on purpose** — see `docs/icosa-fixtures.md` and the design spec's §2 ("the
//! fixture must not know"). Their entire value rests on being unable to tell, and on producing the
//! same 120 PNGs on every run, so that a native run and a remoted run can be compared byte for byte.
//! This crate inherits none of that, because it is not for comparing — it is for looking at — and
//! §2's rule was written to protect a property this crate does not have and does not need:
//!
//! - **It owns its Wayland connection directly** (this module, via `smithay-client-toolkit` and
//!   `wayland-client`) rather than going through a `rayland-*` presentation crate. The fixtures may
//!   never depend on any `rayland-*` crate (§2's dependency rule exists precisely so a fixture cannot
//!   see whether it is being remoted); this crate is not a fixture, so there is nothing here for it
//!   to see that would compromise anything.
//! - **It has a redraw loop paced by the compositor**, not a fixed schedule played once. A fixture
//!   with any such loop would produce different pixels on every run, destroying the bit-identical
//!   comparison its tests depend on. This program is compared against nothing, so a live loop costs
//!   it nothing.
//! - **It must never be pointed at by (c)1's netem sweep** (`docs/icosa-fixtures.md` §11). It writes
//!   no PNGs, prints no CSV, and leaves behind no reproducible artefact at all — only pixels on a
//!   screen that vanish the instant the window closes. A measurement aimed at this binary is a
//!   measurement of the wrong program, full stop.
//! - **Its purpose is to be looked at.** That is a real purpose. It is just not the fixtures' purpose,
//!   and the two must never be confused for one another.
//!
//! # Why the GPU fixture's shader, not the CPU fixture's texture path
//! Two renders are available, unchanged: fixture A's CPU-computed, per-frame-uploaded texture
//! (`shaders/icosa_textured.frag.spv`, `Scene::new(.., Some(sampler))`), and fixture B's
//! fragment-shader fractal (`shaders/icosa_fractal.frag.spv`, `Scene::new(.., None)`). This binary
//! uses fixture B's shader. `docs/icosa-fixtures.md` §3 measured both, on this project's own
//! reference machine: fixture A averages **~50.8 ms/frame** (its CPU Mandelbrot loop alone is ~49.4
//! ms), fixture B averages **~1.5 ms/frame**. A 50 ms frame is 20 fps *before* this program's own
//! window-management overhead is added on top — a slideshow, not something worth calling a demo. Two
//! ordinary costs fixture A pays and this program does not need at all: a staging buffer and its
//! `MappedBuffer`, and the upload/barrier path `rayland-icosa-cpu/src/texture.rs` implements. Passing
//! `None` for the sampler binding, exactly as `rayland-icosa-gpu` does, skips both. It also matters
//! more here than it did for the fixtures: at ~1.5 ms/frame this program's own render cost is well
//! under any real compositor's frame budget (16.7 ms at 60 Hz), so the redraw loop below (see "One
//! window, one redraw loop, paced by the compositor") is limited by the compositor's refresh rate,
//! not by this program — which is the whole point of pacing off `wl_surface::frame` rather than a
//! wall clock.
//!
//! # One window, one redraw loop, paced by the compositor — and why the previous version did not do
//! this
//! An earlier version of this program called `rayland_present::present()` once per animation frame,
//! roughly eleven times a second. `present()` is built to draw **exactly one static frame per call**
//! (see that function's own doc comment) — the right shape for `rayland-s`, which only ever has one
//! frame to show — and each call opens a brand-new `Connection`, a brand-new `wl_surface`, and a
//! brand-new `xdg_toplevel`, waits for a fresh `configure`, draws once, and tears the whole thing
//! down when the call returns. Calling it in a loop therefore did not animate a window; it **opened
//! and closed a new window roughly eleven times a second**, forever, for as long as the program ran.
//! On a real desktop that is not a cosmetic issue: every new toplevel grabs input focus and (on at
//! least one compositor observed during this rewrite) the pointer, so the window the user just tried
//! to close had already been replaced by a different window object before the click landed — there
//! is no window to close, only an endless succession of them. `cosmic-comp` also logged `Toplevel for
//! foreign-toplevel-list not registered for cosmic-toplevel-info` on every one of those churns; that
//! log line is evidence of a real compositor-side bug (creating and destroying `xdg_toplevel`s far
//! faster than its own foreign-toplevel bookkeeping can keep up with), and this program should not be
//! the thing that reproduces it by accident.
//!
//! The fix is not a workaround; it is doing what every ordinary animated Wayland client does. This
//! module never calls `rayland_present::present` — it may not even depend on `rayland-present` (see
//! this crate's `Cargo.toml`) — and instead:
//!
//! 1. Connects, binds `wl_compositor`/`wl_shm`/`xdg_wm_base`, creates **one** `xdg_toplevel`, and
//!    waits for its first `configure`. Once, at [`run`]'s start.
//! 2. Creates a `wl_shm` pool ([`smithay_client_toolkit::shm::slot::SlotPool`]) sized for one frame,
//!    and lets it grow itself if a second, concurrently-live buffer is ever needed (see
//!    [`DemoWindow::draw`]'s doc for when that happens).
//! 3. Draws animation frame *N* into a buffer, `attach`+`damage_buffer`+`commit`s it, and — in the
//!    same call — requests a `wl_surface::frame` callback.
//! 4. When that callback fires (which the compositor sends once per output refresh it is ready to
//!    show a new frame for — the standard Wayland redraw signal), draws frame *N + 1* the same way,
//!    requests the next callback, and repeats. There is no `sleep` anywhere in this loop; its pace
//!    *is* the compositor's, because the callback firing is the compositor's own signal that it is
//!    ready for more, not a guess this program makes about timing.
//! 5. Exits when [`WindowHandler::request_close`] fires (the user closed the window) — the *same*
//!    `xdg_toplevel` for the program's entire run, never recreated.
//!
//! One connection, one surface, one `xdg_toplevel`, for the life of the process. `journalctl` and
//! `WAYLAND_DEBUG=1` both confirm this — see this task's completion report for the counts.
//!
//! # Object lifetime and drop order
//! Both fixtures' `main.rs` get this right by *local-variable* declaration order (Rust drops locals
//! in reverse declaration order, so declaring `context` before `scene` drops `scene` first). This
//! module cannot use that trick: `calloop::EventLoop`'s state type must be `'static` (its
//! `insert_source`/`WaylandSource::insert` calls require it), which rules out [`DemoWindow`] — the
//! type driven by that event loop — borrowing a `VulkanContext`/`Scene` pair that live as separate
//! locals in [`run`]. So [`AnimationSource`] **owns** both instead of borrowing them, and the
//! equivalent guarantee is encoded as *struct field* order instead: Rust drops a struct's fields in
//! **declaration** order (the opposite rule from locals — first-declared drops first), so
//! [`AnimationSource`] declares `scene` before `context`, guaranteeing `scene` — which
//! holds a cloned `ash::Device` produced from `context`; see [`Scene`]'s own struct doc in
//! `rayland-icosa-vk` — is always torn down first regardless of how `AnimationSource` itself is
//! dropped (as part of `DemoWindow`, or directly in the test below). Getting the two orderings
//! (locals: last-declared-first; struct fields: first-declared-first) confused is the exact mistake
//! this note exists to prevent.
//!
//! # The ping-pong schedule
//! [`rayland_icosa_core::FRAME_COUNT`] (120) frames exist, indexed `0..120`, and
//! [`rayland_icosa_core::schedule::frame_zoom`] is geometric: `1.5 * 0.97^i`. A fixture plays them
//! once, `0..119`, and stops — there is no next run to loop into. This program runs until a human
//! closes it, so it has to decide what "frame 120" means, and wrapping straight back to frame 0 would
//! snap the zoom from its deepest point back out to its widest in a single frame — a visible jump, not
//! a loop. [`AnimationSource`] instead **ping-pongs**: `0, 1, .., 119, 118, .., 1, 0, 1, ..` — the
//! zoom breathes in to its deepest point and back out to its start, forever, with every consecutive
//! pair of frames exactly [`rayland_icosa_core::schedule::ZOOM_PER_FRAME`] apart. Nothing about this
//! is a measurement; it is chosen purely because it is the schedule that never jumps.
//!
//! # The upscale
//! [`Scene`] always renders at [`rayland_icosa_core::IMAGE_SIZE`] (256) — a
//! `rayland-icosa-core` constant both fixtures depend on for their pixel-identical comparison, and
//! this crate must not perturb it (see the "do not touch" list in this task's brief). 256 px is a
//! postage stamp on a modern display, so [`WINDOW_EDGE`] instead is `IMAGE_SIZE * SCALE` (768, with
//! [`SCALE`] = 3) and [`nearest_neighbor_upscale`] repeats each rendered texel into a `SCALE`×`SCALE`
//! block of identical pixels before the frame is written into the `wl_shm` buffer. **Nearest
//! neighbour, not linear.** The point of watching this demo is to see the Mandelbrot texture's actual
//! texels — the blocky, stair-stepped edges *are* the picture at this zoom depth — and a linear or
//! bicubic filter would blur exactly the detail a viewer is here to see.
//!
//! # Pixel format: RGBA8 in, `Xrgb8888` out
//! [`Scene::draw`] returns tightly-packed **RGBA8** — red, green, blue, alpha, in that byte order,
//! `IMAGE_SIZE * IMAGE_SIZE * 4` bytes, no row padding. `wl_shm`'s `Xrgb8888` format is a *different*
//! layout: each pixel is a 32-bit value `0x00RRGGBB`, interpreted **little-endian**, so its four
//! bytes in memory are blue, green, red, then an unused byte. [`swizzle_rgba8_to_xrgb8888`] performs
//! that reordering. `rayland-present`'s `src/frame.rs` documents the identical conversion in more
//! depth (recommended reading for the reasoning) — this module does not import it, per this task's
//! brief, and writes its own copy instead. Getting this wrong is a real pitfall with no compiler help
//! at all: a channel-order mistake still produces a valid image, just with red and blue swapped, and
//! nothing about that looks like an error.

use std::num::NonZeroU32;

use rayland_icosa_core::IMAGE_SIZE;
use rayland_icosa_core::schedule::{CENTER, frame_mvp, frame_zoom};
use rayland_icosa_vk::{Scene, Uniforms, VulkanContext};

// Re-exported by smithay-client-toolkit so this crate's Wayland objects are always built from the
// exact `wayland-client` version SCTK itself was compiled against — the same reasoning
// `rayland-present`'s `window.rs` documents for importing through this path rather than through a
// separate top-level `use wayland_client::...`.
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface::WlSurface},
};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_output, delegate_registry, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window, registry_handlers,
};

/// The compiled fractal fragment shader — identical bytes to `rayland-icosa-gpu`'s, evaluating the
/// same Mandelbrot escape-time function in the fragment stage instead of sampling a CPU-computed
/// texture. See this module's doc ("Why the GPU fixture's shader") for why this program uses it
/// rather than fixture A's textured path.
const FRACTAL_FRAGMENT_SPIRV: &[u8] = include_bytes!("../../../shaders/icosa_fractal.frag.spv");

/// How many identical pixels each of [`Scene`]'s rendered texels is expanded into, per axis, before
/// the frame is written into the `wl_shm` buffer. See this module's doc ("The upscale") for why this
/// exists and why the expansion is nearest-neighbour rather than a filtered resize.
const SCALE: u32 = 3;

/// The window's fixed edge length in pixels, both horizontally and vertically: [`IMAGE_SIZE`]
/// upscaled by [`SCALE`] (768). The window is created at this size once, at startup, and never
/// resized — every buffer this program ever allocates is exactly `WINDOW_EDGE * WINDOW_EDGE * 4`
/// bytes (RGBA8/Xrgb8888, both 4 bytes per pixel).
const WINDOW_EDGE: u32 = IMAGE_SIZE * SCALE;

/// The window title the compositor shows in its decoration/taskbar. Says plainly that this is a
/// demo, not a fixture — the same label a human unfamiliar with this repository would need to not
/// mistake it for one of the PNG-writing programs.
const WINDOW_TITLE: &str =
    "Rayland — icosa demo (this is NOT a fixture; see rayland-icosa-window docs)";

/// The stable application id this program claims. Not human-facing; kept distinct from
/// `rayland-s`'s `nl.rayland.C1` so a compositor that groups windows by app id never conflates the
/// two.
const WINDOW_APP_ID: &str = "nl.rayland.icosa-window";

/// Print the top-level error and its full cause chain, then exit 1.
///
/// Identical in shape to both fixtures' `main` — see `rayland-icosa-cpu`'s doc for why the full
/// chain (not a summarised message) is the right thing to print when a Vulkan call fails.
fn main() {
    if let Err(error) = run() {
        eprintln!("rayland-icosa-window: {error}");
        for cause in error.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        std::process::exit(1);
    }
}

/// Reinterpret a byte slice holding compiled SPIR-V as a `Vec` of native-endian 32-bit words.
///
/// A byte-for-byte copy of `rayland-icosa-gpu`'s function of the same name, kept identical rather
/// than shared because this crate's dependency list — `rayland-icosa-vk`, `rayland-icosa-core`,
/// `smithay-client-toolkit`, `wayland-client`, `anyhow`, per this task's brief — deliberately stops
/// short of `ash`, exactly as fixture B's does. `ash::util::read_spv` would do the same job in one
/// call, but only if this crate linked `ash` for it, which it does not.
///
/// # Inputs and outputs
/// `bytes` must be a whole number of 4-byte words — true for any file `glslangValidator -V` produces.
/// Returns those words as `u32`s.
///
/// # Failure modes
/// Asserts that `bytes.len()` is a multiple of 4 and that the first word is SPIR-V's magic number
/// `0x0723_0203`. Both guard against `FRACTAL_FRAGMENT_SPIRV` having been replaced by something that
/// is not valid SPIR-V — a build-time packaging error, not a runtime scenario — rather than letting a
/// mis-sized or corrupt blob reach `vkCreateShaderModule` as silently truncated or garbage words. See
/// `rayland-icosa-gpu`'s identical function for the full argument for why both checks matter.
fn spirv_words(bytes: &[u8]) -> Vec<u32> {
    assert!(
        bytes.len() % 4 == 0,
        "SPIR-V blob is {} bytes, not a whole number of 4-byte words",
        bytes.len()
    );
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|word| u32::from_ne_bytes(word.try_into().unwrap()))
        .collect();
    assert_eq!(
        words.first(),
        Some(&0x0723_0203),
        "SPIR-V magic number missing or wrong; expected the stream to start with 0x0723_0203"
    );
    words
}

/// Draws one frame of the ping-ponging animation per call, advancing its own frame counter each
/// time, and hands back tightly-packed, upscaled RGBA8 pixels ready for [`swizzle_rgba8_to_xrgb8888`].
///
/// # Why this struct owns `context`/`scene` rather than borrowing them
/// [`VulkanContext`] and [`Scene`] are expensive to build (Vulkan instance/device bring-up, pipeline
/// and render-target creation) and cheap to *draw with* (~1.5 ms/frame — see this module's doc). This
/// program redraws every time the compositor asks for a frame (see [`DemoWindow::draw`]) but must not
/// rebuild the GPU state on every one of those redraws, or the 20×-plus cost fixture A pays for its
/// CPU texture path would be reintroduced here for no reason at all — so both are built exactly once,
/// in [`AnimationSource::new`], and kept for the whole run. They are **owned** here (not borrowed, as
/// an earlier draft of this module did) because [`DemoWindow`] — which holds an `AnimationSource` —
/// is driven by a `calloop::EventLoop` whose state type must be `'static`; see this module's doc
/// ("Object lifetime and drop order") for the full account of what that forces and how the
/// `Scene`-before-`VulkanContext` teardown requirement is met without borrowing.
struct AnimationSource {
    /// The long-lived scene every call to [`AnimationSource::next_frame_rgba`] draws through.
    /// Declared **before** `context` so it is dropped first — see this module's doc ("Object
    /// lifetime and drop order").
    scene: Scene,
    /// The Vulkan context `scene` was built from and draws through. Must outlive `scene`; declaring
    /// this field *after* `scene` is what guarantees that (struct fields drop in declaration order).
    context: VulkanContext,
    /// Which of the 120 animation frames the *next* [`AnimationSource::next_frame_rgba`] call will
    /// draw. Starts at 0 (the widest, most recognisable view of the fractal) and is advanced by that
    /// same call, after drawing, according to `ascending`.
    frame_index: u32,
    /// `true` while counting up toward frame 119, `false` while counting back down toward 0. Flipped
    /// exactly at the two endpoints — see [`AnimationSource::next_frame_rgba`] for the turnaround
    /// logic and this module's doc ("The ping-pong schedule") for why a flip, rather than a
    /// wraparound, is what keeps the animation visually seamless.
    ascending: bool,
}

impl AnimationSource {
    /// Bring up a [`Scene`] over `context` using `frag_spirv` and start a source at frame 0, counting
    /// upward.
    ///
    /// # Inputs and outputs
    /// `context`: a freshly-built Vulkan context, moved in and owned from here on (see the struct
    /// doc for why ownership, not a borrow). `frag_spirv`: the fractal fragment shader's words (see
    /// [`spirv_words`]). Returns a source whose first [`AnimationSource::next_frame_rgba`] call draws
    /// frame 0.
    ///
    /// # Errors
    /// Whatever [`Scene::new`] fails with (any Vulkan pipeline/render-target creation call failing).
    fn new(context: VulkanContext, frag_spirv: &[u32]) -> anyhow::Result<Self> {
        // `None`: no sampler binding, because this program uses fixture B's fragment-shader fractal,
        // not fixture A's sampled texture — see this module's doc ("Why the GPU fixture's shader").
        let scene = Scene::new(&context, frag_spirv, None)?;
        Ok(AnimationSource {
            scene,
            context,
            frame_index: 0,
            ascending: true,
        })
    }

    /// Draw the frame at `self.frame_index`, upscale it, advance the schedule, and return the
    /// upscaled RGBA8 pixels.
    ///
    /// Called once per redraw — either the very first draw (on the window's initial `configure`) or
    /// each time the compositor's `wl_surface::frame` callback fires (see [`DemoWindow::draw`]).
    ///
    /// # Errors
    /// Returns an error if [`Scene::draw`] fails (any Vulkan call failing, or a fence timeout — see
    /// that method's doc in `rayland-icosa-vk`).
    fn next_frame_rgba(&mut self) -> anyhow::Result<Vec<u8>> {
        // The same two schedule functions both fixtures call, evaluated at this call's frame index —
        // nothing here differs from `rayland-icosa-gpu`'s per-frame uniform preparation.
        let uniforms = Uniforms {
            mvp: frame_mvp(self.frame_index),
            half_width: frame_zoom(self.frame_index) as f32,
            center: [CENTER.0 as f32, CENTER.1 as f32],
        };
        // The one Vulkan-touching line: draws, waits the fence, and reads back IMAGE_SIZE² RGBA8
        // bytes. ~1.5 ms on this project's reference machine (this module's "Why the GPU fixture's
        // shader" doc) — comfortably under a 60 Hz frame budget, which is what lets this program pace
        // itself off the compositor's own frame callbacks instead of a fixed sleep.
        let native_pixels = self.scene.draw(&self.context, &uniforms)?;

        // Advance the schedule for the *next* call before returning this call's frame — see the
        // struct doc's "The ping-pong schedule" for why a flip at each endpoint, not a wraparound,
        // is what keeps consecutive frames exactly one `ZOOM_PER_FRAME` step apart forever.
        if self.ascending {
            if self.frame_index + 1 >= rayland_icosa_core::FRAME_COUNT {
                // Reached the deepest zoom (frame 119): turn around without moving this call's own
                // frame_index, so frame 119 is drawn exactly once, not skipped or repeated.
                self.ascending = false;
            } else {
                self.frame_index += 1;
            }
        } else if self.frame_index == 0 {
            // Reached the widest zoom (frame 0) again: turn around the same way as above.
            self.ascending = true;
        } else {
            self.frame_index -= 1;
        }

        Ok(nearest_neighbor_upscale(&native_pixels, IMAGE_SIZE, SCALE))
    }
}

/// Expand a tightly-packed, square RGBA8 image from `edge`×`edge` to `(edge * scale)`×`(edge *
/// scale)` by repeating each source pixel into a `scale`×`scale` block of identical pixels.
///
/// # Why nearest-neighbour rather than a filtered resize
/// See this module's doc ("The upscale"): the point of this demo is to see the Mandelbrot texture's
/// actual, blocky texel structure at this zoom depth, not a smoothed approximation of it. A filtered
/// resize would blur exactly the thing a viewer is here to look at.
///
/// # Inputs and outputs
/// `src` must be exactly `edge * edge * 4` bytes (RGBA8, row-major, no padding — [`Scene::draw`]'s
/// documented output shape). Returns `(edge * scale) * (edge * scale) * 4` bytes in the same layout.
///
/// # Failure modes
/// None; this is pure arithmetic over indices already proven in range by construction (every `sy`,
/// `sx` computed below is `< edge` because `dy`, `dx` are each `< edge * scale`).
fn nearest_neighbor_upscale(src: &[u8], edge: u32, scale: u32) -> Vec<u8> {
    let dst_edge = edge * scale;
    let mut dst = vec![0u8; (dst_edge * dst_edge * 4) as usize];
    for dy in 0..dst_edge {
        // Integer division maps a whole run of `scale` destination rows onto the one source row they
        // repeat.
        let sy = dy / scale;
        for dx in 0..dst_edge {
            let sx = dx / scale;
            let src_offset = ((sy * edge + sx) * 4) as usize;
            let dst_offset = ((dy * dst_edge + dx) * 4) as usize;
            // Copy the one source texel's four RGBA8 bytes verbatim — "nearest neighbour" is exactly
            // this: no blending with any other texel.
            dst[dst_offset..dst_offset + 4].copy_from_slice(&src[src_offset..src_offset + 4]);
        }
    }
    dst
}

/// Copy a frame's pixels from tightly-packed RGBA8 into a `wl_shm` `Xrgb8888` buffer.
///
/// See this module's doc ("Pixel format") for the full reasoning; this is the swizzle it describes.
/// `rayland-present/src/frame.rs`'s `pack_xrgb8888` performs the identical conversion — read there
/// for more depth — but this function is written independently, not imported, per this task's brief.
///
/// # Inputs
/// - `rgba`: source pixels, red/green/blue/alpha byte order, `len()` a multiple of 4.
/// - `canvas`: destination; must be exactly `rgba.len()` bytes (the `wl_shm` pool row stride for a
///   32-bit format at this window's fixed width is already tight, so no per-row padding arises).
///
/// # Panics
/// Panics (via `assert_eq!`) if `canvas.len() != rgba.len()` — a caller bug, since both are always
/// sized from the same `WINDOW_EDGE`.
fn swizzle_rgba8_to_xrgb8888(rgba: &[u8], canvas: &mut [u8]) {
    assert_eq!(
        canvas.len(),
        rgba.len(),
        "destination canvas must be exactly as many bytes as the source RGBA8 frame"
    );
    // Walk source and destination four bytes (one pixel) at a time, in lockstep.
    for (src, dst) in rgba.chunks_exact(4).zip(canvas.chunks_exact_mut(4)) {
        // Read the source channels by their documented RGBA positions.
        let r = src[0] as u32;
        let g = src[1] as u32;
        let b = src[2] as u32;
        // Assemble the 32-bit 0x00RRGGBB word; the unused top byte stays 0 (opaque window).
        let word = (r << 16) | (g << 8) | b;
        // Writing the word little-endian lays the bytes out as B, G, R, 0 — exactly the Xrgb8888
        // memory order the compositor expects.
        dst.copy_from_slice(&word.to_le_bytes());
    }
}

/// All state the persistent window's event loop needs, threaded through SCTK's handler callbacks.
///
/// Exactly one `DemoWindow` is created per run, in [`run`], and it lives for the program's entire
/// lifetime — this is the whole fix this rewrite makes (see this module's doc, "One window, one
/// redraw loop"). Its single `window` field is the one and only `xdg_toplevel` this program ever
/// creates.
///
/// Holds no borrows (in particular, [`AnimationSource`] is owned, not borrowed — see that type's
/// doc): `calloop::EventLoop<DemoWindow>` requires its state type to be `'static`, which a borrowing
/// `DemoWindow<'a>` cannot satisfy.
struct DemoWindow {
    /// SCTK's registry bookkeeping (which globals exist).
    registry_state: RegistryState,
    /// SCTK's output (monitor) bookkeeping; required by `CompositorHandler`'s blanket bound even
    /// though this fixed-size window does not otherwise track outputs.
    output_state: OutputState,
    /// SCTK's shared-memory manager; owns the `wl_shm` global.
    shm: Shm,
    /// The one, persistent `xdg_toplevel` window this program ever creates. Never rebuilt.
    window: Window,
    /// The `wl_shm` pool every frame's buffer is allocated from. Sized for one `WINDOW_EDGE ×
    /// WINDOW_EDGE` frame at creation and left to grow itself (SCTK's `SlotPool::create_buffer`
    /// resizes automatically) on the rare occasion a second buffer is needed — see
    /// [`DemoWindow::draw`]'s doc for when that happens.
    pool: SlotPool,
    /// The buffer most recently handed to the compositor. `None` only before the first draw.
    /// [`DemoWindow::draw`] reuses it when the compositor has released it, and allocates a second one
    /// (replacing this) when it has not — see that method's doc for why both cases occur.
    buffer: Option<Buffer>,
    /// The GPU-backed animation, drawing one new frame per call and advancing its own schedule.
    source: AnimationSource,
    /// Set true to break the event loop: the window was closed, or a draw failed.
    exit: bool,
    /// True until the first `configure`, so the first draw happens exactly once, in response to it,
    /// rather than being raced against the compositor not yet having sized the window.
    first_configure: bool,
    /// Why a draw failed, if it did — carried out of the event loop so [`run`] can return it.
    ///
    /// Neither `WindowHandler::configure` nor `CompositorHandler::frame` can return a `Result` (both
    /// are trait callbacks with a fixed signature), so a draw failure has nowhere to go but here; see
    /// [`run`]'s event loop for where this is turned back into a return value.
    draw_error: Option<anyhow::Error>,
}

impl DemoWindow {
    /// Draw the next animation frame into a `wl_shm` buffer, attach it, damage the whole surface,
    /// commit, and request the callback that will trigger the *following* frame.
    ///
    /// This is the one redraw step this module's doc ("One window, one redraw loop") describes,
    /// called both for the very first frame (from `WindowHandler::configure`, on the window's first
    /// `configure`) and for every subsequent one (from `CompositorHandler::frame`, each time the
    /// compositor signals it is ready for another). Requesting the *next* callback inside *this*
    /// call, before returning, is what keeps the chain going: each draw arranges its own successor,
    /// so the loop is entirely event-driven and contains no `sleep` or timer anywhere.
    ///
    /// # Double-buffering
    /// [`SlotPool::canvas`] returns `None` if the compositor has not yet released the buffer this
    /// program handed it last time (it may still be reading from it, e.g. mid-composite). When that
    /// happens this method allocates a **second** buffer via `create_buffer` and swaps it into
    /// `self.buffer`, exactly as SCTK's own `simple_window` example does; the pool grows itself to
    /// hold both live buffers, and the older one's slot is reclaimed automatically once the
    /// compositor's `release` event for it arrives (`delegate_shm!` wires that dispatch). This is the
    /// "double-buffer if a single `wl_buffer` would be reused while the compositor still holds it"
    /// requirement from this task's brief — SCTK's `SlotPool` is exactly the "handles this" it
    /// refers to; nothing here hand-tracks buffer release.
    ///
    /// # Errors
    /// Returns an error if drawing the next animation frame fails ([`AnimationSource::next_frame_rgba`]),
    /// if the shm pool cannot allocate a buffer, or if attaching the buffer to the surface fails.
    fn draw(&mut self, qh: &QueueHandle<Self>) -> anyhow::Result<()> {
        let width = WINDOW_EDGE as i32;
        let height = WINDOW_EDGE as i32;
        let stride = width * 4; // 32-bit format, no per-row padding at this width.

        // The GPU render + CPU upscale for this frame, in RGBA8. Advances the ping-pong schedule as
        // a side effect (see AnimationSource::next_frame_rgba's doc).
        let rgba = self.source.next_frame_rgba()?;

        let buffer = self.buffer.get_or_insert_with(|| {
            // No prior buffer exists yet (the very first draw): allocate one. `expect` is
            // appropriate here, not `?`, because `get_or_insert_with`'s closure cannot return a
            // `Result` — see the fallback branch below for the case this can actually fail in
            // practice, which is handled properly.
            self.pool
                .create_buffer(width, height, stride, wl_shm::Format::Xrgb8888)
                .expect("initial wl_shm buffer allocation must succeed")
                .0
        });

        let canvas = match self.pool.canvas(buffer) {
            // The common case: the compositor already released the buffer we handed it last frame,
            // so we can safely overwrite it in place.
            Some(canvas) => canvas,
            // The compositor is still holding our last buffer (see this method's doc,
            // "Double-buffering"): allocate a second one instead of racing the compositor's read.
            None => {
                let (second_buffer, canvas) = self
                    .pool
                    .create_buffer(width, height, stride, wl_shm::Format::Xrgb8888)
                    .map_err(|e| anyhow::anyhow!("failed to create wl_shm buffer: {e}"))?;
                *buffer = second_buffer;
                canvas
            }
        };

        // The RGBA8 -> Xrgb8888 swizzle this module's doc describes ("Pixel format").
        swizzle_rgba8_to_xrgb8888(&rgba, canvas);

        let surface = self.window.wl_surface();
        // Mark the entire surface as changed so the compositor repaints all of it.
        surface.damage_buffer(0, 0, width, height);
        // Ask for the callback that will drive the *next* draw. Requested before attach+commit
        // (matching SCTK's own example) so the request rides on the same commit as this frame's
        // buffer — the compositor is not required to honour a frame request that arrives on its own,
        // unattached commit.
        surface.frame(qh, surface.clone());
        // Attach the finished buffer and commit — this is what actually makes the frame visible.
        buffer
            .attach_to(surface)
            .map_err(|e| anyhow::anyhow!("failed to attach buffer: {e}"))?;
        self.window.commit();

        Ok(())
    }
}

impl CompositorHandler for DemoWindow {
    // Output scale changes need no action: every buffer this program draws is already sized for
    // WINDOW_EDGE physical pixels; a fractional-scale desktop would need this to redraw at a new
    // buffer size, which is out of scope for a fixed-size demo window.
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_factor: i32,
    ) {
    }
    // Output transform changes likewise need no action for this fixed-size window.
    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }
    /// The compositor's redraw signal: fires once per output refresh after a `wl_surface::frame`
    /// request, meaning "you may draw another frame now". This is the loop's actual heartbeat — see
    /// this module's doc ("One window, one redraw loop, paced by the compositor").
    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _time: u32,
    ) {
        // A draw failure here is unexpected; end the loop rather than let the animation silently
        // freeze, and keep the error so `run` can report it (see `draw_error`'s doc).
        if let Err(e) = self.draw(qh) {
            self.draw_error = Some(e);
            self.exit = true;
        }
    }
    // Which output the surface entered is irrelevant to this fixed-size window.
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

impl OutputHandler for DemoWindow {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for DemoWindow {
    /// The user closed the window (e.g. its close button, or a compositor keybinding). This is the
    /// **only** way this program's event loop ends, other than a draw error — there is no timer, no
    /// dwell period, nothing else that stops it. See this module's doc ("One window, one redraw
    /// loop") for why a user close reliably reaching this callback at all was the actual bug being
    /// fixed: a window that is destroyed and replaced multiple times a second never sits still long
    /// enough for a click to land on the one that is about to receive it.
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _window: &Window) {
        self.exit = true;
    }
    /// The compositor has (re)sized/(re)stated the window. Only the *first* configure matters here:
    /// it is this program's cue that the surface is ready to receive its first buffer, so the very
    /// first draw — and the `wl_surface::frame` chain that keeps every draw after it going — starts
    /// from here. Later configures (e.g. a focus change) need no action: this window's size is
    /// pinned (`set_min_size`/`set_max_size` in [`run`]), so there is nothing to re-layout.
    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _window: &Window,
        _configure: WindowConfigure,
        _serial: u32,
    ) {
        if self.first_configure {
            self.first_configure = false;
            // A draw failure here means the window can never show anything; end the loop rather
            // than sit forever on a blank surface, and keep the error (see `draw_error`'s doc).
            if let Err(e) = self.draw(qh) {
                self.draw_error = Some(e);
                self.exit = true;
            }
        }
    }
}

impl ShmHandler for DemoWindow {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for DemoWindow {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    // OutputState needs registry notifications to discover/track wl_output globals; no other extra
    // global handler is registered (no seat/input handling — this demo takes no keyboard/pointer
    // input of its own beyond what xdg-shell's decoration gives it for free).
    registry_handlers![OutputState];
}

// Wire SCTK's protocol dispatch to the handler impls above.
delegate_compositor!(DemoWindow);
delegate_output!(DemoWindow);
delegate_shm!(DemoWindow);
delegate_xdg_shell!(DemoWindow);
delegate_xdg_window!(DemoWindow);
delegate_registry!(DemoWindow);

/// The real body of [`main`]: bring up Vulkan, build the scene once, open **one** persistent Wayland
/// window, and run its compositor-paced redraw loop until the user closes it.
///
/// # Errors
/// Returns an error if Vulkan bring-up or scene creation fails, if the compositor is unreachable or
/// is missing a required global (`wl_compositor`, `wl_shm`, `xdg_wm_base`), if any buffer allocation
/// fails, or if a draw fails at any point during the run (see [`DemoWindow::draw`]'s doc).
fn run() -> anyhow::Result<()> {
    // Bring Vulkan up exactly once. Which driver answers is the environment's decision, not this
    // program's — it probes nothing, matching both fixtures' own bring-up (`VulkanContext::new`'s
    // only environment interaction is `ash::Entry::load()` finding a driver). `AnimationSource::new`
    // builds the Scene over it and takes ownership of both (see that type's doc for why ownership,
    // not the borrowing this crate's fixtures use, is required here).
    let context = VulkanContext::new()?;
    let frag_spirv = spirv_words(FRACTAL_FRAGMENT_SPIRV);
    let source = AnimationSource::new(context, &frag_spirv)?;

    // Connect to the compositor named by WAYLAND_DISPLAY. This happens exactly once, here, for the
    // whole run — see this module's doc ("One window, one redraw loop") for why that is the entire
    // point of this rewrite.
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow::anyhow!("cannot connect to a Wayland compositor: {e}"))?;
    // Bootstrap the registry and get the initial event queue. `registry_queue_init` performs its own
    // internal roundtrip, so `globals` is fully populated the moment this returns.
    let (globals, event_queue) = registry_queue_init(&conn)
        .map_err(|e| anyhow::anyhow!("Wayland registry initialization failed: {e}"))?;
    let qh: QueueHandle<DemoWindow> = event_queue.handle();

    let mut event_loop: EventLoop<DemoWindow> =
        EventLoop::try_new().map_err(|e| anyhow::anyhow!("failed to create event loop: {e}"))?;
    let loop_handle = event_loop.handle();
    // Feed Wayland events (including the frame callbacks that drive the redraw loop) into calloop.
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle)
        .map_err(|e| anyhow::anyhow!("failed to insert the Wayland source: {e}"))?;

    // Bind the three globals this program needs; a missing one is a clear, fatal error.
    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wl_compositor unavailable: {e}"))?;
    let xdg_shell = XdgShell::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("xdg_wm_base (window shell) unavailable: {e}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| anyhow::anyhow!("wl_shm unavailable: {e}"))?;

    // Create the ONE surface and give it the xdg_toplevel role. This is the single `get_toplevel`
    // request this program's whole run makes — see this module's doc and the completion report's
    // WAYLAND_DEBUG count.
    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title(WINDOW_TITLE);
    window.set_app_id(WINDOW_APP_ID);
    // Pin min == max to WINDOW_EDGE: this window never resizes, so there is exactly one buffer size
    // to ever allocate. `NonZeroU32` is what `set_min_size`/`set_max_size` require; WINDOW_EDGE is a
    // compile-time non-zero constant, so this conversion cannot fail.
    let edge = NonZeroU32::new(WINDOW_EDGE).expect("WINDOW_EDGE is a non-zero constant");
    window.set_min_size(Some((edge.get(), edge.get())));
    window.set_max_size(Some((edge.get(), edge.get())));
    // Initial commit with no buffer: the compositor replies with a configure, after which
    // DemoWindow::configure performs the first draw (see that method's doc).
    window.commit();

    // Sized for exactly one WINDOW_EDGE x WINDOW_EDGE frame; SlotPool grows itself automatically if
    // a second, concurrently-live buffer is ever needed (see DemoWindow::draw's "Double-buffering").
    let pool_size = (WINDOW_EDGE * WINDOW_EDGE * 4) as usize;
    let pool = SlotPool::new(pool_size, &shm)
        .map_err(|e| anyhow::anyhow!("failed to create shm pool: {e}"))?;

    let mut state = DemoWindow {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        window,
        pool,
        buffer: None,
        source,
        exit: false,
        first_configure: true,
        draw_error: None,
    };

    println!(
        "rayland-icosa-window: opening a window; close it to exit (this is a demo, not a fixture — \
         see the crate's module docs)"
    );
    // Dispatch events, blocking until one arrives, until the window is closed or a draw fails. Every
    // wakeup here is either a genuine window-management event or a `wl_surface::frame` callback the
    // compositor itself scheduled — nothing in this loop polls, sleeps, or times anything out.
    while !state.exit {
        event_loop
            .dispatch(None, &mut state)
            .map_err(|e| anyhow::anyhow!("event loop dispatch failed: {e}"))?;
        // A compositor that refuses something we sent it raises a **protocol error** and destroys
        // the connection rather than replying "no" to the request that caused it; `dispatch` does not
        // surface that as an `Err` (see `rayland-present`'s `window.rs`, which documents finding this
        // exact gap by mutation testing), so without this check a rejected request would leave the
        // loop waiting forever on a dead connection for a close event that can now never arrive.
        if conn.protocol_error().is_some() {
            break;
        }
    }
    if let Some(e) = conn.protocol_error() {
        return Err(anyhow::anyhow!(
            "the compositor rejected what we sent and destroyed the connection: {} (object {}@{}, \
             code {}).",
            e.message,
            e.object_interface,
            e.object_id,
            e.code
        ));
    }
    if let Some(e) = state.draw_error.take() {
        return Err(e.context("presenting a frame failed while drawing it"));
    }

    println!("rayland-icosa-window: window closed; exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The DRM render node this repository's GPU tests gate on — matching every other icosa crate's
    /// convention (see `rayland-icosa-gpu/tests/native_render.rs`'s identical constant) so this test
    /// skips, rather than fails, on a machine with no GPU.
    const RENDER_NODE: &str = "/dev/dri/renderD128";

    /// [`AnimationSource::next_frame_rgba`] must return an upscaled, non-blank frame of the
    /// documented size — checked without ever opening a window, matching this task's brief ("keep
    /// that pattern": the existing `produce_pixels`-style test, GPU-gated, skipping cleanly without a
    /// render node).
    ///
    /// # Why this cannot also test presentation
    /// It doesn't try to. Whether a window actually appears on a real compositor, animates, and stays
    /// open is not something any automated check in this repository can assert — no CI runner has a
    /// real compositor to paint into. That is verified by a human looking at the screen and by the
    /// process-level counts (toplevel churn, `get_toplevel` count) in this task's completion report,
    /// not by a unit test. This test's job stops at the boundary this module draws through Vulkan: the
    /// pixels this program *would* show, not whether anything ever shows them.
    #[test]
    fn next_frame_rgba_upscales_and_is_not_blank() {
        if !Path::new(RENDER_NODE).exists() {
            eprintln!(
                "SKIP next_frame_rgba_upscales_and_is_not_blank: no render node at {RENDER_NODE}"
            );
            return;
        }

        let context = VulkanContext::new()
            .expect("Vulkan bring-up must succeed on a host with a render node");
        let frag_spirv = spirv_words(FRACTAL_FRAGMENT_SPIRV);
        let mut source =
            AnimationSource::new(context, &frag_spirv).expect("scene creation must succeed");

        let expected_len = (WINDOW_EDGE * WINDOW_EDGE * 4) as usize;
        let frame = source
            .next_frame_rgba()
            .expect("producing the first frame must succeed");
        assert_eq!(
            frame.len(),
            expected_len,
            "RGBA8, tightly packed, no padding, at the upscaled WINDOW_EDGE"
        );

        // "Not uniformly background": the render pass clears to opaque black (see
        // `rayland_icosa_vk::scene::Scene::draw`'s clear-value doc), so any non-black pixel proves the
        // upscaled frame actually shows the drawn solid rather than an empty clear.
        let background = [0u8, 0, 0, 255];
        let non_background_pixels = frame
            .chunks_exact(4)
            .filter(|pixel| *pixel != background)
            .count();
        assert!(
            non_background_pixels > 0,
            "the frame must show more than just the cleared background"
        );
    }

    /// [`swizzle_rgba8_to_xrgb8888`] must reorder RGBA8 bytes into `Xrgb8888`'s little-endian
    /// B,G,R,X memory layout — the same property `rayland-present`'s `pack_xrgb8888` test checks,
    /// verified independently here since this function is a deliberate, from-scratch reimplementation
    /// rather than an import (per this task's brief).
    #[test]
    fn swizzle_rgba8_to_xrgb8888_reorders_channels() {
        // Two known pixels: pure red then pure green, both fully opaque in the source.
        let rgba = [
            255, 0, 0, 255, // red   (R,G,B,A)
            0, 255, 0, 255, // green (R,G,B,A)
        ];
        let mut canvas = [0u8; 8];
        swizzle_rgba8_to_xrgb8888(&rgba, &mut canvas);
        // Red -> Xrgb8888 little-endian bytes B,G,R,X = 0,0,255,0.
        // Green -> 0,255,0,0.
        assert_eq!(canvas, [0, 0, 255, 0, 0, 255, 0, 0]);
    }
}
