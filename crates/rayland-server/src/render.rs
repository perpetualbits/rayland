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
//!
//! ## From one-shot function to persistent `Renderer` (SP3)
//! SP0/SP1/SP2 only ever needed to render exactly one frame per process, so the original code
//! here was a single function that created every Vulkan object, rendered, read the pixels back,
//! and destroyed everything before returning — there was no state left to keep. SP3 changes
//! that: a later addition (`Renderer::render_to_dmabuf`, Task 3 of the SP3 plan) exports the
//! rendered image as a Linux dmabuf — a file descriptor that refers to *live GPU memory*. That
//! fd is worthless (and importing it elsewhere is undefined behaviour) once the Vulkan device
//! backing the memory is destroyed, so the device — and everything it depends on — must stay
//! alive for as long as a caller might still be using the exported buffer, i.e. across multiple
//! calls, not just for the duration of one render. [`Renderer`] is that persistent object: its
//! long-lived Vulkan state is built once in [`Renderer::new`] and torn down once, in `Drop`,
//! while [`Renderer::render_to_frame`] does only the work that must happen fresh every frame
//! (vertex upload, command recording, submission, readback). [`render_triangle`] remains as a
//! thin convenience wrapper — `Renderer::new()?.render_to_frame(request)` — so SP0's pixel test,
//! the `--png` dump, and any other single-shot caller are unaffected by this restructuring.

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

/// The colour image + its view + the framebuffer binding them to [`Renderer::render_pass`] —
/// the three Vulkan objects that need a *concrete* width/height to be created and therefore
/// cannot be built inside [`Renderer::new`] (which deliberately takes no size argument; the
/// caller only supplies a size later, per [`FrameRequest`], to [`Renderer::render_to_frame`]).
///
/// Bundled into one struct (rather than three loose `Option` fields on [`Renderer`]) so the
/// "do we already have the right size, or do we need to (re)build?" check and the "destroy the
/// old one" cleanup each touch a single field instead of three that could get out of sync.
struct SizedTarget {
    /// The width this target was built for.
    width: u32,
    /// The height this target was built for. A [`Renderer::render_to_frame`] call whose
    /// `request.width`/`request.height` do not match `width`/`height` triggers a rebuild.
    height: u32,
    /// The `OPTIMAL`-tiling colour attachment image the triangle is drawn into.
    image: vk::Image,
    /// The device memory backing `image`.
    image_memory: vk::DeviceMemory,
    /// A view over the whole of `image`, required by `framebuffer`.
    image_view: vk::ImageView,
    /// Binds `image_view` to the render pass at exactly `width` × `height`.
    framebuffer: vk::Framebuffer,
}

/// A persistent, reusable off-screen Vulkan renderer.
///
/// Owns every Vulkan object whose lifetime must survive past a single [`render_to_frame`]
/// call: the loaded Vulkan library, the instance, the logical device, the graphics queue, the
/// render pass, the graphics pipeline, and the command pool. These are all *size-independent* —
/// none of them bakes in a width or height — so every one of them is created exactly once, in
/// [`Renderer::new`], and reused by every frame for as long as the `Renderer` lives.
///
/// ## What is deliberately NOT built in `new()`
/// The colour image, its view, and the framebuffer that binds them to the render pass (bundled
/// as [`SizedTarget`]) genuinely need a concrete extent at creation time, but `new()` takes no
/// size argument — callers only learn the target size from the first [`FrameRequest`], which
/// arrives at [`render_to_frame`], not at construction. These three are therefore built lazily,
/// the first time `render_to_frame` is called, and cached in the `sized` field; a later call
/// with a *different* size destroys the old target and builds a new one (see
/// `ensure_sized_target`). SP3's own use of `Renderer` only ever renders one size per process,
/// so in practice this rebuild path is never exercised outside tests, but implementing it
/// properly costs little and avoids a landmine for whoever changes that later.
///
/// The graphics pipeline sidesteps the same problem a different way: the original one-shot code
/// baked a fixed viewport/scissor (sized to that call's request) directly into the pipeline at
/// creation time. That is no longer possible if the pipeline is to be built once, before any
/// size is known, so the pipeline now declares viewport and scissor as **dynamic state**
/// (`VK_DYNAMIC_STATE_VIEWPORT` / `VK_DYNAMIC_STATE_SCISSOR`) and [`render_to_frame`] sets the
/// real viewport/scissor with `vkCmdSetViewport`/`vkCmdSetScissor` on every call, using that
/// call's `request.width`/`request.height`. The rendered pixels are identical either way — this
/// only changes *when* Vulkan is told the viewport, not what it ends up being.
///
/// Per-frame transient objects (vertex buffer, readback buffer, command buffer, fence) are
/// still created and destroyed within every [`render_to_frame`] call, exactly as the original
/// one-shot code did — there is no benefit to keeping those alive between frames.
pub struct Renderer {
    /// The loaded Vulkan library (`libvulkan.so` / lavapipe's ICD). Not read again after
    /// `new()` returns, but kept alive for as long as `instance`/`device` are: those handles
    /// are backed by function pointers this loader resolved, and unloading the library while
    /// they are still in use would be undefined behaviour. The leading underscore tells Rust
    /// (and readers) that this field exists purely to control drop timing, not to be read.
    _entry: ash::Entry,
    /// The Vulkan instance. Outlives every other Vulkan handle below except `_entry`; destroyed
    /// last (after `device`) in [`Drop`].
    instance: ash::Instance,
    /// The physical GPU chosen at construction time. This is a handle owned by the driver, not
    /// a resource we allocate — nothing to free — kept so future code (Task 3) can re-query its
    /// properties (e.g. supported DRM format modifiers) without re-enumerating devices.
    /// `#[allow(dead_code)]`: nothing in *this* task reads it back — `new_inner` uses a local
    /// of the same value for device/queue creation — but the SP3 plan explicitly calls for
    /// storing it now so Task 3 does not have to re-enumerate physical devices from scratch.
    #[allow(dead_code)]
    physical_device: vk::PhysicalDevice,
    /// The logical device: every other Vulkan object in this struct is created through this
    /// handle, and it must be destroyed after all of them.
    device: ash::Device,
    /// The single graphics-capable queue all work (per-frame draws, and later dmabuf export
    /// blits) is submitted to.
    queue: vk::Queue,
    /// The physical device's memory type table, queried once and reused by every allocation
    /// (the `allocate` helper) for the renderer's whole lifetime — the driver never changes
    /// this at runtime, so re-querying it per frame would be pure waste.
    mem_props: vk::PhysicalDeviceMemoryProperties,
    /// The render pass: clears the colour attachment, draws into it, and leaves it as a
    /// transfer source. Size-independent (it describes attachment *usage* — formats, load/store
    /// ops, the subpass dependency — not a concrete *extent*), so unlike the image/framebuffer
    /// it can be, and is, built once here and reused by every [`SizedTarget`]'s framebuffer
    /// regardless of that target's size.
    render_pass: vk::RenderPass,
    /// The (empty — SP0/SP1/SP2/SP3 use no descriptors or push constants) pipeline layout.
    pipeline_layout: vk::PipelineLayout,
    /// The graphics pipeline: vertex input layout, rasteriser state, and the two shader stages.
    /// Declares viewport/scissor as dynamic state (see the struct docs) precisely so that it,
    /// too, needs no size and can be built once, here, rather than per frame.
    pipeline: vk::Pipeline,
    /// The command pool per-frame command buffers are allocated from (in `render_to_frame`) and
    /// freed back to at the end of each call. Persistent so allocation is a cheap pool op
    /// instead of a full pool creation on every frame.
    command_pool: vk::CommandPool,
    /// The size-dependent colour image/view/framebuffer, or `None` until the first
    /// `render_to_frame` call establishes a size. See the struct docs and [`SizedTarget`].
    sized: Option<SizedTarget>,
    /// Whether `physical_device` exposes every extension
    /// [`crate::dmabuf::required_device_extensions`] needs, and those extensions (plus their
    /// transitive dependency `VK_KHR_image_format_list`) were successfully enabled on `device`
    /// in `new()`. `pub` so Task 3's `render_to_dmabuf` (and its caller, deciding between the
    /// dmabuf and `wl_shm` presentation paths) can branch on it without re-probing the device.
    pub supports_dmabuf: bool,
}

impl Renderer {
    /// Create a persistent renderer: load Vulkan, pick a GPU, create a logical device, and
    /// build every size-independent Vulkan object (render pass, pipeline, command pool) up
    /// front. The size-dependent colour image/view/framebuffer are deliberately NOT created
    /// here — see the [`Renderer`] struct docs — they come into existence on the first
    /// [`render_to_frame`] call.
    ///
    /// When the chosen physical device supports the dmabuf-export extensions
    /// ([`crate::dmabuf::required_device_extensions`]), this enables them (plus their
    /// transitive dependency `VK_KHR_image_format_list`) on the device and sets
    /// [`Renderer::supports_dmabuf`] to `true`, so a later `render_to_dmabuf` (Task 3) can use
    /// them without having to recreate the device. Enabling extensions the render path itself
    /// never calls into is harmless — it does not change how `render_to_frame` behaves.
    ///
    /// # Errors
    /// Returns an error if no Vulkan device is available or any Vulkan call fails.
    pub fn new() -> anyhow::Result<Renderer> {
        // SAFETY: every ash call below is an FFI call into the Vulkan driver. They are unsafe
        // because Vulkan trusts us to pass valid handles and sizes; we uphold that by
        // constructing each argument immediately before use. The whole body is one unsafe
        // block for readability, matching the original one-shot function's convention.
        unsafe { Renderer::new_inner() }
    }

    /// The `unsafe` body of [`Renderer::new`], separated so the public function stays safe to
    /// call and the safety reasoning lives in one place (mirrors `render_triangle_inner`'s role
    /// in the original one-shot code).
    unsafe fn new_inner() -> anyhow::Result<Renderer> {
        // Load the Vulkan loader from the system (libvulkan.so / lavapipe in CI).
        let entry = unsafe { ash::Entry::load() }?;

        // Describe our application; Vulkan uses this only for driver diagnostics. Vulkan 1.1
        // (not 1.0, as the original one-shot code requested) is needed so that
        // `VK_KHR_external_memory`/`VK_KHR_external_memory_capabilities` — transitive
        // dependencies of the dmabuf-export device extensions enabled below — are core and so
        // do not themselves need to be separately requested; SP3 Task 1's spike proved this
        // exact recipe (see `dmabuf.rs`'s test harness, which targets 1.1 for the same reason).
        // Requesting 1.1 costs nothing on a device that does not support dmabuf export: every
        // driver in this project's target set (real GPUs, and Mesa lavapipe) supports 1.1.
        let app_info = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 1, 0));

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

        // Probe dmabuf-export support BEFORE creating the device: which extensions we enable
        // below depends on the answer, and device extensions cannot be changed after creation.
        let supports_dmabuf =
            crate::dmabuf::device_supports_dmabuf_export(&instance, physical_device);

        // Create a logical device with one graphics queue.
        let queue_priorities = [1.0f32]; // single queue, priority is irrelevant but required
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&queue_priorities);
        let queue_infos = [queue_info];
        // Build the device extension list: empty unless the device supports dmabuf export, in
        // which case enable the three functional extensions PLUS `VK_KHR_image_format_list` —
        // a transitive dependency of `VK_EXT_image_drm_format_modifier` per the Vulkan spec
        // (VUID-vkCreateDevice-01387: every dependency of an enabled extension must itself be
        // enabled unless core for the device's API version; `image_format_list` only became
        // core in Vulkan 1.2, and this device targets 1.1). This mirrors `dmabuf.rs`'s test
        // harness exactly (see its `minimal_dmabuf_context`, which documents the same VUID).
        let mut ext_names: Vec<*const std::os::raw::c_char> = Vec::new();
        if supports_dmabuf {
            ext_names.extend(
                crate::dmabuf::required_device_extensions()
                    .iter()
                    .map(|c| c.as_ptr()),
            );
            ext_names.push(ash::khr::image_format_list::NAME.as_ptr());
        }
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&ext_names);
        let device = unsafe { instance.create_device(physical_device, &device_info, None) }?;
        // Retrieve the queue we will submit work to.
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };

        // Query memory properties once; used to choose memory types for the image and buffers.
        let mem_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };

        // --- Render pass: clear the colour attachment, store it, leave it as a transfer src ---
        let format = vk::Format::R8G8B8A8_UNORM; // 8 bits per channel, matches our RGBA8 output
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
        // Vulkan gives NO ordering or visibility guarantee between a render pass's
        // attachment writes and whatever comes after it unless we say so explicitly. Our
        // `vkCmdCopyImageToBuffer` readback (issued right after `cmd_end_render_pass`, see
        // `render_to_frame`) runs at the TRANSFER stage and reads the very same image the
        // fragment shader just wrote via COLOR_ATTACHMENT_OUTPUT. Without a dependency chaining
        // those two, the driver is free to start the transfer read before the colour write is
        // even visible — some drivers happen to serialize enough work that this is never
        // observed (e.g. Intel anv in casual testing), but that is luck, not a spec
        // guarantee, and it is a likely, real failure on other drivers (e.g. lavapipe). This
        // dependency makes the colour-attachment writes from subpass 0 visible to the
        // transfer-stage read that follows the render pass, so the copy is guaranteed to see
        // the finished pixels.
        let dependency = vk::SubpassDependency::default()
            .src_subpass(0)
            .dst_subpass(vk::SUBPASS_EXTERNAL)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dependency_flags(vk::DependencyFlags::BY_REGION); // same image region on both sides
        let dependencies = [dependency];
        let render_pass_info = vk::RenderPassCreateInfo::default()
            .attachments(&attachments)
            .subpasses(&subpasses)
            .dependencies(&dependencies);
        let render_pass = unsafe { device.create_render_pass(&render_pass_info, None) }?;

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
                // `Vertex` is `#[repr(Rust)]`, so the compiler is free to reorder or pad its
                // fields however it likes — a hardcoded byte offset here would silently
                // assume a layout that isn't guaranteed. `offset_of!` asks the compiler for
                // the real offset of `position` in whatever layout it actually chose.
                .offset(std::mem::offset_of!(Vertex, position) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                // Same reasoning as `position` above: ask for `color`'s real offset rather
                // than assuming it immediately follows `position` with no padding.
                .offset(std::mem::offset_of!(Vertex, color) as u32),
        ];
        let bindings = [binding];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&bindings)
            .vertex_attribute_descriptions(&attributes);

        // Draw the vertices as a list of triangles.
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);

        // A viewport/scissor *state* declaring one of each — but, unlike the original one-shot
        // code, NOT baking in a concrete size. `viewport_count`/`scissor_count` are all a
        // pipeline needs to know at creation time when the actual rectangles are supplied later
        // as dynamic state (see `dynamic_state` below and `render_to_frame`'s
        // `cmd_set_viewport`/`cmd_set_scissor`). This is what lets the pipeline be built once,
        // here, before any [`FrameRequest`] — and hence any size — exists.
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        // Declare viewport and scissor as dynamic: their actual values are set per draw via
        // `vkCmdSetViewport`/`vkCmdSetScissor` in `render_to_frame`, using that call's
        // `request.width`/`request.height`, instead of being fixed at pipeline-creation time.
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

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

        // An empty pipeline layout (no descriptors or push constants in SP0/SP1/SP2/SP3).
        let pipeline_layout = unsafe {
            device.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default(), None)
        }?;

        // Assemble the graphics pipeline.
        let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .dynamic_state(&dynamic_state)
            .rasterization_state(&rasterizer)
            .multisample_state(&multisample)
            .color_blend_state(&color_blend)
            .layout(pipeline_layout)
            .render_pass(render_pass)
            .subpass(0);
        // `create_graphics_pipelines` returns `Ok(pipelines)` or `Err((partial_pipelines,
        // vk::Result))` on ash 0.38, so we discard any partially-created pipelines on error
        // and surface just the `vk::Result` (which converts to `anyhow::Error` via `?`).
        let pipeline = unsafe {
            device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
        }
        .map_err(|(_, e)| e)?[0];

        // The shader modules are only needed while building the pipeline above; unlike the
        // long-lived objects in this struct, Vulkan does not keep using them after pipeline
        // creation, so — as the SP3 plan calls out — they can be (and here are) destroyed
        // immediately, rather than being kept alive until `Renderer::drop`.
        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }

        // --- Command pool: source of every per-frame command buffer `render_to_frame` needs ---
        let command_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(queue_family_index),
                None,
            )
        }?;

        Ok(Renderer {
            _entry: entry,
            instance,
            physical_device,
            device,
            queue,
            mem_props,
            render_pass,
            pipeline_layout,
            pipeline,
            command_pool,
            // No frame has been rendered yet, so there is no sized target to hold.
            sized: None,
            supports_dmabuf,
        })
    }

    /// Ensure `self.sized` holds a colour image + view + framebuffer built for exactly
    /// `width` × `height`, building them on first use or rebuilding them if a previous call
    /// built them for a different size. See the [`Renderer`] struct docs for why these three
    /// objects cannot be created in `new()`. Returns the image and framebuffer handles the
    /// caller needs (rather than making the caller re-borrow `self.sized`), so
    /// [`render_to_frame_inner`] never has to assert that this method actually populated it.
    ///
    /// # Errors
    /// Returns an error if image, memory, view, or framebuffer creation fails.
    unsafe fn ensure_sized_target(
        &mut self,
        width: u32,
        height: u32,
    ) -> anyhow::Result<(vk::Image, vk::Framebuffer)> {
        // Already built at the requested size: return the existing handles. This is the common
        // case — SP3 renders one size per process, so after the first call every subsequent
        // call takes this fast path.
        if let Some(sized) = &self.sized {
            if sized.width == width && sized.height == height {
                return Ok((sized.image, sized.framebuffer));
            }
        }
        // Either this is the first call (`sized` is `None`) or the size changed: destroy
        // whatever was there before replacing it, so a size change never leaks the old target.
        if let Some(old) = self.sized.take() {
            unsafe {
                self.device.destroy_framebuffer(old.framebuffer, None);
                self.device.destroy_image_view(old.image_view, None);
                self.device.destroy_image(old.image, None);
                self.device.free_memory(old.image_memory, None);
            }
        }

        // --- Off-screen colour image (OPTIMAL tiling, used as attachment + transfer source) ---
        let format = vk::Format::R8G8B8A8_UNORM; // 8 bits per channel, matches our RGBA8 output
        let extent = vk::Extent3D {
            width,
            height,
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
        let image = unsafe { self.device.create_image(&image_info, None) }?;
        // Allocate and bind DEVICE_LOCAL memory for the image.
        let image_mem_req = unsafe { self.device.get_image_memory_requirements(image) };
        let image_memory = unsafe {
            allocate(
                &self.device,
                &self.mem_props,
                image_mem_req,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
        }?;
        unsafe { self.device.bind_image_memory(image, image_memory, 0) }?;

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
        let image_view = unsafe { self.device.create_image_view(&view_info, None) }?;

        // Framebuffer binding the image view to the (already-created, size-independent) render
        // pass, at this target's concrete width/height.
        let fb_attachments = [image_view];
        let framebuffer_info = vk::FramebufferCreateInfo::default()
            .render_pass(self.render_pass)
            .attachments(&fb_attachments)
            .width(width)
            .height(height)
            .layers(1);
        let framebuffer = unsafe { self.device.create_framebuffer(&framebuffer_info, None) }?;

        self.sized = Some(SizedTarget {
            width,
            height,
            image,
            image_memory,
            image_view,
            framebuffer,
        });
        Ok((image, framebuffer))
    }

    /// Render `request`'s triangle into this renderer's colour image and read the result back
    /// to CPU memory.
    ///
    /// Builds (or reuses) the colour image/view/framebuffer for `request`'s size, uploads the
    /// vertices, records and submits one draw, copies the image into a host-visible buffer
    /// packed tightly, and returns the RGBA8 bytes. Unlike the persistent objects owned by
    /// `self`, the vertex buffer, the readback buffer, the command buffer, and the fence used
    /// for this call are all created and destroyed within this call — there is no benefit to
    /// keeping per-frame objects alive between frames.
    ///
    /// # Errors
    /// Returns an error if building the sized target fails, or any per-frame Vulkan call fails.
    pub fn render_to_frame(&mut self, request: &FrameRequest) -> anyhow::Result<RenderedFrame> {
        // SAFETY: see `Renderer::new`'s comment — every ash call is FFI, made safe here by
        // constructing each argument immediately before use.
        unsafe { self.render_to_frame_inner(request) }
    }

    /// The `unsafe` body of [`Renderer::render_to_frame`], separated so the public method stays
    /// safe to call and the safety reasoning lives in one place.
    unsafe fn render_to_frame_inner(
        &mut self,
        request: &FrameRequest,
    ) -> anyhow::Result<RenderedFrame> {
        // Build or reuse the colour image/view/framebuffer sized for this request.
        let (image, framebuffer) =
            unsafe { self.ensure_sized_target(request.width, request.height) }?;

        // --- Vertex buffer (host-visible so we can copy the vertices straight in) ---
        let vertex_bytes = request.vertices.len() * std::mem::size_of::<Vertex>();
        let vbuf_info = vk::BufferCreateInfo::default()
            .size(vertex_bytes as u64)
            .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let vertex_buffer = unsafe { self.device.create_buffer(&vbuf_info, None) }?;
        let vbuf_req = unsafe { self.device.get_buffer_memory_requirements(vertex_buffer) };
        let vbuf_mem = unsafe {
            allocate(
                &self.device,
                &self.mem_props,
                vbuf_req,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
        }?;
        unsafe { self.device.bind_buffer_memory(vertex_buffer, vbuf_mem, 0) }?;
        // Map the buffer and copy the vertex data in.
        let ptr = unsafe {
            self.device.map_memory(
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
        unsafe { self.device.unmap_memory(vbuf_mem) };

        // --- Readback buffer (host-visible, holds the tightly-packed image after the copy) ---
        // Compute the size in u64 arithmetic. width/height arrive from an untrusted client
        // BeginFrame; multiplying them as u32 first could wrap (e.g. 46341*46341*4), silently
        // sizing the buffer too small in release builds. Widening each factor before the
        // multiply makes the arithmetic correct by construction regardless of the inputs.
        let readback_size = request.width as u64 * request.height as u64 * 4;
        let rbuf_info = vk::BufferCreateInfo::default()
            .size(readback_size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let readback_buffer = unsafe { self.device.create_buffer(&rbuf_info, None) }?;
        let rbuf_req = unsafe { self.device.get_buffer_memory_requirements(readback_buffer) };
        let rbuf_mem = unsafe {
            allocate(
                &self.device,
                &self.mem_props,
                rbuf_req,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
        }?;
        unsafe { self.device.bind_buffer_memory(readback_buffer, rbuf_mem, 0) }?;

        // --- Command buffer: draw, then copy image → readback buffer ---
        let cmd = unsafe {
            self.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }?[0];
        unsafe {
            self.device.begin_command_buffer(
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
            .render_pass(self.render_pass)
            .framebuffer(framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: request.width,
                    height: request.height,
                },
            })
            .clear_values(&clears);
        unsafe {
            self.device
                .cmd_begin_render_pass(cmd, &rp_begin, vk::SubpassContents::INLINE)
        };
        unsafe {
            self.device
                .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline)
        };

        // The pipeline declares viewport/scissor as dynamic state (see `new_inner`), so they
        // must be set here, per draw, from this call's actual request size — this is the direct
        // per-frame replacement for what the original one-shot code baked into the pipeline
        // once. The values are identical to what that code computed; only *when* Vulkan is told
        // them has changed.
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
        unsafe { self.device.cmd_set_viewport(cmd, 0, &[viewport]) };
        unsafe { self.device.cmd_set_scissor(cmd, 0, &[scissor]) };

        unsafe {
            self.device
                .cmd_bind_vertex_buffers(cmd, 0, &[vertex_buffer], &[0])
        };
        // Draw the triangle: vertex_count vertices, 1 instance.
        unsafe {
            self.device
                .cmd_draw(cmd, request.vertices.len() as u32, 1, 0, 0)
        };
        unsafe { self.device.cmd_end_render_pass(cmd) };

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
            .image_extent(vk::Extent3D {
                width: request.width,
                height: request.height,
                depth: 1,
            });
        unsafe {
            self.device.cmd_copy_image_to_buffer(
                cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                readback_buffer,
                &[copy],
            )
        };
        unsafe { self.device.end_command_buffer(cmd) }?;

        // Submit and wait for completion using a fence.
        let fence = unsafe {
            self.device
                .create_fence(&vk::FenceCreateInfo::default(), None)
        }?;
        let cmds = [cmd];
        let submit = vk::SubmitInfo::default().command_buffers(&cmds);
        unsafe { self.device.queue_submit(self.queue, &[submit], fence) }?;
        // Wait up to ~10 seconds for the GPU to finish.
        unsafe { self.device.wait_for_fences(&[fence], true, 10_000_000_000) }?;

        // --- Read the pixels out of the readback buffer ---
        let mapped = unsafe {
            self.device
                .map_memory(rbuf_mem, 0, readback_size, vk::MemoryMapFlags::empty())
        }?;
        // Copy the bytes into an owned Vec so we can free the GPU memory before returning.
        let mut pixels = vec![0u8; readback_size as usize];
        unsafe {
            std::ptr::copy_nonoverlapping(
                mapped as *const u8,
                pixels.as_mut_ptr(),
                readback_size as usize,
            )
        };
        unsafe { self.device.unmap_memory(rbuf_mem) };

        // --- Tear down this frame's TRANSIENT objects only (reverse creation order). The
        // persistent Renderer state (device, pipeline, render pass, command pool, sized target,
        // …) is intentionally left alone here — it is destroyed once, in `Renderer::drop`, not
        // after every frame. Freeing the command buffer back to the pool (rather than
        // destroying the pool, as the one-shot code did) is what makes the pool reusable by the
        // next `render_to_frame` call. ---
        unsafe {
            self.device.destroy_fence(fence, None);
            self.device.free_command_buffers(self.command_pool, &cmds);
            self.device.destroy_buffer(readback_buffer, None);
            self.device.free_memory(rbuf_mem, None);
            self.device.destroy_buffer(vertex_buffer, None);
            self.device.free_memory(vbuf_mem, None);
        }

        // Hand back the pixels.
        Ok(RenderedFrame {
            width: request.width,
            height: request.height,
            pixels,
        })
    }
}

impl Drop for Renderer {
    /// Destroy every Vulkan object this `Renderer` owns, in reverse creation order — child
    /// objects before the parents they were created from, the device before the instance.
    /// Mirrors exactly what the original one-shot `render_triangle_inner` did at the end of
    /// every call; the only difference is that this now runs once, when the `Renderer` itself
    /// is dropped, instead of after every single render.
    fn drop(&mut self) {
        // SAFETY: `self` is being destroyed, so every Vulkan handle here is used for the last
        // time — nothing outside this function can touch them afterward through safe code. All
        // handles were created by `self.device`/`self.instance`, which are the last two things
        // destroyed below, so every child is torn down while its parent is still valid.
        unsafe {
            // The size-dependent target exists only if at least one frame was rendered (`None`
            // if a `Renderer` was created and dropped without ever calling `render_to_frame`).
            if let Some(sized) = self.sized.take() {
                self.device.destroy_framebuffer(sized.framebuffer, None);
                self.device.destroy_image_view(sized.image_view, None);
                self.device.destroy_image(sized.image, None);
                self.device.free_memory(sized.image_memory, None);
            }
            // Destroying the command pool implicitly frees any command buffers still allocated
            // from it (none should remain — `render_to_frame` frees its own — but this is also
            // safe if one somehow did).
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
        // `_entry` (the loaded Vulkan library) drops automatically right after this function
        // returns, unloading the loader now that nothing backed by it — `instance`, `device` —
        // is still alive to use it.
    }
}

/// Render one frame and read it back to CPU memory (SP0/SP1/`--png` path).
///
/// A convenience wrapper that builds a throwaway [`Renderer`] for a single frame, so callers
/// that do not need a persistent renderer (the pixel test, the PNG dump, the `wl_shm` fallback)
/// keep working unchanged. All Vulkan objects are created and destroyed within this call, via
/// the `Renderer`'s own construction and `Drop` — behaviourally identical to how the original
/// one-shot `render_triangle` worked, just implemented in terms of the now-persistent type.
///
/// # Errors
/// Returns an error if renderer creation or rendering fails.
pub fn render_triangle(request: &FrameRequest) -> anyhow::Result<RenderedFrame> {
    // One-shot: create a renderer, render, drop it.
    Renderer::new()?.render_to_frame(request)
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
