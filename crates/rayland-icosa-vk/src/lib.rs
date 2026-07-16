//! The Vulkan scaffolding the two icosahedron fixtures share: bring-up, the depth-tested render
//! pass and pipeline, the render targets, the persistent host mapping, and the readback.
//!
//! # Why this exists as a library the fixtures both depend on, rather than being written twice
//! The two fixtures — one computing its fractal texture on the CPU and uploading it, one computing
//! it in a fragment shader — exist to be *compared*. A comparison between two programs is only
//! informative if the two are identical in everything except the single property under study.
//! Hand-maintaining two copies of a Vulkan render loop would not hold that: someone would fix a
//! rounding detail, an off-by-one in a barrier, or a clear value in one copy and not remember to
//! touch the other, and from that moment on any difference between the two fixtures' output could
//! be either the thing being measured or an accidental divergence in scaffolding — and there would
//! be no way to tell which from the outside. Putting every piece both fixtures must agree on in one
//! crate makes that class of drift structurally impossible rather than merely discouraged.
//!
//! # What is deliberately *not* here
//! The frame loop, the per-fixture CSV/timing output, and the texture upload path. Those are
//! exactly the places the two fixtures differ or tell their own story; see [`scene`]'s module docs
//! for the full argument.
//!
//! # This crate never mentions remoting
//! No `rayland-*` dependency beyond `rayland-icosa-core` (pure mathematics, no GPU), no mention of
//! Venus, vtest, virglrenderer, sockets, or remoting, and no environment probing beyond what
//! `ash::Entry::load()` itself does to find a Vulkan driver. The two fixtures' value as a comparison
//! rests on their being unable to tell whether their rendering is happening locally or is being
//! remoted — the identical property `rayland-refapp` rests on for C0's proof — and that property
//! has to hold for their shared scaffolding too, or it would not really hold for the fixtures built
//! on it.
//!
//! # This is the repository's first depth attachment
//! See [`pipeline`]'s module docs for why `D32_SFLOAT` needs no format negotiation, and
//! [`pipeline::create_render_pass`]-adjacent docs for the depth attachment and depth-stencil
//! pipeline state that make depth testing take effect.

// Vulkan bring-up: the loader, the instance, the logical device, the queue, and the memory-type
// table. Nothing here knows what will be drawn.
mod context;
// A Vulkan buffer whose memory stays mapped for its whole lifetime — the mechanism both fixtures
// write their per-frame data through.
mod mapped;
// The render pass and the graphics pipeline: depth-tested, with a three-attribute vertex layout and
// a descriptor set.
mod pipeline;
// The concrete, sized render targets — the colour image, the depth image, and the framebuffer that
// binds both to a render pass.
mod targets;
// The vertex buffer, the uniforms, the descriptor set, the command buffer, and the fence a fixture
// actually drives, behind `Scene::draw`.
mod scene;

pub use context::VulkanContext;
pub use mapped::MappedBuffer;
pub use pipeline::SamplerBinding;
pub use scene::{Scene, Uniforms, write_png};
