//! [`Scene`]: the one thing a fixture actually drives — the vertex buffer, the uniforms, the
//! descriptor set, the command buffer, and the fence, bundled behind [`Scene::draw`].
//!
//! # What is deliberately not here
//! No frame loop, no CSV, no texture upload path. Those are exactly the things a fixture must do
//! for *itself*: the frame loop is what a fixture's `main` calls `Scene::draw` from, the CSV is
//! where a fixture records its own timings, and the texture path (a `MappedBuffer` staging a
//! fractal into a sampled image) is the CPU fixture's own code, built from the pieces this crate
//! exports ([`crate::MappedBuffer`], [`crate::SamplerBinding`]) rather than baked in here. Baking
//! any of those in would put the experiment's independent variable — where the fractal is computed
//! — inside a library both fixtures share, which is exactly the place nobody would think to look
//! for it going out of sync.

use ash::vk;
use rayland_icosa_core::IMAGE_SIZE;
use rayland_icosa_core::geometry::icosahedron;
use std::path::Path;

use crate::context::VulkanContext;
use crate::mapped::MappedBuffer;
use crate::pipeline::{IcosaPipeline, SamplerBinding};
use crate::targets::Targets;

/// Bytes per pixel in [`crate::pipeline::COLOR_FORMAT`]: four 8-bit channels.
const BYTES_PER_PIXEL: u32 = 4;

/// How long to wait for the GPU to finish a frame before giving up, in nanoseconds (10 seconds).
///
/// A timeout, rather than `u64::MAX`, so a wedged GPU fails as a diagnosable error instead of an
/// unkillable process. Ten seconds is enormously more than a 256×256, 20-triangle solid needs;
/// anything that exceeds it is broken, not slow. Matches `rayland-refapp`'s own constant.
const FENCE_TIMEOUT_NS: u64 = 10_000_000_000;

/// The per-frame uniform block, as a fixture constructs it: the MVP matrix, and the fractal view's
/// half-width and centre.
///
/// `mvp` is column-major, matching [`rayland_icosa_core::schedule::frame_mvp`]'s documented layout
/// and GLSL's default `mat4` storage — no transpose happens anywhere between that function and the
/// shader. `half_width` and `center` are read by neither `shaders/icosa.vert` nor
/// `shaders/icosa_flat.frag`/`shaders/icosa_textured.frag` as given to this crate — they exist so
/// that a future fixture's fractal fragment shader can read the same uniform block this one struct
/// describes, without the block's shape differing between the CPU and GPU fixtures.
///
/// # A landmine this struct's natural layout does *not* avoid, and how `Scene::draw` avoids it
/// `#[repr(C)]` lays out `mvp` at bytes 0–63, `half_width` at 64–67, and `center` at 68–75 — Rust
/// inserts no padding there, because `[f32; 2]`'s alignment is 4, and byte 68 is already a multiple
/// of 4. GLSL's default uniform-block layout (std140) disagrees: a `vec2` has an 8-byte *base
/// alignment*, so std140 pads `center` out to byte 72, not 68. This is verified, not assumed —
/// compiling `shaders/icosa.vert` and inspecting it with `spirv-dis` shows
/// `OpMemberDecorate %Uniforms 1 Offset 64` (`half_width`) and
/// `OpMemberDecorate %Uniforms 2 Offset 72` (`center`), a 4-byte gap this struct's own bytes do not
/// contain. **[`Scene::draw`] therefore never `memcpy`s a `Uniforms` value directly into the
/// uniform buffer.** It builds a private, correctly-padded [`GpuUniforms`] from the fields here and
/// copies *that* instead — see that type's doc. This struct is kept exactly as specified regardless
/// (three fields, no padding field of its own), because that is the shape every caller — including
/// this crate's own tests — constructs it as; the fix belongs on the writing side, not here.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Uniforms {
    /// The model-view-projection matrix for this frame, column-major.
    pub mvp: [[f32; 4]; 4],
    /// The fractal view's complex-plane half-width for this frame.
    pub half_width: f32,
    /// The fractal view's centre, in the complex plane, as `[re, im]` cast to `f32`.
    pub center: [f32; 2],
}

/// The uniform block laid out exactly as GLSL's default (std140) block layout puts it, used only to
/// serialize a public [`Uniforms`] value into the uniform buffer with the correct byte offsets.
///
/// See [`Uniforms`]'s doc for why this type exists rather than copying a `Uniforms` value's own
/// bytes directly: `_pad` reproduces the 4-byte gap std140 inserts before a `vec2` that immediately
/// follows a `float`, which `Uniforms`'s own natural `#[repr(C)]` layout does not contain.
/// [`gpu_uniforms_matches_std140_layout`] pins these exact offsets against a regression.
#[repr(C)]
struct GpuUniforms {
    mvp: [[f32; 4]; 4],
    half_width: f32,
    /// Exists purely to occupy std140's gap between `half_width` (ends at byte 68) and `center`
    /// (starts at byte 72, an 8-byte-aligned offset because `vec2` has 8-byte base alignment in
    /// std140). Never read; its value is irrelevant and left zeroed.
    _pad: f32,
    center: [f32; 2],
}

/// The vertex buffer, the uniform buffer, the descriptor set, the render targets, the command
/// buffer, and the fence a fixture draws frames through.
///
/// Created once per fixture run via [`Scene::new`] and driven by repeated calls to
/// [`Scene::draw`] — one per frame. Every field here is sized and allocated exactly once; nothing
/// in [`Scene::draw`] creates or destroys a Vulkan object, which is what makes 120 draws cheap
/// relative to a design that rebuilt the render targets or reallocated the command buffer per
/// frame.
///
/// # Destruction order, and why `Scene` implements `Drop` unlike `rayland-refapp`'s pieces
/// `rayland-refapp`'s `TrianglePipeline`/`ColorTarget`/`HostBuffer` are deliberately *not*
/// `Drop`-implementing types: their owning code (`render_triangle_inner`) calls their `destroy`
/// methods explicitly, exactly once, only on the success path — because on the error path a fence
/// timeout can mean the GPU is still executing, and destroying a Vulkan object out from under
/// in-flight work is undefined behaviour (see that crate's `render.rs` module docs at length).
///
/// `Scene` cannot use that pattern as-is, because this crate's documented public interface gives a
/// caller no explicit teardown method to call — `Scene::new` and `Scene::draw` are the whole public
/// surface, and this crate's own tests (and, later, the fixtures) simply let a `Scene` go out of
/// scope. So `Scene` implements `Drop`.
///
/// # Why `Drop` cannot rely on "every `draw` waits its fence" alone
/// On the ordinary path this is true and would be enough on its own: every [`Scene::draw`] call
/// already waits on its fence before returning (see that method's doc), so by the time control
/// reaches `Drop` after a normal return, no work from this `Scene` can still be in flight. But that
/// argument only covers `draw` calls that return `Ok`. [`Scene::draw`]'s fence wait can time out
/// (`FENCE_TIMEOUT_NS` exists precisely because a wedged GPU is a reachable failure, not a
/// hypothetical one) — and when it does, `?` propagates the error out of `draw` with the
/// submission possibly still executing. A caller that turns that error into a panic (an `.expect()`
/// at a call site, say) unwinds straight into this `Drop` impl while GPU work may still be reading
/// or writing the very images, buffers and command pool `Drop` is about to destroy — the same
/// use-after-free hazard `rayland-refapp`'s `render.rs` module docs describe at length, just
/// reached through a panic instead of an explicit destroy call on the error path.
///
/// Rather than accept that as a residual, documented hazard, `Drop` closes it directly: it calls
/// `device_wait_idle` unconditionally, before destroying anything (see the impl below), so
/// destruction is sound whether the last `draw` returned `Ok` or unwound through a timeout.
///
/// A second hazard `rayland-refapp`'s `TrianglePipeline` avoids by *not* storing a device handle
/// ("the ordinary Vulkan-in-Rust trade-off... the alternative is storing a device clone in every
/// object") is unavoidable here for the same reason: `Drop::drop` takes no arguments, so a type
/// that destroys its own Vulkan objects in `Drop` must hold the means to do so itself. `Scene`
/// therefore keeps a cloned `ash::Device` (cheap: it is a table of function pointers plus a raw
/// handle, not a deep resource). **This makes drop order a caller obligation this type cannot
/// enforce**: a `Scene` must be dropped before the [`VulkanContext`] it was built from. In every
/// call site in this crate — this crate's own tests, and any fixture with the ordinary shape
/// `let context = VulkanContext::new()?; let mut scene = Scene::new(&context, ...)?;` — Rust's
/// reverse-declaration-order drop rule gets this right automatically, because `scene` is declared
/// after `context` and so drops first. It would *not* get this right if a caller stored both in a
/// struct with `context` declared after `scene`, or otherwise let a `Scene` outlive its
/// `VulkanContext`; doing so is the same use-after-destroy hazard [`VulkanContext`]'s own module
/// docs describe for `instance`/`device`, one level up.
pub struct Scene {
    /// A clone of the device this `Scene` was built from — see the struct doc for why `Drop` needs
    /// it and the caller obligation that comes with keeping it.
    device: ash::Device,
    pipeline: IcosaPipeline,
    targets: Targets,
    /// Written once, in [`Scene::new`]: the solid's 60 vertices, uploaded and never touched again.
    vertex_buffer: MappedBuffer,
    /// Rewritten every [`Scene::draw`] call.
    uniform_buffer: MappedBuffer,
    /// Read every [`Scene::draw`] call, after the fence proves the GPU has finished writing it.
    readback_buffer: MappedBuffer,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set: vk::DescriptorSet,
    command_pool: vk::CommandPool,
    /// The one command buffer every frame is recorded into, re-recorded from scratch each
    /// [`Scene::draw`] call. `command_pool` is created with `RESET_COMMAND_BUFFER`, which is what
    /// makes calling `begin_command_buffer` on an already-recorded buffer legal — see
    /// [`Scene::new`]'s doc for why that flag is load-bearing here.
    command_buffer: vk::CommandBuffer,
    /// Signalled by the GPU when a frame's submission completes; waited on, then reset, by every
    /// [`Scene::draw`] call before the next one may submit.
    fence: vk::Fence,
}

impl Scene {
    /// Build every Vulkan object a fixture needs to draw frames: the pipeline, the render targets,
    /// the vertex buffer (uploaded here, once), the uniform and readback buffers, the descriptor
    /// set, and the reusable command buffer and fence.
    ///
    /// `fragment_spirv` is the fragment shader's compiled SPIR-V, already parsed into 32-bit words
    /// (e.g. via `ash::util::read_spv`) — see [`crate::pipeline::IcosaPipeline::new`]'s doc for why
    /// this crate does not embed it itself. `sampler` is `Some` exactly when `fragment_spirv`
    /// declares a `sampler2D` at binding 1; passing a mismatched combination will fail pipeline
    /// creation or leave a descriptor set write targeting a binding the layout does not declare.
    ///
    /// # Why the command pool needs `RESET_COMMAND_BUFFER`
    /// A command buffer allocated from a pool without this flag can only be re-recorded by
    /// resetting the *entire pool*; calling `vkBeginCommandBuffer` again on an already-recorded
    /// buffer from such a pool is invalid. Since [`Scene`] re-records the same single command
    /// buffer every [`Scene::draw`] call rather than allocating a fresh one per frame, the pool
    /// created here carries this flag specifically so that repeated `begin_command_buffer` calls on
    /// the one buffer are legal.
    ///
    /// # Errors
    /// Returns an error if any Vulkan creation, allocation, or upload call fails.
    pub fn new(
        context: &VulkanContext,
        fragment_spirv: &[u32],
        sampler: Option<SamplerBinding>,
    ) -> anyhow::Result<Scene> {
        // SAFETY: every `ash`/crate-internal `unsafe` call below is FFI into the Vulkan driver or a
        // raw-pointer operation whose preconditions are argued at each call site. Every
        // partially-built piece is torn down on its own error path so a failure part-way through
        // leaks nothing.
        unsafe { Scene::new_inner(context, fragment_spirv, sampler) }
    }

    /// The `unsafe` body of [`Scene::new`].
    unsafe fn new_inner(
        context: &VulkanContext,
        fragment_spirv: &[u32],
        sampler: Option<SamplerBinding>,
    ) -> anyhow::Result<Scene> {
        let has_sampler = sampler.is_some();
        let pipeline = unsafe { IcosaPipeline::new(&context.device, fragment_spirv, has_sampler) }?;

        let targets = match unsafe { Targets::new(context, &pipeline, IMAGE_SIZE, IMAGE_SIZE) } {
            Ok(targets) => targets,
            Err(error) => {
                unsafe { pipeline.destroy(&context.device) };
                return Err(error);
            }
        };

        // The solid's geometry, uploaded exactly once: `icosahedron()` is a pure function of
        // compile-time constants, so there is nothing to recompute per frame.
        let vertices = icosahedron();
        let vertex_bytes = std::mem::size_of_val(&vertices) as u64;
        let mut vertex_buffer =
            match MappedBuffer::new(context, vertex_bytes, vk::BufferUsageFlags::VERTEX_BUFFER) {
                Ok(buffer) => buffer,
                Err(error) => {
                    unsafe {
                        targets.destroy(&context.device);
                        pipeline.destroy(&context.device);
                    }
                    return Err(error);
                }
            };
        // SAFETY: `vertices` is `#[repr(C)]` `rayland_icosa_core::geometry::Vertex`s containing
        // only `f32`s (see that type's own doc for why `#[repr(C)]` is load-bearing there), so its
        // bytes mean the same thing to the GPU as they do here. `bytes()` returns exactly
        // `vertex_bytes` bytes, matching `vertices`' own size, and the two cannot overlap (one is a
        // fresh GPU mapping, the other a local array).
        unsafe {
            std::ptr::copy_nonoverlapping(
                vertices.as_ptr() as *const u8,
                vertex_buffer.bytes().as_mut_ptr(),
                vertex_bytes as usize,
            );
        }

        let uniform_buffer = match MappedBuffer::new(
            context,
            std::mem::size_of::<GpuUniforms>() as u64,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
        ) {
            Ok(buffer) => buffer,
            Err(error) => {
                unsafe {
                    vertex_buffer.destroy(&context.device);
                    targets.destroy(&context.device);
                    pipeline.destroy(&context.device);
                }
                return Err(error);
            }
        };

        let readback_size =
            u64::from(IMAGE_SIZE) * u64::from(IMAGE_SIZE) * u64::from(BYTES_PER_PIXEL);
        let readback_buffer =
            match MappedBuffer::new(context, readback_size, vk::BufferUsageFlags::TRANSFER_DST) {
                Ok(buffer) => buffer,
                Err(error) => {
                    unsafe {
                        uniform_buffer.destroy(&context.device);
                        vertex_buffer.destroy(&context.device);
                        targets.destroy(&context.device);
                        pipeline.destroy(&context.device);
                    }
                    return Err(error);
                }
            };

        let (descriptor_pool, descriptor_set) =
            match unsafe { create_descriptor_set(context, &pipeline, &uniform_buffer, sampler) } {
                Ok(pair) => pair,
                Err(error) => {
                    unsafe {
                        readback_buffer.destroy(&context.device);
                        uniform_buffer.destroy(&context.device);
                        vertex_buffer.destroy(&context.device);
                        targets.destroy(&context.device);
                        pipeline.destroy(&context.device);
                    }
                    return Err(error);
                }
            };

        // RESET_COMMAND_BUFFER — see this function's doc for why it is required, not optional,
        // given that `Scene::draw` re-records the same buffer every frame.
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(context.queue_family_index)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = match unsafe { context.device.create_command_pool(&pool_info, None) } {
            Ok(pool) => pool,
            Err(error) => {
                unsafe {
                    context
                        .device
                        .destroy_descriptor_pool(descriptor_pool, None);
                    readback_buffer.destroy(&context.device);
                    uniform_buffer.destroy(&context.device);
                    vertex_buffer.destroy(&context.device);
                    targets.destroy(&context.device);
                    pipeline.destroy(&context.device);
                }
                return Err(error.into());
            }
        };

        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let command_buffer = match unsafe { context.device.allocate_command_buffers(&alloc_info) } {
            // Exactly one buffer was requested, so exactly one comes back.
            Ok(buffers) => buffers[0],
            Err(error) => {
                unsafe {
                    context.device.destroy_command_pool(command_pool, None);
                    context
                        .device
                        .destroy_descriptor_pool(descriptor_pool, None);
                    readback_buffer.destroy(&context.device);
                    uniform_buffer.destroy(&context.device);
                    vertex_buffer.destroy(&context.device);
                    targets.destroy(&context.device);
                    pipeline.destroy(&context.device);
                }
                return Err(error.into());
            }
        };

        // Created unsignalled: the first `draw` call resets it anyway (see `draw`'s doc), but
        // starting unsignalled rather than relying on that reset avoids the fence ever being in a
        // surprising state before the first submission.
        let fence_info = vk::FenceCreateInfo::default();
        let fence = match unsafe { context.device.create_fence(&fence_info, None) } {
            Ok(fence) => fence,
            Err(error) => {
                unsafe {
                    // The command buffer is freed automatically when its pool is destroyed.
                    context.device.destroy_command_pool(command_pool, None);
                    context
                        .device
                        .destroy_descriptor_pool(descriptor_pool, None);
                    readback_buffer.destroy(&context.device);
                    uniform_buffer.destroy(&context.device);
                    vertex_buffer.destroy(&context.device);
                    targets.destroy(&context.device);
                    pipeline.destroy(&context.device);
                }
                return Err(error.into());
            }
        };

        Ok(Scene {
            device: context.device.clone(),
            pipeline,
            targets,
            vertex_buffer,
            uniform_buffer,
            readback_buffer,
            descriptor_pool,
            descriptor_set,
            command_pool,
            command_buffer,
            fence,
        })
    }

    /// Draw one frame with `uniforms`, and return its `IMAGE_SIZE`×`IMAGE_SIZE` pixels as
    /// tightly-packed RGBA8 bytes, row-major, top row first.
    ///
    /// Records the command buffer, submits it, **waits for the GPU to finish, and only then**
    /// reads the pixels back and returns them.
    ///
    /// # This wait is the whole reason a fixture can write into mapped memory at all
    /// A fixture built on this crate cannot see `fence` — it is private to `Scene`, by design (see
    /// the struct doc: the frame loop belongs to the fixture, not this crate, and a fixture that
    /// could reach into `Scene`'s synchronization primitives could get this wrong in its own loop).
    /// That means a fixture has no way to know, on its own, when it is safe to write the *next*
    /// frame's data into a [`MappedBuffer`] it owns (the CPU fixture's fractal texture) without
    /// racing this frame's GPU reads of the *previous* contents. This method's contract is what
    /// makes that safe anyway: **by the time `draw` returns, the fence has already been waited on,
    /// so every read this frame's GPU work performed of any buffer is complete.** A fixture that
    /// writes its next frame's data only after `draw` returns therefore never races the GPU,
    /// without ever touching a fence itself. This crate's `drawing_twice_gives_the_same_pixels`
    /// test exercises this contract — it draws frame 0, draws a *different* frame in between, then
    /// draws frame 0 again and asserts bit-identical pixels, a result a queued but
    /// not-yet-complete previous submission could not produce reliably — but it does not *verify*
    /// the contract: a version of this method that returned before the wait would make that test
    /// flaky rather than reliably wrong (the race would sometimes still win), so one clean test run
    /// is not proof a future run is guaranteed to also catch a regression here. The contract is
    /// load-bearing regardless of what any single run shows.
    ///
    /// # Errors
    /// Returns an error if any Vulkan call fails, or if the GPU does not finish within
    /// [`FENCE_TIMEOUT_NS`].
    pub fn draw(
        &mut self,
        context: &VulkanContext,
        uniforms: &Uniforms,
    ) -> anyhow::Result<Vec<u8>> {
        // SAFETY: every `ash` call below is FFI into the Vulkan driver; handle validity and
        // ordering are argued inline. `context` is the same context `self` was built from — the
        // caller-checked precondition this whole crate relies on (see `Scene`'s struct doc).
        unsafe { self.draw_inner(context, uniforms) }
    }

    /// The `unsafe` body of [`Scene::draw`].
    unsafe fn draw_inner(
        &mut self,
        context: &VulkanContext,
        uniforms: &Uniforms,
    ) -> anyhow::Result<Vec<u8>> {
        // Serialize into the std140-padded shape the shader actually reads — never the public
        // `Uniforms`' own bytes directly. See `Uniforms`'s doc for why the two differ.
        let gpu_uniforms = GpuUniforms {
            mvp: uniforms.mvp,
            half_width: uniforms.half_width,
            _pad: 0.0,
            center: uniforms.center,
        };
        // SAFETY: `GpuUniforms` is `#[repr(C)]` with every byte accounted for by a named field (the
        // explicit `_pad` occupies what would otherwise be an implicit gap), so reading it as bytes
        // touches no uninitialised padding. `uniform_buffer` was sized to exactly
        // `size_of::<GpuUniforms>()` in `Scene::new`, so the destination is exactly big enough.
        // Writing here is only sound because the *previous* frame's GPU reads of this same buffer
        // are already complete — guaranteed by every earlier `draw` call already having waited on
        // `self.fence` before returning (see this method's doc) — so this write cannot race a read.
        unsafe {
            std::ptr::copy_nonoverlapping(
                (&raw const gpu_uniforms) as *const u8,
                self.uniform_buffer.bytes().as_mut_ptr(),
                std::mem::size_of::<GpuUniforms>(),
            );
        }

        // Re-record the command buffer from scratch. Legal to call `begin_command_buffer` again on
        // an already-recorded buffer only because `command_pool` was created with
        // `RESET_COMMAND_BUFFER` (see `Scene::new`'s doc) — without that flag this would be
        // invalid usage on every call after the first.
        let begin_info = vk::CommandBufferBeginInfo::default()
            // ONE_TIME_SUBMIT: each recording is submitted exactly once before being re-recorded,
            // so the driver need not keep this buffer replayable across multiple submissions.
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            self.device
                .begin_command_buffer(self.command_buffer, &begin_info)
        }?;

        // Two clear values, one per attachment, in the render pass's attachment order (colour then
        // depth — see `pipeline.rs::create_render_pass`). Colour clears to opaque black: the
        // background this crate's own test checks the four corners against. Depth clears to 1.0,
        // the far plane in Vulkan's `0..1` depth convention — the "nothing drawn yet" value that
        // `CompareOp::LESS` compares every fragment's depth against.
        let clears = [
            vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 1.0],
                },
            },
            vk::ClearValue {
                depth_stencil: vk::ClearDepthStencilValue {
                    depth: 1.0,
                    stencil: 0,
                },
            },
        ];
        let rp_begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.pipeline.render_pass)
            .framebuffer(self.targets.framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: IMAGE_SIZE,
                    height: IMAGE_SIZE,
                },
            })
            .clear_values(&clears);
        unsafe {
            self.device.cmd_begin_render_pass(
                self.command_buffer,
                &rp_begin,
                vk::SubpassContents::INLINE,
            )
        };
        unsafe {
            self.device.cmd_bind_pipeline(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.pipeline,
            )
        };

        // The pipeline declared viewport and scissor as dynamic state, so they must be supplied
        // here or the draw is undefined.
        let viewport = vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: IMAGE_SIZE as f32,
            height: IMAGE_SIZE as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        let scissor = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: IMAGE_SIZE,
                height: IMAGE_SIZE,
            },
        };
        unsafe {
            self.device
                .cmd_set_viewport(self.command_buffer, 0, &[viewport])
        };
        unsafe {
            self.device
                .cmd_set_scissor(self.command_buffer, 0, &[scissor])
        };

        unsafe {
            self.device.cmd_bind_vertex_buffers(
                self.command_buffer,
                0,
                &[self.vertex_buffer.buffer],
                &[0],
            )
        };
        unsafe {
            self.device.cmd_bind_descriptor_sets(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.layout,
                0,
                &[self.descriptor_set],
                &[],
            )
        };
        // 60 vertices: 20 unshared, flat-shaded triangular faces (see
        // `rayland_icosa_core::geometry::icosahedron`'s doc).
        unsafe { self.device.cmd_draw(self.command_buffer, 60, 1, 0, 0) };
        // Ending the pass performs the transition to `TRANSFER_SRC_OPTIMAL` declared as the colour
        // attachment's `final_layout`, so the copy below needs no barrier of its own.
        unsafe { self.device.cmd_end_render_pass(self.command_buffer) };

        // Copy the finished colour image into the readback buffer. `buffer_row_length` and
        // `buffer_image_height` are set explicitly — see `rayland-refapp`'s `render.rs` module docs
        // for why this, not an inferred stride, is what makes the row-by-row unpacking below sound.
        let copy = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(IMAGE_SIZE)
            .buffer_image_height(IMAGE_SIZE)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width: IMAGE_SIZE,
                height: IMAGE_SIZE,
                depth: 1,
            });
        unsafe {
            self.device.cmd_copy_image_to_buffer(
                self.command_buffer,
                self.targets.color.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.readback_buffer.buffer,
                &[copy],
            )
        };
        unsafe { self.device.end_command_buffer(self.command_buffer) }?;

        // The fence must be unsignalled before it is submitted against. It is signalled on every
        // successful `draw` call (by the wait below), so it is reset here, unconditionally, right
        // before the submit that will next signal it — see `Scene::new`'s doc for why it starts
        // unsignalled on the very first call too.
        unsafe { self.device.reset_fences(&[self.fence]) }?;

        let cmds = [self.command_buffer];
        let submit = vk::SubmitInfo::default().command_buffers(&cmds);
        unsafe {
            self.device
                .queue_submit(context.queue, &[submit], self.fence)
        }?;
        // The wait this method's doc is about: `vkQueueSubmit` only *queues* the work.
        unsafe {
            self.device
                .wait_for_fences(&[self.fence], true, FENCE_TIMEOUT_NS)
        }?;

        // SAFETY: the fence wait above proves the copy has completed, so the buffer now holds the
        // finished pixels and nothing on the GPU is still writing them.
        Ok(unsafe { self.read_pixels() })
    }

    /// Repack the readback buffer's rows into a tightly-packed RGBA8 `Vec`.
    ///
    /// # Honouring the row stride
    /// The bytes in the buffer are laid out as `IMAGE_SIZE` rows of `row_stride` bytes each — see
    /// `draw_inner`'s copy, which sets `buffer_row_length`/`buffer_image_height` explicitly. This
    /// copies **row by row, advancing by `row_stride`**, rather than one block copy, for the same
    /// reason `rayland-refapp`'s `read_pixels` does: the stride is the *copy's* property, and here
    /// it happens to equal `IMAGE_SIZE * BYTES_PER_PIXEL` exactly (no driver-chosen padding, since
    /// this crate chose `buffer_row_length` itself), but a reader that assumed that rather than
    /// respecting the stride would silently start producing a sheared image the day that stops
    /// being true.
    ///
    /// # Safety
    /// The caller must have already proven the GPU has finished writing `readback_buffer` (a
    /// completed fence wait).
    unsafe fn read_pixels(&mut self) -> Vec<u8> {
        let row_stride = u64::from(IMAGE_SIZE) * u64::from(BYTES_PER_PIXEL);
        let packed_row = IMAGE_SIZE as usize * BYTES_PER_PIXEL as usize;
        let mut pixels = vec![0u8; packed_row * IMAGE_SIZE as usize];
        // Read the mapped bytes before mutating `pixels`, so the two borrows below never overlap.
        let mapped = self.readback_buffer.bytes();
        for row in 0..IMAGE_SIZE as usize {
            let start = row * row_stride as usize;
            // SAFETY: `mapped` covers `IMAGE_SIZE * row_stride` bytes (the buffer's documented
            // size), so `start..start + packed_row` stays inside it for every row (`packed_row <=
            // row_stride` always holds here since the two are equal); `pixels` was sized to hold
            // exactly `IMAGE_SIZE` packed rows.
            pixels[row * packed_row..row * packed_row + packed_row]
                .copy_from_slice(&mapped[start..start + packed_row]);
        }
        pixels
    }
}

impl Drop for Scene {
    /// Wait for the device to go idle, then destroy every Vulkan object this `Scene` owns, in
    /// reverse creation order.
    ///
    /// The `device_wait_idle` call is what makes this sound on *every* path, not just the one where
    /// the last `draw` returned `Ok` — see the struct doc's "Why `Drop` cannot rely on..." section
    /// for the error-path hazard this closes (a timed-out fence wait unwinding into `Drop` with
    /// work still in flight). Its result is deliberately ignored: `Drop` cannot return a `Result`,
    /// there is no caller left to hand an error to, and if the device is lost there is nothing more
    /// sensible to do than proceed and let the destroy calls below fail or no-op as the driver
    /// sees fit. The caller obligation this still relies on (`Scene` must drop before its
    /// `VulkanContext`) is unchanged — see the struct doc.
    fn drop(&mut self) {
        // SAFETY: the caller is relied upon to drop this `Scene` before its `VulkanContext`, so
        // `self.device` is still a live device handle here. Reverse creation order throughout;
        // freeing `command_pool` frees `command_buffer`, and freeing `descriptor_pool` frees
        // `descriptor_set`, so neither needs a separate call.
        unsafe {
            // Block until every submission this `Scene` ever made has finished, closing the
            // fence-timeout error path described in the struct doc. The error case (device lost)
            // is not actionable here, so it is discarded rather than propagated (`Drop` has no
            // `Result` to propagate it through) or panicked on (panicking in `Drop` during an
            // existing unwind would abort the process instead of finishing cleanup).
            let _ = self.device.device_wait_idle();
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.command_pool, None);
            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.readback_buffer.destroy(&self.device);
            self.uniform_buffer.destroy(&self.device);
            self.vertex_buffer.destroy(&self.device);
            self.targets.destroy(&self.device);
            self.pipeline.destroy(&self.device);
        }
    }
}

/// Create the descriptor pool (sized for exactly one set) and the one descriptor set, writing
/// binding 0 (the uniform buffer) unconditionally and binding 1 (the sampler) only when `sampler`
/// is `Some` — mirroring exactly which bindings `pipeline`'s descriptor set layout declares (see
/// `pipeline.rs::create_descriptor_set_layout`).
///
/// # Errors
/// Returns an error if descriptor pool creation or descriptor set allocation fails.
///
/// # Safety
/// `context` and `pipeline` must be live; `uniform_buffer` must outlive the returned descriptor
/// set (nothing unsafe happens if it does not, but every draw against a set referring to a
/// destroyed buffer is undefined behaviour). The returned pool must be destroyed (which frees the
/// set with it) before the device is.
unsafe fn create_descriptor_set(
    context: &VulkanContext,
    pipeline: &IcosaPipeline,
    uniform_buffer: &MappedBuffer,
    sampler: Option<SamplerBinding>,
) -> anyhow::Result<(vk::DescriptorPool, vk::DescriptorSet)> {
    // One uniform-buffer descriptor always; one combined-image-sampler descriptor only if the
    // pipeline's layout declares binding 1 for it.
    let mut pool_sizes = vec![
        vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::UNIFORM_BUFFER)
            .descriptor_count(1),
    ];
    if pipeline.has_sampler_binding {
        pool_sizes.push(
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1),
        );
    }
    let pool_info = vk::DescriptorPoolCreateInfo::default()
        // Exactly one set, ever: this crate allocates one `Scene`-lifetime descriptor set and never
        // frees or reallocates it individually.
        .max_sets(1)
        .pool_sizes(&pool_sizes);
    let descriptor_pool = unsafe { context.device.create_descriptor_pool(&pool_info, None) }?;

    let set_layouts = [pipeline.descriptor_set_layout];
    let alloc_info = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(descriptor_pool)
        .set_layouts(&set_layouts);
    let descriptor_set = match unsafe { context.device.allocate_descriptor_sets(&alloc_info) } {
        Ok(sets) => sets[0],
        Err(error) => {
            unsafe {
                context
                    .device
                    .destroy_descriptor_pool(descriptor_pool, None)
            };
            return Err(error.into());
        }
    };

    // Binding 0: the uniform buffer, its whole (exactly-sized) extent.
    let buffer_info = [vk::DescriptorBufferInfo::default()
        .buffer(uniform_buffer.buffer)
        .offset(0)
        .range(std::mem::size_of::<GpuUniforms>() as u64)];
    let mut writes = vec![
        vk::WriteDescriptorSet::default()
            .dst_set(descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
            .buffer_info(&buffer_info),
    ];

    // Binding 1: the sampler, only when the caller supplied one. `image_info` is declared outside
    // the `if` so it outlives the `writes` array built from it below.
    let image_info = sampler.map(|sampler| {
        [vk::DescriptorImageInfo::default()
            .image_view(sampler.view)
            .sampler(sampler.sampler)
            // The layout every fragment-shader sample of a combined image sampler expects.
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
    });
    if let Some(image_info) = &image_info {
        writes.push(
            vk::WriteDescriptorSet::default()
                .dst_set(descriptor_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(image_info),
        );
    }

    // SAFETY: the caller guarantees `context` is live; every buffer/image referred to by `writes`
    // outlives this call (the caller's contract on `uniform_buffer`/`sampler`).
    unsafe { context.device.update_descriptor_sets(&writes, &[]) };

    Ok((descriptor_pool, descriptor_set))
}

/// Write `pixels` (`IMAGE_SIZE`×`IMAGE_SIZE` tightly-packed RGBA8 bytes, as [`Scene::draw`]
/// returns) to `path` as a PNG.
///
/// # Errors
/// Returns an error if `pixels` is not exactly `IMAGE_SIZE * IMAGE_SIZE * 4` bytes, or if the file
/// cannot be written (a bad path, a full disk, or a permissions failure).
pub fn write_png(path: &Path, pixels: &[u8]) -> anyhow::Result<()> {
    // The bytes are already tightly-packed RGBA8 in the order the encoder wants, because the
    // render target's format was chosen to make that true (see `pipeline::COLOR_FORMAT`), so there
    // is no conversion step here to get wrong.
    image::save_buffer(
        path,
        pixels,
        IMAGE_SIZE,
        IMAGE_SIZE,
        image::ColorType::Rgba8,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`GpuUniforms`]'s byte offsets must match std140's, exactly as `shaders/icosa.vert`'s
    /// compiled SPIR-V declares them.
    ///
    /// This is the regression test for the landmine documented on [`Uniforms`]: it needs no GPU and
    /// runs everywhere, so a future edit that "simplified" `GpuUniforms` back down to `Uniforms`'s
    /// own three fields (dropping `_pad`) fails here, at compile-adjacent speed, rather than only
    /// as a silent wrong-picture defect the day a fragment shader starts reading `half_width` or
    /// `center` for real. The three offsets and the total size are not chosen to make this test
    /// pass — they were read directly from `spirv-dis shaders/icosa.vert.spv`'s
    /// `OpMemberDecorate %Uniforms N Offset` lines while this crate was built, and are transcribed
    /// here, not re-derived.
    #[test]
    fn gpu_uniforms_matches_std140_layout() {
        assert_eq!(
            std::mem::offset_of!(GpuUniforms, mvp),
            0,
            "mvp is the block's first member"
        );
        assert_eq!(
            std::mem::offset_of!(GpuUniforms, half_width),
            64,
            "half_width follows mat4's 64 bytes with no gap"
        );
        assert_eq!(
            std::mem::offset_of!(GpuUniforms, center),
            72,
            "std140 pads a vec2 to an 8-byte-aligned offset; 68 would be Rust's natural (wrong) one"
        );
        assert_eq!(
            std::mem::size_of::<GpuUniforms>(),
            80,
            "std140's block size rounds up to the largest member's base alignment (mat4's 16)"
        );
    }
}
