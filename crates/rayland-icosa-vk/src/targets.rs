//! The concrete, sized render targets a frame is drawn into: the colour image, the depth image, and
//! the framebuffer that binds both to a render pass.
//!
//! Modelled on `rayland-refapp`'s `render.rs::ColorTarget`, with a [`DepthTarget`] added alongside
//! it — this crate's colour target needs no changes from refapp's beyond taking a two-attachment
//! render pass, but the depth target is genuinely new: refapp never allocated a depth image, because
//! this repository never had one before this crate (see `pipeline.rs`'s module docs).
//!
//! # Why these are held for a `Scene`'s whole lifetime, not created per draw
//! `rayland-refapp` creates its `ColorTarget` once and renders once, so "per draw" and "once" were
//! the same thing there. A fixture built on this crate renders `rayland_icosa_core::FRAME_COUNT`
//! frames at the same size, so creating a fresh image, view, and framebuffer for every single frame
//! would be 120× the allocation churn for zero benefit — nothing about the target's size or format
//! ever changes between frames. [`crate::scene::Scene`] creates one [`Targets`] in
//! [`crate::scene::Scene::new`] and reuses it for every [`crate::scene::Scene::draw`] call.

// The Vulkan API surface and its handle/struct types.
use ash::vk;
// The device, queue, and memory-allocation helper every object below is created against.
use crate::context::VulkanContext;
// The render pass these targets' framebuffer is bound to, and the two attachment formats.
use crate::pipeline::{COLOR_FORMAT, DEPTH_FORMAT, IcosaPipeline};

/// The colour image the solid is drawn into and then copied out of, plus its view.
///
/// Deliberately `OPTIMAL`-tiled rather than a host-mappable `LINEAR` image — see
/// `rayland-refapp`'s `render.rs` module docs for the row-pitch trap that tiling choice avoids.
/// This crate sidesteps it the identical way: draw into an unmappable `OPTIMAL` image, then
/// `vkCmdCopyImageToBuffer` into a buffer this crate controls the layout of (see
/// [`crate::scene::Scene::draw`]).
pub(crate) struct ColorTarget {
    /// The `OPTIMAL`-tiled image the solid is drawn into and then copied out of.
    pub image: vk::Image,
    /// The device memory backing `image`.
    memory: vk::DeviceMemory,
    /// A view over the whole of `image`; the framebuffer needs one, the image itself will not do.
    pub view: vk::ImageView,
}

impl ColorTarget {
    /// Create the colour image, allocate and bind its memory, and build its view.
    ///
    /// The image is created with `COLOR_ATTACHMENT` (the render pass draws into it) and
    /// `TRANSFER_SRC` (the readback copies out of it). Both are required: Vulkan rejects an image
    /// used in a way its `usage` flags did not declare, and forgetting `TRANSFER_SRC` here is a
    /// failure that surfaces confusingly far away, at the copy.
    ///
    /// # Errors
    /// Returns an error if image creation, allocation, binding, or view creation fails.
    ///
    /// # Safety
    /// `context` must be live; the returned target must be destroyed via [`ColorTarget::destroy`]
    /// before the device is.
    unsafe fn new(context: &VulkanContext, width: u32, height: u32) -> anyhow::Result<ColorTarget> {
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(COLOR_FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                // A 2-D image still has a depth (in the "third dimension" sense, unrelated to the
                // depth *buffer*), and it must be exactly 1; 0 is a validation error.
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            // OPTIMAL: let the driver lay the pixels out however its hardware likes — see this
            // type's doc for why that is what makes the row-pitch trap unreachable.
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
            // Matches the render pass's `initial_layout`; the clear overwrites everything anyway.
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: the caller guarantees the device is live; `image_info` outlives the call.
        let image = unsafe { context.device.create_image(&image_info, None) }?;

        // DEVICE_LOCAL: this image is only ever touched by the GPU. Nothing on the CPU ever maps
        // it — that is what the readback buffer is for.
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
            .format(COLOR_FORMAT)
            .subresource_range(vk::ImageSubresourceRange {
                // The colour plane — the only one a colour format has.
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

        Ok(ColorTarget {
            image,
            memory,
            view,
        })
    }

    /// Destroy the view, the image, and free its memory, in that reverse-creation order.
    ///
    /// # Safety
    /// `device` must be the live device these came from, and the GPU must be done with them (the
    /// caller guarantees this by fence-waiting its submission before tearing down).
    unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: per the caller's guarantee; reverse creation order respects the references.
        unsafe {
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// The depth image the solid's depth test writes into, plus its view.
///
/// New relative to `rayland-refapp`, which never allocated a depth image (see this module's docs).
/// Unlike [`ColorTarget`], there is no readback path here at all: nothing in this crate ever reads
/// the depth buffer back to the CPU, so it needs no `TRANSFER_SRC` usage and no host-visible
/// counterpart — it exists purely so the pipeline's depth test (see `pipeline.rs`) has somewhere to
/// write and compare against.
pub(crate) struct DepthTarget {
    /// The `OPTIMAL`-tiled image the depth test reads and writes every draw.
    image: vk::Image,
    /// The device memory backing `image`.
    memory: vk::DeviceMemory,
    /// A view over the whole of `image`, with the `DEPTH` aspect — a depth format has no colour or
    /// stencil plane to view instead.
    pub view: vk::ImageView,
}

impl DepthTarget {
    /// Create the depth image, allocate and bind its memory, and build its view.
    ///
    /// # Errors
    /// Returns an error if image creation, allocation, binding, or view creation fails.
    ///
    /// # Safety
    /// `context` must be live; the returned target must be destroyed via [`DepthTarget::destroy`]
    /// before the device is.
    unsafe fn new(context: &VulkanContext, width: u32, height: u32) -> anyhow::Result<DepthTarget> {
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(DEPTH_FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            // Only DEPTH_STENCIL_ATTACHMENT: no TRANSFER_SRC, because nothing ever copies this
            // image out — see this type's doc.
            .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { context.device.create_image(&image_info, None) }?;

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
            .format(DEPTH_FORMAT)
            .subresource_range(vk::ImageSubresourceRange {
                // DEPTH, not COLOR: a depth-only format (no stencil bits) has exactly this one
                // aspect, and asking for the wrong aspect mask is a validation error, not a no-op.
                aspect_mask: vk::ImageAspectFlags::DEPTH,
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

        Ok(DepthTarget {
            image,
            memory,
            view,
        })
    }

    /// Destroy the view, the image, and free its memory, in that reverse-creation order.
    ///
    /// # Safety
    /// `device` must be the live device these came from, and the GPU must be done with them.
    unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: per the caller's guarantee; reverse creation order respects the references.
        unsafe {
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// The colour target, the depth target, and the framebuffer binding both to a render pass at one
/// concrete size.
///
/// Bundled into one type because a framebuffer is created from *both* views at once — Vulkan has no
/// notion of attaching a depth image to an already-built framebuffer — so the three objects are
/// created together, and torn down together for the same reason [`crate::pipeline::IcosaPipeline`]'s
/// pieces are.
pub(crate) struct Targets {
    pub color: ColorTarget,
    pub depth: DepthTarget,
    /// Binds `color.view` (attachment 0) and `depth.view` (attachment 1) to `pipeline.render_pass`
    /// at this frame's exact width and height — the same attachment order the render pass was built
    /// with in `pipeline.rs::create_render_pass`.
    pub framebuffer: vk::Framebuffer,
}

impl Targets {
    /// Create the colour target, the depth target, and the framebuffer that binds them to
    /// `pipeline`'s render pass.
    ///
    /// # Errors
    /// Returns an error if either target or the framebuffer fails to build.
    ///
    /// # Safety
    /// `context` and `pipeline` must be live; the returned value must be destroyed via
    /// [`Targets::destroy`] before the device is.
    pub(crate) unsafe fn new(
        context: &VulkanContext,
        pipeline: &IcosaPipeline,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Targets> {
        let color = unsafe { ColorTarget::new(context, width, height) }?;
        let depth = match unsafe { DepthTarget::new(context, width, height) } {
            Ok(depth) => depth,
            Err(error) => {
                unsafe { color.destroy(&context.device) };
                return Err(error);
            }
        };

        // Attachment order must match `create_render_pass`'s: colour at index 0, depth at index 1.
        let attachments = [color.view, depth.view];
        let framebuffer_info = vk::FramebufferCreateInfo::default()
            .render_pass(pipeline.render_pass)
            .attachments(&attachments)
            .width(width)
            .height(height)
            // One layer: a plain 2-D pair of images, no array layers, no multiview.
            .layers(1);
        let framebuffer =
            match unsafe { context.device.create_framebuffer(&framebuffer_info, None) } {
                Ok(framebuffer) => framebuffer,
                Err(error) => {
                    unsafe {
                        depth.destroy(&context.device);
                        color.destroy(&context.device);
                    }
                    return Err(error.into());
                }
            };

        Ok(Targets {
            color,
            depth,
            framebuffer,
        })
    }

    /// Destroy the framebuffer, then both targets, in that reverse-creation order — the framebuffer
    /// refers to both views, and Vulkan does not check.
    ///
    /// # Safety
    /// `device` must be the live device these came from, and the GPU must be done with them (the
    /// caller guarantees this by fence-waiting its submission before tearing down).
    pub(crate) unsafe fn destroy(&self, device: &ash::Device) {
        unsafe {
            device.destroy_framebuffer(self.framebuffer, None);
            self.depth.destroy(device);
            self.color.destroy(device);
        }
    }
}
