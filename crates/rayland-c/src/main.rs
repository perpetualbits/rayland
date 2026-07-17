//! The **`rayland-c` daemon**: be a vtest host for the local application, and relay what it writes
//! into shared memory to S.
//!
//! # What runs here, and why it takes three threads
//! The shape is forced by the domain, not chosen for elegance. Ring-findings §5.2 established that
//! **in the steady state Venus emits zero notifications**: an actively rendering application's
//! entire action is a `memcpy` into shared memory and a store to a `uint32_t`. There is no event to
//! wait on, so someone must poll — and polling cannot be done by a thread that is also blocked
//! reading a socket. Hence:
//!
//! - **The vtest thread** (this one, after setup) runs [`serve_vtest`], which blocks reading Mesa's
//!   socket. It handles ring *management*: the handshake, blob allocation, and the inline command
//!   path. Ring-findings §2 measured that path at 140–236 bytes for an entire Vulkan
//!   initialization, none of it application drawing.
//! - **The ring watcher thread** polls the ring's `tail` and relays the bytes. This carries **100%
//!   of the application's Vulkan commands**. It is the reason the project works.
//! - **The reader thread** owns `recv` on the link to S, and is the only thing that does. It routes
//!   S's replies to whoever is waiting, writes [`S2C::BlobData`] into the pages Mesa mapped, and
//!   records ring progress.
//!
//! # Why the reader thread must exist (and why a simpler design deadlocks)
//! The tempting design is to let the vtest thread do its own `recv` and skip the reader entirely.
//! That deadlocks. While the vtest thread is blocked in `read_command` waiting for Mesa, nobody
//! would be draining the link — so S's replies would sit unread in the socket. But Mesa is at that
//! moment spinning on the ring's `head` waiting for exactly those replies, and `head` cannot advance
//! until they are read. Both sides wait for the other. A dedicated reader is what breaks it.
//!
//! # Status: this runs, and what running it cost
//! **As of (c)1 Task 6 this binary works end to end.** `rayland-refapp` — unmodified, unaware —
//! renders through it across a QUIC link to `rayland-s`, and the PNG it writes is bit-identical to
//! the same binary run natively (`rayland-s/tests/loopback_e2e.rs`, 10/10 runs).
//!
//! It did not work when it was first run, and every one of the four faults was invisible to the
//! mock-based tests below, because each was a fact about the *peer* rather than about this code.
//! They are recorded here because they are the sub-project's actual findings, and each is documented
//! in full where it was fixed:
//!
//! 1. **The doorbell does not survive the split.** virglrenderer's ring thread parks after 1 ms and
//!    is woken only by `vkNotifyRingMESA`, which Mesa sends only when it reads the IDLE bit — from
//!    **C's** `status` word, which reports *this daemon's watcher*, not S's consumer. The ring
//!    `status` word is a shared-memory channel spec §5's inventory never listed. S now rings its own
//!    doorbell; see `rayland_vtest::venus_ring::doorbell`.
//! 2. **An inline command can overtake the ring bytes it refers to**, because this daemon has two
//!    producers ([`serve_vtest`] on this thread, and the watcher) feeding one link, while Mesa's
//!    protocol assumes the ring is always ahead of the socket. virglrenderer detects it and destroys
//!    the context. See [`RingFlush`].
//! 3. **A blob can be born with its contents already in it** — Mesa creates a readback buffer's blob
//!    at `vkMapMemory`, after the GPU has filled it — so S's baseline swallowed the frame and the
//!    app read its own zeros. See `rayland-s`'s `HostBlob::map`.
//! 4. **A blob's id and its contents must arrive together**, because the reply that carries the id is
//!    what lets Mesa `mmap` the pages. See [`PendingBlob`] and `S2C::BlobCreated`.
//!
//! The pieces with real logic — the ring watcher, the blob shadows, the relay engine, the blob sync
//! — remain unit-tested against a synthetic ring and a mock link (`tests/ring_watch.rs`, and the
//! `tests` modules of [`ring`], [`shm`], [`relay_engine`] and [`blob_sync`]), as is this file's own
//! [`Progress`] (see its `tests` module). Those tests were never worthless — [`Progress`] shipped a
//! real bug they would have caught — but Task 6 is the honest measure of what they could not reach:
//! **not one of the four faults above was a bug in this crate's logic.** Every one was an assumption
//! about Mesa or virglrenderer that only a live peer could refute. That is the lesson, and it is why
//! the loopback e2e is now the gate rather than a demo.
//!
//! This file is written to be read, and it says where it is guessing.

// The daemon's own pieces.
use rayland_c::blob_sync::messages_for_delta;
use rayland_c::link::{QuicRecvLink, QuicSendLink};
use rayland_c::relay_engine::{
    BlobTable, PendingBlob, RelayEngine, RelayLink, RingFlush, RingSlot, commit_pending_blob,
};
use rayland_c::ring::{ParkDecision, RingWatcher, current_tail};
// The relay protocol.
use rayland_relay::{C2S, S2C};
// The vtest server we present to Mesa, and the error type the engine seam speaks.
use rayland_vtest::EngineError;
// Spec §5.1's guard: notice Venus's out-of-line command path rather than relaying a stream that
// would misbehave on S's GPU with no trace of the cause.
use rayland_vtest::venus_ring::scan_for_out_of_line_stream;
use rayland_vtest::vtest::serve_vtest;

use anyhow::{Context, Result};
use std::os::unix::net::UnixListener;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where the daemon listens for Mesa's Venus ICD.
///
/// # Pitfall: this path is length-limited and the limit is not generous
/// A Unix socket path must fit in `sockaddr_un.sun_path`, which is **108 bytes** on Linux. That is
/// not a theoretical constraint here: C0 hit it, which is why this default is terse rather than
/// descriptive. Mesa is pointed at it with `VN_DEBUG=vtest` plus `VTEST_SOCKET_NAME`.
const DEFAULT_VTEST_SOCKET: &str = "/tmp/rl-c1.sock";

/// Environment variable overriding [`DEFAULT_VTEST_SOCKET`].
const ENV_VTEST_SOCKET: &str = "RAYLAND_C1_SOCKET";

/// Environment variable naming S's address, as `host:port`.
///
/// # The transport is QUIC (SP2's), as of (c)1 Task 6
/// Ring-findings §7 is why it is not TCP: **latency, not bandwidth, is what will hurt** — the reply
/// arena was ~12x the command traffic and its replies are round trips the application blocks on, so
/// head-of-line blocking on a single stream is exactly the wrong property.
///
/// **v1 does not yet collect on that**, and the honest statement belongs here rather than in a
/// report: everything still shares **one** QUIC stream, which has the same head-of-line behaviour
/// TCP does. What is bought today is that the endpoint, the handshake and the congestion control
/// exist, so giving the reply path its own stream is a change to [`rayland_c::link`] rather than a
/// transport project.
///
/// QUIC is UDP, so this is a UDP endpoint despite the surrounding talk of connections.
const ENV_S_ADDR: &str = "RAYLAND_C1_S_ADDR";

/// Default address for S.
const DEFAULT_S_ADDR: &str = "127.0.0.1:9401";

/// Environment variable overriding the stall timeout, in seconds.
const ENV_STALL_TIMEOUT: &str = "RAYLAND_C1_STALL_TIMEOUT";

/// How long the ring may have un-acknowledged work before the daemon declares S stalled and exits.
///
/// # Why this exists, and why it is not optional (ring-findings §5.4)
/// Venus has a watchdog, and **it reports liveness it never checks**. The host's monitor thread sets
/// the `ALIVE` bit on every monitored ring every ~3 s, *unconditionally*, without consulting the
/// ring thread's state at all (`vkr_context.c:536-539`). It proves the host process is being
/// scheduled. It proves nothing whatsoever about the ring making progress.
///
/// The consequence, stated plainly in the findings: a transport that faithfully forwards the
/// heartbeat while the ring is stalled converts a **fast, diagnosable 3.5-second abort** into an
/// **895-second hang**. Forwarding it faithfully is the obvious implementation and it is the wrong
/// one. This timeout is the gate on real evidence of progress that the watchdog cannot provide, and
/// it **must exist before anyone sets `VN_DEBUG=no_abort`** — disabling Mesa's abort without it
/// removes the only thing that currently makes a stall visible at all.
///
/// # How this interacts with the ALIVE heartbeat, and why 30 s is reachable at all
/// `rayland-c` does write the ring's ALIVE bit — it has to, or Mesa aborts every slow-but-healthy
/// session at ~3.5 s (see [`RingWatcher::set_alive`](rayland_c::ring::RingWatcher::set_alive)). That
/// could look like the very mistake the findings warn about, and it is not, because of the division
/// of labour between the two mechanisms:
///
/// - The heartbeat is **gated on evidence**: it is set only where an `S2C::RingProgress` has
///   actually advanced `consumed_tail`, never on a timer and never merely because S is reachable.
///   A wedged S therefore stops setting it, exactly as it should.
/// - This timeout is the **independent** backstop, and it is what the findings' "gate on real
///   evidence of progress" ultimately cashes out as. It measures the one thing that matters — the
///   ring not moving — and it is not derived from Mesa's watchdog in any way.
///
/// So the 895-second hang the findings describe cannot happen here: the two are wired to the same
/// evidence, and whichever notices first, the session ends in seconds with a message naming the
/// cause. Note the ordering this implies in the default configuration — the ALIVE gate is what makes
/// *this* 30 s limit reachable at all, since before it existed Mesa would always have aborted at
/// 3.5 s first.
const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the ring watcher sleeps when it has parked.
///
/// It is a *bounded* sleep rather than a wait for the doorbell, and that is deliberate belt-and-
/// braces: the park decision ([`RingWatcher::decide_park`]) is what makes sleeping safe, but Mesa's
/// kick is throttled to at most one per millisecond and is not guaranteed for every write
/// (`vn_ring.c:475-483`). If the park logic were ever wrong, a bounded sleep degrades to latency;
/// an unbounded one degrades to a hang. The cost of being wrong should not be unbounded.
const PARK_SLEEP: Duration = Duration::from_micros(500);

/// How long the ring watcher sleeps while waiting for Mesa to allocate its ring.
///
/// Coarser than [`PARK_SLEEP`] because nothing is happening yet: this runs once, during startup,
/// before the application has produced a single command.
const RING_WAIT_SLEEP: Duration = Duration::from_millis(2);

/// How long [`RingBarrier`] will wait for the watcher to ship the ring before giving up.
///
/// # Why it gives up at all, rather than waiting as long as it takes
/// The barrier's guarantee is only worth having while there is a watcher to make it: if the watcher
/// thread has exited (S vanished, the ring was unref'd, the out-of-line guard fired), the frontier
/// it is waiting on will never move and a patient barrier becomes a **silent hang inside the vtest
/// thread** — which is the one failure shape (c)1 has spent a whole sub-project learning to avoid.
/// So it bounds the wait and says what it saw.
///
/// Generously sized against what it actually measures. The watcher notices Mesa's write within one
/// [`PARK_SLEEP`] (500 µs) and ships it in one network hop; a second is three orders of magnitude
/// more than a healthy loopback needs and still comfortable for a WAN. It is a *liveness* bound, not
/// a latency target — reaching it means something is broken, not that the network is slow.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// How long [`RingBarrier`] sleeps between checks of the watcher's frontier.
///
/// Short against [`PARK_SLEEP`], because this is pure added latency on the inline path and the thing
/// it waits for arrives on the watcher's own poll interval. It costs nothing in aggregate:
/// ring-findings §2 measured the entire inline path at 140–236 bytes for a whole Vulkan
/// initialization, so this loop runs a few dozen times per session, not per frame.
const FLUSH_POLL: Duration = Duration::from_micros(50);

/// The barrier that stops an inline command overtaking the ring bytes it refers to.
///
/// **See [`RingFlush`] for the finding, the live evidence, and why polling faster is not a fix.**
/// This is the implementation: it reads the ring's `tail` as Mesa left it, then waits for the
/// watcher to report having shipped that far.
///
/// # Why it waits on the watcher instead of draining the ring itself
/// Draining is [`RingWatcher::take_delta`]'s job and it hands each byte over exactly once. A second
/// drainer would consume deltas the watcher then never relays — trading an ordering bug for a
/// **data-loss** bug, which is strictly worse and far harder to see. So this thread does not touch
/// the ring's frontier; it only waits for the thread that owns it.
struct RingBarrier {
    /// The blob shadows, for reading the ring's `tail`. See [`BlobTable`]'s lock discipline.
    blobs: BlobTable,
    /// Which resource is the ring. `None` until Mesa creates it — before that there is nothing to
    /// wait for, which is exactly the case for `vkCreateRingMESA` itself.
    ring: RingSlot,
    /// The watcher's frontier, which is what this waits on.
    progress: Arc<Mutex<Progress>>,
}

impl RingFlush for RingBarrier {
    /// Block until the watcher has shipped everything Mesa wrote before this call.
    ///
    /// # Why reading `tail` *now* is the correct target
    /// Mesa stores `tail` and only then sends the command that brought us here, so the value read on
    /// this line is at least as far as anything that command can refer to. Waiting for exactly it is
    /// therefore sufficient — and waiting for more would be waiting for bytes Mesa has not written.
    ///
    /// # Failure modes
    /// Never fails: it returns after [`FLUSH_TIMEOUT`] having said what it saw. Letting the command
    /// cross unordered will probably end the session on S — but it will end it *loudly*, with
    /// virglrenderer naming the seqno it could not reach, and with this message on C's side saying
    /// why. That is a strictly better outcome than a vtest thread parked forever in a barrier.
    fn flush_ring(&self) {
        // No ring, nothing to order against. This is the `vkCreateRingMESA` case: the command that
        // creates the ring cannot be overtaken by bytes in a ring that does not exist yet.
        let Some(identity) = *self
            .ring
            .lock()
            .expect("the ring slot lock is never poisoned")
        else {
            return;
        };

        // The frontier Mesa had reached when it sent the command that brought us here.
        let target = {
            let table = self
                .blobs
                .lock()
                .expect("the blob table lock is never poisoned");
            let Some(blob) = table.get(&identity.res_id) else {
                // The ring's shadow is gone: the session is being torn down. Nothing to order.
                return;
            };
            current_tail(blob.bytes())
        };

        let started = Instant::now();
        loop {
            // `shipped_tail` is recorded *after* the bytes are on the wire, which is the whole point
            // — `relayed_tail` moves before the send and would let this barrier pass while the delta
            // was still queued behind us for the very link lock we are about to take.
            let shipped = self
                .progress
                .lock()
                .expect("the progress lock is never poisoned")
                .shipped_tail;
            // A wrapping compare, like every other frontier comparison here: `tail` is a free-running
            // 2^32 counter, and a plain `>=` would read the wrap as "not there yet" and stall the
            // session permanently once per 4 GiB of commands.
            if shipped.wrapping_sub(target) as i32 >= 0 {
                return;
            }
            if started.elapsed() > FLUSH_TIMEOUT {
                eprintln!(
                    "rayland-c: the ring watcher has not shipped tail {target} within \
                     {FLUSH_TIMEOUT:?} (it stands at {shipped}); letting an inline command cross \
                     ahead of it. If S reports 'ring seqno unable to reach wait seqno' and destroys \
                     the context, this is why — the watcher thread has most likely stopped."
                );
                return;
            }
            std::thread::sleep(FLUSH_POLL);
        }
    }
}

/// What the daemon knows about S's progress through the ring.
///
/// # Why this is two fields and not one
/// Detecting a stall requires distinguishing *"S is slow"* from *"S has stopped"*, and a single
/// timestamp cannot: a session that is legitimately idle (the application is not drawing) looks
/// exactly like a wedged one. So the watcher records **what it has relayed** and the reader records
/// **what S has acknowledged**, and a stall is the two disagreeing for too long. That is precisely
/// the distinction ring-findings §5.4 says Mesa's own watchdog cannot make, because it reports host
/// liveness rather than ring progress.
#[derive(Debug)]
struct Progress {
    /// The highest ring `tail` C has **begun** relaying to S. Recorded *before* the send.
    ///
    /// This is the ceiling [`Progress::note_consumed`] checks S's acknowledgements against, and it
    /// must move first: a fast S can answer a delta before the watcher would get the progress lock
    /// back, so recording it afterwards would see S's legitimate first ack arrive past the frontier
    /// and reject it.
    relayed_tail: u32,
    /// The highest ring `tail` C has **finished** putting on the wire. Recorded *after* the send.
    ///
    /// # Why this is not the same field as `relayed_tail`, and why collapsing them breaks a fix
    /// They differ by exactly the duration of the send, and [`RingBarrier`] lives in that gap. The
    /// barrier's guarantee is *"the delta is on the wire ahead of this inline command"*; against
    /// `relayed_tail` it would pass as soon as the watcher had *decided* to send — while the delta
    /// was still queued behind the very link lock the inline command is about to take. The barrier
    /// would then be satisfied by a delta that crosses second, which is the bug it exists to fix,
    /// reintroduced inside the fix. So the two frontiers are deliberately distinct: one bounds what
    /// S may claim, the other reports what S has been told.
    shipped_tail: u32,
    /// The highest `tail` S has reported fully replaying, from [`S2C::RingProgress`].
    consumed_tail: u32,
    /// When `relayed_tail` last moved ahead of `consumed_tail`. `None` when the two agree, i.e.
    /// when there is nothing outstanding and therefore nothing that could be stalled.
    outstanding_since: Option<Instant>,
}

/// What [`Progress::note_consumed`] made of an acknowledgement from S.
///
/// This is a type rather than a `bool` because the caller treats the three outcomes differently —
/// one is the hot path, one is silently ignorable, and one is a protocol violation worth a human's
/// attention — and because two of them look identical from the outside while meaning opposite
/// things about S's health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ack {
    /// `consumed_tail` moved forward: S genuinely replayed more of the ring. **This is the only
    /// outcome that counts as evidence of progress**, and therefore the only one that may restart
    /// the stall clock or set the ring's ALIVE bit.
    Advanced,
    /// A repeat of a `consumed_tail` already acknowledged. Ignored: see [`Progress::note_consumed`]
    /// for why treating this as progress reintroduces the exact footgun the stall detector exists
    /// to avoid.
    Stale,
    /// An acknowledgement of bytes C never relayed. A protocol error on S's side; ignored, and
    /// reported by the caller.
    PastFrontier,
}

impl Progress {
    /// A session with nothing relayed and nothing outstanding.
    fn new() -> Self {
        Progress {
            relayed_tail: 0,
            shipped_tail: 0,
            consumed_tail: 0,
            outstanding_since: None,
        }
    }

    /// Record that C has finished putting everything up to `tail` on the wire.
    ///
    /// Called by the watcher **after** its batch has been sent, and read only by [`RingBarrier`].
    /// See [`Progress::shipped_tail`] for why this is a separate frontier from
    /// [`Progress::note_relayed`]'s, and why merging the two would silently disarm the barrier.
    fn note_shipped(&mut self, tail: u32) {
        self.shipped_tail = tail;
    }

    /// Record that C has relayed up to `tail`, starting the stall clock if S is now behind.
    fn note_relayed(&mut self, tail: u32) {
        self.relayed_tail = tail;
        // Only start the clock on the *transition* into "something is outstanding". Restarting it
        // on every delta would mean a steadily-relaying C could never time out, no matter how long
        // S had been silent — the stall would be masked by our own liveness rather than S's.
        if self.relayed_tail != self.consumed_tail && self.outstanding_since.is_none() {
            self.outstanding_since = Some(Instant::now());
        }
    }

    /// Record S's acknowledgement, but only if it is real evidence that the ring moved.
    ///
    /// # Why this checks that the tail moved, and why "it arrived" is not the same thing
    /// The obvious implementation restarts the stall clock on any `RingProgress` where S is behind.
    /// That is **the exact footgun ring-findings §5.4 documents**, rebuilt inside the mechanism
    /// written to avoid it. If S ever sends periodic `RingProgress` keepalives — an entirely natural
    /// Task 5 design — then an S that wedges mid-frame while its keepalive thread keeps running
    /// resends `consumed_tail = 4024` forever. Every one of those restarts the clock, and
    /// [`DEFAULT_STALL_TIMEOUT`] never fires. The message would then prove only that S's process is
    /// being scheduled, which is precisely what the findings say the engine's own watchdog proves
    /// and why it is worthless: *"it proves the host process is being scheduled; it proves nothing
    /// whatsoever about the ring making progress"*. So the clock is restarted on **movement**, never
    /// on arrival.
    ///
    /// # Why the frontier is a bound and not a formality
    /// `tail` arrives over the network and is not trusted. Accepting a regressing or over-eager ack
    /// would corrupt `consumed_tail`, which the watcher publishes verbatim as the ring's `head` —
    /// and [`RingWatcher::advance_head`](rayland_c::ring::RingWatcher::advance_head) asserts against
    /// exactly that. Concretely: C relays past one buffer's worth (`relayed_tail = 262144`) and S
    /// resends a stale `consumed = 4024`; the assert computes `262144 - 4024 = 258120 > 131072` and
    /// **panics the watcher thread**. That thread is detached, so the panic does not stop the daemon
    /// — it kills the only thread that relays commands and poisons the `blobs` mutex, after which
    /// every `.expect("the blob table lock is never poisoned")` in this file is a lie. Clamping here
    /// makes that assert unreachable, and matches the standard [`apply_blob_data`] already holds for
    /// remote input.
    ///
    /// Both comparisons are wrapping differences, so they stay correct across the `tail` counter's
    /// 2^32 overflow — the same reason Mesa computes ring occupancy as a difference rather than a
    /// comparison.
    ///
    /// # Inputs / outputs
    /// - `tail`: the `consumed_tail` S reported.
    /// - Returns what was made of it; only [`Ack::Advanced`] mutated any state.
    fn note_consumed(&mut self, tail: u32) -> Ack {
        // How far this ack would move the frontier forward, and how far it *could* legitimately
        // move it. S cannot have replayed bytes C never sent, so `relayed_tail` is the ceiling.
        let advance = tail.wrapping_sub(self.consumed_tail);
        let outstanding = self.relayed_tail.wrapping_sub(self.consumed_tail);

        if advance == 0 {
            // A duplicate of what we already have. Not progress, so the clock keeps running: this
            // is the branch that makes a keepalive-while-wedged S detectable.
            return Ack::Stale;
        }
        if advance > outstanding {
            // Either a regression (which wraps to something enormous) or an ack of bytes we never
            // relayed. Both are S misbehaving; refuse the value rather than publish it as `head`.
            return Ack::PastFrontier;
        }

        self.consumed_tail = tail;
        if self.relayed_tail == self.consumed_tail {
            // Fully acknowledged: nothing is outstanding, so nothing can be stalled.
            self.outstanding_since = None;
        } else {
            // Still behind, but it genuinely moved — S is slow, not stopped. Restart the clock so
            // progress, however gradual, is never mistaken for a stall.
            self.outstanding_since = Some(Instant::now());
        }
        Ack::Advanced
    }

    /// How long work has been outstanding, or `None` if S is fully caught up.
    fn outstanding_for(&self) -> Option<Duration> {
        self.outstanding_since.map(|t| t.elapsed())
    }
}

/// The link half the engine uses: send over the shared socket, receive from the reader thread.
///
/// # Why replies arrive through a channel
/// Only the reader thread calls `recv` on the socket (see the module docs — anything else
/// deadlocks). But [`RelayEngine`] genuinely needs request/reply for the capset and for blob
/// creation, so the reader routes *solicited* replies here while handling unsolicited ones itself.
/// This keeps [`RelayLink`]'s simple send/recv shape — and therefore [`RelayEngine`]'s mock-based
/// tests — intact.
struct ChannelLink {
    /// The shared send half. Also used by the ring watcher, which is why it is behind a mutex.
    tx: Arc<Mutex<QuicSendLink>>,
    /// Solicited replies, routed here by the reader thread.
    replies: Receiver<S2C>,
}

impl RelayLink for ChannelLink {
    fn send(&mut self, m: &C2S) -> Result<(), EngineError> {
        self.tx
            .lock()
            .expect("the link send lock is never poisoned")
            .send(m)
    }

    fn recv(&mut self) -> Result<S2C, EngineError> {
        // This is the *only* place in `rayland-c` where a thread blocks waiting for S, which makes
        // it the only honest place to measure a round trip. Ring deltas are fire-and-forget and cost
        // bandwidth, not latency; the requests that land here — the capset, blob creation — are the
        // ones the application genuinely waits on, and spec §8.1 predicts they cluster at startup
        // and go quiet afterwards. Timing the wait here is what tests that.
        let waited = std::time::Instant::now();
        // A closed channel means the reader thread is gone, i.e. S dropped the connection. That is
        // a link failure, not an end of stream: whoever is waiting here will never get its answer.
        let r = self
            .replies
            .recv()
            .map_err(|_| EngineError::RelayLinkFailed {
                detail: "the reader thread ended before S answered this request".into(),
            });
        // Record the stall even when the wait ended in failure: time spent blocked on an answer that
        // never came is still time the application lost, and dropping it would flatter a failing
        // link by making its worst stalls invisible.
        rayland_c::metrics::metrics().round_trip(waited.elapsed());
        r
    }
}

/// The reader thread: own `recv`, and route everything S says.
///
/// # The routing rule
/// A message is either an **answer to a request** (`Capset`, `BlobCreated`, `Error`), which goes to
/// whoever is blocked waiting for it, or it is **unsolicited** (`BlobData`, `RingProgress`), which
/// this thread acts on directly. Nothing is dropped: an unroutable message is a protocol error and
/// is logged rather than ignored.
///
/// # Inputs / outputs
/// - `rx`: the receive half of the link. Owned exclusively — nothing else may `recv`.
/// - `replies`: where solicited replies are sent.
/// - `blobs` / `progress`: shared state this thread writes.
/// - `pending`: the shadow awaiting S's id. This thread commits it on `S2C::BlobCreated`, because it
///   is the only thread that learns the id before the blob's own data arrives — see `PendingBlob`.
/// - Returns when S closes the link or a read fails; the session is over either way.
fn reader_thread(
    mut rx: QuicRecvLink,
    replies: Sender<S2C>,
    blobs: BlobTable,
    progress: Arc<Mutex<Progress>>,
    pending: PendingBlob,
) {
    loop {
        let msg = match rx.recv() {
            Ok(m) => m,
            Err(e) => {
                // Not necessarily an error: a clean shutdown ends here too. Either way the session
                // is over, and dropping `replies` unblocks anyone waiting on an answer that is
                // never coming rather than leaving them hung.
                eprintln!("rayland-c: link to S ended: {e}");
                return;
            }
        };

        match msg {
            // The reply path the application is blocked on. Ring-findings §7 measured the reply
            // arena at ~12x the command traffic: this is the bulk of the session, not an edge case.
            S2C::BlobData {
                res_id,
                offset,
                bytes,
            } => {
                if let Err(e) = apply_blob_data(&blobs, res_id, offset, &bytes) {
                    eprintln!("rayland-c: {e}");
                }
            }
            // S's acknowledgement. Under this daemon's design this is what drives `head`, so it is
            // load-bearing rather than diagnostic — see `ring_watcher_thread`.
            S2C::RingProgress { consumed_tail, .. } => {
                let ack = progress
                    .lock()
                    .expect("the progress lock is never poisoned")
                    .note_consumed(consumed_tail);
                // A stale repeat is unremarkable — a Task 5 keepalive would produce them by design,
                // and `note_consumed` deliberately ignores them. An ack of bytes C never relayed is
                // not: it means S and C disagree about the ring's contents, which is the kind of
                // desynchronization that surfaces later as inexplicable GPU errors, so name it here
                // where the cause is visible.
                if ack == Ack::PastFrontier {
                    eprintln!(
                        "rayland-c: S acknowledged ring tail {consumed_tail}, which is past \
                         anything C has relayed. Ignoring it — publishing it as `head` would let \
                         Mesa overwrite commands that were never shipped. This is a protocol error \
                         on S."
                    );
                }
            }
            // **An error about something nobody asked for must not be queued as an answer.**
            //
            // Most of this protocol is fire-and-forget, and the ring watcher — which never waits for
            // anything — sends a `RingDelta` and a `BlobData` per delta, many times a second. If S
            // refuses one of those, routing its error to `replies` puts an unasked-for message at the
            // head of the queue, where it answers the *next* request; every request after that is
            // then answered by the previous one's reply, permanently. The desynchronization is
            // unbounded and surfaces arbitrarily far from its cause.
            //
            // S is the only party that can tell the two cases apart, because only S knows which
            // message it was refusing (an `Error` names nothing), which is why `solicited` is on the
            // wire at all. Logged rather than dropped: this is a genuine failure on S, and it is
            // C-side evidence for a human who has only C's log in front of them.
            S2C::Error {
                message,
                solicited: false,
            } => {
                eprintln!(
                    "rayland-c: S refused a fire-and-forget message: {message}\n\
                     rayland-c: nothing on C was waiting for this, so it is reported here rather \
                     than answered. The session is likely now producing a stream S cannot replay."
                );
            }
            // Solicited (`Capset`, `BlobCreated`, and an `Error` that genuinely answers a request):
            // queue it for whoever asked. The channel is unbounded and its receiver lives inside the
            // engine for the whole session, so a send failure here means only one thing: the engine
            // has been dropped, i.e. the vtest session is over. That is a reason to stop, not an
            // error to report.
            other => {
                // **Commit the staged shadow here, before the reply is forwarded.** Both halves of
                // that matter. Registering it here rather than on the vtest thread means S's next
                // message — routinely this blob's own data — finds a shadow to land in. And doing it
                // *before* forwarding is what makes the blob's initial contents safe: this reply
                // releases the vtest thread, which hands Mesa the descriptor, which Mesa `mmap`s and
                // the application reads. `PendingBlob` and `S2C::BlobCreated` carry the full
                // argument and the run each cost.
                if let S2C::BlobCreated { res_id, initial } = &other {
                    commit_pending_blob(&pending, &blobs, *res_id, initial);
                }
                if replies.send(other).is_err() {
                    return;
                }
            }
        }
    }
}

/// Write bytes S produced into the local pages Mesa mapped.
///
/// # Why the bounds check is not paranoia
/// `offset` and `bytes.len()` arrive over the network. Writing past the end of a blob's mapping
/// would be a heap/mapping overflow driven by a remote peer. The slice is bounds-checked against the
/// real mapping length before anything is copied.
///
/// # Inputs / outputs
/// - `blobs`: the shadow table.
/// - `res_id` / `offset` / `bytes`: the message's fields.
/// - Returns an error describing an unknown resource or an out-of-range write; both are protocol
///   errors on S's side, and both are reported rather than silently skipped, because a reply that
///   never lands is an application that hangs with no explanation.
fn apply_blob_data(blobs: &BlobTable, res_id: u32, offset: u64, bytes: &[u8]) -> Result<()> {
    let mut table = blobs.lock().expect("the blob table lock is never poisoned");
    let blob = table.get_mut(&res_id).with_context(|| {
        format!("S sent BlobData for resource {res_id}, which C has no shadow of")
    })?;

    // Compute the destination range in `u64` first: a `usize` cast before the check could wrap on a
    // 32-bit target and turn an out-of-range write into an in-range one.
    let end = offset.checked_add(bytes.len() as u64).with_context(|| {
        format!(
            "BlobData for resource {res_id}: offset {offset} + {} overflows",
            bytes.len()
        )
    })?;
    anyhow::ensure!(
        end <= blob.size(),
        "BlobData for resource {res_id} writes {offset}..{end}, past the blob's {} bytes",
        blob.size()
    );

    let start = offset as usize;
    blob.bytes_mut()[start..end as usize].copy_from_slice(bytes);
    Ok(())
}

/// The ring watcher thread: notice Mesa's commands, ship them, and keep `head` honest.
///
/// **This is the loop the whole sub-project exists for.** Everything else is setup.
///
/// # The `head` sequencing, and a deliberate deviation from this task's brief
/// `head` is advanced from [`S2C::RingProgress`] — that is, only once **S** reports having replayed
/// the bytes — rather than as soon as C has relayed them.
///
/// This task's brief specified the opposite (advance `head` locally on relay, making C's ring a
/// pure staging buffer) and recorded the consequence as being about *backpressure*: that a slow S
/// would block C's relay rather than stall the application. That reasoning is right as far as it
/// goes, but `head` does not only mean "space is free". Mesa polls it as the **reply-ready signal**:
///
/// ```c
/// /* vn_ring.c:722-727 */
/// if (submit->reply_size) {
///    ...
///    VN_CS_DECODER_INITIALIZER(reply_ptr, submit->reply_size);
///    vn_ring_wait_seqno(ring, submit->ring_seqno);   /* polls head until it reaches the seqno */
/// ```
///
/// So advancing `head` on relay releases the application's wait *before S has answered*, and it
/// reads a reply arena that is still zeros. Ring-findings §7 names this exact ordering constraint —
/// "a transport must ship the reply-shmem contents before it ships the head update that releases the
/// client's wait" — and warns it produces once-an-hour heisenbugs. Under a local advance it would
/// not be once an hour: every reply-bearing command hits it, starting with the
/// `vkEnumerateInstanceVersion` that is command #2 of every capture.
///
/// The cost of doing it correctly is the round trip the brief hoped to remove, and ring-findings §7
/// is clear that this is the real ceiling: ≤128 KiB in flight per RTT ≈ 1.3 MB/s at 100 ms. The
/// lever it identifies is a **bigger ring** (`buf_size` is a client-side constant and S's engine caps
/// rings at 16 MiB), not a locally-forged `head`. **[UNVERIFIED]** — this daemon has never run
/// against a real S, so the ordering argued here is reasoned from source, not observed.
fn ring_watcher_thread(
    ring_slot: RingSlot,
    blobs: BlobTable,
    tx: Arc<Mutex<QuicSendLink>>,
    progress: Arc<Mutex<Progress>>,
    stall_timeout: Duration,
) {
    // Wait for Mesa to allocate its ring. Nothing to watch until it does.
    let identity = loop {
        if let Some(id) = *ring_slot
            .lock()
            .expect("the ring slot lock is never poisoned")
        {
            break id;
        }
        std::thread::sleep(RING_WAIT_SLEEP);
    };
    eprintln!(
        "rayland-c: watching command ring res_id={} buffer_size={}",
        identity.res_id, identity.buffer_size
    );
    let mut watcher = RingWatcher::new(identity.res_id, identity.buffer_size);
    // The last value written to the ring's `head`. A fresh ring's `head` is already 0, so starting
    // here means the first pass writes nothing until S has genuinely acknowledged something.
    let mut published_head: u32 = 0;
    // Whether the IDLE bit currently stands published. Tracked rather than recomputed for the same
    // reason `published_head` is: `status` is a word Mesa polls from another thread on every submit,
    // and this loop runs every few hundred microseconds. Clearing a bit that is already clear on
    // every pass of a busy burst would dirty that shared cache line continuously — precisely the
    // coherence traffic Mesa's 64-byte `alignas` on these words exists to prevent (ring-findings §4).
    let mut idle_published = false;

    loop {
        // --- 1. Drain, and retract IDLE if we found anything. The lock is held only long enough to
        // copy the bytes out, never across the network send below: the reader thread needs this
        // table to deliver replies.
        let delta = {
            let mut table = blobs.lock().expect("the blob table lock is never poisoned");
            let Some(blob) = table.get_mut(&identity.res_id) else {
                // The ring was unref'd: the session is over.
                eprintln!("rayland-c: the command ring is gone; watcher stopping");
                return;
            };
            let delta = watcher.take_delta(blob.bytes());
            // Draining bytes proves this watcher is awake, so the IDLE claim published before the
            // last park must go — and it must go *here*, before the network send below, not on some
            // later pass. Mesa tests IDLE on every submit (`vn_ring.c:475-483`) and doorbells if it
            // is set; leaving it standing through a busy burst means an application at 60 fps drives
            // ~1000 spurious `vkNotifyRingMESA` calls a second, each one a socket write on C and a
            // relayed message to S, all to wake a thread that never slept. Ring-findings §5.2's
            // headline result is that the steady state emits *zero* notifications; this is the line
            // that keeps that true. `RingWatcher::clear_idle` documents the mechanism.
            if delta.is_some() && idle_published {
                watcher.clear_idle(blob.bytes_mut());
                idle_published = false;
            }
            delta
        };

        // --- 2. Relay, with no lock held. These bytes are the application's Vulkan commands.
        if let Some(delta) = delta {
            let tail = delta.tail;
            // Before anything crosses: refuse a stream (c)1 v1 cannot faithfully carry. Venus
            // replaces any submission over `direct_size` (8192 B for the 128 KiB instance ring) with
            // a `vkExecuteCommandStreamsMESA` pointing at *other* shmems this version never ships —
            // S would then resolve those ids to blobs it holds and execute their contents, which are
            // zeros. Spec §5.1 requires that this never be silent, and the scan is deliberately a
            // sound over-approximation rather than a decode; `scan_for_out_of_line_stream`'s module
            // docs carry the whole argument and the reason a decode-based check could not work.
            //
            // Exiting rather than continuing, for the same reason the stall below exits: this thread
            // is detached and the vtest thread is blocked inside `serve_vtest`, so there is nothing
            // to return an error to — and a relay that carried on would corrupt S's stream, which
            // surfaces as inexplicable GPU misbehaviour nowhere near the cause.
            if let Err(found) = scan_for_out_of_line_stream(&delta.bytes) {
                eprintln!(
                    "rayland-c: refusing to relay the ring delta ending at tail {tail}: {found}"
                );
                std::process::exit(1);
            }
            // Record the frontier *before* the send, not after. `Progress::note_consumed` refuses
            // any acknowledgement past `relayed_tail`, and a fast S can answer this delta before
            // this thread would reacquire the progress lock — so noting it afterwards would race,
            // and S's legitimate first ack would be rejected as `Ack::PastFrontier`. Doing it first
            // is also strictly conservative for the stall clock: it starts marginally earlier
            // (it now includes the time the send itself takes), never later.
            progress
                .lock()
                .expect("the progress lock is never poisoned")
                .note_relayed(tail);

            // Decide what must accompany this delta, and in what order. `messages_for_delta` copies
            // the application's mapped blobs out under the blob lock and releases it before
            // returning, so the send below cannot hold it — the discipline `BlobTable` documents,
            // made structural. The order it returns is a correctness contract: the app's vertices
            // must be on S before the commands that read them, because S's ring thread dispatches
            // the instant `tail` lands. See `crate::blob_sync`.
            let msgs = messages_for_delta(&blobs, identity.res_id, delta);

            // One lock for the whole batch. This loop is the only ring watcher there is, so it
            // cannot overtake itself — the reason to hold one lock is the *other* thread sharing
            // `tx`. The vtest thread's `RelayEngine` sends over this same `Arc<Mutex<TcpLink>>` (via
            // `ChannelLink`, e.g. for blob creation), and dropping the lock between messages would
            // let one of its sends land in the middle of this batch, between a blob and the delta
            // that must follow it. Holding one lock for the whole batch keeps it atomic against that
            // thread — and it is also simply cheaper than re-locking per message.
            {
                let mut link = tx.lock().expect("the link send lock is never poisoned");
                for msg in &msgs {
                    if let Err(e) = link.send(msg) {
                        eprintln!("rayland-c: relaying the ring to S failed: {e}");
                        return;
                    }
                }
                // Publish the shipped frontier **while still holding the link lock**. This is what
                // `RingBarrier` waits on, and the placement is the whole of its correctness: an
                // inline command can only cross by taking this same lock, so a barrier released here
                // is released with the delta already ahead of it on the wire. Moving this line after
                // the unlock would open exactly the gap the barrier exists to close — the inline
                // command could win the lock and overtake the delta between the two statements.
                progress
                    .lock()
                    .expect("the progress lock is never poisoned")
                    .note_shipped(tail);
            }
            // New work was just produced, so do not even consider parking: go straight back and
            // look again. An application mid-frame produces continuously. IDLE was retracted in
            // step 1, so this shortcut leaves no stale claim behind.
            continue;
        }

        // --- 3. Publish whatever S has acknowledged as the new `head`, and check for a stall.
        let (consumed, outstanding) = {
            let p = progress
                .lock()
                .expect("the progress lock is never poisoned");
            (p.consumed_tail, p.outstanding_for())
        };
        // Written as a nested `if` rather than a let-chain: let-chains stabilized in Rust 1.88 and
        // this crate declares `rust-version = "1.85"`, so a let-chain would compile on a modern
        // toolchain while quietly breaking the MSRV the manifest promises.
        if let Some(waited) = outstanding {
            if waited > stall_timeout {
                // Mesa's watchdog would report ALIVE here without ever consulting the ring
                // (ring-findings §5.4), so this is the only thing that can tell the difference.
                eprintln!(
                    "rayland-c: S has not acknowledged ring progress for {waited:?} (limit {stall_timeout:?}). \
                     The ring is stalled. Exiting rather than hanging — Mesa's own watchdog reports host \
                     liveness, not ring progress, and would happily let this hang for ~895 seconds."
                );
                std::process::exit(1);
            }
        }
        // Republish `head` only when S's acknowledgement has actually moved. Writing an unchanged
        // value would be harmless to correctness and wrong for performance: this loop runs every
        // few hundred microseconds, and `head` is a word Mesa polls continuously from another
        // thread. Storing to it needlessly dirties the cache line on every pass, which is precisely
        // the coherence traffic Mesa's 64-byte `alignas` on these words exists to avoid
        // (ring-findings §4).
        if consumed != published_head {
            let mut table = blobs.lock().expect("the blob table lock is never poisoned");
            if let Some(blob) = table.get_mut(&identity.res_id) {
                // Only ever the frontier we have relayed *and* S has acknowledged. `advance_head`
                // refuses anything past what this watcher drained, which is the backstop against
                // S reporting a tail we never sent (`Progress::note_consumed` is the first line of
                // that defence and makes this assert unreachable).
                watcher.advance_head(blob.bytes_mut(), consumed);
                // Reaching here *is* the evidence of ring progress: `consumed` only ever advances
                // (`note_consumed` clamps it) and it differs from what we last published, so S has
                // demonstrably replayed more of the ring since the last heartbeat. That is exactly
                // the gate ring-findings §5.4 demands — and the reason this is not simply forwarding
                // Mesa's own watchdog, which would set ALIVE on a wedged ring just as happily.
                //
                // Without this, C's ring never has ALIVE set by anyone (S's virglrenderer sets it on
                // S's mirror, which never crosses back), and Mesa aborts the application ~3.5 s into
                // any wait. `RingWatcher::set_alive` documents the abort path and the honest limit of
                // this gate: it cannot cover an S that is healthy but silent inside one long command.
                watcher.set_alive(blob.bytes_mut());
            }
            published_head = consumed;
        }

        // --- 4. Announce IDLE, then re-check before sleeping. This two-step is the whole defence
        // against the hang: Mesa's kick is throttled to at most one per millisecond and is not
        // guaranteed for every write, so a watcher that published IDLE and slept unconditionally
        // would sleep through pending work. See `RingWatcher::decide_park`.
        let decision = {
            let mut table = blobs.lock().expect("the blob table lock is never poisoned");
            let Some(blob) = table.get_mut(&identity.res_id) else {
                return;
            };
            watcher.publish_idle(blob.bytes_mut());
            watcher.decide_park(blob.bytes_mut())
        };
        match decision {
            // Nothing pending and IDLE is published, so Mesa knows to kick us. Sleep — but only for
            // a bounded time; see `PARK_SLEEP`.
            ParkDecision::Park => {
                // The claim now stands, and step 1 owes Mesa its retraction the moment we drain
                // anything. This is the only place that becomes true.
                idle_published = true;
                std::thread::sleep(PARK_SLEEP);
            }
            // Mesa wrote while we were announcing. `decide_park` has already retracted IDLE on our
            // behalf, so record that and go straight back.
            ParkDecision::StayAwake => {
                idle_published = false;
                continue;
            }
        }
    }
}

/// Read an environment variable, falling back to a default.
fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Bring up the daemon: connect to S, listen for Mesa, and run the three threads.
///
/// # Failure modes
/// Returns an error if S is unreachable, if the local socket cannot be bound, or if the vtest
/// session fails. A stalled ring is *not* reported here — it exits the process from the watcher
/// thread, because by then this thread is blocked inside [`serve_vtest`] and cannot be signalled.
fn main() -> Result<()> {
    let socket_path = env_or(ENV_VTEST_SOCKET, DEFAULT_VTEST_SOCKET);
    let s_addr = env_or(ENV_S_ADDR, DEFAULT_S_ADDR);
    // A malformed timeout is a silent misconfiguration if it falls back quietly, so say so.
    let stall_timeout = match std::env::var(ENV_STALL_TIMEOUT) {
        Ok(v) => match v.parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs),
            Err(e) => {
                eprintln!(
                    "rayland-c: ignoring unparseable {ENV_STALL_TIMEOUT}={v:?} ({e}); using default"
                );
                DEFAULT_STALL_TIMEOUT
            }
        },
        Err(_) => DEFAULT_STALL_TIMEOUT,
    };

    // --- Connect to S first. If the GPU machine is unreachable there is no point letting an
    // application start and then fail halfway through its Vulkan initialization.
    //
    // QUIC needs no Nagle switch: the TCP placeholder this replaces had to disable it, because
    // coalescing a doorbell or a small ring delta by up to 40 ms is 40 ms the application spends
    // blocked on a reply (ring-findings §7: latency is what will hurt, not bandwidth). quinn sends
    // what it is given when it is given it, so there is nothing to turn off.
    //
    // The split gives the reader thread its own half: it blocks in `recv` while the vtest thread and
    // the watcher send on the other, with no lock between them. That is not an optimization — see
    // the module docs for the deadlock a single-owner design causes.
    let s_socket = s_addr
        .parse()
        .with_context(|| format!("{ENV_S_ADDR}={s_addr:?} is not a valid host:port address"))?;
    // Start Task 9's clock *before* the QUIC handshake, not after: spec §8.1 predicts startup is
    // round-trip-bound but one-off, and a time-to-first-frame that excluded the handshake could not
    // test that claim. `metrics()` fixes the start instant on first call, so this call is the
    // measurement's zero. It is a no-op unless RAYLAND_C1_METRICS is set.
    rayland_c::metrics::metrics();
    // The reporter prints running totals every 100 ms, so the sweep harness always holds a last-good
    // sample even when it kills the daemon rather than letting it exit.
    rayland_c::metrics::start_reporter();
    let (tx, rx) = rayland_c::link::connect(s_socket).map_err(|e| {
        anyhow::anyhow!("connecting to S at {s_addr} (set {ENV_S_ADDR} to change it): {e}")
    })?;
    let tx = Arc::new(Mutex::new(tx));

    // The session handshake belongs to whoever owns the connection, not to the engine.
    tx.lock()
        .expect("the link send lock is never poisoned")
        .send(&C2S::Hello {
            // The version our vtest server implements and will negotiate with Mesa, so S can reject
            // a mismatch loudly and early rather than misdecoding bytes later.
            vtest_protocol_version: 4,
        })
        .map_err(|e| anyhow::anyhow!("greeting S: {e}"))?;

    // --- Shared state, and the engine that will populate it.
    let progress = Arc::new(Mutex::new(Progress::new()));
    let (reply_tx, reply_rx) = channel();
    let mut engine = RelayEngine::new(ChannelLink {
        tx: Arc::clone(&tx),
        replies: reply_rx,
    });
    let blobs = engine.blobs();
    let ring_slot = engine.ring();

    // **Arm the barrier.** Without this an inline `vkWaitRingSeqnoMESA` can reach S ahead of the
    // ring delta that satisfies it, and virglrenderer answers by destroying the context rather than
    // waiting — see `RingFlush`. It is installed here, after the engine exists, because it needs the
    // blob table and the ring slot that the engine owns.
    engine.set_ring_flush(Arc::new(RingBarrier {
        blobs: Arc::clone(&blobs),
        ring: Arc::clone(&ring_slot),
        progress: Arc::clone(&progress),
    }));

    // --- The reader: the only thing that may `recv`. See the module docs for why a design without
    // it deadlocks.
    std::thread::Builder::new()
        .name("rayland-c-reader".into())
        .spawn({
            let blobs = Arc::clone(&blobs);
            let progress = Arc::clone(&progress);
            let pending = engine.pending();
            move || reader_thread(rx, reply_tx, blobs, progress, pending)
        })
        .context("spawning the reader thread")?;

    // --- The watcher: the loop that carries 100% of the application's Vulkan commands.
    std::thread::Builder::new()
        .name("rayland-c-ring".into())
        .spawn({
            let blobs = Arc::clone(&blobs);
            let progress = Arc::clone(&progress);
            let tx = Arc::clone(&tx);
            move || ring_watcher_thread(ring_slot, blobs, tx, progress, stall_timeout)
        })
        .context("spawning the ring watcher thread")?;

    // --- Listen for Mesa. A stale socket from a previous run would make `bind` fail with
    // EADDRINUSE even though nothing is listening, so clear it first. This is safe for the intended
    // one-session-per-daemon model and would not be if two daemons could share a path.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding the vtest socket at {socket_path}"))?;
    eprintln!(
        "rayland-c: listening at {socket_path}, relaying to S at {s_addr}\n\
         rayland-c: point Mesa at it with:\n  \
         VN_DEBUG=vtest VTEST_SOCKET_NAME={socket_path} VK_ICD_FILENAMES=<...>/virtio_icd.x86_64.json <app>"
    );

    // One connection, then done: vtest is one context per connection, and (c)1's walking skeleton
    // serves a single application.
    let (mut mesa, _) = listener.accept().context("accepting Mesa's connection")?;
    eprintln!("rayland-c: Mesa connected");
    // Mark the application's arrival for Task 9. Everything before this instant is the daemon
    // waiting on a socket for an application that has not been started yet — in the sweep, three
    // seconds of the harness sleeping while the daemon comes up. A time-to-first-frame measured
    // from daemon startup therefore reports the harness's sleep, not the protocol's cost, which is
    // exactly the wrong answer to spec §8.1's "startup is RTT-bound but one-off". Both figures are
    // kept: from-startup includes the QUIC handshake (which §8.1 is also about), from-connect is
    // what the *application* waits.
    rayland_c::metrics::metrics().note_app_connected();

    let outcome = serve_vtest(&mut mesa, &mut engine)
        .map_err(|e| anyhow::anyhow!("the vtest session failed: {e}"))?;
    eprintln!(
        "rayland-c: session ended cleanly (context {:?}, {} inline batches relayed)",
        outcome.context_id, outcome.submitted_batches
    );
    // Emit the authoritative totals, marked `final=1`.
    //
    // # Why this line matters more than the periodic ones, and what it cost to learn
    // A harness that samples the periodic prints when the *application* exits is sampling too early:
    // the application is gone, but this daemon is still relaying. The first sweep measured the same
    // refapp cell at 60,319 and 10,091 bytes C->S on two runs — a 6x spread that looked like a
    // finding about Venus and was really the harness reading the log before the session had
    // finished. Both numbers were monotonic, both were honestly printed, and one was of a session
    // that was still happening.
    //
    // The monotonic-max rule protects against a truncated *print*; it cannot protect against a
    // truncated *session*. This line is the marker that says the session is over, so a harness can
    // wait for it rather than guess. See `scripts/c1-sweep.sh`, which waits for the daemon to exit.
    rayland_c::metrics::report_final();
    Ok(())
}

/// Unit tests for [`Progress`], the daemon's stall detector.
///
/// # Why this struct is tested when the rest of the file is not
/// The module docs are honest that this file's *wiring* — sockets, threads, the vtest session — has
/// no live peer to run against until (c)1 Task 6 links it to S, and so is not covered. [`Progress`]
/// is the exception and deserves to be: it is three fields and no I/O, its whole job is a decision
/// that is invisible until it is wrong, and the bug it shipped with (restarting the stall clock on any
/// acknowledgement, whether or not the ring had moved) was pure logic that any of these tests would
/// have caught. "There is no S to run against" is a reason to test the parts that do not need one,
/// not a reason to test nothing.
#[cfg(test)]
mod tests {
    use super::*;

    /// The frontier from the live capture: 4024 bytes of ring traffic for the reference app's whole
    /// Vulkan initialization (ring-findings §2). Used throughout so the numbers are the real ones.
    const FIRST_FRONTIER: u32 = 4024;

    /// Nothing relayed means nothing outstanding: an idle session must never look stalled. Without
    /// this, an application that simply is not drawing would be killed by the stall timeout.
    #[test]
    fn an_idle_session_has_nothing_outstanding() {
        let p = Progress::new();
        assert_eq!(p.outstanding_for(), None);
    }

    /// Relaying bytes S has not acknowledged starts the stall clock. This is the "tail advanced but
    /// no progress arrived" case, and it is the one the timeout exists to fire on.
    #[test]
    fn relaying_unacknowledged_bytes_starts_the_stall_clock() {
        let mut p = Progress::new();
        p.note_relayed(FIRST_FRONTIER);

        assert!(
            p.outstanding_for().is_some(),
            "work relayed but unacknowledged must be visible to the stall timeout"
        );
    }

    /// The clock, once started, is not restarted by C's own continued relaying. Otherwise a steadily
    /// producing application would mask an S that had been silent for hours: the daemon would be
    /// measuring its own liveness rather than S's.
    #[test]
    fn c_relaying_more_does_not_restart_the_clock() {
        let mut p = Progress::new();
        p.note_relayed(FIRST_FRONTIER);
        let started_at = p.outstanding_since;

        p.note_relayed(FIRST_FRONTIER * 2);

        assert_eq!(
            p.outstanding_since, started_at,
            "only S's progress may restart the clock; C's own relaying is not evidence about S"
        );
    }

    /// A full acknowledgement stops the clock: nothing is outstanding, so nothing can be stalled.
    #[test]
    fn a_full_acknowledgement_clears_the_clock() {
        let mut p = Progress::new();
        p.note_relayed(FIRST_FRONTIER);

        assert_eq!(p.note_consumed(FIRST_FRONTIER), Ack::Advanced);

        assert_eq!(
            p.outstanding_for(),
            None,
            "S has caught up entirely; there is nothing left to be stalled on"
        );
    }

    /// A partial but genuine acknowledgement restarts the clock: S is slow, not stopped, and gradual
    /// progress must never be mistaken for a stall.
    #[test]
    fn a_genuinely_advancing_acknowledgement_restarts_the_clock() {
        let mut p = Progress::new();
        p.note_relayed(FIRST_FRONTIER);
        // Backdate the clock so a failure to restart it is unambiguous rather than a matter of
        // microseconds: `Instant::now()` twice in a row can be indistinguishable.
        p.outstanding_since = Some(backdated(Duration::from_secs(20)));

        assert_eq!(p.note_consumed(FIRST_FRONTIER / 2), Ack::Advanced);

        let waited = p
            .outstanding_for()
            .expect("still behind, so still outstanding");
        assert!(
            waited < Duration::from_secs(1),
            "a genuinely advancing ack must restart the clock, but it still reads {waited:?}"
        );
    }

    /// **The regression test for the stall detector's original bug.**
    ///
    /// A repeated acknowledgement is not progress. If S ever sends periodic `RingProgress`
    /// keepalives — an entirely natural Task 5 design — then an S that wedges mid-frame while its
    /// keepalive thread keeps running resends the same `consumed_tail` forever. Restarting the clock
    /// on those makes `RAYLAND_C1_STALL_TIMEOUT` unreachable and rebuilds ring-findings §5.4's
    /// footgun ("it proves the host process is being scheduled; it proves nothing whatsoever about
    /// the ring making progress") inside the mechanism written to avoid it.
    #[test]
    fn a_repeated_acknowledgement_does_not_restart_the_clock() {
        let mut p = Progress::new();
        p.note_relayed(FIRST_FRONTIER);
        // S acknowledges half, then wedges.
        assert_eq!(p.note_consumed(FIRST_FRONTIER / 2), Ack::Advanced);
        // Twenty seconds pass with the ring not moving.
        p.outstanding_since = Some(backdated(Duration::from_secs(20)));

        // The keepalive: the same tail, resent. The ring has not moved by one byte.
        assert_eq!(
            p.note_consumed(FIRST_FRONTIER / 2),
            Ack::Stale,
            "an unchanged consumed_tail is not evidence of progress"
        );

        let waited = p
            .outstanding_for()
            .expect("still behind, so still outstanding");
        assert!(
            waited >= Duration::from_secs(20),
            "a stale ack must leave the stall clock running, but it was reset to {waited:?}; \
             a wedged S with a live keepalive thread would never be detected"
        );
    }

    /// A regressing acknowledgement is refused rather than applied.
    ///
    /// This is the exact scenario that panics the watcher: C has relayed past one buffer's worth and
    /// S resends a stale, much lower `consumed`. Accepting it would let the watcher publish it as
    /// `head`, and `RingWatcher::advance_head`'s guard computes `262144 - 4024 = 258120 > 131072`
    /// and panics — on a *detached* thread, so the daemon does not die loudly; it loses the only
    /// thread that relays commands and poisons the blob table's mutex.
    #[test]
    fn a_regressing_acknowledgement_is_refused() {
        let mut p = Progress::new();
        // Two buffers' worth relayed, and S has acknowledged the first frontier.
        p.note_relayed(262144);
        assert_eq!(p.note_consumed(131072), Ack::Advanced);

        // A stale duplicate from before, arriving late.
        assert_eq!(
            p.note_consumed(FIRST_FRONTIER),
            Ack::PastFrontier,
            "an ack below the one already recorded must not be applied"
        );
        assert_eq!(
            p.consumed_tail, 131072,
            "the frontier must not regress: the watcher publishes it verbatim as the ring's head"
        );
    }

    /// An acknowledgement of bytes C never relayed is refused. S cannot have replayed what it was
    /// never sent, so this is a protocol error — and publishing it as `head` would invite Mesa to
    /// overwrite commands still waiting to be shipped.
    #[test]
    fn an_acknowledgement_past_the_relayed_frontier_is_refused() {
        let mut p = Progress::new();
        p.note_relayed(FIRST_FRONTIER);

        assert_eq!(
            p.note_consumed(FIRST_FRONTIER + 4),
            Ack::PastFrontier,
            "S claims to have replayed 4 bytes C never sent it"
        );
        assert_eq!(
            p.consumed_tail, 0,
            "a refused ack must not move the frontier"
        );
    }

    /// The frontier arithmetic survives the `tail` counter's 2^32 wrap, which happens once per 4 GiB
    /// of commands. Both the "did it move" and the "is it past the frontier" tests are wrapping
    /// differences for this reason — a plain `<` comparison would read the wrap as a regression and
    /// refuse every acknowledgement from then on, permanently wedging the session.
    #[test]
    fn acknowledgements_survive_the_counter_wrap() {
        let mut p = Progress::new();
        // Relayed frontier sits just past the wrap; S's last ack sits just before it.
        p.consumed_tail = u32::MAX - 100;
        p.note_relayed(24); // i.e. 124 bytes further on, having wrapped through 0.

        assert_eq!(
            p.note_consumed(12),
            Ack::Advanced,
            "an ack that wrapped past 0 is forward progress, not a regression"
        );
        assert_eq!(p.consumed_tail, 12);
    }

    /// An `Instant` `ago` in the past, for tests that must observe a clock that has genuinely run.
    ///
    /// `checked_sub` rather than plain subtraction because `Instant`'s epoch is unspecified and may
    /// be close to the process start on some platforms; a panic here would be a broken test rather
    /// than a broken daemon, so it says so.
    fn backdated(ago: Duration) -> Instant {
        Instant::now()
            .checked_sub(ago)
            .expect("the test clock must be able to reach into the past")
    }
}
