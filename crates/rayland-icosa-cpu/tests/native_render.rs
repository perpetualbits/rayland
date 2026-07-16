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
///
/// # Why this also counts distinct colours
/// The checks above pin the solid's *silhouette* — its size and position — but say nothing about
/// its *content*. A fragment shader that sampled a constant colour instead of the fractal texture,
/// or an upload that never actually ran (leaving the image's `UNDEFINED`-layout garbage, or whatever
/// was in device memory before), would still light up the correct fraction of the frame with the
/// correct silhouette and pass every check above — per-face Lambert shading alone still varies
/// brightness across faces. What only the *fractal itself* produces is fine-grained colour variation
/// *within* a face, from its escape-time gradient. Measured directly on a real render: frame 0 shows
/// 2,860 distinct non-background RGBA colours; a build mutated to sample a constant colour instead
/// (verified the same way, see this crate's task report) shows 7 — the few flat shades one per
/// visible face, no more. A `100`-colour floor sits nowhere near either boundary: two orders of
/// magnitude below a real render and well over an order of magnitude above what flat shading alone
/// can produce.
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

    // Distinct-colour content check (see this test's doc comment, "Why this also counts distinct
    // colours"). A solid-coloured or unuploaded texture would still pass every check above — the
    // silhouette's size and position say nothing about what is painted on it — so this counts how
    // many different colours appear on the lit solid and requires far more than flat shading alone
    // could ever produce.
    let distinct_colors: std::collections::HashSet<image::Rgba<u8>> = frame
        .pixels()
        .filter(|(_, _, pixel)| *pixel != background)
        .map(|(_, _, pixel)| pixel)
        .collect();
    assert!(
        distinct_colors.len() > 100,
        "the lit solid must show substantial colour variation from the sampled fractal, not a flat \
         or near-flat shade per face; got {} distinct non-background colours",
        distinct_colors.len()
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

/// A `khronos_validation` settings file, written by this test into a temporary directory and
/// pointed to via `VK_LAYER_SETTINGS_PATH`.
///
/// `debug_action = VK_DBG_LAYER_ACTION_LOG_MSG` plus `log_filename = stdout` is what actually gives
/// the layer somewhere to report to (see [`validation_layer_reports_no_errors_across_a_full_run`]'s
/// doc comment for why this is not optional). `validate_sync = true` is requested for completeness;
/// see this crate's task report (`.superpowers/sdd/task-6-report.md`) for why sync validation is
/// specifically the wrong tool for this fixture's staging-buffer hazard, and is expected to stay
/// silent regardless of that hazard's presence.
const VALIDATION_LAYER_SETTINGS: &str = "\
khronos_validation.debug_action = VK_DBG_LAYER_ACTION_LOG_MSG\n\
khronos_validation.log_filename = stdout\n\
khronos_validation.validate_core = true\n\
khronos_validation.validate_sync = true\n\
";

/// True if the Khronos Validation Layer's Vulkan-loader manifest is present in any location the
/// loader itself would search.
///
/// This is a filesystem probe, not a Vulkan call — matching this file's [`RENDER_NODE`] convention
/// of skipping (not failing) a GPU-dependent test by checking for the resource's presence up front,
/// rather than by launching Vulkan and interpreting an error. `$VK_LAYER_PATH`, when set, replaces
/// the loader's default search path, so it is checked first and exclusively-first among the
/// directories tried; the remaining paths are the standard Linux locations for explicit layer
/// manifests (system-wide, then a local-to-root install, then per-user).
fn khronos_validation_layer_installed() -> bool {
    let mut directories: Vec<std::path::PathBuf> = Vec::new();
    if let Some(layer_path) = std::env::var_os("VK_LAYER_PATH") {
        directories.extend(std::env::split_paths(&layer_path));
    }
    directories.push(std::path::PathBuf::from(
        "/usr/share/vulkan/explicit_layer.d",
    ));
    directories.push(std::path::PathBuf::from(
        "/usr/local/share/vulkan/explicit_layer.d",
    ));
    if let Some(home) = std::env::var_os("HOME") {
        directories
            .push(std::path::PathBuf::from(home).join(".local/share/vulkan/explicit_layer.d"));
    }
    directories.iter().any(|directory| {
        std::fs::read_dir(directory)
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("VkLayer_khronos_validation")
            })
    })
}

/// Running under Khronos Validation, with sync validation on, the app must report zero
/// `Validation Error`s across a full 120-frame run.
///
/// # Why this test exists: a predecessor's validation run was vacuous
/// An earlier report concluded that removing `texture.rs`'s second `cmd_pipeline_barrier` — the
/// `TRANSFER_DST_OPTIMAL -> SHADER_READ_ONLY_OPTIMAL` transition the draw's sampled read depends on
/// — was "not caught" by the validation layer, and treated that as evidence the mutation was benign.
/// It is not benign, and the negative result was an artefact of how the layer was invoked, not a
/// fact about the code: this app creates no `VK_EXT_debug_utils` messenger (an ordinary Vulkan
/// program has no reason to), so once the layer is force-loaded (`VK_LOADER_LAYERS_ENABLE` or
/// `VK_INSTANCE_LAYERS`) it still has **no reporting sink** — it runs every check exactly as
/// configured and prints nothing, on a correct build and a broken one alike. Only a
/// `VK_LAYER_SETTINGS_PATH` settings file that requests `debug_action = VK_DBG_LAYER_ACTION_LOG_MSG`
/// (a legacy, sink-agnostic reporting path the layer still honours without any messenger) makes the
/// layer's findings observable at all — see [`VALIDATION_LAYER_SETTINGS`].
///
/// Confirmed directly, not just argued, while writing this test: with this exact settings file, a
/// correct build produces zero `Validation Error` lines over a full 120-frame run (121 lines of
/// plain CSV, matching [`icosa_cpu_prints_a_timing_report`]'s expectation, and nothing else).
/// Commenting out `texture.rs`'s second barrier and rerunning the same command produces ten
/// `Validation Error: [ VUID-vkCmdDraw-None-09600 ]` lines — one per draw call whose descriptor set
/// was written against `SHADER_READ_ONLY_OPTIMAL` while the image was actually left in
/// `TRANSFER_DST_OPTIMAL` — confirming both that the barrier is load-bearing and that this test
/// catches its removal. That mutation was reverted immediately after being observed; see this
/// crate's task report for the full transcript of both runs.
///
/// # Why this does not also catch the staging-buffer fence-wait removal
/// It doesn't, and shouldn't be expected to: sync validation (`validate_sync = true`, requested
/// above) cannot observe a host write through a persistently-mapped `HOST_COHERENT` buffer with no
/// flush, because there is no Vulkan API call for it to hook — the write is a bare memory store
/// through a pointer `vkMapMemory` handed back once, entirely outside anything the loader or a layer
/// ever sees. That is not a gap in this test; it is verbatim the thing this whole fixture exists to
/// demonstrate (see this crate's own module doc, and `.superpowers/sdd/task-6-report.md`'s corrected
/// account of that mutation).
///
/// # Skip, don't fail, without the render node or the layer
/// Following this repository's convention for GPU tests, either missing dependency is reported as a
/// SKIP rather than a failure, so CI without a GPU or without the validation layer installed stays
/// green.
#[test]
fn validation_layer_reports_no_errors_across_a_full_run() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!(
            "SKIP validation_layer_reports_no_errors_across_a_full_run: no render node at \
             {RENDER_NODE}"
        );
        return;
    }
    if !khronos_validation_layer_installed() {
        eprintln!(
            "SKIP validation_layer_reports_no_errors_across_a_full_run: \
             VK_LAYER_KHRONOS_validation not installed"
        );
        return;
    }

    let work_dir = std::env::temp_dir().join("rayland-icosa-cpu-validation");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).expect("the work directory must be creatable");

    // Written fresh every run rather than checked in: the settings' content is this test's own
    // contract with the layer, and keeping it inline keeps that contract visible in one place.
    let settings_path = work_dir.join("vk_layer_settings.txt");
    std::fs::write(&settings_path, VALIDATION_LAYER_SETTINGS)
        .expect("the layer settings file must be writable");

    let output_dir = work_dir.join("frames");
    std::fs::create_dir_all(&output_dir).expect("the frame output directory must be creatable");

    let output = Command::new(env!("CARGO_BIN_EXE_rayland-icosa-cpu"))
        .arg(&output_dir)
        // Forces the loader to insert the layer even though this app itself requests no layers —
        // an ordinary Vulkan program, by design, never asks for validation (see this crate's module
        // doc on why this fixture is deliberately ignorant of its environment).
        .env("VK_LOADER_LAYERS_ENABLE", "*validation")
        .env("VK_LAYER_SETTINGS_PATH", &settings_path)
        .output()
        .expect("the fixture binary must be launchable under the validation layer");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the fixture must exit successfully under validation; got {}\nstdout:\n{stdout}\nstderr:\n\
         {stderr}",
        output.status
    );

    // `log_filename = stdout` above sends every message here; both streams are checked anyway in
    // case a different loader/layer version ever redirects logging to stderr instead.
    let error_lines: Vec<&str> = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| line.contains("Validation Error"))
        .collect();
    assert!(
        error_lines.is_empty(),
        "the validation layer reported {} error(s) across the run:\n{}",
        error_lines.len(),
        error_lines.join("\n")
    );
}
