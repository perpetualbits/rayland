//! *How* to draw: the render pass and the graphics pipeline, plus the vertex shader they are built
//! from.
//!
//! These objects describe the drawing method — which attachments exist, how they are loaded and
//! stored, what the vertex data looks like, and which shader stages run — without referring to any
//! particular image or any particular size. That independence is why they live in their own module
//! from [`crate::targets`], which owns the concrete per-frame-size objects, and from
//! [`crate::scene`], which owns the concrete per-draw data.
//!
//! # The two shader stages are sourced differently, on purpose
//! The vertex shader is the same for every fixture and every test in this crate — it just applies
//! the MVP matrix and passes normal/UV through — so it is embedded here via `include_bytes!`,
//! exactly as `rayland-refapp`'s pipeline embeds its shaders. The fragment shader is **not**
//! embedded here: it is the one thing this crate deliberately does not fix, because it is the
//! fixtures' independent variable (CPU-computed fractal vs. GPU-computed fractal). It arrives as a
//! parameter to [`IcosaPipeline::new`] instead, already read from whichever `.spv` the caller
//! chose — `rayland-icosa-vk`'s own test passes `icosa_flat.frag.spv`; a later fixture will pass
//! `icosa_textured.frag.spv`.
//!
//! # This is the repository's first depth attachment
//! Nothing before this crate has allocated a depth image or enabled depth testing. See
//! [`DEPTH_FORMAT`] for why `D32_SFLOAT` needs no format-support negotiation, and
//! `create_render_pass`/`create_pipeline` below for the render-pass attachment and the
//! depth-stencil pipeline state that make the depth buffer actually take effect.
//!
//! # Depth testing is configured but not exercised by any test in this crate
//! `create_pipeline` enables depth testing (`depth_test_enable(true)`, `CompareOp::LESS`), and the
//! render pass clears and binds a real depth attachment every frame — this is genuinely wired up,
//! not a placeholder. But for *this* scene, no test here or in `tests/renders_the_solid.rs` can
//! tell whether it is working, because it has nothing to do: the rasterizer's back-face culling
//! (`cull_mode(vk::CullModeFlags::BACK)`, below) already discards every triangle facing away from
//! the camera before it ever reaches the depth test, and for a convex solid like the icosahedron
//! that is exactly the set of triangles the depth test would otherwise need to arbitrate against
//! the front-facing ones. A convex solid's front-facing surface covers each screen pixel with at
//! most one triangle, so once culling has run there is never more than one fragment competing for
//! any given pixel — nothing for `CompareOp::LESS` to ever reject.
//!
//! This was checked experimentally, not assumed: setting `depth_test_enable(false)` here while
//! leaving culling untouched leaves this crate's entire test suite passing, and produces
//! byte-for-byte identical rendered images (checked across several different frame rotations) to
//! the depth-tested pipeline. Disabling culling instead (with depth testing back on) makes the two
//! configurations diverge, which confirms the mechanism above rather than some other, coincidental
//! explanation: culling is what is currently doing all the work depth testing was written to do.
//!
//! None of this makes the depth attachment dead weight worth removing. The two icosahedron
//! fixtures' *later* geometry — anything concave, or anything that draws more than one convex
//! solid — can produce screen pixels genuinely covered by more than one front-facing triangle, and
//! depth testing is what will resolve those correctly once that geometry exists. This crate keeps
//! the depth attachment and the depth-tested pipeline state for that reason; a future fixture that
//! actually needs it will be the first thing in this repository to exercise it, and should add a
//! test that can tell the two configurations apart the way this crate's own tests currently cannot.

// The Vulkan API surface and its handle/struct types.
use ash::vk;
// The vertex layout this pipeline's vertex input state describes — the geometry crate's `Vertex`,
// not a local copy, so the two can never silently drift apart (see `Vertex`'s own `#[repr(C)]` doc).
use rayland_icosa_core::geometry::Vertex;

/// The compiled vertex shader: applies the MVP matrix and passes the normal and UV through.
///
/// Shared by both fixtures — see the module docs for why this one shader stage is embedded here
/// while the fragment stage is not.
const VERT_SPV: &[u8] = include_bytes!("../../../shaders/icosa.vert.spv");

/// The format of the colour attachment, and of the PNG written from it.
///
/// `R8G8B8A8_UNORM` is universally supported as a colour attachment, so no format negotiation is
/// needed, and it maps one-to-one onto the PNG's bytes with no conversion that could introduce a
/// rounding difference between hosts.
pub const COLOR_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// The format of the depth attachment.
///
/// `D32_SFLOAT` is chosen because the Vulkan specification *requires* every implementation to
/// support it as a depth-stencil attachment. That matters more here than usual: this is the first
/// depth buffer in this repository, and picking a format that needs
/// `vkGetPhysicalDeviceFormatProperties` negotiation would add a failure mode on the very path
/// being brought up. A stencil-bearing format would also be fine on most hardware and is not
/// guaranteed; there is no stencil in this scene.
pub const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;

/// The texture a fragment shader samples, if it samples one.
///
/// `Scene` takes this as an `Option` rather than always declaring binding 1, because a descriptor
/// set layout that declares a binding no shader reads is not free: validation layers warn about it,
/// and every set written against it must still supply something. The GPU fixture has no texture at
/// all — passing `None` is an honest statement of that, and a dummy 1×1 image would be a fiction
/// that made the two fixtures look more alike than they are.
pub struct SamplerBinding {
    /// The view the shader samples through.
    pub view: vk::ImageView,
    /// The sampler's filtering and addressing rules.
    pub sampler: vk::Sampler,
}

/// The render pass, the graphics pipeline, and the layouts they were built with.
///
/// Bundled together because they are created together, destroyed together, and are useless apart:
/// a pipeline is permanently bound to the render pass it was created against, and a descriptor set
/// allocated against `descriptor_set_layout` is only valid for a pipeline built from that same
/// layout.
pub(crate) struct IcosaPipeline {
    /// Clears the colour and depth attachments, draws the solid into them with depth testing, and
    /// leaves the colour attachment ready to be copied out.
    pub render_pass: vk::RenderPass,
    /// The pipeline: vertex layout, depth-stencil state, culling, and the two shader stages.
    pub pipeline: vk::Pipeline,
    /// The pipeline's layout — the descriptor set layout it was built against, wrapped as Vulkan
    /// requires. Must outlive the pipeline, so it is kept here to be destroyed alongside it.
    pub layout: vk::PipelineLayout,
    /// The descriptor set layout: binding 0 (the uniform buffer) always, binding 1 (the sampler)
    /// only when [`IcosaPipeline::new`] was given a [`SamplerBinding`]. [`crate::scene::Scene`]
    /// allocates its one descriptor set against this.
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    /// Whether binding 1 exists in `descriptor_set_layout`. `Scene` consults this when writing its
    /// descriptor set, rather than re-deriving it from whether a sampler was supplied, so the two
    /// can never disagree about what the layout actually declares.
    pub has_sampler_binding: bool,
}

impl IcosaPipeline {
    /// Build the render pass and pipeline.
    ///
    /// `fragment_spirv` is the fragment shader's compiled SPIR-V, already parsed into words (e.g.
    /// via `ash::util::read_spv`) — see the module docs for why this is a parameter rather than a
    /// constant. `has_sampler` controls whether the descriptor set layout declares binding 1; it
    /// must agree with whether `fragment_spirv` actually declares a `sampler2D` at that binding, or
    /// the pipeline will fail to build (a shader that reads an undeclared binding) or the
    /// descriptor set will be written against a binding nothing reads (harmless, but see
    /// [`SamplerBinding`]'s doc for why this crate avoids that instead).
    ///
    /// Neither object bakes in an image size: viewport and scissor are declared as **dynamic
    /// state** and supplied per draw by [`crate::scene::Scene::draw`]. Every fixture built on this
    /// crate only ever renders one size (`rayland_icosa_core::IMAGE_SIZE`), so this is not needed
    /// for flexibility — it is done because it keeps "how to draw" genuinely free of "how big",
    /// which is the split this module exists to maintain.
    ///
    /// # Errors
    /// Returns an error if any Vulkan creation call fails.
    ///
    /// # Safety
    /// `device` must be live and must outlive the returned pipeline, which the caller destroys via
    /// [`IcosaPipeline::destroy`].
    pub unsafe fn new(
        device: &ash::Device,
        fragment_spirv: &[u32],
        has_sampler: bool,
    ) -> anyhow::Result<IcosaPipeline> {
        let render_pass = unsafe { create_render_pass(device) }?;
        let descriptor_set_layout =
            match unsafe { create_descriptor_set_layout(device, has_sampler) } {
                Ok(layout) => layout,
                Err(error) => {
                    // Nothing else owns `render_pass` yet — free it before giving up, or it leaks for
                    // the process's lifetime.
                    unsafe { device.destroy_render_pass(render_pass, None) };
                    return Err(error);
                }
            };

        // The two shader stages. The vertex module is built from the embedded SPIR-V; the fragment
        // module from whatever the caller supplied. Both are needed only while the pipeline is
        // being created — Vulkan copies what it needs out of them — so neither is kept afterwards.
        let vert_module = match unsafe { create_shader_module_from_bytes(device, VERT_SPV) } {
            Ok(module) => module,
            Err(error) => {
                unsafe {
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_render_pass(render_pass, None);
                };
                return Err(error);
            }
        };
        let frag_info = vk::ShaderModuleCreateInfo::default().code(fragment_spirv);
        let frag_module = match unsafe { device.create_shader_module(&frag_info, None) } {
            Ok(module) => module,
            Err(error) => {
                unsafe {
                    device.destroy_shader_module(vert_module, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_render_pass(render_pass, None);
                };
                return Err(error.into());
            }
        };

        // Build the pipeline in a helper so this function can destroy the shader modules on both
        // the success and the failure path without duplicating the cleanup.
        let built = unsafe {
            create_pipeline(
                device,
                render_pass,
                descriptor_set_layout,
                vert_module,
                frag_module,
            )
        };

        // SAFETY: the modules are live and, whether the build succeeded or failed, nothing refers
        // to them any more — a created pipeline does not.
        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }

        match built {
            Ok((pipeline, layout)) => Ok(IcosaPipeline {
                render_pass,
                pipeline,
                layout,
                descriptor_set_layout,
                has_sampler_binding: has_sampler,
            }),
            Err(error) => {
                // Neither `render_pass` nor `descriptor_set_layout` is owned by anything else yet.
                unsafe {
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_render_pass(render_pass, None);
                };
                Err(error)
            }
        }
    }

    /// Destroy the pipeline, its layout, its descriptor set layout, and the render pass.
    ///
    /// # Safety
    /// `device` must be the same live device these objects were created from, and no in-flight
    /// command buffer may still reference them — the caller guarantees this by fence-waiting its
    /// submission to completion before tearing down.
    pub unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: the caller guarantees the device is live, matches, and that the GPU is done with
        // these objects. Order is not significant among the four; they refer to nothing else.
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_render_pass(self.render_pass, None);
        }
    }
}

/// Create the descriptor set layout: binding 0 is always a uniform buffer visible to both stages
/// (the vertex shader reads `mvp`; a fragment shader may read `half_width`/`center`). Binding 1, a
/// combined image sampler visible to the fragment stage, exists only when `has_sampler` is true —
/// see [`SamplerBinding`]'s doc for why this is conditional rather than always present.
///
/// # Errors
/// Returns an error if `vkCreateDescriptorSetLayout` fails.
///
/// # Safety
/// `device` must be live.
unsafe fn create_descriptor_set_layout(
    device: &ash::Device,
    has_sampler: bool,
) -> anyhow::Result<vk::DescriptorSetLayout> {
    // Binding 0 is unconditional: every fixture's vertex shader reads `mvp` out of it.
    let uniform_binding = vk::DescriptorSetLayoutBinding::default()
        .binding(0)
        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT);
    let sampler_binding = vk::DescriptorSetLayoutBinding::default()
        .binding(1)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::FRAGMENT);

    // Build the slice conditionally rather than always declaring binding 1 with a dummy — see
    // `SamplerBinding`'s doc for why an always-present binding would be dishonest here.
    let bindings: Vec<vk::DescriptorSetLayoutBinding> = if has_sampler {
        vec![uniform_binding, sampler_binding]
    } else {
        vec![uniform_binding]
    };
    let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
    // SAFETY: the caller guarantees `device` is live; `bindings` outlives the call.
    Ok(unsafe { device.create_descriptor_set_layout(&info, None) }?)
}

/// Create the render pass: a colour attachment and a depth attachment, both cleared at the start.
/// The colour attachment ends in `TRANSFER_SRC_OPTIMAL` so the readback copy needs no further
/// barrier; the depth attachment ends in `DEPTH_STENCIL_ATTACHMENT_OPTIMAL` since nothing ever
/// reads it back (see [`DEPTH_FORMAT`]'s doc).
///
/// # The subpass dependency is not optional
/// Vulkan gives **no** ordering or visibility guarantee between a render pass's attachment writes
/// and whatever is submitted afterwards, unless a dependency says so. [`crate::scene::Scene::draw`]
/// copies the colour image to a buffer immediately after the render pass ends — a transfer-stage
/// read of exactly the pixels the fragment shader just wrote. Without the dependency below, a
/// driver is entirely within its rights to start that copy before the colour writes are visible,
/// and read a blank or half-drawn image. Only the colour attachment needs this: nothing ever reads
/// the depth attachment after the pass, so no depth-side dependency is added.
///
/// # Why no explicit dependency guards frame-to-frame reuse of the depth image
/// The colour and depth targets are created once and reused across every draw (see
/// [`crate::targets`]), and depth's `load_op` is `CLEAR` every time. That is safe with no extra
/// synchronization only because [`crate::scene::Scene::draw`] fence-waits the GPU to finish before
/// it returns: by the time a frame's command buffer is recorded, the previous frame's reads and
/// writes of these same images are already complete, so there is nothing for a render-pass-level
/// dependency to order against. A design that submitted frame N+1 before waiting on frame N would
/// need one; this crate's contract (documented on `Scene::draw`) means it never has to.
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
        // CLEAR: fill with the clear colour at the start of the pass — the black background the
        // corner pixels in this crate's own test check for.
        .load_op(vk::AttachmentLoadOp::CLEAR)
        // STORE: keep what was drawn; DONT_CARE would let the driver discard it the instant the
        // pass ends, which is exactly the opposite of what the readback needs.
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        // End the pass with the image already in the layout the readback copy needs, so no
        // separate pipeline barrier is required afterwards.
        .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);

    // The depth attachment. `CLEAR` on load because every frame starts with nothing drawn, and
    // `DONT_CARE` on store because — unlike the colour attachment — nothing ever reads the depth
    // buffer after the render pass ends. Saying so explicitly lets a tiler discard it instead of
    // writing a megabyte back to memory for nobody.
    let depth_attachment = vk::AttachmentDescription::default()
        .format(DEPTH_FORMAT)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::DONT_CARE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL);

    let color_ref = vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let color_refs = [color_ref];
    let depth_ref = vk::AttachmentReference::default()
        .attachment(1)
        .layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL);
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_refs)
        .depth_stencil_attachment(&depth_ref);

    // Make subpass 0's colour writes available and visible to the transfer-stage read that follows
    // the render pass — see this function's docs for why the copy is unsound without this, and for
    // why depth needs no equivalent.
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

    let attachments = [color_attachment, depth_attachment];
    let subpasses = [subpass];
    let dependencies = [dependency];
    let info = vk::RenderPassCreateInfo::default()
        .attachments(&attachments)
        .subpasses(&subpasses)
        .dependencies(&dependencies);
    // SAFETY: the caller guarantees `device` is live; every borrowed array above outlives the call.
    Ok(unsafe { device.create_render_pass(&info, None) }?)
}

/// Assemble the graphics pipeline and its layout from the two shader modules and the descriptor set
/// layout.
///
/// Returns both handles: the layout must be kept and destroyed alongside the pipeline, not dropped
/// here.
///
/// # Errors
/// Returns an error if layout or pipeline creation fails. On pipeline failure the layout is
/// destroyed before returning, so the error path leaks nothing.
///
/// # Safety
/// `device`, `render_pass`, `descriptor_set_layout`, and both shader modules must be live for the
/// duration of the call.
unsafe fn create_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    descriptor_set_layout: vk::DescriptorSetLayout,
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

    // One interleaved buffer holding position, normal and UV per vertex. The stride is the Rust
    // struct's size, and the offsets are its field offsets: this description and `Vertex`'s
    // `#[repr(C)]` layout are two halves of one contract, and a mismatch feeds one attribute's
    // bytes into another's slot — producing a picture that renders happily and is wrong.
    //
    // `offset_of!` is used rather than the hand-computed 0/12/24 the brief for this task quotes,
    // matching `rayland-refapp`'s own established idiom for this exact situation ("belt and braces
    // — the two can never disagree even if a field is added between them later"). The values are
    // the same either way for `Vertex` as it stands today; `offset_of!` is what keeps them the same
    // automatically if that struct's fields are ever reordered or a field is inserted.
    let binding = vk::VertexInputBindingDescription::default()
        .binding(0)
        .stride(std::mem::size_of::<Vertex>() as u32)
        .input_rate(vk::VertexInputRate::VERTEX);
    let attributes = [
        vk::VertexInputAttributeDescription::default()
            .location(0)
            .binding(0)
            .format(vk::Format::R32G32B32_SFLOAT)
            .offset(std::mem::offset_of!(Vertex, position) as u32),
        vk::VertexInputAttributeDescription::default()
            .location(1)
            .binding(0)
            .format(vk::Format::R32G32B32_SFLOAT)
            .offset(std::mem::offset_of!(Vertex, normal) as u32),
        vk::VertexInputAttributeDescription::default()
            .location(2)
            .binding(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(std::mem::offset_of!(Vertex, uv) as u32),
    ];
    let bindings = [binding];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attributes);

    // Every three vertices form one independent triangle — `icosahedron()` emits 60 vertices as 20
    // unshared, flat-shaded triangles (see that function's doc).
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
        // Back-face culling, matching the winding `icosahedron()`'s `FACES` table produces (see
        // that module's `all_normals_point_outward` test). Do NOT disable this to "fix" a missing
        // face — a mis-wound face belongs to the geometry table, and silently drawing both sides of
        // every triangle here would hide that defect rather than surface it.
        .cull_mode(vk::CullModeFlags::BACK)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        // Required to be exactly 1.0 unless the `wideLines` feature is enabled; irrelevant to FILL
        // mode, but a value of 0.0 here is a validation error rather than a no-op.
        .line_width(1.0);

    // No multisampling: one sample per pixel, so every pixel is either fully covered or not
    // covered, and the corner/centre checks in this crate's tests hold with no edge blending.
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);

    // `LESS` with `depth_write_enable` is the ordinary opaque-geometry configuration: a fragment
    // survives only if it is nearer than what is already there, and if it survives it becomes the
    // new nearest. Note what disabling this would NOT currently do: it would not make the 20 faces
    // paint over each other in submission order, because `cull_mode(BACK)` above already discards
    // every back face before it reaches this stage, and for this convex solid that leaves at most
    // one front face per pixel — nothing left for depth testing to arbitrate. See the module docs
    // for the experiment that confirmed this and why the depth state stays enabled regardless.
    let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
        .depth_test_enable(true)
        .depth_write_enable(true)
        .depth_compare_op(vk::CompareOp::LESS)
        .depth_bounds_test_enable(false)
        .stencil_test_enable(false);

    // Write all four channels; no blending, so the fragment colour lands in the attachment
    // verbatim. Note the write mask is not defaulted — a zeroed `color_write_mask` would write no
    // channels at all and produce an image of pure clear colour.
    let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)
        .blend_enable(false);
    let blend_attachments = [blend_attachment];
    let color_blend =
        vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);

    // The pipeline layout: one descriptor set (binding 0 always, binding 1 conditionally — see
    // `create_descriptor_set_layout`), no push constants.
    let set_layouts = [descriptor_set_layout];
    let layout_info = vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts);
    // SAFETY: the caller guarantees `device` is live; `set_layouts` outlives the call.
    let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }?;

    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .dynamic_state(&dynamic_state)
        .rasterization_state(&rasterizer)
        .multisample_state(&multisample)
        .depth_stencil_state(&depth_stencil)
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
            // requested there is nothing partial to salvage, so the handles are discarded and only
            // the `vk::Result` is surfaced.
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
/// Used only for the embedded vertex shader; the fragment shader arrives pre-parsed (see
/// [`IcosaPipeline::new`]'s doc for why) and is turned into a module directly at its call site.
///
/// # Errors
/// Returns an error if the bytes are not valid SPIR-V framing (`read_spv` checks the magic number
/// and that the length is a whole number of 32-bit words) or if `vkCreateShaderModule` fails.
///
/// # Safety
/// `device` must be live. The returned module must be destroyed before the device is.
unsafe fn create_shader_module_from_bytes(
    device: &ash::Device,
    spv: &[u8],
) -> anyhow::Result<vk::ShaderModule> {
    // SPIR-V is a stream of 32-bit words, but `include_bytes!` gives bytes with no alignment
    // guarantee. `read_spv` copies them into a properly aligned `Vec<u32>`, which is what Vulkan
    // requires — casting the byte pointer in place would be undefined behaviour on the unlucky day
    // the literal is not 4-byte aligned.
    let mut cursor = std::io::Cursor::new(spv);
    let words = ash::util::read_spv(&mut cursor)?;
    let info = vk::ShaderModuleCreateInfo::default().code(&words);
    // SAFETY: the caller guarantees `device` is live; `words` outlives the call.
    Ok(unsafe { device.create_shader_module(&info, None) }?)
}
