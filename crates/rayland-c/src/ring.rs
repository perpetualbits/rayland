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
//! # Known gaps: memory ordering. There are **two**, and they are not the same gap
//! This module reads and writes the ring's control words through a plain `&[u8]`/`&mut [u8]` with
//! `u32::from_le_bytes` / `to_le_bytes`. Those are ordinary loads and stores, not atomic ones. Mesa's
//! side of every one of these words *is* atomic, so both gaps below are formal data races. They are
//! recorded rather than fixed because a correct fix is a cross-process atomics question this task did
//! not scope, and a half-correct one would look solved. **Both are about the ordering of the
//! accesses, not about the logic below** — which is why the logic is testable exactly as written.
//!
//! ## Gap 1 — the `tail` load and the buffer bytes it publishes (a load-load pair)
//! Mesa stores `tail` with `memory_order_seq_cst` *after* `memcpy`ing the command bytes, and expects
//! its consumer to load `tail` with at least **Acquire**: that load is what makes the bytes written
//! before the store visible to the reader. [`RingWatcher::take_delta`]'s load is plain.
//!
//! On x86-64 — C0's only tested target — the hardware does not reorder loads with other loads (TSO),
//! so the risk here is compiler reordering only. On a **weakly-ordered target it can genuinely
//! reorder**: the reader could observe the new `tail` before the buffer bytes it publishes, and ship
//! uninitialized memory to S's GPU as Vulkan commands. CLAUDE.md names RISC-V as an explicit target
//! for machine C, so this must be closed before C runs anywhere but x86-64.
//!
//! ## Gap 2 — the IDLE/`tail` handshake (a **store-load** pair, and x86-64 does not save us)
//! This one is distinct, and the reassurance that "x86-64 is fine, only the compiler can reorder"
//! is **false** for it. The park protocol — publish IDLE, then re-read `tail`
//! ([`RingWatcher::publish_idle`] then [`RingWatcher::decide_park`]) — against Mesa's submit path —
//! store `tail`, then load `status` — is **Dekker's algorithm**. Each side stores its own flag and
//! then loads the other's, and the pattern is only correct if *both* sides order that store before
//! that load. Nothing weaker than seq_cst does that.
//!
//! Mesa holds up its half explicitly, and its comments say why (`vn_ring.c:100-111`): the tail store
//! is seq_cst because that "has required a full mfence instruction", and `vn_ring_load_status` is
//! annotated "must be called and ordered after vn_ring_store_tail for idle status".
//!
//! C's half is a plain store to `status` followed by a plain load of `tail`, and **x86-64's TSO
//! explicitly permits StoreLoad reordering** — the store sits in the store buffer while the later
//! load is satisfied. So this interleaving is legal on the one target C0 tested:
//!
//! ```text
//! C:    load tail (old)       [satisfied ahead of the still-buffered status store]
//! Mesa: store tail (new); load status -> sees IDLE clear -> sends no doorbell
//! C:    store status(IDLE) drains
//! C:    Park                  [parked, with work pending and no doorbell coming]
//! ```
//!
//! The consequence is **bounded by the watcher's timed sleep** (`PARK_SLEEP`, 500 µs, in the daemon's
//! `main.rs`), so it is added latency rather than a hang — which is precisely what that bounded
//! sleep's belt-and-braces rationale exists for, and the one place on this branch where "if the park
//! logic were ever wrong, a bounded sleep degrades to latency" has turned out to be load-bearing
//! rather than decorative. Closing it properly means a seq_cst store and a seq_cst load on C's side,
//! i.e. the same cross-process atomics work Gap 1 needs.

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

/// `VK_RING_STATUS_ALIVE_BIT_MESA` — bit 2 of the ring's `status` word.
///
/// **Set means "this ring's consumer has proved it is still there"**, and it is the bit Mesa's
/// watchdog aborts the application over when it is missing. The value is taken from Mesa's generated
/// header (`vn_protocol_renderer_defines.h:475-477`: `IDLE = 0x1`, `FATAL = 0x2`, `ALIVE = 0x4`),
/// not inferred — the same discipline `venus_ring/mod.rs` applied to the IDLE bit, and for the same
/// reason: guessing a bit here would be a silent, plausible-looking wrong answer.
///
/// See [`RingWatcher::set_alive`] for why C has to write this at all, and why Rayland's version of
/// the heartbeat is gated on evidence when virglrenderer's own is not.
const RING_STATUS_ALIVE_BIT: u32 = 0x0000_0004;

/// The ring recognizer, re-exported so this module remains the one place `rayland-c` speaks about
/// rings.
///
/// It **lives in `rayland-vtest`** ([`rayland_vtest::venus_ring::RingIdentity`]), not here, because
/// (c)1 Task 4 gave it a second caller: `rayland-s` needs the same answer — the ring's buffer size —
/// to lay a relayed delta back down into its own ring memory, and the two ends must agree exactly.
/// A disagreement would not surface as an error; it would surface as S writing the application's
/// commands at offsets Mesa never wrote them to.
pub use rayland_vtest::venus_ring::RingIdentity;

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
        self.clear_idle(ring);
        ParkDecision::StayAwake
    }

    /// Clear the IDLE bit, announcing to Mesa that this ring's consumer is awake and polling, and
    /// that a doorbell would therefore be wasted work.
    ///
    /// # Why a caller must do this on every path that stays awake, not just when parking is refused
    /// IDLE is a *claim about the consumer's state*, and Mesa acts on it on the application's hot
    /// path: `vn_ring_submit_internal` (`vn_ring.c:475-483`) tests it on **every** submit and, if it
    /// is set and the 1 ms throttle has elapsed, sends a `vkNotifyRingMESA` doorbell. Leaving it set
    /// through a busy burst therefore does not merely waste a bit — it makes Mesa pay for up to 1000
    /// doorbells a second to wake a watcher that is demonstrably already awake, each of which is a
    /// socket write on C and a relayed message to S. That inverts ring-findings §5.2's headline
    /// result, which is that the steady state emits **zero** notifications.
    ///
    /// The bit is masked out rather than the word assigned, mirroring Mesa's `atomic_fetch_and`:
    /// `status` also carries `FATAL` (0x2) and `ALIVE` (0x4), and clobbering the word destroys them.
    ///
    /// # Inputs / outputs
    /// - `ring`: the ring blob's bytes, mutable because `status` is a word C writes and Mesa reads.
    /// - Nothing is returned; the effect is the retracted bit.
    pub fn clear_idle(&mut self, ring: &mut [u8]) {
        let status = read_control(ring, RING_STATUS_OFFSET);
        // Preserve FATAL/ALIVE; only retract the IDLE claim.
        write_control(ring, RING_STATUS_OFFSET, status & !RING_STATUS_IDLE_BIT);
    }

    /// Set the ALIVE bit: the heartbeat that stops Mesa's watchdog from aborting the application.
    ///
    /// # Why C must write this word at all (ring-findings §5.4)
    /// Every Venus ring is monitored — `VkRingMonitorInfoMESA` is placed in `VkRingCreateInfoMESA`'s
    /// `pNext` **unconditionally** (`vn_ring.c:346-353`), so there is no configuration in which this
    /// does not apply. Mesa's side of the contract is:
    ///
    /// - `vn_relax_init` (`vn_common.c:234-236`) **clears** ALIVE on entry to *every* wait.
    /// - At the first warning threshold — iteration 4096, roughly 3.5 s of accumulated sleep —
    ///   `vn_relax` (`vn_common.c:268-283`) re-reads `status`. If ALIVE is still clear it calls
    ///   `vn_watchdog_acquire(watchdog, false)`, which sets `watchdog->alive = false`, so
    ///   `vn_watchdog_timeout()` returns true and Mesa calls **`abort()`** on the application.
    ///
    /// In a local vtest setup virglrenderer's `vkr-ringmon` thread re-sets the bit every ~3 s and the
    /// application never notices. Under `rayland-c` there is no such thread on C: S's virglrenderer
    /// sets ALIVE on **S's mirror** of the ring, and that store never crosses the network. So unless
    /// C writes this word itself, a legitimately slow-but-healthy S — a pipeline compile, a busy GPU,
    /// a cold shader cache, or simply a large command stream in flight over a slow link — kills the
    /// application at 3.5 s. It also makes `rayland-c`'s own 30 s stall timeout unreachable by
    /// default, since Mesa would always abort first.
    ///
    /// # Why Rayland's heartbeat is more honest than virglrenderer's, and where it still is not
    /// The caller must set this bit **only on evidence that the ring actually moved** — an
    /// `S2C::RingProgress` whose `consumed_tail` advanced. That is what ring-findings §5.4 asks for:
    /// *"a correct transport must gate the heartbeat on evidence of actual ring progress"*. Contrast
    /// `vkr_context_ring_monitor_thread` (`vkr_context.c:532-539`), which walks the context's rings
    /// and sets ALIVE on every monitored one **without consulting any ring state whatsoever** — it
    /// proves only that the host process is being scheduled, which is exactly the property the
    /// findings call worthless.
    ///
    /// **The honest limit of this gate:** it can only fire when S reports progress, so it covers a
    /// slow S that is *streaming* progress (the case Rayland most cares about — a big command stream
    /// crossing a slow link, acknowledged incrementally). It does **not** cover an S that is healthy
    /// but silent inside one long-running command, because C has no evidence to distinguish that from
    /// a wedge, and it does not receive a `RingProgress` during it. Closing that needs a liveness
    /// signal only S can emit while it executes; it is (c)1 Task 5's protocol surface and does not
    /// exist yet. `main.rs`'s stall timeout is the backstop that makes the gap safe rather than silent.
    ///
    /// The bit is OR'ed in rather than assigned, mirroring Mesa's `atomic_fetch_or`, so `FATAL` and
    /// any concurrently-published `IDLE` survive.
    ///
    /// # Inputs / outputs
    /// - `ring`: the ring blob's bytes, mutable because `status` is a word C writes and Mesa reads.
    /// - Nothing is returned; the effect is the published bit.
    ///
    /// # Pitfall: this widens the module's Gap 1/Gap 2 data race, deliberately
    /// C's read-modify-write of `status` is not atomic, while Mesa's clear of ALIVE
    /// (`atomic_fetch_and`) is. The two can therefore lose each other's update. The only direction
    /// that matters here is C re-setting an ALIVE that Mesa has just cleared, which delays a
    /// would-be abort by one warning period — the safe direction, and bounded. It is nonetheless a
    /// real race, and it is why every write to `status` is kept on the single watcher thread rather
    /// than being done from the reader thread where the `RingProgress` actually arrives.
    pub fn set_alive(&mut self, ring: &mut [u8]) {
        let status = read_control(ring, RING_STATUS_OFFSET);
        // Preserve IDLE/FATAL; only claim the ALIVE bit.
        write_control(ring, RING_STATUS_OFFSET, status | RING_STATUS_ALIVE_BIT);
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
