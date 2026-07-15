//! [`HostBlob`]: S's own writable view of a blob resource's shared pages.
//!
//! # Why S maps the blob at all, when it is S's engine that allocated it
//! On one machine this type would not exist. Venus's design is that the client `mmap`s the host's
//! pages and writes commands straight into them, and the host's ring thread reads the same physical
//! memory — no copy, no message (ring-findings §2.1). Across a network there is no shared page, so
//! **S has to write those pages itself**, on the client's behalf, from the bytes `rayland-c` relays.
//! To do that it needs a mapping of the memory its own engine allocated.
//!
//! That mapping is obtainable because virglrenderer hands one out: C0 measured
//! `virgl_renderer_resource_export_blob` returning `fd_type = 3 =
//! VIRGL_RENDERER_BLOB_FD_TYPE_SHM` — **plain shared memory** (ring-findings §2.1). S maps that
//! descriptor and is then writing the very pages `vkr_ring`'s `res->u.data` points at.
//!
//! # Why this exposes raw pointers rather than `&mut [u8]`
//! These pages have another writer: virglrenderer's ring thread, which stores `head` and `status`
//! into them (`vkr_ring_store_head`, `vkr_ring.c:60-67`) while S is writing `tail` and the buffer.
//! Forming a `&mut [u8]` over the whole blob would assert exclusive access that S demonstrably does
//! not have, which is a data race in Rust's model even where the hardware would forgive it.
//! [`HostBlob::copy_in`] therefore writes through `copy_nonoverlapping`, and the control words are
//! read and written as **real atomics** (see [`crate::ring_mirror`]) rather than as bytes.
//!
//! This is a genuinely stronger position than `rayland-c`'s equivalent, but it is not, as an earlier
//! draft of this comment claimed, a *same-process* one. **S's peer is virglrenderer's ring thread,
//! and that thread runs inside the forked render-server (`vkr` proxy) subprocess** —
//! `RAYLAND_INIT_FLAGS` (`rayland-engine/src/ffi.rs`) always includes
//! `VIRGL_RENDERER_RENDER_SERVER`, and without it Venus context creation fails outright
//! (`ffi.rs`'s doc comment on that flag), so there is no code path on which the ring thread is a
//! thread in *this* process. `crates/rayland-engine/src/virgl.rs` corroborates it empirically: a
//! Venus resource is observed to be "served by the render-server *proxy*". So S's mapping and the
//! ring thread's mapping are two separate `mmap`s of the same shared-memory object, made by two
//! separate processes — precisely `rayland-c`'s situation, not its opposite, and Rust's memory
//! model has nothing to say about a peer across a process boundary any more than it does for C's
//! peer, Mesa.
//!
//! What S genuinely has that C does not is **not the absence of a gap** — it is that the gap is
//! smaller. `rayland-c/src/ring.rs` documents two: Gap 1 (Mesa uses plain, non-atomic loads/stores
//! on its side of the mapping, so C's atomics do not truly pair with anything) and Gap 2 (the Dekker
//! StoreLoad park handshake). S closes Gap 1 — virglrenderer's ring thread genuinely uses C11
//! atomics on its side (`vkr_ring_load_tail` / `vkr_ring_store_head`), so S's `AtomicU32` pairs with
//! a real atomic rather than a plain load. And Gap 2 has no analogue here because S never parks. The
//! formal hole that remains — `MAP_SHARED` coherence across two processes' mappings of the same
//! shared-memory object, honoured by every real implementation but outside what the C11/Rust
//! abstract machine promises for cross-process memory — is the same hole every lock-free
//! shared-memory IPC scheme relies on, C's included. It is benign on every real target; it is not
//! "no gap".
//!
//! The subprocess split is not an accident to route around — see `ffi.rs`'s note on
//! `VIRGL_RENDERER_RENDER_SERVER`: sandboxing the untrusted client's Vulkan away from S's own process
//! is exactly the point, given Rayland's threat model of an untrusted party driving the host GPU. So
//! the topology this module lives with is a deliberate feature of that threat model, not a surprise.

// The mapping primitive. Reused from `rayland-vtest` rather than reimplemented: it already maps
// `MAP_SHARED` + `PROT_READ | PROT_WRITE` and owns the `munmap`, which is precisely what S needs.
use rayland_vtest::EngineError;
use rayland_vtest::transport::ShmMapping;

use std::os::fd::BorrowedFd;

/// A write that would have landed outside a blob.
///
/// Both `offset` and `len` originate in a message from the network, so this is a routine refusal of
/// hostile or broken input rather than an internal invariant failing — which is why it is a typed
/// value the caller can dress up with the resource id and report, not a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a write of {len} bytes at offset {offset} does not fit the blob's {size} bytes")]
pub struct OutOfRange {
    /// The offset the message asked for.
    pub offset: u64,
    /// How many bytes it carried.
    pub len: usize,
    /// The blob's real size — the only number here that did not come off the wire.
    pub size: u64,
}

/// One blob resource, as S sees it: the engine's resource id, its size, and a live mapping of its
/// pages.
///
/// # Lifetime contract
/// The mapping is independent of the engine's resource. S maps the *descriptor* virglrenderer
/// exported, which the kernel refcounts separately — so dropping this after the resource has been
/// unref'd is safe, and so is the reverse. This is unlike the `GUEST` blob path inside
/// `rayland-engine`, where virglrenderer holds an iovec into the mapping and the ordering genuinely
/// matters; here the two mappings of the same shared-memory object are simply peers.
pub struct HostBlob {
    /// The live mapping of the blob's pages. Owns the `munmap`.
    mapping: ShmMapping,
    /// The blob's size in bytes, as the client requested it. Every bound checked against remote
    /// input is checked against this.
    size: u64,
}

impl HostBlob {
    /// Map all `size` bytes of a blob descriptor the engine exported.
    ///
    /// # Inputs / outputs
    /// - `fd`: the descriptor `RenderEngine::create_blob_resource` returned. Borrowed — `mmap`
    ///   takes its own reference to the underlying object, so the caller may drop the fd
    ///   afterwards.
    /// - `size`: the blob's size in bytes.
    /// - Returns the owning [`HostBlob`].
    ///
    /// # Failure modes
    /// [`EngineError::ShmMapFailed`] if the mapping fails or `size` does not fit this platform's
    /// address space. `size` originates from a remote peer, so that is a real check rather than a
    /// formality.
    pub fn map(fd: BorrowedFd<'_>, size: u64) -> Result<Self, EngineError> {
        let mapping = ShmMapping::map(fd, size)?;
        Ok(HostBlob { mapping, size })
    }

    /// The blob's size in bytes — the ceiling every remote-supplied offset is checked against.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The mapping's base address.
    ///
    /// Page-aligned, because that is what `mmap` returns. [`crate::ring_mirror`] relies on that:
    /// it is what makes the ring's 64-byte-aligned control words correctly aligned for the atomic
    /// accesses it performs on them.
    pub fn as_ptr(&self) -> *mut u8 {
        self.mapping.as_ptr().cast::<u8>()
    }

    /// Copy `bytes` into the blob at `offset`, refusing anything that would write outside it.
    ///
    /// # Why the bounds check is not paranoia
    /// `offset` and `bytes.len()` both arrive over the network. An unchecked copy here is a mapping
    /// overflow driven by a remote peer — the exact standard `rayland-c`'s `apply_blob_data` already
    /// holds for the mirror-image message travelling the other way.
    ///
    /// # Inputs / outputs
    /// - `offset`: byte offset within the blob to write at.
    /// - `bytes`: what to write.
    /// - Returns `Ok(())`, or [`OutOfRange`] describing what did not fit. The caller adds the
    ///   resource id, which this type has no business knowing.
    ///
    /// The arithmetic is done in `u64` **before** any `usize` cast: casting first could truncate on
    /// a 32-bit target and turn an out-of-range write into an in-range one.
    pub fn copy_in(&mut self, offset: u64, bytes: &[u8]) -> Result<(), OutOfRange> {
        let out_of_range = || OutOfRange {
            offset,
            len: bytes.len(),
            size: self.size,
        };
        // An offset chosen to wrap the addition would otherwise land back inside the blob.
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(out_of_range)?;
        if end > self.size {
            return Err(out_of_range());
        }
        // Nothing to do, and `as_ptr().add(offset)` on an empty write need not be dereferenceable.
        if bytes.is_empty() {
            return Ok(());
        }
        // SAFETY: `end <= self.size` was just established, and the mapping is `self.size` bytes of
        // live, writable memory, so `[offset, end)` is entirely within it. `offset` fits a `usize`
        // because it is less than `size`, which `ShmMapping::map` already proved fits one. The
        // destination cannot overlap `bytes`, which the caller owns elsewhere. A raw copy rather
        // than a `&mut [u8]` because virglrenderer's ring thread may be writing other parts of
        // these same pages — see the module docs.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.as_ptr().add(offset as usize),
                bytes.len(),
            );
        }
        Ok(())
    }
}
