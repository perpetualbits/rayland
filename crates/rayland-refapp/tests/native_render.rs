//! The reference application's **baseline**: it draws the right picture on this host's own GPU,
//! with this host's own Vulkan driver, and nothing else involved.
//!
//! # Why this test exists, and why it must pass before the Venus one is even attempted
//! C0's end-to-end proof (`rayland-engine/tests/refapp_venus_e2e.rs`) launches this same binary
//! with Mesa's Venus ICD pointed at Rayland's engine, and asserts the resulting PNG shows a red
//! triangle on blue. That test spans an enormous amount of machinery: the app, Mesa's Venus ICD,
//! a Unix socket, the vtest protocol, our engine, virglrenderer, and the GPU driver underneath.
//! When something that long fails, the expensive question is *which link broke*.
//!
//! This test removes every link but the first. It runs the app against the host's ordinary ICD —
//! no Venus, no socket, no Rayland — and asserts the exact same pixels. So if this passes and the
//! end-to-end test fails, the app is provably not the problem and the fault is somewhere in the
//! remoting path. That localization is the whole reason this test is written and run *first*; it
//! is worth very little afterwards and a great deal beforehand.
//!
//! # Skip, don't fail, without a GPU
//! The app needs a real Vulkan device. Following the convention of this repository's other GPU
//! tests, absence is reported as a SKIP rather than a failure, so CI without a render node stays
//! green and stays light.

// Reading the PNG the binary produced. This is deliberately a *separate* decode of the file the
// app wrote, rather than an inspection of the app's in-memory pixels: it checks the artifact the
// end-to-end test will later compare against, encoder included.
use image::GenericImageView;
// Locating and launching the binary under test, and placing its output somewhere writable.
use std::path::Path;
use std::process::Command;

/// The DRM render node this repository's GPU tests gate on.
///
/// The reference app itself never looks at this path — it asks the Vulkan loader for a device and
/// takes what it is given. The node is checked here only as this repository's established proxy
/// for "there is a real GPU on this host", matching `rayland-engine`'s `virgl_available` gating so
/// that the two halves of C0's proof skip under the same conditions rather than one silently
/// running while the other does not.
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// The image size the app renders, and which the assertions below index into. Must agree with
/// `rayland-refapp`'s own `IMAGE_WIDTH`/`IMAGE_HEIGHT`; asserted against the decoded PNG rather
/// than assumed, so a change on one side fails loudly here instead of silently shifting which
/// pixels "centre" and "corner" name.
const IMAGE_SIZE: u32 = 64;

/// Assert that `pixel` is the fully-opaque red the app draws its triangle in.
///
/// Exact equality is correct here, not overly strict: the fragment shader writes a constant
/// `(1, 0, 0, 1)`, the render target is `R8G8B8A8_UNORM`, and `1.0` in UNORM is exactly 255 — no
/// interpolation, no blending, and no rounding stands between the shader and the byte. A tolerance
/// would only hide a real defect.
fn assert_red(pixel: image::Rgba<u8>, label: &str) {
    assert_eq!(
        pixel,
        image::Rgba([255, 0, 0, 255]),
        "{label} must be the triangle's opaque red"
    );
}

/// Assert that `pixel` is the fully-opaque blue the app clears the image to.
///
/// Exact equality, for the same reason as [`assert_red`]: the clear value is a constant
/// `(0, 0, 1, 1)` and lands on exact UNORM bytes.
fn assert_blue(pixel: image::Rgba<u8>, label: &str) {
    assert_eq!(
        pixel,
        image::Rgba([0, 0, 255, 255]),
        "{label} must be the cleared opaque blue"
    );
}

/// The whole app, natively: run the binary, decode its PNG, and check the picture.
///
/// The checks are the ones C0 has used since SP0 — **centre red, corners blue** — chosen because
/// they distinguish the two failures that actually happen in practice. A centre that is not red
/// means the draw did not land (no geometry, wrong viewport, culled winding). A corner that is not
/// blue means the clear did not happen or the triangle is not the size it should be. Together they
/// pin down "a triangle of roughly the right shape, in the right colours, in the right place"
/// without asserting on a full pixel hash, which would be brittle across drivers for no benefit.
#[test]
fn refapp_natively_renders_a_red_triangle_on_blue() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!(
            "SKIP refapp_natively_renders_a_red_triangle_on_blue: no render node at {RENDER_NODE}"
        );
        return;
    }

    // Write next to the test binary rather than into the source tree: `target/` is guaranteed
    // writable and is already ignored by git, so a failed run leaves the PNG behind for inspection
    // without dirtying the working tree.
    let output = std::env::temp_dir().join("rayland-refapp-native.png");
    // A stale PNG from an earlier run would let a silently-failing binary "pass" on last run's
    // artifact. Removing it first makes the file's existence below real evidence that this run
    // produced it. A missing file is the normal case, not an error.
    let _ = std::fs::remove_file(&output);

    // `CARGO_BIN_EXE_<name>` is set by Cargo for this package's own binaries, so the test always
    // runs the executable built from the source next to it — no path guessing, no stale binary.
    let status = Command::new(env!("CARGO_BIN_EXE_rayland-refapp"))
        .arg(&output)
        .status()
        .expect("the reference app binary must be launchable");
    assert!(
        status.success(),
        "the reference app must exit successfully on a host with a GPU; got {status}"
    );

    let image = image::open(&output).expect("the app must have written a decodable PNG");
    assert_eq!(
        image.dimensions(),
        (IMAGE_SIZE, IMAGE_SIZE),
        "the app must render at its documented size"
    );

    // The centre lies well inside the triangle, whose vertices span roughly half the image.
    assert_red(
        image.get_pixel(IMAGE_SIZE / 2, IMAGE_SIZE / 2),
        "the centre",
    );

    // All four corners lie outside the triangle, so each must still show the clear colour. All
    // four are checked rather than one because a single corner cannot distinguish "the clear
    // worked" from "the image is rotated or flipped" — an axis mix-up that shows up nowhere else
    // in a symmetric-looking test.
    let last = IMAGE_SIZE - 1;
    assert_blue(image.get_pixel(0, 0), "the top-left corner");
    assert_blue(image.get_pixel(last, 0), "the top-right corner");
    assert_blue(image.get_pixel(0, last), "the bottom-left corner");
    assert_blue(image.get_pixel(last, last), "the bottom-right corner");

    eprintln!(
        "OK: the reference app renders a red triangle on blue natively, with no Rayland involved"
    );
}
