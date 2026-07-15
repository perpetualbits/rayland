//! [`RingMirror`]: S's side of one Venus command ring — reconstructing, in S's own memory, the ring
//! that Mesa is writing on C.
//!
//! # This module is the heart of (c)1 Task 4, and it is where the task's brief was wrong
//! The obvious design — the one Task 4's brief specified — is that a `C2S::RingDelta` becomes
//! `RenderEngine::submit(ring_res_id, bytes)`, handing the relayed command bytes to
//! `virgl_renderer_submit_cmd`. **That does not work, and the source is unambiguous about why.**
//!
//! virglrenderer consumes a Venus command stream at two places, and they are *different decoder
//! instances* reading *different memory* (ring-findings §3.1):
//!
//! - **The inline path**, `vkr_context.c:170-173`, decodes into the **context's** decoder. This is
//!   what `virgl_renderer_submit_cmd` — i.e. `RenderEngine::submit` — reaches. It carries ring
//!   *management* only: one `vkCreateRingMESA` and a handful of doorbells, 140–236 bytes across an
//!   entire Vulkan initialization.
//! - **The ring path**, `vkr_ring.c:220-223`, decodes into the **ring's own private** decoder. It is
//!   fed by `vkr_ring_thread` (`vkr_ring.c:262-266`), which does not receive anything: it *polls*.
//!
//! And what it polls is the decisive detail. `vkr_ring_create` points the ring's control words and
//! its buffer straight into the blob resource's memory (`vkr_ring.c:33-58`):
//!
//! ```c
//! ctrl->head   = get_resource_pointer(layout->resource, layout->head.begin);
//! ctrl->tail   = get_resource_pointer(layout->resource, layout->tail.begin);
//! ctrl->status = get_resource_pointer(layout->resource, layout->status.begin);
//! /* ... */
//! buf->data    = get_resource_pointer(layout->resource, layout->buffer.begin);
//! ```
//!
//! and the thread then loops on exactly that memory (`vkr_ring.c:262-266`):
//!
//! ```c
//! const uint32_t cmd_size = vkr_ring_load_tail(ring) - ring->buffer.cur;
//! if (cmd_size) {
//!    const uint32_t ring_head = ring->buffer.cur;
//!    vkr_ring_read_buffer(ring, ring->cmd, cmd_size);
//!    if (!vkr_ring_submit_cmd(ring, ring->cmd, cmd_size, ring_head)) { ... }
//! ```
//!
//! **So the way to give S's engine the application's commands is to write them into the ring blob's
//! pages and store the new `tail`.** There is no function to call. `virgl_renderer_submit_cmd` would
//! feed the wrong decoder, splicing the application's stream into a byte stream it was never part
//! of, while the ring thread went on polling memory that never changed — and nothing the application
//! draws would ever execute.
//!
//! # The three control words, and who owns each
//! Ring-findings §4 pins the layout; the ownership is what matters here, and on S it is the mirror
//! image of C's:
//!
//! | word | on a normal Venus setup | on S |
//! |---|---|---|
//! | `tail` | written by the client | **written by this module**, standing in for the client |
//! | `buffer` | written by the client | **written by this module** |
//! | `head` | written by the host | written by virglrenderer's ring thread; **read** by this module |
//! | `status` | written by the host | written by virglrenderer's ring thread; untouched here |
//!
//! S deliberately never writes `head` or `status`. They are the ring thread's, and S's job is to
//! *report* what that thread did — see [`RingMirror::take_progress`].
//!
//! # Memory ordering: real atomics, and a smaller gap — not "no gap"
//! `rayland-c`'s equivalent module has to document two ordering gaps, because its peer across the
//! mapping is Mesa **in another process** and Rust's memory model cannot describe that sharing. **S's
//! peer is likewise in another process**: virglrenderer runs the ring thread (`vkr_ring_start` spawns
//! it) inside the forked render-server subprocess that `VIRGL_RENDERER_RENDER_SERVER` requires for
//! Venus (`rayland-engine/src/ffi.rs`) — S's own process never runs this thread. So the pairing rests
//! on `MAP_SHARED` coherence between two processes' mappings of the same object, plus thread-scoped
//! release/acquire on each side — the standard shared-memory idiom, formally outside the C11/Rust
//! abstract machine's cross-process guarantees but honoured by every real implementation, exactly as
//! it is for C's peer, Mesa.
//!
//! What genuinely differs from C is narrower than "no gap": virglrenderer's ring thread uses real
//! C11 atomics on its side (`vkr_ring_load_tail`, `vkr_ring_store_head`), so S's `AtomicU32` pairs
//! with an actual atomic — closing `rayland-c`'s Gap 1 (Mesa's plain, non-atomic accesses). And S
//! never parks, so C's Gap 2 (the Dekker StoreLoad park handshake) has no analogue here. Both are
//! real advantages; neither removes the cross-process formal hole itself. With that scoped correctly,
//! the pairing this module relies on can be stated exactly:
//!
//! - virglrenderer loads `tail` with `memory_order_acquire` (`vkr_ring_load_tail`,
//!   `vkr_ring.c:68-75`), with the comment *"the driver is expected to store the tail with
//!   memory_order_release, forming a release-acquire ordering"*. [`RingMirror::apply_delta`] stores
//!   it with [`Ordering::Release`], which is precisely the store that comment asks for — and it is
//!   what makes the buffer bytes written beforehand visible to the ring thread.
//! - virglrenderer stores `head` with `memory_order_release` (`vkr_ring_store_head`,
//!   `vkr_ring.c:60-67`), with the comment *"the renderer is expected to load the head with
//!   memory_order_acquire"*. [`RingMirror::take_progress`] does exactly that.
//!
//! The ordering of the two writes in [`RingMirror::apply_delta`] — bytes first, then the `Release`
//! store of `tail` — is therefore load-bearing rather than stylistic. Reversed, the ring thread
//! could observe a `tail` covering bytes it has not yet been shown, and would decode uninitialized
//! memory as Vulkan commands.

// The ring's field offsets: the repository's single copy of its layout knowledge.
use rayland_vtest::venus_ring::{RING_BUFFER_OFFSET, RING_HEAD_OFFSET, RING_TAIL_OFFSET};

use crate::blob::HostBlob;
use std::sync::atomic::{AtomicU32, Ordering};

/// Why a relayed ring delta was refused.
///
/// Every field of a `C2S::RingDelta` arrives over the network and is therefore attacker-controlled.
/// These are the ways one can fail to describe something Mesa could have produced — and each is
/// refused rather than clamped, because a delta that does not mean what it says cannot be
/// *partially* honoured: writing it at all would desynchronize S's ring frontier from C's, and every
/// later delta would then land at the wrong offset. A corrupt Vulkan command stream, decoded on a
/// real GPU, is not a recoverable condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RingDeltaError {
    /// The delta's `tail` and its byte count disagree.
    #[error(
        "ring delta claims tail {claimed} (i.e. {advance} new bytes since {previous}) but carries \
         {carried}; the two must agree exactly or S's ring frontier desynchronizes from C's"
    )]
    LengthMismatch {
        /// The `tail` the message claimed.
        claimed: u32,
        /// S's frontier before this delta.
        previous: u32,
        /// How many bytes `claimed` implies.
        advance: u32,
        /// How many bytes actually arrived.
        carried: usize,
    },

    /// The delta is bigger than the ring's entire command buffer.
    ///
    /// Mesa cannot produce this: its producer refuses to write past `head + buffer_size`
    /// (`vn_ring_has_space`, `vn_ring.c:213`). So it is a broken or hostile C, and writing it would
    /// run past the buffer region into the `extra` word and beyond.
    #[error(
        "ring delta of {advance} bytes exceeds the ring's {buffer_size}-byte command buffer; Mesa \
         cannot produce this (vn_ring_has_space refuses to write past head + buffer_size)"
    )]
    LongerThanBuffer {
        /// How many bytes the delta carries.
        advance: u32,
        /// The ring's command-buffer size.
        buffer_size: u32,
    },

    /// The blob is too small for the ring layout claimed of it.
    ///
    /// Unreachable for a mirror built by [`RingMirror::new`] from the same blob size, but checked
    /// rather than assumed: every offset computed below would otherwise address memory outside the
    /// mapping.
    #[error(
        "ring blob is {blob_size} bytes but the ring layout needs {required} ({RING_BUFFER_OFFSET} \
         control + {buffer_size} buffer)"
    )]
    BlobTooSmall {
        /// The blob's actual size.
        blob_size: u64,
        /// What the layout requires.
        required: u64,
        /// The ring's command-buffer size.
        buffer_size: u32,
    },
}

/// S's mirror of one Venus command ring: where its frontier stands, and how to lay bytes into it.
///
/// # What it owns and does not own
/// It owns the *frontier* — how much of C's byte stream has been written into S's ring — and the
/// last `head` reported to C. It does **not** own the ring memory: every method takes the
/// [`HostBlob`] as an argument. That split is what lets this be tested against a plain memfd with no
/// GPU, no virglrenderer and no network.
///
/// # Pitfall: never construct two mirrors for one ring
/// `applied_tail` is the only record of where the next delta belongs. Two mirrors would each write
/// the same bytes at the same offset and then both advance `tail`, publishing a frontier over bytes
/// written twice and commands never written at all.
pub struct RingMirror {
    /// The command buffer's size in bytes (128 KiB in every capture). Mesa asserts this is a power
    /// of two, and virglrenderer refuses a ring whose buffer is not
    /// (`vkr_ring_layout_init`, `vkr_transport.c:166-171`).
    buffer_size: u32,
    /// `buffer_size - 1`: the mask that turns a free-running counter into a buffer offset, exactly
    /// as virglrenderer's `buf->mask` does.
    buffer_mask: u32,
    /// How much of C's byte stream has been written into S's ring. Free-running like `tail` itself,
    /// and compared with wrapping arithmetic so the 2^32 counter overflow needs no special case.
    applied_tail: u32,
    /// The last `head` value reported to C. Progress is reported on *movement* only; see
    /// [`Self::take_progress`].
    reported_head: u32,
}

impl RingMirror {
    /// Create a mirror for a ring whose command buffer is `buffer_size` bytes.
    ///
    /// The frontier starts at 0, matching a ring virglrenderer has just created: `vkr_ring_create`
    /// refuses a ring whose `head` or `status` is non-zero at creation (`vkr_ring_init_control`,
    /// `vkr_ring.c:44-58`), and a freshly allocated blob is zeroed.
    ///
    /// # Panics
    /// If `buffer_size` is not a power of two or is zero. This is not a wire-supplied value — it is
    /// derived from the blob's own size by
    /// [`RingIdentity::from_blob_request`](rayland_vtest::venus_ring::RingIdentity::from_blob_request),
    /// which already establishes the property — so a violation here is a bug in this crate rather
    /// than a hostile peer, and every offset computed from `buffer_mask` would be silently wrong
    /// rather than merely slow.
    pub fn new(buffer_size: u32) -> Self {
        assert!(
            buffer_size.is_power_of_two(),
            "ring buffer size {buffer_size} is not a power of two, so `tail & buffer_mask` cannot \
             be the buffer offset; virglrenderer itself refuses such a ring \
             (vkr_ring_layout_init, vkr_transport.c:166-171)"
        );
        RingMirror {
            buffer_size,
            // Valid precisely because of the assert above.
            buffer_mask: buffer_size - 1,
            applied_tail: 0,
            reported_head: 0,
        }
    }

    /// The command buffer's size in bytes.
    pub fn buffer_size(&self) -> u32 {
        self.buffer_size
    }

    /// How much of C's byte stream has been laid into S's ring.
    pub fn applied_tail(&self) -> u32 {
        self.applied_tail
    }

    /// Write a relayed delta into S's ring memory and publish the new `tail`.
    ///
    /// **This is the function the whole sub-project exists to make work**: its argument *is* the
    /// application's Vulkan command stream, and this is where it becomes something S's GPU will run.
    ///
    /// # The wrap, and why S has to re-do one C already undid
    /// `bytes` arrives **already un-wrapped**: `rayland-c`'s `RingWatcher::take_delta` joins the two
    /// halves of a straddling delta in producer order, so the wire carries one contiguous run and no
    /// consumer downstream has to know a wrap happened. S is the exception, because S is writing
    /// into a *circular buffer* again. virglrenderer's consumer masks its cursor
    /// (`vkr_ring_read_buffer`, `vkr_ring.c:83-99`) and will look for the run's second half at the
    /// buffer's **start**, so this function re-splits it there — mirroring Mesa's own producer
    /// (`vn_ring_write_buffer`, `vn_ring.c:127-142`) exactly.
    ///
    /// Ring-findings §8 records that **no live run has ever reached a wrap** (peak `tail` was 7.58%
    /// of the buffer), so this arithmetic is untested against real Mesa and is pinned by a unit test
    /// instead.
    ///
    /// # Ordering
    /// The buffer bytes are written first, then `tail` is stored with [`Ordering::Release`]. That is
    /// load-bearing: it is the store virglrenderer's `Acquire` load of `tail` pairs with, and it is
    /// what makes the bytes visible to the ring thread. Reversed, the thread could see a frontier
    /// covering bytes it has not been shown.
    ///
    /// # Inputs / outputs
    /// - `blob`: the ring blob's mapping.
    /// - `tail`: the free-running byte counter *after* this delta.
    /// - `bytes`: the bytes in `[applied_tail, tail)`, already un-wrapped by C.
    /// - Returns `Ok(())`, having advanced the frontier, or a [`RingDeltaError`] having written
    ///   nothing at all.
    ///
    /// # Failure modes
    /// Every field is remote. See [`RingDeltaError`]: a delta whose length contradicts its `tail`,
    /// one longer than the whole buffer, or a blob too small for the layout. In every case **nothing
    /// is written and `tail` is not published**, because a half-applied delta is worse than a
    /// refused one.
    ///
    /// # What this does *not* check, and why that is safe by an inherited invariant rather than a
    /// local one
    /// This function checks a delta against `self.buffer_size` (the `LongerThanBuffer` guard above),
    /// but it never checks the *cumulative* distance between `self.applied_tail` and the ring
    /// thread's `head` — i.e. nothing here refuses a delta merely because applying it would advance
    /// `tail` past `head + buffer_size` and overwrite bytes the ring thread has not consumed yet.
    /// That is not an oversight: it is safe only because C's own flow control already guarantees it
    /// never happens for a well-behaved peer, so the guarantee is *inherited*, not established here.
    ///
    /// Mesa's producer refuses to advance its local `tail` past that same bound in the first place
    /// (`vn_ring_has_space`, `vn_ring.c:206-215`) — and `rayland-c`'s relay only ever forwards deltas
    /// that a real `vn_ring_has_space`-gated Mesa produced, so C's `tail` (and therefore every
    /// `RingDelta.tail` S ever receives from a correct C) can never lap S's `head`. **A broken or
    /// hostile C is a different story**: nothing on the wire proves the sender actually ran Mesa's
    /// check, so a lapping delta from such a peer is a real possibility this function does not
    /// itself rule out. Its consequence is bounded and detected rather than silent, though: every
    /// write here still lands inside `[RING_BUFFER_OFFSET, RING_BUFFER_OFFSET + buffer_size)` (this
    /// function's own bounds checks guarantee that much regardless of what a hostile C claims), and
    /// virglrenderer's ring thread independently raises `FATAL` if it ever computes a `cmd_size`
    /// larger than the buffer (`vkr_ring_thread`, `vkr_ring.c:291-296`). So a hostile C can corrupt
    /// unconsumed ring bytes and crash the render-server subprocess; it cannot make this function
    /// write outside the mapping.
    pub fn apply_delta(
        &mut self,
        blob: &mut HostBlob,
        tail: u32,
        bytes: &[u8],
    ) -> Result<(), RingDeltaError> {
        // The declared layout must actually fit in the blob, or every offset below addresses memory
        // outside the mapping.
        let required = RING_BUFFER_OFFSET as u64 + self.buffer_size as u64;
        if blob.size() < required {
            return Err(RingDeltaError::BlobTooSmall {
                blob_size: blob.size(),
                required,
                buffer_size: self.buffer_size,
            });
        }

        // How far this delta claims to move the frontier. Wrapping subtraction is not a defensive
        // flourish: it is what makes the 2^32 counter overflow a non-event, exactly as it is for
        // Mesa's and virglrenderer's own occupancy arithmetic (both compute a *difference*).
        let advance = tail.wrapping_sub(self.applied_tail);

        // The message must mean what it says. `tail` and `bytes` are independent wire fields, and
        // trusting one over the other would silently shift every subsequent delta by the difference.
        if advance as usize != bytes.len() {
            return Err(RingDeltaError::LengthMismatch {
                claimed: tail,
                previous: self.applied_tail,
                advance,
                carried: bytes.len(),
            });
        }

        // A duplicate of a delta already applied. Nothing to write, and `tail` already stands where
        // it should — republishing it would only dirty a cache line the ring thread polls.
        if advance == 0 {
            return Ok(());
        }

        if advance > self.buffer_size {
            return Err(RingDeltaError::LongerThanBuffer {
                advance,
                buffer_size: self.buffer_size,
            });
        }

        // Where these bytes physically start. This mask — applied at access time, never in storage —
        // is the whole of virglrenderer's `buf->cur & buf->mask`.
        let start = (self.applied_tail & self.buffer_mask) as usize;
        // How many of them fit before the buffer's physical end. This comparison, not
        // `tail < applied_tail`, is what detects a wrap: a delta that straddles the end still has
        // `tail > applied_tail`, while `tail < applied_tail` means only the 2^32 counter overflow.
        let first_run = bytes.len().min(self.buffer_size as usize - start);

        // The tail-to-end half (or the whole delta, when it does not wrap). `copy_in`'s bounds are
        // already guaranteed by the `required` check plus `first_run`'s clamp, so a failure here is
        // unreachable; it is mapped to `BlobTooSmall` rather than unwrapped so that a future edit
        // which broke the arithmetic would surface as a refusal rather than a panic on S's daemon.
        let buffer_base = RING_BUFFER_OFFSET as u64;
        blob.copy_in(buffer_base + start as u64, &bytes[..first_run])
            .map_err(|_| RingDeltaError::BlobTooSmall {
                blob_size: blob.size(),
                required,
                buffer_size: self.buffer_size,
            })?;
        if bytes.len() > first_run {
            // The start-to-tail half: the second `memcpy` of Mesa's producer, and where
            // virglrenderer's masked cursor will look for it.
            blob.copy_in(buffer_base, &bytes[first_run..])
                .map_err(|_| RingDeltaError::BlobTooSmall {
                    blob_size: blob.size(),
                    required,
                    buffer_size: self.buffer_size,
                })?;
        }

        // Publish. `Release` is what orders the writes above before this store, and it is exactly
        // the store virglrenderer's `Acquire` load of `tail` expects to pair with. Until this line
        // executes, the ring thread computes `cmd_size == 0` and does nothing at all.
        self.tail_word(blob).store(tail, Ordering::Release);

        // Advance the frontier only after the bytes are down and published.
        self.applied_tail = tail;
        Ok(())
    }

    /// Read the ring's `head` and report it, but **only if it moved**.
    ///
    /// # Why this reads `head` rather than reporting the tail S was handed
    /// `head` is not a space counter — it is the **reply-ready signal**. Mesa's
    /// `vn_ring_get_seqno_status` is `vn_ring_ge_seqno(ring, vn_ring_load_head(ring), seqno)`
    /// (`vn_ring.c:176-179`) and `vn_ring_wait_seqno` busy-polls it, so a synchronous Vulkan call
    /// returns the moment `head` reaches its seqno — *on the understanding that the reply is then
    /// ready*. C advances its local `head` only from `S2C::RingProgress`, which means whatever this
    /// function returns is what releases the application's waits.
    ///
    /// So the only honest source is the word virglrenderer's ring thread actually wrote
    /// (`vkr_ring_store_head`, `vkr_ring.c:60-67`), which it does after each dispatched command.
    /// Reporting the relayed `tail` instead would release the application on a reply that had not
    /// been computed, and it would read an unwritten reply arena — non-deterministically, and
    /// nowhere near the cause (ring-findings §7).
    ///
    /// # Why movement, not arrival
    /// A `head` resent while it stands still proves only that S's process is being scheduled — the
    /// exact property ring-findings §5.4 calls worthless, and the reason Mesa's own watchdog cannot
    /// detect a stalled ring. C's stall detector already refuses to count a repeat as progress; S
    /// declines to manufacture one.
    ///
    /// # Inputs / outputs
    /// - `blob`: the ring blob's mapping.
    /// - Returns `Some(head)` the first time each new value is observed, `None` otherwise.
    pub fn take_progress(&mut self, blob: &HostBlob) -> Option<u32> {
        // `Acquire`, pairing with virglrenderer's `Release` store — the ordering its own comment
        // asks of a renderer ("the renderer is expected to load the head with memory_order_acquire,
        // forming a release-acquire ordering").
        let head = self.head_word(blob).load(Ordering::Acquire);
        if head == self.reported_head {
            return None;
        }
        self.reported_head = head;
        Some(head)
    }

    /// The ring's `tail` word, as an atomic.
    ///
    /// See [`Self::control_word`] for why this is sound.
    fn tail_word<'a>(&self, blob: &'a HostBlob) -> &'a AtomicU32 {
        self.control_word(blob, RING_TAIL_OFFSET)
    }

    /// The ring's `head` word, as an atomic.
    fn head_word<'a>(&self, blob: &'a HostBlob) -> &'a AtomicU32 {
        self.control_word(blob, RING_HEAD_OFFSET)
    }

    /// Reinterpret one of the ring's 32-bit control words as an [`AtomicU32`] living in the blob's
    /// shared pages.
    ///
    /// # Why this is sound, and how it actually compares to C
    /// The other accessor of these words is virglrenderer's ring thread — and that thread runs in
    /// the forked render-server subprocess `VIRGL_RENDERER_RENDER_SERVER` requires for Venus
    /// (`rayland-engine/src/ffi.rs`), not in this process. So this is **cross-process** sharing of a
    /// `u32`, the same topology `rayland-c` has with Mesa, and `AtomicU32` is sound here for the
    /// same reason it is sound anywhere `MAP_SHARED` pages a real atomic on both sides: `Release`/
    /// `Acquire` constrain the compiler and the CPU, not the address, and virglrenderer's ring
    /// thread genuinely uses C11 atomics on its side (`vkr_ring_load_tail` / `vkr_ring_store_head`).
    /// That is real ground `rayland-c` cannot stand on for Gap 1 (Mesa's side uses plain,
    /// non-atomic accesses), so this pairing is *more* rigorous than C's — but the pairing itself
    /// still rests on the same formally-unspecified-but-universally-honoured cross-process
    /// `MAP_SHARED` coherence C's does, not on being in the same address space.
    ///
    /// # Alignment
    /// [`HostBlob::as_ptr`] is `mmap`'s return value and therefore page-aligned, and the ring's
    /// control words sit at offsets 0x00 / 0x40 / 0x80 — each on its own 64-byte cache line, because
    /// Mesa declares them `alignas(64)` (ring-findings §4). So the resulting address is 64-byte
    /// aligned, comfortably satisfying `AtomicU32`'s 4-byte requirement. **A reader that assumed
    /// three adjacent dwords would be both misaligned and wrong**; the offsets come from
    /// `rayland-vtest`'s layout constants for that reason.
    ///
    /// # Panics
    /// Never for a blob that passed [`Self::apply_delta`]'s size check; the control area is the
    /// first 192 bytes of any ring-shaped blob.
    fn control_word<'a>(&self, blob: &'a HostBlob, offset: usize) -> &'a AtomicU32 {
        debug_assert!(
            offset + 4 <= blob.size() as usize,
            "control word at {offset} is outside a {}-byte blob",
            blob.size()
        );
        // SAFETY: `offset + 4 <= blob.size()`, so the address is inside the live mapping, which
        // outlives the returned reference (tied to `blob`'s borrow). The address is 64-byte aligned
        // — see this method's "Alignment" note — so it is validly aligned for `AtomicU32`. Every
        // other access to this word, here and in virglrenderer's ring thread, is atomic, so no
        // non-atomic access can race with it.
        unsafe { &*(blob.as_ptr().add(offset).cast::<AtomicU32>()) }
    }
}
