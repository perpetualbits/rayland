//! **Fixture B: the volume control.** An ordinary offscreen Vulkan program that draws the exact
//! same spinning icosahedron as its sibling `rayland-icosa-cpu`, but evaluates the Mandelbrot
//! fractal **in a fragment shader** instead of computing it on the CPU and uploading it as a
//! texture.
//!
//! # This fixture does NOT avoid mapped memory
//! Do not read "no texture, no staging buffer" as "no mapped writes". This program still writes
//! its per-frame uniforms — the MVP matrix, and the fractal's view half-width and centre — through
//! the same kind of persistently-mapped `HOST_COHERENT` buffer fixture A uses
//! (`rayland_icosa_vk::Scene`'s own `uniform_buffer`, written inside [`rayland_icosa_vk::Scene::draw`]).
//! That write has no flush and is reachable through no Vulkan call any more than fixture A's
//! megabyte-per-frame texture write is — it is a bare memory store through a pointer obtained once
//! at startup, exactly the same mechanism, exactly as invisible to anything watching the Vulkan API.
//!
//! What differs between the two fixtures is not *whether* a mapped write happens every frame, but
//! *how much* crosses it: exactly 80 bytes here (the std140-padded uniform block: 64 bytes for the
//! MVP matrix, 4 for `half_width`, 4 bytes of std140 padding, and 8 for `center` — already a
//! multiple of 16, so there is no further tail padding) against roughly a megabyte for fixture A's
//! whole fractal texture. That volume — not the presence of the write — is the one thing this pair
//! of fixtures exists to isolate. A reader who concludes "the GPU one has no staging buffer, so it
//! has no mapped-memory problem" has drawn exactly the wrong lesson from this program.
//!
//! # Why this crate is so small
//! Every piece both fixtures must agree on for their comparison to mean anything — geometry, the
//! animation schedule, the fractal's arithmetic, the render pass, the pipeline, the persistent
//! mapping, the readback, even the render loop's shape — lives in `rayland-icosa-vk` and
//! `rayland-icosa-core`, which both fixtures depend on identically. What is left here is exactly
//! what makes this *fixture B* rather than its sibling: the fragment shader that evaluates the
//! fractal (`shaders/icosa_fractal.frag`, compiled and embedded below) and this frame loop, with no
//! texture path at all. If this crate ever grows a second module, either something that belongs in
//! `rayland-icosa-vk` has leaked into it, or the two fixtures have started to differ somewhere they
//! must not — see that crate's own module doc for the full argument.
//!
//! # Object lifetime and drop order in [`run`]
//! `context` (a [`VulkanContext`]) is created first and must outlive everything built from it,
//! including `scene` (a [`Scene`]), which is declared after it so Rust's reverse-declaration-order
//! drop rule runs `scene`'s `Drop` — which waits the whole device idle before destroying anything
//! it owns — before `context` is torn down. See [`Scene`]'s struct doc for why that ordering is a
//! caller obligation this type cannot enforce itself, and `rayland-icosa-cpu`'s `main.rs` for the
//! identical pattern applied around its own extra pieces (the staging buffer and texture, neither
//! of which exists here).
//!
//! # Usage
//! ```text
//! rayland-icosa-gpu <output-directory>
//! ```
//! Writes `frame_0000.png` … `frame_0119.png` into the given directory (which must already exist)
//! and prints a CSV timing report — one `frame,fractal_us,upload_us,draw_readback_us` line per
//! frame, with a header — to stdout. The columns are kept identical to fixture A's so the two
//! reports can be diffed directly. `upload_us` is always `0`: there is no upload, because there is
//! no texture to upload. `fractal_us` times this frame's (now trivial) preparation of the fractal's
//! view parameters — the same values fixture A spends milliseconds turning into a whole texture, and
//! here cost only a few floating-point multiplications, since the actual per-pixel fractal
//! evaluation happens on the GPU during the draw itself.

use std::time::Instant;

use rayland_icosa_core::FRAME_COUNT;
use rayland_icosa_core::schedule::{CENTER, frame_mvp, frame_zoom};
use rayland_icosa_vk::{Scene, Uniforms, VulkanContext, write_png};

/// The fractal fragment shader: evaluates the Mandelbrot set per fragment instead of sampling a
/// texture. See `shaders/icosa_fractal.frag`'s own header for the full transcription of
/// `rayland_icosa_core`'s `exact_math::log2` and HSV ramp this shader mirrors, and for why that
/// transcription (rather than GLSL's built-in `log2`) is done anyway even though this shader's own
/// reproducibility does not strictly require it.
///
/// Not embedded in `rayland-icosa-vk`'s pipeline module — see that crate's `pipeline.rs` module doc
/// for why the fragment stage is deliberately a parameter rather than a constant: it is the two
/// fixtures' independent variable, and this crate supplies the one that evaluates the fractal
/// itself rather than sampling a texture computed elsewhere.
const FRACTAL_FRAGMENT_SPIRV: &[u8] = include_bytes!("../../../shaders/icosa_fractal.frag.spv");

/// Print the top-level error and its full cause chain, then exit 1.
///
/// The cause chain matters here exactly as it does for `rayland-icosa-cpu` (see that crate's
/// `main.rs` doc): when this binary is run through a remoting path, the *specific* Vulkan call that
/// failed is the entire diagnostic, and a summarised message would throw it away.
fn main() {
    if let Err(error) = run() {
        eprintln!("rayland-icosa-gpu: {error}");
        for cause in error.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        std::process::exit(1);
    }
}

/// Reinterpret a byte slice holding compiled SPIR-V as a `Vec` of native-endian 32-bit words.
///
/// SPIR-V is specified as a stream of 32-bit words; `include_bytes!` hands them back as a `&[u8]`
/// with no alignment guarantee, and Vulkan's `vkCreateShaderModule` requires a `u32`-aligned,
/// `u32`-sized buffer (`ash`'s `code()` setter takes a `&[u32]`). This copies four bytes at a time
/// into freshly allocated, correctly aligned `u32`s rather than casting the byte pointer in place,
/// which would be undefined behaviour on the unlucky day the embedded literal is not 4-byte
/// aligned. `from_ne_bytes` (native-endian) matches what every target this repository builds for
/// actually is (little-endian), and is the same assumption `ash::util::read_spv` makes internally.
///
/// Dropping the `ash` dependency (see this crate's `Cargo.toml` doc for why) meant losing the two
/// checks `ash::util::read_spv` performs for free on its way in: that the byte length is a whole
/// number of words, and that the first word is SPIR-V's magic number. Both are reinstated below as
/// explicit assertions, because without them this function does something worse than panic on bad
/// input — see "Failure modes".
///
/// # Inputs and outputs
/// `bytes` must be a whole number of 4-byte words — true for any file `glslangValidator -V`
/// produces, since SPIR-V's own container format is defined in 32-bit words throughout. Returns
/// those words as `u32`s.
///
/// # Failure modes
/// Asserts that `bytes.len()` is a multiple of 4, and that the first resulting word is SPIR-V's
/// magic number `0x0723_0203`. Both assertions exist because of what silently happens without
/// them: `chunks_exact(4)` never yields a short trailing slice — it *drops* a trailing partial
/// chunk instead — so an unchecked mis-sized blob would be silently **truncated**, not rejected,
/// and the `try_into().unwrap()` on each chunk could then never fail (every chunk `chunks_exact`
/// yields is already exactly 4 bytes), so it documents no failure mode of its own — the length
/// check belongs before the chunking, not folded into it. Likewise, with no magic-number check, a
/// corrupt or byte-swapped `.spv` would sail through as `u32`s and only surface as an opaque
/// driver-level failure inside `vkCreateShaderModule`. Both inputs reaching this function are
/// `include_bytes!` of files committed to this repository, so in practice both assertions are
/// defence against the embedded file being replaced with something that is not valid SPIR-V — a
/// build-time packaging error — not a scenario expected to occur at runtime.
fn spirv_words(bytes: &[u8]) -> Vec<u32> {
    // Reject a mis-sized blob outright: chunks_exact(4) would otherwise silently truncate it to
    // the largest whole number of 4-byte words instead of ever reporting the mismatch.
    assert!(
        bytes.len() % 4 == 0,
        "SPIR-V blob is {} bytes, not a whole number of 4-byte words",
        bytes.len()
    );
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|word| u32::from_ne_bytes(word.try_into().unwrap()))
        .collect();
    // SPIR-V's magic number is always the stream's first word; ash::util::read_spv checks the same
    // thing internally. Catches a corrupt or byte-swapped blob here, with a clear message, instead
    // of letting it reach vkCreateShaderModule as garbage.
    assert_eq!(
        words.first(),
        Some(&0x0723_0203),
        "SPIR-V magic number missing or wrong; expected the stream to start with 0x0723_0203"
    );
    words
}

/// The real body of [`main`]: bring up Vulkan, build the scene, then render and write every one of
/// [`FRAME_COUNT`]'s frames, printing one CSV timing line per frame.
///
/// # Errors
/// Returns an error if the output-directory argument is missing, if Vulkan bring-up or scene
/// creation fails, or if any frame's draw or PNG write fails.
fn run() -> anyhow::Result<()> {
    // Exactly one argument: the directory frames and (implicitly, via stdout) the timing report
    // are written to. An argument rather than a fixed path, so two runs never clobber each other's
    // output — the same reasoning `rayland-icosa-cpu`'s single output-directory argument rests on.
    let output_dir = std::env::args_os().nth(1).ok_or_else(|| {
        anyhow::anyhow!(
            "usage: rayland-icosa-gpu <output-directory>\n  \
             renders the GPU fixture's 120 frames as frame_0000.png..frame_0119.png"
        )
    })?;
    let output_dir = std::path::PathBuf::from(output_dir);

    // Bring Vulkan up. Which driver answers is entirely the environment's decision — this crate
    // does no probing of its own and reaches Vulkan only through `rayland-icosa-vk` (see that
    // crate's module doc, "This crate never mentions remoting", for what it does and does not
    // probe) — this is the only place that decision's outcome could be observed, and this program
    // deliberately never looks.
    let context = VulkanContext::new()?;

    // The compiled fragment shader, parsed into properly aligned 32-bit words. Fixture A's
    // equivalent step uses `ash::util::read_spv` — but fixture A depends on `ash` anyway (its
    // `texture.rs` records its own barriers and copies), and this crate deliberately does not:
    // see this crate's `Cargo.toml` doc for why its dependency list stops at `rayland-icosa-vk`,
    // `rayland-icosa-core`, and `anyhow`. `spirv_words` below is the same conversion — SPIR-V's
    // byte stream, reinterpreted as native-endian 32-bit words — done by hand instead, so this
    // crate never needs `ash` in scope at all.
    let frag_spirv = spirv_words(FRACTAL_FRAGMENT_SPIRV);

    // `Scene` is declared after `context` so Rust's reverse-declaration-order drop rule runs
    // `Scene::drop` (which waits the device idle) before `context` is torn down — see this module's
    // doc comment ("Object lifetime and drop order"). Unlike fixture A there is no nested block and
    // no explicit-destroy pair here: this fixture owns no `MappedBuffer` or texture of its own, so
    // there is nothing beyond `scene` and `context` whose teardown order needs arguing about.
    //
    // `None` for the sampler binding: there is genuinely no texture. Fixture A passes
    // `Some(SamplerBinding{..})`; this is the one call-site difference between the two fixtures'
    // scene construction, and it is exactly the difference this fixture exists to be.
    let mut scene = Scene::new(&context, &frag_spirv, None)?;

    // The timing report's header. Ordinary profiling output: it measures this program's own work
    // with this program's own clock. The clock only ever *measures* and never *decides* — nothing
    // about what is drawn depends on a timing value. If that ever changed, every frame would stop
    // being reproducible and this program would stop being useful as a fixture.
    println!("frame,fractal_us,upload_us,draw_readback_us");

    for frame in 0..FRAME_COUNT {
        // 1. The fractal's view parameters for this frame: the MVP matrix and the zoomed
        //    half-width. No Vulkan call is involved in this step — it is the same schedule
        //    arithmetic fixture A performs to fill in the same two `Uniforms` fields, just without
        //    the megabyte of per-texel Mandelbrot iteration fixture A also does here, because that
        //    iteration happens per-fragment on the GPU during `scene.draw` below instead.
        let fractal_start = Instant::now();
        let uniforms = Uniforms {
            mvp: frame_mvp(frame),
            half_width: frame_zoom(frame) as f32,
            center: [CENTER.0 as f32, CENTER.1 as f32],
        };
        let fractal_us = fractal_start.elapsed().as_micros();

        // 2. The upload: there is none. Fixture A's step 2 copies a staging buffer into a texture
        //    image; this fixture has no texture, so this column is always zero — kept only so the
        //    two CSVs diff column-for-column against each other.
        let upload_us: u128 = 0;

        // 3. Draw and read back. `uniforms` (built above) goes into mapped uniform memory inside
        //    `draw` itself (`Scene` owns that buffer, not this frame loop) — the one mapped write
        //    this fixture makes every frame, exactly 80 bytes against fixture A's roughly one
        //    megabyte. See this module's doc comment for why that volume difference, not the
        //    write's presence, is the whole point.
        let draw_start = Instant::now();
        let pixels = scene.draw(&context, &uniforms)?;
        let draw_readback_us = draw_start.elapsed().as_micros();

        // 4. The artefact. Written by the application itself, from pixels it read back — not by
        //    anything else on its behalf.
        write_png(&output_dir.join(format!("frame_{frame:04}.png")), &pixels)?;

        println!("{frame},{fractal_us},{upload_us},{draw_readback_us}");
    }
    // `scene` drops here, at the end of `run` — before `context`, by Rust's reverse-declaration-
    // order drop rule (see this module's doc comment) — running `Scene::drop`'s unconditional
    // device-idle wait before `context`'s own `Drop` tears down the instance and device underneath
    // it.

    Ok(())
}
