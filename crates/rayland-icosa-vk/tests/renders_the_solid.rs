//! The scaffolding's **baseline**: it draws a correct, depth-tested, shaded solid on this host's
//! own GPU, with this host's own Vulkan driver, and neither fixture involved.
//!
//! # Why this is tested here rather than only through the fixtures
//! Everything in this crate is shared by both fixtures, which means a defect here shows up as
//! *both* of them being wrong — and two fixtures failing together is a far more confusing signal
//! than one library failing alone. Proving the scaffolding independently means that when a fixture
//! misbehaves later, this code is already excluded.
//!
//! # Skip, don't fail, without a GPU
//! Following this repository's convention for GPU tests, absence of a render node is reported as a
//! SKIP rather than a failure, so CI without one stays green and stays light.

use rayland_icosa_core::{IMAGE_SIZE, schedule::frame_mvp};
use rayland_icosa_vk::{Scene, Uniforms, VulkanContext};
use std::path::Path;

/// The DRM render node this repository's GPU tests gate on.
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// The flat-shading fragment shader, which needs no texture — so this test exercises the
/// scaffolding without depending on either fixture's fractal.
const FLAT_FRAGMENT_SPIRV: &[u8] = include_bytes!("../../../shaders/icosa_flat.frag.spv");

/// Read a pixel out of a readback buffer as RGBA.
fn pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let offset = ((y * IMAGE_SIZE + x) * 4) as usize;
    [
        pixels[offset],
        pixels[offset + 1],
        pixels[offset + 2],
        pixels[offset + 3],
    ]
}

/// The scaffolding must draw a shaded solid: lit centre, cleared corners.
///
/// The checks are chosen to distinguish the failures that actually happen. A centre that matches the
/// background means the draw did not land — no geometry, wrong viewport, or every face culled. A
/// corner that is *not* background means the solid is the wrong size or the projection is wrong.
/// Together they pin "a solid of roughly the right size in the right place" without asserting a full
/// hash, which is what the end-to-end tests are for.
#[test]
fn the_scaffolding_renders_a_shaded_solid() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!("SKIP the_scaffolding_renders_a_shaded_solid: no render node at {RENDER_NODE}");
        return;
    }

    let context =
        VulkanContext::new().expect("a host with a render node must give a Vulkan device");
    let spirv = ash::util::read_spv(&mut std::io::Cursor::new(FLAT_FRAGMENT_SPIRV))
        .expect("the committed SPIR-V must be readable");
    let mut scene = Scene::new(&context, &spirv, None).expect("the scene must build");

    let pixels = scene
        .draw(
            &context,
            &Uniforms {
                mvp: frame_mvp(0),
                half_width: 1.5,
                center: [0.0, 0.0],
            },
        )
        .expect("the draw must succeed");
    assert_eq!(
        pixels.len(),
        (IMAGE_SIZE * IMAGE_SIZE * 4) as usize,
        "the readback must be the documented size"
    );

    // The camera looks straight at the solid, so the image centre is always covered by a face.
    assert_ne!(
        pixel(&pixels, IMAGE_SIZE / 2, IMAGE_SIZE / 2),
        [0, 0, 0, 255],
        "the centre must be covered by a lit face, not the cleared background"
    );

    // All four corners lie outside the solid's silhouette. All four are checked because a single
    // corner cannot distinguish "the clear worked" from "the image is flipped".
    let last = IMAGE_SIZE - 1;
    for (x, y, label) in [
        (0, 0, "top-left"),
        (last, 0, "top-right"),
        (0, last, "bottom-left"),
        (last, last, "bottom-right"),
    ] {
        assert_eq!(
            pixel(&pixels, x, y),
            [0, 0, 0, 255],
            "the {label} corner must still show the cleared background"
        );
    }

    eprintln!("OK: the scaffolding renders a shaded, depth-tested solid");
}

/// Two draws of the same uniforms must produce the same pixels.
///
/// The scaffolding reuses one command buffer and one set of targets across frames, and the obvious
/// bug there is state left behind — an un-cleared depth buffer, a stale descriptor. That would show
/// up as frame N depending on frame N-1, which is fatal to a fixture whose entire premise is that
/// frame N is a pure function of N.
#[test]
fn drawing_twice_gives_the_same_pixels() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!("SKIP drawing_twice_gives_the_same_pixels: no render node at {RENDER_NODE}");
        return;
    }

    let context =
        VulkanContext::new().expect("a host with a render node must give a Vulkan device");
    let spirv = ash::util::read_spv(&mut std::io::Cursor::new(FLAT_FRAGMENT_SPIRV))
        .expect("the committed SPIR-V must be readable");
    let mut scene = Scene::new(&context, &spirv, None).expect("the scene must build");

    let uniforms = Uniforms {
        mvp: frame_mvp(0),
        half_width: 1.5,
        center: [0.0, 0.0],
    };
    let first = scene
        .draw(&context, &uniforms)
        .expect("the first draw must succeed");
    // A different frame in between, so the second draw of frame 0 has to actually reset state
    // rather than passively still be showing it.
    let _ = scene
        .draw(
            &context,
            &Uniforms {
                mvp: frame_mvp(37),
                half_width: 1.5,
                center: [0.0, 0.0],
            },
        )
        .expect("the intervening draw must succeed");
    let second = scene
        .draw(&context, &uniforms)
        .expect("the second draw must succeed");

    assert_eq!(
        first, second,
        "the same uniforms must always produce the same pixels"
    );
}

/// A different orientation must produce a different picture.
#[test]
fn a_different_orientation_gives_a_different_picture() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!(
            "SKIP a_different_orientation_gives_a_different_picture: no render node at {RENDER_NODE}"
        );
        return;
    }

    let context =
        VulkanContext::new().expect("a host with a render node must give a Vulkan device");
    let spirv = ash::util::read_spv(&mut std::io::Cursor::new(FLAT_FRAGMENT_SPIRV))
        .expect("the committed SPIR-V must be readable");
    let mut scene = Scene::new(&context, &spirv, None).expect("the scene must build");

    let first = scene
        .draw(
            &context,
            &Uniforms {
                mvp: frame_mvp(0),
                half_width: 1.5,
                center: [0.0, 0.0],
            },
        )
        .expect("the draw must succeed");
    let later = scene
        .draw(
            &context,
            &Uniforms {
                mvp: frame_mvp(60),
                half_width: 1.5,
                center: [0.0, 0.0],
            },
        )
        .expect("the draw must succeed");

    assert_ne!(first, later, "the mvp must actually reach the shader");
}
