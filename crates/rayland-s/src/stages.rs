//! S's stage recorder: where the ~16 ms per ring round trip actually goes.
//!
//! # What this is looking for
//! The C-side measurement of 2026-08-31 (`docs/data/2026-08-31-vn-relax/`) attributed **76.7% of the
//! wall clock to Rayland**, found 91% of that in intervals longer than 5 ms, and found **90.5% of
//! *those* in intervals that begin with a ring delta going out** — about **3.1 round trips per frame
//! at ~16 ms each, on loopback**, where the network costs microseconds. Every previously suspected
//! term is excluded by measurement: the network, S's lock contention, C's send path, both poll
//! intervals, Mesa's client-side back-off, and virglrenderer's host-side back-off.
//!
//! That leaves exactly one unmeasured span, and it is entirely on S: **from S reading a ring delta to
//! the first reply reaching C.** This records its stages.
//!
//! # The stages, and what sits between them
//! ```text
//!   DeltaRead      S's message thread has the C2S::RingDelta off the link
//!     |              <- S's own message handling: the applier lock, then a memcpy
//!   DeltaApplied   the bytes are in the ring blob's memory and `tail` is published
//!     |              <- VIRGLRENDERER'S RING THREAD notices and executes. Nothing here is ours,
//!     |                 and nothing here has ever been timed. This is the prime suspect.
//!   RingProgress   `head` moved: virglrenderer consumed the commands
//!     |              <- the reply arena is written during execution
//!   VenusReply     changed reply-arena bytes are in hand
//!     |
//!   ReplyShipped   the reply is on the link to C
//! ```
//! `FenceSignaled` and `ReadbackShipped` are recorded alongside, because a submit's completion
//! barrier and its readback are the two things that make one round trip cost far more than another,
//! and a decomposition that could not separate "a delta carrying a draw" from "a delta carrying a
//! status poll" would average them into a meaningless middle.
//!
//! # Why it is safe on this path
//! The mechanism is [`rayland_relay::stagelog`], shared with C so the two sides' numbers stay
//! comparable: one `CLOCK_MONOTONIC` read and one push per event, nothing at all when the gate is
//! off. That matters more here than on C — these call sites sit inside the applier lock's critical
//! section and on the 200 µs progress poll, which is precisely where five earlier instruments in this
//! project became participants in what they were measuring.
//!
//! On loopback S and C share `CLOCK_MONOTONIC`, so this record joins C's `RELAXSTAT` directly and the
//! two together cover the whole round trip with no gap.

use rayland_relay::stagelog::StageLog;

/// S's recorder, gated on `RAYLAND_S_STAGES`.
static LOG: StageLog = StageLog::new("SSTAGE", "RAYLAND_S_STAGES");

/// A point in S's handling of one relayed ring delta. See the module docs for what lies between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// S's message thread has read a `C2S::RingDelta` off the link.
    DeltaRead,
    /// `Applier::apply` returned, **with the applier lock still held**.
    ///
    /// # Why this sits between the other two
    /// `DeltaRead -> DeltaApplied` was measured at **5.18 ms** on the riscv64 board — 71% of S's whole
    /// span — for what the message loop's own docs describe as "a `memcpy` and one atomic store". That
    /// span actually contains four things: waiting for the applier lock, `apply` itself, the frame
    /// capture, and the replies being sent *while the lock is held*. This marker separates the first
    /// two from the last two, which is the difference between "the delta is expensive to apply" and
    /// "something after it is expensive and is holding the lock".
    ApplyReturned,
    /// `Applier::apply` returned and the applier lock was released: the delta's bytes are in the ring
    /// blob's memory and `tail` is published, so virglrenderer's ring thread may now see them.
    DeltaApplied,
    /// `take_ring_progress` returned something: virglrenderer's ring thread has advanced `head`, so it
    /// has consumed commands. **The gap from `DeltaApplied` to here is virglrenderer's, not ours.**
    RingProgress,
    /// `take_venus_blob_writes` returned changed reply-arena bytes.
    VenusReply,
    /// The reply-arena scan found a `vkGetFenceStatus` reply reading `VK_SUCCESS` — the application's
    /// submit *and its readback copy* are complete on S's GPU.
    FenceSignaled,
    /// The application's readback blob was shipped: this round trip carried a finished frame.
    ReadbackShipped,
    /// The reply arena and the head-advance are on the link to C, which is what releases the app.
    ReplyShipped,
}

impl Stage {
    /// The `'static` label this stage is recorded under.
    fn label(self) -> &'static str {
        match self {
            Stage::DeltaRead => "DeltaRead",
            Stage::ApplyReturned => "ApplyReturned",
            Stage::DeltaApplied => "DeltaApplied",
            Stage::RingProgress => "RingProgress",
            Stage::VenusReply => "VenusReply",
            Stage::FenceSignaled => "FenceSignaled",
            Stage::ReadbackShipped => "ReadbackShipped",
            Stage::ReplyShipped => "ReplyShipped",
        }
    }
}

/// Record that `stage` happened, now. Inert unless `RAYLAND_S_STAGES` is set.
pub fn note(stage: Stage) {
    LOG.note(stage.label());
}

/// Print the stages recorded since the last call.
pub fn report() {
    LOG.report();
}

/// Start the periodic reporter. Idempotent; a no-op when the gate is off.
pub fn start_reporter() {
    LOG.start_reporter();
}
