//! The Vulkan objects that have nothing to do with *what* is drawn: the loader, the instance, the
//! logical device, the queue, and the memory-type table.
//!
//! Split out from the drawing code because these are the parts every Vulkan program has, in very
//! nearly this form, regardless of what it renders — and keeping them separate is what lets the
//! rest of this crate be read as "the triangle" rather than "the triangle, buried in bring-up".
//!
//! # The domain pitfall this module is shaped around: destruction order
//! Vulkan has no garbage collector and no ownership tracking. Every object is a plain handle, and
//! destroying one while another still refers to it is undefined behaviour, not an error you get
//! told about. The rules that matter here are: the device must be destroyed before the instance,
//! and the loader (which owns the `dlopen`'d driver, and therefore the function pointers *both*
//! handles are called through) must outlive both. Rust's drop order for struct fields is
//! declaration order, which would destroy the loader first — exactly backwards — so [`Drop`] is
//! implemented by hand below rather than derived, and the field order is not load-bearing.

// The Vulkan API surface and its handle/struct types.
use ash::vk;

/// A ready-to-use Vulkan device: everything needed to create resources and submit work, and
/// nothing that depends on what those resources or that work are.
///
/// Created once by [`VulkanContext::new`] and torn down once, in [`Drop`]. There is no reason for
/// this program to build more than one — it renders a single frame — but keeping bring-up in a
/// type with a destructor (rather than inline in `main`) means the teardown happens in the right
/// order automatically, including on the error paths, which is precisely where hand-written
/// cleanup normally rots.
pub struct VulkanContext {
    /// The loaded Vulkan library. Never read after `new` returns, but it owns the driver's
    /// function pointers that `instance` and `device` are called through, so unloading it while
    /// either is still alive would be undefined behaviour. The leading underscore says "this field
    /// exists to control drop timing, not to be used".
    _entry: ash::Entry,
    /// The Vulkan instance. Destroyed after `device` — see [`Drop`].
    instance: ash::Instance,
    /// The logical device. Every resource this program creates comes from here, and every one of
    /// them must be destroyed before this is.
    pub device: ash::Device,
    /// The single graphics-capable queue all work is submitted to.
    pub queue: vk::Queue,
    /// The queue family `queue` belongs to; the command pool must be created against the same one.
    pub queue_family_index: u32,
    /// The physical device's memory-type table, queried once at bring-up and consulted by every
    /// allocation (see [`allocate`]). Drivers do not change this at runtime, so re-querying it per
    /// allocation would be pure waste.
    pub mem_props: vk::PhysicalDeviceMemoryProperties,
}

impl VulkanContext {
    /// Load Vulkan, pick the first GPU that can draw, and create a logical device with one
    /// graphics queue.
    ///
    /// No instance extensions and no device extensions are requested, because this program renders
    /// off-screen: it has no window, no surface, and no swapchain, and therefore needs nothing
    /// beyond core Vulkan. No validation layers either — a deliberate choice, not an oversight,
    /// since this binary is run under drivers where validation is known not to work.
    ///
    /// # Errors
    /// Returns an error if the Vulkan loader cannot be loaded (no driver installed), if no
    /// physical device exposes a graphics-capable queue family, or if instance or device creation
    /// fails.
    pub fn new() -> anyhow::Result<VulkanContext> {
        // SAFETY: every `ash` call below is FFI into the Vulkan driver, which trusts the caller to
        // pass valid handles, pointers, and sizes. Each argument is constructed immediately before
        // the call that consumes it and outlives that call, and no handle is used after being
        // destroyed. Kept as one block, rather than a dozen, so the reasoning reads once.
        unsafe { VulkanContext::new_inner() }
    }

    /// The `unsafe` body of [`VulkanContext::new`], separated so the public constructor stays safe
    /// to call and the safety argument lives in exactly one place.
    unsafe fn new_inner() -> anyhow::Result<VulkanContext> {
        // Find and open the system's Vulkan loader. Which driver this resolves to is decided
        // entirely by the environment (`VK_ICD_FILENAMES` and friends) — this program neither
        // knows nor cares which one answers, which is what makes it usable as a reference app.
        let entry = unsafe { ash::Entry::load() }?;

        // Identify the application. Drivers use this only for diagnostics and driver-specific
        // workaround matching. Vulkan 1.0 is requested because that is genuinely all this program
        // needs: an off-screen triangle uses nothing added in any later version. Asking for the
        // lowest version we can actually use is the ordinary thing an application does, and it
        // keeps the set of drivers that can run this as wide as possible.
        let app_info = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 0, 0));

        let instance_info = vk::InstanceCreateInfo::default().application_info(&app_info);
        let instance = unsafe { entry.create_instance(&instance_info, None) }?;

        // Choose the first physical device with a graphics-capable queue family. "First that
        // works" is the right policy here and not laziness: this program is a fixture whose job is
        // to render on whatever device the environment put in front of it, so any cleverness about
        // picking the *best* GPU would actively work against its purpose.
        let physical_devices = match unsafe { instance.enumerate_physical_devices() } {
            Ok(physical_devices) => physical_devices,
            Err(error) => {
                // Same reasoning as the queue-family and device-creation failures below: no
                // `VulkanContext` exists yet, so this error path owns the instance and must free it
                // itself, rather than leaving that inconsistent with its two neighbours.
                unsafe { instance.destroy_instance(None) };
                return Err(error.into());
            }
        };
        let (physical_device, queue_family_index) = physical_devices
            .iter()
            .find_map(|&pd| {
                // A queue family's flags say what kinds of work it accepts; GRAPHICS is what a
                // draw call needs. Transfer capability comes with it implicitly (the Vulkan spec
                // guarantees any graphics-capable family also supports transfer), so the readback
                // copy this program issues needs no separate family.
                unsafe { instance.get_physical_device_queue_family_properties(pd) }
                    .iter()
                    .enumerate()
                    .find(|(_, props)| props.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                    .map(|(index, _)| (pd, index as u32))
            })
            .ok_or_else(|| {
                // Clean up the instance we created before giving up: `VulkanContext` was never
                // constructed, so its `Drop` will not run and nothing else will free this.
                unsafe { instance.destroy_instance(None) };
                anyhow::anyhow!("no Vulkan device with a graphics queue was found")
            })?;

        // One queue from the chosen family. The priority is required by the API but meaningless
        // with a single queue: priorities only order contending queues against each other.
        let queue_priorities = [1.0f32];
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&queue_priorities);
        let queue_infos = [queue_info];
        let device_info = vk::DeviceCreateInfo::default().queue_create_infos(&queue_infos);
        let device = match unsafe { instance.create_device(physical_device, &device_info, None) } {
            Ok(device) => device,
            Err(error) => {
                // Same reasoning as the queue-family failure above: no `VulkanContext` exists yet,
                // so this error path owns the instance and must free it itself.
                unsafe { instance.destroy_instance(None) };
                return Err(error.into());
            }
        };
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };

        // The memory-type table, read once here and used by every later allocation.
        let mem_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };

        Ok(VulkanContext {
            _entry: entry,
            instance,
            device,
            queue,
            queue_family_index,
            mem_props,
        })
    }
}

impl Drop for VulkanContext {
    /// Destroy the device, then the instance — in that order, which Vulkan requires and Rust's
    /// automatic field-drop order would get wrong (see the module docs).
    ///
    /// Nothing here can fail or be reported: `Drop` returns nothing, and a Vulkan destroy call has
    /// no failure mode to begin with. The caller's responsibility, which this type cannot enforce,
    /// is that every object created *from* `device` has already been destroyed by the time this
    /// runs. That holds **on the success path only**: `render::render_triangle` destroys its child
    /// objects itself before returning `Ok`. On the error path it destroys nothing (a fence timeout
    /// can mean the GPU is still executing), and `main.rs`'s `run()` deliberately leaks the whole
    /// `VulkanContext` with `std::mem::forget` in that case — precisely so this `Drop` never runs
    /// while work may still be in flight.
    fn drop(&mut self) {
        // SAFETY: both handles are still live (nothing else destroys them), and by contract every
        // object created from `device` is already gone by the time this runs.
        unsafe {
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// Allocate device memory of a type that satisfies both `requirements` (which Vulkan object needs
/// it) and `flags` (what the program needs to be able to do with it, e.g. map it on the CPU).
///
/// # Why this search is necessary rather than a constant
/// Vulkan does not let a program name a memory type directly. Instead the driver publishes a table
/// of types, each with its own property flags, and every allocation must name an *index* into that
/// table. Which index means "host-visible" differs per driver, per GPU, and — very much to this
/// program's point — differs between running on a local Intel driver and running on Venus. So the
/// index must be discovered at runtime by searching the table, never hardcoded. `memory_type_bits`
/// in `requirements` is a bitmask of which of those indices are legal for this particular object;
/// an allocation that ignores it may be rejected or, worse, accepted on one driver and rejected on
/// the next.
///
/// # Errors
/// Returns an error if no memory type satisfies both constraints, or if `vkAllocateMemory` fails
/// (typically genuine exhaustion).
///
/// # Safety
/// `device` must be live, and `requirements` must have come from the very object this memory will
/// be bound to — a mismatch produces memory that is the wrong size or the wrong type for it.
pub unsafe fn allocate(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    requirements: vk::MemoryRequirements,
    flags: vk::MemoryPropertyFlags,
) -> anyhow::Result<vk::DeviceMemory> {
    // Walk the driver's memory-type table for the first index that is both permitted for this
    // object (`memory_type_bits`) and has the properties the caller asked for.
    let memory_type_index = (0..mem_props.memory_type_count)
        .find(|&index| {
            // Bit `index` of `memory_type_bits` set means "this object may use memory type
            // `index`". The shift is over `u32`, and `memory_type_count` is capped by the spec at
            // 32, so this cannot overflow the mask.
            let permitted_for_object = requirements.memory_type_bits & (1 << index) != 0;
            // And the type must actually offer everything the caller needs — `contains` is a
            // superset test, so a type with *extra* properties (e.g. also DEVICE_LOCAL) qualifies.
            let has_required_properties = mem_props.memory_types[index as usize]
                .property_flags
                .contains(flags);
            permitted_for_object && has_required_properties
        })
        .ok_or_else(|| {
            anyhow::anyhow!("no Vulkan memory type satisfies {flags:?} for this allocation")
        })?;

    let allocate_info = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(memory_type_index);
    // SAFETY: the caller guarantees `device` is live and `requirements` describes the object this
    // memory is for; `memory_type_index` was just verified legal for that object.
    Ok(unsafe { device.allocate_memory(&allocate_info, None) }?)
}
