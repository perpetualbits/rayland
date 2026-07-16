//! **Fixture A: the mapped fractal, all 120 frames.** An ordinary offscreen Vulkan program that
//! draws a spinning icosahedron textured with a zooming Mandelbrot fractal it computes on **its
//! own CPU**, every frame, and writes through a persistently-mapped `HOST_COHERENT` staging buffer
//! — with no flush, and therefore no Vulkan call anywhere between the write and the copy that reads
//! it.
//!
//! # This is the task the whole icosahedron sub-project exists for
//! Everything before this crate — `rayland-icosa-core`'s geometry/schedule/fractal math,
//! `rayland-icosa-vk`'s shared Vulkan scaffolding — was scaffolding built so that this program
//! could be small. What is left here is exactly what makes this *the CPU fixture* rather than its
//! sibling (Task 7's GPU fixture, which evaluates the same fractal in a fragment shader instead):
//! the texture upload path (`texture.rs`) and this frame loop.
//!
//! # Why an ordinary program is the point
//! This program looks like any other Vulkan application that keeps a texture fresh every frame: it
//! maps a buffer once at startup, writes into it, and issues a copy. Nothing here is written to be
//! easy to intercept, and nothing here is written to be hard to intercept either — it is written
//! the way an application with no idea it might be remoted would be written, because that is
//! exactly what it is. `rayland-icosa-vk`'s own module docs make the same commitment for the
//! scaffolding this program is built on: no `rayland-*` dependency beyond the two icosa crates, no
//! mention of Venus, vtest, virglrenderer, sockets, or remoting, and no environment probing beyond
//! what `ash::Entry::load()` itself does to find a driver. This program cannot tell whether its
//! rendering is happening locally or is being remoted, and that ignorance is the whole point: an
//! application that adapted itself to being remoted would prove nothing about the ordinary
//! applications it stands in for.
//!
//! # Object lifetime and drop order in [`run`]
//! `context` (a [`VulkanContext`]) is created first and must outlive everything built from it.
//! `staging` (the fractal's staging [`MappedBuffer`]) and `texture` (a [`FractalTexture`]) are
//! created next; `scene` (a [`Scene`]) is created last, from `texture`'s sampler binding, and is
//! deliberately built inside its own nested block. `Scene` implements [`Drop`] and its `drop`
//! unconditionally waits for the whole device to go idle before destroying anything it owns (see
//! that type's struct doc) — ending `scene`'s block before this function calls
//! [`FractalTexture::destroy`] and [`MappedBuffer::destroy`] guarantees that wait has already run,
//! so both of those explicit-destroy calls are sound: by the time they run, every submission either
//! of them could possibly be referenced by (every `upload`, every `draw`) has been fence-waited to
//! completion at least once over. `MappedBuffer` and `FractalTexture` are not `Drop` types
//! themselves — see `rayland_icosa_vk::scene`'s module doc for why `rayland-refapp`'s
//! explicit-destroy-on-the-success-path-only pattern is used here rather than a second `Drop` impl:
//! an object destroyed while the GPU may still be reading or writing it is undefined behaviour, and
//! an error path (a `?` propagating out of this function) is exactly the case where that guarantee
//! cannot be made, so this function simply does not destroy anything on that path and lets process
//! exit reclaim the driver-side resources instead.
//!
//! # Usage
//! ```text
//! rayland-icosa-cpu <output-directory>
//! ```
//! Writes `frame_0000.png` … `frame_0119.png` into the given directory (which must already exist)
//! and prints a CSV timing report — one `frame,fractal_us,upload_us,draw_readback_us` line per
//! frame, with a header — to stdout.

// The texture image, its sampler, and the upload path that fills it from the staging buffer this
// module's frame loop writes the fractal into.
mod texture;

use std::time::Instant;

use rayland_icosa_core::schedule::{CENTER, frame_mvp, frame_zoom};
use rayland_icosa_core::{FRAME_COUNT, TEXTURE_SIZE, fractal};
use rayland_icosa_vk::{MappedBuffer, Scene, Uniforms, VulkanContext, write_png};

use ash::vk;
use texture::FractalTexture;

/// The textured fragment shader: samples `texture.rs`'s `FractalTexture` and shades it by the same
/// fixed light every fixture built on `rayland-icosa-vk` uses.
///
/// Not embedded in `rayland-icosa-vk`'s pipeline module — see that crate's `pipeline.rs` module doc
/// for why the fragment stage is deliberately a parameter rather than a constant: it is the two
/// fixtures' independent variable, and this crate supplies the one that reads a texture rather than
/// evaluating the fractal itself.
const TEXTURED_FRAGMENT_SPIRV: &[u8] = include_bytes!("../../../shaders/icosa_textured.frag.spv");

/// Print the top-level error and its full cause chain, then exit 1.
///
/// The cause chain matters here exactly as it does for `rayland-refapp` (see that crate's `main.rs`
/// doc): when this binary is run through a remoting path, the *specific* Vulkan call that failed is
/// the entire diagnostic, and a summarised message would throw it away.
fn main() {
    if let Err(error) = run() {
        eprintln!("rayland-icosa-cpu: {error}");
        for cause in error.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        std::process::exit(1);
    }
}

/// The real body of [`main`]: bring up Vulkan, build the texture and the scene, then render and
/// write every one of [`FRAME_COUNT`]'s frames, printing one CSV timing line per frame.
///
/// # Errors
/// Returns an error if the output-directory argument is missing, if Vulkan bring-up, texture
/// creation, or scene creation fails, or if any frame's fractal upload, draw, or PNG write fails.
fn run() -> anyhow::Result<()> {
    // Exactly one argument: the directory frames and (implicitly, via stdout) the timing report
    // are written to. An argument rather than a fixed path, so two runs never clobber each other's
    // output — the same reasoning `rayland-refapp`'s single PNG-path argument rests on.
    let output_dir = std::env::args_os().nth(1).ok_or_else(|| {
        anyhow::anyhow!(
            "usage: rayland-icosa-cpu <output-directory>\n  \
             renders the CPU fixture's 120 frames as frame_0000.png..frame_0119.png"
        )
    })?;
    let output_dir = std::path::PathBuf::from(output_dir);

    // Bring Vulkan up. Which driver answers is entirely the environment's decision (see this
    // program's module doc) — this is the only place that decision's outcome could be observed,
    // and this program deliberately never looks.
    let context = VulkanContext::new()?;

    // The staging buffer the fractal is written into every frame: HOST_VISIBLE and HOST_COHERENT,
    // mapped once here and held for the whole run. See `texture.rs`'s module doc for the race this
    // buffer is exposed to and how `FractalTexture::upload`'s own fence wait closes it.
    let staging_size = u64::from(TEXTURE_SIZE) * u64::from(TEXTURE_SIZE) * 4;
    let mut staging =
        MappedBuffer::new(&context, staging_size, vk::BufferUsageFlags::TRANSFER_SRC)?;

    // The texture image the solid's faces sample, and the small amount of Vulkan machinery
    // `FractalTexture::upload` submits its copy through.
    let texture = FractalTexture::new(&context)?;

    // The compiled fragment shader, parsed into properly aligned 32-bit words — `include_bytes!`
    // gives no alignment guarantee on the raw bytes, and `read_spv` is what Vulkan requires instead
    // of a cast in place (see `rayland_icosa_vk::pipeline`'s identical use of this same function).
    let mut frag_cursor = std::io::Cursor::new(TEXTURED_FRAGMENT_SPIRV);
    let frag_spirv = ash::util::read_spv(&mut frag_cursor)?;

    // The scene is built inside its own block, deliberately: see this module's doc comment
    // ("Object lifetime and drop order") for why ending this block — which runs `Scene::drop`,
    // waiting the whole device idle — before `texture` and `staging` are torn down below is what
    // makes those two explicit `destroy` calls sound.
    {
        let mut scene = Scene::new(&context, &frag_spirv, Some(texture.sampler_binding()))?;

        // The timing report's header. Ordinary profiling output: it measures this program's own
        // work with this program's own clock. The clock only ever *measures* and never *decides* —
        // nothing about what is drawn depends on a timing value. If that ever changed, every frame
        // would stop being reproducible and this program would stop being useful as a fixture.
        println!("frame,fractal_us,upload_us,draw_readback_us");

        for frame in 0..FRAME_COUNT {
            // 1. The fractal, straight into mapped host-visible memory. No Vulkan call is involved
            //    in this step at all — it is a plain memory write through a pointer obtained once
            //    at startup ([`MappedBuffer::new`]'s one and only `vkMapMemory`).
            //
            //    This write is safe from the staging-buffer race because the *previous* iteration's
            //    `texture.upload` call (or, on the very first iteration, nothing at all) already
            //    fence-waited its copy to completion before returning — see `texture.rs`'s module
            //    doc for the full argument.
            let fractal_start = Instant::now();
            fractal::render_into(staging.bytes(), frame_zoom(frame));
            let fractal_us = fractal_start.elapsed().as_micros();

            // 2. The upload: the one Vulkan call that touches the megabyte step 1 just wrote. It
            //    says nothing about which bytes changed — only that the buffer's current contents
            //    should be copied into the texture image.
            let upload_start = Instant::now();
            texture.upload(&context, &staging)?;
            let upload_us = upload_start.elapsed().as_micros();

            // 3. Draw and read back. The matrix goes into mapped uniform memory inside `draw`
            //    itself (`Scene` owns that buffer, not this frame loop).
            let draw_start = Instant::now();
            let pixels = scene.draw(
                &context,
                &Uniforms {
                    mvp: frame_mvp(frame),
                    // Present but unread by this fixture's fragment shader: the uniform block's
                    // layout is shared with the GPU fixture, whose shader does read these two
                    // fields. Keeping one layout is what lets `rayland-icosa-vk` serve both fixtures
                    // without a conditional — see `Uniforms`'s own doc.
                    half_width: frame_zoom(frame) as f32,
                    center: [CENTER.0 as f32, CENTER.1 as f32],
                },
            )?;
            let draw_readback_us = draw_start.elapsed().as_micros();

            // 4. The artefact. Written by the application itself, from pixels it read back — not by
            //    anything else on its behalf.
            write_png(&output_dir.join(format!("frame_{frame:04}.png")), &pixels)?;

            println!("{frame},{fractal_us},{upload_us},{draw_readback_us}");
        }
        // `scene` drops here, at the end of this block — its `Drop` impl waits the device idle
        // before destroying anything it owns (see `Scene`'s struct doc), which is the wait the
        // `destroy` calls below depend on.
    }

    // SAFETY: `scene`'s `Drop` (immediately above) already waited the device idle, and `scene` was
    // the last thing that could have referenced either `texture` or `staging` (every `draw` reads
    // `texture` via its descriptor set; every `upload` reads `staging` directly) — so the GPU is
    // provably done with both by this point, on the only path that reaches this line (`?` above
    // would already have returned out of this function on any earlier failure).
    unsafe {
        texture.destroy(&context.device);
        staging.destroy(&context.device);
    }

    Ok(())
}
