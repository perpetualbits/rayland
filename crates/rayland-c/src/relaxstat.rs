//! Splitting the application's synchronous poll cycle into **our latency** and **the app's own
//! back-off**, to test whether Rayland is what sets frame time.
//!
//! # The question this exists to answer
//! On 2026-08-31 four independent interventions each collapsed a mechanism by a large factor and
//! moved the frame rate by nothing measurable: S's largest lock-holder (8.3× less lock-held time),
//! C's forward message count (6.1× fewer), S's `PROGRESS_POLL` (3× more polling, p = 0.94), and C's
//! `PARK_SLEEP` (shorter is *worse*, longer is nothing). Four large wins moving nothing is a
//! signature, and the hypothesis it points at is that **the frame time is not ours to spend**.
//!
//! With fence feedback off, Mesa implements `vkWaitForFences` by polling `vkGetFenceStatus`, and the
//! interval between polls is chosen by Mesa's own `vn_relax` back-off — not by us. If the application
//! sleeps in `vn_relax` between polls, every microsecond Rayland saves is absorbed by the application
//! waiting longer before it next asks, which is exactly what four null results look like.
//!
//! # Why it records a sequence rather than a histogram
//! A histogram would answer "how long are the gaps" and lose the thing that identifies `vn_relax`:
//! its back-off **grows within a single wait**, so the signature is an *ordered* run of increasing
//! intervals, not a distribution. Two systems with identical histograms — one sleeping a constant
//! 1 ms, one doubling 62 µs → 125 → 250 → 500 → 1000 — are different findings, and only the ordered
//! record tells them apart. So this keeps the events in order and lets the analysis be done offline.
//!
//! # Why it cannot perturb what it measures
//! Four instruments in this project have been caught changing their own measurement, most recently
//! the link trace whose two `eprintln`s per message halved the frame rate. This one performs, per
//! event, **one `CLOCK_MONOTONIC` read and one store into a preallocated array** — no allocation, no
//! I/O, no lock beyond an uncontended mutex, and nothing at all when the gate is off. The array is
//! fixed-size and simply stops recording when full, so a long run degrades to a truncated record
//! rather than to unbounded memory or to a flush in the middle of the thing being timed.

use rayland_relay::stagelog::StageLog;

/// C's recorder. The mechanism — ordered events, incremental chunks, one clock read and one push per
/// sample — lives in [`rayland_relay::stagelog`], shared with S so the two sides' numbers stay
/// comparable and cannot drift apart.
static LOG: StageLog = StageLog::new("RELAXSTAT", "RAYLAND_C1_RELAXSTAT");

/// What happened. The three events bracket the application's frame from C's side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// C's ring watcher has a delta in hand. **Nothing has been diffed or sent yet.**
    ///
    /// The name is historical and the position is deliberately unchanged: it was recorded here when
    /// the 2026-08-31 measurement was taken, and moving it would silently invalidate comparison with
    /// that data. The two events below were *added* rather than repositioning this one, for the same
    /// reason.
    RingShipped,
    /// `messages_for_delta` has returned: every blob has been diffed against its baseline and the
    /// whole batch is serialized and ready to go.
    ///
    /// The interval from [`Event::RingShipped`] is where the forward path's *work* lives — including
    /// the `memcmp` of the 8 MiB Venus staging pool, which happens once per delta.
    SyncPrepared,
    /// The whole batch — every blob message and the ring delta last — is on the link to S.
    ///
    /// The interval from [`Event::SyncPrepared`] is time spent writing and flushing; the interval
    /// from here to S's own `DeltaRead` is transit plus however long S's message thread takes to work
    /// through the batch and reach the delta at the end of it.
    SyncSent,
    /// C applied bytes S wrote into the reply arena — the answer is now visible to the application.
    ///
    /// This is the moment the application *could* observe its reply. Whether it does so immediately,
    /// or after a `vn_relax` sleep, was the question this instrument was built to answer. (It does:
    /// the application's whole share of the wall clock is 9.6%, so it was not sleeping through our
    /// savings — see `docs/data/2026-08-31-vn-relax/`.)
    ReplyApplied,
    /// C delivered a `wl_callback.done` to the application — the compositor's frame callback.
    ///
    /// # Why this is here and not an afterthought
    /// The first capture put a median of **25.0 ms** between C's successive ring deltas, and the
    /// native ceiling for this app and compositor was separately measured at 25.4 ms/frame *of which
    /// 24.9 ms is compositor pacing*. Those numbers are too close to ignore: without this event, time
    /// the application spends legitimately blocked waiting for the compositor to say "draw again" is
    /// indistinguishable from time Rayland cost it, and would be silently charged to us. It turned out
    /// to be 13.7%, which is not nothing.
    FrameCallback,
}

impl Event {
    /// The `'static` label this event is recorded under.
    fn label(self) -> &'static str {
        match self {
            Event::RingShipped => "RingShipped",
            Event::SyncPrepared => "SyncPrepared",
            Event::SyncSent => "SyncSent",
            Event::ReplyApplied => "ReplyApplied",
            Event::FrameCallback => "FrameCallback",
        }
    }
}

/// Whether recording is on, from `RAYLAND_C1_RELAXSTAT`.
pub fn enabled() -> bool {
    LOG.enabled()
}

/// Record one event at the current time on the clock S also uses.
pub fn note(what: Event) {
    LOG.note(what.label());
}

/// Print the events recorded since the last call. See [`StageLog::report`] for the format.
pub fn report() {
    LOG.report();
}

/// Start the periodic reporter. Idempotent; a no-op when the gate is off.
pub fn start_reporter() {
    LOG.start_reporter();
}
