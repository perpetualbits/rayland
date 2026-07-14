//! Zero-copy presentation (SP3): export a Vulkan-rendered image as a Linux **dmabuf** so the
//! compositor samples GPU memory directly, with no CPU round-trip.
//!
//! A dmabuf is a kernel handle (a file descriptor) to a buffer of GPU memory. We render into a
//! LINEAR-tiled image whose memory is allocated *exportable*, export an fd with
//! `vkGetMemoryFdKHR`, and hand that fd (plus the pixel layout) to the compositor via
//! `zwp_linux_dmabuf_v1`. The image and its memory must stay alive as long as the compositor
//! holds the buffer — that lifetime is owned by [`crate::render::Renderer`] (added in Task 3).
//!
//! Pitfalls (see the code): the export image must be created with the *modifier* extension so
//! its tiling is well-defined for the compositor; the OPTIMAL→LINEAR step must be a
//! `vkCmdBlitImage` (component-semantic) not a copy, so R/B channel order lands as XRGB8888;
//! and the dmabuf layout must come from `vkGetImageSubresourceLayout`, not `width*4`.
//!
//! ## Colour-order reasoning (the crux of "blit not copy")
//! The source image the triangle renderer draws into is `R8G8B8A8_UNORM`: in memory that is
//! bytes `R,G,B,A`. The compositor, however, wants DRM fourcc `XRGB8888`, whose memory byte
//! order is `B,G,R,X`. The Vulkan format that lays out memory as `B,G,R,A` is
//! `B8G8R8A8_UNORM`, so that is the export image's format. `vkCmdBlitImage` copies by
//! *component semantics* — source red goes to destination red, green to green, blue to blue —
//! so red `R=255` in the source lands in the destination's red **component**, which for
//! `B8G8R8A8_UNORM` is the *third* byte. A byte-wise `vkCmdCopyImage` would instead move byte 0
//! (source red) into byte 0 (destination blue), swapping the channels and producing wrong
//! colours. This is why the export step must be a blit.

// ash Vulkan bindings.
use ash::vk;
// Wrap the exported raw fd in an owning handle so it is closed exactly once, on drop.
use std::os::fd::{FromRawFd, OwnedFd};

/// DRM fourcc for `XRGB8888` ('XR24'): a little-endian 0x00RRGGBB word (memory B,G,R,X). The
/// matching Vulkan format is `B8G8R8A8_UNORM`; the compositor must advertise this fourcc.
pub const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
/// The trivial "linear, row-major, no vendor tiling" DRM modifier — universally importable.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// The Vulkan format whose in-memory byte order (`B,G,R,A`) matches DRM `XRGB8888`.
///
/// Kept next to [`DRM_FORMAT_XRGB8888`] so the two never drift apart: the compositor is told
/// the DRM fourcc, and the GPU is told this Vulkan format, and they must describe the same
/// bytes. See the module docs for why this is `BGRA` and not `RGBA`.
const EXPORT_VK_FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// A rendered frame exported as a dmabuf: the fd plus everything the compositor needs to
/// interpret the memory. The fd owns a dup of the exported handle; the *backing GPU memory* is
/// owned separately (by the `Renderer`) and outlives this struct's fd.
pub struct DmabufFrame {
    /// The exported dmabuf file descriptor.
    pub fd: OwnedFd,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// DRM fourcc describing the pixel format (`DRM_FORMAT_XRGB8888`).
    pub drm_format: u32,
    /// DRM format modifier describing the tiling (`DRM_FORMAT_MOD_LINEAR`).
    pub modifier: u64,
    /// Byte offset of plane 0 within the buffer (from `vkGetImageSubresourceLayout`).
    pub offset: u32,
    /// Row stride in bytes of plane 0 (from `vkGetImageSubresourceLayout`; may exceed width*4).
    pub stride: u32,
}

/// The device extensions the dmabuf export path requires.
///
/// Returned as `&CStr` names so the `Renderer` can enable them and the probe can check them.
///
/// - `VK_KHR_external_memory_fd` provides `vkGetMemoryFdKHR`, the call that turns device
///   memory into an exportable fd.
/// - `VK_EXT_external_memory_dma_buf` makes that fd specifically a **dmabuf** (the handle type
///   the Wayland/DRM stack understands), rather than an opaque driver fd.
/// - `VK_EXT_image_drm_format_modifier` lets us create an image whose tiling is a named DRM
///   modifier (here LINEAR), so the compositor and GPU agree byte-for-byte on the layout.
pub fn required_device_extensions() -> [&'static std::ffi::CStr; 3] {
    [
        ash::khr::external_memory_fd::NAME,
        ash::ext::external_memory_dma_buf::NAME,
        ash::ext::image_drm_format_modifier::NAME,
    ]
}

/// Return true iff `physical_device` exposes all [`required_device_extensions`].
///
/// This is the GPU half of the SP3 capability probe (the compositor half lives in `window.rs`).
/// A device missing any of these cannot export a dmabuf, so the server falls back to `wl_shm`.
pub fn device_supports_dmabuf_export(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> bool {
    // Enumerate the device's supported extensions once.
    let props = match unsafe { instance.enumerate_device_extension_properties(physical_device) } {
        Ok(p) => p,
        // If we cannot even query, treat dmabuf as unavailable (the fallback handles it).
        Err(_) => return false,
    };
    // Collect the supported extension names for membership testing.
    let have: std::collections::HashSet<&std::ffi::CStr> = props
        .iter()
        // Each `extension_name` is a fixed-size NUL-padded C string; parse it back to a CStr.
        .filter_map(|e| e.extension_name_as_c_str().ok())
        .collect();
    // Every required extension must be present.
    required_device_extensions()
        .iter()
        .all(|ext| have.contains(ext))
}

/// Create a LINEAR `B8G8R8A8_UNORM` export image of `extent`, blit `src_optimal_rgba` (an
/// OPTIMAL `R8G8B8A8_UNORM` image already rendered) into it, wait for completion, and export a
/// dmabuf.
///
/// Returns the [`DmabufFrame`] **and** the Vulkan image + memory handles the caller must keep
/// alive until the compositor releases the buffer (Task 3 stores them in the `Renderer`).
///
/// # Preconditions
/// `src_optimal_rgba` must already be in `TRANSFER_SRC_OPTIMAL` layout and its contents
/// finished (the caller's render submission must have completed, or be ordered before this via
/// the same queue). It must have been created with `VK_IMAGE_USAGE_TRANSFER_SRC_BIT`. This
/// function issues its own submission on `queue` and blocks on a fence until the blit is done,
/// so on return the export image's memory is fully written and safe to export.
///
/// # Safety
/// The returned `image` / `memory` back the dmabuf fd; destroying them while the compositor
/// holds the buffer would dangle. The caller owns their lifetime. This function is `unsafe`
/// because it drives raw Vulkan handles: `device`, `queue`, `command_pool` and
/// `src_optimal_rgba` must all be valid and belong to the same device, and `queue` must be
/// idle enough to accept a blocking submission.
///
/// # Errors
/// Returns an error if any Vulkan step (image/memory creation, blit submit, fence wait, fd
/// export, layout query) fails, or if no memory type is suitable for an exportable image.
pub unsafe fn export_as_dmabuf(
    device: &ash::Device,
    external_memory_fd: &ash::khr::external_memory_fd::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    src_optimal_rgba: vk::Image,
    extent: vk::Extent2D,
) -> anyhow::Result<(DmabufFrame, vk::Image, vk::DeviceMemory)> {
    // --- (1) Create the LINEAR, exportable B8G8R8A8_UNORM export image ---------------------
    // The single-entry modifier list pins the tiling to LINEAR: the driver must lay the image
    // out row-major with no vendor swizzle, which is what makes the exported fd importable by
    // an arbitrary compositor. A longer list would let the driver *choose*, and we would then
    // have to read back which one it picked via vkGetImageDrmFormatModifierPropertiesEXT.
    let modifiers = [DRM_FORMAT_MOD_LINEAR];
    let mut modifier_list =
        vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
    // Declare up front that this image's memory will be exported as a dmabuf. Vulkan requires
    // the intended external handle type to be known at image-creation time (it may change the
    // allocation the driver makes), not only at allocation time.
    let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    // `DRM_FORMAT_MODIFIER_EXT` tiling means "the layout is described by a DRM modifier",
    // selected from the modifier list chained above. Usage covers both directions of the blit
    // path: TRANSFER_DST so we can blit *into* it, TRANSFER_SRC so the test (and any readback)
    // can copy *out* of it.
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(EXPORT_VK_FORMAT)
        .extent(vk::Extent3D {
            width: extent.width,
            height: extent.height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut modifier_list)
        .push_next(&mut external_info);
    let export_image = unsafe { device.create_image(&image_info, None) }?;

    // --- (2) Allocate exportable, dedicated memory and bind it -----------------------------
    // Ask the driver what this image needs, then allocate a memory block reserved *for this
    // image alone*. Dedicated allocation is the safe default for external images: several
    // drivers refuse to export a suballocated block, and it guarantees the exported fd refers
    // to exactly this image's pixels with nothing else sharing the buffer.
    let mem_req = unsafe { device.get_image_memory_requirements(export_image) };
    // Tell the allocator this fd is destined to become a dmabuf.
    let mut export_alloc = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    // Bind the allocation to this specific image (dedicated allocation).
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(export_image);
    // Prefer DEVICE_LOCAL memory so the GPU renders into it at full speed; on an integrated
    // GPU (ANV) the same heap is also host-visible, but we never rely on that here.
    let mem_type = choose_memory_type(
        mem_props,
        mem_req.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .ok_or_else(|| anyhow::anyhow!("no DEVICE_LOCAL memory type is valid for the export image"))?;
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_req.size)
        .memory_type_index(mem_type)
        .push_next(&mut export_alloc)
        .push_next(&mut dedicated);
    let export_memory = unsafe { device.allocate_memory(&alloc_info, None) }?;
    // Bind the memory before recording any command that touches the image.
    unsafe { device.bind_image_memory(export_image, export_memory, 0) }?;

    // --- (3) Record: transition → blit → transition; submit; fence-wait --------------------
    // A throwaway primary command buffer from the caller's pool; freed before we return.
    let cmd = unsafe {
        device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(command_pool)
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

    // The whole-image, single-mip colour subresource used by every barrier and blit below.
    let full_color = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };

    // Transition the freshly created export image UNDEFINED → TRANSFER_DST_OPTIMAL. UNDEFINED
    // as the old layout tells the driver it may discard any existing contents (there are
    // none), which is exactly right for a blit that overwrites every pixel.
    let to_dst = vk::ImageMemoryBarrier::default()
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(export_image)
        .subresource_range(full_color);
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_dst],
        )
    };

    // Blit the whole source image to the whole export image. Both offset pairs span (0,0,0) to
    // (width,height,1): a 1:1 copy, so the NEAREST filter never actually interpolates — it is
    // only named because a blit signature requires a filter. The component-semantic blit is
    // what performs the R8G8B8A8 → B8G8R8A8 channel reorder (see module docs).
    let layers = vk::ImageSubresourceLayers {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        base_array_layer: 0,
        layer_count: 1,
    };
    let blit = vk::ImageBlit {
        src_subresource: layers,
        src_offsets: [
            vk::Offset3D { x: 0, y: 0, z: 0 },
            vk::Offset3D {
                x: extent.width as i32,
                y: extent.height as i32,
                z: 1,
            },
        ],
        dst_subresource: layers,
        dst_offsets: [
            vk::Offset3D { x: 0, y: 0, z: 0 },
            vk::Offset3D {
                x: extent.width as i32,
                y: extent.height as i32,
                z: 1,
            },
        ],
    };
    unsafe {
        device.cmd_blit_image(
            cmd,
            src_optimal_rgba,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            export_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[blit],
            vk::Filter::NEAREST,
        )
    };

    // Transition the export image TRANSFER_DST_OPTIMAL → TRANSFER_SRC_OPTIMAL so a later
    // readback copy (the test) can read it. The exported dmabuf itself does not carry a Vulkan
    // layout — the compositor re-imports the LINEAR memory with its own assumptions — so this
    // transition matters only to further Vulkan use of the same image on this device.
    let to_src = vk::ImageMemoryBarrier::default()
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(export_image)
        .subresource_range(full_color);
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_src],
        )
    };
    unsafe { device.end_command_buffer(cmd) }?;

    // Submit and block until the GPU has finished the blit: the fd we export next must refer to
    // memory whose pixels are already written, or the compositor would read garbage.
    let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?;
    let cmds = [cmd];
    let submit = vk::SubmitInfo::default().command_buffers(&cmds);
    unsafe { device.queue_submit(queue, &[submit], fence) }?;
    // Wait up to ~10 seconds; a longer hang means a driver problem worth surfacing, not masking.
    unsafe { device.wait_for_fences(&[fence], true, 10_000_000_000) }?;

    // The command buffer and fence have served their purpose; free them now that the GPU is
    // idle on this work. The export image and memory deliberately survive — they back the fd.
    unsafe {
        device.destroy_fence(fence, None);
        device.free_command_buffers(command_pool, &cmds);
    }

    // --- (4) Export the dmabuf fd ----------------------------------------------------------
    // vkGetMemoryFdKHR returns a *new* fd that owns a reference to the memory; closing it does
    // not free the Vulkan allocation (the driver holds its own reference until we free_memory).
    let get_fd = vk::MemoryGetFdInfoKHR::default()
        .memory(export_memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw_fd = unsafe { external_memory_fd.get_memory_fd(&get_fd) }?;
    // Vulkan guarantees success returns a valid (>= 0) fd, but guard the invariant explicitly
    // before we hand it to `OwnedFd::from_raw_fd`, whose safety contract requires a real fd.
    anyhow::ensure!(
        raw_fd >= 0,
        "vkGetMemoryFdKHR returned an invalid fd {raw_fd}"
    );
    // Take ownership so the fd is closed exactly once when the DmabufFrame is dropped.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    // --- (5) Query the real pixel layout ---------------------------------------------------
    // The compositor needs the *driver's* row stride and plane offset, which for a LINEAR
    // modifier image can still be padded beyond width*4 for alignment. Reading them from the
    // driver (rather than assuming width*4) is the difference between a correct image and a
    // sheared one.
    //
    // Pitfall: for a `DRM_FORMAT_MODIFIER_EXT` image the layout must be queried by *memory
    // plane*, not by the colour aspect. `MEMORY_PLANE_0_EXT` names the first (and, for LINEAR
    // XRGB8888, only) memory plane; passing `COLOR` here returns a zeroed layout (stride 0) on
    // ANV — the driver treats a modifier image as having no colour aspect to describe.
    let subresource = vk::ImageSubresource {
        aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
        mip_level: 0,
        array_layer: 0,
    };
    let layout = unsafe { device.get_image_subresource_layout(export_image, subresource) };

    // --- (6) Assemble the frame ------------------------------------------------------------
    let frame = DmabufFrame {
        fd,
        width: extent.width,
        height: extent.height,
        drm_format: DRM_FORMAT_XRGB8888,
        modifier: DRM_FORMAT_MOD_LINEAR,
        // offset/stride are DeviceSize (u64); a single-plane 2D image's values fit u32 for any
        // sane extent, and the dmabuf/DRM ABI carries them as u32.
        offset: layout.offset as u32,
        stride: layout.row_pitch as u32,
    };
    Ok((frame, export_image, export_memory))
}

/// Pick a memory type index that is legal for `memory_type_bits` and carries every flag in
/// `wanted`, or `None` if the device exposes no such type.
///
/// Vulkan enumerates several memory types; a resource's `memory_type_bits` is a bitmask of
/// which indices are legal for it, and we choose the first legal one that also has all the
/// requested property flags. Factored out so [`export_as_dmabuf`] reads top-to-bottom.
fn choose_memory_type(
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    memory_type_bits: u32,
    wanted: vk::MemoryPropertyFlags,
) -> Option<u32> {
    // Scan every advertised memory type for one that is both allowed and fully-flagged.
    (0..mem_props.memory_type_count).find(|&i| {
        // Bit i set in memory_type_bits means "type i is allowed for this resource".
        let allowed = memory_type_bits & (1 << i) != 0;
        // The type must also expose every property flag we asked for.
        let has_flags = mem_props.memory_types[i as usize]
            .property_flags
            .contains(wanted);
        allowed && has_flags
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    // Raw fd inspection for the skip/valid-fd assertions.
    use std::os::fd::AsRawFd;

    /// A minimal Vulkan context that has the dmabuf export extensions enabled, standing on its
    /// own (no triangle renderer). Held together so the test can tear it down in one place.
    struct DmabufContext {
        // `entry` must outlive `instance`/`device` (it owns the loaded loader); kept to enforce
        // drop order even though it is not otherwise read.
        _entry: ash::Entry,
        instance: ash::Instance,
        device: ash::Device,
        external_memory_fd: ash::khr::external_memory_fd::Device,
        queue: vk::Queue,
        queue_family_index: u32,
        mem_props: vk::PhysicalDeviceMemoryProperties,
    }

    impl Drop for DmabufContext {
        /// Destroy the device then the instance (reverse creation order). The queue is owned by
        /// the device and needs no explicit destruction.
        fn drop(&mut self) {
            unsafe {
                self.device.destroy_device(None);
                self.instance.destroy_instance(None);
            }
        }
    }

    /// Build a minimal Vulkan instance + device that ENABLES [`required_device_extensions`], or
    /// return `None` if no physical device supports them (e.g. under lavapipe, which has no
    /// dmabuf export). Returning `None` is the signal for the test to skip cleanly rather than
    /// fail — a software rasteriser genuinely cannot do this, and that is not a bug in our code.
    fn minimal_dmabuf_context() -> Option<DmabufContext> {
        // Load the system Vulkan loader; if even that fails there is no GPU stack to test.
        let entry = unsafe { ash::Entry::load() }.ok()?;
        // Request Vulkan 1.1: it makes the external-memory *capabilities* core, so we need no
        // instance extensions to reach the device-level external_memory_fd path.
        let app_info = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 1, 0));
        let instance_info = vk::InstanceCreateInfo::default().application_info(&app_info);
        let instance = unsafe { entry.create_instance(&instance_info, None) }.ok()?;

        // Find a physical device that both has a graphics queue AND supports all three dmabuf
        // extensions. If none qualifies, clean up the instance and signal skip.
        let physical_devices = unsafe { instance.enumerate_physical_devices() }.ok()?;
        let picked = physical_devices.iter().find_map(|&pd| {
            // The device must expose the dmabuf export extensions...
            if !device_supports_dmabuf_export(&instance, pd) {
                return None;
            }
            // ...and have a graphics-capable queue family to submit the blit on.
            unsafe { instance.get_physical_device_queue_family_properties(pd) }
                .iter()
                .position(|props| props.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .map(|qf| (pd, qf as u32))
        });
        let (physical_device, queue_family_index) = match picked {
            Some(v) => v,
            None => {
                // No suitable device: destroy the instance and tell the caller to skip.
                unsafe { instance.destroy_instance(None) };
                return None;
            }
        };

        // Create the logical device with one graphics queue and the dmabuf extensions enabled.
        let priorities = [1.0f32];
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&priorities);
        let queue_infos = [queue_info];
        // Enable the three functional extensions PLUS their transitive dependency
        // `VK_KHR_image_format_list`: the Vulkan spec (VUID-vkCreateDevice-01387) requires every
        // dependency of an enabled extension to *also* appear in the enable list unless it is
        // core for the device's API version. `VK_EXT_image_drm_format_modifier` depends on
        // `VK_KHR_image_format_list`, which only became core in Vulkan 1.2; this context targets
        // 1.1, so we must list it. It is safe to enable and imposes no probe change — any device
        // exposing the modifier extension necessarily exposes its dependency too, so
        // [`required_device_extensions`] (used for capability detection) need only name the three.
        // Task 3's `Renderer` must do the same (enable this dependency, or target Vulkan 1.2+).
        let mut ext_names: Vec<*const std::os::raw::c_char> = required_device_extensions()
            .iter()
            .map(|c| c.as_ptr())
            .collect();
        ext_names.push(ash::khr::image_format_list::NAME.as_ptr());
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&ext_names);
        let device = match unsafe { instance.create_device(physical_device, &device_info, None) } {
            Ok(d) => d,
            Err(_) => {
                // Enabling the extensions failed unexpectedly; skip rather than fail.
                unsafe { instance.destroy_instance(None) };
                return None;
            }
        };
        // The device-level dispatch table for VK_KHR_external_memory_fd (vkGetMemoryFdKHR).
        let external_memory_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };
        let mem_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };

        Some(DmabufContext {
            _entry: entry,
            instance,
            device,
            external_memory_fd,
            queue,
            queue_family_index,
            mem_props,
        })
    }

    /// End-to-end proof of the export mechanics on the real GPU: create a source OPTIMAL
    /// `R8G8B8A8_UNORM` image, clear it to red (with alpha 0, since XRGB ignores alpha),
    /// export it as a LINEAR dmabuf via [`export_as_dmabuf`], read the export image back
    /// through a HOST_VISIBLE buffer, and assert the bytes are `B,G,R,X = 0,0,255,0` and the
    /// fd is valid. Skips cleanly when no device supports dmabuf export (e.g. lavapipe).
    #[test]
    fn dmabuf_export_round_trips_a_known_colour() {
        // Stand up a device with the extensions enabled, or skip if none supports them.
        let Some(ctx) = minimal_dmabuf_context() else {
            eprintln!("skipping: no device supports dmabuf export (e.g. lavapipe)");
            return;
        };
        // A modest square keeps the test fast while still exercising a real row stride.
        let extent = vk::Extent2D {
            width: 64,
            height: 64,
        };

        // One command pool for both the source setup and the readback.
        let pool = unsafe {
            ctx.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(ctx.queue_family_index),
                None,
            )
        }
        .expect("create command pool");

        // --- Source OPTIMAL R8G8B8A8_UNORM image, cleared to red -------------------------
        // TRANSFER_DST lets us clear it; TRANSFER_SRC lets export_as_dmabuf blit out of it.
        let src_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let src_image = unsafe { ctx.device.create_image(&src_info, None) }.expect("create src");
        let src_req = unsafe { ctx.device.get_image_memory_requirements(src_image) };
        let src_type = choose_memory_type(
            &ctx.mem_props,
            src_req.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .expect("device-local memory type for src");
        let src_mem = unsafe {
            ctx.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(src_req.size)
                    .memory_type_index(src_type),
                None,
            )
        }
        .expect("alloc src memory");
        unsafe { ctx.device.bind_image_memory(src_image, src_mem, 0) }.expect("bind src");

        // Record: transition src to TRANSFER_DST, clear to red, transition to TRANSFER_SRC.
        let setup_cmd = unsafe {
            ctx.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .expect("alloc setup cmd")[0];
        unsafe {
            ctx.device
                .begin_command_buffer(setup_cmd, &vk::CommandBufferBeginInfo::default())
        }
        .expect("begin setup");
        let full_color = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        // UNDEFINED → TRANSFER_DST_OPTIMAL so vkCmdClearColorImage may write it.
        let to_dst = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(src_image)
            .subresource_range(full_color);
        unsafe {
            ctx.device.cmd_pipeline_barrier(
                setup_cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            )
        };
        // Red with alpha 0: R=1,G=0,B=0,A=0. Alpha 0 makes the exported X byte 0 as expected.
        let red = vk::ClearColorValue {
            float32: [1.0, 0.0, 0.0, 0.0],
        };
        unsafe {
            ctx.device.cmd_clear_color_image(
                setup_cmd,
                src_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &red,
                &[full_color],
            )
        };
        // TRANSFER_DST_OPTIMAL → TRANSFER_SRC_OPTIMAL: export_as_dmabuf's blit reads it as src.
        let to_src = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(src_image)
            .subresource_range(full_color);
        unsafe {
            ctx.device.cmd_pipeline_barrier(
                setup_cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_src],
            )
        };
        unsafe { ctx.device.end_command_buffer(setup_cmd) }.expect("end setup");
        // Submit the source setup and wait, so the source is ready before the export blit.
        let setup_fence = unsafe {
            ctx.device
                .create_fence(&vk::FenceCreateInfo::default(), None)
        }
        .expect("setup fence");
        let setup_cmds = [setup_cmd];
        unsafe {
            ctx.device.queue_submit(
                ctx.queue,
                &[vk::SubmitInfo::default().command_buffers(&setup_cmds)],
                setup_fence,
            )
        }
        .expect("submit setup");
        unsafe {
            ctx.device
                .wait_for_fences(&[setup_fence], true, 10_000_000_000)
        }
        .expect("wait setup");

        // --- The call under test: export the source as a dmabuf --------------------------
        let (frame, export_image, export_memory) = unsafe {
            export_as_dmabuf(
                &ctx.device,
                &ctx.external_memory_fd,
                &ctx.mem_props,
                ctx.queue,
                pool,
                src_image,
                extent,
            )
        }
        .expect("export_as_dmabuf must succeed on a dmabuf-capable GPU");

        // The fd must be real (>= 0), and the frame must describe an XRGB8888 LINEAR buffer.
        assert!(
            frame.fd.as_raw_fd() >= 0,
            "exported dmabuf fd must be valid"
        );
        assert_eq!(frame.drm_format, DRM_FORMAT_XRGB8888, "format is XRGB8888");
        assert_eq!(frame.modifier, DRM_FORMAT_MOD_LINEAR, "modifier is LINEAR");
        // A LINEAR B8G8R8A8 row is at least width*4 bytes; the driver may pad it larger.
        assert!(
            frame.stride >= extent.width * 4,
            "stride {} must be at least width*4 = {}",
            frame.stride,
            extent.width * 4
        );

        // --- Read the export image back through a HOST_VISIBLE buffer ---------------------
        // vkCmdCopyImageToBuffer with bufferRowLength=0 packs the pixels tightly (width*4),
        // so the top-left pixel is bytes [0..4] regardless of the image's internal stride.
        let readback_size = (extent.width as u64) * (extent.height as u64) * 4;
        let rbuf = unsafe {
            ctx.device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(readback_size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }
        .expect("create readback buffer");
        let rreq = unsafe { ctx.device.get_buffer_memory_requirements(rbuf) };
        let rtype = choose_memory_type(
            &ctx.mem_props,
            rreq.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .expect("host-visible memory type for readback");
        let rmem = unsafe {
            ctx.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(rreq.size)
                    .memory_type_index(rtype),
                None,
            )
        }
        .expect("alloc readback memory");
        unsafe { ctx.device.bind_buffer_memory(rbuf, rmem, 0) }.expect("bind readback");

        // The export image is left in TRANSFER_SRC_OPTIMAL by export_as_dmabuf, so copy directly.
        let copy_cmd = unsafe {
            ctx.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .expect("alloc copy cmd")[0];
        unsafe {
            ctx.device
                .begin_command_buffer(copy_cmd, &vk::CommandBufferBeginInfo::default())
        }
        .expect("begin copy");
        let region = vk::BufferImageCopy::default()
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
                width: extent.width,
                height: extent.height,
                depth: 1,
            });
        unsafe {
            ctx.device.cmd_copy_image_to_buffer(
                copy_cmd,
                export_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                rbuf,
                &[region],
            )
        };
        unsafe { ctx.device.end_command_buffer(copy_cmd) }.expect("end copy");
        let copy_fence = unsafe {
            ctx.device
                .create_fence(&vk::FenceCreateInfo::default(), None)
        }
        .expect("copy fence");
        let copy_cmds = [copy_cmd];
        unsafe {
            ctx.device.queue_submit(
                ctx.queue,
                &[vk::SubmitInfo::default().command_buffers(&copy_cmds)],
                copy_fence,
            )
        }
        .expect("submit copy");
        unsafe {
            ctx.device
                .wait_for_fences(&[copy_fence], true, 10_000_000_000)
        }
        .expect("wait copy");

        // Read the top-left pixel: for B8G8R8A8 memory of red-with-alpha-0, expect B,G,R,X.
        let mapped = unsafe {
            ctx.device
                .map_memory(rmem, 0, readback_size, vk::MemoryMapFlags::empty())
        }
        .expect("map readback");
        let mut first_pixel = [0u8; 4];
        unsafe {
            std::ptr::copy_nonoverlapping(mapped as *const u8, first_pixel.as_mut_ptr(), 4);
        }
        unsafe { ctx.device.unmap_memory(rmem) };

        assert_eq!(
            first_pixel,
            [0, 0, 255, 0],
            "red XRGB8888 = B,G,R,X = 0,0,255,0 (blit must reorder R↔B, not byte-copy)"
        );
        eprintln!(
            "dmabuf export OK: fd={} {}x{} stride={} offset={} first_pixel={:?}",
            frame.fd.as_raw_fd(),
            frame.width,
            frame.height,
            frame.stride,
            frame.offset,
            first_pixel
        );

        // --- Tear down (reverse creation order). The fd closes when `frame` drops. --------
        unsafe {
            ctx.device.destroy_fence(copy_fence, None);
            ctx.device.destroy_buffer(rbuf, None);
            ctx.device.free_memory(rmem, None);
            ctx.device.destroy_image(export_image, None);
            ctx.device.free_memory(export_memory, None);
            ctx.device.destroy_fence(setup_fence, None);
            ctx.device.destroy_image(src_image, None);
            ctx.device.free_memory(src_mem, None);
            ctx.device.destroy_command_pool(pool, None);
        }
        // `frame` (and its OwnedFd) and `ctx` drop here, closing the fd and the device.
        drop(frame);
    }
}
