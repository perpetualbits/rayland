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
//! # Status: this runs, and what running it cost
//! **As of (c)1 Task 6 this binary completes real sessions.** `rayland-refapp` — unmodified, and
//! running against `rayland-c` a QUIC link away — renders through it and gets back a PNG
//! bit-identical to a native run (`tests/loopback_e2e.rs`, 10/10 runs).
//!
//! Task 5b had already given spec §5's channel 2, the reply arena, the owner it never had, by
//! correcting the S→C rule to spec §7.2's **S ships back exactly the bytes S wrote**.
//! [`Applier::poll_progress`] documents that rule and the two ways its predecessor was wrong. Task 6
//! found that the rule was right and its **implementation had two holes**, both invisible without a
//! live Mesa:
//!
//! - *"bytes S wrote"* was implemented as *"bytes that changed since S mapped the blob"*, and those
//!   differ by every write that happened before the mapping existed — which, for a readback buffer,
//!   is the whole frame. See [`HostBlob::map`](rayland_s::blob::HostBlob::map).
//! - Blob bytes were shipped only when a **ring retired**, but a blob can be born with its contents
//!   already in it and no ring traffic need follow. See the `CreateBlob` arm of [`Applier::apply`].
//!
//! S also now rings its own ring's doorbell after every applied delta, because Mesa's doorbell
//! decision reads a `status` word that never crosses the network — see
//! `rayland_vtest::venus_ring::doorbell` for the finding.
//!
//! [`Applier`] and the ring arithmetic under it remain tested against a real shared-memory mapping
//! with no GPU and no network (`tests/apply.rs`). Those tests are still the right shape — but note
//! that **both holes above sat underneath them**, because a memfd is zero-filled and a test never
//! renders into a blob before mapping it. The live e2e is what closed them, and is why it is now the
//! gate.
//!
//! This file is written to be read, and it says where it is guessing.

// The engine actor and its client, plus the gate that tells us whether this host has a usable GPU.
// The daemon no longer holds a `VirglEngine` directly — one actor thread owns it and everything else
// messages it through an `EngineClient` (see `spawn_engine` in main). The progress thread no longer
// drives the engine at all (the return path keys on the application's own `vkGetFenceStatus` completion,
// not an S-issued fence), so only `apply` needs the client, as `&mut dyn RenderEngine`.
use rayland_engine::{EngineClient, spawn_engine, virgl_available};
// The relay protocol and its framing.
use rayland_relay::{C2S, S2C, WaylandMessage, read_msg, write_msg};
// The message applier: everything this daemon actually knows how to do.
use rayland_s::apply::Applier;
use std::os::fd::OwnedFd;

use rayland_s::wayland_client::{EventSink, ExportedFdSource, WaylandReplay};
// Presentation: finding the application's readback buffer among S's blobs, and putting it on S's
// screen. See that module's docs for why finding it is the one guess (c)1 has to make.
use rayland_s::present::{
    ENV_NO_PRESENT, FrameCapture, LiveFrame, frame_size_from_env, present_frame_live,
};

// SP2's QUIC transport: the network C's commands cross.
use rayland_transport::{QuicRecv, QuicSend};

use anyhow::{Context, Result};
// `flush` on the link to C: `write_msg` hands bytes to the stream, but an unflushed reply is a
// reply C never sees.
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
/// **QUIC is UDP**, so this is a UDP endpoint despite the surrounding talk of connections. See
/// `rayland-c`'s matching `ENV_S_ADDR` for why the transport is QUIC and what v1 does not yet
/// collect on (everything still shares one stream, which has TCP's head-of-line behaviour).
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

/// The environment variable a Wayland client finds its compositor through.
///
/// Consulted directly, rather than just letting `rayland_present::present` fail, so that "this
/// machine has no display" and "this machine has a display and presentation broke" are two different
/// outcomes — see [`present_the_frame`].
const ENV_WAYLAND_DISPLAY: &str = "WAYLAND_DISPLAY";

/// The second half of spec §1's success criterion: put the frame on S's screen.
///
/// §1 asks for correctness to be asserted **twice, by two independent paths** — the application's
/// own readback PNG on C, and *the frame the host presents on S*. (c)1 Task 6 delivered the first
/// and only the first; this is the other one.
///
/// # The three ways this declines, and why each is a decline rather than a failure
/// 1. **[`ENV_NO_PRESENT`] is set.** Something automated is driving this daemon and cannot click a
///    close button. `tests/loopback_e2e.rs` is the only such caller today, and it says so.
/// 2. **No compositor.** `rayland-s` on a headless box is still a perfectly good relay — the
///    application on C renders correctly and gets its pixels back either way. Presentation is the
///    part that needs a screen, and a machine without one has not failed at anything. This mirrors
///    how every GPU/Wayland-dependent test in this repository skips rather than reddens.
///
/// A **failure to identify the frame is not on that list**: it is an error, and it exits non-zero.
/// The session may well have succeeded — the application's PNG on C is untouched by any of this — but
/// §1's second path did not happen, and this branch's recurring failure is things that quietly did
/// not happen. See [`FrameCapture::into_frame`](rayland_s::present::FrameCapture::into_frame).
///
/// # Inputs / outputs
/// - `capture`: what the session collected. Consumed — the decision is final.
/// - Returns when the window is closed, or immediately if presentation is declined.
///
/// # Errors
/// Returns an error if the frame could not be identified (no candidate, or an ambiguity S refuses to
/// guess through), or if presentation itself failed on a machine that does have a compositor.
fn present_the_frame(capture: FrameCapture, live: &Arc<Mutex<LiveFrame>>) -> Result<()> {
    // Always say what S would show, before any decision to decline. An automated run never reaches
    // `into_frame`, so without this the only report on presentation is a human looking at a window —
    // and a blank one looks exactly like a correct one in the log. See `FrameCapture::report`.
    capture.report();
    // **Prefer the live frame.** It is the newest frame S proved complete and shipped, whereas the
    // capture holds each candidate as it looked when its blob was *created* — a finished frame for a
    // one-frame application, an empty buffer for a multi-frame one. Falling back to the capture when
    // no run ever landed keeps single-frame applications working exactly as before, including the
    // ambiguity refusal, which the live frame has no way to express.
    let live_frame = match live.lock() {
        Ok(frame) => {
            frame.report();
            frame.frame()
        }
        Err(poisoned) => {
            let frame = poisoned.into_inner();
            frame.report();
            frame.frame()
        }
    };
    if std::env::var_os(ENV_NO_PRESENT).is_some() {
        eprintln!(
            "rayland-s: not presenting ({ENV_NO_PRESENT} is set). The relay itself is unaffected; \
             the application on C has its pixels either way."
        );
        return Ok(());
    }
    if std::env::var_os(ENV_WAYLAND_DISPLAY).is_none() {
        eprintln!(
            "rayland-s: not presenting (no {ENV_WAYLAND_DISPLAY}, so there is no compositor to \
             present to). S relayed the session correctly regardless — but note that on a machine \
             with no display, S is not the machine (c)1 §1 describes."
        );
        return Ok(());
    }
    // Refuse loudly here rather than show something wrong. `into_frame`'s two errors both explain
    // themselves at length, so there is nothing to add with a `context`.
    let frame = match live_frame {
        Some(frame) => {
            eprintln!(
                "rayland-s: presenting the newest complete frame S shipped ({}x{})",
                frame.width, frame.height
            );
            frame
        }
        None => capture.into_frame()?,
    };
    // Keep following the render for as long as the window is open. The closure holds only the shared
    // frame — the relay may well have ended by now, in which case it simply keeps returning the last
    // frame and the window behaves like the static one. `try_lock` rather than `lock`: this runs on
    // the compositor's frame callback, and a presentation path that can block on the progress
    // thread's lock could stall the relay. A missed frame is not worth that risk; the next callback
    // is 16 ms away.
    let live_for_window = Arc::clone(live);
    present_frame_live(
        frame,
        Some(Box::new(move || match live_for_window.try_lock() {
            Ok(frame) => frame.frame(),
            Err(_) => None,
        })),
    )
}

/// Frame and write one message to C, flushing it.
///
/// Flushing is not politeness: an unflushed `Capset` is an answer C never sees, and C is blocked in
/// a request/reply waiting for exactly it — so the application stalls on a reply that was computed
/// and then sat in a buffer.
fn send(stream: &mut QuicSend, msg: &S2C) -> Result<()> {
    // DIAGNOSTIC (`RAYLAND_S_REPLY_LOG`): bracket the write *and the flush* separately. `write_msg`
    // returning is not delivery — `rayland-c`'s own `record_send` counts after the write and before
    // its flush, which is why "C sent 91, S applied 90" cannot currently distinguish a message that
    // was never flushed from one that was never read. A `w>` with no matching `w<` is a send that
    // did not complete, and that is exactly the shape this is looking for.
    link_log("w>", &s2c_kind(msg));
    write_msg(stream, msg).with_context(|| format!("writing {msg:?} to C"))?;
    let flushed = stream.flush().context("flushing the link to C");
    link_log("w<", &s2c_kind(msg));
    flushed
}

/// Whether the link diagnostic is on, read from `RAYLAND_S_REPLY_LOG` — the same switch as the
/// applier's instrumentation, so one variable turns the whole investigation's logging on.
fn link_log_enabled() -> bool {
    std::env::var_os("RAYLAND_S_REPLY_LOG").is_some()
}

/// Report how long a lock-held critical section took, when it took long enough to matter.
///
/// # Why a threshold rather than every call
/// This runs on a 200 µs poll loop, so logging unconditionally would emit millions of lines and
/// change the timing it is measuring. Only sections that are slow enough to starve the message
/// thread are interesting, and 50 ms is already two orders of magnitude past what a "handful of
/// loads" (the applier's own description of `take_ring_progress`) should cost.
///
/// # Inputs / outputs
/// - `what`: the call being timed.
/// - `elapsed`: how long it held the applier lock.
/// - Prints only above the threshold. **No-op unless `RAYLAND_S_REPLY_LOG` is set.**
fn section_log(what: &str, elapsed: Duration) {
    /// Two orders of magnitude above what any of these sections is documented to cost.
    const REPORT_ABOVE: Duration = Duration::from_millis(50);
    if !link_log_enabled() || elapsed < REPORT_ABOVE {
        return;
    }
    eprintln!("[s-section] {what} held the applier lock {} ms", elapsed.as_millis());
}

/// A throttled "the progress thread is still looping" line, emitted while holding **no** lock.
///
/// # Why it must be outside the locks
/// The ring sampler in `Applier::take_ring_progress` runs with the applier lock held — necessarily,
/// since it reads the applier's blobs — so its silence is ambiguous: it means either "this thread
/// stopped" or "this thread never got the lock". This heartbeat sits before any acquisition, so it
/// separates those two, and together with the watchdog it says which thread is blocked on what.
///
/// Prints at most once per interval. **No-op unless `RAYLAND_S_REPLY_LOG` is set.**
fn progress_heartbeat() {
    if !link_log_enabled() {
        return;
    }
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    static LAST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    /// Twice a second: frequent enough to bracket a ~4 s stall, sparse enough not to flood a log
    /// that a 200 µs poll loop would otherwise fill with millions of lines.
    const INTERVAL_MS: u64 = 500;

    let now_ms = BASE
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64;
    if now_ms < LAST_MS.load(std::sync::atomic::Ordering::Relaxed) + INTERVAL_MS {
        return;
    }
    LAST_MS.store(now_ms, std::sync::atomic::Ordering::Relaxed);
    eprintln!("[s-heartbeat] progress thread looping at {now_ms}ms");
}

/// Emit one link-traffic line: a direction marker and a compact message description.
///
/// # Inputs / outputs
/// - `marker`: `r<` for a message read from C, `w>` / `w<` for the start and end of a write to C.
///   The paired write markers are what make an incomplete send visible.
/// - `what`: the message description from [`c2s_kind`] or [`s2c_kind`].
/// - Prints one line to stderr. **No-op unless `RAYLAND_S_REPLY_LOG` is set.**
fn link_log(marker: &str, what: &str) {
    if !link_log_enabled() {
        return;
    }
    eprintln!("[s-link] {marker} {what}");
}

/// Describe a `C2S` in one short line — **never** its payload.
///
/// # Why not `{:?}`
/// A `BlobData` can carry a megabyte, and a `RingDelta` a full ring's worth of commands; debug-
/// formatting either would produce a log line longer than the rest of the session's output combined
/// and bury the very sequence this is meant to reveal. Only the identifying fields are printed —
/// which for the stall means `RingDelta`'s `tail`, the key every other log in this investigation is
/// already joined on.
fn c2s_kind(m: &C2S) -> String {
    match m {
        C2S::Hello { .. } => "Hello".to_string(),
        C2S::CreateContext { .. } => "CreateContext".to_string(),
        C2S::GetCapset { .. } => "GetCapset".to_string(),
        C2S::CreateBlob { blob_id, size, .. } => format!("CreateBlob blob_id={blob_id} size={size}"),
        C2S::BlobData {
            res_id,
            offset,
            bytes,
        } => format!("BlobData res={res_id} off={offset} len={}", bytes.len()),
        C2S::RingDelta {
            ring_res_id,
            tail,
            bytes,
        } => format!("RingDelta ring={ring_res_id} tail={tail} len={}", bytes.len()),
        C2S::SubmitCmd { .. } => "SubmitCmd".to_string(),
        C2S::NotifyRing { .. } => "NotifyRing".to_string(),
        C2S::UnrefResource { res_id } => format!("UnrefResource res={res_id}"),
        C2S::WaylandRequest { .. } => "WaylandRequest".to_string(),
        C2S::WaylandBind { .. } => "WaylandBind".to_string(),
    }
}

/// Describe an `S2C` in one short line — **never** its payload. See [`c2s_kind`] for why.
fn s2c_kind(m: &S2C) -> String {
    match m {
        S2C::Capset { bytes } => format!("Capset len={}", bytes.len()),
        S2C::BlobCreated { res_id, initial } => {
            format!("BlobCreated res={res_id} runs={}", initial.len())
        }
        S2C::BlobData {
            res_id,
            offset,
            bytes,
        } => format!("BlobData res={res_id} off={offset} len={}", bytes.len()),
        S2C::RingProgress {
            ring_res_id,
            consumed_tail,
        } => format!("RingProgress ring={ring_res_id} consumed_tail={consumed_tail}"),
        S2C::Error { .. } => "Error".to_string(),
        S2C::WaylandEvent { .. } => "WaylandEvent".to_string(),
    }
}

/// Ship a batch of messages to C, stamping the T6 trace point for each `BlobData`.
///
/// Both the return path's retirement branch and its fence-feedback delivery branch send the same way,
/// so the send loop lives here rather than being written twice. `BlobData` is the only pixel-bearing
/// message, so it is the only one T6-stamped (design note §7); `RingProgress` is the head update, not
/// pixels.
///
/// # Inputs / outputs
/// - `tx`: the shared link to C. Locked per message, never held across two.
/// - `msgs`: the messages to send, in order. The caller is responsible for ordering pixels ahead of
///   anything that would release the application to read them.
/// - Returns `Err(())` if a send failed; the caller ends the session, exactly as the inline sends did.
fn ship(tx: &Arc<Mutex<QuicSend>>, msgs: &[S2C]) -> Result<(), ()> {
    if msgs.is_empty() {
        return Ok(());
    }
    // **One lock and one flush for the whole batch, not one per message.**
    //
    // This loop used to take the send lock and flush inside it, once per message. That is what made
    // the return path message-rate bound: a 120-frame `icosa-gpu` run ships **29414** `BlobData`
    // messages, so it was paying 29414 lock acquisitions and 29414 flushes — and a flush is a
    // syscall-shaped operation on the QUIC stream, not a memcpy. Measured breakdown of that run:
    // 24874 messages for the readback (`res=5`, average run 377 bytes — its gap-256 coalescing works)
    // and 4540 for the reply arena (`res=2`, average 4.4 bytes, 3247 of them a single byte).
    //
    // The arena's fine grain is **not** a defect to coalesce away: `take_venus_blob_writes` uses
    // gap 0 deliberately, because a gap byte is one S did not write, and shipping it could clobber
    // what C's Mesa has there. So the fix cannot be "send fewer bytes"; it has to be "send the same
    // bytes in fewer operations", which is exactly what batching the flush does — losslessly, with
    // no change to the wire format and no byte shipped that was not shipped before.
    //
    // Ordering is untouched. Messages are written in the order given, and the *between*-batch
    // ordering the return path depends on (readback pixels, then reply arena, then the head-advance
    // that releases the application — see `progress_thread`) is a property of separate `ship` calls,
    // each of which still flushes before it returns.
    let mut guard = tx.lock().expect("the link send lock is never poisoned");
    let stream = &mut *guard;
    for msg in msgs {
        // T6 — transfer packet emitted (design note §7): the point a pixel packet leaves S for C.
        if let S2C::BlobData { res_id, offset, bytes } = msg {
            rayland_relay::trace::emit(
                "T6",
                &format!("side=S res={res_id} off={offset} len={}", bytes.len()),
            );
        }
        link_log("w>", &s2c_kind(msg));
        if let Err(e) = write_msg(stream, msg).with_context(|| format!("writing {msg:?} to C"))
        {
            eprintln!("rayland-s: shipping to C failed: {e:#}");
            return Err(());
        }
    }
    // The flush is what actually delivers: an unflushed message is an answer C never sees, and C may
    // be blocked in a request/reply waiting for exactly it. Once for the batch, after every write.
    if let Err(e) = stream.flush().context("flushing the link to C") {
        eprintln!("rayland-s: shipping to C failed: {e:#}");
        return Err(());
    }
    link_log("w<", &format!("batch of {}", msgs.len()));
    Ok(())
}

/// The WP0 buffer-token fd source: resolves a token's resource id to a duplicate of the dma-buf S
/// exported for that resource, by asking the [`Applier`] that holds it.
///
/// # Why a newtype rather than handing the replay the `Arc<Mutex<Applier>>`
/// The rule this exists to enforce is *the applier lock is never held across a `send_request`*. A Wayland
/// request is a round trip to S's compositor; holding the relay's applier mutex across one would put the
/// entire ring session behind the compositor's scheduling — the same class of self-inflicted stall this
/// project has already shipped twice with instrumentation. Because `dup_exported_fd` takes the lock,
/// duplicates, and returns, **the guard cannot escape this function**, and no caller can violate the rule
/// even by accident.
///
/// The duplicate matters too: `Applier` must keep its own descriptor, because virglrenderer exports a
/// resource exactly once (`mem->exported`) and the export cannot be repeated.
struct ApplierFdSource {
    /// The session state holding every resource's creation-time exported descriptor.
    applier: Arc<Mutex<Applier>>,
}

impl ExportedFdSource for ApplierFdSource {
    fn note_presented(&self, resource_id: u32) {
        // Same lock discipline as `dup_exported_fd`: take it, record, release — never held across a
        // compositor round trip.
        self.applier
            .lock()
            .expect("the applier lock is never poisoned")
            .note_presented(resource_id);
    }

    fn dup_exported_fd(&self, resource_id: u32) -> Option<OwnedFd> {
        // Lock, borrow, duplicate, release — all three inside this expression, so nothing downstream can
        // hold the applier while talking to the compositor.
        let session = self.applier.lock().expect("the applier lock is never poisoned");
        let borrowed = session.exported_fd(resource_id)?;
        match borrowed.try_clone_to_owned() {
            Ok(fd) => Some(fd),
            Err(e) => {
                // A dup failure is an fd-table exhaustion, not a missing resource: say which it was, since
                // the caller's refusal message cannot distinguish them.
                eprintln!("rayland-s: WP0 4.3: dup of resource {resource_id}'s dma-buf failed: {e}");
                None
            }
        }
    }
}

/// The WP0 event-return sink: ships each translated compositor event to C as an [`S2C::WaylandEvent`].
///
/// The S-side replay ([`WaylandReplay`]) translates a compositor event into the app's id space and hands it
/// here; this puts it on the same link C's proxy reads, where the reader thread `post`s it to the proxy and
/// the app receives it. It shares the one send link with the message and progress threads (the mutex inside
/// [`ship`] serializes the three). Delivery is fire-and-forget: a failed ship is logged by `ship` and
/// dropped, because an undeliverable event must not stall the compositor-reader thread.
struct LinkEventSink {
    /// The shared link to C, locked per message by [`ship`].
    tx: Arc<Mutex<QuicSend>>,
}

impl EventSink for LinkEventSink {
    /// Ship one app-space compositor event to C. Errors are swallowed (logged inside `ship`): the event
    /// return path is best-effort and independent of the ring/blob path's own error handling.
    fn emit(&self, event: WaylandMessage) {
        let _ = ship(&self.tx, &[S2C::WaylandEvent { message: event }]);
    }
}

/// The return path: ship each finished readback frame **ahead of** the ring-progress that releases the
/// application onto it. This thread is the only thing that releases the application's synchronous calls.
///
/// # The completion barrier and the ordering (the (c)2 return-path fix, 2026-07-21)
/// With fence feedback disabled the application releases itself by polling `vkGetFenceStatus` until the
/// reply reads `VK_SUCCESS` (see [`Applier::reply_arena_fence_signaled`]). That reply is the moment the
/// application's submit — its readback copy included — is complete on S's GPU, so `res6` is a whole frame.
/// The stale-frame residual was S shipping the head-advance (and that `VK_SUCCESS` reply) **before** the
/// `res6` bytes: C released the application, which read its own local `res6` before S's `BlobData` for it
/// landed — the whole previous frame.
///
/// So on each poll, when the reply delta reports a fence signalled, S ships the readback `BlobData`
/// **first** — via [`Applier::take_app_blob_writes`], which is non-empty only for a readback-bearing
/// (draw) submit and, because `VK_SUCCESS` proves the copy done, carries complete (never torn) bytes —
/// then the reply arena, then the head-advance. C therefore always has the finished frame before it is
/// released onto it. An upload-copy submit also signals a fence but leaves `res6` unchanged, so
/// `take_app_blob_writes` is empty and nothing is shipped for it, exactly as required.
///
/// # Lock discipline, and why the reply/head shipping stays old-style
/// The applier lock is held only for the short reads (`take_ring_progress`, `take_*_blob_writes`) and
/// never across a `ship`. The reply arena and head-advance are shipped **only when the ring moved**, venus
/// before progress — the same lockstep the working gate used, which initialization depends on (a wholesale
/// rewrite of this cadence broke device init; see `docs/DIARY.md`, 2026-07-21). Nothing here enters the
/// engine, so no cycle can form between this thread, the message thread, and the actor.
fn progress_thread(
    applier: Arc<Mutex<Applier>>,
    tx: Arc<Mutex<QuicSend>>,
    live: Arc<Mutex<LiveFrame>>,
) {
    loop {
        // DIAGNOSTIC (`RAYLAND_S_REPLY_LOG`): a heartbeat taken **outside every lock**, unlike the
        // ring sampler inside `take_ring_progress` which necessarily runs with the applier held. If
        // this line stops while the watchdog still reports, this thread is blocked *acquiring* a
        // lock; if it keeps going while nothing else moves, it is looping and finding nothing.
        progress_heartbeat();
        // The head-advance that releases the application's synchronous calls, taken first. Shipped
        // old-style below (only when the ring moved, venus before progress) so init's reply/head lockstep
        // is exactly the working gate's — that lockstep is load-bearing for initialization.
        let progress = {
            let mut session = applier.lock().expect("the applier lock is never poisoned");
            let t = std::time::Instant::now();
            let p = session.take_ring_progress();
            // DIAGNOSTIC (`RAYLAND_S_REPLY_LOG`): time the critical section. The watchdog showed the
            // applier lock held essentially continuously while this thread looped only every ~3 s,
            // which is not a deadlock but a critical section long enough to starve the message
            // thread past Mesa's ~3.5 s stall abort. This says which call spends it.
            section_log("take_ring_progress", t.elapsed());
            p
        };
        if !progress.is_empty() {
            // The reply arena for the commands that just retired, plus a check of whether the arena now
            // shows a `vkGetFenceStatus` reply reading `VK_SUCCESS` — read from the **live** arena, in the
            // same lock, because `take_venus_blob_writes` fragments the reply into per-changed-byte runs
            // and the contiguous `[38][0]` is not visible in them (see `Applier::reply_arena_fence_signaled`).
            let (venus, signaled) = {
                let mut session = applier.lock().expect("the applier lock is never poisoned");
                // Timed separately: these two walk every Venus-internal blob — including the 8 MiB
                // staging pool — byte-granular at gap 0, and they do it with the lock held.
                let t = std::time::Instant::now();
                let v = session.take_venus_blob_writes();
                section_log("take_venus_blob_writes", t.elapsed());
                let t = std::time::Instant::now();
                let s = session.reply_arena_fence_signaled();
                section_log("reply_arena_fence_signaled", t.elapsed());
                (v, s)
            };
            if signaled {
                // A fence just signalled: the application's submit and its readback copy are complete on
                // S's GPU, so `res6` is a *whole* frame (the empty-submit context fence never guaranteed
                // this — it retired before the DMA). Ship the readback pixels BEFORE the reply arena (which
                // carries the `VK_SUCCESS` that ends the application's poll loop) and the head-advance, so C
                // applies the finished frame before it releases the application onto it. `take_app_blob_writes`
                // is non-empty only for a readback-bearing (draw) submit — an upload copy leaves `res6`
                // unchanged — and, because completion is proven, the bytes are complete (no tear).
                let app = {
                    let mut session = applier.lock().expect("the applier lock is never poisoned");
                    session.take_app_blob_writes()
                };
                // Tee the frame before shipping it. These are the only bytes in the whole daemon
                // that are known to be a *complete* frame — the fence proves the submit and its
                // readback copy are done — and they have already been read out of S's mapping, so
                // folding them into the live frame costs no second read of GPU-shared memory. That
                // matters: doing this read on the relay's path was measured stalling the ring for
                // 30 s. See `LiveFrame`'s docs for the two approaches that failed before this one.
                if !app.is_empty() {
                    match live.lock() {
                        Ok(mut frame) => frame.apply_runs(&app),
                        // Presentation is not worth killing a working relay for.
                        Err(poisoned) => poisoned.into_inner().apply_runs(&app),
                    }
                }
                if !app.is_empty() && ship(&tx, &app).is_err() {
                    return;
                }
            }
            // **This ship order is load-bearing — do not reorder.** `progress` (the head-advance) is what
            // releases the application on C, so it must go LAST, after the readback pixels (`app`) and the
            // reply arena (`venus`). It is the sole guarantee that C applies the finished frame before it is
            // released onto it, and the reason an early/stale completion signal cannot torn- or stale-ship
            // (see `Applier::reply_arena_fence_signaled`'s "real safety property").
            if ship(&tx, &venus).is_err() {
                return;
            }
            if ship(&tx, &progress).is_err() {
                return;
            }
        }

        std::thread::sleep(PROGRESS_POLL);
    }
}

/// Serve one session: read C's messages, apply them, and send back what S owes.
///
/// # Inputs / outputs
/// - `rx`: the reading half of the link to C. Owned exclusively — nothing else may read it.
/// - `tx`: the shared writing half, also used by the progress thread.
/// - `applier`: the session state, shared with the progress thread.
/// - `engine`: a client for the engine actor (the one thread that owns virglrenderer). `apply`
///   messages the actor through it; there is no engine lock to contend, and the progress thread holds
///   its own clone, so neither thread can block the other behind GPU work.
/// - `capture`: collects the application's readback buffer as it goes past, for presentation after
///   the session. Owned by this thread — the progress thread never touches it.
/// - Returns when C closes the link or a read fails.
fn serve(
    mut rx: QuicRecv,
    tx: Arc<Mutex<QuicSend>>,
    applier: Arc<Mutex<Applier>>,
    engine: &mut EngineClient,
    capture: &mut FrameCapture,
    wl_replay: &mut WaylandReplay,
) -> Result<()> {
    loop {
        // The framed byte count `read_msg` now returns is C's measurement seam (Task 9); S keeps its
        // own accounting out of this path, so it is discarded here rather than plumbed through.
        let msg: C2S = match read_msg(&mut rx) {
            Ok((m, _framed_bytes)) => {
                // DIAGNOSTIC (`RAYLAND_S_REPLY_LOG`): the first observable point on S. C reports
                // sending 91 ring messages while S applies 90, and until now "sent" and "applied"
                // were the only two points on that path — so a message lost in the link and a message
                // read but not applied were indistinguishable. This is the read seam.
                link_log("r<", &c2s_kind(&m));
                m
            }
            Err(e) => {
                // Not necessarily an error: a clean shutdown ends here too.
                eprintln!("rayland-s: link from C ended: {e}");
                return Ok(());
            }
        };

        // **WP0 router.** The Wayland-proxy messages are replayed against S's real compositor by
        // `wl_replay`, not applied to the vtest engine — `Applier::apply` refuses them by design (they
        // are not vtest/ring messages). Split them off here, before the apply path; everything else
        // falls through, rebound to `msg`.
        let msg = match msg {
            C2S::WaylandRequest { message } => {
                wl_replay.handle_request(message);
                continue;
            }
            C2S::WaylandBind {
                interface,
                version,
                app_object_id,
            } => {
                wl_replay.handle_bind(interface, version, app_object_id);
                continue;
            }
            other => other,
        };

        // **The applier lock is held across `apply` *and* the replies it produced.** Both halves
        // matter, for different reasons.
        //
        // Holding it across `apply` is deliberate and cheap for the message that matters: a
        // `C2S::RingDelta` is a `memcpy` and one atomic store — the GPU work happens later, on
        // virglrenderer's own ring thread, not in here. The messages that *do* enter the engine
        // (`CreateBlob`, `SubmitCmd`) are rare: ring-findings §2 measured the whole inline path at
        // 140–236 bytes across an entire Vulkan initialization.
        //
        // **Holding it across the sends is what keeps a blob's announcement ahead of its data**, and
        // (c)1 Task 6 found out the hard way what happens without it. `apply` maps a new blob and
        // makes it visible in `Applier`; the `S2C::BlobCreated` that tells C its `res_id` is only
        // sent afterwards. Release the lock in between and the progress thread — which locks the
        // same `Applier` — polls, finds the new blob, and ships an `S2C::BlobData` for a `res_id` C
        // has never been told about. C then logs "S sent BlobData for resource 5, which C has no
        // shadow of" and **drops the bytes**, which for the readback buffer means the application
        // renders correctly across the network and then reads its own zeros. That is not a
        // theoretical window: it is the readback blob's normal case, because Mesa creates that blob
        // at `vkMapMemory`, i.e. when the GPU has *already* filled it — so there is data to ship the
        // instant it is mapped, and the race is on every single run.
        //
        // No deadlock: the progress thread takes `applier` and releases it **before** taking `tx`,
        // so it never holds both, and this is the only path that holds them together.
        let mut session = applier.lock().expect("the applier lock is never poisoned");
        // No engine lock any more: `apply` drives the engine through the client, which messages the
        // actor (the one thread that owns virglrenderer). The applier lock is still held across
        // `apply` and the sends below — its BlobCreated-before-BlobData reason (this function's docs)
        // is unchanged. `apply`'s engine calls block only on the actor, which services them promptly
        // even while a readback fence is in flight, so this can no longer deadlock the doorbell.
        let out = session.apply(engine, msg);

        // **Look for the frame here, before the lock is released and before the replies go out.**
        // Spec §7.3: Mesa creates a blob resource lazily, at `vkMapMemory`, so the readback buffer's
        // blob is born *after* `vkCmdCopyImageToBuffer` has already run — with the finished frame
        // already in it. This is the moment S has the pixels, and there is no later one: the
        // application reads them and exits without touching the ring again, which is exactly why
        // Task 6's retirement-gated return path never shipped them. Presentation must not repeat
        // that mistake, so it hangs off the same event the fix does.
        //
        // Reading S's *own* mapping rather than the runs `poll_progress` ships is what makes §1's
        // two verification paths independent: the window shows what S's GPU wrote, the app's PNG on
        // C shows what the relay delivered, and a divergence between them is a finding rather than
        // two views of one diff agreeing with each other. See `Applier::blob`.
        capture.observe_replies(&session, &out);

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
        // Explicit rather than waiting for the loop's end: the next iteration blocks reading C's
        // link, and holding the applier across that would stop the progress thread dead — it is the
        // only thing that ever releases the application's synchronous Vulkan calls.
        drop(session);
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
    // One thread owns virglrenderer; `serve` and the progress thread hold clients and message it. This
    // replaces the `Arc<Mutex<VirglEngine>>` whose lock deadlocked the readback fence against the ring
    // doorbell (docs/design/2026-07-18-c2-engine-actor.md). The actor builds the engine on its own
    // thread because virglrenderer's EGL context is thread-affine (Task 3 finding), so `spawn_engine`
    // takes the render-node path, not a pre-built engine. `_engine_thread` is bound (not dropped) so
    // the actor thread lives for the whole session.
    let (engine, _engine_thread) = spawn_engine(render_node.clone()).map_err(|e| {
        anyhow::anyhow!(
            "creating the render engine on {}: {e}",
            render_node.display()
        )
    })?;

    let bind_addr = listen
        .parse()
        .with_context(|| format!("{ENV_LISTEN}={listen:?} is not a valid host:port address"))?;
    let listener = rayland_transport::listen(bind_addr).with_context(|| {
        format!("binding S's listen address {listen} (set {ENV_LISTEN} to change it)")
    })?;
    // Report the address actually bound, not the one requested: a caller may pass port 0 to let the
    // OS choose, and printing the request back would then name a port nobody can connect to.
    let bound = listener
        .local_addr()
        .context("reading S's bound listen address")?;
    eprintln!(
        "rayland-s: listening on {bound}, rendering on {}",
        render_node.display()
    );

    // One connection, then done: vtest is one context per connection, and (c)1's walking skeleton
    // serves a single application. This mirrors `rayland-c`, which likewise accepts exactly one.
    //
    // `accept_bi` rather than SP2's `accept`: that one hands back a **read-only** view plus a
    // `Liveness` whose send half is contractually silent, which suits SP0–SP3's one-directional
    // command stream and cannot serve (c)1 at all. S owes C real answers on this connection — the
    // capset, every blob's resource id, the reply-arena bytes the application is blocked on, and the
    // ring progress that is the only thing that ever releases a synchronous Vulkan call.
    //
    // QUIC needs no Nagle switch: the TCP placeholder this replaces had to disable it, because an
    // `S2C::RingProgress` coalesced by up to 40 ms is 40 ms the application on C spends blocked on a
    // reply S already has (ring-findings §7).
    let stream = listener.accept_bi().context("accepting C's connection")?;
    eprintln!("rayland-s: C connected");

    // Two halves, two threads: the message thread blocks reading one while the progress thread writes
    // on the other. Without the split, a blocking read would hold whatever lock a write needs — the
    // same deadlock the module docs describe, rebuilt one layer down.
    let (tx, rx) = stream.split();
    let tx = Arc::new(Mutex::new(tx));
    let applier = Arc::new(Mutex::new(Applier::new()));

    // What to look for. Read *before* the progress thread starts, because that thread accumulates the
    // frame and so needs the size; and before the session, so a malformed `RAYLAND_C1_PRESENT_SIZE` is
    // a startup refusal naming the setting rather than a surprise at the end of a run that has
    // already done all its work and cannot be repeated for free.
    let (present_width, present_height) = frame_size_from_env()?;
    // The newest complete frame, assembled by the progress thread from the readback runs it ships.
    // See `LiveFrame` for why presentation cannot instead just read the blob when it wants it.
    let live = Arc::new(Mutex::new(LiveFrame::new(present_width, present_height)));

    // The poller: the only thing that ever releases the application's synchronous calls. It holds its
    // own `EngineClient` clone so it can drive the readback fence through the actor.
    std::thread::Builder::new()
        .name("rayland-s-progress".into())
        .spawn({
            let applier = Arc::clone(&applier);
            let tx = Arc::clone(&tx);
            let live = Arc::clone(&live);
            move || progress_thread(applier, tx, live)
        })
        .context("spawning the progress thread")?;

    // DIAGNOSTIC (`RAYLAND_S_REPLY_LOG`): a lock watchdog. Both of S's threads were measured stopping
    // at the same instant after S read C's final ring delta, which is the signature of a deadlock —
    // but "both stopped" does not say *which* lock is held or by whom, and the two threads' own logs
    // cannot say so because a thread blocked on a lock writes nothing. This one owns no locks and
    // only ever `try_lock`s, so it keeps reporting while the others are wedged.
    if link_log_enabled() {
        let applier_probe = Arc::clone(&applier);
        let tx_probe = Arc::clone(&tx);
        std::thread::Builder::new()
            .name("rayland-s-lockdog".into())
            .spawn(move || {
                loop {
                    // `try_lock` and immediately drop: this must never itself hold either lock, or the
                    // instrument becomes a participant in the thing it is measuring.
                    let applier_free = applier_probe.try_lock().is_ok();
                    let tx_free = tx_probe.try_lock().is_ok();
                    eprintln!("[s-lockdog] applier_free={applier_free} tx_free={tx_free}");
                    std::thread::sleep(Duration::from_millis(500));
                }
            })
            .context("spawning the lock watchdog")?;
    }

    let mut capture = FrameCapture::new(present_width, present_height);

    // `serve` needs `&mut` to call the `RenderEngine` trait methods through the client; the message
    // thread keeps this original `engine`, the progress thread got a clone above.
    let mut engine = engine;
    // WP0: the S-side replay of the app's Wayland session. Unconnected until the first relayed request,
    // so an offscreen session never touches a compositor. Owned by the message thread, which is the only
    // thing that dispatches relayed Wayland requests. Its event sink ships compositor events back to C over
    // the shared link, so the app receives `xdg_surface.configure`, `wl_buffer.release`, and the rest.
    let mut wl_replay = WaylandReplay::new(
        Arc::new(LinkEventSink {
            tx: Arc::clone(&tx),
        }),
        // Task 4.3: how the replay turns a token's resource id into a real dma-buf to hand the compositor.
        Arc::new(ApplierFdSource {
            applier: Arc::clone(&applier),
        }),
    );
    // **Presentation runs alongside the session, not after it.**
    //
    // It used to run only once `serve` returned, which is correct for one still frame and useless
    // for a live one: by then the render is over and there is nothing left to follow. So the window
    // gets its own thread, started before the session, which waits for the first complete frame and
    // then follows the render until a human closes it.
    //
    // `presented` records whether that thread ever got a frame. If it did not — a session that
    // shipped no readback at all — the old post-session path still runs, so single-frame
    // applications and the ambiguity refusal behave exactly as before.
    let presented = Arc::new(AtomicBool::new(false));
    let session_over = Arc::new(AtomicBool::new(false));
    let window_thread = if std::env::var_os(ENV_NO_PRESENT).is_some()
        || std::env::var_os(ENV_WAYLAND_DISPLAY).is_none()
    {
        // Declined for the same two reasons `present_the_frame` declines, checked here as well so
        // no thread is spawned at all when presentation is off.
        None
    } else {
        let live = Arc::clone(&live);
        let presented = Arc::clone(&presented);
        let session_over = Arc::clone(&session_over);
        Some(
            std::thread::Builder::new()
                .name("rayland-s-present".into())
                .spawn(move || {
                    loop {
                        // `try_lock`: the progress thread owns this lock on the relay's path, and a
                        // window waiting to open must never be a reason the relay stalls.
                        let first = live.try_lock().ok().and_then(|frame| frame.frame());
                        if let Some(first) = first {
                            presented.store(true, Ordering::SeqCst);
                            let live_for_window = Arc::clone(&live);
                            let result = present_frame_live(
                                first,
                                Some(Box::new(move || match live_for_window.try_lock() {
                                    Ok(frame) => frame.frame(),
                                    Err(_) => None,
                                })),
                            );
                            if let Err(e) = result {
                                eprintln!("rayland-s: live presentation failed: {e:#}");
                            }
                            return;
                        }
                        // The session ended without ever shipping a complete frame; leave the
                        // post-session path to report and decide.
                        if session_over.load(Ordering::SeqCst) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                })
                .context("spawning the presentation thread")?,
        )
    };

    let applier_for_refresh = Arc::clone(&applier);
    serve(rx, tx, applier, &mut engine, &mut capture, &mut wl_replay)?;
    eprintln!("rayland-s: session ended");

    // **Refresh the frame candidates exactly once, here, and never on the relay's hot path.**
    //
    // `observe_replies` captured each candidate at `BlobCreated`, which holds the finished frame for
    // a one-frame application and an empty buffer for a multi-frame one — so a 120-frame run would
    // otherwise present black. The fix is to re-read the blob; the trap is *where*.
    //
    // Doing it per-apply was measured stalling the ring for 30 s and killing the run outright. The
    // blob is a live GPU-shared mapping, and reading a quarter of a megabyte from device-visible
    // memory is nothing like reading it from RAM — while holding the session lock the relay needs.
    // This repository has now made that same mistake six times with six different instruments (see
    // `docs/DIARY.md`); the general rule is that anything touching a blob's pages belongs off the
    // path the application's latency runs through.
    //
    // After the session there is no relay left to starve, and one read is all presentation needs. A
    // candidate whose blob the application already freed keeps its creation-time copy, which
    // `refresh_candidates` handles by design.
    match applier_for_refresh.lock() {
        Ok(session) => capture.refresh_candidates(&session),
        // A poisoned lock means a thread panicked mid-session. The relay is over and the pixels are
        // whatever they are; presenting the older copy beats aborting on the way out.
        Err(poisoned) => capture.refresh_candidates(&poisoned.into_inner()),
    }

    // Release the presentation thread if it is still waiting for a frame that will never come.
    session_over.store(true, Ordering::SeqCst);
    if let Some(window_thread) = window_thread {
        // Join rather than detach: the window must outlive the session — an application that exits
        // the instant it has its pixels would otherwise take the picture off screen with it.
        let _ = window_thread.join();
    }
    // The live window already showed (and followed) the render; there is nothing left to present.
    if presented.load(Ordering::SeqCst) {
        capture.report();
        exit_without_engine_teardown();
    }

    // Now that the session is over, put the frame on screen — and keep it there until a human closes
    // it. Presentation deliberately runs *after* the session rather than alongside it; the reasons
    // (one static frame, and a window that must outlive an application that exits the instant it has
    // its pixels) are on `rayland_s::present::present_frame`.
    present_the_frame(capture, &live)?;
    exit_without_engine_teardown();
}

/// End the process **without** running `VirglEngine`'s `Drop`, and never return.
///
/// # The bug this fixes
/// About **one session teardown in five** ended with `SIGABRT` rather than a clean exit — 83 of 400
/// runs in the overnight soak, 2 of 10 in a targeted loopback hunt. The message, which only survives
/// if the log is not overwritten by the next run:
///
/// ```text
/// epoxy_get_proc_address: Assertion `0 && "Couldn't find current GLX or EGL context."' failed.
/// ```
///
/// That is **libepoxy**, reached from `virgl_renderer_cleanup` inside `VirglEngine::drop` as it
/// releases the EGL winsys. Something in that path asks epoxy to resolve a GL entry point when no
/// context is current, and epoxy's response is to `abort()`. It is intermittent because it depends on
/// the order threads and the render-server subprocess wind down in, which is why a single clean run
/// looks like a refutation and is not.
///
/// # Why skipping the teardown is the right fix rather than a dodge
/// `VirglEngine::drop` exists so that *repeated* new→use→drop cycles are safe — the tests do exactly
/// that, and its ordering (resources, then contexts, then `virgl_renderer_cleanup`, then the global
/// lock) is load-bearing there. **None of that applies to a process that is exiting.** The kernel
/// reclaims the mappings, the descriptors and the address space, and the render-server subprocess
/// already exits when its socket closes — demonstrably so, since the ~21% of runs that *did* abort
/// skipped this same cleanup and left no render server behind.
///
/// So the `Drop` stays exactly as it is for every embedder and every test; only the daemon's own exit
/// declines to run it. That keeps the fix to the one place where cleanup buys nothing and costs a
/// one-in-five crash.
///
/// # Inputs / outputs
/// - Diverges: `std::process::exit` does not return and does not run destructors.
/// - Exits `0`. A failure before this point returns through `?` and never reaches here.
fn exit_without_engine_teardown() -> ! {
    // Flush our own buffered output first: `process::exit` runs no destructors, and Rust's stdout is
    // line-buffered to a terminal but block-buffered to a pipe — which is exactly how every script in
    // `scripts/` runs this daemon.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(0);
}
