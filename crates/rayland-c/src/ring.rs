//! The **ring watcher**: the loop that notices Mesa wrote Vulkan commands into shared memory, and
//! decides when it is safe to stop looking.
//!
//! # Why this module is the heart of (c)1's client half
//! Sub-project C0 proved (`docs/design/2026-07-15-venus-ring-findings.md`) that Mesa's Venus ICD
//! does **not** send the application's Vulkan commands over the vtest socket. It `memcpy`s them
//! into a shared-memory ring and stores a new `tail`. That is the whole notification: no syscall,
//! no ioctl, no protocol message. 100% of the application's command stream travels this way and 0%
//! of it touches the socket.
//!
//! On one machine that is free — the host's ring thread reads the same physical pages. Across a
//! network there is no shared page, so *something* on C must notice the bytes changed and ship
//! them. Ring-findings §5.1 established there is no seam to hook: Venus's renderer abstraction has
//! coherence hooks (`bo_flush`/`bo_invalidate`) but they are nops in **both** backends, and the
//! ring is not even a `bo` — `vn_renderer_shmem_ops` has exactly two members, `create` and
//! `destroy`. There is no entry point that could be made into a notification.
//!
//! So the only mechanism left is the one this module implements: **poll `tail` locally**. That is
//! cheap (a 4-byte read of our own address space, no syscall) and it is what ring-findings §5.2
//! concluded a transport must do. The ring lives in the application's own process on C, so the
//! missing notification never has to cross the network at all.
//!
//! # The named hang bug, and the discipline that avoids it
//! Mesa *does* have a doorbell (`vkNotifyRingMESA`), and it is tempting to treat it as "there is
//! work". **It is not.** Mesa rings it only when it observes the IDLE bit set **and** at least 1 ms
//! has passed since the last kick (`vn_ring.c:475-483`):
//!
//! ```c
//! if (status & VK_RING_STATUS_IDLE_BIT_MESA) {
//!    const int64_t now = os_time_get_nano();
//!    if (os_time_timeout(ring->last_notify, ring->next_notify, now)) {
//!       ring->last_notify = now;
//!       ring->next_notify = now + VN_RING_IDLE_TIMEOUT_NS;
//!       return true;   /* the only path that sends vkNotifyRingMESA */
//!    }
//! }
//! return false;
//! ```
//!
//! Read that carefully: **a kick is not guaranteed for every write.** Both conditions can fail. So
//! a watcher that publishes IDLE and then sleeps, trusting a kick to wake it, will sleep through
//! pending work — and because the 1 ms throttle depends on timing, it will do so *intermittently*.
//!
//! The discipline this module enforces is therefore: **drain → publish IDLE → re-read `tail` → park
//! only if nothing new arrived.** [`RingWatcher::decide_park`] is that re-read, and it compares
//! against the drained frontier (`last_tail`), not against a snapshot taken when IDLE was
//! published — see its doc comment for why the difference is a hang.
//!
//! # Pitfall: the IDLE bit's polarity is the opposite of what it reads like
//! `status & 1` **set** means *the ring's consumer is parked*; `status == 0` means *actively
//! polling*. "1" looks like "busy" and means the reverse. This repository documented it backwards
//! in committed code until 2026-07-15, and the inverted reading is self-consistent enough to look
//! plausible — it simply makes Mesa kick a consumer that is already spinning, i.e. it wastes work
//! instead of hanging, which is exactly why it survived review. The authority is Mesa's generated
//! header (`vn_protocol_renderer_defines.h:473-478`): `IDLE = 0x1`, `FATAL = 0x2`, `ALIVE = 0x4`.
//!
//! # Pitfall: `head` and `tail` are free-running counters, not offsets
//! This is the single easiest thing to get wrong here, so it is stated with its source. Mesa's
//! producer (`vn_ring_write_buffer`, `vn_ring.c:127-142`) is:
//!
//! ```c
//! const uint32_t offset = ring->cur & ring->buffer_mask;
//! if (offset + size <= ring->buffer_size) {
//!    memcpy(ring->shared.buffer + offset, data, size);
//! } else {
//!    const uint32_t s = ring->buffer_size - offset;
//!    memcpy(ring->shared.buffer + offset, data, s);   /* tail-to-end */
//!    memcpy(ring->shared.buffer, data + s, size - s); /* start-to-tail */
//! }
//! ring->cur += size;
//! ```
//!
//! `ring->cur` *is* `tail`. It is incremented and **never masked in storage**; the mask is applied
//! only at access time. Two consequences, both load-bearing:
//!
//! - **A wrap is `offset + size > buffer_size`, not `tail < last_tail`.** A delta that straddles
//!   the buffer's physical end still has `tail > last_tail`. Using `tail < last_tail` as the wrap
//!   test would take a linear slice running off the end of the ring and panic the first time a real
//!   application wraps.
//! - **`tail < last_tail` signals only the 2^32 counter overflow**, once per 4 GiB of commands.
//!   Wrapping arithmetic handles it for free, which is precisely why Mesa computes occupancy as a
//!   *difference* (`vn_ring_has_space`, `vn_ring.c:213`) rather than a comparison.
//!
//! [`RingWatcher::take_delta`] mirrors that producer exactly, in both halves.
//!
//! # Known gap: memory ordering (read this before running on aarch64 or riscv64)
//! Mesa stores `tail` with `memory_order_seq_cst` and expects its consumer to load it with at least
//! **Acquire** ordering: that load is what makes the `memcpy`ed command bytes written *before* the
//! store visible to the reader. This module reads the control words through a plain `&[u8]` with
//! `u32::from_le_bytes`, which is **not** an atomic acquire load.
//!
//! In practice this works on x86-64 — C0's only tested target — because aligned 32-bit loads have
//! acquire semantics in that hardware's memory model, leaving compiler reordering as the only
//! risk. It is nonetheless a formal data race, and on a **weakly-ordered target it can genuinely
//! reorder**: the reader could observe the new `tail` before the buffer bytes it published, and
//! ship uninitialized memory to S's GPU as Vulkan commands. CLAUDE.md names RISC-V as an explicit
//! target for machine C, so this must be closed before C runs anywhere but x86-64. It is recorded
//! here rather than fixed because a correct fix is a cross-process atomics question this task did
//! not scope, and a half-correct one would look solved. **This gap is about the ordering of the
//! accesses, not about the logic below**, which is why the logic is testable exactly as written.

// The ring's field offsets, and the buffer size the observed client declared. These come from
// `rayland-vtest` (which links no GPU code) rather than being restated here, so there is exactly
// one copy of the repository's ring knowledge.
use rayland_vtest::venus_ring::{
    RING_BUFFER_OFFSET, RING_HEAD_OFFSET, RING_STATUS_OFFSET, RING_TAIL_OFFSET,
};

/// `VK_RING_STATUS_IDLE_BIT_MESA` — bit 0 of the ring's `status` word.
///
/// **Set means the ring's consumer is parked**, and it is the bit Mesa tests before deciding
/// whether a doorbell is even worth sending. See the module docs' polarity pitfall: this reads
/// backwards to almost everyone, including this repository until 2026-07-15.
const RING_STATUS_IDLE_BIT: u32 = 0x0000_0001;

/// The control area's size in bytes: three 64-byte-aligned words (`head`, `tail`, `status`).
///
/// Not imported from `rayland-vtest` as a named constant because none exists there; it is
/// `RING_BUFFER_OFFSET` by construction, and stated here as the *meaning* of that offset for
/// [`RingIdentity::from_blob_request`]'s arithmetic.
const RING_CONTROL_BYTES: u64 = RING_BUFFER_OFFSET as u64;

/// The `extra` region's size in bytes: one dword after the command buffer.
///
/// Ring-findings §4.2 established it is **vestigial** in Mesa 26.0.3 — declared to the host, mapped
/// by the client, and read by nothing (a grep of all 48 files in Mesa's `src/virtio/vulkan/` finds
/// `shared.extra` only at its assignment). It is accounted for here because it is part of the size
/// arithmetic that identifies a ring, not because it is known to be useful.
const RING_EXTRA_BYTES: u64 = 4;

/// Which blob is the command ring, and how big its buffer is.
///
/// # Why this has to be inferred at all, and the honest status of the inference
/// The ring's layout is **declared in-band**, by the client, in `vkCreateRingMESA`'s
/// `VkRingCreateInfoMESA` — precisely so a host need not know it a priori (ring-findings §4). The
/// rigorous way to obtain this is therefore to parse that command out of the inline
/// [`rayland_relay::C2S::SubmitCmd`] stream, and `rayland-vtest`'s ring constants say plainly that a
/// production reader must do exactly that.
///
/// [`Self::from_blob_request`] does **not** do that. It recognizes the ring by the shape of its
/// allocation request, which is a *heuristic* — a good one, and a documented one, but a heuristic.
/// It is recorded as such rather than dressed up, because the day Mesa picks a different buffer size
/// this silently stops finding the ring and (c)1 relays nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingIdentity {
    /// The S-side resource id of the ring blob.
    pub res_id: u32,
    /// The command buffer's size in bytes, derived from the blob's total size.
    pub buffer_size: u32,
}

impl RingIdentity {
    /// Decide whether a blob allocation request describes a Venus command ring, and if so recover
    /// its buffer size.
    ///
    /// # How the recognition works, and why it is trustworthy enough for now
    /// This is the same reasoning that *found* the ring in the first place (ring-findings §4): the
    /// client's first blob was **131268** bytes, and that number is a 128 KiB power-of-two buffer
    /// plus a 196-byte remainder — *a non-power-of-two remainder next to a power-of-two buffer is
    /// what a header looks like*. The decomposition closes exactly:
    ///
    /// ```text
    ///   192 (control) + 131072 (128 KiB command buffer) + 4 (extra) = 131268
    /// ```
    ///
    /// So a request is taken to be the ring when `size - 196` is a non-zero power of two, and when
    /// `blob_id == 0`. The second condition is the discriminator ring-findings §6 found to be clean:
    /// `blob_id == 0` marks Venus's *internal* shmems (ring, reply arena, staging pool), while a
    /// non-zero id marks an application `VkDeviceMemory` allocation. An application is free to
    /// allocate a buffer whose size happens to decompose this way — 131268 bytes of vertex data is
    /// perfectly legal — and `blob_id` is what stops that from being mistaken for a ring.
    ///
    /// Checked against every blob the live capture observed (ring-findings §6), this matches the
    /// ring and nothing else: the 1 MiB reply arena, the 8 MiB staging pool, and the 64/4096/16384
    /// byte application buffers all fail the power-of-two test on `size - 196`.
    ///
    /// # Inputs / outputs
    /// - `res_id`: the S-side resource id assigned to this blob.
    /// - `blob_id`: the client-chosen blob id from the wire message.
    /// - `size`: the blob's total size in bytes.
    /// - Returns `Some(identity)` if this looks like a ring, `None` otherwise.
    ///
    /// # Pitfall: a false negative is silent
    /// If this fails to recognize the real ring, `rayland-c` watches nothing, relays nothing, and
    /// the application hangs on its first synchronous call with no error anywhere. That is why the
    /// daemon logs the identification rather than performing it quietly.
    pub fn from_blob_request(res_id: u32, blob_id: u64, size: u64) -> Option<Self> {
        // Venus's own shmems only. An application buffer that happens to be ring-shaped is not a
        // ring, and `blob_id` is the signal that separates them.
        if blob_id != 0 {
            return None;
        }
        // Strip the header and the vestigial tail; whatever remains must be the command buffer.
        let buffer = size.checked_sub(RING_CONTROL_BYTES + RING_EXTRA_BYTES)?;
        // The power-of-two property is not decoration: Mesa asserts it, because it is what makes
        // `tail & buffer_mask` a valid substitute for `tail % buffer_size`. A blob whose remainder
        // is not a power of two cannot be a ring Mesa produced.
        if buffer == 0 || !buffer.is_power_of_two() {
            return None;
        }
        // A ring buffer larger than u32 is not something Mesa can address with a 32-bit counter.
        let buffer_size = u32::try_from(buffer).ok()?;
        Some(RingIdentity {
            res_id,
            buffer_size,
        })
    }
}

/// A contiguous run of command bytes Mesa produced, already reassembled across any buffer wrap.
///
/// The bytes are the Venus command language verbatim — the same `vn_cs_encoder` output the vtest
/// socket's inline path carries (ring-findings §3, proven twice independently). Nothing in this
/// crate parses them; they are payload, and S's virglrenderer is what decodes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingDelta {
    /// The ring's `tail` counter *after* this delta — the free-running byte count, not a buffer
    /// index. S applies `bytes` and advances its own mirror of `tail` to exactly this value.
    pub tail: u32,
    /// The bytes Mesa wrote in the half-open range `[previous_tail, tail)`, in the order it wrote
    /// them. If the range straddled the buffer's physical end, the two halves are already joined
    /// here in producer order, so a consumer never has to know a wrap happened.
    pub bytes: Vec<u8>,
}

/// Whether the watcher may sleep, as decided by [`RingWatcher::decide_park`].
///
/// This exists as a type rather than a `bool` because the two outcomes are not symmetric and the
/// asymmetry is the whole point: [`ParkDecision::Park`] is a promise that nothing is pending, and
/// getting that promise wrong is an indefinite hang rather than a slow path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkDecision {
    /// Nothing new was produced since the last drain. IDLE stands, and the watcher may sleep until
    /// Mesa's doorbell (or its own timeout) wakes it.
    Park,
    /// Mesa produced bytes that have not been drained. IDLE has been cleared and the watcher must
    /// loop back to [`RingWatcher::take_delta`] immediately rather than sleep.
    StayAwake,
}

/// Watches one Venus command ring: tracks how much of it has been relayed, extracts the new bytes,
/// and owns the park/wake decision.
///
/// # What it does and does not own
/// It owns the *frontier* — how far into Mesa's byte stream we have read — and the two control
/// words C is responsible for writing (`head` and `status`). It does **not** own the ring memory:
/// every method takes the ring's bytes as an argument. That is deliberate, and it is what lets the
/// hang bug be tested against a `vec![0u8; 131268]` with no GPU, no Mesa and no network. A stall
/// found only in a live drive looks like a network problem.
///
/// # Pitfall: this type must never be cloned or duplicated for one ring
/// `last_tail` is the *only* record of what has already been shipped. Two watchers on one ring
/// would each relay the same bytes, and duplicate Vulkan commands replayed on S's GPU are not a
/// performance problem — they are a correctness one.
pub struct RingWatcher {
    /// The S-side resource id of this ring, stamped onto every [`rayland_relay::C2S::RingDelta`] so
    /// S knows which of its resources the bytes belong to.
    res_id: u32,
    /// The command buffer's size in bytes (128 KiB in every capture). Mesa asserts this is a power
    /// of two so the buffer index is a cheap mask.
    buffer_size: u32,
    /// `buffer_size - 1`: the mask that turns a free-running counter into a buffer offset, exactly
    /// as Mesa's `ring->buffer_mask` does.
    buffer_mask: u32,
    /// How far into Mesa's byte stream we have drained. Free-running like `tail`, and compared with
    /// wrapping arithmetic so the 2^32 overflow needs no special case.
    last_tail: u32,
}

impl RingWatcher {
    /// Create a watcher for the ring resource `res_id`, whose command buffer is `buffer_size` bytes.
    ///
    /// # Inputs / outputs
    /// - `res_id`: the S-side resource id of the ring blob, used to address relayed deltas.
    /// - `buffer_size`: the command buffer's size in bytes, as the client declared it in
    ///   `vkCreateRingMESA`. Must be a power of two.
    /// - Returns a watcher whose frontier starts at 0, matching a freshly created ring.
    ///
    /// # Panics
    /// If `buffer_size` is not a power of two or is zero. Mesa itself asserts the power-of-two
    /// property (it is what makes `cur & buffer_mask` a valid substitute for `cur % buffer_size`),
    /// so a violation here means the declared layout is not a ring Mesa could have produced, and
    /// every offset this type computes would be silently wrong rather than merely slow.
    pub fn new(res_id: u32, buffer_size: u32) -> Self {
        assert!(
            buffer_size.is_power_of_two(),
            "ring buffer size {buffer_size} is not a power of two, so `tail & buffer_mask` cannot \
             be the buffer offset; Mesa asserts this property and a ring without it is malformed"
        );
        RingWatcher {
            res_id,
            buffer_size,
            // Valid precisely because of the assert above.
            buffer_mask: buffer_size - 1,
            // A fresh ring has produced nothing, and Mesa's `cur` likewise starts at 0.
            last_tail: 0,
        }
    }

    /// Create a watcher whose frontier already stands at `last_tail`, rather than at 0.
    ///
    /// # Why this exists
    /// Two reasons, and the honest one first. **It is what makes the 2^32 counter-overflow case
    /// testable at all.** Reaching that frontier through [`Self::take_delta`] would mean actually
    /// relaying 4 GiB of commands, so a test for the overflow either gets to construct the state
    /// directly or does not exist — and "the arithmetic that runs once per 4 GiB" is precisely the
    /// code that will never be exercised before it matters. [`Self::new`]'s overrun guard is not
    /// wrong to reject a 4-billion-byte first delta; the frontier simply has to be seeded.
    ///
    /// The second reason is prospective: a session that reconnects to a ring already in flight
    /// needs exactly this. (c)1 has no reconnect path today, so that is a note, not a claim.
    ///
    /// # Pitfall
    /// `last_tail` asserts that everything below it has **already been relayed to S**. Seeding it
    /// past what S has actually received silently skips those bytes — the commands are simply never
    /// sent, and S decodes the following ones as though nothing were missing.
    ///
    /// # Panics
    /// If `buffer_size` is not a power of two — see [`Self::new`].
    pub fn resuming_at(res_id: u32, buffer_size: u32, last_tail: u32) -> Self {
        let mut watcher = Self::new(res_id, buffer_size);
        watcher.last_tail = last_tail;
        watcher
    }

    /// The S-side resource id of the ring this watcher is following.
    pub fn res_id(&self) -> u32 {
        self.res_id
    }

    /// The frontier: how far into Mesa's byte stream this watcher has drained.
    pub fn last_tail(&self) -> u32 {
        self.last_tail
    }

    /// Take every byte Mesa has produced since the last call, reassembling across a buffer wrap,
    /// and advance the frontier past them.
    ///
    /// This is the function the whole sub-project exists to make work: its output *is* the
    /// application's Vulkan command stream.
    ///
    /// # How the wrap is handled (mirroring `vn_ring_write_buffer` exactly)
    /// The number of new bytes is `tail - last_tail` under **wrapping** arithmetic, which is
    /// correct across the 2^32 counter overflow for the same reason Mesa's own occupancy check is.
    /// Their location starts at `last_tail & buffer_mask`; if that run reaches the buffer's
    /// physical end, the remainder continues from the buffer's start. See the module docs for why
    /// `tail < last_tail` is *not* the wrap test.
    ///
    /// # Inputs / outputs
    /// - `ring`: the ring blob's bytes — control words and command buffer, exactly as Mesa's
    ///   `vkCreateRingMESA` laid them out. Must be at least `RING_BUFFER_OFFSET + buffer_size`
    ///   long.
    /// - Returns `Some(delta)` with the new bytes, or `None` if `tail` has not moved (the common
    ///   case in a poll loop — an idle ring produces nothing).
    ///
    /// # Panics
    /// - If `ring` is too short to contain the declared buffer. This means the blob and the
    ///   declared layout disagree, so any slice computed from the layout would read the wrong
    ///   memory rather than fail.
    /// - If Mesa appears to have produced more than `buffer_size` bytes since the last drain. That
    ///   is impossible while `head` is honest — Mesa's producer asserts
    ///   `cur + size - head <= buffer_size` and refuses to write past it (`vn_ring_has_space`,
    ///   `vn_ring.c:213`), and [`Self::advance_head`] never publishes a head beyond the drained
    ///   frontier. If it happens anyway we have *already* lost command bytes irrecoverably, and
    ///   every byte shipped afterwards would be garbage decoded as Vulkan on S's GPU. Failing
    ///   loudly here names the cause; continuing would surface it as an inexplicable GPU error far
    ///   away from it.
    pub fn take_delta(&mut self, ring: &[u8]) -> Option<RingDelta> {
        // The declared layout must actually fit in the blob, or every offset below is nonsense.
        let required = RING_BUFFER_OFFSET + self.buffer_size as usize;
        assert!(
            ring.len() >= required,
            "ring blob is {} bytes but the declared layout needs {required} \
             ({RING_BUFFER_OFFSET} control + {} buffer)",
            ring.len(),
            self.buffer_size
        );

        // Mesa's write frontier. Under the module's memory-ordering gap this is a plain load; a
        // rigorous reader would make it an Acquire load, which is what would guarantee the bytes
        // below are visible.
        let tail = read_control(ring, RING_TAIL_OFFSET);

        // How much was produced since we last looked. Wrapping subtraction is not a defensive
        // flourish: it is what makes the 2^32 counter overflow a non-event, exactly as it is for
        // Mesa's own `cur + size - head` occupancy arithmetic.
        let produced = tail.wrapping_sub(self.last_tail);
        if produced == 0 {
            // The overwhelmingly common case in a poll loop.
            return None;
        }
        assert!(
            produced <= self.buffer_size,
            "the client produced {produced} bytes since the last drain, which exceeds the \
             {}-byte ring buffer: command bytes have already been overwritten and lost. This is \
             unreachable while `head` is honest (Mesa refuses to write past head + buffer_size), \
             so it means `advance_head` published a frontier that was never relayed",
            self.buffer_size
        );

        // Where those bytes physically start. This mask — applied at access time, never in storage
        // — is the whole of `ring->cur & ring->buffer_mask`.
        let start = (self.last_tail & self.buffer_mask) as usize;
        let produced = produced as usize;
        // How many of them fit before the buffer's physical end. This comparison, not
        // `tail < last_tail`, is what detects a wrap.
        let first_run = produced.min(self.buffer_size as usize - start);

        let mut bytes = Vec::with_capacity(produced);
        // The tail-to-end half (or the whole delta, when it does not wrap).
        bytes.extend_from_slice(
            &ring[RING_BUFFER_OFFSET + start..RING_BUFFER_OFFSET + start + first_run],
        );
        if produced > first_run {
            // The start-to-tail half: Mesa's second `memcpy`. Appending it here is what lets every
            // consumer downstream stay ignorant of the wrap.
            bytes.extend_from_slice(
                &ring[RING_BUFFER_OFFSET..RING_BUFFER_OFFSET + (produced - first_run)],
            );
        }

        // Advance the frontier only after the bytes are safely copied out: `last_tail` is the sole
        // record of what has been shipped, so it must never run ahead of what we actually hold.
        self.last_tail = tail;
        Some(RingDelta { tail, bytes })
    }

    /// Set the IDLE bit, announcing to Mesa that this ring's consumer is about to stop polling and
    /// therefore needs a doorbell to be woken.
    ///
    /// This is one half of a two-step protocol and is **never safe on its own**: publishing IDLE
    /// and sleeping is precisely the hang the module docs describe. It must always be followed by
    /// [`Self::decide_park`], whose re-read of `tail` is what closes the race.
    ///
    /// The bit is OR'ed in rather than assigned, mirroring Mesa's `atomic_fetch_or`: `status` also
    /// carries `FATAL` (0x2) and `ALIVE` (0x4), and clobbering the word would destroy them.
    ///
    /// # Inputs / outputs
    /// - `ring`: the ring blob's bytes, mutable because `status` is a word C writes and Mesa reads.
    /// - Nothing is returned; the effect is the published bit.
    pub fn publish_idle(&mut self, ring: &mut [u8]) {
        let status = read_control(ring, RING_STATUS_OFFSET);
        // Preserve FATAL/ALIVE; only claim the IDLE bit.
        write_control(ring, RING_STATUS_OFFSET, status | RING_STATUS_IDLE_BIT);
    }

    /// Re-read `tail` after IDLE was published and decide whether sleeping is actually safe.
    ///
    /// **This function is the fix for the named hang bug.** Mesa's kick is throttled to at most one
    /// per millisecond and is sent only when IDLE is already visible (`vn_ring.c:475-483`), so a
    /// write that lands in the window between our drain and our sleep may produce **no doorbell at
    /// all**. Nothing will ever wake us, and because the window's width depends on scheduling, the
    /// stall is intermittent — the worst way to find out.
    ///
    /// # The comparison is against `last_tail`, and that is not an arbitrary choice
    /// The dangerous window opens the moment [`Self::take_delta`] reads `tail`, not the moment IDLE
    /// is published. An implementation that snapshotted `tail` inside [`Self::publish_idle`] and
    /// compared against *that* would be blind to anything Mesa wrote between the drain and the
    /// publish: both reads would return the same value, and it would park on top of pending work.
    /// The question that is actually correct is **"is there anything I have not drained?"** — so
    /// the comparison is against the drained frontier. `tests/ring_watch.rs` pins both halves of
    /// this window with separate tests, because the two implementations agree on one and disagree
    /// on the other.
    ///
    /// # Inputs / outputs
    /// - `ring`: the ring blob's bytes. Mutable because refusing to park must also **clear** the
    ///   IDLE bit we just published — leaving it set would make Mesa keep paying for doorbells to
    ///   wake a watcher that is demonstrably awake.
    /// - Returns [`ParkDecision::Park`] only if nothing new was produced.
    pub fn decide_park(&mut self, ring: &mut [u8]) -> ParkDecision {
        // The re-read. Everything about this function is in service of it happening *after* IDLE
        // became visible to Mesa.
        let tail = read_control(ring, RING_TAIL_OFFSET);
        if tail == self.last_tail {
            // Nothing pending: IDLE stands, and Mesa now knows a doorbell is required.
            return ParkDecision::Park;
        }
        // Work arrived. Retract the IDLE claim before looping, so Mesa stops treating us as parked.
        let status = read_control(ring, RING_STATUS_OFFSET);
        write_control(ring, RING_STATUS_OFFSET, status & !RING_STATUS_IDLE_BIT);
        ParkDecision::StayAwake
    }

    /// Publish `upto` as the ring's `head`: the byte count C has consumed, which is what tells Mesa
    /// how much ring space is free.
    ///
    /// Mesa's producer refuses to write past `head + buffer_size` (`vn_ring_has_space`,
    /// `vn_ring.c:213`), so `head` is the *only* thing that lets a long-running ring make progress
    /// instead of filling up permanently.
    ///
    /// # Domain pitfall: `head` means more than "space is free"
    /// Ring-findings §7 records that `head` gates three things on the client's critical path: flow
    /// control, **every seqno wait**, and shmem retirement. In particular `vn_ring_wait_seqno`
    /// (`vn_ring.c:181-198`) busy-polls this word, and a synchronous Vulkan call returns as soon as
    /// `head` reaches its submission's seqno — on the understanding that the reply is then ready.
    /// So publishing a `head` here is not merely a hint about buffer space: it is a claim that
    /// everything below `upto` has been **executed and replied to**. Advancing it on the strength
    /// of having merely *relayed* the bytes releases the application's wait before S has answered,
    /// and it reads a stale reply arena. See `main.rs`'s watcher for how this is sequenced.
    ///
    /// # Inputs / outputs
    /// - `ring`: the ring blob's bytes, mutable because `head` is C's word to write.
    /// - `upto`: the free-running byte count to publish.
    ///
    /// # Panics
    /// If `upto` is ahead of the drained frontier. Mesa would then be free to overwrite bytes this
    /// watcher has never even read, losing Vulkan commands silently — a corrupt stream on S with no
    /// trace of where it went wrong. The check is a wrapping difference, so it stays correct across
    /// the 2^32 counter overflow.
    pub fn advance_head(&mut self, ring: &mut [u8], upto: u32) {
        // If `upto` is at or behind the frontier, this difference is a small number (at most the
        // buffer size). If it is ahead, it wraps to something enormous — which is exactly the
        // condition we refuse, and the wrapping form is what keeps that true past the 2^32 overflow.
        assert!(
            self.last_tail.wrapping_sub(upto) <= self.buffer_size,
            "refusing to advance head to {upto}, which is past the frontier {} this watcher has \
             actually relayed: Mesa would be free to overwrite command bytes that were never \
             shipped, and the loss would surface only as a corrupt stream on S",
            self.last_tail
        );
        write_control(ring, RING_HEAD_OFFSET, upto);
    }
}

/// Read one of the ring's 32-bit control words.
///
/// The words are little-endian (vtest is host-endian, and every target C realistically runs on —
/// x86-64, aarch64, riscv64 — is little-endian; a live capture confirmed LE on x86-64).
///
/// Each word sits in its **own 64-byte slot** because Mesa declares them `alignas(64)`: `head` and
/// `tail` are written by different threads on different sides of the mapping, and packing them into
/// one cache line would turn every doorbell into a false-sharing storm. Callers must therefore pass
/// the real offsets (`0x00`, `0x40`, `0x80`) — a reader that assumes three adjacent dwords reads
/// garbage.
///
/// See the module docs' memory-ordering gap: this is a plain load, not an Acquire one.
fn read_control(ring: &[u8], offset: usize) -> u32 {
    // `try_into` cannot fail: the slice is exactly 4 bytes by construction. Indexing panics with a
    // clear range if a caller passes an offset outside the blob, which beats reading adjacent
    // memory and calling it a control word.
    u32::from_le_bytes(
        ring[offset..offset + 4]
            .try_into()
            .expect("a 4-byte control word"),
    )
}

/// Write one of the ring's 32-bit control words. See [`read_control`] for the endianness and the
/// 64-byte-slot pitfall; only `head` and `status` are C's to write.
fn write_control(ring: &mut [u8], offset: usize, value: u32) {
    ring[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
