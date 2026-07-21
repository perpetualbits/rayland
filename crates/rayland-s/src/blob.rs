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
    ///
    /// **That framing is against the refapp; against a hostile peer the doubling is leverage, not
    /// merely cost.** `size` in [`HostBlob::map`] is remote-supplied (`C2S::CreateBlob`), and
    /// building this field allocates exactly `size` more bytes on S's heap. `RenderEngine::
    /// create_blob_resource` already gates `size` before this type ever sees it, so this field does
    /// not create a new avenue for a peer to force allocation — it **doubles the existing one**: the
    /// same `size` S's engine already agreed to allocate for the mapping itself. See
    /// [`HostBlob::map`]'s failure-modes note for what happens if that doubled allocation fails.
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
    /// on S — see [`HostBlob::take_bytes_s_wrote`] — and with it the wart that `blob_id` is a number
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
    ///
    /// **Not listed above because it is not an `EngineError`, and is worth naming anyway:** the
    /// baseline below heap-allocates `size` more bytes, where `size` is the same attacker-controlled
    /// value just bounds-checked by `ShmMapping::map`. An allocator failure here **aborts the
    /// process** (Rust's global allocator has no fallible path a caller can catch) rather than
    /// returning a `Result` this function's signature could report.
    /// `RenderEngine::create_blob_resource` gates `size` before `map` is ever called, so this does
    /// not open new leverage a hostile peer did not already have — it doubles the existing one,
    /// since S already agreed to allocate `size` bytes once for the mapping itself. See the `shadow`
    /// field's doc for the same point stated as a cost rather than a failure mode.
    pub fn map(fd: BorrowedFd<'_>, size: u64) -> Result<Self, EngineError> {
        let mapping = ShmMapping::map(fd, size)?;
        // **The baseline is what C has, and C has zeros.** `LocalBlob::create` on C answers every
        // `CreateBlob` with a brand-new memfd, and a fresh memfd is zero-filled by construction. So
        // at the moment this blob exists, C's copy of it is all zeros — and the baseline's whole
        // meaning (see the `shadow` field) is *"the bytes C already has"*.
        //
        // # This line was `blob.bytes().to_vec()`, and that was the bug that made (c)1 return a
        // # blank picture
        // Reading the live pages looks more careful and is not. It quietly redefines the rule from
        // *"bytes S wrote"* to *"bytes that changed since S mapped the blob"*, and those differ by
        // **every write that happened before the mapping existed**. That is not a corner: it is the
        // readback buffer's normal life. Mesa creates a blob resource lazily, at `vkMapMemory` — so
        // for a readback buffer the resource is created *after* `vkCmdCopyImageToBuffer` has already
        // run, and S's very first sight of those pages is of the finished frame. A live-pages
        // baseline therefore swallows the rendered image into the baseline, `take_bytes_s_wrote`
        // finds nothing to report, and the application on C reads its own untouched zeros. (c)1 Task
        // 6 observed exactly this: `created blob res=5 blob_id=17 size=16384` and, on the very next
        // poll, `res=5 nonzero=8192 first8=[00, 00, ff, ff, ...]` — the blue clear colour, already
        // present, already invisible. The app wrote a fully transparent PNG.
        //
        // # The rejected worry, and why shipping is the *faithful* answer rather than a leak
        // The old comment feared that a zero baseline would "ship an engine's leftovers to C as if
        // S's GPU had rendered them". Two things are wrong with that. First, it inverts the risk:
        // hiding bytes costs correctness on the one blob the whole return path exists for, while
        // shipping them costs at most some bytes. Second, and decisively — **on one machine the
        // application maps these very pages and reads whatever is in them, leftovers included.**
        // Shipping them is what makes C see what a local client would see; withholding them is the
        // deviation. So a zero baseline is not merely the fix, it is the more faithful rule, and it
        // exposes nothing Venus would not have exposed anyway.
        //
        // A `vec![0; n]` also costs less than the read it replaces: no eager fault-in of the whole
        // mapping, and the allocator may hand back zeroed pages without touching them. The failure
        // mode noted above (a `size`-sized allocation that can abort) is unchanged.
        let shadow = vec![0u8; mapping.len()];
        Ok(HostBlob {
            mapping,
            size,
            shadow,
        })
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
    /// # The grain is the **byte**, and that is load-bearing rather than fastidious
    /// Spec §7.2 originally said *page*, and was amended during Task 5b because the page bought
    /// nothing and cost correctness. Dirty-*page* tracking is the usual idiom because *page tables*
    /// are the usual mechanism — but nothing here uses a page table, it uses a comparison, **and a
    /// comparison is byte-granular for free**.
    ///
    /// Rounding runs out to a 4096-byte grain would reintroduce the very race §7.2 exists to
    /// remove, just narrower. If S's engine writes one region of a page while the application writes
    /// another region of *the same* page — entirely legal, and requiring no Vulkan synchronization
    /// between them, since they are different memory — then a page-grain run carries S's **stale**
    /// copy of the application's bytes alongside S's own fresh ones, and C's reader lays the lot
    /// down over what the application has written since. `VkDeviceMemory` is page-aligned and
    /// applications suballocate, so that is realistic rather than theoretical. It is the same
    /// species as the whole-blob race — invisible in the reference app, live for the first real
    /// application. **Every byte in a returned run is a byte S is observed to have written.**
    ///
    /// # Inputs / outputs
    /// - Returns the changed runs, in ascending offset order, with contiguous changed bytes
    ///   coalesced into one run. Empty — the overwhelmingly common case — when S has written
    ///   nothing.
    /// - **Taking is consuming**: each returned run becomes the new baseline, so the same bytes are
    ///   never shipped twice. Two callers would therefore split one retirement's news between them.
    ///
    /// # Pitfall: this races S's engine, and cannot not
    /// virglrenderer's ring thread writes these pages from another process with no lock and no
    /// notification — the whole `vkMapMemory` problem (ring-findings §6). A region written *during*
    /// this call may be read torn, and a byte written between the comparison and the copy is
    /// shipped in its newer state. Neither is silent data loss: a byte that changes after being
    /// compared is left out of the baseline too, so the next retirement sees it and ships it. What
    /// bounds the race in practice is the caller's ordering — S only asks after `head` moved past
    /// the commands that wrote these pages, which is evidence the writes retired, though not a
    /// guarantee the C11 model would recognise.
    ///
    /// # Pitfall: a write whose bytes *coincide* with the baseline fragments into many runs
    /// This is the byte grain's real cost, and it is a **volume** cost rather than a correctness
    /// one. A run breaks wherever a written byte happens to equal the byte already there, because
    /// from a comparison's point of view nothing happened — which is true, and which is exactly why
    /// not shipping it is safe. But it means one logical write can leave as many runs as it has
    /// coincidences.
    ///
    /// **This is steady-state and resolution-scaling, not a one-off "first readback" wart.** An
    /// earlier version of this comment claimed the pathological case was specifically a first write
    /// of a flat colour sharing bytes with a zero baseline, and measurement disproved that. Ring-
    /// findings §6 caught the readback buffer holding `00 00 ff ff` repeated — RGBA `(0, 0, 255,
    /// 255)`, the blue clear colour. Against a freshly mapped blob's zero baseline, only the `ff ff`
    /// pairs differ, so the reference app's *first* readback fragments into 4096 two-byte runs
    /// instead of one 16 KiB run (8192 of the blob's 16384 bytes cross). But render a **second**
    /// frame that rewrites every pixel to a different flat colour — say RGBA `(255, 0, 0, 255)`, red
    /// over the blue baseline — and it fragments *worse*: the G byte (`0x00`) and the A byte
    /// (`0xff`) coincide with the previous frame's G and A on every pixel, so only R and B differ,
    /// giving 4096 pixels × 2 one-byte runs = **8192 runs**, shipping the same 8192 bytes in twice
    /// the messages. A full rewrite in which *nothing is logically unchanged* fragments worse than
    /// the very first write, because G and A coincide **per pixel**, not merely at the start of the
    /// session. Any renderer that keeps its alpha channel constant — i.e. draws only opaque pixels,
    /// the overwhelmingly common case — reproduces the A-byte coincidence on **every** frame,
    /// forever, and the effect scales with resolution: roughly two million such runs per frame at
    /// 1080p. See this module's test
    /// `a_full_rewrite_fragments_worse_than_the_first_write_and_the_pattern_recurs_every_frame` for
    /// the pinned frame-2 measurement (named in prose, not as an intra-doc link, because
    /// `#[cfg(test)]` items are invisible to a non-test `cargo doc`).
    ///
    /// **Why this matters beyond the arithmetic:** ring-findings §7 measured the return path (reply
    /// arena + readback) at roughly 12× the command traffic even *before* this fragmentation, and
    /// per-message framing dominates the return path's cost. Because the fragmentation is steady-
    /// state rather than a startup-only cost, deferring the fix is not "Task 9 can measure it
    /// whenever" — it is required before any real (non-toy) workload, not an optimization to reach
    /// for only "if it ever matters".
    ///
    /// **This is deliberately not optimized here anyway — the fix must not be merging.** Merging runs
    /// across a small gap of unchanged bytes is the obvious-looking fix and is exactly the hole this
    /// grain exists to close: those skipped bytes are precisely the ones S did not write, and
    /// shipping them is what clobbers the application. The correct trade, when it is made, is a wire
    /// change that carries many runs in one message — reducing per-message framing overhead without
    /// widening any single run past bytes S actually wrote. The cost is left visible and measurable
    /// here, which is what spec §6 and §8 ask of v1, and Task 9 measures it against this section's
    /// numbers.
    pub fn take_bytes_s_wrote(&mut self, coalesce_gap: usize) -> Vec<WrittenRun> {
        // Two phases, because they need different borrows of `self` — and because doing the
        // comparison against a baseline that a copy in the same pass had already begun to overwrite
        // would compare each byte against the wrong thing.
        //
        // `coalesce_gap` merges runs separated by up to that many unchanged bytes (re-shipping them),
        // trading a bounded number of redundant bytes for far fewer `S2C::BlobData` messages. It is 0
        // — inert — for every path but the readback, where the grain is not load-bearing (S alone
        // writes `res6`, C only reads it). See [`coalesce_ranges`].
        let ranges = coalesce_ranges(self.changed_byte_ranges(), coalesce_gap);

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

    /// The half-open ranges of every **byte** that diverges from the baseline, with contiguous
    /// changed bytes merged into one range.
    ///
    /// # Why a byte at a time, and why the merging is not an optimization
    /// The grain is the byte because the mechanism is a comparison and a comparison costs the same
    /// either way — see [`HostBlob::take_bytes_s_wrote`] for why any coarser grain would ship bytes
    /// S did not write and clobber the application's.
    ///
    /// Merging is what makes that affordable to *describe*: a 4 KiB reply is one write by S, and
    /// emitting an `S2C::BlobData` per byte would be absurd where one message says the same thing.
    /// So the **byte** is the unit of detection; the **run** is only the unit of description, and
    /// merging never widens a run beyond bytes that actually changed.
    ///
    /// # Inputs / outputs
    /// - Returns half-open `(start, end)` ranges in ascending order, none empty, none adjacent (two
    ///   adjacent ranges would have been merged). Empty when nothing changed.
    ///
    /// Borrows `&self` alone, so the baseline it compares against cannot shift underneath it. No
    /// range can exceed the blob's length, because both slices are the blob's length — which is why
    /// a 64-byte vertex buffer yields a 64-byte run at most, never a padded one that C would have to
    /// refuse as past the end of its own shadow.
    fn changed_byte_ranges(&self) -> Vec<(usize, usize)> {
        // `shadow` is built from `bytes()` at map time and only ever written in place, so the two
        // are the same length and one bound governs both.
        let live = self.bytes();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        // Where the run currently being accumulated began, if one is open.
        let mut open: Option<usize> = None;
        // Zipped rather than indexed: the pairing of each live byte with *its own* baseline byte is
        // the entire predicate, and `zip` states it in the types instead of leaving it to two index
        // expressions that must be kept identical by hand.
        for (i, (&now, &then)) in live.iter().zip(self.shadow.iter()).enumerate() {
            if now != then {
                // A byte S wrote. Extend the open run, or start one here — `get_or_insert` keeps an
                // already-open run's start rather than resetting it to `i`.
                open.get_or_insert(i);
            } else if let Some(start) = open.take() {
                // A byte S did not write, so the run stops *before* it. Shipping it would be
                // shipping a byte S has no claim to — precisely the clobber this grain avoids.
                ranges.push((start, i));
            }
        }
        // A run still open at the end of the blob is closed by the blob's end rather than by an
        // unchanged byte. Without this, a write reaching the final byte would be silently dropped.
        if let Some(start) = open {
            ranges.push((start, live.len()));
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
        // C agree about this range and `take_bytes_s_wrote` must not report it as S's own write.
        // Exactly this range, and no more: re-snapshotting the whole blob here would swallow every
        // unshipped write S had made elsewhere in it, and the arena's replies would vanish the
        // moment C synced anything that shared it. Indexing is in range because `end <= self.size`
        // was established above and `shadow` is `self.size` bytes.
        self.shadow[offset as usize..end as usize].copy_from_slice(bytes);
        Ok(())
    }
}

/// Merge `(start, end)` ranges separated by no more than `gap` unchanged bytes into single ranges,
/// re-including the gap bytes.
///
/// [`HostBlob::changed_byte_ranges`]' byte grain exists to avoid shipping bytes S did not write (which
/// would clobber the application's own writes — see [`HostBlob::take_bytes_s_wrote`]). This widens it
/// back **only** for a blob where that grain is not load-bearing: the readback buffer, which S's GPU
/// alone writes and C only reads, so a re-shipped unchanged byte equals what C already holds
/// (idempotent). The trade is a bounded number of re-shipped unchanged bytes for far fewer
/// `S2C::BlobData` messages — the return path is message-rate-bound, not bandwidth-bound (a readback
/// frame otherwise shatters into thousands of one-byte runs).
///
/// # Inputs / outputs
/// - `ranges`: ascending, non-overlapping half-open ranges, as [`HostBlob::changed_byte_ranges`]
///   returns.
/// - `gap`: merge a range into the previous one when its start is within `gap` bytes of that range's
///   end. `gap == 0` is **inert** — `changed_byte_ranges` never returns adjacent ranges, so nothing
///   merges and the output equals the input (the property the venus/reply path relies on).
/// - Returns the coalesced ranges, ascending and non-overlapping.
fn coalesce_ranges(ranges: Vec<(usize, usize)>, gap: usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        // Extend the open run if this range begins within `gap` unchanged bytes of its end; otherwise
        // start a fresh run. `start - last.1` cannot underflow — inputs are ascending and disjoint.
        if let Some(last) = out.last_mut() {
            if start - last.1 <= gap {
                last.1 = end;
                continue;
            }
        }
        out.push((start, end));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    // A real anonymous shared-memory object: the same kind of thing
    // `virgl_renderer_resource_export_blob` hands S (`fd_type = 3 = VIRGL_RENDERER_BLOB_FD_TYPE_SHM`,
    // ring-findings §2.1), so these tests map exactly what production maps.
    use rayland_vtest::transport::create_memfd;
    use std::os::fd::AsFd;

    #[test]
    fn coalesce_ranges_merges_ranges_within_the_gap() {
        // [0,3) then [5,8): a 2-byte unchanged gap (3..5). With a threshold of 2 they merge, so the
        // gap bytes ride along in one run instead of splitting into two messages.
        assert_eq!(coalesce_ranges(vec![(0, 3), (5, 8)], 2), vec![(0, 8)]);
    }

    #[test]
    fn coalesce_ranges_keeps_ranges_farther_apart_than_the_gap_split() {
        // Same 2-byte gap, but the threshold is 1: it exceeds the threshold, so the runs stay split
        // and no unchanged bytes are re-shipped.
        assert_eq!(coalesce_ranges(vec![(0, 3), (5, 8)], 1), vec![(0, 3), (5, 8)]);
    }

    #[test]
    fn coalesce_ranges_with_zero_gap_merges_nothing() {
        // Gap 0 is the inert case the venus path relies on: `changed_byte_ranges` never returns
        // adjacent ranges, so nothing merges and the output equals the input.
        assert_eq!(coalesce_ranges(vec![(0, 3), (4, 8)], 0), vec![(0, 3), (4, 8)]);
    }

    #[test]
    fn coalesce_ranges_chains_several_small_gaps_into_one() {
        // The readback's real pattern: many tiny runs separated by tiny gaps collapse to one run.
        assert_eq!(
            coalesce_ranges(vec![(3, 4), (7, 8), (11, 12)], 256),
            vec![(3, 12)]
        );
    }

    #[test]
    fn coalesce_ranges_empty_and_single_are_unchanged() {
        assert_eq!(coalesce_ranges(Vec::new(), 256), Vec::<(usize, usize)>::new());
        assert_eq!(coalesce_ranges(vec![(2, 9)], 256), vec![(2, 9)]);
    }

    #[test]
    fn take_bytes_s_wrote_coalesces_runs_within_the_gap() {
        let mut blob = a_blob(16);
        // Two S writes with a 2-byte unchanged gap between them (bytes 1..3 stay zero).
        s_engine_writes(&blob, 0, 0x11, 1);
        s_engine_writes(&blob, 3, 0x11, 1);
        // Gap 2 >= the 2-byte hole: one run ships, the unchanged gap bytes riding along.
        let runs = blob.take_bytes_s_wrote(2);
        assert_eq!(runs.len(), 1, "the 2-byte gap is within the threshold, so one run ships");
        assert_eq!(runs[0].offset, 0);
        assert_eq!(runs[0].bytes, vec![0x11, 0x00, 0x00, 0x11]);
    }

    #[test]
    fn take_bytes_s_wrote_with_zero_gap_keeps_runs_fine() {
        let mut blob = a_blob(16);
        s_engine_writes(&blob, 0, 0x11, 1);
        s_engine_writes(&blob, 3, 0x11, 1);
        // Gap 0 is the venus/reply path's setting: no coalescing, the fine grain is preserved.
        let runs = blob.take_bytes_s_wrote(0);
        assert_eq!(runs.len(), 2, "gap 0 = no coalescing = the fine grain the reply path relies on");
    }

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

    /// The grain the **retracted** page rule used, kept only so these tests can build cases that
    /// distinguish it from the byte grain that replaced it (spec §7.2, as amended in Task 5b).
    ///
    /// Nothing in the module under test knows this number any more. It is here because a test that
    /// cannot tell the two rules apart is not testing the rule it names.
    const A_PAGE: usize = 4096;

    /// A freshly mapped blob nobody has written owes C nothing.
    ///
    /// A blob nobody has written since C last had it owes C nothing. This is the overwhelmingly
    /// common case on the poll loop, and shipping on it would make S a bandwidth source.
    ///
    /// `a_blob` builds over a fresh memfd, so the pages and the zero baseline genuinely agree here —
    /// which is the ordinary case, and is why this test says nothing either way about
    /// [`HostBlob::map`]'s baseline choice. The test that does is
    /// `bytes_already_in_the_pages_at_map_time_are_shipped_because_c_has_never_seen_them`.
    #[test]
    fn a_blob_nobody_wrote_has_no_runs() {
        let mut blob = a_blob(8192);
        assert_eq!(blob.take_bytes_s_wrote(0), Vec::new());
    }

    /// **The regression test for (c)1 Task 6's blank-picture finding.** Bytes that were already in
    /// the pages when S mapped them must still be shipped: C has never seen them.
    ///
    /// # This test asserts the exact opposite of the one it replaces, and the live run is the reason
    /// Its predecessor (`map_takes_its_baseline_from_the_real_pages_not_an_assumed_zero`, added in
    /// review on 2026-07-16) pinned the belief that pre-existing bytes are *"not something S wrote"*
    /// and must stay put. That reasoning treats "S wrote it" as meaning "S wrote it **while S was
    /// watching**", and the difference is the whole return path: Mesa creates a blob resource lazily
    /// at `vkMapMemory`, so for a readback buffer S's first sight of the pages is **after** the GPU
    /// has finished rendering into them. Under the old rule the finished frame *was* the baseline,
    /// nothing was ever reported, and the reference app wrote a fully transparent PNG across a
    /// working network — which is precisely what Task 6 observed.
    ///
    /// The correct predicate is not "did S write it while watching" but **"does C have it?"** — and
    /// C, whose blob is a fresh zero-filled memfd, does not. Shipping is also what a single machine
    /// does: there, the application maps these very pages and reads whatever is in them.
    ///
    /// The `0xee` pattern stands in for the real case, which a memfd cannot reproduce (a fresh one
    /// is always zero-filled): a GPU that has already rendered into the memory before the blob
    /// resource naming it exists.
    #[test]
    fn bytes_already_in_the_pages_at_map_time_are_shipped_because_c_has_never_seen_them() {
        let fd = create_memfd(64).expect("a memfd");
        {
            // Put bytes in the pages *before* `HostBlob::map` sees them — standing in for the GPU
            // having already written the readback buffer by the time Mesa asks for its blob.
            // Dropped before `HostBlob::map` below, so the two mappings never coexist.
            let pre = ShmMapping::map(fd.as_fd(), 64).expect("a pre-mapping");
            // SAFETY: `pre` is a live `MAP_SHARED` mapping of exactly 64 bytes, and nothing else
            // touches this memfd while `pre` is alive.
            unsafe { std::ptr::write_bytes(pre.as_ptr().cast::<u8>(), 0xee, 64) };
        }

        let mut blob = HostBlob::map(fd.as_fd(), 64).expect("mapping it");

        assert_eq!(
            blob.take_bytes_s_wrote(0),
            vec![WrittenRun {
                offset: 0,
                bytes: vec![0xee; 64],
            }],
            "bytes present before S mapped the blob must be shipped: C's blob is a fresh memfd and \
             is all zeros, so C has never seen them. Baselining against the live pages instead \
             swallows a readback buffer's finished frame — Mesa creates that blob only at \
             vkMapMemory, i.e. after the GPU has already written it — and the application reads its \
             own untouched zeros."
        );
        assert_eq!(
            blob.take_bytes_s_wrote(0),
            Vec::new(),
            "and once shipped they are the baseline: C has them now, so they are not news twice"
        );
    }

    /// A partial write of a blob ships **exactly the bytes written**, not the blob and not a page.
    ///
    /// The application's vertex buffer is 64 bytes (`res=3`, ring-findings §6) — smaller than a
    /// page in its entirety, so under the retracted page rule *any* write to it shipped all 64
    /// bytes, including the ones the application owns. Under the byte rule the run is the write.
    #[test]
    fn a_partial_write_ships_only_the_bytes_written() {
        let mut blob = a_blob(64);
        s_engine_writes(&blob, 8, 0xc3, 8);

        let runs = blob.take_bytes_s_wrote(0);

        assert_eq!(
            runs,
            vec![WrittenRun {
                offset: 8,
                bytes: vec![0xc3; 8]
            }],
            "S wrote 8 bytes at offset 8, so 8 bytes at offset 8 cross — a run covering the whole \
             64-byte blob would be carrying 56 bytes S never wrote"
        );
    }

    /// A run that reaches the blob's **final byte** is closed by the blob's end, and still carries
    /// only the bytes S wrote.
    ///
    /// This is the one range the loop cannot close by finding an unchanged byte after it, so it is
    /// the branch a diff most easily drops on the floor: the write would simply never cross, and
    /// the last thing S rendered would silently never reach the application.
    #[test]
    fn a_run_reaching_the_blob_s_final_byte_is_closed_by_the_end() {
        let mut blob = a_blob(A_PAGE as u64 + 10);
        // Ends exactly at the blob's last byte, and starts partway into the trailing page so that
        // a page-grain run would visibly differ (it would start at the page boundary, 4096).
        s_engine_writes(&blob, A_PAGE + 4, 0x7f, 6);

        let runs = blob.take_bytes_s_wrote(0);

        assert_eq!(
            runs,
            vec![WrittenRun {
                offset: A_PAGE as u64 + 4,
                bytes: vec![0x7f; 6]
            }],
            "the run must start where S's write started and stop at the blob's end; got {runs:?}"
        );
    }

    /// **The byte grain's known cost, pinned with the live capture's real bytes rather than left as
    /// a hunch.**
    ///
    /// A run breaks wherever a written byte coincides with the byte already there — from a
    /// comparison's point of view nothing happened, which is true, and which is why not shipping it
    /// is safe. But one logical write can then leave as many runs as it has coincidences.
    ///
    /// Ring-findings §6 caught the readback buffer (`res=6`, 16384 B = 64×64×4) holding
    /// `00 00 ff ff` repeated — RGBA `(0, 0, 255, 255)`, the blue clear colour. Against a fresh
    /// blob's zero baseline **only the `ff ff` pairs differ**, so the reference app's very first
    /// readback fragments into 4096 two-byte runs rather than one 16 KiB run.
    ///
    /// This test exists to make that number a fact Task 9 can measure against, and to fail loudly if
    /// someone "fixes" it by merging runs across unchanged bytes — those skipped bytes are exactly
    /// the ones S did not write, and shipping them is the clobber §7.2 exists to prevent. The
    /// correct trade, if this ever matters, is a wire change that carries many runs in one message,
    /// not a diff that lies about what S wrote.
    #[test]
    fn a_flat_colour_readback_fragments_into_one_run_per_pixel_and_that_is_the_known_cost() {
        let mut blob = a_blob(16384);
        // The blue clear colour, written as the GPU writes it: RGBA (0, 0, 255, 255) per pixel.
        for pixel in 0..4096 {
            s_engine_writes(&blob, pixel * 4 + 2, 0xff, 2);
        }

        let runs = blob.take_bytes_s_wrote(0);

        assert_eq!(
            runs.len(),
            4096,
            "one run per pixel: the R and G bytes are zero over a zero baseline, so they are not \
             bytes S wrote and must not ride along. Got {} runs",
            runs.len()
        );
        assert_eq!(
            runs[0],
            WrittenRun {
                offset: 2,
                bytes: vec![0xff; 2]
            },
            "each run is the pixel's B and A bytes alone"
        );
        let shipped: usize = runs.iter().map(|r| r.bytes.len()).sum();
        assert_eq!(
            shipped, 8192,
            "half the blob's bytes cross, in 4096 messages — fewer bytes than the page rule shipped, \
             in far more messages. That trade is v1's to measure (spec §8), not to pre-empt"
        );
    }

    /// **Important 2 (review, 2026-07-16): the fragmentation cost is steady-state and
    /// per-frame, not a one-off "first readback" wart — this pins the frame-2 measurement that
    /// disproves the weaker claim.**
    ///
    /// Frame 1 (above) fragments against a *zero* baseline: only the `ff` bytes of blue's `B` and
    /// `A` channels differ from zero, so 4096 two-byte runs cross. This test asks what happens on
    /// **frame 2**, where the GPU rewrites *every* pixel to a different flat colour — nothing is
    /// logically "unchanged" between the two frames, which is the case the "first readback" framing
    /// implied would be cheap.
    ///
    /// It is not cheap — it is worse. Blue is RGBA `(0, 0, 255, 255)`; red is
    /// `(255, 0, 0, 255)`. Byte for byte against the blue baseline: `R` differs (`0x00` → `0xff`),
    /// `G` coincides (`0x00` == `0x00`), `B` differs (`0xff` → `0x00`), `A` coincides
    /// (`0xff` == `0xff`). The `G`/`A` coincidences split every pixel's run in two, so this frame
    /// ships **8192** one-byte runs — twice frame 1's run count — carrying the same 8192 bytes.
    /// `A` staying `0xff` is not a coincidence of this specific test: any renderer that keeps its
    /// alpha channel constant (i.e. draws only opaque pixels — the ordinary case) reproduces that
    /// coincidence, and therefore this fragmentation, on **every** frame it ever renders, and the
    /// run count scales with the image's pixel count, not with the size of any one write.
    #[test]
    fn a_full_rewrite_fragments_worse_than_the_first_write_and_the_pattern_recurs_every_frame() {
        let mut blob = a_blob(16384);
        // Frame 1: the GPU clears to blue, RGBA (0, 0, 255, 255) per pixel — same as the test above.
        for pixel in 0..4096 {
            s_engine_writes(&blob, pixel * 4 + 2, 0xff, 2);
        }
        let frame_1_runs = blob.take_bytes_s_wrote(0);
        assert_eq!(
            frame_1_runs.len(),
            4096,
            "sanity check: this must reproduce the frame-1 baseline the test above pins"
        );

        // Frame 2: the GPU rewrites *every* pixel to red, RGBA (255, 0, 0, 255). A full rewrite —
        // nothing left "unchanged" between the frames — yet G and A coincide with frame 1's values
        // on every single pixel, because both colours are fully opaque and both have a zero G channel.
        for pixel in 0..4096 {
            s_engine_writes(&blob, pixel * 4, 0xff, 1); // R: 0x00 -> 0xff
            s_engine_writes(&blob, pixel * 4 + 2, 0x00, 1); // B: 0xff -> 0x00
        }

        let frame_2_runs = blob.take_bytes_s_wrote(0);

        assert_eq!(
            frame_2_runs.len(),
            8192,
            "a full rewrite of every pixel must fragment *worse* than the first write, not better: \
             G and A coincide with the previous frame on every pixel, splitting each pixel's R and B \
             writes into two separate one-byte runs. Got {} runs",
            frame_2_runs.len()
        );
        let shipped: usize = frame_2_runs.iter().map(|r| r.bytes.len()).sum();
        assert_eq!(
            shipped, 8192,
            "the same 8192 bytes changed as in frame 1, but now split across twice as many messages \
             — this is the steady-state cost, not a first-readback-only one"
        );
        assert!(
            frame_2_runs.iter().all(|r| r.bytes.len() == 1),
            "every run this frame must be exactly one byte (R alone, or B alone) — a run spanning \
             both would mean G or A was mistakenly folded in. Got {frame_2_runs:?}"
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
            blob.take_bytes_s_wrote(0).len(),
            1,
            "the first take ships it"
        );
        assert_eq!(
            blob.take_bytes_s_wrote(0),
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
        let mut blob = a_blob(2 * A_PAGE as u64);
        // S's engine writes into the second page and has not yet shipped it.
        s_engine_writes(&blob, A_PAGE, 0x5a, 16);
        // C's own bytes land in the first page.
        blob.copy_in(0, &[0x33; 8]).expect("an in-range write");

        let runs = blob.take_bytes_s_wrote(0);

        assert_eq!(
            runs,
            vec![WrittenRun {
                offset: A_PAGE as u64,
                bytes: vec![0x5a; 16]
            }],
            "C's write must not be shipped back, and S's must not be swallowed by it — and S's run \
             is the 16 bytes S wrote, not the 4096-byte page containing them, of which 4080 bytes \
             are S's stale copy of memory the application owns. Got {runs:?}"
        );
    }

    /// A refused `copy_in` must not move the baseline either — it wrote nothing, so C has nothing.
    #[test]
    fn a_refused_copy_in_does_not_touch_the_baseline() {
        let mut blob = a_blob(64);
        s_engine_writes(&blob, 0, 0x5a, 64);

        blob.copy_in(60, &[0xff; 8])
            .expect_err("a write past the end must be refused");

        assert_eq!(
            blob.take_bytes_s_wrote(0),
            vec![WrittenRun {
                offset: 0,
                bytes: vec![0x5a; 64]
            }],
            "the refused write changed nothing on C, so S still owes C the bytes it wrote"
        );
    }
}
