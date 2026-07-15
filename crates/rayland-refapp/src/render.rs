//! *What* is drawn, and getting it back: the colour image, the vertex data, the command buffer,
//! the submission, and the copy back to CPU memory.
//!
//! # Why the image is rendered OPTIMAL and copied, rather than mapped directly
//! The obvious-looking shortcut for an off-screen program is to create the colour image with
//! `LINEAR` tiling in host-visible memory and just `vkMapMemory` it after the draw. It works, and
//! it is a trap. A `LINEAR` image's rows are padded to a **driver-chosen `rowPitch`** that has no
//! obligation to equal `width * 4`; a program that assumes it does gets an image that is subtly
//! sheared — each row shifted a little further than the last — which looks like a rendering bug
//! and is not one. The pitch must be read back with `vkGetImageSubresourceLayout` and honoured.
//!
//! This module sidesteps that entirely: the triangle is drawn into an `OPTIMAL`-tiled image (whose
//! layout is driver-private and not mappable at all, so the trap cannot even be reached) and then
//! `vkCmdCopyImageToBuffer` copies it into a plain buffer, where **this program**, not the driver,
//! chooses the row stride. See [`read_pixels`] for how that stride is then honoured on the way out
//! — it is honoured rather than assumed precisely because the assumption is the classic bug.
//!
//! # Everything here is one-shot
//! This program renders exactly one frame and exits, so every object below is created, used, and
//! destroyed within a single [`render_triangle`] call. A program that rendered repeatedly would
//! hoist most of them out; there is nothing to be gained by doing so here, and a great deal of
//! clarity to be lost.

// The Vulkan API surface and its handle/struct types.
use ash::vk;
// The device, queue, and memory-type table every object below is created against.
use crate::context::{VulkanContext, allocate};
// The render pass and pipeline the draw is recorded against, and the vertex layout it consumes.
use crate::pipeline::{COLOR_FORMAT, TrianglePipeline, Vertex};

/// Bytes per pixel in [`COLOR_FORMAT`]: four 8-bit channels.
///
/// Named rather than written as a bare `4` at each use, because the same number means two
/// different things here — the size of a pixel in the GPU image and the size of a pixel in the
/// output buffer — and they are only equal because the copy does no format conversion.
const BYTES_PER_PIXEL: u32 = 4;

/// How long to wait for the GPU to finish before giving up, in nanoseconds (10 seconds).
///
/// A timeout, rather than `u64::MAX`, so that a wedged GPU or a hung remoting path fails as a
/// diagnosable error instead of an unkillable process that a test harness must eventually notice
/// has not exited. Ten seconds is enormously more than a 64×64 triangle needs even when every
/// command is being serialized across a socket and replayed on another driver; anything that
/// exceeds it is broken, not slow.
const FENCE_TIMEOUT_NS: u64 = 10_000_000_000;

/// The colour image, the memory behind it, its view, and the framebuffer binding it to the render
/// pass — the four objects that all depend on the frame's concrete size.
struct ColorTarget {
    /// The `OPTIMAL`-tiled image the triangle is drawn into and then copied out of.
    image: vk::Image,
    /// The device memory backing `image`.
    memory: vk::DeviceMemory,
    /// A view over the whole of `image`; the framebuffer needs one, the image itself will not do.
    view: vk::ImageView,
    /// Binds `view` to the render pass at this frame's exact width and height.
    framebuffer: vk::Framebuffer,
}

impl ColorTarget {
    /// Create the image, allocate and bind its memory, and build the view and framebuffer.
    ///
    /// The image is created with `COLOR_ATTACHMENT` (the render pass draws into it) and
    /// `TRANSFER_SRC` (the readback copies out of it). Both are required: Vulkan rejects an image
    /// used in a way its `usage` flags did not declare, and forgetting `TRANSFER_SRC` here is a
    /// failure that surfaces confusingly far away, at the copy.
    ///
    /// # Errors
    /// Returns an error if image creation, allocation, binding, view creation, or framebuffer
    /// creation fails.
    ///
    /// # Safety
    /// `ctx` and `pipeline` must be live; the returned target must be destroyed via
    /// [`ColorTarget::destroy`] before the device is.
    unsafe fn new(
        ctx: &VulkanContext,
        pipeline: &TrianglePipeline,
        width: u32,
        height: u32,
    ) -> anyhow::Result<ColorTarget> {
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(COLOR_FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                // A 2-D image still has a depth, and it must be exactly 1; 0 is a validation error.
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            // OPTIMAL: let the driver lay the pixels out however its hardware likes. This is the
            // choice that makes the image unmappable and therefore makes the row-pitch trap
            // described in the module docs unreachable.
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
            // Matches the render pass's `initial_layout`; the clear overwrites everything anyway.
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: the caller guarantees the device is live; `image_info` outlives the call.
        let image = unsafe { ctx.device.create_image(&image_info, None) }?;

        // DEVICE_LOCAL: this image is only ever touched by the GPU, so put it in the memory the
        // GPU reads fastest. Nothing on the CPU ever maps it — that is what the readback is for.
        let requirements = unsafe { ctx.device.get_image_memory_requirements(image) };
        let memory = unsafe {
            allocate(
                &ctx.device,
                &ctx.mem_props,
                requirements,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
        }?;
        unsafe { ctx.device.bind_image_memory(image, memory, 0) }?;

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(COLOR_FORMAT)
            .subresource_range(vk::ImageSubresourceRange {
                // The colour plane — the only one a colour format has.
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        let view = unsafe { ctx.device.create_image_view(&view_info, None) }?;

        let attachments = [view];
        let framebuffer_info = vk::FramebufferCreateInfo::default()
            .render_pass(pipeline.render_pass)
            .attachments(&attachments)
            .width(width)
            .height(height)
            // One layer: a plain 2-D image, no array layers, no multiview.
            .layers(1);
        let framebuffer = unsafe { ctx.device.create_framebuffer(&framebuffer_info, None) }?;

        Ok(ColorTarget {
            image,
            memory,
            view,
            framebuffer,
        })
    }

    /// Destroy all four objects, in the reverse of the order they were created — each one refers
    /// to the one before it, and Vulkan does not check.
    ///
    /// # Safety
    /// `device` must be the live device these came from, and the GPU must be done with them (the
    /// caller guarantees this by fence-waiting its submission before tearing down).
    unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: per the caller's guarantee; reverse creation order respects the references.
        unsafe {
            device.destroy_framebuffer(self.framebuffer, None);
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// A buffer in memory the CPU can map, plus that memory.
///
/// Used for both of this program's host-visible buffers — the vertex data on the way in and the
/// pixels on the way out — because the two differ only in their usage flags and size. Sharing one
/// type keeps the "allocate, bind, and remember to free both halves" dance in one place instead of
/// two subtly diverging copies.
struct HostBuffer {
    /// The buffer handle.
    buffer: vk::Buffer,
    /// The host-visible, host-coherent memory bound to it.
    memory: vk::DeviceMemory,
}

impl HostBuffer {
    /// Create a buffer of `size` bytes with the given `usage`, backed by memory the CPU can map.
    ///
    /// The memory is requested `HOST_VISIBLE | HOST_COHERENT`. `HOST_VISIBLE` is what makes
    /// `vkMapMemory` legal at all. `HOST_COHERENT` is what makes the mapping *correct* without
    /// further ceremony: on non-coherent memory the CPU and GPU views can diverge, and a program
    /// must call `vkFlushMappedMemoryRanges` after writing and `vkInvalidateMappedMemoryRanges`
    /// before reading, or it will see stale bytes. Asking for coherent memory buys the guarantee
    /// outright. Every driver is required to expose at least one such type, so this never fails
    /// for lack of a candidate.
    ///
    /// # Errors
    /// Returns an error if buffer creation, allocation, or binding fails.
    ///
    /// # Safety
    /// `ctx` must be live; the returned buffer must be destroyed via [`HostBuffer::destroy`].
    unsafe fn new(
        ctx: &VulkanContext,
        size: u64,
        usage: vk::BufferUsageFlags,
    ) -> anyhow::Result<HostBuffer> {
        let info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            // Only one queue family ever touches these buffers, so exclusive ownership is correct
            // and lets the driver skip the coherence work CONCURRENT would imply.
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: the caller guarantees the device is live; `info` outlives the call.
        let buffer = unsafe { ctx.device.create_buffer(&info, None) }?;
        let requirements = unsafe { ctx.device.get_buffer_memory_requirements(buffer) };
        let memory = unsafe {
            allocate(
                &ctx.device,
                &ctx.mem_props,
                requirements,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
        }?;
        unsafe { ctx.device.bind_buffer_memory(buffer, memory, 0) }?;
        Ok(HostBuffer { buffer, memory })
    }

    /// Destroy the buffer and free its memory.
    ///
    /// # Safety
    /// `device` must be the live device this came from, the memory must not still be mapped, and
    /// the GPU must be done with the buffer.
    unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: per the caller's guarantee; the buffer must go before the memory it is bound to.
        unsafe {
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// Draw `vertices` into a `width`×`height` image cleared to `clear_color`, and return the result
/// as tightly-packed RGBA8 bytes, row-major, top row first.
///
/// This is the whole program's actual work. It creates every Vulkan object it needs, records one
/// command buffer that both draws the triangle and copies the finished image into a mappable
/// buffer, submits it, waits for the GPU to finish, reads the pixels out, and destroys everything
/// it created before returning.
///
/// # Why one submission rather than two
/// The draw and the readback copy could be submitted separately, with a fence-wait between them.
/// They are not, because they do not need to be: the render pass's subpass dependency (see
/// [`crate::pipeline`]) already makes the colour writes visible to the transfer-stage read that
/// follows, *within* a command buffer. One submission is therefore both correct and simpler, and
/// it is what an ordinary Vulkan program would do. The single fence-wait before mapping is not
/// optional, though — see below.
///
/// # Why the fence-wait before mapping is load-bearing
/// `vkQueueSubmit` returns as soon as the work is *queued*, not when it is done. Mapping the
/// readback buffer without waiting reads whatever happens to be in that memory at that instant —
/// very likely uninitialised garbage, and on a fast enough machine occasionally the right answer,
/// which is the worst possible outcome because it makes the bug intermittent. The fence is what
/// turns "submitted" into "finished".
///
/// # Errors
/// Returns an error if any Vulkan call fails, or if the GPU does not finish within
/// [`FENCE_TIMEOUT_NS`].
pub fn render_triangle(
    ctx: &VulkanContext,
    width: u32,
    height: u32,
    clear_color: [f32; 4],
    vertices: &[Vertex],
) -> anyhow::Result<Vec<u8>> {
    // SAFETY: every `ash` call below is FFI into the Vulkan driver, which trusts the caller for
    // handle validity, pointer liveness, and sizes. Each argument is constructed immediately
    // before the call that uses it; every object is destroyed exactly once, after the fence proves
    // the GPU is no longer using it; and no handle is touched after its destruction.
    unsafe { render_triangle_inner(ctx, width, height, clear_color, vertices) }
}

/// The `unsafe` body of [`render_triangle`], separated so the public function stays safe to call
/// and the safety argument lives in exactly one place.
unsafe fn render_triangle_inner(
    ctx: &VulkanContext,
    width: u32,
    height: u32,
    clear_color: [f32; 4],
    vertices: &[Vertex],
) -> anyhow::Result<Vec<u8>> {
    let pipeline = unsafe { TrianglePipeline::new(&ctx.device) }?;
    let target = unsafe { ColorTarget::new(ctx, &pipeline, width, height) }?;

    // --- The vertex data, copied into memory the GPU can read ---
    let vertex_bytes = std::mem::size_of_val(vertices) as u64;
    let vertex_buffer =
        unsafe { HostBuffer::new(ctx, vertex_bytes, vk::BufferUsageFlags::VERTEX_BUFFER) }?;
    let mapped = unsafe {
        ctx.device.map_memory(
            vertex_buffer.memory,
            0,
            vertex_bytes,
            vk::MemoryMapFlags::empty(),
        )
    }?;
    // SAFETY: `mapped` is a live mapping of exactly `vertex_bytes` bytes, `vertices` is a live
    // slice of exactly that many bytes, and the two cannot overlap (one is a fresh GPU mapping).
    // `Vertex` is `#[repr(C)]` and contains only `f32`s, so its bytes mean the same thing to the
    // GPU as they do here — this reinterpretation would not be sound for a `repr(Rust)` type.
    unsafe {
        std::ptr::copy_nonoverlapping(
            vertices.as_ptr() as *const u8,
            mapped as *mut u8,
            vertex_bytes as usize,
        )
    };
    // The memory is HOST_COHERENT, so unmapping is enough — no explicit flush is needed for the
    // GPU to see these bytes.
    unsafe { ctx.device.unmap_memory(vertex_buffer.memory) };

    // --- Where the pixels will land. Sized in u64 throughout: `width * height * 4` in u32
    // arithmetic overflows silently for large images in release builds, sizing the buffer too
    // small and corrupting memory. The values here are small and fixed, but the habit is what
    // keeps this correct the day they are not. ---
    let row_stride = width as u64 * BYTES_PER_PIXEL as u64;
    let readback_size = row_stride * height as u64;
    let readback =
        unsafe { HostBuffer::new(ctx, readback_size, vk::BufferUsageFlags::TRANSFER_DST) }?;

    // --- Record and run the frame ---
    let command_pool = unsafe {
        ctx.device.create_command_pool(
            &vk::CommandPoolCreateInfo::default().queue_family_index(ctx.queue_family_index),
            None,
        )
    }?;
    let result = unsafe {
        record_submit_and_read(
            ctx,
            &pipeline,
            &target,
            &vertex_buffer,
            &readback,
            command_pool,
            width,
            height,
            clear_color,
            vertices.len() as u32,
            row_stride,
            readback_size,
        )
    };

    // --- Tear down, but ONLY on success. ---
    //
    // The tempting thing is to destroy these unconditionally, so that a mid-way error does not leak
    // them. That would be a bug. If `record_submit_and_read` failed because the fence timed out
    // (see `FENCE_TIMEOUT_NS`), the submission is still *in flight*: the GPU may be reading the
    // vertex buffer, writing the image, and executing the pipeline at this very moment. Destroying
    // a Vulkan object the GPU is still using is undefined behaviour — Vulkan tracks no references
    // and will not stop us — and it would corrupt the driver while it is already wedged, turning a
    // clean, diagnosable timeout into a crash somewhere unrelated.
    //
    // Distinguishing "failed before submit, nothing in flight, safe to destroy" from "failed after
    // submit, work in flight, unsafe to destroy" is possible but is exactly the kind of subtle
    // condition that rots. So this takes the simple, always-correct option: on any error, destroy
    // nothing here — and this function does not own `VulkanContext`, so it cannot leak it either.
    // The caller (`main.rs`'s `run()`) is the one that owns `ctx`, and on this `Err` path it
    // `std::mem::forget`s it rather than letting it drop, precisely so that `VulkanContext::drop`
    // (`vkDestroyDevice`) never runs over a submission that may still be executing. Only with that
    // in place is it true that every error path here leads to `main` printing the cause and calling
    // `std::process::exit(1)`, and process exit returns all of it — GPU allocations included — to
    // the driver and the kernel. Leaking on the way out of a failing one-shot program costs
    // nothing; the undefined behaviour would cost a great deal.
    //
    // On the success path the fence has already proven the GPU is finished, so every destroy below
    // is safe. Destroying the pool frees the command buffer allocated from it, which is why that
    // needs no separate cleanup. Reverse creation order throughout.
    if result.is_ok() {
        unsafe {
            ctx.device.destroy_command_pool(command_pool, None);
            readback.destroy(&ctx.device);
            vertex_buffer.destroy(&ctx.device);
            target.destroy(&ctx.device);
            pipeline.destroy(&ctx.device);
        }
    }

    result
}

/// Record the draw and the readback copy into one command buffer, submit it, wait for the GPU, and
/// read the pixels back.
///
/// Split out from [`render_triangle_inner`] purely so that the caller can destroy the Vulkan
/// objects on every path, including the ones this function fails on — the split is about cleanup,
/// not about the work being separable.
///
/// # Errors
/// Returns an error if any Vulkan call fails or the fence times out.
///
/// # Safety
/// Every handle passed in must be live and belong to `ctx`'s device.
#[allow(clippy::too_many_arguments)]
unsafe fn record_submit_and_read(
    ctx: &VulkanContext,
    pipeline: &TrianglePipeline,
    target: &ColorTarget,
    vertex_buffer: &HostBuffer,
    readback: &HostBuffer,
    command_pool: vk::CommandPool,
    width: u32,
    height: u32,
    clear_color: [f32; 4],
    vertex_count: u32,
    row_stride: u64,
    readback_size: u64,
) -> anyhow::Result<Vec<u8>> {
    // SAFETY: the caller guarantees every handle is live and from this device.
    let cmd = unsafe {
        ctx.device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )
    }?[0];
    unsafe {
        ctx.device.begin_command_buffer(
            cmd,
            // ONE_TIME_SUBMIT: this buffer is recorded, submitted once, and thrown away. Saying so
            // lets the driver skip the bookkeeping a re-submittable buffer would need.
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
    }?;

    // Begin the render pass. Its CLEAR load-op consumes this value, painting every pixel blue
    // before the triangle is drawn over the middle of it.
    let clears = [vk::ClearValue {
        color: vk::ClearColorValue {
            float32: clear_color,
        },
    }];
    let rp_begin = vk::RenderPassBeginInfo::default()
        .render_pass(pipeline.render_pass)
        .framebuffer(target.framebuffer)
        .render_area(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D { width, height },
        })
        .clear_values(&clears);
    unsafe {
        ctx.device
            .cmd_begin_render_pass(cmd, &rp_begin, vk::SubpassContents::INLINE)
    };
    unsafe {
        ctx.device
            .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline.pipeline)
    };

    // The pipeline declared viewport and scissor as dynamic state, so they must be supplied here
    // or the draw is undefined. The viewport maps normalised device coordinates onto the image;
    // the scissor discards anything outside it (nothing, here — it covers the whole image).
    let viewport = vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: width as f32,
        height: height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    };
    let scissor = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D { width, height },
    };
    unsafe { ctx.device.cmd_set_viewport(cmd, 0, &[viewport]) };
    unsafe { ctx.device.cmd_set_scissor(cmd, 0, &[scissor]) };

    // Bind the vertex data at binding 0, offset 0 — matching the pipeline's single binding.
    unsafe {
        ctx.device
            .cmd_bind_vertex_buffers(cmd, 0, &[vertex_buffer.buffer], &[0])
    };
    // Draw all the vertices as one instance. This is the entire point of the program.
    unsafe { ctx.device.cmd_draw(cmd, vertex_count, 1, 0, 0) };
    // Ending the pass performs the transition to TRANSFER_SRC_OPTIMAL declared as the
    // attachment's `final_layout`, so the copy below needs no barrier of its own.
    unsafe { ctx.device.cmd_end_render_pass(cmd) };

    // Copy the finished image into the mappable buffer. `buffer_row_length` and
    // `buffer_image_height` are set **explicitly** rather than left 0: 0 means "infer from
    // `image_extent`", which is the same thing here, but stating them makes the buffer's layout a
    // property of this program that `read_pixels` can rely on rather than something inferred by
    // the driver. That is what makes the stride below ours to know — see the module docs.
    let copy = vk::BufferImageCopy::default()
        .buffer_offset(0)
        .buffer_row_length(width)
        .buffer_image_height(height)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        })
        .image_extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        });
    unsafe {
        ctx.device.cmd_copy_image_to_buffer(
            cmd,
            target.image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            readback.buffer,
            &[copy],
        )
    };
    unsafe { ctx.device.end_command_buffer(cmd) }?;

    // --- Submit and wait. See `render_triangle`'s docs for why the wait is not optional. ---
    let fence = unsafe {
        ctx.device
            .create_fence(&vk::FenceCreateInfo::default(), None)
    }?;
    let cmds = [cmd];
    let submit = vk::SubmitInfo::default().command_buffers(&cmds);
    let submitted = unsafe { ctx.device.queue_submit(ctx.queue, &[submit], fence) };
    // Wait for the GPU only if the submission was actually accepted; waiting on a fence nothing was
    // ever submitted against would block for the full timeout to no purpose. `wait_for_fences`
    // returns `Err(VK_TIMEOUT)` rather than blocking forever, which is exactly the diagnosable
    // failure the timeout exists to produce.
    let waited = submitted
        .and_then(|()| unsafe { ctx.device.wait_for_fences(&[fence], true, FENCE_TIMEOUT_NS) });
    // Destroy the fence only once the wait has succeeded — the same rule, and for the same reason,
    // as the object teardown in `render_triangle_inner`: a fence the queue may still signal must
    // not be destroyed, and on the error path this program exits immediately anyway.
    // SAFETY: a successful wait proves the submission retired, so nothing will signal this fence.
    if waited.is_ok() {
        unsafe { ctx.device.destroy_fence(fence, None) };
    }
    waited?;

    // SAFETY: the fence wait above proves the copy has completed, so the buffer now holds the
    // finished pixels and nothing on the GPU is still writing them.
    unsafe { read_pixels(ctx, readback, width, height, row_stride, readback_size) }
}

/// Map the readback buffer and repack its rows into a tightly-packed RGBA8 `Vec`.
///
/// # Honouring the row stride
/// The bytes in the buffer are laid out as `height` rows of `row_stride` bytes each, and the first
/// `width * 4` bytes of each row are the pixels. This function copies **row by row, advancing by
/// `row_stride`**, rather than copying the whole thing in one block — even though, for this
/// program, `row_stride` is exactly `width * 4` and the two are identical.
///
/// That is deliberate. The stride is the *copy's* property, chosen where `vkCmdCopyImageToBuffer`
/// is recorded, and a reader that assumes it equals the packed row length is making an assumption
/// it has not checked. Here the assumption happens to hold; the moment someone aligns
/// `buffer_row_length` up to a hardware requirement, or copies a sub-region, it stops holding and
/// a block copy starts producing a sheared image with no error anywhere. Writing the loop that
/// respects the stride costs three lines and removes the failure mode permanently.
///
/// # Errors
/// Returns an error if `vkMapMemory` fails.
///
/// # Safety
/// The GPU must have finished writing `readback` (the caller guarantees this with a fence wait),
/// and `row_stride`/`readback_size` must describe the layout the copy actually produced.
unsafe fn read_pixels(
    ctx: &VulkanContext,
    readback: &HostBuffer,
    width: u32,
    height: u32,
    row_stride: u64,
    readback_size: u64,
) -> anyhow::Result<Vec<u8>> {
    // SAFETY: the caller guarantees the GPU is done; the memory is HOST_VISIBLE by construction.
    let mapped = unsafe {
        ctx.device.map_memory(
            readback.memory,
            0,
            readback_size,
            vk::MemoryMapFlags::empty(),
        )
    }? as *const u8;

    // The tightly-packed output: exactly what a PNG encoder wants, with no padding between rows.
    let packed_row = width as usize * BYTES_PER_PIXEL as usize;
    let mut pixels = vec![0u8; packed_row * height as usize];
    for row in 0..height as usize {
        // SAFETY: `mapped` covers `readback_size` == `row_stride * height` bytes, so reading
        // `packed_row` (<= `row_stride`) bytes at `row * row_stride` stays inside it for every
        // row; the destination `Vec` was sized to hold exactly `height` packed rows; and the two
        // regions cannot overlap (one is a GPU mapping, the other a fresh heap allocation).
        unsafe {
            std::ptr::copy_nonoverlapping(
                mapped.add(row * row_stride as usize),
                pixels.as_mut_ptr().add(row * packed_row),
                packed_row,
            )
        };
    }

    // The memory is HOST_COHERENT, so no invalidate was needed before reading it, and unmapping
    // here is all the cleanup the mapping itself requires.
    // SAFETY: unmapping memory that is currently mapped, with nothing still referring to `mapped`.
    unsafe { ctx.device.unmap_memory(readback.memory) };

    Ok(pixels)
}
