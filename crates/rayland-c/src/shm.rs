//! **Local blob shadows**: the shared memory `rayland-c` hands Mesa so that a stock, unmodified
//! Venus ICD believes it is talking to an ordinary local vtest host.
//!
//! # The idea (c)1 rests on, in one paragraph
//! Mesa's Venus ICD hardcodes its shmem type to `HOST3D` on the vtest backend
//! (`vn_renderer_vtest.c:1055`), which means: *the host allocates the memory, and the client maps
//! the host's pages*. Ring-findings §2.1 traces the consequence — the client asks for a blob, the
//! host allocates it and passes the descriptor back over `SCM_RIGHTS`, the client `mmap`s it, and
//! from then on both processes write the same physical pages with a bare `memcpy`, with no protocol
//! message involved and none required. That is why the vtest socket carries 0% of the application's
//! commands.
//!
//! `SCM_RIGHTS` is a Unix-domain socket feature and cannot cross a network; there is no such thing
//! as a page shared between two machines. So the naive plan — forward the descriptor to S — is not
//! merely hard, it is impossible. **The insight is that the vtest protocol lets *us* be the host.**
//! `rayland-c` runs on the same machine as the application, so it can allocate a perfectly ordinary
//! local memfd, pass that descriptor over a perfectly ordinary local socket, and let Mesa map it.
//! Mesa gets exactly the coherent shared memory its design assumes, from a host that happens to be
//! us. It cannot tell the difference, and it needs no fork and no patch. What crosses the network is
//! then *bytes we copied out of those pages* — which is what the rest of this crate is about.
//!
//! # Why this module reuses `rayland-vtest`'s primitives rather than reimplementing them
//! (c)1 Task 1 made `create_memfd` and `ShmMapping` public precisely because it found that the vtest
//! `GUEST` blob path — host allocates a memfd, client maps it, host reads the pages — is *exactly*
//! the shape `rayland-c` needs. The mechanics were already written, reviewed and covered by tests;
//! duplicating them here would create a second copy of the same `unsafe` to keep correct.
//!
//! # The lifecycle pitfall this module exists to make structural
//! The fd and the mapping have **different lifetimes**, and getting that wrong is a use-after-free:
//!
//! - The **fd** may be closed the instant it has been sent to Mesa. The kernel duplicates it into
//!   the receiving process, and — as virglrenderer's own comment at this exact step puts it —
//!   "closing the file descriptor does not unmap the region".
//! - The **mapping** must outlive every reader of the pages. `rayland-c`'s ring watcher reads
//!   command bytes straight out of this mapping for the resource's whole lifetime; unmapping it
//!   early would leave that reader walking freed address space, driven by an untrusted application's
//!   command stream.
//!
//! `ShmMapping`'s doc comment states that invariant, but a doc comment one crate away from its
//! caller is not enforcement — and this module is that caller. [`LocalBlob`] is the enforcement:
//! it owns the mapping, and hands the pages out only as a slice borrowed from `&self`. The borrow
//! checker then makes "read the pages after the mapping is gone" not a bug to be avoided but a
//! program that does not compile.

// The blob's client-facing descriptor, and the borrow `ShmMapping::map` takes (it keeps its own
// reference to the underlying object, so the fd may be closed afterwards).
use std::os::fd::{AsFd, OwnedFd};

// Task 1 made these `pub` for exactly this caller. `create_memfd` allocates and sizes the anonymous
// shared memory; `ShmMapping` owns our `MAP_SHARED` view of it and `munmap`s on drop.
use rayland_vtest::EngineError;
use rayland_vtest::transport::{ShmMapping, create_memfd};
// Ring-findings §6's blob_id discrimination, held once for both (c)1 daemons. See
// `LocalBlob::is_application_memory`.
use rayland_vtest::venus_ring::is_application_memory;

/// One blob resource's **local** shared memory: the pages Mesa maps and writes, and that
/// `rayland-c` reads in order to relay their contents to S.
///
/// # What this is a shadow *of*
/// Every blob has two allocations that are deliberately **not** the same memory: this local one,
/// which exists so Mesa's `mmap` succeeds and its `memcpy`s land somewhere real, and a GPU-backed
/// one on S, which exists so virglrenderer has something to read. On one machine those would be a
/// single shared page and the whole problem would vanish. Across a network they cannot be, and
/// keeping them in step *is* (c)1: [`rayland_relay::C2S::RingDelta`] and
/// [`rayland_relay::C2S::BlobData`] carry C→S, and [`rayland_relay::S2C::BlobData`] carries S→C.
///
/// # Ownership
/// The mapping lives as long as this value and is unmapped exactly once, on drop. The descriptor is
/// **not** kept: [`LocalBlob::create`] hands it back to the caller, which sends it to Mesa and drops
/// it immediately, matching virglrenderer's own vtest server. Keeping it would pin a descriptor for
/// the whole session for no benefit — the mapping does not need it.
pub struct LocalBlob {
    /// Our `MAP_SHARED` view of the pages Mesa also maps. Owns the mapping; `munmap`s on drop.
    ///
    /// Private, and deliberately so: the only ways to reach these bytes are [`LocalBlob::bytes`]
    /// and [`LocalBlob::bytes_mut`], both of which tie the resulting slice's lifetime to `self`.
    /// Exposing the raw pointer would hand callers back the exact use-after-free this type exists
    /// to make unrepresentable.
    mapping: ShmMapping,
    /// The blob's size in bytes, as Mesa requested it. Equal to `mapping.len()`; kept because it is
    /// the number the wire protocol speaks in, and `u64` is the type it travels as.
    size: u64,
    /// The client-chosen blob id from `VCMD_RESOURCE_CREATE_BLOB`.
    ///
    /// Kept because it is the **only clean signal** separating the application's own memory from
    /// Venus's internal plumbing (ring-findings §6), and (c)1's blob synchronisation routes on
    /// exactly that: see [`LocalBlob::is_application_memory`]. It is recorded at creation because it
    /// is never recoverable afterwards — nothing else on the wire or in the pages carries it.
    blob_id: u64,
}

impl LocalBlob {
    /// Allocate `size` bytes of local shared memory for a blob Mesa asked for, and produce both our
    /// lasting view of it and the descriptor Mesa must receive.
    ///
    /// # Inputs / outputs
    /// - `blob_id`: the client-chosen blob id, straight from the client's
    ///   `VCMD_RESOURCE_CREATE_BLOB`. Not used for the allocation itself — it is recorded so that
    ///   [`LocalBlob::is_application_memory`] can answer later; see that method for why it matters.
    /// - `size`: the blob size in bytes, straight from the client's `VCMD_RESOURCE_CREATE_BLOB`.
    ///   Untrusted input, so it is bounded by the syscalls themselves rather than assumed sane:
    ///   `create_memfd` fails on a size that cannot be an `off_t`, and `ShmMapping::map` fails on
    ///   one that cannot be a `usize` or that the kernel will not map.
    /// - Returns `(blob, fd)`. The **caller owns `fd`** and must send it to Mesa and then drop it;
    ///   the kernel duplicates it into the client, so dropping our copy neither closes the client's
    ///   nor unmaps anything. The `blob` must be kept for as long as the resource exists.
    ///
    /// # Failure modes
    /// - [`EngineError::ShmCreateFailed`] — `memfd_create` or the `ftruncate` that gives the object
    ///   its length failed. The `ftruncate` is not optional: a live client `mmap`s `size` bytes, and
    ///   touching a page past a memfd's end raises `SIGBUS`, so an unsized memfd would crash the
    ///   application the instant it wrote its first Venus command.
    /// - [`EngineError::ShmMapFailed`] — `mmap` failed, so there is no view to read the client's
    ///   commands through and the resource cannot be served at all.
    pub fn create(blob_id: u64, size: u64) -> Result<(Self, OwnedFd), EngineError> {
        // Anonymous, path-less, self-cleaning shared memory: the object lives exactly as long as
        // some fd or mapping refers to it, which is precisely the lifetime we want.
        let fd = create_memfd(size)?;
        // `MAP_SHARED` is the entire point. A `MAP_PRIVATE` mapping would copy-on-write, so Mesa's
        // writes into its own mapping of the same memfd would never be visible here and the ring
        // would read stale zeros forever — a failure that looks like "the application produced no
        // commands" rather than like a mapping bug.
        let mapping = ShmMapping::map(fd.as_fd(), size)?;
        Ok((
            LocalBlob {
                mapping,
                size,
                blob_id,
            },
            fd,
        ))
    }

    /// The blob's size in bytes, as Mesa requested it.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Whether this blob is the **application's own memory** rather than one of Venus's internal
    /// shmems — i.e. whether (c)1 must ship its contents across the network.
    ///
    /// Delegates to [`rayland_vtest::venus_ring::is_application_memory`], which holds the
    /// repository's single copy of ring-findings §6's `blob_id` discrimination and documents the
    /// evidence behind it. It is not reimplemented here: `rayland-s` asks the same question of its
    /// own blobs, and the two ends disagreeing about which memory belongs to whom would corrupt
    /// whichever side lost the argument.
    pub fn is_application_memory(&self) -> bool {
        is_application_memory(self.blob_id)
    }

    /// The blob's pages, for reading — the ring's control words and command buffer, or an
    /// application buffer's contents.
    ///
    /// # Why the returned lifetime is the safety property, not a formality
    /// The slice borrows `&self`, so it cannot outlive the [`LocalBlob`] and therefore cannot
    /// outlive the mapping. That is what turns `ShmMapping`'s "the mapping must outlive its readers"
    /// invariant from a doc comment someone must remember into something the compiler checks.
    ///
    /// # Pitfall: these bytes are written by another process, concurrently
    /// Mesa `memcpy`s into these pages with no lock and no notification. Reading them is therefore
    /// *inherently* racy, and it is the ring protocol — not this slice — that makes it safe:
    /// [`crate::ring::RingWatcher::take_delta`] reads `tail` first and then only reads bytes below
    /// it, a range Mesa has finished writing and will not touch again until `head` frees it. Do not
    /// read these bytes outside that discipline, and see [`crate::ring`]'s module docs for the
    /// memory-ordering obligation that discipline still owes on weakly-ordered targets.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `mapping` is a live `MAP_SHARED` mapping of exactly `len()` bytes, created by
        // `ShmMapping::map` and unmapped only when `self` drops — so the pointer is valid for the
        // whole of the returned slice's lifetime, which is bounded by `&self`. `u8` has no
        // alignment requirement and no invalid bit patterns, so any byte the client writes is a
        // valid `u8`. The concurrent-writer caveat is a data race in the abstract model, not an
        // aliasing or validity violation, and it is what the ring protocol above governs.
        unsafe {
            std::slice::from_raw_parts(self.mapping.as_ptr() as *const u8, self.mapping.len())
        }
    }

    /// The blob's pages, for writing — the ring's `head` and `status` words, and the reply-arena
    /// bytes S sends back for the application to read.
    ///
    /// `&mut self` is not merely conventional here: it is what stops a reader and a writer of the
    /// same mapping from coexisting *on this side*. It says nothing about Mesa, which writes these
    /// pages whenever it likes — see [`LocalBlob::bytes`] for the discipline that governs that.
    ///
    /// # Pitfall: only some of these bytes are C's to write
    /// The ring's `head` and `status` are written by the consumer (us) and read by Mesa; `tail` and
    /// the command buffer are Mesa's and must never be written here. Writing Mesa's words would
    /// corrupt its view of its own ring in a way it has no way to detect.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `bytes`, plus `&mut self` guarantees no other slice into this mapping is live
        // on this side for the returned slice's lifetime. The mapping is `PROT_READ | PROT_WRITE`.
        unsafe {
            std::slice::from_raw_parts_mut(self.mapping.as_ptr() as *mut u8, self.mapping.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Mapping the same memfd a second time, to play the part of Mesa.
    use std::os::fd::AsFd;

    /// **The inherited invariant, made a test rather than a doc comment.**
    ///
    /// (c)1 Task 1's review named this as `rayland-c`'s sharpest inherited risk: `ShmMapping`'s
    /// lifecycle rule — the fd may be closed early, but the mapping must outlive its readers — is
    /// enforced only by prose, one crate away from its caller, and this crate is that caller. The
    /// rule is not academic: `LocalBlob::create` returns the fd expressly so it can be sent to Mesa
    /// and dropped, exactly as virglrenderer's vtest server does, so **every** blob `rayland-c`
    /// serves runs with a closed fd and a live mapping. If closing the fd tore the mapping down,
    /// the ring watcher would read freed address space on the very first blob.
    ///
    /// This test proves all three halves of the arrangement at once, and none of them by
    /// assumption:
    /// 1. the mapping survives the fd being dropped ("closing the fd does not unmap the region"),
    /// 2. it is genuinely `MAP_SHARED` — a `MAP_PRIVATE` mapping would pass any "can I write to it"
    ///    check and silently fail here,
    /// 3. writes made by a *different* mapping of the same object — which is precisely what Mesa is
    ///    — are visible through `bytes()`.
    ///
    /// Point 3 is the one that matters most: it is the mechanism by which the application's Vulkan
    /// commands reach `rayland-c` at all.
    #[test]
    fn the_mapping_outlives_the_fd_and_still_sees_a_foreign_writers_bytes() {
        const SIZE: u64 = 4096;
        // `blob_id = 0`: this test plays the part of the command ring, which is one of Venus's own
        // shmems. The id is irrelevant to the mapping mechanics under test — it only classifies the
        // blob for `crate::blob_sync` — but it is passed honestly rather than arbitrarily.
        let (blob, fd) = LocalBlob::create(0, SIZE).expect("a local blob");
        assert_eq!(blob.size(), SIZE);

        // Stand in for Mesa: an independent mapping of the same object, made through the descriptor
        // before we drop it — just as the client maps the fd we send it over SCM_RIGHTS.
        let mesa_view = ShmMapping::map(fd.as_fd(), SIZE).expect("the client's own mapping");

        // Drop our descriptor, exactly where the real path drops it: after it has been handed over.
        // If the mapping's lifetime were tied to the fd, everything below would be a use-after-free.
        drop(fd);

        // "Mesa" writes a Venus command's first dword into the pages, with a bare memcpy and no
        // notification of any kind — which is all the real client does.
        let command = [0xb2u8, 0x00, 0x00, 0x00];
        // SAFETY: `mesa_view` is a live, writable mapping of at least 4 bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(command.as_ptr(), mesa_view.as_ptr() as *mut u8, 4);
        }

        // The watcher's view must show the foreign writer's bytes. This is the whole mechanism.
        assert_eq!(
            &blob.bytes()[..4],
            &command,
            "a blob shadow must see writes made through another mapping of the same memfd, with \
             the fd already closed — this is exactly how Mesa's commands reach rayland-c"
        );
    }

    /// The reverse direction, which the reply path depends on: bytes `rayland-c` writes into a blob
    /// must be visible to Mesa's mapping.
    ///
    /// This is not symmetry for its own sake. Ring-findings §7 measured the **reply arena at ~12x
    /// the command traffic** — the return path is the bulk, not the command stream — and every
    /// synchronous Vulkan call the application makes blocks until its reply appears in a blob
    /// exactly like this one. `S2C::BlobData` arrives from S and is written through `bytes_mut`; if
    /// that write were not visible to Mesa, every synchronous call would read stale zeros.
    #[test]
    fn bytes_written_through_the_shadow_are_visible_to_the_clients_mapping() {
        const SIZE: u64 = 4096;
        // `blob_id = 0`: this test plays the part of the reply arena, which is Venus-internal.
        let (mut blob, fd) = LocalBlob::create(0, SIZE).expect("a local blob");
        let mesa_view = ShmMapping::map(fd.as_fd(), SIZE).expect("the client's own mapping");
        drop(fd);

        // Stand in for a reply S sent back: `0x00404155` is the encoded Vulkan 1.4.341 that the
        // live capture actually caught in the reply arena (ring-findings §3.2).
        blob.bytes_mut()[..4].copy_from_slice(&0x0040_4155u32.to_le_bytes());

        // Read it back through the client's independent mapping.
        let mut seen = [0u8; 4];
        // SAFETY: `mesa_view` is a live mapping of at least 4 bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(mesa_view.as_ptr() as *const u8, seen.as_mut_ptr(), 4);
        }
        assert_eq!(
            u32::from_le_bytes(seen),
            0x0040_4155,
            "a reply written into a blob shadow must be visible to the application's mapping, or \
             every synchronous Vulkan call blocks forever on stale zeros"
        );
    }
}
