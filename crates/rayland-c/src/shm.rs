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
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

/// Read a descriptor's `(st_dev, st_ino)` via `fstat`, for the buffer-by-token correlation key.
///
/// Returns `(0, 0)` if `fstat` fails — a value that can never match a real fd's inode, so a failure
/// degrades to "not correlatable" rather than a wrong match. For the fresh memfd this is called on,
/// `fstat` does not fail in practice.
fn fd_inode(fd: &OwnedFd) -> (u64, u64) {
    // SAFETY: a zeroed `stat` is a valid initialization target; `fstat` fully populates it on success
    // (return 0), and its fields are read only after that check. The fd is valid for the borrow.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut st) == 0 {
            (st.st_dev as u64, st.st_ino as u64)
        } else {
            (0, 0)
        }
    }
}

// Task 1 made these `pub` for exactly this caller. `create_memfd` allocates and sizes the anonymous
// shared memory; `ShmMapping` owns our `MAP_SHARED` view of it and `munmap`s on drop.
use rayland_vtest::EngineError;
use rayland_vtest::transport::{ShmMapping, create_memfd};
// Ring-findings §6's blob_id discrimination, held once for both (c)1 daemons. See
// `LocalBlob::is_application_memory`.
use rayland_vtest::venus_ring::is_application_memory;
use rayland_relay::BlobRun;

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
    /// The memfd's identity as `(st_dev, st_ino)`, captured at creation.
    ///
    /// This is the **buffer-by-token correlation key** (WP0). The descriptor this memfd backs is the
    /// exact fd Mesa's WSI later hands the Wayland compositor at `zwp_linux_buffer_params_v1.add` for
    /// a swapchain image; the C-side proxy `fstat`s that fd and matches its inode here to recover the
    /// resource id. Captured at creation because the fd is only briefly in hand — it is handed to Mesa
    /// and dropped — but the inode is stable for the memfd's whole life, which is this blob's life.
    inode: (u64, u64),
    /// C's copy of the bytes S currently holds for this blob — the baseline the C→S diff ships against.
    ///
    /// Zero-length for a Venus-internal blob (the ring, reply arena, staging pool): only the
    /// application's own memory is shipped whole C→S, so only it needs a baseline (see
    /// [`crate::blob_sync`]). For an application blob it is the blob's size, initialised to zeros —
    /// which matches S's fresh, zero-filled memfd, so the first diff ships exactly the application's
    /// initial non-zero content and the two copies agree from the first relay onward.
    baseline: Vec<u8>,
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
        // Capture the memfd's inode now, while the fd is in hand — it is the buffer-by-token key and
        // the fd is dropped immediately after this call. See the `inode` field.
        let inode = fd_inode(&fd);
        // `MAP_SHARED` is the entire point. A `MAP_PRIVATE` mapping would copy-on-write, so Mesa's
        // writes into its own mapping of the same memfd would never be visible here and the ring
        // would read stale zeros forever — a failure that looks like "the application produced no
        // commands" rather than like a mapping bug.
        let mapping = ShmMapping::map(fd.as_fd(), size)?;
        // Only the application's own memory is shipped whole C→S and therefore needs a baseline; a
        // Venus-internal blob (the ring, the reply arena) gets an empty one and is never diffed. Zeros
        // match S's fresh memfd, so the first diff ships exactly the application's initial content.
        let baseline = if is_application_memory(blob_id) {
            vec![0u8; mapping.len()]
        } else {
            Vec::new()
        };
        Ok((
            LocalBlob {
                mapping,
                size,
                blob_id,
                inode,
                baseline,
            },
            fd,
        ))
    }

    /// This blob's memfd identity `(st_dev, st_ino)`, the buffer-by-token correlation key.
    ///
    /// The C-side Wayland proxy `fstat`s the swapchain fd Mesa passes at `params.add` and matches its
    /// inode against this to recover the resource id (see the `inode` field). Returns `(0, 0)` only if
    /// the creation-time `fstat` failed, which for a fresh memfd does not happen in practice; such a
    /// value simply never matches a real fd, so it degrades to "not a tracked resource" rather than a
    /// false correlation.
    pub fn inode(&self) -> (u64, u64) {
        self.inode
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
    /// evidence behind it. It is not reimplemented here because that function is where the evidence
    /// lives, and inlining a `!= 0` at the call site would read as an arbitrary null check.
    ///
    /// # This governs C→S only
    /// Spec §7.2 retired the predicate for the return direction: S decides what to ship back by
    /// asking which pages **S wrote**, not whose memory it is. So this answer no longer has an
    /// opposite number on S that it could come to disagree with — see
    /// [`crate::blob_sync`]'s module docs for why C keeps it and S could not use it.
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

    /// The byte-runs of this blob that differ from the baseline, re-baselining as it goes — the C→S
    /// incremental sync. Empty for an unchanged blob, and empty for a Venus-internal blob (which has
    /// no baseline). See `docs/design/2026-07-25-c1-incremental-blob-sync.md`.
    ///
    /// # Why one pass, from a single read
    /// The application writes these pages with no call to intercept, so C must read the whole blob to
    /// learn what changed (there is no signal). Reading it once and using those same bytes for both the
    /// shipped run and the new baseline is what keeps C's baseline equal to what it actually sent — a
    /// second read for the baseline could see later writes and drift. A byte that matches the baseline
    /// ends the current run rather than being coalesced over, because coalescing would re-ship exactly
    /// the unchanged bytes this exists to skip (spec §"fragmentation").
    ///
    /// # Pitfall: this read is racy against Mesa, by construction
    /// A torn read (the application mid-write) ships a torn intermediate; it is transient and the next
    /// relay's diff corrects it. This is the same inherent raciness [`Self::bytes`] documents and the
    /// remote-`vkMapMemory` problem (c)2 owns — not this method's to solve.
    pub fn take_changed_runs(&mut self) -> Vec<BlobRun> {
        let len = self.mapping.len();
        // Only application blobs carry a baseline sized to the mapping; a Venus-internal blob has an
        // empty one and is never diffed.
        if self.baseline.len() != len {
            return Vec::new();
        }
        // Take a raw pointer to the mapping so the live view below borrows nothing tracked, letting us
        // mutate `self.baseline` in the same loop. The mapping and the baseline are disjoint
        // allocations (an mmap and a `Vec`), so this cannot alias.
        let ptr = self.mapping.as_ptr() as *const u8;
        // SAFETY: `ptr` addresses `len` bytes of a live `MAP_SHARED` mapping that outlives this borrow;
        // `u8` has no invalid patterns; and `live` is disjoint from `self.baseline`, so mutating the
        // baseline below does not alias it. The concurrent-writer race is documented above.
        let live: &[u8] = unsafe { std::slice::from_raw_parts(ptr, len) };
        let mut runs = Vec::new();
        let mut i = 0;
        while i < len {
            // Skip bytes that already match what S has.
            if live[i] == self.baseline[i] {
                i += 1;
                continue;
            }
            // A changed run: extend it while the bytes differ, updating the baseline to the live bytes
            // as we go so the baseline ends equal to what this run ships.
            let start = i;
            while i < len && live[i] != self.baseline[i] {
                self.baseline[i] = live[i];
                i += 1;
            }
            runs.push(BlobRun {
                offset: start as u64,
                bytes: live[start..i].to_vec(),
            });
        }
        runs
    }

    /// Fold bytes S wrote (arriving over the S→C return path) into the baseline, so the next
    /// [`Self::take_changed_runs`] does not turn around and ship S's own bytes back to S.
    ///
    /// No-op for a Venus-internal blob (no baseline). The caller [`crate::main`]'s `apply_blob_data`
    /// has already bounds-checked `offset + bytes.len()` against the blob size, and the baseline is
    /// exactly that size, so the slice below is in range.
    pub fn note_s_wrote(&mut self, offset: usize, bytes: &[u8]) {
        if self.baseline.len() != self.mapping.len() {
            return;
        }
        self.baseline[offset..offset + bytes.len()].copy_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Mapping the same memfd a second time, to play the part of Mesa.
    use std::os::fd::AsFd;

    /// The app's vertex buffer id (ring-findings §6): a non-zero `blob_id` marks application memory.
    const APP_BLOB_ID: u64 = 16;
    /// A Venus-internal shmem id: `blob_id == 0` is the ring/arena/staging pool, never diffed C→S.
    const INTERNAL_BLOB_ID: u64 = 0;

    /// The blob records the memfd's real inode, and it is the same inode a second `fstat` of the
    /// descriptor reports — the property the buffer-by-token proxy relies on. If the recorded inode
    /// drifted from the fd's, the proxy would fail to correlate the swapchain fd Mesa passes at
    /// `params.add` back to its resource, and every buffer token would be unresolved.
    #[test]
    fn the_blob_records_the_memfds_real_inode() {
        const SIZE: u64 = 4096;
        let (blob, fd) = LocalBlob::create(0, SIZE).expect("a local blob");

        // `fstat` the descriptor independently and compare against what the blob captured at create.
        // SAFETY: zeroed stat is a valid target; fstat populates it on success, read only after.
        let (dev, ino) = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            assert_eq!(libc::fstat(fd.as_fd().as_raw_fd(), &mut st), 0, "fstat the memfd");
            (st.st_dev as u64, st.st_ino as u64)
        };

        assert_eq!(blob.inode(), (dev, ino), "captured inode must match the fd's");
        assert_ne!(blob.inode(), (0, 0), "a real memfd has a nonzero inode");
    }

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
            "a reply written into a blob shadow must be visible to the application's mapping — \
             `vn_ring_wait_seqno` is released by the ring's `head`, independently of this write, so \
             a broken shadow does not hang the application; it releases it onto stale zeros, which \
             fails whatever check the reply's real value would have passed"
        );
    }

    #[test]
    fn a_fresh_app_blob_diffs_its_whole_nonzero_content_as_one_run() {
        // A fresh application blob's baseline is zeros; writing non-zero content makes every byte
        // differ, so the first diff is exactly one run covering the whole blob.
        let (mut blob, _fd) = LocalBlob::create(APP_BLOB_ID, 64).expect("an app blob");
        blob.bytes_mut().fill(0x33);
        let runs = blob.take_changed_runs();
        assert_eq!(runs.len(), 1, "a wholly-written blob is one run");
        assert_eq!(runs[0].offset, 0);
        assert_eq!(runs[0].bytes, vec![0x33; 64]);
    }

    #[test]
    fn an_unchanged_app_blob_diffs_to_nothing_after_it_was_shipped() {
        // Once a diff has run it has re-baselined; a second diff of the untouched blob ships nothing.
        let (mut blob, _fd) = LocalBlob::create(APP_BLOB_ID, 64).expect("an app blob");
        blob.bytes_mut().fill(0x33);
        let _ = blob.take_changed_runs(); // ships and re-baselines
        assert!(
            blob.take_changed_runs().is_empty(),
            "an unchanged blob must ship nothing on the next relay"
        );
    }

    #[test]
    fn a_partial_change_diffs_only_the_changed_run() {
        // After the baseline holds 0x33, changing bytes [10..20] yields one run at offset 10.
        let (mut blob, _fd) = LocalBlob::create(APP_BLOB_ID, 64).expect("an app blob");
        blob.bytes_mut().fill(0x33);
        let _ = blob.take_changed_runs();
        blob.bytes_mut()[10..20].fill(0x44);
        let runs = blob.take_changed_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].offset, 10);
        assert_eq!(runs[0].bytes, vec![0x44; 10]);
    }

    #[test]
    fn two_disjoint_changes_diff_as_two_runs() {
        // Two separated changed regions, with unchanged bytes between them, are two runs — never one
        // coalesced run, which would re-ship the unchanged bytes the diff exists to skip.
        let (mut blob, _fd) = LocalBlob::create(APP_BLOB_ID, 64).expect("an app blob");
        blob.bytes_mut().fill(0x33);
        let _ = blob.take_changed_runs();
        blob.bytes_mut()[5..10].fill(0x44);
        blob.bytes_mut()[20..25].fill(0x55);
        let runs = blob.take_changed_runs();
        assert_eq!(runs.len(), 2, "disjoint changes must not coalesce across unchanged bytes");
        assert_eq!((runs[0].offset, runs[0].bytes.len()), (5, 5));
        assert_eq!((runs[1].offset, runs[1].bytes.len()), (20, 5));
    }

    #[test]
    fn an_internal_blob_has_no_baseline_and_diffs_to_nothing() {
        // Venus's own shmems are never shipped whole C→S; they carry no baseline and diff to nothing
        // even when written, so `messages_for_delta` never ships them by this path.
        let (mut blob, _fd) = LocalBlob::create(INTERNAL_BLOB_ID, 64).expect("an internal blob");
        blob.bytes_mut().fill(0x11);
        assert!(blob.take_changed_runs().is_empty());
    }

    #[test]
    fn note_s_wrote_keeps_the_baseline_so_c_does_not_ship_s_bytes_back() {
        // The return-path fold: when S writes a blob and sends it back, C applies it to the mapping AND
        // folds it into the baseline. The next C→S diff must then see no difference and ship nothing —
        // otherwise C would ship S's own bytes straight back to S (the last-writer-wins wobble).
        let (mut blob, _fd) = LocalBlob::create(APP_BLOB_ID, 64).expect("an app blob");
        // Simulate `apply_blob_data`: S's bytes land in the mapping and in the baseline together.
        blob.bytes_mut().fill(0x77);
        blob.note_s_wrote(0, &[0x77; 64]);
        assert!(
            blob.take_changed_runs().is_empty(),
            "C must not re-ship the bytes S itself wrote"
        );
    }
}
