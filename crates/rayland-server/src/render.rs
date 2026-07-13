//! Off-screen Vulkan rendering of a single triangle.
//!
//! "Head-less" means there is no window, no swapchain, and no surface — we render into an
//! ordinary GPU image and copy the result back to CPU memory. This is exactly what the S
//! side must do when the real display is handled separately (by the compositor, in later
//! sub-projects). In SP0 the caller just gets the pixels back.
//!
//! ## Why copy-to-buffer instead of mapping the image directly
//! A GPU image in `OPTIMAL` tiling has a driver-private memory layout you cannot read
//! meaningfully on the CPU. A `LINEAR` image *can* be mapped, but each row is padded to a
//! driver-chosen `rowPitch`, and assuming `width * 4` there produces a subtly sheared
//! image — a classic first-timer trap. We sidestep both by rendering to an `OPTIMAL`
//! image and then using `vkCmdCopyImageToBuffer`, which packs the pixels tightly
//! (`bufferRowLength = 0`) into a host-visible buffer we can read directly.

// The Vulkan API surface and its core handle/struct types.
use ash::vk;
// The vertex type as it arrives over the wire.
use rayland_wire::Vertex;

/// Everything needed to render one frame.
pub struct FrameRequest {
    /// Target width in pixels.
    pub width: u32,
    /// Target height in pixels.
    pub height: u32,
    /// Background colour (RGBA, `0.0..=1.0`) the image is cleared to.
    pub clear_color: [f32; 4],
    /// The triangle's vertices, in draw order.
    pub vertices: Vec<Vertex>,
}

/// The rendered result: a tightly-packed RGBA8 image.
pub struct RenderedFrame {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes of RGBA8, row-major, no padding.
    pub pixels: Vec<u8>,
}

// The compiled shaders, embedded so the build needs no shader compiler (see Task 3).
// SPIR-V is a stream of 32-bit words, so we align the bytes to 4 for `read_spv`.
const VERT_SPV: &[u8] = include_bytes!("../../../shaders/triangle.vert.spv");
const FRAG_SPV: &[u8] = include_bytes!("../../../shaders/triangle.frag.spv");

/// Render `request`'s triangle off-screen and return the pixels.
///
/// Creates a throwaway Vulkan instance, device, image, pipeline, and vertex buffer;
/// records and submits one draw; copies the image into a host-visible buffer packed
/// tightly; and returns the RGBA8 bytes. All Vulkan objects are created and destroyed
/// within this call — SP0 renders one frame per process, so there is no state to keep.
///
/// # Errors
/// Returns an error if no Vulkan device is available or any Vulkan call fails.
pub fn render_triangle(request: &FrameRequest) -> anyhow::Result<RenderedFrame> {
    // SAFETY: every ash call below is an FFI call into the Vulkan driver. They are unsafe
    // because Vulkan trusts us to pass valid handles and sizes; we uphold that by
    // constructing each argument immediately before use and destroying handles in reverse
    // order at the end. The whole body is one unsafe block for readability.
    unsafe { render_triangle_inner(request) }
}

/// The `unsafe` body of [`render_triangle`], separated so the public function stays safe
/// to call and the safety reasoning lives in one place.
unsafe fn render_triangle_inner(request: &FrameRequest) -> anyhow::Result<RenderedFrame> {
    // Load the Vulkan loader from the system (libvulkan.so / lavapipe in CI).
    let entry = unsafe { ash::Entry::load() }?;

    // Describe our application; Vulkan uses this only for driver diagnostics.
    let app_info = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 0, 0)); // request Vulkan 1.0 — all we need

    // Create the instance with no extensions (off-screen needs none).
    let instance_info = vk::InstanceCreateInfo::default().application_info(&app_info);
    let instance = unsafe { entry.create_instance(&instance_info, None) }?;

    // Pick the first physical device that has a graphics-capable queue family.
    let physical_devices = unsafe { instance.enumerate_physical_devices() }?;
    let (physical_device, queue_family_index) = physical_devices
        .iter()
        .find_map(|&pd| {
            // Inspect each queue family for graphics support.
            unsafe { instance.get_physical_device_queue_family_properties(pd) }
                .iter()
                .enumerate()
                .find(|(_, props)| props.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .map(|(index, _)| (pd, index as u32))
        })
        .ok_or_else(|| anyhow::anyhow!("no Vulkan device with a graphics queue was found"))?;

    // Create a logical device with one graphics queue.
    let queue_priorities = [1.0f32]; // single queue, priority is irrelevant but required
    let queue_info = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(queue_family_index)
        .queue_priorities(&queue_priorities);
    let queue_infos = [queue_info];
    let device_info = vk::DeviceCreateInfo::default().queue_create_infos(&queue_infos);
    let device = unsafe { instance.create_device(physical_device, &device_info, None) }?;
    // Retrieve the queue we will submit work to.
    let queue = unsafe { device.get_device_queue(queue_family_index, 0) };

    // Query memory properties once; used to choose memory types for the image and buffers.
    let mem_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };

    // --- Off-screen colour image (OPTIMAL tiling, used as attachment + transfer source) ---
    let format = vk::Format::R8G8B8A8_UNORM; // 8 bits per channel, matches our RGBA8 output
    let extent = vk::Extent3D {
        width: request.width,
        height: request.height,
        depth: 1,
    };
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(extent)
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { device.create_image(&image_info, None) }?;
    // Allocate and bind DEVICE_LOCAL memory for the image.
    let image_mem_req = unsafe { device.get_image_memory_requirements(image) };
    let image_mem = unsafe {
        allocate(
            &device,
            &mem_props,
            image_mem_req,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
    }?;
    unsafe { device.bind_image_memory(image, image_mem, 0) }?;

    // An image view over the whole image, needed by the framebuffer.
    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        });
    let image_view = unsafe { device.create_image_view(&view_info, None) }?;

    // --- Render pass: clear the colour attachment, store it, leave it as a transfer src ---
    let color_attachment = vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR) // clear to clear_color at the start
        .store_op(vk::AttachmentStoreOp::STORE) // keep the drawn pixels
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL); // ready for the readback copy
    let color_ref = vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let color_refs = [color_ref];
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_refs);
    let attachments = [color_attachment];
    let subpasses = [subpass];
    let render_pass_info = vk::RenderPassCreateInfo::default()
        .attachments(&attachments)
        .subpasses(&subpasses);
    let render_pass = unsafe { device.create_render_pass(&render_pass_info, None) }?;

    // Framebuffer binding the image view to the render pass.
    let fb_attachments = [image_view];
    let framebuffer_info = vk::FramebufferCreateInfo::default()
        .render_pass(render_pass)
        .attachments(&fb_attachments)
        .width(request.width)
        .height(request.height)
        .layers(1);
    let framebuffer = unsafe { device.create_framebuffer(&framebuffer_info, None) }?;

    // --- Shader modules from the embedded SPIR-V ---
    let vert_module = unsafe { create_shader_module(&device, VERT_SPV) }?;
    let frag_module = unsafe { create_shader_module(&device, FRAG_SPV) }?;
    // The entry point name every shader uses, as a NUL-terminated C string. Bound to a
    // local so the `CString`'s buffer outlives the `PipelineShaderStageCreateInfo`
    // borrows below (ash 0.38's `.name()` takes a `&CStr` and stores that reference).
    let entry_name = std::ffi::CString::new("main").expect("literal has no NUL bytes");
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert_module)
            .name(&entry_name),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag_module)
            .name(&entry_name),
    ];

    // --- Vertex input: one binding (our Vertex), two attributes (position, colour) ---
    let binding = vk::VertexInputBindingDescription::default()
        .binding(0)
        .stride(std::mem::size_of::<Vertex>() as u32) // 5 f32s = 20 bytes
        .input_rate(vk::VertexInputRate::VERTEX);
    let attributes = [
        vk::VertexInputAttributeDescription::default()
            .location(0)
            .binding(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(0), // position at byte 0
        vk::VertexInputAttributeDescription::default()
            .location(1)
            .binding(0)
            .format(vk::Format::R32G32B32_SFLOAT)
            .offset(8), // colour at byte 8
    ];
    let bindings = [binding];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attributes);

    // Draw the vertices as a list of triangles.
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);

    // A viewport and scissor covering the whole image.
    let viewport = vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: request.width as f32,
        height: request.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    };
    let scissor = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D {
            width: request.width,
            height: request.height,
        },
    };
    let viewports = [viewport];
    let scissors = [scissor];
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewports(&viewports)
        .scissors(&scissors);

    // Standard rasterisation: fill triangles, no culling (so winding order can't hide it).
    let rasterizer = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    // No multisampling.
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    // Write all colour channels, no blending.
    let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)
        .blend_enable(false);
    let blend_attachments = [blend_attachment];
    let color_blend =
        vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);

    // An empty pipeline layout (no descriptors or push constants in SP0).
    let layout =
        unsafe { device.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default(), None) }?;

    // Assemble the graphics pipeline.
    let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterizer)
        .multisample_state(&multisample)
        .color_blend_state(&color_blend)
        .layout(layout)
        .render_pass(render_pass)
        .subpass(0);
    // `create_graphics_pipelines` returns `Ok(pipelines)` or `Err((partial_pipelines,
    // vk::Result))` on ash 0.38, so we discard any partially-created pipelines on error
    // and surface just the `vk::Result` (which converts to `anyhow::Error` via `?`).
    let pipeline = unsafe {
        device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
    }
    .map_err(|(_, e)| e)?[0];

    // --- Vertex buffer (host-visible so we can copy the vertices straight in) ---
    let vertex_bytes = request.vertices.len() * std::mem::size_of::<Vertex>();
    let vbuf_info = vk::BufferCreateInfo::default()
        .size(vertex_bytes as u64)
        .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let vertex_buffer = unsafe { device.create_buffer(&vbuf_info, None) }?;
    let vbuf_req = unsafe { device.get_buffer_memory_requirements(vertex_buffer) };
    let vbuf_mem = unsafe {
        allocate(
            &device,
            &mem_props,
            vbuf_req,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
    }?;
    unsafe { device.bind_buffer_memory(vertex_buffer, vbuf_mem, 0) }?;
    // Map the buffer and copy the vertex data in.
    let ptr = unsafe {
        device.map_memory(
            vbuf_mem,
            0,
            vertex_bytes as u64,
            vk::MemoryMapFlags::empty(),
        )
    }?;
    unsafe {
        std::ptr::copy_nonoverlapping(
            request.vertices.as_ptr() as *const u8,
            ptr as *mut u8,
            vertex_bytes,
        )
    };
    unsafe { device.unmap_memory(vbuf_mem) };

    // --- Readback buffer (host-visible, holds the tightly-packed image after the copy) ---
    let readback_size = (request.width * request.height * 4) as u64;
    let rbuf_info = vk::BufferCreateInfo::default()
        .size(readback_size)
        .usage(vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let readback_buffer = unsafe { device.create_buffer(&rbuf_info, None) }?;
    let rbuf_req = unsafe { device.get_buffer_memory_requirements(readback_buffer) };
    let rbuf_mem = unsafe {
        allocate(
            &device,
            &mem_props,
            rbuf_req,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
    }?;
    unsafe { device.bind_buffer_memory(readback_buffer, rbuf_mem, 0) }?;

    // --- Command buffer: draw, then copy image → readback buffer ---
    let pool = unsafe {
        device.create_command_pool(
            &vk::CommandPoolCreateInfo::default().queue_family_index(queue_family_index),
            None,
        )
    }?;
    let cmd = unsafe {
        device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )
    }?[0];
    unsafe {
        device.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
    }?;

    // Begin the render pass, clearing to the requested colour.
    let clear = vk::ClearValue {
        color: vk::ClearColorValue {
            float32: request.clear_color,
        },
    };
    let clears = [clear];
    let rp_begin = vk::RenderPassBeginInfo::default()
        .render_pass(render_pass)
        .framebuffer(framebuffer)
        .render_area(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: request.width,
                height: request.height,
            },
        })
        .clear_values(&clears);
    unsafe { device.cmd_begin_render_pass(cmd, &rp_begin, vk::SubpassContents::INLINE) };
    unsafe { device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline) };
    unsafe { device.cmd_bind_vertex_buffers(cmd, 0, &[vertex_buffer], &[0]) };
    // Draw the triangle: vertex_count vertices, 1 instance.
    unsafe { device.cmd_draw(cmd, request.vertices.len() as u32, 1, 0, 0) };
    unsafe { device.cmd_end_render_pass(cmd) };

    // Copy the rendered image (now TRANSFER_SRC_OPTIMAL) into the readback buffer,
    // tightly packed: buffer_row_length = 0 means "use the image width", no padding.
    let copy = vk::BufferImageCopy::default()
        .buffer_offset(0)
        .buffer_row_length(0)
        .buffer_image_height(0)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        })
        .image_extent(extent);
    unsafe {
        device.cmd_copy_image_to_buffer(
            cmd,
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            readback_buffer,
            &[copy],
        )
    };
    unsafe { device.end_command_buffer(cmd) }?;

    // Submit and wait for completion using a fence.
    let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?;
    let cmds = [cmd];
    let submit = vk::SubmitInfo::default().command_buffers(&cmds);
    unsafe { device.queue_submit(queue, &[submit], fence) }?;
    // Wait up to ~10 seconds for the GPU to finish.
    unsafe { device.wait_for_fences(&[fence], true, 10_000_000_000) }?;

    // --- Read the pixels out of the readback buffer ---
    let mapped =
        unsafe { device.map_memory(rbuf_mem, 0, readback_size, vk::MemoryMapFlags::empty()) }?;
    // Copy the bytes into an owned Vec so we can free the GPU memory before returning.
    let mut pixels = vec![0u8; readback_size as usize];
    unsafe {
        std::ptr::copy_nonoverlapping(
            mapped as *const u8,
            pixels.as_mut_ptr(),
            readback_size as usize,
        )
    };
    unsafe { device.unmap_memory(rbuf_mem) };

    // --- Tear down (reverse creation order). SP0 renders once per process, so a leak on
    // an earlier `?` is harmless; explicit destruction here keeps the happy path clean. ---
    unsafe {
        device.destroy_fence(fence, None);
        device.destroy_command_pool(pool, None);
        device.destroy_buffer(readback_buffer, None);
        device.free_memory(rbuf_mem, None);
        device.destroy_buffer(vertex_buffer, None);
        device.free_memory(vbuf_mem, None);
        device.destroy_pipeline(pipeline, None);
        device.destroy_pipeline_layout(layout, None);
        device.destroy_shader_module(vert_module, None);
        device.destroy_shader_module(frag_module, None);
        device.destroy_framebuffer(framebuffer, None);
        device.destroy_render_pass(render_pass, None);
        device.destroy_image_view(image_view, None);
        device.destroy_image(image, None);
        device.free_memory(image_mem, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
    }

    // Hand back the pixels.
    Ok(RenderedFrame {
        width: request.width,
        height: request.height,
        pixels,
    })
}

/// Choose a memory type index satisfying `requirements` and `wanted` property flags, then
/// allocate that much device memory.
///
/// Vulkan exposes several memory types with different properties (device-local,
/// host-visible, …); a buffer/image's `memory_type_bits` says which are legal for it, and
/// we pick the first legal type that also has every flag in `wanted`.
///
/// # Errors
/// Returns an error if no memory type matches or the allocation fails.
unsafe fn allocate(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    requirements: vk::MemoryRequirements,
    wanted: vk::MemoryPropertyFlags,
) -> anyhow::Result<vk::DeviceMemory> {
    // Scan the memory types for one allowed by the resource and carrying all wanted flags.
    let type_index = (0..mem_props.memory_type_count)
        .find(|&i| {
            // Bit i set in memory_type_bits means "type i is allowed for this resource".
            let allowed = requirements.memory_type_bits & (1 << i) != 0;
            // The type must also expose every property flag we asked for.
            let has_flags = mem_props.memory_types[i as usize]
                .property_flags
                .contains(wanted);
            allowed && has_flags
        })
        .ok_or_else(|| anyhow::anyhow!("no suitable Vulkan memory type for {wanted:?}"))?;
    // Allocate exactly the required size of the chosen type.
    let info = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(type_index);
    Ok(unsafe { device.allocate_memory(&info, None) }?)
}

/// Create a Vulkan shader module from SPIR-V bytes.
///
/// SPIR-V is a sequence of 32-bit words; `ash::util::read_spv` converts the byte slice
/// into the `u32` slice Vulkan expects and validates the length and magic number.
///
/// # Errors
/// Returns an error if the bytes are not valid SPIR-V or module creation fails.
unsafe fn create_shader_module(
    device: &ash::Device,
    spv: &[u8],
) -> anyhow::Result<vk::ShaderModule> {
    // Wrap the bytes in a Cursor so read_spv can consume them.
    let mut cursor = std::io::Cursor::new(spv);
    // Decode the byte stream into 32-bit SPIR-V words.
    let words = ash::util::read_spv(&mut cursor)?;
    // Build the module from the words.
    let info = vk::ShaderModuleCreateInfo::default().code(&words);
    Ok(unsafe { device.create_shader_module(&info, None) }?)
}
