//! **An ordinary off-screen Vulkan program.** It draws one red triangle on a blue background at
//! 64×64, reads the pixels back, and writes them to a PNG. That is all it does.
//!
//! # Why a project about remote GPU rendering contains a plain Vulkan triangle
//! C0's headline claim is that a *real, unmodified* Vulkan application can run on machine C while
//! its rendering happens on machine S's GPU, by shipping the **command stream** rather than
//! pixels. Proving that claim needs an application that is genuinely unmodified — and the only way
//! to be certain an application has not been adapted to the remoting is for it to have no
//! knowledge of the remoting to adapt to. So this program has none. It depends on no `rayland-*`
//! crate. It does not mention Venus, vtest, virglrenderer, sockets, or remoting anywhere, and it
//! cannot tell whether it is being remoted.
//!
//! Everything that makes the remote path happen lives in the **environment** the binary is
//! launched with — which Vulkan driver the loader picks up, and where that driver sends its
//! commands. Run normally, this program renders on the local GPU. Run with Mesa's Venus ICD
//! pointed at Rayland's engine, exactly the same binary renders through Rayland instead, and
//! produces the same PNG. The fact that the program cannot tell the difference *is the result*.
//!
//! # What it is used for
//! - `rayland-refapp/tests/native_render.rs` runs it on the host's own driver, establishing that
//!   the picture is right when nothing else is involved.
//! - `rayland-engine/tests/refapp_venus_e2e.rs` runs this same binary against Rayland's engine and
//!   checks it produces the same picture. That is C0's proof.
//!
//! # Usage
//! ```text
//! rayland-refapp <output.png>
//! ```
//!
//! # Pitfall for anyone changing this
//! Resist the temptation to make it "better" in ways that make it special. No command-line
//! rendering options, no environment probing, no conditional paths, and above all nothing that
//! knows about Rayland. Its value is precisely that it is boring and typical; a bespoke program
//! written to be remotable would prove nothing about real applications.

// The Vulkan bring-up, the drawing method, and the frame itself.
mod context;
mod pipeline;
mod render;

// The vertex type the geometry below is expressed in.
use pipeline::Vertex;

/// The rendered image's width in pixels.
///
/// 64×64 is small enough that the whole frame is trivial for any GPU and the readback is a few
/// kilobytes, and large enough that a triangle in it has an unambiguous interior and unambiguous
/// corners for a test to check. It matches the size Rayland's earlier sub-projects rendered at, so
/// the outputs are directly comparable.
const IMAGE_WIDTH: u32 = 64;

/// The rendered image's height in pixels. Square, so a transposed image would still be the right
/// shape — the corner checks in the tests, not the dimensions, are what would catch that.
const IMAGE_HEIGHT: u32 = 64;

/// The background: opaque blue, in RGBA order, each channel `0.0..=1.0`.
///
/// Blue against a red triangle is chosen for contrast in the most literal sense: the two differ in
/// *every* colour channel, so any channel-ordering mistake anywhere along the path — in the render
/// target's format, the readback, the PNG encoding, or a driver's swizzle — turns the picture into
/// something obviously wrong rather than something plausibly right. Two similar colours could swap
/// silently. These cannot.
const CLEAR_COLOR: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// The triangle: three vertices, all opaque red, in Vulkan's normalised device coordinates.
///
/// The coordinates run -1 to +1 across the image with **y pointing down** (Vulkan's convention,
/// the opposite of OpenGL's), so this is an upward-pointing triangle: apex at the top-centre, base
/// along the bottom. It spans roughly the middle half of the image, which is what makes "the
/// centre pixel is red, and all four corners are blue" a meaningful check — the triangle
/// comfortably covers the former and comes nowhere near the latter.
///
/// The geometry is hardcoded because this program is a fixture, not a renderer: there is nothing
/// for it to derive a scene from, and a fixed picture is what makes its output comparable across
/// runs and across drivers.
const VERTICES: [Vertex; 3] = [
    Vertex {
        // Apex, top-centre.
        position: [0.0, -0.5],
        color: [1.0, 0.0, 0.0],
    },
    Vertex {
        // Bottom-right.
        position: [0.5, 0.5],
        color: [1.0, 0.0, 0.0],
    },
    Vertex {
        // Bottom-left.
        position: [-0.5, 0.5],
        color: [1.0, 0.0, 0.0],
    },
];

/// Render the triangle and write it to the PNG path given as the single argument.
///
/// # Exit status
/// 0 on success. On any failure — no argument, no Vulkan device, a Vulkan error, or a PNG that
/// could not be written — the error is printed with its full cause chain and the process exits 1.
/// The cause chain matters: when this binary is run through a remoting path, the *specific*
/// Vulkan call that failed is the entire diagnostic, and a summarised message would throw it away.
fn main() {
    if let Err(error) = run() {
        eprintln!("rayland-refapp: {error}");
        // `anyhow`'s chain carries the underlying `vk::Result` (or `io::Error`) that actually
        // failed; printing only the top-level message would hide it.
        for cause in error.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        std::process::exit(1);
    }
}

/// The real body of [`main`], returning a `Result` so every step can use `?` and the error
/// reporting lives in exactly one place.
///
/// # Errors
/// Returns an error if the output path argument is missing, if Vulkan bring-up or rendering fails,
/// or if the PNG cannot be written.
fn run() -> anyhow::Result<()> {
    // Exactly one argument: where to write the PNG. Taking it as an argument rather than fixing a
    // filename is what lets two different tests run this binary concurrently without one
    // clobbering the other's output.
    let output = std::env::args_os().nth(1).ok_or_else(|| {
        anyhow::anyhow!("usage: rayland-refapp <output.png>\n  renders a 64x64 red triangle on blue and writes it as a PNG")
    })?;

    // Bring Vulkan up. Which driver answers is entirely the environment's decision — see the
    // module docs. This is the only place the outcome of that decision could be observed, and the
    // program deliberately does not look.
    let ctx = context::VulkanContext::new()?;

    // Draw the frame and get the pixels back. Everything Vulkan happens inside this call.
    let pixels = render::render_triangle(&ctx, IMAGE_WIDTH, IMAGE_HEIGHT, CLEAR_COLOR, &VERTICES)?;

    // Write the PNG. The bytes are already tightly-packed RGBA8 in the order the encoder wants,
    // because the render target's format was chosen to make that true (see `pipeline::COLOR_FORMAT`),
    // so there is no conversion step here to get wrong.
    image::save_buffer(
        &output,
        &pixels,
        IMAGE_WIDTH,
        IMAGE_HEIGHT,
        image::ColorType::Rgba8,
    )?;

    Ok(())
}
