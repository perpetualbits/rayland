//! The CPU fixture's **baseline**: the whole program draws the right pictures on this host's own
//! GPU, with this host's own Vulkan driver, and no remoting involved.
//!
//! # Why this must pass before the end-to-end test is even attempted
//! The end-to-end test spans an enormous amount of machinery. When something that long fails, the
//! expensive question is *which link broke*. This test removes every link but the first. If this
//! passes and the end-to-end test fails, the app is provably not the problem. That localisation is
//! the whole reason it is written and run first.
//!
//! # Why this drives the binary rather than the library
//! `rayland-icosa-vk`'s own test already proves the scaffolding renders. What is unproven here is
//! everything this crate adds: the frame loop, the per-frame fractal, the upload, the file naming
//! and the timing report. Only running the actual executable exercises those.

use image::GenericImageView;
use std::path::Path;
use std::process::Command;

/// The DRM render node this repository's GPU tests gate on.
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// The image edge length the app renders at. Asserted against the decoded PNG rather than assumed.
const IMAGE_SIZE: u32 = 256;

/// The app must render a shaded, textured solid: a substantial lit area, cleared corners.
///
/// A frame that is entirely background means the draw did not land. A corner that is *not*
/// background means the solid is the wrong size or the projection is wrong. Together they pin "a
/// solid of roughly the right size in the right place" without asserting a full hash, which is what
/// the end-to-end test is for.
///
/// # Why this checks a *fraction of non-background pixels* rather than the single centre pixel
/// The brief this test was written against checks exactly one pixel, `(IMAGE_SIZE/2,
/// IMAGE_SIZE/2)`, on the reasoning that the camera always looks straight at the solid so the
/// centre is always covered by a lit face. That reasoning is correct, but it is not enough on its
/// own: a lit, *textured* face's centre pixel is not automatically non-background, because the
/// fractal texture is not uniformly bright — the Mandelbrot set's own interior is painted pure black
/// (see `rayland_icosa_core::fractal::point_color`), the exact same RGBA8 bytes,
/// `[0, 0, 0, 255]`, as this render pass's clear colour. At frame 0 specifically, this is not a
/// hypothetical: independent verification (bypassing Vulkan entirely, replaying
/// `rayland_icosa_core::schedule::frame_mvp(0)`'s exact projection and the fixed camera in pure
/// Rust) shows screen pixel `(128, 128)` is covered by face 15, sampling fractal UV
/// `≈(0.712, 0.499)` — a point that lands inside a black bulb of the Mandelbrot set at
/// `TEXTURE_SIZE = 512`. Rendering this frame and inspecting the actual output confirms it: pixels
/// `(128±5, 128±5)` are *all* `[0, 0, 0, 255]`, a solid 11×11 block, not a one-pixel fluke — this is
/// the deterministic, reproducible consequence of `frame_mvp(0)`'s identity rotation landing the
/// projected centre of a bilaterally symmetric icosahedron exactly on a shared edge between two
/// mirror faces, both of which sample the same UV, which happens to sit inside a black bulb at this
/// run's `CENTER`/zoom. None of `rayland-icosa-core`'s own arithmetic is wrong — its own test suite
/// (`fills_every_pixel_opaquely`, `interior_points_are_black`, and the rest) already documents and
/// relies on exactly this "interior points are black" behaviour; it is simply a fact this test's
/// single fixed probe point did not anticipate.
///
/// The fix keeps the same diagnostic intent — "a real, lit, textured surface was drawn, not just
/// background left untouched" — without depending on any one texel's colour, which the fractal's
/// own structure makes unreliable: it counts how much of the frame differs from the background and
/// requires a substantial fraction (much more than any plausible sliver of coincidentally-black
/// texture). A real render of this scene covers roughly 27% of the frame with non-background pixels
/// (measured directly from a real run); a `10%` floor is comfortably below that measured value and
/// still far above what a broken draw (0%, an all-background frame) or a badly mis-sized solid could
/// produce.
#[test]
fn icosa_cpu_natively_renders_a_textured_solid() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!(
            "SKIP icosa_cpu_natively_renders_a_textured_solid: no render node at {RENDER_NODE}"
        );
        return;
    }

    let output_dir = std::env::temp_dir().join("rayland-icosa-cpu-native");
    // A stale directory from an earlier run would let a silently-failing binary "pass" on last
    // run's artefacts. Removing it makes the files' existence real evidence of this run.
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir).expect("the output directory must be creatable");

    let status = Command::new(env!("CARGO_BIN_EXE_rayland-icosa-cpu"))
        .arg(&output_dir)
        .status()
        .expect("the fixture binary must be launchable");
    assert!(
        status.success(),
        "the fixture must exit successfully on a host with a GPU; got {status}"
    );

    let frame = image::open(output_dir.join("frame_0000.png"))
        .expect("the app must have written a decodable PNG for frame 0");
    assert_eq!(
        frame.dimensions(),
        (IMAGE_SIZE, IMAGE_SIZE),
        "the app must render at its documented size"
    );

    // See this test's doc comment for why a fraction, not one fixed pixel, is the robust check.
    let background = image::Rgba([0, 0, 0, 255]);
    let non_background_pixels = frame
        .pixels()
        .filter(|(_, _, pixel)| *pixel != background)
        .count();
    let total_pixels = (IMAGE_SIZE * IMAGE_SIZE) as usize;
    let non_background_fraction = non_background_pixels as f64 / total_pixels as f64;
    assert!(
        non_background_fraction > 0.10,
        "at least 10% of the frame must be covered by lit, textured surface, not background; got \
         {:.1}% ({non_background_pixels}/{total_pixels} pixels)",
        non_background_fraction * 100.0
    );

    let last = IMAGE_SIZE - 1;
    for (x, y, label) in [
        (0, 0, "top-left"),
        (last, 0, "top-right"),
        (0, last, "bottom-left"),
        (last, last, "bottom-right"),
    ] {
        assert_eq!(
            frame.get_pixel(x, y),
            image::Rgba([0, 0, 0, 255]),
            "the {label} corner must still show the cleared background"
        );
    }
}

/// The app must render all 120 frames, and they must differ from one another.
///
/// "All 120 exist" catches a loop that stops early. "Frame 0 and frame 119 differ" catches the
/// far more insidious failure: a loop that runs but re-uploads the same texture every time, or
/// never updates the matrix — which would produce 120 identical files that pass every other check
/// here and would make the end-to-end comparison meaningless, since a static picture is trivially
/// easy to transport correctly.
#[test]
fn icosa_cpu_renders_all_frames_and_they_differ() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!(
            "SKIP icosa_cpu_renders_all_frames_and_they_differ: no render node at {RENDER_NODE}"
        );
        return;
    }

    let output_dir = std::env::temp_dir().join("rayland-icosa-cpu-frames");
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir).expect("the output directory must be creatable");

    let status = Command::new(env!("CARGO_BIN_EXE_rayland-icosa-cpu"))
        .arg(&output_dir)
        .status()
        .expect("the fixture binary must be launchable");
    assert!(
        status.success(),
        "the fixture must exit successfully; got {status}"
    );

    for frame in 0..120u32 {
        let path = output_dir.join(format!("frame_{frame:04}.png"));
        assert!(
            path.exists(),
            "frame {frame} must have been written to {path:?}"
        );
    }

    let first = std::fs::read(output_dir.join("frame_0000.png")).expect("frame 0 must be readable");
    let last =
        std::fs::read(output_dir.join("frame_0119.png")).expect("frame 119 must be readable");
    assert_ne!(
        first, last,
        "the solid must rotate and the fractal must zoom — 120 identical frames would mean neither is happening"
    );
}

/// The app must print one CSV line per frame, with a header.
///
/// The timing report is the fixture's other output, and the reason a *pair* of fixtures exists at
/// all: the comparison between them is a comparison of numbers. A run that silently stopped
/// printing them would still pass every image check above.
#[test]
fn icosa_cpu_prints_a_timing_report() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!("SKIP icosa_cpu_prints_a_timing_report: no render node at {RENDER_NODE}");
        return;
    }

    let output_dir = std::env::temp_dir().join("rayland-icosa-cpu-timing");
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir).expect("the output directory must be creatable");

    let output = Command::new(env!("CARGO_BIN_EXE_rayland-icosa-cpu"))
        .arg(&output_dir)
        .output()
        .expect("the fixture binary must be launchable");
    let stdout = String::from_utf8(output.stdout).expect("the timing report must be valid UTF-8");
    let lines: Vec<&str> = stdout.lines().collect();

    assert_eq!(
        lines[0], "frame,fractal_us,upload_us,draw_readback_us",
        "the report must start with its header"
    );
    assert_eq!(lines.len(), 121, "one header line plus one line per frame");
    // Spot-check the shape of a data line rather than its values, which are timings and vary.
    let fields: Vec<&str> = lines[1].split(',').collect();
    assert_eq!(fields.len(), 4, "each data line must have four fields");
    assert_eq!(fields[0], "0", "the first data line must be frame 0");
}
