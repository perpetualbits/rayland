//! The **`rayland-s` daemon**: accept C's relayed Venus command stream, replay it on a real GPU,
//! and report what the engine actually did.
//!
//! # What runs here, and why it takes two threads
//! The shape is forced by the domain, not chosen for elegance — and it is the mirror image of
//! `rayland-c`'s.
//!
//! - **The message thread** (this one, after setup) blocks reading C's link and hands each message
//!   to [`Applier::apply`]. That covers everything C *says*.
//! - **The progress thread** polls each ring's `head` and reports movement. That covers what S's
//!   engine *does*, which C has no other way to learn.
//!
//! # Why the progress thread must exist (and why a simpler design deadlocks)
//! The tempting design is one thread: read a message, apply it, write the replies. **It deadlocks**,
//! and this is worth spelling out because the deadlock is silent.
//!
//! Writing a ring delta into S's ring memory does not execute it. virglrenderer's ring *thread*
//! notices the new `tail` some time later, dispatches the commands, and stores `head`
//! (`vkr_ring.c:262-266`). There is no callback and no completion event — ring-findings §5.2's
//! result is that in the steady state Venus's design emits **zero notifications in either
//! direction**; both ends poll shared memory. So at the moment `apply` returns, there is no progress
//! to report that would be true.
//!
//! Now consider a synchronous Vulkan call. The application on C blocks in `vn_ring_wait_seqno`,
//! spinning on its local `head` (`vn_ring.c:181-198`). C's `head` advances *only* from an
//! `S2C::RingProgress`. If S produced those only in reply to inbound messages, then an application
//! blocked on a reply — and therefore sending nothing — would wait forever for the reply it is
//! blocked on, while S sat idle holding the answer. The poll loop is what breaks that.
//!
//! # Status: never run against a real C
//! **This binary has never completed a session**, because there is nothing to run it against yet:
//! the QUIC transport is (c)1 Task 6. Task 5 shipped the blob synchronisation and Task 5b corrected
//! its S→C half to spec §7.2's rule — **S ships back exactly the bytes S wrote** — which also gave
//! spec §5's channel 2, the reply arena, the owner it had never had. [`Applier::poll_progress`]
//! documents both the rule and the two ways its predecessor was wrong, including why the obvious
//! widening (ship Venus's `blob_id == 0` shmems too) would have wiped C's staging pool rather than
//! fixed anything. The piece with the real logic — [`Applier`], and
//! the ring arithmetic under it — is tested against a real shared-memory mapping with no GPU and no
//! network (`tests/apply.rs`). What is uncovered here is the *wiring*: the socket, the two threads,
//! and the engine's own behaviour. That genuinely needs a peer, and is unverified until Task 6.
//!
//! This file is written to be read, and it says where it is guessing.

// The real GPU, and the gate that tells us whether this host has one.
use rayland_engine::{VirglEngine, virgl_available};
// The relay protocol and its framing.
use rayland_relay::{C2S, S2C, read_msg, write_msg};
// The message applier: everything this daemon actually knows how to do.
use rayland_s::apply::Applier;

use anyhow::{Context, Result};
// `flush` on the link to C: `write_msg` hands bytes to the stream, but an unflushed reply is a
// reply C never sees.
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Environment variable naming the address S listens on, as `host:port`.
const ENV_LISTEN: &str = "RAYLAND_C1_S_LISTEN";

/// Default listen address.
///
/// The port matches `rayland-c`'s `DEFAULT_S_ADDR` (`127.0.0.1:9401`); the bind address is
/// `0.0.0.0` because S is, by construction, the machine on the *other* end of a network — (c)1
/// Task 8's two-machine bring-up connects to it from a different host.
///
/// # Why this is TCP, and why that is temporary
/// (c)1 Task 6 replaces this with QUIC, exactly as it does for `rayland-c`'s matching placeholder.
/// TCP is what lets this task be a real program rather than a sketch: `rayland-relay`'s framing
/// works over any `Read`/`Write`. Ring-findings §7 is the reason the real answer is not TCP —
/// head-of-line blocking on a single stream is precisely the wrong property for a protocol whose
/// replies are round trips the application blocks on.
const DEFAULT_LISTEN: &str = "0.0.0.0:9401";

/// Environment variable naming the DRM render node to open.
const ENV_RENDER_NODE: &str = "RAYLAND_C1_RENDER_NODE";

/// Default render node — the one C0 ran its whole proof against.
const DEFAULT_RENDER_NODE: &str = "/dev/dri/renderD128";

/// How often the progress thread reads each ring's `head`.
///
/// # Why this number is a latency/CPU trade with no clean answer
/// It is pure added latency on **every synchronous Vulkan call**: the application on C is spinning
/// on `head`, and `head` cannot cross the network faster than this loop notices it moved. That
/// argues for small. Against it, this is a busy loop on S's CPU that finds nothing the overwhelming
/// majority of the time.
///
/// 200 µs is chosen to be small against the thing it is added to. Ring-findings §7 is emphatic that
/// **latency, not bandwidth, is what will hurt Rayland**, and that the replies are round trips the
/// application blocks on — but a round trip over any real network is measured in milliseconds, so a
/// 200 µs poll adds a small fraction to it while costing S (the *strong* machine) a negligible slice
/// of one core. On a loopback link, where the RTT is microseconds, this becomes the dominant term —
/// a real caveat for Task 6's loopback e2e, stated here rather than discovered there.
///
/// **[INFERENCE]** — never measured. virglrenderer's own ring thread faces the identical trade and
/// answers it with an adaptive scheme (`thrd_yield()` for 16 iterations, then an exponentially
/// growing sleep from 10 µs — `vkr_ring_relax`, `vkr_ring.c:190-210`). Copying that shape here is
/// the obvious improvement and has not been done, because a fixed interval is the honest starting
/// point for something with no measurements behind it.
const PROGRESS_POLL: Duration = Duration::from_micros(200);

/// Read an environment variable, falling back to a default.
fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Frame and write one message to C, flushing it.
///
/// Flushing is not politeness: an unflushed `Capset` is an answer C never sees, and C is blocked in
/// a request/reply waiting for exactly it — so the application stalls on a reply that was computed
/// and then sat in a buffer.
fn send(stream: &mut TcpStream, msg: &S2C) -> Result<()> {
    write_msg(stream, msg).with_context(|| format!("writing {msg:?} to C"))?;
    stream.flush().context("flushing the link to C")
}

/// The progress thread: notice what S's engine retired, and tell C.
///
/// **This is the only thing that ever releases the application's synchronous Vulkan calls.** See the
/// module docs for why it cannot be folded into the message loop, and [`Applier::poll_progress`] for
/// why it reports only genuine movement rather than a reassuring keepalive.
///
/// # Inputs / outputs
/// - `applier`: shared with the message thread. The lock is held only for the poll itself.
/// - `tx`: the link to C.
/// - Returns when the link fails; the session is over either way.
fn progress_thread(applier: Arc<Mutex<Applier>>, tx: Arc<Mutex<TcpStream>>) {
    loop {
        // Poll with the lock held, send without it. The poll is a handful of atomic loads, so the
        // message thread is never blocked behind anything slow — and crucially never behind a
        // network write, which is the mistake that would make this thread the bottleneck it exists
        // to remove.
        let progress = applier
            .lock()
            .expect("the applier lock is never poisoned")
            .poll_progress();

        for msg in &progress {
            let mut stream = tx.lock().expect("the link send lock is never poisoned");
            if let Err(e) = send(&mut stream, msg) {
                eprintln!("rayland-s: reporting ring progress to C failed: {e:#}");
                return;
            }
        }

        // Wait before looking again. Sleeping unconditionally — rather than only when nothing moved
        // — keeps this loop simple and its cost bounded; see `PROGRESS_POLL` for the trade and the
        // honest note that it has never been measured.
        std::thread::sleep(PROGRESS_POLL);
    }
}

/// Serve one session: read C's messages, apply them, and send back what S owes.
///
/// # Inputs / outputs
/// - `rx`: the reading half of the link to C. Owned exclusively — nothing else may read it.
/// - `tx`: the shared writing half, also used by the progress thread.
/// - `applier`: the session state, shared with the progress thread.
/// - `engine`: S's real GPU. Owned by this thread rather than shared, so the progress thread can
///   never be blocked behind GPU work.
/// - Returns when C closes the link or a read fails.
fn serve(
    mut rx: TcpStream,
    tx: Arc<Mutex<TcpStream>>,
    applier: Arc<Mutex<Applier>>,
    engine: &mut VirglEngine,
) -> Result<()> {
    loop {
        let msg: C2S = match read_msg(&mut rx) {
            Ok(m) => m,
            Err(e) => {
                // Not necessarily an error: a clean shutdown ends here too.
                eprintln!("rayland-s: link from C ended: {e}");
                return Ok(());
            }
        };

        // The lock is held across `apply`, which is deliberate and cheap for the message that
        // matters: a `C2S::RingDelta` is a `memcpy` and one atomic store — the GPU work happens
        // later, on virglrenderer's own ring thread, not in here. The messages that *do* enter the
        // engine (`CreateBlob`, `SubmitCmd`) are rare: ring-findings §2 measured the whole inline
        // path at 140–236 bytes across an entire Vulkan initialization.
        let out = applier
            .lock()
            .expect("the applier lock is never poisoned")
            .apply(engine, msg);

        for reply in &out {
            // Worth a human's attention either way, and S's log is the more reliable of the two
            // places it appears: an unsolicited refusal reaches C's reader, which logs it and
            // deliberately does **not** route it to anyone waiting (see `S2C::Error`), so nothing on
            // C fails loudly because of it. `solicited` is ignored here because S logs its own
            // refusals regardless of who was listening.
            if let S2C::Error { message, .. } = reply {
                eprintln!("rayland-s: refusing a message from C: {message}");
            }
            let mut stream = tx.lock().expect("the link send lock is never poisoned");
            send(&mut stream, reply).context("answering C")?;
        }
    }
}

/// Bring the daemon up: open the GPU, listen for C, and run the two threads.
///
/// # Failure modes
/// Returns an error if this host has no usable Venus render node, if the engine cannot be created,
/// if the listen address cannot be bound, or if the session fails. The no-GPU case is refused **at
/// startup, by name**, rather than at the first blob: S with no GPU is not a degraded S, it is not
/// an S at all, and finding out three messages into a session would surface as an inexplicable
/// engine error on the machine that is not the problem.
fn main() -> Result<()> {
    let listen = env_or(ENV_LISTEN, DEFAULT_LISTEN);
    let render_node = PathBuf::from(env_or(ENV_RENDER_NODE, DEFAULT_RENDER_NODE));

    // Check before creating anything. `virgl_available` opens the node and asks virglrenderer
    // whether Venus (capset 4) is supported at all — the same gate C0's GPU tests use.
    anyhow::ensure!(
        virgl_available(&render_node),
        "no usable Venus render node at {} (set {ENV_RENDER_NODE} to change it). S is the machine \
         with the GPU; without one there is nothing for it to be.",
        render_node.display()
    );
    let mut engine = VirglEngine::new(&render_node).map_err(|e| {
        anyhow::anyhow!(
            "creating the render engine on {}: {e}",
            render_node.display()
        )
    })?;

    let listener = TcpListener::bind(&listen).with_context(|| {
        format!("binding S's listen address {listen} (set {ENV_LISTEN} to change it)")
    })?;
    eprintln!(
        "rayland-s: listening on {listen}, rendering on {}",
        render_node.display()
    );

    // One connection, then done: vtest is one context per connection, and (c)1's walking skeleton
    // serves a single application. This mirrors `rayland-c`, which likewise accepts exactly one.
    let (stream, peer) = listener.accept().context("accepting C's connection")?;
    eprintln!("rayland-s: C connected from {peer}");
    // Nagle would coalesce small writes, which is exactly wrong here: an `S2C::RingProgress` delayed
    // by up to 40 ms is 40 ms the application on C spends blocked on a reply S already has.
    // Ring-findings §7: latency is what will hurt, not bandwidth.
    stream
        .set_nodelay(true)
        .context("disabling Nagle on the link to C")?;

    // Two independent handles to one connection: the message thread blocks reading one while the
    // progress thread writes on the other. Without the split, a blocking read would hold whatever
    // lock a write needs — the same deadlock the module docs describe, rebuilt one layer down.
    let rx = stream
        .try_clone()
        .context("cloning the link to C for the reader")?;
    let tx = Arc::new(Mutex::new(stream));
    let applier = Arc::new(Mutex::new(Applier::new()));

    // The poller: the only thing that ever releases the application's synchronous calls.
    std::thread::Builder::new()
        .name("rayland-s-progress".into())
        .spawn({
            let applier = Arc::clone(&applier);
            let tx = Arc::clone(&tx);
            move || progress_thread(applier, tx)
        })
        .context("spawning the progress thread")?;

    serve(rx, tx, applier, &mut engine)?;
    eprintln!("rayland-s: session ended");
    Ok(())
}
