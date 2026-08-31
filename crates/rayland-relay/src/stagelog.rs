//! An ordered, timestamped event recorder cheap enough to leave in a latency-critical path.
//!
//! # Why this exists in the shared crate
//! Both daemons need the same thing and for the same reason: to say **where a synchronous round trip
//! spends its time**, which requires the *order* of events, not a distribution of them. C got one
//! first ([`rayland_c::relaxstat`]); S needs the mirror of it to localise the ~16 ms that the C-side
//! measurement of 2026-08-31 charged to "what S does between reading a delta and the first reply".
//! Two copies of a measurement instrument drift exactly as two copies of a safety argument do, and
//! the two sides' numbers are only comparable while the recording discipline is identical — so there
//! is one implementation, here, in the crate both already depend on. It brings no dependency with it:
//! this crate must stay free of GPU, sockets and async, and a `Vec` behind a `Mutex` is none of those.
//!
//! # Why a sequence and not a histogram
//! A histogram answers "how long are the gaps" and destroys the thing that identifies a back-off: it
//! grows *within* one wait, so the signature is an ordered run of increasing intervals. Two systems
//! with identical histograms — one sleeping a constant 1 ms, one doubling 62 → 125 → 250 → 500 µs —
//! are different findings, and only the ordered record separates them. Analysis is done offline.
//!
//! # Why it cannot perturb what it measures
//! Five instruments in this project have been caught changing their own measurement, most recently a
//! link trace whose two `eprintln`s per message halved the frame rate. Per event this performs **one
//! `CLOCK_MONOTONIC` read and one push into a pre-reserved `Vec`** — no allocation, no I/O, no lock
//! beyond an uncontended mutex, and nothing at all when the gate is off.
//!
//! # Two failure modes this shape exists to avoid, both met in practice
//! - **A run that ends before the first report leaves nothing.** C's first version reported every 10 s
//!   and lost two of three captures outright, because the harness stopped those runs sooner. Reports
//!   are therefore **incremental** and frequent: each one emits only what is new.
//! - **Re-dumping the whole log every tick is O(n²)** in I/O and grows without bound. Incremental
//!   chunks fix that too. **Chunks concatenate**: the full record is every line in the file, in order,
//!   not the last chunk alone.

use std::sync::Mutex;

/// One recorder. Construct as a `static`; every method is a no-op unless its gate variable is set.
pub struct StageLog {
    /// Line prefix, so two recorders in one log file can be told apart by `grep`.
    prefix: &'static str,
    /// The environment variable whose *presence* turns this recorder on.
    gate: &'static str,
    /// `(timestamp_ns, stage)` in push order; sorted by time before each report.
    events: Mutex<Vec<(u64, &'static str)>>,
    /// How many events have already been printed, so a report emits only what is new.
    reported: Mutex<usize>,
    /// Resolved gate, decided once on first use.
    on: std::sync::OnceLock<bool>,
}

/// How many events are kept. A 60-frame `vkcube` run produces a few thousand on either side, so this
/// is ample while staying a fixed ~2.4 MB reserved once, before the first sample.
const CAPACITY: usize = 200_000;

impl StageLog {
    /// A recorder writing lines prefixed `prefix`, enabled by the presence of `gate` in the
    /// environment.
    pub const fn new(prefix: &'static str, gate: &'static str) -> StageLog {
        StageLog {
            prefix,
            gate,
            events: Mutex::new(Vec::new()),
            reported: Mutex::new(0),
            on: std::sync::OnceLock::new(),
        }
    }

    /// Whether this recorder is on. The variable's *presence* is the signal, matching every other
    /// diagnostic gate in this project; a value of `0` enabling it would be a surprise.
    ///
    /// Reserves the buffer on the first affirmative answer, so no sample ever pays for a reallocation
    /// — a `Vec` growing from 1 MB to 2 MB mid-run would copy megabytes inside the interval it is
    /// trying to time.
    pub fn enabled(&self) -> bool {
        *self.on.get_or_init(|| {
            let on = std::env::var_os(self.gate).is_some();
            if on {
                self.events
                    .lock()
                    .expect("a stagelog mutex is never poisoned")
                    .reserve_exact(CAPACITY);
            }
            on
        })
    }

    /// Record that `stage` happened, now.
    ///
    /// # Inputs / outputs
    /// - `stage`: a short `'static` name. `'static` deliberately: it makes an allocating or
    ///   `format!`-built label impossible at the call site, which is where the cost would land.
    /// - Silently stops recording at [`CAPACITY`]. A truncated record is a correct partial answer;
    ///   growing the buffer mid-run would corrupt the timings around the growth.
    pub fn note(&self, stage: &'static str) {
        if !self.enabled() {
            return;
        }
        // Read the clock *before* taking the lock, so lock acquisition is not counted into the
        // interval being measured.
        let now = crate::trace::monotonic_ns();
        let mut ev = self
            .events
            .lock()
            .expect("a stagelog mutex is never poisoned");
        if ev.len() < CAPACITY {
            ev.push((now, stage));
        }
    }

    /// Print everything recorded since the last call, to stderr.
    ///
    /// Format is `<prefix> t_ns=<n> <stage>` per event, then `<prefix>-CHUNK n=<count>` so a reader
    /// can tell a complete chunk from a capture cut off mid-write.
    pub fn report(&self) {
        if !self.enabled() {
            return;
        }
        let mut ev = self
            .events
            .lock()
            .expect("a stagelog mutex is never poisoned");
        // Sorted by time: events are recorded from several threads, so push order is not time order,
        // and every interval derived downstream depends on time order being right. Because a chunk is
        // only emitted once its events are well past, cross-chunk ordering follows from timestamps.
        ev.sort_by_key(|(t, _)| *t);
        let mut reported = self
            .reported
            .lock()
            .expect("a stagelog mutex is never poisoned");
        if *reported >= ev.len() {
            return;
        }
        let fresh = &ev[*reported..];
        let mut out = String::with_capacity(fresh.len() * 40);
        for (t, stage) in fresh {
            out.push_str(&format!("{} t_ns={t} {stage}\n", self.prefix));
        }
        out.push_str(&format!("{}-CHUNK n={}\n", self.prefix, fresh.len()));
        eprint!("{out}");
        *reported = ev.len();
    }

    /// Start a thread reporting every three seconds, so a run the harness kills still leaves a record.
    ///
    /// Three seconds, not ten: the harness stops a fast run in well under ten, and C's first version
    /// lost two of three captures because the first tick never came. Incremental chunks make a short
    /// interval cheap. Idempotent; a no-op when the gate is off.
    pub fn start_reporter(&'static self) {
        if !self.enabled() {
            return;
        }
        if self.reporter_started().set(()).is_err() {
            return;
        }
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3));
                self.report();
            }
        });
    }

    /// One-shot latch for [`Self::start_reporter`], kept out of the struct so `new` can stay `const`.
    fn reporter_started(&'static self) -> &'static std::sync::OnceLock<()> {
        // Keyed by the recorder's gate string, which is unique per recorder by construction. Two
        // recorders sharing a gate would share a latch, and that is a programming error this
        // deliberately does not paper over — it would mean two recorders that cannot be told apart in
        // the environment either.
        static LATCHES: Mutex<Vec<(&'static str, &'static std::sync::OnceLock<()>)>> =
            Mutex::new(Vec::new());
        let mut l = LATCHES.lock().expect("a stagelog mutex is never poisoned");
        if let Some((_, latch)) = l.iter().find(|(g, _)| *g == self.gate) {
            return latch;
        }
        // Leaked deliberately and exactly once per recorder: a `StageLog` is a `static`, so its latch
        // must outlive every caller, and there is no owner to hang it on without giving up `const fn`.
        let latch: &'static std::sync::OnceLock<()> =
            Box::leak(Box::new(std::sync::OnceLock::new()));
        l.push((self.gate, latch));
        latch
    }
}

#[cfg(test)]
mod tests {
    use super::StageLog;

    /// A recorder whose gate is unset records nothing and costs nothing, which is what makes it safe
    /// to leave on a latency-critical path in a shipping build.
    #[test]
    fn a_recorder_with_its_gate_unset_records_nothing() {
        static LOG: StageLog = StageLog::new("TESTLOG", "RAYLAND_TEST_GATE_DEFINITELY_UNSET");
        assert!(!LOG.enabled());
        LOG.note("a");
        LOG.note("b");
        assert_eq!(
            LOG.events.lock().expect("the test recorder's mutex").len(),
            0,
            "an unset gate must not record"
        );
    }

    /// With the gate set, events are kept in time order and each is kept exactly once.
    #[test]
    fn an_enabled_recorder_keeps_events_in_time_order() {
        // SAFETY: single-threaded test, and the variable is unique to this test.
        unsafe { std::env::set_var("RAYLAND_TEST_GATE_ORDER", "1") };
        static LOG: StageLog = StageLog::new("TESTLOG", "RAYLAND_TEST_GATE_ORDER");
        assert!(LOG.enabled());
        LOG.note("first");
        LOG.note("second");
        LOG.note("third");
        let ev = LOG.events.lock().expect("the test recorder's mutex");
        assert_eq!(ev.len(), 3);
        assert_eq!(
            ev.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );
        // Monotonic clock: timestamps must not go backwards, or every derived interval is suspect.
        assert!(ev[0].0 <= ev[1].0 && ev[1].0 <= ev[2].0);
    }

    /// A report emits each event once: the second call after no new events emits nothing. This is the
    /// property that makes a 3-second reporter affordable, and its absence is what made the first
    /// version O(n²).
    #[test]
    fn reports_are_incremental() {
        unsafe { std::env::set_var("RAYLAND_TEST_GATE_INCR", "1") };
        static LOG: StageLog = StageLog::new("TESTLOG", "RAYLAND_TEST_GATE_INCR");
        LOG.note("one");
        LOG.report();
        assert_eq!(
            *LOG.reported.lock().expect("the test recorder's mutex"),
            1,
            "the first report must consume the one event"
        );
        LOG.report();
        assert_eq!(
            *LOG.reported.lock().expect("the test recorder's mutex"),
            1,
            "a second report with nothing new must emit nothing"
        );
        LOG.note("two");
        LOG.report();
        assert_eq!(
            *LOG.reported.lock().expect("the test recorder's mutex"),
            2,
            "a later report must consume only the new event"
        );
    }
}
