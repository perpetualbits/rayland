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
use rayland_relay::BlobRun;
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
    /// Whether C has published this blob to S as a [`rayland_relay::BufferToken`] — i.e. whether S
    /// turns it into a `wl_buffer` and shows it on the compositor.
    ///
    /// # Why this exists, and why it only ever *disables* something
    /// It gates one thing: whether the C→S diff may coalesce nearly-adjacent changed runs. Coalescing
    /// re-ships the unchanged bytes in between, which is safe only while `baseline` is a faithful
    /// model of S's copy — and it is faithful only because S reports every byte it writes back to C,
    /// where [`Self::note_s_wrote`] folds it in. **S makes exactly one exception: it excludes
    /// presented resources from its return path** (`Applier::presented`, added 2026-08-29 when S was
    /// found shipping ~877 KB of rendered frame per second back to a machine with no display). For
    /// those blobs S's GPU writes and never tells C, so C's baseline is stale by design, and
    /// re-shipping a gap byte would lay C's old news over S's freshly rendered pixels.
    ///
    /// Set when the proxy resolves this blob's inode for a buffer token, which is **conservative on
    /// purpose**: the resolve happens at `params.add`, before `create_immed` decides whether a token
    /// is really emitted, so this marks a superset. Marking too many blobs costs a missed
    /// optimisation; marking too few costs corrupted pixels, and only one of those is recoverable.
    /// Once set it is never cleared — a resource that has been a swapchain image does not stop having
    /// been one.
    presented: bool,
}

/// How many bytes the C→S diff compares in its **inner** pass before descending to individual bytes.
///
/// # Why there are two levels
/// A single level forces a false choice. A *small* chunk makes the scan of unchanged memory slow —
/// 13.2 MiB in 64-byte slices is 216,000 comparisons and was measured at 8.78 ms per delta on the
/// riscv64 board. A *large* chunk makes a change expensive instead: with a 4096-byte chunk, **one
/// changed byte costs a 4096-byte byte-loop**, and the Venus staging pool's changes are exactly that
/// shape — a 2026-08-31 census found 6,560 changed bytes arriving as 4,564 separate runs, i.e. almost
/// all of them isolated.
///
/// That is why skipping the four 1 MB swapchain images — 30% of the bytes walked — bought only 15% of
/// the time (8.80 → 7.51 ms, measured): those megabytes are entirely unchanged, so they were the
/// *cheapest* bytes in the walk. The expensive bytes are the few thousand that differ, each dragging a
/// whole chunk of byte-loop behind it.
///
/// So the outer pass uses [`DIFF_CHUNK`] to skip unchanged memory at `memcmp` speed, and a chunk that
/// differs is subdivided into `DIFF_SUBCHUNK` blocks before any byte is looked at individually. An
/// isolated changed byte then costs one 4096-byte `memcmp`, sixteen 256-byte `memcmp`s and a 256-byte
/// byte-loop, instead of a 4096-byte byte-loop.
///
/// **Neither constant can change what the diff produces, only how fast** — a differing block is still
/// walked byte by byte and a run straddling any boundary is still emitted whole. That equivalence is
/// what [`tests::the_chunked_diff_agrees_with_a_byte_at_a_time_reference`] asserts against a naive
/// reference, with cases derived from *both* constants so neither can be changed into vacuity.
pub(crate) const DIFF_SUBCHUNK: usize = 256;

/// How many bytes the C→S diff compares at a time before descending to individual bytes.
///
/// # Why this is a module constant and not a local one
/// The guard test [`tests::the_chunked_diff_agrees_with_a_byte_at_a_time_reference`] derives its
/// buffer size and its boundary cases **from this value**. When it was a local `const` the test used
/// a hard-coded 512-byte buffer and hand-written offsets around 64 — so raising the chunk to 4096
/// would have made the whole test buffer a single chunk, and every "straddles a boundary" case would
/// have silently stopped testing anything while still passing. That is the third time in a fortnight
/// this project has met a test that could only confirm the belief it was written from, so the
/// coupling is now structural rather than remembered.
///
/// # Why 4096, measured
/// This is the single largest term in the application's frame time, and it was found by measurement
/// on 2026-09-01, not by inspection. `messages_for_delta` diffs **every** blob on **every** ring
/// delta — 13.2 MiB for `vkcube` (an 8 MiB staging pool, a 1 MiB reply arena, four 1 MB swapchain
/// images and the rest) — roughly three times per frame. At 64 bytes that is ~216,000 slice
/// comparisons per delta, and a joined C/S stage trace put it at **9.99 ms, 51.1% of the whole
/// round trip**. S's equivalent was raised from 64 to 4096 in August (`rayland_s::blob`) and C's was
/// simply never changed with it, on the machine where it costs the most and which may be the weak
/// one.
///
/// The chunk size **cannot change what the diff produces, only how fast it produces it** — a chunk
/// that differs is still walked byte by byte, and a run straddling a boundary is still emitted whole.
/// That equivalence is what the guard test asserts, against a deliberately naive reference.
pub(crate) const DIFF_CHUNK: usize = 4096;

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
                // Nothing has been presented yet; the proxy marks this when it resolves the blob's
                // inode for a buffer token. See the field's docs for why the default is the
                // permissive one and the mark only ever restricts.
                presented: false,
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

    /// Record that this blob has been published to S as a [`rayland_relay::BufferToken`].
    ///
    /// Called by `BlobInodeResolver::resolve_inode` — the moment C decides this blob's memfd is the
    /// swapchain image behind a `zwp_linux_buffer_params_v1.add`. Idempotent, and one-way: see
    /// [`Self::presented`] for why marking early and never clearing is the safe direction.
    pub fn note_presented(&mut self) {
        self.presented = true;
    }

    /// Whether [`Self::note_presented`] has ever been called for this blob.
    ///
    /// The single consumer is [`crate::blob_sync::messages_for_delta`], which must pass a zero
    /// coalescing gap for such a blob. See [`Self::presented`] for the full argument.
    pub fn is_presented(&self) -> bool {
        self.presented
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

    /// Give this blob a zero baseline if it does not have one, so [`Self::take_changed_runs`] will
    /// diff it.
    ///
    /// # Why this is not simply part of `create`, and why it is now called for every blob
    /// It began as a door opened for one named resource by an experiment (`RAYLAND_C1_SHIP_BLOB`),
    /// and the 2026-07-26 out-of-line command stream work made it the normal case: `messages_for_delta`
    /// now calls it on **every** blob but the ring, because Venus puts any submission over 8 KiB in
    /// the staging pool (`blob_id == 0`), which the application-memory-only rule skipped. The
    /// paragraph below is why it is still a separate call rather than something `create` does.
    /// A Venus-internal blob is created with no baseline, and that empty baseline is the backstop
    /// that makes `take_changed_runs` return nothing for it (see [`crate::blob_sync`]). Opening that
    /// door is therefore a deliberate act at the call site rather than a default, and it is the
    /// *incremental* form that matters: shipping the pool's contents on every relay tripled C's blob messages and slowed the
    /// relay enough that the application never reached the submit under test, which invalidated the
    /// first attempt entirely. Diffing against a baseline makes the steady-state cost one chunked
    /// comparison and no traffic.
    ///
    /// Zero is the right initial value for the same reason it is for an application blob: S's copy is
    /// a fresh zero-filled memfd, so the first diff ships exactly the current non-zero content.
    ///
    /// # Inputs / outputs
    /// - Returns nothing; idempotent, and a no-op once a baseline of the right length exists.
    pub fn ensure_baseline(&mut self) {
        if self.baseline.len() != self.mapping.len() {
            self.baseline = vec![0u8; self.mapping.len()];
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
    /// This is also why the shipped run is copied out of **the baseline**, not out of `live`, once the
    /// inner loop below has finished writing that range into `self.baseline`: at that point the two
    /// hold identical bytes for `[start..i)`, but `live` is still `Mesa`'s mapping and may have moved on
    /// by the time the run is materialised into a `Vec`. Reading `live` a second time here would be the
    /// exact "second read" the paragraph above warns against, just moved a few lines down — the baseline
    /// would then record bytes C never actually sent, and if the application's next write happened to
    /// revert to that earlier value, the two copies would disagree forever with nothing to notice it.
    ///
    /// # Pitfall: this read is racy against Mesa, by construction
    /// A torn read (the application mid-write) ships a torn intermediate; it is transient and the next
    /// relay's diff corrects it. This is the same inherent raciness [`Self::bytes`] documents and the
    /// remote-`vkMapMemory` problem (c)2 owns — not this method's to solve.
    ///
    /// # `coalesce_gap`, and the safety argument this caller owes
    /// Runs separated by at most `coalesce_gap` unchanged bytes are merged into one, **re-shipping
    /// the unchanged bytes in between**. That trades a bounded number of redundant bytes for far
    /// fewer messages, which is the trade that matters because the forward path is message-rate
    /// bound: measured 2026-08-31, one loopback `vkcube` run sent 5,495 `BlobData` of which **5,409
    /// carried one to three bytes each**, costing C about a second inside `send()`.
    ///
    /// It is legal only where C's baseline is a faithful model of S's copy, because a gap byte is
    /// shipped *from the baseline*: if the model is right the write is a no-op, and if it is stale
    /// the write lays C's old news over S's authoritative bytes. The model is faithful because S
    /// reports every byte it writes and [`Self::note_s_wrote`] folds it in — with exactly one
    /// exception, presented resources, which S renders into and deliberately never returns. Those
    /// must be passed `0`; see [`Self::is_presented`] and
    /// [`rayland_relay::ranges::coalesce_ranges`], which holds the rule both directions share.
    ///
    /// `coalesce_gap == 0` is inert: the diff never produces adjacent ranges, so nothing merges and
    /// the output is byte-granular exactly as before.
    ///
    /// # Inputs / outputs
    /// - `coalesce_gap`: the merge threshold in unchanged bytes, per the argument above.
    /// - Returns one [`BlobRun`] per (possibly coalesced) run of bytes differing from the baseline,
    ///   ascending by offset. Empty for an unchanged blob, and empty for a blob with no baseline.
    pub fn take_changed_runs(&mut self, coalesce_gap: usize) -> Vec<BlobRun> {
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
        // Half-open `[start, end)` ranges of differing bytes, ascending and non-overlapping — the
        // shape `rayland_relay::ranges::coalesce_ranges` requires. Collected as ranges rather than
        // materialised runs so the coalescing pass below can widen one over the unchanged bytes
        // between two of them without a second walk.
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        // The start of a changed run still being extended, if any. Carried across chunk boundaries so
        // a run that straddles one is emitted as a single run, exactly as the byte-at-a-time version
        // did — the chunking below is an optimisation, and must not change what is produced.
        let mut open: Option<usize> = None;
        let mut i = 0;
        // **Compare a chunk at a time, and only descend to bytes inside a chunk that differs.**
        //
        // This is not premature optimisation; it is a fix for a measured stall. The byte-at-a-time
        // version this replaces is fine for a few hundred KiB of application buffers, and ruinous the
        // moment it is pointed at the 8 MiB command-buffer staging pool — which is exactly what
        // relaying Venus's out-of-line command streams requires. A slice comparison lowers to
        // `memcmp`, so an unchanged region costs a wide vectorised scan instead of one branch per
        // byte, and the overwhelmingly common case here is "almost nothing changed".
        //
        // This repository has now stalled the relay three separate times by reading blob pages
        // byte-at-a-time (see `docs/DIARY.md`); the shape below is the same one that fixed the
        // 637 ms critical section on S.
        const CHUNK: usize = DIFF_CHUNK;
        while i < len {
            let chunk_end = (i + CHUNK).min(len);
            // Whole chunk agrees with what S has: nothing to ship, and any run ends here.
            if live[i..chunk_end] == self.baseline[i..chunk_end] {
                if let Some(start) = open.take() {
                    ranges.push((start, i));
                }
                i = chunk_end;
                continue;
            }
            // Something in this chunk differs. **Subdivide before looking at any byte individually**:
            // the changes here are typically a handful of isolated bytes, and byte-walking the whole
            // 4096-byte chunk to find them is what the second level exists to avoid. See
            // [`DIFF_SUBCHUNK`].
            let mut s = i;
            while s < chunk_end {
                let sub_end = (s + DIFF_SUBCHUNK).min(chunk_end);
                // This block agrees with what S has: nothing to ship, and any open run ends here —
                // exactly as the outer level does, so a run straddling a sub-block boundary is still
                // emitted whole.
                if live[s..sub_end] == self.baseline[s..sub_end] {
                    if let Some(start) = open.take() {
                        ranges.push((start, s));
                    }
                    s = sub_end;
                    continue;
                }
                for j in s..sub_end {
                    if live[j] != self.baseline[j] {
                        // Updating the baseline as we go is what makes the shipped bytes come from
                        // `self.baseline` rather than a second read of `live` — see below.
                        self.baseline[j] = live[j];
                        if open.is_none() {
                            open = Some(j);
                        }
                    } else if let Some(start) = open.take() {
                        ranges.push((start, j));
                    }
                }
                s = sub_end;
            }
            i = chunk_end;
        }
        // A run still open at the end of the mapping.
        if let Some(start) = open {
            ranges.push((start, len));
        }
        // **Widen the runs over short unchanged gaps, when the caller has earned it.** The rule and
        // the argument each caller owes live in `rayland_relay::ranges::coalesce_ranges`; in this
        // direction it is that S reports every byte it writes (folded in by `note_s_wrote`) except
        // for presented resources, so a gap byte is one S already holds with that exact value and
        // writing it again is a no-op. `coalesce_gap == 0` is inert and leaves the byte grain exactly
        // as it was.
        let ranges = rayland_relay::ranges::coalesce_ranges(ranges, coalesce_gap);
        // Materialise from `self.baseline`, not from `live`. The loop above wrote every differing
        // byte into the baseline, so for a changed byte the two agree right now; for a gap byte they
        // agreed already. Reading `live` again here would be a second, later look at memory Mesa may
        // still be writing — see the doc comment above for why that reopens the exact gap this method
        // exists to close.
        ranges
            .into_iter()
            .map(|(start, end)| BlobRun {
                offset: start as u64,
                bytes: self.baseline[start..end].to_vec(),
            })
            .collect()
    }

    /// Fold bytes S wrote (arriving over the S→C return path) into the baseline, so the next
    /// [`Self::take_changed_runs`] does not turn around and ship S's own bytes back to S.
    ///
    /// No-op for a Venus-internal blob (no baseline). This has **two** call sites, both of which have
    /// already bounds-checked `offset + bytes.len()` against the blob size before calling here:
    /// `apply_blob_data` in `crate::main`, on the steady-state `S2C::BlobData` path, and
    /// `commit_pending_blob` in `crate::relay_engine`, on the `initial` runs a `S2C::BlobCreated` may
    /// carry (a readback buffer is routinely born with its finished frame already in it). Both
    /// bounds-check before calling, so the slice below is in range in ordinary operation — the
    /// `debug_assert!` exists as a named failure for a *third*, future call site that forgets to.
    ///
    /// # Panics
    /// Panics (debug builds only) if `offset + bytes.len()` exceeds the baseline's length — i.e. if a
    /// caller did not honour the bounds-checked-before-calling contract above. This is deliberately
    /// `debug_assert!`, not a `Result`: it costs nothing in release, and a violation here is a caller
    /// bug on C's own side, not remote input to be recovered from (both real callers validate remote
    /// offsets themselves, before this call, and report a bad one as S's protocol error rather than
    /// C's panic). Turning a silent index-out-of-range panic into one that names the contract it broke
    /// matters because this runs on the reader thread — the one thread that delivers every reply.
    pub fn note_s_wrote(&mut self, offset: usize, bytes: &[u8]) {
        if self.baseline.len() != self.mapping.len() {
            return;
        }
        debug_assert!(
            offset.saturating_add(bytes.len()) <= self.baseline.len(),
            "note_s_wrote: {offset}..{} is out of range for a {}-byte baseline; every caller must \
             bounds-check the run against the blob's size before calling this",
            offset.saturating_add(bytes.len()),
            self.baseline.len()
        );
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

    /// **The chunked diff must produce exactly what a byte-at-a-time diff would.**
    ///
    /// `take_changed_runs` compares 64 bytes at a time and only descends into a chunk that differs,
    /// because pointing a per-byte loop at the 8 MiB staging pool stalls the relay. That optimisation
    /// is only safe if it is invisible: runs that straddle a chunk boundary must still come out as
    /// one run, and runs that end exactly on one must not be merged with the next.
    ///
    /// So this checks the real implementation against a deliberately naive reference, over patterns
    /// chosen to sit on and across the 64-byte boundaries — which the pre-existing tests, using small
    /// buffers, never exercised.
    #[test]
    fn the_chunked_diff_agrees_with_a_byte_at_a_time_reference() {
        /// The obvious implementation, kept dumb on purpose: maximal spans where the bytes differ.
        fn reference(live: &[u8], baseline: &[u8]) -> Vec<(u64, Vec<u8>)> {
            let mut out = Vec::new();
            let mut i = 0;
            while i < live.len() {
                if live[i] == baseline[i] {
                    i += 1;
                    continue;
                }
                let start = i;
                while i < live.len() && live[i] != baseline[i] {
                    i += 1;
                }
                out.push((start as u64, live[start..i].to_vec()));
            }
            out
        }

        // Derived from the real chunk size, never hard-coded: the cases below are *about* the chunk
        // boundaries, so a buffer smaller than a few chunks would make them vacuous. Five chunks
        // gives at least four interior boundaries to straddle.
        //
        // **Deliberately NOT a whole multiple of the chunk.** The `+ 37` makes the final chunk a
        // partial one, so the loop's trailing-remainder path is exercised by every case. Mutation
        // testing caught this: with an exact multiple, an implementation that simply *dropped* a
        // trailing partial chunk passed the whole suite, because no buffer in it ever had one.
        const CHUNK: usize = DIFF_CHUNK;
        const SUB: usize = DIFF_SUBCHUNK;
        const SIZE: u64 = (CHUNK * 5 + 37) as u64;
        // Each case is a set of byte indices to change, expressed **relative to `CHUNK`** so the
        // boundary cases keep straddling a real boundary whatever the chunk size becomes.
        let cases: Vec<(&str, Vec<usize>)> = vec![
            ("nothing changed", vec![]),
            ("one byte mid-chunk", vec![10]),
            (
                "a run straddling the first boundary",
                (CHUNK - 4..CHUNK + 6).collect(),
            ),
            (
                "a run ending exactly on a boundary",
                (CHUNK - 8..CHUNK).collect(),
            ),
            (
                "a run starting exactly on a boundary",
                (CHUNK..CHUNK + 8).collect(),
            ),
            ("two runs in one chunk, one equal byte between", {
                let mut v: Vec<usize> = (100..128).collect();
                v.extend(129..160);
                v
            }),
            (
                "a run spanning several whole chunks",
                (CHUNK / 2..CHUNK * 3 + 7).collect(),
            ),
            (
                "a change in the last chunk only",
                (CHUNK * 4 + 3..CHUNK * 4 + 9).collect(),
            ),
            // The inner level's boundaries, derived from `DIFF_SUBCHUNK` for the same reason the
            // outer ones are derived from `DIFF_CHUNK`: a hand-written offset would stop straddling
            // anything the moment either constant moved.
            (
                "a run straddling a sub-block boundary",
                (SUB - 3..SUB + 5).collect(),
            ),
            (
                "a run ending exactly on a sub-block boundary",
                (SUB * 2 - 6..SUB * 2).collect(),
            ),
            (
                "a run starting exactly on a sub-block boundary",
                (SUB * 3..SUB * 3 + 6).collect(),
            ),
            (
                "two isolated bytes in different sub-blocks",
                vec![SUB + 1, SUB * 5 + 9],
            ),
            (
                "one isolated byte per sub-block across a whole chunk",
                (0..CHUNK / SUB).map(|b| b * SUB + 11).collect(),
            ),
            ("the very last byte", vec![SIZE as usize - 1]),
            ("every byte", (0..SIZE as usize).collect()),
        ];

        for (name, changed) in cases {
            let (mut blob, fd) = LocalBlob::create(APP_BLOB_ID, SIZE).expect("a local blob");
            blob.ensure_baseline();
            // Drain the initial state so the baseline matches the mapping and the case starts clean.
            let _ = blob.take_changed_runs(0);

            // Write through a second mapping of the same memfd, playing the part of Mesa.
            let mapping = ShmMapping::map(fd.as_fd(), SIZE).expect("second mapping");
            // SAFETY: `mapping` is a live MAP_SHARED mapping of exactly SIZE bytes that outlives this
            // borrow, and `u8` has no invalid patterns.
            let live_writer: &mut [u8] = unsafe {
                std::slice::from_raw_parts_mut(mapping.as_ptr() as *mut u8, SIZE as usize)
            };
            for &i in &changed {
                live_writer[i] = 0xAB;
            }

            // What the reference says, from an independent copy of the same before/after state.
            let mut baseline_copy = vec![0u8; SIZE as usize];
            let mut live_copy = vec![0u8; SIZE as usize];
            for &i in &changed {
                live_copy[i] = 0xAB;
            }
            let expected = reference(&live_copy, &mut baseline_copy);

            let got: Vec<(u64, Vec<u8>)> = blob
                .take_changed_runs(0)
                .into_iter()
                .map(|r| (r.offset, r.bytes))
                .collect();
            assert_eq!(got, expected, "case: {name}");
        }
    }

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
            assert_eq!(
                libc::fstat(fd.as_fd().as_raw_fd(), &mut st),
                0,
                "fstat the memfd"
            );
            (st.st_dev as u64, st.st_ino as u64)
        };

        assert_eq!(
            blob.inode(),
            (dev, ino),
            "captured inode must match the fd's"
        );
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
        let runs = blob.take_changed_runs(0);
        assert_eq!(runs.len(), 1, "a wholly-written blob is one run");
        assert_eq!(runs[0].offset, 0);
        assert_eq!(runs[0].bytes, vec![0x33; 64]);
    }

    #[test]
    fn an_unchanged_app_blob_diffs_to_nothing_after_it_was_shipped() {
        // Once a diff has run it has re-baselined; a second diff of the untouched blob ships nothing.
        let (mut blob, _fd) = LocalBlob::create(APP_BLOB_ID, 64).expect("an app blob");
        blob.bytes_mut().fill(0x33);
        let _ = blob.take_changed_runs(0); // ships and re-baselines
        assert!(
            blob.take_changed_runs(0).is_empty(),
            "an unchanged blob must ship nothing on the next relay"
        );
    }

    #[test]
    fn a_partial_change_diffs_only_the_changed_run() {
        // After the baseline holds 0x33, changing bytes [10..20] yields one run at offset 10.
        let (mut blob, _fd) = LocalBlob::create(APP_BLOB_ID, 64).expect("an app blob");
        blob.bytes_mut().fill(0x33);
        let _ = blob.take_changed_runs(0);
        blob.bytes_mut()[10..20].fill(0x44);
        let runs = blob.take_changed_runs(0);
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
        let _ = blob.take_changed_runs(0);
        blob.bytes_mut()[5..10].fill(0x44);
        blob.bytes_mut()[20..25].fill(0x55);
        let runs = blob.take_changed_runs(0);
        assert_eq!(
            runs.len(),
            2,
            "disjoint changes must not coalesce across unchanged bytes"
        );
        assert_eq!((runs[0].offset, runs[0].bytes.len()), (5, 5));
        assert_eq!((runs[1].offset, runs[1].bytes.len()), (20, 5));
    }

    #[test]
    fn an_internal_blob_has_no_baseline_and_diffs_to_nothing() {
        // Venus's own shmems are never shipped whole C→S; they carry no baseline and diff to nothing
        // even when written, so `messages_for_delta` never ships them by this path.
        let (mut blob, _fd) = LocalBlob::create(INTERNAL_BLOB_ID, 64).expect("an internal blob");
        blob.bytes_mut().fill(0x11);
        assert!(blob.take_changed_runs(0).is_empty());
    }

    /// `note_s_wrote` must be a documented no-op on a Venus-internal blob, not a panic — even though
    /// the offset/length below would be out of range for a *baseline* such a blob does not have. The
    /// internal-blob branch returns before the method's `debug_assert!` bounds check ever runs, so a
    /// Venus-internal blob's shadow (the ring, the reply arena) can safely receive whatever offsets a
    /// caller happens to pass without tripping it.
    #[test]
    fn note_s_wrote_is_a_no_op_on_a_venus_internal_blob() {
        let (mut blob, _fd) = LocalBlob::create(INTERNAL_BLOB_ID, 64).expect("an internal blob");
        // An offset/length pair that would be in-range for the mapping but has no baseline to land in.
        blob.note_s_wrote(0, &[0x11; 64]);
        assert!(
            blob.take_changed_runs(0).is_empty(),
            "an internal blob has no baseline to disturb, and must still diff to nothing"
        );
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
            blob.take_changed_runs(0).is_empty(),
            "C must not re-ship the bytes S itself wrote"
        );
    }
}
