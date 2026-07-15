//! *How* to draw: the render pass and the graphics pipeline, plus the shaders they are built from.
//!
//! These objects describe the drawing method — which attachments exist, how they are loaded and
//! stored, what the vertex data looks like, and which shaders run — without referring to any
//! particular image or any particular size. That independence is why they live in their own module
//! from [`crate::render`], which owns the concrete per-frame objects.
//!
//! # The shaders are embedded, not compiled
//! The SPIR-V is `include_bytes!`d straight out of the repository's `shaders/` directory, which is
//! the same mechanism `rayland-server` uses. The point is that building this crate requires no
//! shader compiler (`glslangValidator`) to be installed: the compiled artefacts are committed, and
//! `shaders/README.md` documents how to regenerate them after editing the GLSL. The shaders take
//! `(vec2 position, vec3 color)` per vertex, which is what [`Vertex`] below must mirror exactly.

// The Vulkan API surface and its handle/struct types.
use ash::vk;

/// The compiled vertex shader: places each vertex at its 2-D position and passes its colour on.
///
/// Shared verbatim with `rayland-server`, so that what this app draws and what SP0's renderer drew
/// are the same picture produced by the same code — which is what makes the outputs comparable.
const VERT_SPV: &[u8] = include_bytes!("../../../shaders/triangle.vert.spv");

/// The compiled fragment shader: writes the interpolated colour, opaque.
const FRAG_SPV: &[u8] = include_bytes!("../../../shaders/triangle.frag.spv");

/// The colour format the triangle is rendered into, and the format the pixels are read back in.
///
/// `R8G8B8A8_UNORM` is chosen so the bytes that come back from the GPU are already exactly what a
/// PNG wants — 8 bits per channel, red first — with no swizzle or conversion on the CPU. A `BGRA`
/// format would render identically and then silently produce a blue triangle in the PNG, which is
/// a genuinely easy mistake to make and an annoying one to spot, since a red/blue swap looks like
/// a plausible rendering bug rather than a formatting one.
pub const COLOR_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// One vertex, laid out exactly as the vertex shader's inputs expect.
///
/// `#[repr(C)]` is load-bearing, not decoration: the field offsets declared to Vulkan below must
/// match the real in-memory layout, and a `repr(Rust)` struct may legally reorder or pad its
/// fields however the compiler likes. `offset_of!` is still used for the offsets — belt and braces
/// — so the two can never disagree even if a field is added between them later.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Vertex {
    /// Position in normalised device coordinates: x and y each run from -1 to +1, with (-1, -1) at
    /// the **top-left** of the image. That y-down convention is Vulkan's, and differs from OpenGL's
    /// — a classic source of vertically-mirrored first triangles. This program's triangle is not
    /// symmetric about the horizontal axis, so the corner checks in the tests would catch a flip.
    pub position: [f32; 2],
    /// Linear RGB, each channel `0.0..=1.0`. Interpolated across the triangle by the rasteriser;
    /// with all three vertices the same colour, that interpolation is a no-op and every covered
    /// pixel gets exactly this value.
    pub color: [f32; 3],
}

/// The render pass and graphics pipeline, and the layout the pipeline was built with.
///
/// Bundled together because they are created together, destroyed together, and are useless apart:
/// a pipeline is permanently bound to the render pass it was created against.
pub struct TrianglePipeline {
    /// Clears the colour attachment, draws into it, and leaves it ready to be copied out.
    pub render_pass: vk::RenderPass,
    /// The pipeline: vertex layout, fixed-function state, and the two shader stages.
    pub pipeline: vk::Pipeline,
    /// The pipeline's layout. Empty (this program uses no descriptors and no push constants) but
    /// required by Vulkan, and it must outlive the pipeline built from it, so it is kept here to
    /// be destroyed alongside it rather than being dropped early.
    pub layout: vk::PipelineLayout,
}

impl TrianglePipeline {
    /// Build the render pass and pipeline.
    ///
    /// Neither object bakes in an image size: viewport and scissor are declared as **dynamic
    /// state** and supplied per draw by [`crate::render`]. This program only ever renders one
    /// size, so that is not needed for flexibility — it is done because it keeps "how to draw"
    /// genuinely free of "how big", which is the split this module exists to maintain.
    ///
    /// # Errors
    /// Returns an error if any Vulkan creation call fails.
    ///
    /// # Safety
    /// `device` must be live and must outlive the returned pipeline, which the caller destroys via
    /// [`TrianglePipeline::destroy`].
    pub unsafe fn new(device: &ash::Device) -> anyhow::Result<TrianglePipeline> {
        let render_pass = unsafe { create_render_pass(device) }?;

        // The two shader stages, built from the embedded SPIR-V. These modules are needed only
        // while the pipeline is being created — Vulkan copies what it needs out of them — so they
        // are destroyed at the end of this function rather than kept in the struct.
        let vert_module = unsafe { create_shader_module(device, VERT_SPV) }?;
        let frag_module = unsafe { create_shader_module(device, FRAG_SPV) }?;

        // Build the pipeline in a helper so that this function can destroy the shader modules on
        // both the success and the failure path without duplicating the cleanup or leaking them
        // when a `?` fires mid-build.
        let built = unsafe { create_pipeline(device, render_pass, vert_module, frag_module) };

        // SAFETY: the modules are live and, whether the build succeeded or failed, nothing refers
        // to them any more — a created pipeline does not.
        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }

        match built {
            Ok((pipeline, layout)) => Ok(TrianglePipeline {
                render_pass,
                pipeline,
                layout,
            }),
            Err(error) => {
                // The render pass was created before the failure and nothing else owns it yet, so
                // this error path must free it or it leaks for the process's lifetime.
                // SAFETY: `render_pass` is live and, the pipeline having failed, unreferenced.
                unsafe { device.destroy_render_pass(render_pass, None) };
                Err(error)
            }
        }
    }

    /// Destroy the pipeline, its layout, and the render pass.
    ///
    /// Not a [`Drop`] impl because destroying any of these requires the `ash::Device` they were
    /// created from, and a `Drop` cannot be handed one. Keeping teardown explicit is the ordinary
    /// Vulkan-in-Rust trade-off: the alternative is storing a device clone in every object.
    ///
    /// # Safety
    /// `device` must be the same live device these objects were created from, and no in-flight
    /// command buffer may still reference them — the caller guarantees this by fence-waiting its
    /// submission to completion before tearing down.
    pub unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: the caller guarantees the device is live, matches, and that the GPU is done with
        // these objects. Order is not significant among the three; they refer to nothing else.
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_render_pass(self.render_pass, None);
        }
    }
}

/// Create the render pass: one colour attachment, cleared at the start, kept at the end, and left
/// in `TRANSFER_SRC_OPTIMAL` so the readback copy can read it with no further barrier.
///
/// # The subpass dependency is not optional
/// Vulkan gives **no** ordering or visibility guarantee between a render pass's attachment writes
/// and whatever is submitted afterwards, unless a dependency says so. This program copies the
/// image to a buffer immediately after the render pass ends — a transfer-stage read of exactly the
/// pixels the fragment shader just wrote. Without the dependency below, a driver is entirely
/// within its rights to start that copy before the colour writes are visible, and read a blank or
/// half-drawn image. Some drivers happen to serialize enough work that this never bites; that is
/// luck, not a guarantee, and relying on it is how a program renders perfectly on the machine it
/// was written on and produces garbage everywhere else.
///
/// # Errors
/// Returns an error if `vkCreateRenderPass` fails.
///
/// # Safety
/// `device` must be live.
unsafe fn create_render_pass(device: &ash::Device) -> anyhow::Result<vk::RenderPass> {
    let color_attachment = vk::AttachmentDescription::default()
        .format(COLOR_FORMAT)
        .samples(vk::SampleCountFlags::TYPE_1)
        // CLEAR: fill with the clear colour at the start of the pass. This is what puts blue
        // everywhere the triangle does not cover, and it is cheaper than a separate clear command.
        .load_op(vk::AttachmentLoadOp::CLEAR)
        // STORE: keep what was drawn. The default (DONT_CARE) would let the driver discard the
        // pixels the instant the pass ends, which is exactly what this program came for.
        .store_op(vk::AttachmentStoreOp::STORE)
        // The image's previous contents are irrelevant — the clear overwrites all of them — and
        // saying so lets the driver skip preserving them.
        .initial_layout(vk::ImageLayout::UNDEFINED)
        // End the pass with the image already in the layout the readback copy needs, so no
        // separate pipeline barrier is required afterwards.
        .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
    let color_ref = vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let color_refs = [color_ref];
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_refs);

    // Make subpass 0's colour writes available and visible to the transfer-stage read that follows
    // the render pass — see this function's docs for why the copy is unsound without this.
    let dependency = vk::SubpassDependency::default()
        .src_subpass(0)
        .dst_subpass(vk::SUBPASS_EXTERNAL)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
        // The copy reads the same pixels the pass wrote, region for region, so per-region ordering
        // is sufficient and lets the driver overlap more than a full barrier would.
        .dependency_flags(vk::DependencyFlags::BY_REGION);

    let attachments = [color_attachment];
    let subpasses = [subpass];
    let dependencies = [dependency];
    let info = vk::RenderPassCreateInfo::default()
        .attachments(&attachments)
        .subpasses(&subpasses)
        .dependencies(&dependencies);
    // SAFETY: the caller guarantees `device` is live; every borrowed array above outlives the call.
    Ok(unsafe { device.create_render_pass(&info, None) }?)
}

/// Assemble the graphics pipeline and its (empty) layout from the two shader modules.
///
/// Returns both handles: the layout must be kept and destroyed alongside the pipeline, not dropped
/// here.
///
/// # Errors
/// Returns an error if layout or pipeline creation fails. On pipeline failure the layout is
/// destroyed before returning, so the error path leaks nothing.
///
/// # Safety
/// `device`, `render_pass`, and both shader modules must be live for the duration of the call.
unsafe fn create_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    vert_module: vk::ShaderModule,
    frag_module: vk::ShaderModule,
) -> anyhow::Result<(vk::Pipeline, vk::PipelineLayout)> {
    // The entry point every one of these shaders uses. Bound to a local because ash 0.38's
    // `.name()` stores a borrow of this `CStr` — a temporary here would dangle by the time
    // `create_graphics_pipelines` reads it.
    let entry_name = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert_module)
            .name(entry_name),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag_module)
            .name(entry_name),
    ];

    // Vertex input: one buffer binding carrying tightly-packed `Vertex`es, consumed one per vertex.
    let binding = vk::VertexInputBindingDescription::default()
        .binding(0)
        .stride(size_of::<Vertex>() as u32)
        .input_rate(vk::VertexInputRate::VERTEX);
    let attributes = [
        vk::VertexInputAttributeDescription::default()
            .location(0)
            .binding(0)
            .format(vk::Format::R32G32_SFLOAT)
            // Ask the compiler where `position` really is rather than assuming 0 — see `Vertex`.
            .offset(std::mem::offset_of!(Vertex, position) as u32),
        vk::VertexInputAttributeDescription::default()
            .location(1)
            .binding(0)
            .format(vk::Format::R32G32B32_SFLOAT)
            // Likewise: do not assume `color` immediately follows `position` with no padding.
            .offset(std::mem::offset_of!(Vertex, color) as u32),
    ];
    let bindings = [binding];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attributes);

    // Every three vertices form one independent triangle.
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);

    // Declare that there is one viewport and one scissor, but not what they are: both are dynamic
    // state, set per draw. This is what lets the pipeline be built without knowing the image size.
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

    let rasterizer = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        // Cull nothing. With culling on, a triangle whose vertices happen to wind the "wrong" way
        // is silently not drawn at all — a blank image with no error anywhere. Disabling culling
        // removes that entire failure mode, and costs nothing when there is one triangle.
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        // Required to be exactly 1.0 unless the `wideLines` feature is enabled; irrelevant to FILL
        // mode, but a value of 0.0 here is a validation error rather than a no-op.
        .line_width(1.0);

    // No multisampling: one sample per pixel, so every pixel is either fully covered or not
    // covered, and the test's exact-colour assertions hold with no edge blending to reason about.
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);

    // Write all four channels; no blending, so the fragment colour lands in the attachment
    // verbatim. Note the write mask is not defaulted — a zeroed `color_write_mask` would write no
    // channels at all and produce an image of pure clear colour.
    let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)
        .blend_enable(false);
    let blend_attachments = [blend_attachment];
    let color_blend =
        vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);

    // An empty layout: the shaders read no descriptors and no push constants.
    // SAFETY: the caller guarantees `device` is live.
    let layout =
        unsafe { device.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default(), None) }?;

    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .dynamic_state(&dynamic_state)
        .rasterization_state(&rasterizer)
        .multisample_state(&multisample)
        .color_blend_state(&color_blend)
        .layout(layout)
        .render_pass(render_pass)
        .subpass(0);
    // SAFETY: the caller guarantees `device`, `render_pass`, and the modules are live; every
    // borrowed array above outlives the call.
    let pipeline =
        match unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) }
        {
            // Exactly one `GraphicsPipelineCreateInfo` was passed, so exactly one pipeline comes back.
            Ok(pipelines) => pipelines[0],
            // ash 0.38 reports partial success as `Err((partial, result))`. With a single pipeline
            // requested there is nothing partial to salvage, so the handles are discarded and only the
            // `vk::Result` is surfaced.
            Err((_, error)) => {
                // The layout was created above and the pipeline that would have owned its lifetime
                // does not exist, so free it here rather than leak it.
                // SAFETY: `layout` is live and now unreferenced.
                unsafe { device.destroy_pipeline_layout(layout, None) };
                return Err(error.into());
            }
        };

    Ok((pipeline, layout))
}

/// Turn a blob of SPIR-V bytes into a Vulkan shader module.
///
/// # Errors
/// Returns an error if the bytes are not valid SPIR-V framing (`read_spv` checks the magic number
/// and that the length is a whole number of 32-bit words) or if `vkCreateShaderModule` fails.
///
/// # Safety
/// `device` must be live. The returned module must be destroyed before the device is.
unsafe fn create_shader_module(
    device: &ash::Device,
    spv: &[u8],
) -> anyhow::Result<vk::ShaderModule> {
    // SPIR-V is a stream of 32-bit words, but `include_bytes!` gives bytes with no alignment
    // guarantee. `read_spv` copies them into a properly aligned `Vec<u32>`, which is what Vulkan
    // requires — casting the byte pointer in place would be undefined behaviour on the unlucky
    // day the literal is not 4-byte aligned.
    let mut cursor = std::io::Cursor::new(spv);
    let words = ash::util::read_spv(&mut cursor)?;
    let info = vk::ShaderModuleCreateInfo::default().code(&words);
    // SAFETY: the caller guarantees `device` is live; `words` outlives the call.
    Ok(unsafe { device.create_shader_module(&info, None) }?)
}
