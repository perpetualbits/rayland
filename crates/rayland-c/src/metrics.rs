//! (c)1 Task 9's instrument: what actually crosses the link, split by channel, and how long
//! the application spends blocked waiting for S.
//!
//! # Why this module exists at all
//! (c)1's whole claim is that shipping **commands** rather than **pixels** is a viable way to
//! render remotely. That claim is either evidence or marketing, and the difference is this file.
//! The plan is explicit that measurement "is not instrumentation added at the end — it is what
//! makes (c)1 evidence instead of a demo."
//!
//! # What it measures, and why each number was chosen
//! The design spec's §8.1 makes three falsifiable predictions, and every counter here exists to
//! confirm or refute exactly one of them:
//!
//! 1. **Steady state is bandwidth-bound, not round-trip-bound**, because Venus is asynchronous by
//!    design — the application does not block on most commands. → [`Metrics::record_send`] /
//!    [`Metrics::record_recv`] split bytes by channel, so "how much" is answerable per direction.
//! 2. **Startup is round-trip-bound, but one-off.** → [`Metrics::round_trip`] counts *only* the
//!    sends that block for an answer, and sums the wall-clock actually spent blocked. If startup
//!    is RTT-bound and steady state is not, that shows up as round trips clustering before the
//!    first frame and going quiet after it.
//! 3. **The return path is ~12x the command path** (ring-findings §7 measured the reply arena at
//!    roughly twelve times the command traffic). → the C→S and S→C totals are kept separately and
//!    never summed, because their *ratio* is the prediction under test.
//!
//! # The one thing this module deliberately does not count
//! **Doorbells.** [`rayland_relay::C2S::NotifyRing`]'s own documentation forbids it, and it is
//! right to: ring-findings §5.2 measured **1 notification in one run and 4 in another for
//! byte-identical ring traffic**, because Mesa rings the doorbell only when it observes the
//! consumer's IDLE bit and a 1 ms throttle has elapsed. A doorbell count is a fact about thread
//! scheduling, not about the workload, and a table with a column that moves for no reason invites
//! a reader to explain the noise. Doorbells are therefore counted as [`Channel::Control`] traffic
//! (they are real bytes and must appear in the byte totals) but never reported as an event count.
//!
//! # Why the counters are atomics and the reporting is monotonic
//! Three threads touch the link — the vtest thread, the ring watcher, and the reader thread — so
//! the counters must be safe to update from all of them without a lock on the data path (a lock
//! here would perturb the very latency being measured). They are therefore plain relaxed atomics:
//! we need each counter to be individually correct, not to observe a consistent snapshot across
//! counters, and `Relaxed` is the cheapest ordering that guarantees the former.
//!
//! Totals are **monotonic** and, under [`start_reporter`], printed to stderr on a timer. That is
//! deliberate and inherited from the throwaway spike this module replaces: a sweep harness kills
//! the daemon when the application exits, and a `SIGKILL` that lands mid-print truncates the last
//! line. Because every printed line carries running totals rather than deltas, a parser can take
//! the **maximum** line it sees and a truncated final print costs nothing. Deltas would make the
//! final, lost print the one that mattered.
//!
//! # Cost when disabled
//! Every entry point begins with [`enabled`], a `OnceLock<bool>` read of `RAYLAND_C1_METRICS`. When
//! unset this is a predictable branch on a cached value and nothing else — no allocation, no clock
//! read, no atomic write. Measurement must not be something one hesitates to leave compiled in.

// The message types whose variants this module classifies. Metrics must never influence what
// crosses the wire, so this module only ever *reads* them.
use rayland_relay::{C2S, S2C};

// Relaxed atomics: the data path is three threads wide and must not take a lock to be measured.
use std::sync::atomic::{AtomicU64, Ordering};
// OnceLock caches the env-var decision and the process start instant; both are read on hot paths.
use std::sync::OnceLock;
// Instant is monotonic — immune to wall-clock jumps (NTP, suspend) that would corrupt a latency
// measurement taken across a real network over minutes.
use std::time::{Duration, Instant};

/// Which conversation a message belongs to.
///
/// The split is by **purpose**, not by size or direction, because the spec's predictions are about
/// purposes: "the ring is the payload", "the return path is 12x", "blob sync is the mapped-memory
/// cost". A byte total that mixed them would confirm nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// [`C2S::RingDelta`] — the command ring. **This is the payload the project is about**: the
    /// ring-findings document proved 100% of the application's Vulkan commands live here and 0%
    /// touch the vtest socket.
    Ring,

    /// [`C2S::SubmitCmd`] — Venus commands that arrived inline on the vtest socket rather than
    /// through the ring. Kept apart from [`Channel::Ring`] because although it is the same command
    /// language, S feeds it to a *different decoder* (`vkr_context.c` rather than `vkr_ring.c`), and
    /// because it is where `vkCreateRingMESA` lives. Expected to be tiny (ring-findings §2 measured
    /// 140–236 bytes for a complete Vulkan initialisation) and indispensable.
    Inline,

    /// [`C2S::BlobData`] and [`S2C::BlobData`] — blob synchronisation: the bytes that exist only
    /// because C's mapped memory and S's real GPU memory are not the same pages. **This is the
    /// number the icosa fixtures were built to provoke** (`docs/icosa-fixtures.md` §11): fixture A
    /// writes 1 MiB per frame into a persistent `HOST_COHERENT` mapping with no flush and no
    /// interceptable API call, so 120 frames should cost 120 MiB here if nothing elides it.
    BlobSync,

    /// [`S2C::Capset`], [`S2C::BlobCreated`], [`S2C::RingProgress`], [`S2C::Error`] — answers S owes
    /// C.
    ///
    /// # A caveat that must be stated wherever this number is
    /// [`S2C::BlobCreated`] carries a blob's initial contents as [`rayland_relay::BlobRun`]s, so
    /// some genuine blob-sync *content* is counted here rather than under [`Channel::BlobSync`].
    /// That makes the S→C blob-sync figure a **lower bound** and this figure an upper one. It is
    /// counted by purpose (it is a reply; C asked for it and blocked until it came) rather than by
    /// payload, and the honest thing is to say so in the write-up rather than to invent a split the
    /// wire does not have.
    Replies,

    /// Session and resource management: `Hello`, `CreateContext`, `GetCapset`, `CreateBlob`,
    /// `UnrefResource`, and `NotifyRing`. Small, but counted, because "small" is a claim and this is
    /// the file that gets to make it.
    Control,
}

impl Channel {
    /// The channel a C→S message belongs to.
    ///
    /// # Why this is exhaustive rather than a `_ =>` catch-all
    /// A new `C2S` variant must not silently land in `Control` and quietly under-report a new
    /// channel. With every variant named, adding one fails to compile here, which is precisely the
    /// moment to decide what it means for the measurement.
    pub fn of_c2s(m: &C2S) -> Channel {
        match m {
            // The whole point of the project.
            C2S::RingDelta { .. } => Channel::Ring,
            // Same language, different decoder on S; see `Channel::Inline`.
            C2S::SubmitCmd { .. } => Channel::Inline,
            // The mapped-memory cost, made visible.
            C2S::BlobData { .. } => Channel::BlobSync,
            // Doorbells are real bytes: counted here, but never reported as an event count — see
            // the module docs and `C2S::NotifyRing`'s own "do not build a metric on this".
            C2S::NotifyRing { .. } => Channel::Control,
            // Session and resource management.
            C2S::Hello { .. }
            | C2S::CreateContext { .. }
            | C2S::GetCapset { .. }
            | C2S::CreateBlob { .. }
            | C2S::UnrefResource { .. } => Channel::Control,
        }
    }

    /// The channel an S→C message belongs to.
    ///
    /// See [`Channel::Replies`] for why `BlobCreated` is a reply here even though it carries blob
    /// content — and why that caveat must travel with the number.
    pub fn of_s2c(m: &S2C) -> Channel {
        match m {
            // Unsolicited blob content: S telling C what the GPU wrote.
            S2C::BlobData { .. } => Channel::BlobSync,
            // Answers C blocked on.
            S2C::Capset { .. }
            | S2C::BlobCreated { .. }
            | S2C::RingProgress { .. }
            | S2C::Error { .. } => Channel::Replies,
        }
    }

    /// A stable, short name for tables and log lines.
    ///
    /// Stable is the operative word: these strings end up in a CSV a human diffs across a sweep, so
    /// they must not be a `Debug` rendering that a later refactor could change under the reader.
    pub fn name(self) -> &'static str {
        match self {
            Channel::Ring => "ring",
            Channel::Inline => "inline",
            Channel::BlobSync => "blob_sync",
            Channel::Replies => "replies",
            Channel::Control => "control",
        }
    }

    /// Every channel, in report order. Used to iterate without risking a forgotten arm.
    pub const ALL: [Channel; 5] = [
        Channel::Ring,
        Channel::Inline,
        Channel::BlobSync,
        Channel::Replies,
        Channel::Control,
    ];

    /// This channel's index into the counter arrays.
    fn idx(self) -> usize {
        match self {
            Channel::Ring => 0,
            Channel::Inline => 1,
            Channel::BlobSync => 2,
            Channel::Replies => 3,
            Channel::Control => 4,
        }
    }
}

/// One channel's message and byte counters for one direction.
///
/// Bytes are **framed** bytes — the postcard body plus its 4-byte length prefix — because that is
/// what the network actually carries. Counting only the body would flatter the protocol by exactly
/// 4 bytes per message, which on a chatty channel is not nothing and is not what we would be
/// claiming to have measured.
#[derive(Default)]
struct Counter {
    /// Number of framed messages.
    msgs: AtomicU64,
    /// Number of framed bytes (body + 4-byte length prefix).
    bytes: AtomicU64,
}

/// The process-wide instrument.
///
/// One instance, reached through [`metrics`]. A singleton rather than a value threaded through the
/// call graph because the three threads that touch the link do not share an owner, and threading a
/// `&Metrics` through the vtest thread, the ring watcher and the reader thread would change those
/// signatures purely to serve measurement.
pub struct Metrics {
    /// C→S counters, indexed by [`Channel::idx`].
    c2s: [Counter; 5],
    /// S→C counters, indexed by [`Channel::idx`].
    s2c: [Counter; 5],
    /// Number of sends that blocked for an answer. See [`Metrics::round_trip`].
    round_trips: AtomicU64,
    /// Total nanoseconds spent blocked in those waits. This — not the count — is what an RTT sweep
    /// moves, and it is the cost the application actually pays.
    round_trip_nanos: AtomicU64,
    /// Nanoseconds from [`Metrics::start`] to the first frame data arriving from S. Zero means "not
    /// yet"; see [`Metrics::note_first_frame`] for what counts as a frame and why.
    first_frame_nanos: AtomicU64,
    /// The instant every duration here is relative to.
    start: Instant,
}

/// The singleton.
static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Whether measurement is on, decided once from `RAYLAND_C1_METRICS`.
///
/// # Why an env var and not a flag
/// `rayland-c` is launched by a stock Mesa Venus ICD's idea of a vtest server and, in the sweep, by
/// an ssh command line assembled on another machine. An environment variable is the one control
/// surface that survives both without either of them knowing this feature exists.
///
/// Any value enables it; the variable's *presence* is the signal. `RAYLAND_C1_METRICS=0` enabling
/// metrics would be a surprise, so this is documented rather than clever: the sweep sets
/// `RAYLAND_C1_METRICS=1` and nothing sets it to anything else.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RAYLAND_C1_METRICS").is_some())
}

/// The process-wide [`Metrics`], created on first use.
pub fn metrics() -> &'static Metrics {
    METRICS.get_or_init(|| Metrics {
        c2s: Default::default(),
        s2c: Default::default(),
        round_trips: AtomicU64::new(0),
        round_trip_nanos: AtomicU64::new(0),
        first_frame_nanos: AtomicU64::new(0),
        start: Instant::now(),
    })
}

impl Metrics {
    /// Record one framed message sent C→S.
    ///
    /// - `m`: the message, read only to classify it.
    /// - `framed_bytes`: what [`rayland_relay::write_msg`] reported it wrote — body plus prefix.
    ///
    /// Cheap and infallible: two relaxed atomic adds. It is called on the data path with the link's
    /// send mutex held, so anything expensive here would show up as latency in the thing measured.
    pub fn record_send(&self, m: &C2S, framed_bytes: usize) {
        if !enabled() {
            return;
        }
        let c = &self.c2s[Channel::of_c2s(m).idx()];
        c.msgs.fetch_add(1, Ordering::Relaxed);
        c.bytes.fetch_add(framed_bytes as u64, Ordering::Relaxed);
    }

    /// Record one framed message received S→C, and note the first frame if this is it.
    ///
    /// - `m`: the message, read only to classify it.
    /// - `framed_bytes`: what [`rayland_relay::read_msg`] reported it read — body plus prefix.
    pub fn record_recv(&self, m: &S2C, framed_bytes: usize) {
        if !enabled() {
            return;
        }
        let c = &self.s2c[Channel::of_s2c(m).idx()];
        c.msgs.fetch_add(1, Ordering::Relaxed);
        c.bytes.fetch_add(framed_bytes as u64, Ordering::Relaxed);
        // Blob data flowing S→C is the GPU's output reaching the application; the first of it is
        // the closest thing on this link to "a frame happened".
        if matches!(m, S2C::BlobData { .. }) {
            self.note_first_frame();
        }
    }

    /// Record a completed round trip: a request that blocked until S answered.
    ///
    /// - `waited`: how long the calling thread was actually blocked.
    ///
    /// # What counts, and why the count alone would mislead
    /// Only requests that *block* are round trips. A `RingDelta` is fire-and-forget — it costs
    /// bandwidth, not latency — and counting it here would make Venus's asynchrony look like a
    /// stall. The number that matters is `round_trip_nanos`: the count is a property of the
    /// protocol (roughly fixed per session), while the time is a property of the network, and the
    /// sweep exists to move the network.
    pub fn round_trip(&self, waited: Duration) {
        if !enabled() {
            return;
        }
        self.round_trips.fetch_add(1, Ordering::Relaxed);
        self.round_trip_nanos
            .fetch_add(waited.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Note that the first frame's data has arrived, if it has not already been noted.
    ///
    /// # What "first frame" means here, stated precisely because a vague metric is worse than none
    /// It is the moment the **first [`S2C::BlobData`] arrives at C**: S's GPU has produced bytes and
    /// they have crossed back. It is *not* the moment the application's `vkQueueSubmit` returns, and
    /// it is *not* when a PNG hits the disk — this module cannot see either.
    ///
    /// The measurement is from [`Metrics::start`], which is when the first counter was touched
    /// (effectively daemon startup), so it includes the QUIC handshake, the capset round trip, and
    /// blob creation. That is the intent: spec §8.1 predicts startup is RTT-bound but one-off, and
    /// a number that excluded the handshake could not test it.
    ///
    /// Idempotent: only the first call stores anything, via a compare-and-exchange, so the reader
    /// thread racing itself cannot overwrite an earlier frame with a later one.
    pub fn note_first_frame(&self) {
        if !enabled() {
            return;
        }
        let elapsed = self.start.elapsed().as_nanos() as u64;
        // Zero is the sentinel for "not yet". A frame at exactly 0 ns is not physically possible
        // here — a QUIC handshake has already happened — so the sentinel is safe.
        let _ = self.first_frame_nanos.compare_exchange(
            0,
            elapsed,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// One line carrying every running total, in `key=value` form.
    ///
    /// # Why one line and why these names
    /// The sweep runs `rayland-c` on apollo over ssh and parses its stderr; a single greppable line
    /// per sample survives interleaving with Mesa's own chatter, whereas a multi-line block does
    /// not. The `C1METRICS` prefix is what the harness greps for. Names are stable identifiers, not
    /// prose, because they become CSV column headers.
    ///
    /// Every value is a **running total**, never a delta — see the module docs on why a truncated
    /// final print must be harmless.
    pub fn line(&self) -> String {
        // Start with the durations, which are what a latency sweep is actually about.
        let mut s = format!(
            "C1METRICS elapsed_us={} first_frame_us={} round_trips={} round_trip_wait_us={}",
            self.start.elapsed().as_micros(),
            self.first_frame_nanos.load(Ordering::Relaxed) / 1_000,
            self.round_trips.load(Ordering::Relaxed),
            self.round_trip_nanos.load(Ordering::Relaxed) / 1_000,
        );
        // Then per-channel bytes and message counts, both directions, in a fixed order.
        for ch in Channel::ALL {
            let c = &self.c2s[ch.idx()];
            let r = &self.s2c[ch.idx()];
            s.push_str(&format!(
                " c2s_{n}_msgs={cm} c2s_{n}_bytes={cb} s2c_{n}_msgs={rm} s2c_{n}_bytes={rb}",
                n = ch.name(),
                cm = c.msgs.load(Ordering::Relaxed),
                cb = c.bytes.load(Ordering::Relaxed),
                rm = r.msgs.load(Ordering::Relaxed),
                rb = r.bytes.load(Ordering::Relaxed),
            ));
        }
        // Finally the direction totals. Derivable from the per-channel figures, but printed anyway:
        // the 12x return-path prediction is read off exactly these two numbers, and making a
        // reader sum five columns by hand to test a headline prediction invites arithmetic errors
        // in the one place they would be most embarrassing.
        let c2s_total: u64 = Channel::ALL
            .iter()
            .map(|ch| self.c2s[ch.idx()].bytes.load(Ordering::Relaxed))
            .sum();
        let s2c_total: u64 = Channel::ALL
            .iter()
            .map(|ch| self.s2c[ch.idx()].bytes.load(Ordering::Relaxed))
            .sum();
        s.push_str(&format!(
            " c2s_total_bytes={c2s_total} s2c_total_bytes={s2c_total}"
        ));
        s
    }
}

/// Start the reporter thread: print [`Metrics::line`] to stderr every 100 ms.
///
/// Does nothing unless [`enabled`]. Idempotent — a second call is ignored — so a caller cannot
/// accidentally spawn two reporters printing interleaved totals.
///
/// # Why a thread and not a print at exit only
/// The sweep kills `rayland-c` once the application has exited; the daemon has no orderly shutdown
/// to hook in every case, and at 100 ms RTT a run can also be aborted by a timeout. A periodic
/// monotonic print means the harness always has a last-good sample, and [`report_final`] merely
/// improves on it when a clean exit does happen.
///
/// 100 ms is chosen to be far below any sweep cell's duration (seconds to minutes) and far above
/// the cost of formatting one line, so the reporter cannot perturb the measurement.
pub fn start_reporter() {
    if !enabled() {
        return;
    }
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_millis(100));
            eprintln!("{}", metrics().line());
        }
    });
}

/// Print the final totals, prefixed so a parser can prefer them over the periodic samples.
///
/// Does nothing unless [`enabled`]. Called on the daemon's orderly shutdown path; if the daemon is
/// killed instead, the reporter's last periodic line stands in and the parser's max-line rule makes
/// that lossless.
pub fn report_final() {
    if !enabled() {
        return;
    }
    eprintln!("{} final=1", metrics().line());
}
