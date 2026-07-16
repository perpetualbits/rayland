//! [`MappedBuffer`]: a Vulkan buffer backed by memory the CPU keeps mapped for the buffer's entire
//! lifetime.
//!
//! # Why this exists, and why both fixtures use it
//! `rayland-refapp`'s `HostBuffer` maps, writes, and unmaps once, because refapp renders a single
//! frame and exits. Every fixture built on this crate renders `rayland_icosa_core::FRAME_COUNT`
//! frames, writing new data into the *same* buffer every time — the CPU fixture rewrites ~1 MiB of
//! fractal texels each frame, the GPU fixture rewrites a few dozen bytes of uniforms, and this
//! crate's own [`crate::scene::Scene`] rewrites uniforms and reads back pixels every frame too.
//! Mapping and unmapping around every one of those writes would add `vkMapMemory`/`vkUnmapMemory`
//! call overhead to a loop whose entire point, for the CPU fixture, is to measure the cost of
//! writing through mapped memory — so the map happens exactly once, in [`MappedBuffer::new`], and
//! is held until [`MappedBuffer::destroy`].
//!
//! That both fixtures reach for this same type is itself part of what this crate's existence
//! proves: the GPU fixture is not somehow "avoiding" mapped memory by computing the fractal in a
//! shader — it uses the identical mapping mechanism, just for ~128 bytes of uniforms a frame
//! instead of ~1 MiB of texture. The difference the pair exists to measure is *how much* goes
//! through the mapping, not *whether* one of them uses one at all.
//!
//! # Why `HOST_COHERENT` matters as much as `HOST_VISIBLE`
//! `HOST_VISIBLE` is what makes a mapping legal at all. `HOST_COHERENT` is what makes writing
//! through it *correct* with no further ceremony: on non-coherent memory the CPU's and the GPU's
//! views of the same bytes can diverge, and a program must call `vkFlushMappedMemoryRanges` after
//! every write and `vkInvalidateMappedMemoryRanges` before every read, tracking exactly which byte
//! ranges changed, or it risks the GPU reading stale bytes. Coherent memory buys that guarantee
//! outright, and every conformant Vulkan driver is required to expose at least one host-visible,
//! host-coherent memory type, so requesting both here never fails for lack of a candidate.

// The Vulkan API surface and its handle/struct types.
use ash::vk;
// The device and the memory-allocation helper this buffer is created against.
use crate::context::VulkanContext;

/// A Vulkan buffer plus a CPU pointer into its memory, valid for as long as the buffer is.
///
/// The pointer is kept mapped from creation until [`MappedBuffer::destroy`] — see the module docs
/// for why holding the mapping open, rather than remapping per write, is the right shape for a
/// buffer that is rewritten every frame of a many-frame run.
pub struct MappedBuffer {
    /// The buffer handle — bind this at a vertex/uniform binding, or as a copy source/destination,
    /// exactly as an ordinary Vulkan buffer.
    pub buffer: vk::Buffer,
    /// The host-visible, host-coherent memory bound to `buffer`.
    memory: vk::DeviceMemory,
    /// The pointer `vkMapMemory` returned, valid for `size` bytes until this buffer is destroyed.
    /// A raw pointer, not a slice, because Rust has no way to name "a slice whose lifetime is tied
    /// to a `Drop` impl on this same struct" — [`MappedBuffer::bytes`] manufactures the slice
    /// on demand from this pointer and `size` instead.
    mapped_ptr: *mut u8,
    /// How many bytes `mapped_ptr` is valid for; the exact size requested from
    /// [`MappedBuffer::new`], not the (possibly larger) size the driver actually allocated.
    size: u64,
}

impl MappedBuffer {
    /// Create a buffer of `size` bytes with the given `usage`, backed by memory that is mapped for
    /// the lifetime of the returned `MappedBuffer`.
    ///
    /// The memory is requested `HOST_VISIBLE | HOST_COHERENT` — see the module docs for why both
    /// flags, and why that combination is guaranteed to exist.
    ///
    /// # Errors
    /// Returns an error if buffer creation, allocation, binding, or `vkMapMemory` fails.
    pub fn new(
        context: &VulkanContext,
        size: u64,
        usage: vk::BufferUsageFlags,
    ) -> anyhow::Result<MappedBuffer> {
        // SAFETY: every `ash` call below is FFI into the Vulkan driver, which trusts the caller for
        // handle validity and sizes. Each argument is constructed immediately before the call that
        // uses it, and every partially-constructed object is destroyed on its own error path so
        // nothing leaks when a later step fails.
        unsafe { MappedBuffer::new_inner(context, size, usage) }
    }

    /// The `unsafe` body of [`MappedBuffer::new`], separated so the public constructor stays safe
    /// to call and the safety argument lives in exactly one place.
    unsafe fn new_inner(
        context: &VulkanContext,
        size: u64,
        usage: vk::BufferUsageFlags,
    ) -> anyhow::Result<MappedBuffer> {
        let info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            // Only one queue family ever touches these buffers (this crate creates exactly one
            // graphics/transfer-capable queue — see `VulkanContext::new`), so exclusive ownership
            // is correct and lets the driver skip the coherence work CONCURRENT would imply.
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { context.device.create_buffer(&info, None) }?;

        let requirements = unsafe { context.device.get_buffer_memory_requirements(buffer) };
        let memory = match unsafe {
            context.allocate(
                requirements,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
        } {
            Ok(memory) => memory,
            Err(error) => {
                unsafe { context.device.destroy_buffer(buffer, None) };
                return Err(error);
            }
        };

        if let Err(error) = unsafe { context.device.bind_buffer_memory(buffer, memory, 0) } {
            unsafe {
                context.device.destroy_buffer(buffer, None);
                context.device.free_memory(memory, None);
            }
            return Err(error.into());
        }

        // Map once, for the buffer's whole lifetime — see the module docs for why this differs
        // from `rayland-refapp`'s map-write-unmap-per-use `HostBuffer`.
        let mapped_ptr = match unsafe {
            context
                .device
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
        } {
            Ok(ptr) => ptr as *mut u8,
            Err(error) => {
                unsafe {
                    context.device.destroy_buffer(buffer, None);
                    context.device.free_memory(memory, None);
                }
                return Err(error.into());
            }
        };

        Ok(MappedBuffer {
            buffer,
            memory,
            mapped_ptr,
            size,
        })
    }

    /// Borrow the mapped memory as a byte slice the caller can write into (to upload data) or read
    /// from (to read back a copy the GPU has finished writing).
    ///
    /// # Pitfall: this crate does not itself enforce "the GPU is not using this buffer right now"
    /// Nothing about this method touches Vulkan, so it cannot know whether the GPU has an
    /// in-flight command buffer that reads or writes the same bytes. Writing here while the GPU is
    /// still reading the previous frame's contents is the classic mapped-memory race, and the fix
    /// is always the same: synchronize with a fence *before* calling this, not after. See
    /// [`crate::scene::Scene::draw`]'s doc for how this crate discharges that obligation for its
    /// own uniform and readback buffers, and note that obligation transfers to any fixture that
    /// writes a `MappedBuffer` of its own (the CPU fixture's fractal texture) — that fixture must
    /// establish the same "wait before write" discipline itself, since this type has no way to
    /// enforce it on the caller's behalf.
    ///
    /// The returned slice's lifetime is tied to `&mut self`, which prevents two overlapping
    /// borrows from *this* call site, but cannot see across a Vulkan submission — the pitfall above
    /// is a runtime discipline this type's API cannot make the compiler check.
    pub fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: `mapped_ptr` was returned by `vkMapMemory` for exactly `size` bytes in `new`, and
        // stays valid until `destroy` unmaps it; `&mut self` here rules out another live borrow of
        // this same slice existing at the same time from Rust's point of view (see the doc above
        // for the limits of what that does and does not guarantee).
        unsafe { std::slice::from_raw_parts_mut(self.mapped_ptr, self.size as usize) }
    }

    /// Unmap the memory, destroy the buffer, and free the memory.
    ///
    /// # Safety
    /// `device` must be the live device this came from, and the GPU must be done with the buffer —
    /// the caller guarantees this by fence-waiting its submission before tearing down.
    pub unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: per the caller's guarantee. Unmapping before destroying is not strictly required
        // by the spec (`vkDestroyBuffer`/`vkFreeMemory` do not require an explicit unmap first),
        // but doing it explicitly, in this order, keeps this type's own bookkeeping honest about
        // when the mapping stops being valid, rather than relying on that spec detail implicitly.
        unsafe {
            device.unmap_memory(self.memory);
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
    }
}
