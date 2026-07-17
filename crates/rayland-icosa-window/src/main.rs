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
//! - **It depends on `rayland-present`**, a `rayland-*` crate, to put pixels in a real window. The
//!   fixtures may never do this (§2's dependency rule exists precisely so a fixture cannot see
//!   whether it is being remoted); this crate is not a fixture, so there is nothing here for it to
//!   see that would compromise anything.
//! - **It has a wall-clock frame loop.** [`FRAME_DWELL`] and [`std::thread::sleep`] appear below,
//!   deliberately. A fixture with a clock in its render loop would produce different pixels on every
//!   run, destroying the bit-identical comparison its tests depend on. This program is compared
//!   against nothing, so a clock costs it nothing.
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
//! `None` for the sampler binding, exactly as `rayland-icosa-gpu` does, skips both.
//!
//! # Why this program reopens the window every frame, and what that costs
//! `rayland_present::present` draws **exactly one static frame per call** — see that function's own
//! doc comment ("one static frame per call") and `rayland-s`'s `present_frame` (the crate's own
//! worked example, which this module follows structurally). It creates a `Connection`, binds the
//! globals, creates a surface, waits for the compositor's first `configure`, draws once, and then
//! only services window-close/disconnect events until one of them fires. There is no per-frame
//! redraw hook anywhere in it, by design: it was built for `rayland-s`, which only ever has one
//! frame to show. This crate may not modify `rayland-present` (see the repository's task brief for
//! this crate) — touching shared scaffolding to fit a demo would be exactly backwards — so animation
//! has to be built *on top of* one-shot presentation rather than inside it.
//!
//! The mechanism [`show_one_frame`] uses is therefore: call `present` once per animation frame, let
//! the window it opens sit on screen for [`FRAME_DWELL`], then let that call return and open the next
//! one with the next frame's pixels. [`main`]'s loop repeats this for as long as the user leaves the
//! window(s) alone; closing whichever window is currently up ends the whole program (see
//! [`show_one_frame`]'s doc for exactly how that is detected, since `present` gives no direct signal
//! for "the user closed it" versus "our own timer ended the call"). On a compositor that creates and
//! maps a new `xdg_toplevel` quickly, this reads as continuous motion with the same title, the same
//! app id, and the same fixed size every time; on a slow one it will look like what it is — a rapid
//! slideshow of separate windows. Both are honest descriptions of what this code actually does, and
//! which one a human sees is a fact about the compositor, not about this program. See this task's
//! completion report for what was actually observed on this repository's own test compositor.
//!
//! # Object lifetime and drop order
//! Identical to both fixtures' `main.rs`, and for the identical reason: [`Scene`] implements [`Drop`]
//! and holds a cloned `ash::Device` (see that type's struct doc in `rayland-icosa-vk`), so it must be
//! destroyed before its [`VulkanContext`]. `context` is declared first in [`run`], `scene` second, so
//! Rust's reverse-declaration-order drop rule gets this right with no explicit teardown code needed —
//! unlike fixture A, this program owns no extra `MappedBuffer` or texture of its own to sequence
//! around that drop, so there is no nested block here either (matching fixture B's shape, not
//! fixture A's).
//!
//! # The ping-pong schedule
//! [`rayland_icosa_core::FRAME_COUNT`] (120) frames exist, indexed `0..120`, and
//! [`rayland_icosa_core::schedule::frame_zoom`] is geometric: `1.5 * 0.97^i`. A fixture plays them
//! once, `0..119`, and stops — there is no next run to loop into. This program runs until a human
//! closes it, so it has to decide what "frame 120" means, and wrapping straight back to frame 0 would
//! snap the zoom from its deepest point back out to its widest in a single frame — a visible jump, not
//! a loop. [`IcosaFrameSource`] instead **ping-pongs**: `0, 1, .., 119, 118, .., 1, 0, 1, ..` — the
//! zoom breathes in to its deepest point and back out to its start, forever, with every consecutive
//! pair of frames exactly [`rayland_icosa_core::schedule::ZOOM_PER_FRAME`] apart. Nothing about this
//! is a measurement; it is chosen purely because it is the schedule that never jumps.
//!
//! # The upscale
//! [`Scene`] always renders at [`rayland_icosa_core::IMAGE_SIZE`] (256) — a
//! `rayland-icosa-core` constant both fixtures depend on for their pixel-identical comparison, and
//! this crate must not perturb it (see the "do not touch" list in this task's brief). 256 px is a
//! postage stamp on a modern display, so [`FrameSource::width`]/[`FrameSource::height`] instead report
//! `IMAGE_SIZE * SCALE` ([`SCALE`] = 3, giving 768×768), and [`nearest_neighbor_upscale`] repeats each
//! rendered texel into a `SCALE`×`SCALE` block of identical pixels before handing the frame to
//! `rayland-present`. **Nearest-neighbour, not linear.** The point of watching this demo is to see the
//! Mandelbrot texture's actual texels — the blocky, stair-stepped edges *are* the picture at this
//! zoom depth — and a linear or bicubic filter would blur exactly the detail a viewer is here to see.

use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use rayland_icosa_core::IMAGE_SIZE;
use rayland_icosa_core::schedule::{CENTER, frame_mvp, frame_zoom};
use rayland_icosa_vk::{Scene, Uniforms, VulkanContext};
use rayland_present::{FrameSource, RenderedFrame, WindowConfig, present};

/// The compiled fractal fragment shader — identical bytes to `rayland-icosa-gpu`'s, evaluating the
/// same Mandelbrot escape-time function in the fragment stage instead of sampling a CPU-computed
/// texture. See this module's doc ("Why the GPU fixture's shader") for why this program uses it
/// rather than fixture A's textured path.
const FRACTAL_FRAGMENT_SPIRV: &[u8] = include_bytes!("../../../shaders/icosa_fractal.frag.spv");

/// How many identical pixels each of [`Scene`]'s rendered texels is expanded into, per axis, before
/// the frame is handed to `rayland-present`. See this module's doc ("The upscale") for why this
/// exists and why the expansion is nearest-neighbour rather than a filtered resize.
const SCALE: u32 = 3;

/// How long each reopened window sits on screen before this program lets its `present` call end and
/// moves on to the next frame — the closest thing this demo has to a target frame interval. See
/// [`show_one_frame`]'s doc for the mechanism this paces and why ~30 fps (33 ms) was chosen: fast
/// enough to read as motion, slow enough that a compositor slower than this program's own render
/// time (~1.5 ms, per this module's "Why the GPU fixture's shader" doc) is still the pacing's real
/// bottleneck, not this program's.
const FRAME_DWELL: Duration = Duration::from_millis(33);

/// If a `present` call returns in under this fraction of [`FRAME_DWELL`], [`show_one_frame`] treats
/// it as the user having closed the window rather than this program's own timer having fired. See
/// that function's doc for the full argument; `0.5` is a wide enough margin to absorb ordinary
/// scheduling jitter in the sleeping timer thread without ever mistaking a genuine timer-driven
/// return (which is always close to the full [`FRAME_DWELL`]) for a user close.
const EARLY_RETURN_FRACTION: f64 = 0.5;

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
/// `rayland-present`, `anyhow`, per this task's brief — deliberately stops short of `ash`, exactly as
/// fixture B's does. `ash::util::read_spv` would do the same job in one call, but only if this crate
/// linked `ash` for it, which it does not.
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

/// A [`FrameSource`] that draws one frame of the ping-ponging animation per call, advancing its own
/// frame counter each time.
///
/// # Why this struct borrows rather than owns `context`/`scene`
/// [`VulkanContext`] and [`Scene`] are expensive to build (Vulkan instance/device bring-up, pipeline
/// and render-target creation) and cheap to *draw with* (~1.5 ms/frame — see this module's doc). This
/// program reopens a window every frame (see [`show_one_frame`]) but must not rebuild the GPU state
/// every frame, or the 20×-plus cost fixture A pays for its CPU texture path would be reintroduced
/// here for no reason at all. So [`run`] builds `context` and `scene` exactly once and constructs one
/// `IcosaFrameSource` that borrows both for the whole run; only the surrounding `present` call and its
/// window are what get rebuilt per frame.
struct IcosaFrameSource<'a> {
    /// The Vulkan context `scene` was built from. Must outlive `scene` — the caller obligation
    /// [`Scene`]'s own struct doc describes, upheld here by `context` being declared before `scene`
    /// in [`run`] (see this module's doc, "Object lifetime and drop order").
    context: &'a VulkanContext,
    /// The long-lived scene every call to [`FrameSource::produce_pixels`] draws through.
    scene: &'a mut Scene,
    /// Which of the 120 animation frames the *next* [`FrameSource::produce_pixels`] call will draw.
    /// Starts at 0 (the widest, most recognisable view of the fractal) and is advanced by that same
    /// call, after drawing, according to `ascending`.
    frame_index: u32,
    /// `true` while counting up toward frame 119, `false` while counting back down toward 0. Flipped
    /// exactly at the two endpoints — see [`FrameSource::produce_pixels`] for the turnaround logic
    /// and this module's doc ("The ping-pong schedule") for why a flip, rather than a wraparound, is
    /// what keeps the animation visually seamless.
    ascending: bool,
}

impl<'a> IcosaFrameSource<'a> {
    /// Start a source at frame 0, counting upward.
    ///
    /// # Inputs and outputs
    /// `context`/`scene`: the long-lived Vulkan state to draw through (see the struct doc for why
    /// this type borrows rather than owns them). Returns a source whose first
    /// [`FrameSource::produce_pixels`] call draws frame 0.
    fn new(context: &'a VulkanContext, scene: &'a mut Scene) -> Self {
        IcosaFrameSource {
            context,
            scene,
            frame_index: 0,
            ascending: true,
        }
    }
}

impl FrameSource for IcosaFrameSource<'_> {
    /// The window's fixed width: the rendered `IMAGE_SIZE`, expanded by `SCALE`. Constant across
    /// every frame, so `rayland-present` sizes the window identically on every reopened `present`
    /// call — see this module's doc ("Why this program reopens the window every frame") for why that
    /// matters to the animation reading as continuous rather than as a window that resizes each time.
    fn width(&self) -> u32 {
        IMAGE_SIZE * SCALE
    }

    /// The window's fixed height. See [`Self::width`]; the image is square.
    fn height(&self) -> u32 {
        IMAGE_SIZE * SCALE
    }

    /// Draw the frame at `self.frame_index`, upscale it, advance the schedule, and return it.
    ///
    /// This is the "frame loop" this module's doc promises, just shaped to match `FrameSource`'s
    /// one-call-per-`present`-invocation contract: each call is one animation frame, and the counter
    /// that would ordinarily be a fixture's `for frame in 0..FRAME_COUNT` loop variable lives on
    /// `self` instead, advanced here rather than by an enclosing loop, so it survives from one
    /// `present` call to the next (see [`show_one_frame`], which reuses the same `IcosaFrameSource`
    /// across every reopened window).
    ///
    /// # Errors
    /// Returns an error if [`Scene::draw`] fails (any Vulkan call failing, or a fence timeout — see
    /// that method's doc in `rayland-icosa-vk`).
    fn produce_pixels(&mut self) -> anyhow::Result<RenderedFrame> {
        // The same two schedule functions both fixtures call, evaluated at this call's frame index —
        // nothing here differs from `rayland-icosa-gpu`'s per-frame uniform preparation.
        let uniforms = Uniforms {
            mvp: frame_mvp(self.frame_index),
            half_width: frame_zoom(self.frame_index) as f32,
            center: [CENTER.0 as f32, CENTER.1 as f32],
        };
        // The one Vulkan-touching line: draws, waits the fence, and reads back IMAGE_SIZE² RGBA8
        // bytes. ~1.5 ms on this project's reference machine (this module's "Why the GPU fixture's
        // shader" doc) — the reason this program can afford to redraw every frame at all.
        let native_pixels = self.scene.draw(self.context, &uniforms)?;

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

        Ok(RenderedFrame {
            width: IMAGE_SIZE * SCALE,
            height: IMAGE_SIZE * SCALE,
            pixels: nearest_neighbor_upscale(&native_pixels, IMAGE_SIZE, SCALE),
        })
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

/// Open one window, show `source`'s next frame in it, and let it sit on screen for one
/// [`FRAME_DWELL`] before returning — unless the user closes the window first.
///
/// # The disconnect trap — read this before changing anything here
/// `rayland_present::present` ends its event loop when the window is closed **or** its `disconnect`
/// argument reaches end-of-file (see that function's doc). This function wants **both** exits to
/// work — the timer *and* the close button — which is a different requirement from `rayland-s`'s
/// `present_frame` (which wants the close button to be the *only* exit, and holds its `disconnect`
/// peer alive for the whole call precisely to suppress the other one). Here, the `UnixStream::pair`'s
/// `ours` end is deliberately handed to a background thread that sleeps for `FRAME_DWELL` and *then*
/// drops it — never before. **Dropping `ours` any earlier — binding it to `_`, for instance — makes
/// `theirs` readable immediately, and `present` would return before the compositor has even shown the
/// frame it just drew: the window would flash for approximately zero milliseconds and this whole
/// function would appear to do nothing.** This is exactly the "mysterious bug later" this task's
/// brief warned about, and it is why the timer thread's only job is to sleep first and drop second,
/// in that order, with nothing else happening in between.
///
/// # Telling a user-close apart from a timer-close, with no direct signal for either
/// `present` returns `Ok(())` in both cases — closing the window and the `disconnect` source reaching
/// EOF are structurally indistinguishable from its return value alone (see its doc: "the loop ends
/// when either trigger fires"). This function infers which one happened from **timing**: the
/// background timer thread cannot drop `ours` before `FRAME_DWELL` has elapsed, so a `present` call
/// that returns *close to* `FRAME_DWELL` almost certainly ended via the timer, while one that returns
/// **much sooner** can only mean the user reached the window first — nothing else here can make
/// `present` return early. [`EARLY_RETURN_FRACTION`] sets how much sooner counts as "much sooner": a
/// wide-enough margin to absorb ordinary OS scheduling jitter on the timer thread without ever
/// mistaking a genuine timer-driven return for a user close.
///
/// # Inputs and outputs
/// `source`/`config`: passed straight through to [`present`]. Returns `Ok(true)` if the call is
/// believed to have ended via the timer (keep looping — see [`run`]), `Ok(false)` if it is believed
/// to have ended via a user close (stop).
///
/// # Errors
/// Whatever [`present`] itself returns — an unreachable compositor, a missing Wayland global, a
/// buffer allocation failure, or an event-loop error. Propagated to [`run`] and from there to
/// [`main`], which prints it and exits 1; a demo that cannot open its window has nothing sensible
/// left to do.
fn show_one_frame(
    source: &mut IcosaFrameSource<'_>,
    config: &WindowConfig<'_>,
) -> anyhow::Result<bool> {
    let (ours, theirs) = UnixStream::pair()
        .map_err(|e| anyhow::anyhow!("creating this frame's liveness socket pair: {e}"))?;
    // `present`'s calloop callback reads `theirs` until WouldBlock on every readiness event; a
    // blocking fd would stall its event loop the first time that happens.
    theirs
        .set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("making this frame's liveness socket non-blocking: {e}"))?;

    // The timer that ends this frame's dwell time — see this function's doc, "The disconnect trap",
    // for why it must sleep BEFORE dropping `ours`, never after nothing. `move` hands `ours` into the
    // thread so nothing in this function can accidentally drop it early; the thread is intentionally
    // never joined (see below) — if the user closes the window before the sleep finishes, this thread
    // simply finishes its sleep in the background and drops an end whose peer is already gone, which
    // is harmless.
    let timer = std::thread::spawn(move || {
        std::thread::sleep(FRAME_DWELL);
        drop(ours);
    });

    let started = Instant::now();
    present(source, config, theirs)?;
    let elapsed = started.elapsed();

    // Deliberately not joined: joining would make this function block for up to the remainder of
    // FRAME_DWELL even when the user closed the window immediately, which is exactly the sluggish
    // exit a demo should not have. The thread finishes on its own shortly after and its only remaining
    // action (dropping an already-orphaned `ours`) has no observable effect on anything by then.
    drop(timer);

    let early_return_threshold = FRAME_DWELL.mul_f64(EARLY_RETURN_FRACTION);
    Ok(elapsed >= early_return_threshold)
}

/// The real body of [`main`]: bring up Vulkan, build the scene once, then keep reopening a window —
/// one animation frame at a time — until the user closes one of them.
///
/// # Errors
/// Returns an error if Vulkan bring-up or scene creation fails, or if any [`show_one_frame`] call
/// fails (see that function's doc for its own failure modes).
fn run() -> anyhow::Result<()> {
    // Bring Vulkan up exactly once. Which driver answers is the environment's decision, not this
    // program's — it probes nothing, matching both fixtures' own bring-up (`VulkanContext::new`'s
    // only environment interaction is `ash::Entry::load()` finding a driver).
    let context = VulkanContext::new()?;
    let frag_spirv = spirv_words(FRACTAL_FRAGMENT_SPIRV);
    // `None`: no sampler binding, because this program uses fixture B's fragment-shader fractal, not
    // fixture A's sampled texture — see this module's doc ("Why the GPU fixture's shader").
    let mut scene = Scene::new(&context, &frag_spirv, None)?;
    // Declared after `context` and `scene`, and dropped before either: `IcosaFrameSource` only
    // borrows, so its drop is a no-op regardless, but the ordering keeps every value in this function
    // consistent with the drop-order rule described in this module's doc.
    let mut source = IcosaFrameSource::new(&context, &mut scene);

    let config = WindowConfig {
        title: WINDOW_TITLE,
        app_id: WINDOW_APP_ID,
        // `false`, not because this source could export a dmabuf (it never claims to —
        // `FrameSource::supports_dmabuf`'s default is left untouched, see the crate's task brief),
        // but so `rayland-present`'s own log line names the real reason for the `wl_shm` path rather
        // than blaming a flag nothing here passes.
        force_shm: false,
    };

    println!(
        "rayland-icosa-window: opening a window; close it to exit (this is a demo, not a fixture — \
         see the crate's module docs)"
    );
    // Keep reopening a window, one frame at a time, until `show_one_frame` reports the user closed
    // one — see that function's doc for how it tells the two kinds of `present` return apart.
    while show_one_frame(&mut source, &config)? {}
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

    /// [`FrameSource::produce_pixels`] must return an upscaled, non-blank frame of the documented
    /// size — checked without ever opening a window, per this task's brief ("`produce_pixels` is
    /// testable without a window").
    ///
    /// # Why this cannot also test presentation
    /// It doesn't try to. Whether a window actually appears on a real compositor is not something any
    /// automated check in this repository can assert — see `rayland-present`'s own module doc
    /// ("verified... by a human looking at the screen — because no automated test can assert what a
    /// compositor actually painted"). This test's job stops at the boundary `FrameSource` draws: the
    /// pixels this program *would* show, not whether anything ever shows them.
    #[test]
    fn produce_pixels_upscales_and_is_not_blank() {
        if !Path::new(RENDER_NODE).exists() {
            eprintln!(
                "SKIP produce_pixels_upscales_and_is_not_blank: no render node at {RENDER_NODE}"
            );
            return;
        }

        let context = VulkanContext::new()
            .expect("Vulkan bring-up must succeed on a host with a render node");
        let frag_spirv = spirv_words(FRACTAL_FRAGMENT_SPIRV);
        let mut scene =
            Scene::new(&context, &frag_spirv, None).expect("scene creation must succeed");
        let mut source = IcosaFrameSource::new(&context, &mut scene);

        let expected_edge = IMAGE_SIZE * SCALE;
        assert_eq!(
            source.width(),
            expected_edge,
            "width() must report the upscaled edge, not the underlying IMAGE_SIZE"
        );
        assert_eq!(source.height(), expected_edge, "the image is square");

        let frame = source
            .produce_pixels()
            .expect("producing the first frame must succeed");
        assert_eq!(frame.width, expected_edge);
        assert_eq!(frame.height, expected_edge);
        assert_eq!(
            frame.pixels.len(),
            (expected_edge * expected_edge * 4) as usize,
            "RGBA8, tightly packed, no padding"
        );

        // "Not uniformly background": the render pass clears to opaque black (see
        // `rayland_icosa_vk::scene::Scene::draw`'s clear-value doc), so any non-black pixel proves the
        // upscaled frame actually shows the drawn solid rather than an empty clear.
        let background = [0u8, 0, 0, 255];
        let non_background_pixels = frame
            .pixels
            .chunks_exact(4)
            .filter(|pixel| *pixel != background)
            .count();
        assert!(
            non_background_pixels > 0,
            "the frame must show more than just the cleared background"
        );
    }
}
