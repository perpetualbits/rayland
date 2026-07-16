//! [`FractalTexture`]: the sampled image the fractal is uploaded into, and the upload path that
//! fills it — this crate's only unique code besides its frame loop (`main.rs`).
//!
//! # What this module is, and is not, responsible for
//! `rayland-icosa-vk` owns everything both fixtures share — the render pass, the pipeline, the
//! solid's vertex buffer, the uniform buffer, the readback. What is left for *this* fixture to own
//! is exactly the thing that makes it "the CPU fixture": a `DEVICE_LOCAL` image the fragment
//! shader samples, and the copy that gets a CPU-computed fractal into it. `main.rs`'s frame loop
//! owns the staging [`rayland_icosa_vk::MappedBuffer`] the fractal is written into — this module
//! only consumes it, via [`FractalTexture::upload`].
//!
//! # The staging-buffer race, and why `upload` owns its own fence
//! The frame loop is: (1) the CPU writes ~1 MiB of fractal into a mapped staging buffer; (2) a copy
//! command reads that buffer and writes the texture image; (3) `Scene::draw` reads the texture.
//! Step (1) of frame N+1 must not happen until the GPU has *finished reading* the staging buffer in
//! step (2) of frame N — otherwise the CPU could overwrite bytes the copy is still mid-read on, and
//! the texture would end up a torn mixture of two frames' fractals. Vulkan gives no ordering
//! guarantee between two `vkQueueSubmit` calls on the same queue unless something explicit asks for
//! one (the specification's queue-submission model only orders *within* a single submission's own
//! command buffer, via barriers — never *between* submissions, which may overlap, reorder, or run
//! out of program order entirely as far as the application can observe without synchronising).
//!
//! [`FractalTexture::upload`] closes this the same way [`rayland_icosa_vk::Scene::draw`] closes the
//! analogous hazard for its own buffers: it submits its copy commands and **waits on its own fence
//! before returning**. By the time `upload` gives control back to the frame loop, the GPU has
//! provably finished reading the staging buffer — so the *next* frame's `staging.bytes()` write,
//! which the frame loop only ever issues after the current frame's `upload` call has returned, can
//! never race the copy that just read the previous contents. This also gives `upload_us` an honest
//! meaning: without the wait, `upload_us` would measure only how long it takes to *record and
//! submit* the copy, not how long the copy itself takes — a number so small it would suggest the
//! upload is nearly free, which is precisely the false impression this fixture exists to correct.
//!
//! # The layout transitions, and why both are needed
//! A freshly created image is in `UNDEFINED` layout, and the fragment shader can only sample an
//! image in `SHADER_READ_ONLY_OPTIMAL` (or a handful of other read layouts this crate never uses).
//! Getting from one to the other, through the copy, takes two explicit barriers — the driver does
//! not do this automatically, and the render pass's own layout transitions (which handle the colour
//! and depth attachments) have nothing to do with this image, which is never an attachment:
//!
//! 1. `UNDEFINED → TRANSFER_DST_OPTIMAL`, before the copy. `UNDEFINED` is used as the *old* layout
//!    deliberately, not because it happens to match the image's very first state: every upload
//!    overwrites the image's entire contents via the copy that immediately follows, so there is
//!    nothing in the image worth preserving across the transition on any upload, first or
//!    hundredth. Vulkan explicitly permits `UNDEFINED` as an old layout whenever the previous
//!    contents may be discarded (`vkspec` §"Image Layout Transitions": the driver "does not need to
//!    preserve the contents of the image" in that case) — using it here means [`FractalTexture`]
//!    tracks no "am I on the first upload" state and the same two-barrier sequence runs unchanged
//!    every frame.
//! 2. `TRANSFER_DST_OPTIMAL → SHADER_READ_ONLY_OPTIMAL`, after the copy. Without this, the image
//!    stays in a transfer-only layout, and sampling it (`SamplerBinding`'s use in `Scene`'s
//!    descriptor set demands `SHADER_READ_ONLY_OPTIMAL`, see `rayland_icosa_vk`'s
//!    `create_descriptor_set`) is undefined layout usage — some drivers render garbage, some read
//!    stale texels, and validation (were it enabled) would reject it outright.
//!
//! Both barriers also carry stage/access masks, not just layouts — a layout transition with no
//! access mask says nothing about *visibility*, only about the bit pattern the image is
//! reinterpreted as, and a reader on the wrong side of a memory-visibility gap can still see stale
//! or torn data even though the layout itself is correct. Barrier 1 is `srcAccessMask = 0` (nothing
//! to wait on: the layout was `UNDEFINED`, so there is no prior write in this image to be visible
//! yet) to `dstAccessMask = TRANSFER_WRITE` at the `TRANSFER` stage — exactly what the copy needs.
//! Barrier 2 is `srcAccessMask = TRANSFER_WRITE` at `TRANSFER` to `dstAccessMask = SHADER_READ` at
//! `FRAGMENT_SHADER` — exactly what the sampling draw call needs. Because `upload` waits its own
//! fence before returning (see above), the *draw* that samples this image is a wholly separate,
//! later submission that only ever begins after the host has observed the upload's completion —
//! so by the time `Scene::draw` records its command buffer, this second barrier's writes are
//! already visible to any subsequent read, with no further synchronisation required between the two
//! submissions.
//!
//! # Why the sampler is `LINEAR`, and why that is load-bearing
//! `rayland_icosa_core::fractal::render_into` iterates the sampled UV triangle dilated by two
//! texels precisely so that a bilinear fetch landing just inside a face's edge never reads an
//! un-iterated (and therefore black) padding texel — see that module's doc comment ("Why 'near that
//! triangle' and not 'inside that triangle'"). Switching this sampler to `NEAREST` would not be
//! merely a quality regression: it would silently stop exercising the one hazard that dilation
//! exists to guard against, and the two fixtures could then diverge at face edges without either
//! fixture's own tests noticing.

use ash::vk;
use rayland_icosa_core::TEXTURE_SIZE;

use rayland_icosa_vk::{MappedBuffer, SamplerBinding, VulkanContext};

/// The fractal texture format: four 8-bit unsigned normalised channels, matching the RGBA8 bytes
/// [`rayland_icosa_core::fractal::render_into`] writes with no conversion in between.
const TEXTURE_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// How long to wait for an upload's copy to finish before giving up, in nanoseconds (10 seconds).
///
/// Matches `rayland_icosa_vk::Scene`'s own fence timeout and its reasoning: a copy of one megabyte
/// finishes in microseconds on any functioning GPU, so anything that takes ten seconds is wedged,
/// not slow, and should fail loudly rather than hang the process forever.
const FENCE_TIMEOUT_NS: u64 = 10_000_000_000;

/// The `DEVICE_LOCAL` image the fractal is uploaded into, its view, its sampler, and the small
/// amount of Vulkan machinery ([`FractalTexture::upload`]'s own command pool, command buffer, and
/// fence) needed to record and submit the copy that fills it.
///
/// Created once, in [`FractalTexture::new`], and reused for every [`FractalTexture::upload`] call —
/// the image is never resized or recreated, only overwritten, once per frame.
pub struct FractalTexture {
    /// The `DEVICE_LOCAL` `TEXTURE_SIZE`×`TEXTURE_SIZE` image the fragment shader samples.
    image: vk::Image,
    /// The memory backing `image`.
    memory: vk::DeviceMemory,
    /// A view over the whole of `image`; what the sampler descriptor binds.
    view: vk::ImageView,
    /// Filtering and addressing rules for reading `image`. `LINEAR` and `CLAMP_TO_EDGE` — see the
    /// module docs for why `LINEAR` is load-bearing, not a quality choice.
    sampler: vk::Sampler,
    /// The pool [`FractalTexture::upload`]'s command buffer is allocated from. Kept for the
    /// texture's whole lifetime, like `rayland_icosa_vk::Scene`'s own command pool, rather than
    /// created and destroyed per upload.
    command_pool: vk::CommandPool,
    /// The one command buffer every upload re-records, exactly as `Scene::draw` re-records its own
    /// single command buffer. `command_pool` is created with `RESET_COMMAND_BUFFER` for the same
    /// reason `Scene::new`'s is (see that type's doc): without it, re-recording an already-recorded
    /// buffer is invalid.
    command_buffer: vk::CommandBuffer,
    /// Signalled when an upload's copy submission completes; waited on, then reset, by every
    /// [`FractalTexture::upload`] call. See the module docs for why this wait is not optional.
    fence: vk::Fence,
}

impl FractalTexture {
    /// Create the texture image, its view, its sampler, and the command pool/buffer/fence
    /// [`FractalTexture::upload`] submits through.
    ///
    /// The image is created `UNDEFINED`, `DEVICE_LOCAL`, `TRANSFER_DST | SAMPLED`: `TRANSFER_DST`
    /// because every frame's fractal arrives via a buffer-to-image copy, `SAMPLED` because the
    /// fragment shader reads it through a combined image sampler. No data is written into it here —
    /// the image starts genuinely empty and stays that way until the first [`FractalTexture::upload`]
    /// call, which is always issued before the first draw (see `main.rs`'s frame loop).
    ///
    /// # Errors
    /// Returns an error if any Vulkan creation, allocation, or binding call fails.
    pub fn new(context: &VulkanContext) -> anyhow::Result<FractalTexture> {
        // SAFETY: every `ash` call below is FFI into the Vulkan driver, which trusts the caller for
        // handle validity and sizes. Each argument is constructed immediately before the call that
        // uses it, and every partially-built piece is torn down on its own error path so a failure
        // part-way through leaks nothing.
        unsafe { FractalTexture::new_inner(context) }
    }

    /// The `unsafe` body of [`FractalTexture::new`].
    unsafe fn new_inner(context: &VulkanContext) -> anyhow::Result<FractalTexture> {
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(TEXTURE_FORMAT)
            .extent(vk::Extent3D {
                width: TEXTURE_SIZE,
                height: TEXTURE_SIZE,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            // OPTIMAL: this image is never mapped by the CPU (the staging buffer is what gets
            // mapped), so nothing forces `LINEAR` tiling's row-pitch constraints on it.
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { context.device.create_image(&image_info, None) }?;

        // DEVICE_LOCAL: only the GPU ever touches this image directly (the CPU writes the fractal
        // into the staging buffer instead, and a copy command bridges the two).
        let requirements = unsafe { context.device.get_image_memory_requirements(image) };
        let memory = match unsafe {
            context.allocate(requirements, vk::MemoryPropertyFlags::DEVICE_LOCAL)
        } {
            Ok(memory) => memory,
            Err(error) => {
                unsafe { context.device.destroy_image(image, None) };
                return Err(error);
            }
        };
        if let Err(error) = unsafe { context.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                context.device.destroy_image(image, None);
                context.device.free_memory(memory, None);
            }
            return Err(error.into());
        }

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(TEXTURE_FORMAT)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        let view = match unsafe { context.device.create_image_view(&view_info, None) } {
            Ok(view) => view,
            Err(error) => {
                unsafe {
                    context.device.destroy_image(image, None);
                    context.device.free_memory(memory, None);
                }
                return Err(error.into());
            }
        };

        // LINEAR filtering and CLAMP_TO_EDGE addressing — see the module docs for why LINEAR is
        // load-bearing (the fractal's dilation margin exists specifically for this filter mode) and
        // not merely a visual preference. CLAMP_TO_EDGE, rather than REPEAT, because every sampled
        // UV lies within `FACE_UVS`'s dilated triangle, comfortably inside `[0, 1]` on both axes
        // (see `rayland_icosa_core::fractal`'s doc comment for the bounding box) — addressing modes
        // never actually engage for this scene, so the choice matters only as a matter of not
        // asserting a wrapping behaviour this program never relies on.
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
            .min_lod(0.0)
            .max_lod(0.0)
            .anisotropy_enable(false)
            .compare_enable(false)
            .unnormalized_coordinates(false);
        let sampler = match unsafe { context.device.create_sampler(&sampler_info, None) } {
            Ok(sampler) => sampler,
            Err(error) => {
                unsafe {
                    context.device.destroy_image_view(view, None);
                    context.device.destroy_image(image, None);
                    context.device.free_memory(memory, None);
                }
                return Err(error.into());
            }
        };

        // RESET_COMMAND_BUFFER — see the struct doc for why this must match Scene's own reasoning:
        // this pool's one command buffer is re-recorded, not reallocated, on every upload.
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(context.queue_family_index)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = match unsafe { context.device.create_command_pool(&pool_info, None) } {
            Ok(pool) => pool,
            Err(error) => {
                unsafe {
                    context.device.destroy_sampler(sampler, None);
                    context.device.destroy_image_view(view, None);
                    context.device.destroy_image(image, None);
                    context.device.free_memory(memory, None);
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
                    context.device.destroy_sampler(sampler, None);
                    context.device.destroy_image_view(view, None);
                    context.device.destroy_image(image, None);
                    context.device.free_memory(memory, None);
                }
                return Err(error.into());
            }
        };

        // Created unsignalled, matching `Scene`'s own reasoning: the first `upload` call resets it
        // anyway, but starting unsignalled avoids the fence ever being in a surprising state before
        // the first submission.
        let fence_info = vk::FenceCreateInfo::default();
        let fence = match unsafe { context.device.create_fence(&fence_info, None) } {
            Ok(fence) => fence,
            Err(error) => {
                unsafe {
                    // The command buffer is freed automatically when its pool is destroyed.
                    context.device.destroy_command_pool(command_pool, None);
                    context.device.destroy_sampler(sampler, None);
                    context.device.destroy_image_view(view, None);
                    context.device.destroy_image(image, None);
                    context.device.free_memory(memory, None);
                }
                return Err(error.into());
            }
        };

        Ok(FractalTexture {
            image,
            memory,
            view,
            sampler,
            command_pool,
            command_buffer,
            fence,
        })
    }

    /// The view and sampler [`rayland_icosa_vk::Scene::new`] binds at descriptor binding 1.
    ///
    /// Returns plain Vulkan handles (both `Copy`), not a borrow — `SamplerBinding` outlives this
    /// call by referring to the same underlying objects `self` owns, which is sound only because
    /// `self` (this `FractalTexture`) is guaranteed by `main.rs`'s declaration order to outlive the
    /// `Scene` built from this binding (see `main.rs`'s module doc for the drop-order argument).
    pub fn sampler_binding(&self) -> SamplerBinding {
        SamplerBinding {
            view: self.view,
            sampler: self.sampler,
        }
    }

    /// Copy `staging`'s bytes into the texture image, transitioning it from `UNDEFINED` (or,
    /// implicitly, whatever it was left in — see the module docs for why `UNDEFINED` is always used
    /// as the old layout) to `SHADER_READ_ONLY_OPTIMAL`, and **wait for the GPU to finish before
    /// returning**.
    ///
    /// # Why this must be called after `staging.bytes()` is filled and before `Scene::draw`
    /// This is the one Vulkan call that touches the megabyte the frame loop just wrote — see the
    /// module docs' "staging-buffer race" section for the full argument that this call's own fence
    /// wait is what makes it safe for the *next* frame to overwrite `staging` immediately after
    /// this returns, and for `Scene::draw`, called immediately after this returns, to see the
    /// finished texture with no further synchronisation of its own.
    ///
    /// # Errors
    /// Returns an error if any Vulkan call fails, or if the GPU does not finish within
    /// [`FENCE_TIMEOUT_NS`].
    pub fn upload(&self, context: &VulkanContext, staging: &MappedBuffer) -> anyhow::Result<()> {
        // SAFETY: every `ash` call below is FFI into the Vulkan driver; handle validity and
        // ordering are argued inline. `context` is the same context `self` was built from, and
        // `staging` is sized `TEXTURE_SIZE² × 4` bytes — both are the caller's contract, matching
        // every other method in this crate.
        unsafe { self.upload_inner(context, staging) }
    }

    /// The `unsafe` body of [`FractalTexture::upload`].
    unsafe fn upload_inner(
        &self,
        context: &VulkanContext,
        staging: &MappedBuffer,
    ) -> anyhow::Result<()> {
        let begin_info = vk::CommandBufferBeginInfo::default()
            // ONE_TIME_SUBMIT: this recording is submitted exactly once before being re-recorded
            // next frame, so the driver need not keep it replayable across multiple submissions.
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            context
                .device
                .begin_command_buffer(self.command_buffer, &begin_info)
        }?;

        let subresource = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };

        // Barrier 1: UNDEFINED -> TRANSFER_DST_OPTIMAL. See the module docs ("The layout
        // transitions") for why UNDEFINED is deliberate here on every call, not just the first, and
        // for why srcAccessMask is empty (nothing in this image is worth waiting to become visible
        // when its old layout is UNDEFINED).
        let to_transfer_dst = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.image)
            .subresource_range(subresource)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
        unsafe {
            context.device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_transfer_dst],
            )
        };

        // The copy itself. `buffer_row_length`/`buffer_image_height` are set explicitly (both equal
        // to the image's own extent, since the staging buffer is tightly packed with no padding) —
        // matching `rayland_icosa_vk::Scene`'s own readback copy's reasoning for never leaving a
        // stride to be inferred.
        let copy = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(TEXTURE_SIZE)
            .buffer_image_height(TEXTURE_SIZE)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width: TEXTURE_SIZE,
                height: TEXTURE_SIZE,
                depth: 1,
            });
        unsafe {
            context.device.cmd_copy_buffer_to_image(
                self.command_buffer,
                staging.buffer,
                self.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[copy],
            )
        };

        // Barrier 2: TRANSFER_DST_OPTIMAL -> SHADER_READ_ONLY_OPTIMAL. See the module docs for why
        // both the layout and the access/stage masks are required for the fragment shader's later
        // sample to be sound.
        let to_shader_read = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.image)
            .subresource_range(subresource)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ);
        unsafe {
            context.device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_shader_read],
            )
        };

        unsafe { context.device.end_command_buffer(self.command_buffer) }?;

        // The fence must be unsignalled before it is submitted against — reset here, unconditionally,
        // right before the submit that will next signal it, matching `Scene::draw`'s own reasoning.
        unsafe { context.device.reset_fences(&[self.fence]) }?;

        let cmds = [self.command_buffer];
        let submit = vk::SubmitInfo::default().command_buffers(&cmds);
        unsafe {
            context
                .device
                .queue_submit(context.queue, &[submit], self.fence)
        }?;
        // The wait the module docs' "staging-buffer race" section is about: `vkQueueSubmit` only
        // *queues* the work. Waiting here, before returning, is what makes it safe for the frame
        // loop to immediately overwrite `staging` and for `Scene::draw` to immediately sample
        // `self.image` with no synchronisation of its own.
        unsafe {
            context
                .device
                .wait_for_fences(&[self.fence], true, FENCE_TIMEOUT_NS)
        }?;

        Ok(())
    }

    /// Destroy every Vulkan object this `FractalTexture` owns.
    ///
    /// # Why this is an explicit method rather than `Drop`
    /// Mirrors `rayland-refapp`'s established pattern (see `rayland_icosa_vk::Scene`'s struct doc
    /// for the fuller argument): `main.rs` calls this exactly once, on the success path, after every
    /// frame's `upload` and `draw` have already fence-waited to completion — so by the time this
    /// runs, the GPU is provably done with every object here, and no `device_wait_idle` of its own
    /// is needed first. `Drop`'s job of covering an *error* path (a fence timeout unwinding into
    /// teardown while work may still be in flight) already belongs to `Scene`'s `Drop`: `main.rs`
    /// builds its `Scene` inside a nested block that ends — running `Scene::drop`'s unconditional
    /// `device_wait_idle`, which waits for the *whole device*, not just `Scene`'s own submissions —
    /// before this method is ever called. See `main.rs`'s module doc ("Object lifetime and drop
    /// order") for that ordering argument in full.
    ///
    /// # Safety
    /// `device` must be the live device this came from, and the GPU must be done with every object
    /// here — guaranteed, on the call site this crate uses, by the ordering argued above.
    pub unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: per the caller's guarantee. The command buffer is freed with its pool; reverse
        // creation order otherwise.
        unsafe {
            device.destroy_fence(self.fence, None);
            device.destroy_command_pool(self.command_pool, None);
            device.destroy_sampler(self.sampler, None);
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}
