//! The GPU fixture's **baseline**: the whole program draws the right pictures on this host's own
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
//! everything this crate adds: the frame loop, the per-frame uniform preparation, the file naming
//! and the timing report. Only running the actual executable exercises those.
//!
//! # Why this file duplicates `rayland-icosa-cpu`'s test almost verbatim
//! Deliberately. Both fixtures draw the same scene, so both baselines check the same things — a
//! shared test helper would have to live in a crate both fixtures depend on, and a fixture's
//! baseline test is exactly the thing that must prove "this program renders correctly with nothing
//! else involved" without importing machinery that could itself be the thing under test. The cost
//! is two similar files that drift only if someone edits one and not the other; the benefit is that
//! each fixture's baseline stands on its own.

use image::GenericImageView;
use std::path::Path;
use std::process::Command;

/// The DRM render node this repository's GPU tests gate on.
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// The image edge length the app renders at. Asserted against the decoded PNG rather than assumed.
const IMAGE_SIZE: u32 = 256;

/// The app must render a shaded solid whose surface colour comes from the fractal: a substantial
/// lit area, cleared corners.
///
/// A frame that is entirely background means the draw did not land. A corner that is *not*
/// background means the solid is the wrong size or the projection is wrong. Together they pin "a
/// solid of roughly the right size in the right place" without asserting a full hash, which is what
/// the end-to-end test is for.
///
/// # Why this checks a *fraction of non-background pixels* rather than the single centre pixel
/// Fixture A's own test doc comment ("Why this checks a *fraction* of non-background pixels rather
/// than the single centre pixel") explains the general hazard in detail: `frame_mvp(0)`'s identity
/// rotation lands the projected screen centre on a shared edge between two mirror faces, both
/// sampling a point that this run's fixed `CENTER`/zoom puts inside a black bulb of the Mandelbrot
/// set — so the centre pixel is legitimately black at frame 0, in *both* fixtures, since both draw
/// the identical schedule and the identical fractal centre. This fixture's fragment shader computes
/// the same escape-time function fixture A's CPU path does (see `shaders/icosa_fractal.frag`'s
/// header), so it lands on the same interior point and would fail a single-pixel check the same way.
/// A non-background *fraction* sidesteps that without depending on any one pixel's colour.
///
/// The `10%` floor matches fixture A's: a real render of this scene on this host covers 26.67% of
/// the frame with non-background pixels (measured directly, frame 0: 17,477 of 65,536 pixels — see
/// this crate's task report), comfortably above the floor and far above what a broken draw (0%) or
/// a badly mis-sized solid could produce.
///
/// # Why this also counts distinct colours
/// The checks above pin the solid's *silhouette* — its size and position — but say nothing about
/// its *content*. A fragment shader that emitted a constant colour instead of evaluating the
/// fractal would still light up the correct fraction of the frame with the correct silhouette and
/// pass every check above — per-face Lambert shading alone still varies brightness across faces.
/// What only the *fractal itself* produces is fine-grained colour variation *within* a face, from
/// its escape-time gradient. This is the direct GPU-fixture analogue of fixture A's identical check,
/// and it exists for the identical reason: without it, a shader that read `frag_normal` and ignored
/// `u.center`/`u.half_width` entirely would still pass every geometric check. Measured directly on
/// this host: frame 0 of a real render shows 2,053 distinct non-background RGBA colours (see this
/// crate's task report for the transcript, and for the constant-colour mutation that confirms the
/// `100`-colour floor below actually bites — it is well over an order of magnitude below the real
/// count and, as with fixture A, comfortably above what flat per-face shading alone can produce).
#[test]
fn icosa_gpu_natively_renders_a_fractal_solid() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!(
            "SKIP icosa_gpu_natively_renders_a_fractal_solid: no render node at {RENDER_NODE}"
        );
        return;
    }

    let output_dir = std::env::temp_dir().join("rayland-icosa-gpu-native");
    // A stale directory from an earlier run would let a silently-failing binary "pass" on last
    // run's artefacts. Removing it makes the files' existence real evidence of this run.
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir).expect("the output directory must be creatable");

    let status = Command::new(env!("CARGO_BIN_EXE_rayland-icosa-gpu"))
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
        "at least 10% of the frame must be covered by lit, fractal-shaded surface, not background; \
         got {:.1}% ({non_background_pixels}/{total_pixels} pixels)",
        non_background_fraction * 100.0
    );

    // Distinct-colour content check (see this test's doc comment, "Why this also counts distinct
    // colours"). A solid-coloured shader would still pass every check above — the silhouette's size
    // and position say nothing about what is painted on it — so this counts how many different
    // colours appear on the lit solid and requires far more than flat shading alone could ever
    // produce.
    let distinct_colors: std::collections::HashSet<image::Rgba<u8>> = frame
        .pixels()
        .filter(|(_, _, pixel)| *pixel != background)
        .map(|(_, _, pixel)| pixel)
        .collect();
    assert!(
        distinct_colors.len() > 100,
        "the lit solid must show substantial colour variation from the evaluated fractal, not a \
         flat or near-flat shade per face; got {} distinct non-background colours",
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
/// far more insidious failure: a loop that runs but never updates the uniforms — which would
/// produce 120 identical files that pass every other check here and would make the end-to-end
/// comparison meaningless, since a static picture is trivially easy to transport correctly.
#[test]
fn icosa_gpu_renders_all_frames_and_they_differ() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!(
            "SKIP icosa_gpu_renders_all_frames_and_they_differ: no render node at {RENDER_NODE}"
        );
        return;
    }

    let output_dir = std::env::temp_dir().join("rayland-icosa-gpu-frames");
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir).expect("the output directory must be creatable");

    let status = Command::new(env!("CARGO_BIN_EXE_rayland-icosa-gpu"))
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
fn icosa_gpu_prints_a_timing_report() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!("SKIP icosa_gpu_prints_a_timing_report: no render node at {RENDER_NODE}");
        return;
    }

    let output_dir = std::env::temp_dir().join("rayland-icosa-gpu-timing");
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir).expect("the output directory must be creatable");

    let output = Command::new(env!("CARGO_BIN_EXE_rayland-icosa-gpu"))
        .arg(&output_dir)
        .output()
        .expect("the fixture binary must be launchable");
    let stdout = String::from_utf8(output.stdout).expect("the timing report must be valid UTF-8");
    let lines: Vec<&str> = stdout.lines().collect();

    assert_eq!(
        lines[0], "frame,fractal_us,upload_us,draw_readback_us",
        "the report must start with its header, identical to fixture A's for direct diffing"
    );
    assert_eq!(lines.len(), 121, "one header line plus one line per frame");
    // Spot-check the shape of a data line rather than its values, which are timings and vary.
    let fields: Vec<&str> = lines[1].split(',').collect();
    assert_eq!(fields.len(), 4, "each data line must have four fields");
    assert_eq!(fields[0], "0", "the first data line must be frame 0");
    assert_eq!(
        fields[2], "0",
        "upload_us must always be 0: this fixture has no texture and therefore no upload"
    );
}

/// A `khronos_validation` settings file, written by this test into a temporary directory and
/// pointed to via `VK_LAYER_SETTINGS_PATH`.
///
/// `debug_action = VK_DBG_LAYER_ACTION_LOG_MSG` plus `log_filename = stdout` is what actually gives
/// the layer somewhere to report to (see [`validation_layer_reports_no_errors_across_a_full_run`]'s
/// doc comment for why this is not optional, and `rayland-icosa-cpu`'s identical constant for the
/// full story of why an earlier validation run without it was vacuous). `validate_sync = true` is
/// requested for completeness, matching fixture A's settings exactly so the two fixtures are run
/// under identical validation configuration.
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
/// # Why this test exists: a predecessor's validation run (in fixture A) was vacuous
/// See `rayland-icosa-cpu`'s identical test for the full story: an app that creates no
/// `VK_EXT_debug_utils` messenger gives the validation layer no reporting sink at all once
/// force-loaded, so it prints nothing whether the code is right or wrong, unless a
/// `VK_LAYER_SETTINGS_PATH` settings file requests the legacy `debug_action =
/// VK_DBG_LAYER_ACTION_LOG_MSG` reporting path instead (see [`VALIDATION_LAYER_SETTINGS`]). This
/// fixture's binary is exactly as silent about its own environment as fixture A's — see this
/// crate's module doc — so the identical hazard applies here, and the identical fix is copied
/// across rather than reinvented.
///
/// Confirmed directly, not just argued, while writing this test: with this exact settings file, a
/// correct build produces zero `Validation Error` lines over a full 120-frame run. Deliberately
/// breaking the pipeline's descriptor-set-layout/shader agreement (temporarily requesting the
/// binding-1 sampler layout while supplying `None` to `Scene::new`, so the layout the descriptor
/// set is allocated against does not match what a caller unconditionally treats as safe) and
/// rerunning the same command reproduces validation errors, confirming both that this mismatch is
/// load-bearing and that this test catches its introduction; see this crate's task report for the
/// full transcript of both runs. That mutation was reverted immediately after being observed.
///
/// # Why this does not also catch a hypothetical uniform-write race
/// It doesn't, and shouldn't be expected to, for the same reason fixture A's identical test doesn't
/// catch its own staging-buffer race: sync validation cannot observe a host write through a
/// persistently-mapped `HOST_COHERENT` buffer with no flush, because there is no Vulkan API call
/// for it to hook. `Scene::draw`'s own contract (see that method's doc in `rayland-icosa-vk`) is
/// what actually prevents a race here — every `draw` call fence-waits before returning, so the next
/// frame's uniform write can never race the previous frame's GPU read — but that contract is a
/// property of the shared scaffolding, not something this or any validation layer test can observe.
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

    let work_dir = std::env::temp_dir().join("rayland-icosa-gpu-validation");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).expect("the work directory must be creatable");

    // Written fresh every run rather than checked in: the settings' content is this test's own
    // contract with the layer, and keeping it inline keeps that contract visible in one place.
    let settings_path = work_dir.join("vk_layer_settings.txt");
    std::fs::write(&settings_path, VALIDATION_LAYER_SETTINGS)
        .expect("the layer settings file must be writable");

    let output_dir = work_dir.join("frames");
    std::fs::create_dir_all(&output_dir).expect("the frame output directory must be creatable");

    let output = Command::new(env!("CARGO_BIN_EXE_rayland-icosa-gpu"))
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
