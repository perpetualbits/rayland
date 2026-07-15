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

/// The granularity at which S detects, and ships, the bytes its own side wrote.
///
/// # What this is, and the thing it is deliberately *not*
/// Spec §7.2 pins S's rule to *"the pages S wrote"*, and this is that page. It is **the chunk size
/// of a byte comparison**, not a property of any mapping: nothing in this module asks the kernel
/// about page boundaries, and a host whose MMU page is 16 KiB (aarch64) or 64 KiB (ppc64le) does
/// not make any of the arithmetic below wrong. It would only change how many unchanged bytes ride
/// along with each changed one. **Do not "fix" this to `sysconf(_SC_PAGESIZE)`**: that would tie a
/// wire-visible decision on S to a property of whatever machine S happens to be, for no correctness
/// gain.
///
/// 4096 is chosen because it is the common page size and therefore the natural grain of the writes
/// being detected, and because §7.2 budgets for the cost it implies: the live capture's blobs are
/// 8 MiB of staging pool plus 1 MiB of reply arena plus 16 KiB of readback, i.e. **roughly 2300
/// comparisons per retirement**. That is the intended, measured slowness of v1 (spec §6, §8), not
/// something to optimize ahead of Task 9's numbers.
pub const SYNC_PAGE_BYTES: usize = 4096;

/// A contiguous run of bytes **S wrote** into a blob and has not yet told C about.
///
/// It goes on the wire as one [`S2C::BlobData`](rayland_relay::S2C::BlobData) — `offset` is that
/// message's `offset` field, which has existed since Task 4 and was 0 on every message until §7.2
/// gave it a use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenRun {
    /// Where in the blob these bytes belong.
    pub offset: u64,
    /// The bytes standing there now. Never empty — a run exists only because something changed.
    pub bytes: Vec<u8>,
}

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
    /// **What S last knows C's copy of these pages holds.**
    ///
    /// This is the baseline spec §7.2's rule is expressed against, and its meaning is worth stating
    /// precisely, because it is not "the last thing S saw". It is a record of the two — and only
    /// the two — events after which S and C are known to agree about a byte:
    ///
    /// - the blob was created, and neither side has written it yet ([`HostBlob::map`]);
    /// - C's own bytes arrived and were laid into these pages ([`HostBlob::copy_in`]).
    ///
    /// Anything that diverges from it afterwards therefore diverged because **S's side wrote it**,
    /// which is exactly the question §7.2 says to ask. Nothing else on S can answer it: the writer
    /// is virglrenderer's ring thread, in another process, and it announces nothing.
    ///
    /// # Cost, stated rather than hidden
    /// It doubles S's memory for every blob — ~9.1 MiB across the live capture's six. That is the
    /// deliberate, measurable cost of v1 (spec §6, §8). It is not new pressure of a *kind* S did not
    /// already have: before §7.2, `poll_progress` copied whole blobs out on every retirement.
    shadow: Vec<u8>,
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
    /// # Why there is no `blob_id` parameter any more
    /// There was, until spec §7.2. It was recorded so `is_application_memory` could later answer
    /// "whose memory is this?" and route S's outbound blob sync on the answer. That rule is retired
    /// on S — see [`HostBlob::take_pages_s_wrote`] — and with it the wart that `blob_id` is a number
    /// a remote peer chose, unverified against anything, silently deciding what S publishes.
    /// `blob_id` still reaches
    /// [`RingIdentity::from_blob_request`](rayland_vtest::venus_ring::RingIdentity::from_blob_request)
    /// straight off the wire, where it separates a ring-shaped application buffer from a real ring;
    /// it simply no longer needs to outlive that.
    ///
    /// # Failure modes
    /// [`EngineError::ShmMapFailed`] if the mapping fails or `size` does not fit this platform's
    /// address space. `size` originates from a remote peer, so that is a real check rather than a
    /// formality.
    pub fn map(fd: BorrowedFd<'_>, size: u64) -> Result<Self, EngineError> {
        let mapping = ShmMapping::map(fd, size)?;
        let mut blob = HostBlob {
            mapping,
            size,
            // Filled immediately below. It cannot be built before the blob exists, because the only
            // sound read of these pages goes through `bytes()`, which needs the mapping.
            shadow: Vec::new(),
        };
        // Take the baseline from the pages as they actually are, rather than assuming a fresh blob
        // is zeros. The two agree for every allocator that matters — a memfd and virglrenderer's
        // SHM blobs are both zero-filled — but if one ever were not, whatever is in there is still
        // not something **S wrote**, and §7.2's rule is that S stays quiet about it. Assuming zeros
        // would instead ship an engine's leftovers to C as if S's GPU had rendered them.
        blob.shadow = blob.bytes().to_vec();
        Ok(blob)
    }

    /// Take every run of bytes **S's own side wrote** since C last had this blob, and adopt them as
    /// the new baseline.
    ///
    /// # This is spec §7.2's rule, and the reason it is phrased this way
    /// The question is not *"whose memory is this?"* but *"did I write it?"*. On one machine every
    /// byte S writes is instantly visible to C, so an ownership predicate — `blob_id`, a decoded
    /// `vkSetReplyCommandStreamMESA`, anything — is a *guess* at that relationship, while an
    /// observed write **is** it. This function is that observation, and it is a predicate over
    /// bytes rather than a reading of them, so spec §7's "no decoding the ring to make a
    /// correctness decision" is untouched.
    ///
    /// What falls out, with no knowledge of what any blob *is*: the reply arena crosses (S's engine
    /// writes replies into it — spec §5's channel 2, which nothing carried before §7.2); the 8 MiB
    /// command-buffer staging pool never does (S never writes it, so C's recording in progress is
    /// never wiped); the application's vertex buffers never do (S's GPU only reads them, so the
    /// last-writer-wins race §7.2 describes cannot happen); the readback buffer does, because the
    /// GPU genuinely wrote it. It is immune to the reply pool growing a new `res_id`, to the shmem
    /// cache recycling ids, and to Venus adding a fourth internal shmem tomorrow.
    ///
    /// **The ring is the one thing this must not be asked about**, and its caller excludes it by
    /// `res_id` rather than by anything here: S's engine really does write the ring's `head`, so
    /// this function would rightly report it. See [`crate::apply::Applier::poll_progress`].
    ///
    /// # Inputs / outputs
    /// - Returns the changed runs, in ascending offset order, with adjacent changed pages coalesced
    ///   into one run. Empty — the overwhelmingly common case — when S has written nothing.
    /// - **Taking is consuming**: each returned run becomes the new baseline, so the same bytes are
    ///   never shipped twice. Two callers would therefore split one retirement's news between them.
    ///
    /// # Pitfall: this races S's engine, and cannot not
    /// virglrenderer's ring thread writes these pages from another process with no lock and no
    /// notification — the whole `vkMapMemory` problem (ring-findings §6). A page written *during*
    /// this call may be read torn, and a page written between the comparison and the copy is
    /// shipped in its newer state. Neither is silent data loss: a page that changes after being
    /// compared is left out of the baseline too, so the next retirement sees it and ships it. What
    /// bounds the race in practice is the caller's ordering — S only asks after `head` moved past
    /// the commands that wrote these pages, which is evidence the writes retired, though not a
    /// guarantee the C11 model would recognise.
    ///
    /// # Pitfall: the page is the grain, so a page can be *falsely shared*
    /// If S's engine writes one region of a page while the application writes another region of the
    /// same page — legal, and needing no Vulkan synchronization between them, since they are
    /// different memory — then the run S ships carries S's stale copy of the application's bytes
    /// alongside S's own fresh ones, and C's reader lays the lot down. That is a genuine residual
    /// of the page grain: it is far narrower than the whole-blob race §7.2 removed (which fired
    /// whenever the app touched *any* blob S had), but it is the same species, and a byte-granular
    /// diff would close it at the same comparison cost. Recorded rather than fixed because §7.2
    /// specifies the page, and because (c)2 owns mapped-memory coherence.
    pub fn take_pages_s_wrote(&mut self) -> Vec<WrittenRun> {
        // Two phases, because they need different borrows of `self` — and because doing the
        // comparison against a baseline that a copy in the same pass had already begun to overwrite
        // would compare each page against the wrong thing.
        let ranges = self.changed_page_ranges();

        let mut runs = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            // Read the run out of the live pages, ending the borrow of the mapping before the
            // baseline is touched. This may pick up a write that landed since the comparison; that
            // is fine, and better than shipping the older bytes — what is shipped and what becomes
            // the baseline are the same bytes either way.
            let bytes = self.bytes()[start..end].to_vec();
            // Adopt them: C is about to have exactly these bytes, so S no longer owes them.
            self.shadow[start..end].copy_from_slice(&bytes);
            runs.push(WrittenRun {
                offset: start as u64,
                bytes,
            });
        }
        runs
    }

    /// The half-open byte ranges of every page that diverges from the baseline, with adjacent
    /// changed pages merged into one range.
    ///
    /// Merging is not an optimization for its own sake: a reply spanning several pages is one write
    /// by S, and splitting it into a message per page would put 256 `S2C::BlobData` on the wire for
    /// a fully-written arena, each describing a fragment of one thing. A run is what the message
    /// shape actually means. The **page** remains the unit of *detection*; the run is only the unit
    /// of description.
    ///
    /// Borrows `&self` alone, so the baseline it compares against cannot shift underneath it.
    fn changed_page_ranges(&self) -> Vec<(usize, usize)> {
        let live = self.bytes();
        let len = live.len();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut start = 0usize;
        while start < len {
            // `min(len)` is what makes a blob that is not a whole number of pages work: the
            // application's vertex buffer is 64 bytes, and a run claiming a padded 4096 would be a
            // `BlobData` C must refuse as past the end of its own 64-byte shadow.
            let end = (start + SYNC_PAGE_BYTES).min(len);
            if live[start..end] != self.shadow[start..end] {
                match ranges.last_mut() {
                    // The previous page changed too and ended exactly here, so this is more of the
                    // same run rather than a new one.
                    Some(last) if last.1 == start => last.1 = end,
                    _ => ranges.push((start, end)),
                }
            }
            start = end;
        }
        ranges
    }

    /// The blob's size in bytes — the ceiling every remote-supplied offset is checked against.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The blob's pages, for reading — the bytes S's GPU wrote that C is waiting for.
    ///
    /// # Why this exists: it is how the application ever sees its own rendered frame
    /// C0 Task 4b caught the reference app's readback buffer (`res=6`, 16384 B = 64×64×4) holding
    /// the blue clear colour — the picture, sitting in S's memory. On one machine the application
    /// would simply read those pages. Across a network somebody has to copy them out and ship them,
    /// and this is that read. See [`crate::apply::Applier::poll_progress`].
    ///
    /// # Pitfall: these bytes have another writer, concurrently
    /// virglrenderer's ring thread — in the forked render-server subprocess — writes these pages
    /// with no lock and no notification, exactly as Mesa does on C. Reading them is therefore
    /// inherently racy, and v1 has nothing that makes it not so: the application never told anyone
    /// when its GPU writes finished, which is the whole `vkMapMemory` problem (ring-findings §6).
    /// What bounds the race in practice is that S reads *after* the ring thread advanced `head` past
    /// the commands that wrote these pages — see `poll_progress`'s ordering — which is evidence the
    /// writes retired, though not a guarantee the C11 model would recognise.
    ///
    /// A `&[u8]` rather than a raw read because a shared reference asserts only that *this* side
    /// forms no `&mut` to the same pages, which is true: [`HostBlob::copy_in`] takes `&mut self`.
    /// The cross-process writer is outside Rust's model either way — see the module docs.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `mapping` is a live `MAP_SHARED` mapping of exactly `size` bytes, unmapped only
        // when `self` drops, so the pointer is valid for the whole of the returned slice's lifetime,
        // which `&self` bounds. `size` fits a `usize` because `ShmMapping::map` already proved it.
        // `u8` has no alignment requirement and no invalid bit patterns, so any byte virglrenderer
        // writes is a valid `u8`. The concurrent cross-process writer is a data race in the abstract
        // model, not an aliasing or validity violation — see the module docs.
        unsafe { std::slice::from_raw_parts(self.as_ptr() as *const u8, self.size as usize) }
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
    ///
    /// # This is also where S re-takes its baseline, and that is load-bearing
    /// Spec §7.2 requires S to snapshot a blob after applying an inbound `C2S::BlobData`, so that
    /// C's own writes can never later be mistaken for S's and shipped back. That snapshot lives
    /// *here*, in the one function through which C's bytes can reach these pages, rather than at the
    /// call site — so it is a structural guarantee rather than a discipline someone must remember.
    /// Forgetting it would not fail loudly: it would make S echo C's every write back at C, over
    /// whatever the application had written in the meantime.
    ///
    /// The baseline takes `bytes` — what C sent — rather than a re-read of the mapping. A re-read
    /// could pick up a write S's engine landed in the same range a moment later and absorb it into
    /// the baseline, and S would then never ship it.
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
        // C now demonstrably has these bytes — it is the side that sent them — so record that S and
        // C agree about this range and `take_pages_s_wrote` must not report it as S's own write.
        // Exactly this range, and no more: re-snapshotting the whole blob here would swallow every
        // unshipped write S had made elsewhere in it, and the arena's replies would vanish the
        // moment C synced anything that shared it. Indexing is in range because `end <= self.size`
        // was established above and `shadow` is `self.size` bytes.
        self.shadow[offset as usize..end as usize].copy_from_slice(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // A real anonymous shared-memory object: the same kind of thing
    // `virgl_renderer_resource_export_blob` hands S (`fd_type = 3 = VIRGL_RENDERER_BLOB_FD_TYPE_SHM`,
    // ring-findings §2.1), so these tests map exactly what production maps.
    use rayland_vtest::transport::create_memfd;
    use std::os::fd::AsFd;

    /// A blob of `size` bytes, mapped exactly as [`crate::apply::Applier`] maps one.
    fn a_blob(size: u64) -> HostBlob {
        let fd = create_memfd(size).expect("a memfd");
        HostBlob::map(fd.as_fd(), size).expect("mapping it")
    }

    /// Write into a blob's pages **behind [`HostBlob::copy_in`]'s back**, standing in for S's
    /// engine: virglrenderer writes replies and rendered pixels into these pages from another
    /// process, with no call into this crate and nothing to intercept.
    fn s_engine_writes(blob: &HostBlob, offset: usize, fill: u8, len: usize) {
        // SAFETY: the caller keeps `offset + len` within the blob, the mapping is live and writable
        // for the whole of `blob`'s lifetime, and nothing else touches these pages during a test.
        unsafe {
            std::ptr::write_bytes(blob.as_ptr().add(offset), fill, len);
        }
    }

    /// A freshly mapped blob nobody has written owes C nothing.
    ///
    /// The baseline is taken from the pages themselves at map time rather than assumed to be zeros:
    /// if an engine ever handed S a blob with something already in it, that is still not something
    /// **S wrote**, and §7.2's rule says S stays quiet about it.
    #[test]
    fn a_blob_nobody_wrote_has_no_runs() {
        let mut blob = a_blob(8192);
        assert_eq!(blob.take_pages_s_wrote(), Vec::new());
    }

    /// A blob smaller than a page ships its **real length**, not a padded page.
    ///
    /// The application's vertex buffer is 64 bytes (`res=3`, ring-findings §6). A run that claimed
    /// 4096 bytes for it would be a `BlobData` C must refuse as past the end of its own 64-byte
    /// shadow — so the whole sync would collapse on the smallest blob in the capture.
    #[test]
    fn a_blob_shorter_than_a_page_ships_its_real_length() {
        let mut blob = a_blob(64);
        s_engine_writes(&blob, 0, 0xc3, 64);

        let runs = blob.take_pages_s_wrote();

        assert_eq!(
            runs,
            vec![WrittenRun {
                offset: 0,
                bytes: vec![0xc3; 64]
            }]
        );
    }

    /// The last page of a blob whose size is not a whole number of pages is likewise not padded.
    #[test]
    fn a_trailing_partial_page_ships_only_the_bytes_that_exist() {
        let mut blob = a_blob(SYNC_PAGE_BYTES as u64 + 10);
        s_engine_writes(&blob, SYNC_PAGE_BYTES, 0x7f, 10);

        let runs = blob.take_pages_s_wrote();

        assert_eq!(
            runs.len(),
            1,
            "only the trailing page changed; got {runs:?}"
        );
        assert_eq!(runs[0].offset, SYNC_PAGE_BYTES as u64);
        assert_eq!(
            runs[0].bytes,
            vec![0x7f; 10],
            "the run stops at the blob's end, not at the page's"
        );
    }

    /// Taking the runs adopts them as the new baseline, so the same bytes are not shipped twice.
    ///
    /// Without this, every retirement for the rest of the session would re-ship every byte S had
    /// ever written — the arena's replies included, long after C had consumed them.
    #[test]
    fn taking_the_runs_adopts_them_so_they_are_not_shipped_twice() {
        let mut blob = a_blob(8192);
        s_engine_writes(&blob, 0, 0x11, 8192);

        assert_eq!(
            blob.take_pages_s_wrote().len(),
            1,
            "the first take ships it"
        );
        assert_eq!(
            blob.take_pages_s_wrote(),
            Vec::new(),
            "S has written nothing since, so it has nothing more to say"
        );
    }

    /// [`HostBlob::copy_in`] is what re-takes the baseline, and it must do so over **exactly** the
    /// range it wrote.
    ///
    /// The narrowness is the point. If `copy_in` re-snapshotted the whole blob, an inbound
    /// `BlobData` touching one byte would absorb every unshipped write S had made elsewhere in the
    /// blob — the arena's replies would vanish the moment C synced anything sharing it.
    #[test]
    fn copy_in_re_snapshots_only_the_range_it_wrote() {
        let mut blob = a_blob(2 * SYNC_PAGE_BYTES as u64);
        // S's engine writes the second page and has not yet shipped it.
        s_engine_writes(&blob, SYNC_PAGE_BYTES, 0x5a, 16);
        // C's own bytes land in the first page.
        blob.copy_in(0, &[0x33; 8]).expect("an in-range write");

        let runs = blob.take_pages_s_wrote();

        assert_eq!(
            runs.len(),
            1,
            "C's write must not be shipped back, and S's must not be swallowed by it; got {runs:?}"
        );
        assert_eq!(
            runs[0].offset, SYNC_PAGE_BYTES as u64,
            "the run is S's page, not C's"
        );
        assert_eq!(&runs[0].bytes[..16], &[0x5a; 16]);
    }

    /// A refused `copy_in` must not move the baseline either — it wrote nothing, so C has nothing.
    #[test]
    fn a_refused_copy_in_does_not_touch_the_baseline() {
        let mut blob = a_blob(64);
        s_engine_writes(&blob, 0, 0x5a, 64);

        blob.copy_in(60, &[0xff; 8])
            .expect_err("a write past the end must be refused");

        assert_eq!(
            blob.take_pages_s_wrote(),
            vec![WrittenRun {
                offset: 0,
                bytes: vec![0x5a; 64]
            }],
            "the refused write changed nothing on C, so S still owes C the bytes it wrote"
        );
    }
}
