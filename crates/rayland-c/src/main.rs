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
//! [`shm`] and [`relay_engine`]). The *wiring* in this file is not covered by any test and must be
//! treated as unverified until Task 5 gives it a peer. It is written to be read, and it says where
//! it is guessing.

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

    /// Record S's acknowledgement, clearing the stall clock if it has caught up.
    fn note_consumed(&mut self, tail: u32) {
        self.consumed_tail = tail;
        if self.relayed_tail == self.consumed_tail {
            // Fully acknowledged: nothing is outstanding, so nothing can be stalled.
            self.outstanding_since = None;
        } else {
            // Still behind, but it moved — S is slow, not stopped. Restart the clock so progress,
            // however gradual, is never mistaken for a stall.
            self.outstanding_since = Some(Instant::now());
        }
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
                progress
                    .lock()
                    .expect("the progress lock is never poisoned")
                    .note_consumed(consumed_tail);
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

    loop {
        // --- 1. Drain. The lock is held only long enough to copy the bytes out, never across the
        // network send below: the reader thread needs this table to deliver replies.
        let delta = {
            let mut table = blobs.lock().expect("the blob table lock is never poisoned");
            let Some(blob) = table.get_mut(&identity.res_id) else {
                // The ring was unref'd: the session is over.
                eprintln!("rayland-c: the command ring is gone; watcher stopping");
                return;
            };
            watcher.take_delta(blob.bytes())
        };

        // --- 2. Relay, with no lock held. These bytes are the application's Vulkan commands.
        if let Some(delta) = delta {
            let tail = delta.tail;
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
            progress
                .lock()
                .expect("the progress lock is never poisoned")
                .note_relayed(tail);
            // New work was just produced, so do not even consider parking: go straight back and
            // look again. An application mid-frame produces continuously.
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
                // S reporting a tail we never sent.
                watcher.advance_head(blob.bytes_mut(), consumed);
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
            ParkDecision::Park => std::thread::sleep(PARK_SLEEP),
            // Mesa wrote while we were announcing. IDLE has been retracted; go straight back.
            ParkDecision::StayAwake => continue,
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
