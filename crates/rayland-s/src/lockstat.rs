//! Where S's message thread actually spends its time: waiting for the applier lock.
//!
//! # Why this module exists
//! WP0's frame time is set by a synchronous round trip — the application writes a command into the
//! ring, C ships it, S executes it, and the application is released only when the reply comes back.
//! A cross-daemon trace on 2026-08-31 put a **median 3.9 ms** between C flushing a doorbell and S
//! *reading* it, on loopback, where the wire costs tens of microseconds. The queue was almost always
//! empty (median depth 2), so the delay was not congestion: S's message loop simply was not coming
//! around. That loop blocks on `read_msg` with no sleep in it, which leaves exactly one candidate —
//! it is blocked on the applier mutex, held by the 200 µs progress poll.
//!
//! # Why a histogram and not a log line
//! Three instruments in this project have been caught perturbing the thing they measured, the most
//! recent being the link trace whose two `eprintln`s cost more than the send they bracketed. A
//! `clock_gettime` pair and one relaxed atomic increment per acquisition cannot do that, and a
//! log2-bucketed histogram keeps the *shape* — which is the whole question here, because a median of
//! 224 µs with a 26 ms p99 and a flat 224 µs are different bugs with different fixes.
//!
//! Everything here is inert unless `RAYLAND_S_LOCKSTAT` is set.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Number of log2 buckets. Bucket `i` counts durations in `[2^i, 2^(i+1))` nanoseconds, so 32
/// buckets reach ~4 s — past any lock wait that is not a deadlock — and anything longer saturates
/// into the last bucket rather than indexing out of bounds.
const BUCKETS: usize = 32;

/// One named duration distribution: a total, a count, and the log2 histogram.
pub struct Stat {
    /// What is being timed, printed verbatim in the report.
    name: &'static str,
    /// Number of samples folded in.
    count: AtomicU64,
    /// Sum of all samples, in nanoseconds. Kept alongside the histogram because "how much of the
    /// wall clock did this consume in total" is a different question from "how is it distributed",
    /// and both are asked of this data.
    nanos: AtomicU64,
    /// The distribution.
    hist: [AtomicU64; BUCKETS],
}

impl Stat {
    /// A zeroed distribution under `name`. `const` so the table below can be a `static`.
    const fn new(name: &'static str) -> Stat {
        // `AtomicU64::new(0)` is const, but an array of them needs a const-evaluable repeat, which
        // requires the element type to be `Copy` — it is not. The explicit loop-free form below is
        // the standard workaround: build the array from a const expression per element.
        #[allow(clippy::declare_interior_mutable_const)]
        const Z: AtomicU64 = AtomicU64::new(0);
        Stat {
            name,
            count: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
            hist: [Z; BUCKETS],
        }
    }

    /// Fold one sample in.
    fn record(&self, d: Duration) {
        // Saturating: a `Duration` past `u64::MAX` nanoseconds cannot arise from a mutex wait, but
        // saturating removes the question at no cost.
        let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.nanos.fetch_add(ns, Ordering::Relaxed);
        // `floor(log2(ns))`. Zero has no logarithm — a sample below the clock's resolution — and is
        // bucket 0 by definition.
        let b = if ns == 0 {
            0
        } else {
            (63 - ns.leading_zeros()) as usize
        };
        self.hist[b.min(BUCKETS - 1)].fetch_add(1, Ordering::Relaxed);
    }

    /// One report line, or `None` if nothing was ever recorded.
    ///
    /// Empty buckets are omitted: a line carrying twenty-eight zeroes is a line nobody reads.
    fn line(&self) -> Option<String> {
        let n = self.count.load(Ordering::Relaxed);
        if n == 0 {
            return None;
        }
        let total_us = self.nanos.load(Ordering::Relaxed) / 1_000;
        let mut s = format!(
            "  {:<28} n={n:<7} total_ms={:<9.1} hist=",
            self.name,
            total_us as f64 / 1000.0
        );
        for (i, b) in self.hist.iter().enumerate() {
            let c = b.load(Ordering::Relaxed);
            if c > 0 {
                // The bucket's lower bound in microseconds; sub-microsecond buckets print as `0`.
                s.push_str(&format!("{}us:{c},", (1u64 << i) / 1_000));
            }
        }
        Some(s)
    }
}

/// Every distribution S measures. A fixed table rather than a map, so recording is one array index
/// with no allocation and no lock of its own — an instrument that took a lock to measure a lock
/// would be measuring itself.
pub struct Table {
    /// How long the **message thread** waited to acquire the applier lock. This is the one that
    /// shows up as round-trip latency: every millisecond here is a millisecond the application's
    /// doorbell sat unread.
    pub msg_lock_wait: Stat,
    /// How long the message thread then **held** it, across `apply` and the replies it produced.
    pub msg_lock_held: Stat,
    /// How long the **progress thread** waited for the same lock.
    pub prog_lock_wait: Stat,
    /// How long `take_ring_progress` held it.
    pub prog_ring: Stat,
    /// How long `take_venus_blob_writes` plus `reply_arena_fence_signaled` held it — the pair that
    /// rescans the reply arena, and the standing suspect for the message thread's wait.
    pub prog_venus: Stat,
    /// `take_venus_blob_writes` alone.
    ///
    /// # Why the pair is also split
    /// The two calls have different fixes. `take_venus_blob_writes` diffs every Venus-internal blob
    /// and must ship what changed; `reply_arena_fence_signaled` only looks for a two-byte pattern and
    /// could in principle look at far less. Attributing the pair's 2–4 ms to the wrong half would
    /// send the fix to the wrong place, which is exactly the kind of error this project has made
    /// before by measuring one level too coarse.
    pub prog_venus_diff: Stat,
    /// `reply_arena_fence_signaled` alone.
    pub prog_venus_fence: Stat,
}

/// The process-wide table.
static TABLE: Table = Table {
    msg_lock_wait: Stat::new("message thread lock WAIT"),
    msg_lock_held: Stat::new("message thread lock HELD"),
    prog_lock_wait: Stat::new("progress thread lock WAIT"),
    prog_ring: Stat::new("  take_ring_progress HELD"),
    prog_venus: Stat::new("  venus+fence scan HELD"),
    prog_venus_diff: Stat::new("    take_venus_blob_writes"),
    prog_venus_fence: Stat::new("    reply_arena_fence_signaled"),
};

/// Whether lock statistics are on, decided once from `RAYLAND_S_LOCKSTAT`.
///
/// The variable's *presence* is the signal, matching every other diagnostic gate in this project;
/// `RAYLAND_S_LOCKSTAT=0` enabling it would be a surprise, so it is documented rather than clever.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RAYLAND_S_LOCKSTAT").is_some())
}

/// Reach the table. Callers should guard with [`enabled`] before timing, so an unmeasured run does
/// not pay even the clock reads.
pub fn table() -> &'static Table {
    &TABLE
}

/// Record one sample into `stat`, doing nothing when measurement is off.
pub fn record(stat: &Stat, d: Duration) {
    if enabled() {
        stat.record(d);
    }
}

/// Print the whole table to stderr, under a banner that is easy to grep for.
///
/// # When this is called
/// From a reporter thread on a fixed interval, so a run that is killed rather than exiting cleanly —
/// which is how every soak run ends — still leaves its numbers behind. A report printed only at exit
/// would be a report never printed.
pub fn report() {
    if !enabled() {
        return;
    }
    let t = table();
    let mut out = String::from("S1LOCKSTAT\n");
    for s in [
        &t.msg_lock_wait,
        &t.msg_lock_held,
        &t.prog_lock_wait,
        &t.prog_ring,
        &t.prog_venus,
        &t.prog_venus_diff,
        &t.prog_venus_fence,
    ] {
        if let Some(l) = s.line() {
            out.push_str(&l);
            out.push('\n');
        }
    }
    eprint!("{out}");
}

/// Start the periodic reporter. Idempotent; a no-op when measurement is off.
pub fn start_reporter() {
    if !enabled() {
        return;
    }
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    // Five seconds: often enough that a killed run has a recent report, rare enough that the report
    // itself is not traffic. The thread is detached and never joined — it has no state to clean up
    // and the process exiting is its termination condition.
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(5));
            report();
        }
    });
}
