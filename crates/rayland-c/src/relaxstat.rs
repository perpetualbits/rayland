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

use std::sync::Mutex;

/// What happened. The two events bracket the application's poll cycle from C's side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// C shipped a ring delta to S — the application's request is on its way.
    RingShipped,
    /// C applied bytes S wrote into the reply arena — the answer is now visible to the application.
    ///
    /// This is the moment the application *could* observe its reply. Whether it does so immediately,
    /// or after a `vn_relax` sleep, is precisely the question.
    ReplyApplied,
    /// C delivered a `wl_callback.done` to the application — the compositor's frame callback.
    ///
    /// # Why this is here and not an afterthought
    /// The first capture put a median of **25.0 ms** between C's successive ring deltas, and the
    /// native ceiling for this app and compositor was separately measured at 25.4 ms/frame *of which
    /// 24.9 ms is compositor pacing*. Those numbers are too close to ignore: without this event, time
    /// the application spends legitimately blocked waiting for the compositor to say "draw again" is
    /// indistinguishable from time Rayland cost it, and would be silently charged to us. A
    /// decomposition that cannot separate our latency from the compositor's frame rate cannot answer
    /// the question it was built for.
    FrameCallback,
}

/// How many events are kept. A 60-frame `vkcube` run produces roughly 1,200 ring deltas and 2,000
/// reply applications, so 200,000 is ample for any run this harness makes while staying a fixed
/// ~2.4 MB that is allocated once, before the first sample.
const CAPACITY: usize = 200_000;

/// The recorded events, in order.
static LOG: Mutex<Vec<(u64, Event)>> = Mutex::new(Vec::new());

/// Whether recording is on, decided once from `RAYLAND_C1_RELAXSTAT`.
///
/// The variable's *presence* is the signal, matching every other diagnostic gate in this project.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("RAYLAND_C1_RELAXSTAT").is_some();
        if on {
            // Reserve up front so no sample ever pays for a reallocation — a `Vec` growing from 1 MB
            // to 2 MB mid-run would copy megabytes inside the interval it is trying to time.
            LOG.lock()
                .expect("the relaxstat lock is never poisoned")
                .reserve_exact(CAPACITY);
        }
        on
    })
}

/// Record one event at the current time on the shared monotonic clock.
///
/// # Inputs / outputs
/// - `what`: which end of the poll cycle this is.
/// - Returns nothing. Does nothing when the gate is off, and silently stops recording once
///   [`CAPACITY`] events have been kept — a truncated record is a correct partial answer, whereas
///   growing the buffer mid-run would corrupt the timings around the growth.
///
/// # Failure modes
/// None observable. The clock read cannot fail and the lock is only ever held for a push.
pub fn note(what: Event) {
    if !enabled() {
        return;
    }
    // Read the clock *before* taking the lock, so lock acquisition is not counted into the interval
    // being measured. The events are pushed in whatever order the threads reach the lock; the
    // analysis sorts by timestamp, which is the ordering that actually means something.
    let now = rayland_relay::trace::monotonic_ns();
    let mut log = LOG.lock().expect("the relaxstat lock is never poisoned");
    if log.len() < CAPACITY {
        log.push((now, what));
    }
}

/// How many events have already been printed, so each report emits only what is new.
static REPORTED: Mutex<usize> = Mutex::new(0);

/// Print the events recorded since the last call, one per line, under a greppable prefix.
///
/// # Why it emits incrementally, and never one line per event as it happens
/// Emitting a line *per event, as it happens* is exactly the mistake that halved the frame rate when
/// the link trace did it. Emitting the *whole* log on every tick — which this did at first — is the
/// opposite mistake: it is O(n²) in I/O and it silently loses short runs, because a run that ends
/// before the first tick prints nothing at all. Two of three captures were lost that way before this
/// was fixed. So: batched, incremental, and frequent enough that a fast run still leaves a record.
///
/// Format is `RELAXSTAT t_ns=<n> <kind>` per event, then `RELAXSTAT-CHUNK n=<count>` so a reader can
/// tell a complete chunk from a capture cut off mid-write. **Chunks concatenate** — the full record is
/// every `RELAXSTAT` line in the file, in order, not the last chunk alone.
///
/// Events from different threads are sorted by timestamp within a chunk; because a chunk is only
/// emitted after the events in it are well past, cross-chunk ordering follows from the timestamps.
pub fn report() {
    if !enabled() {
        return;
    }
    let mut log = LOG.lock().expect("the relaxstat lock is never poisoned");
    // The ring watcher and the link reader push from different threads, so push order is not time
    // order, and every interval derived downstream depends on time order being right.
    log.sort_by_key(|(t, _)| *t);
    let mut reported = REPORTED.lock().expect("the relaxstat lock is never poisoned");
    if *reported >= log.len() {
        return;
    }
    let fresh = &log[*reported..];
    let mut out = String::with_capacity(fresh.len() * 40);
    for (t, what) in fresh {
        out.push_str(&format!("RELAXSTAT t_ns={t} {what:?}\n"));
    }
    out.push_str(&format!("RELAXSTAT-CHUNK n={}\n", fresh.len()));
    eprint!("{out}");
    *reported = log.len();
}

/// Start a thread that reports periodically, so a run killed by the harness still leaves its record.
///
/// Idempotent; a no-op when the gate is off. Every report is a full dump of everything recorded so
/// far, so the *last* banner in a log is the complete one and earlier ones are prefixes of it.
pub fn start_reporter() {
    if !enabled() {
        return;
    }
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    // Three seconds, not ten: the harness stops a fast run in well under ten, and two of three early
    // captures were lost entirely because the first tick never came. Incremental chunks make a short
    // interval cheap.
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3));
            report();
        }
    });
}
