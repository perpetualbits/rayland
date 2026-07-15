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
//! # Status: what has and has not been run
//! **This binary has never been run end-to-end, because there is nothing to run it against.**
//! `rayland-s` is (c)1 Task 5 and does not exist; the QUIC transport is Task 6. The pieces with real
//! logic — the ring watcher, the blob shadows, the relay engine — are unit-tested against a
//! synthetic ring and a mock link (`tests/ring_watch.rs`, and the `tests` modules of [`ring`],
//! [`shm`] and [`relay_engine`]), as is this file's own [`Progress`] (see its `tests` module).
//!
//! What remains uncovered is the *wiring*: the sockets, the three threads, and the vtest session.
//! That genuinely needs a peer and must be treated as unverified until Task 5 provides one. The
//! distinction matters, because "there is no S to run against" is a reason to test the parts that do
//! not need one — not a licence to test nothing. [`Progress`] shipped a real bug behind exactly that
//! excuse: it restarted its stall clock on any acknowledgement from S rather than on one that showed
//! the ring had moved, which would have made the stall timeout unreachable against an S that sent
//! keepalives. It is three fields and no I/O; nothing about the missing S ever stood in the way.
//!
//! This file is written to be read, and it says where it is guessing.

// The daemon's own pieces.
use rayland_c::relay_engine::{BlobTable, RelayEngine, RelayLink, RingSlot};
use rayland_c::ring::{ParkDecision, RingWatcher};
// The relay protocol and its framing.
use rayland_relay::{C2S, S2C, read_msg, write_msg};
// The vtest server we present to Mesa, and the error type the engine seam speaks.
use rayland_vtest::EngineError;
use rayland_vtest::vtest::serve_vtest;

use anyhow::{Context, Result};
// `flush` on the link to S: `write_msg` hands bytes to the stream, but an unflushed request is a
// request S never sees.
use std::io::Write;
use std::net::TcpStream;
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
/// # Why this is TCP, and why that is temporary
/// (c)1 Task 6 replaces this with QUIC. TCP is the placeholder that lets Task 3 be a real program
/// rather than a sketch: `rayland-relay`'s framing works over any [`Read`]/[`Write`], so swapping
/// the transport touches [`TcpLink`] and nothing else. Ring-findings §7 is the reason the real
/// answer is not TCP — **latency, not bandwidth, is what will hurt**, and head-of-line blocking on
/// a single TCP stream is exactly the wrong property for a protocol whose replies are round trips
/// the application blocks on.
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

/// The relay link to S over a TCP stream.
///
/// A placeholder for Task 6's QUIC transport; see [`ENV_S_ADDR`] for why TCP is the wrong final
/// answer. Split into a send half and a receive half via `try_clone` so the reader thread can block
/// in `recv` without holding a lock that the watcher's `send` needs — the whole point of the
/// three-thread arrangement.
struct TcpLink {
    /// The stream. `try_clone` duplicates the descriptor, so two `TcpLink`s over one connection
    /// share the socket without sharing a lock.
    stream: TcpStream,
}

impl RelayLink for TcpLink {
    /// Frame and write one message. See [`RelayLink::send`].
    fn send(&mut self, m: &C2S) -> Result<(), EngineError> {
        write_msg(&mut self.stream, m).map_err(|e| EngineError::RelayLinkFailed {
            detail: format!("writing {m:?} to S failed: {e}"),
        })?;
        // `write_msg` hands bytes to the kernel but a buffered stream may hold them. Flushing is
        // not politeness here: an unflushed `GetCapset` is a request S never sees, and the
        // application blocks forever on a reply that was never asked for.
        self.stream
            .flush()
            .map_err(|e| EngineError::RelayLinkFailed {
                detail: format!("flushing the link to S failed: {e}"),
            })
    }

    /// Block for the next message. See [`RelayLink::recv`].
    fn recv(&mut self) -> Result<S2C, EngineError> {
        read_msg(&mut self.stream).map_err(|e| EngineError::RelayLinkFailed {
            detail: format!("reading from S failed: {e}"),
        })
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
    /// The highest ring `tail` C has relayed to S.
    relayed_tail: u32,
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
            consumed_tail: 0,
            outstanding_since: None,
        }
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
    tx: Arc<Mutex<TcpLink>>,
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
        // A closed channel means the reader thread is gone, i.e. S dropped the connection. That is
        // a link failure, not an end of stream: whoever is waiting here will never get its answer.
        self.replies
            .recv()
            .map_err(|_| EngineError::RelayLinkFailed {
                detail: "the reader thread ended before S answered this request".into(),
            })
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
/// - Returns when S closes the link or a read fails; the session is over either way.
fn reader_thread(
    mut rx: TcpLink,
    replies: Sender<S2C>,
    blobs: BlobTable,
    progress: Arc<Mutex<Progress>>,
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
            // Solicited (`Capset`, `BlobCreated`, `Error`): queue it for whoever asked. The channel
            // is unbounded and its receiver lives inside the engine for the whole session, so a
            // send failure here means only one thing: the engine has been dropped, i.e. the vtest
            // session is over. That is a reason to stop, not an error to report.
            other => {
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
    tx: Arc<Mutex<TcpLink>>,
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
            let msg = C2S::RingDelta {
                ring_res_id: identity.res_id,
                tail,
                bytes: delta.bytes,
            };
            if let Err(e) = tx
                .lock()
                .expect("the link send lock is never poisoned")
                .send(&msg)
            {
                eprintln!("rayland-c: relaying the ring to S failed: {e}");
                return;
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
    let stream = TcpStream::connect(&s_addr)
        .with_context(|| format!("connecting to S at {s_addr} (set {ENV_S_ADDR} to change it)"))?;
    // Nagle would coalesce small writes, which is exactly wrong here: a doorbell or a small ring
    // delta delayed by up to 40 ms is 40 ms the application spends blocked on a reply.
    // Ring-findings §7: latency is what will hurt, not bandwidth.
    stream
        .set_nodelay(true)
        .context("disabling Nagle on the link to S")?;
    // Two independent handles to one connection: the reader thread blocks in `recv` on one while
    // the vtest thread and the watcher send on the other. Without this split, a `recv` would hold
    // whatever lock a `send` needs.
    let rx = TcpLink {
        stream: stream
            .try_clone()
            .context("cloning the link to S for the reader thread")?,
    };
    let tx = Arc::new(Mutex::new(TcpLink { stream }));

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

    // --- The reader: the only thing that may `recv`. See the module docs for why a design without
    // it deadlocks.
    std::thread::Builder::new()
        .name("rayland-c-reader".into())
        .spawn({
            let blobs = Arc::clone(&blobs);
            let progress = Arc::clone(&progress);
            move || reader_thread(rx, reply_tx, blobs, progress)
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

    let outcome = serve_vtest(&mut mesa, &mut engine)
        .map_err(|e| anyhow::anyhow!("the vtest session failed: {e}"))?;
    eprintln!(
        "rayland-c: session ended cleanly (context {:?}, {} inline batches relayed)",
        outcome.context_id, outcome.submitted_batches
    );
    Ok(())
}

/// Unit tests for [`Progress`], the daemon's stall detector.
///
/// # Why this struct is tested when the rest of the file is not
/// The module docs are honest that this file's *wiring* — sockets, threads, the vtest session — has
/// no peer to run against until (c)1 Task 5 builds S, and so is not covered. [`Progress`] is the
/// exception and deserves to be: it is three fields and no I/O, its whole job is a decision that is
/// invisible until it is wrong, and the bug it shipped with (restarting the stall clock on any
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
