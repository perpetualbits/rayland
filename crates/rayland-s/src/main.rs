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
// messages it through an `EngineClient` (see `spawn_engine` in main).
// `RenderEngine` is the trait `EngineClient` implements; it must be in scope for the progress thread
// to call `wait_for_work_retired` and for `apply` to receive the client as `&mut dyn RenderEngine`.
use rayland_engine::{EngineClient, RenderEngine, spawn_engine, virgl_available};
// The relay protocol and its framing.
use rayland_relay::{C2S, S2C, read_msg, write_msg};
// The message applier: everything this daemon actually knows how to do.
use rayland_s::apply::Applier;
// Presentation: finding the application's readback buffer among S's blobs, and putting it on S's
// screen. See that module's docs for why finding it is the one guess (c)1 has to make.
use rayland_s::present::{ENV_NO_PRESENT, FrameCapture, frame_size_from_env, present_frame};

// SP2's QUIC transport: the network C's commands cross.
use rayland_transport::{QuicRecv, QuicSend};

use anyhow::{Context, Result};
// `flush` on the link to C: `write_msg` hands bytes to the stream, but an unflushed reply is a
// reply C never sees.
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// How long the progress thread will wait for the application's per-queue `ring_idx` to become
/// available (its `vkGetDeviceQueue2` decoded *and* its queue registered on the host) before giving
/// up on a pending readback and **ending the session loudly**.
///
/// On (c)1's pinned single-queue path this never elapses: the queue is registered during device
/// init, milliseconds in, so `retirement_ring_idx` returns `Some` before the very first readback is
/// pending. The deadline exists only so an unsupported case — an application whose queue is never
/// decoded (e.g. it obtained the queue via `vkGetDeviceQueue` v1, or on a ring this skeleton cannot
/// follow) — surfaces as a clear diagnostic instead of the application hanging forever in
/// `vkWaitForFences` with nothing pointing at the cause. Five seconds is far above the millisecond
/// registration latency yet well under Mesa's own multi-second stall aborts.
const QUEUE_REGISTER_DEADLINE: Duration = Duration::from_secs(5);

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
fn present_the_frame(capture: FrameCapture) -> Result<()> {
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
    let frame = capture.into_frame()?;
    present_frame(frame)
}

/// Frame and write one message to C, flushing it.
///
/// Flushing is not politeness: an unflushed `Capset` is an answer C never sees, and C is blocked in
/// a request/reply waiting for exactly it — so the application stalls on a reply that was computed
/// and then sat in a buffer.
fn send(stream: &mut QuicSend, msg: &S2C) -> Result<()> {
    write_msg(stream, msg).with_context(|| format!("writing {msg:?} to C"))?;
    stream.flush().context("flushing the link to C")
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
    for msg in msgs {
        // T6 — transfer packet emitted (design note §7): the point a pixel packet leaves S for C.
        if let S2C::BlobData { res_id, offset, bytes } = msg {
            rayland_relay::trace::emit(
                "T6",
                &format!("side=S res={res_id} off={offset} len={}", bytes.len()),
            );
        }
        let mut stream = tx.lock().expect("the link send lock is never poisoned");
        if let Err(e) = send(&mut stream, msg) {
            eprintln!("rayland-s: shipping to C failed: {e:#}");
            return Err(());
        }
    }
    Ok(())
}

/// The return path: deliver what S's GPU wrote, releasing the application only once the GPU has
/// actually retired the work — driven through the engine actor so the fence never starves the doorbell.
///
/// Each frame: the message thread applies the app's commands and the ring retires, which this thread
/// sees as a non-empty `take_ring_progress`. It ships the **reply arena** (Venus-internal writes) and
/// the `RingProgress` immediately — those carry the app's non-readback synchronous replies and its
/// ring-space news. It does **not** ship the readback or the feedback word yet: the GPU has not
/// finished them. It marks the frame's readback in flight. On a later poll (the app now blocked in
/// `vkWaitForFences`), it waits for the GPU work to retire — via `wait_for_work_retired` **through the
/// `EngineClient`**, which the actor drives cooperatively, so this wait no longer deadlocks the
/// doorbell — and only then ships the application blobs (readback largest-first, feedback word last),
/// whose feedback word is what releases the application onto the finished pixels.
///
/// # Lock discipline (the no-deadlock argument, docs §4)
/// This thread holds the applier lock only for the short reads (`take_ring_progress`,
/// `take_*_blob_writes`) and **never across the fence wait**. The actor never takes the applier lock.
/// So no cycle can form between this thread, the message thread, and the actor.
fn progress_thread(applier: Arc<Mutex<Applier>>, mut engine: EngineClient, tx: Arc<Mutex<QuicSend>>) {
    // Whether a submitted frame's readback still needs delivering.
    let mut delivery_pending = false;
    // When the current pending delivery began. Bounds the wait into a loud session end if it can never
    // complete (see `QUEUE_REGISTER_DEADLINE`); on the healthy path each delivery finishes within a few
    // polls, so this barely arms.
    let mut pending_since: Option<Instant> = None;
    // The ring position of the `vkQueueSubmit` whose readback S last delivered. A submit at a *larger*
    // position is a new frame; that, plus a drained ring (proving it is dispatched), is the fence
    // trigger. `None` before the first delivery.
    let mut last_delivered_submit: Option<u32> = None;
    loop {
        // One short lock: advance the ring frontier, note the context, read the fence's `ring_idx`,
        // whether the ring is drained, and the latest submit's position — all read together so they are
        // mutually consistent. `latest_queue_submit_start` scans the ring buffer, but only meaningfully
        // once a delivery is pending (the app blocked, message thread idle), so it never contends the
        // lock during active rendering.
        let (progress, ctx_id, ring_idx, drained, latest_submit) = {
            let mut session = applier.lock().expect("the applier lock is never poisoned");
            (
                session.take_ring_progress(),
                session.ctx_id(),
                session.retirement_ring_idx(),
                session.queue_ring_drained(),
                session.latest_submit_pos(),
            )
        };

        if !progress.is_empty() {
            // A frame's commands retired on the ring. Ship the reply arena, then the progress that
            // advances C's head — NOT the readback/feedback (the GPU has not finished them). This is
            // what releases the application's non-readback synchronous calls.
            let venus = {
                let mut session = applier.lock().expect("the applier lock is never poisoned");
                session.take_venus_blob_writes()
            };
            if ship(&tx, &venus).is_err() { return; }
            if ship(&tx, &progress).is_err() { return; }
            if !delivery_pending {
                // A new delivery begins; start the completion-wait clock.
                delivery_pending = true;
                pending_since = Some(Instant::now());
            }
        }

        // Deliver the readback once **a new `vkQueueSubmit` has crossed the ring and been dispatched**.
        // `latest_submit > last_delivered_submit` means this frame's submit is present (not a stale
        // earlier one, and not a between-deltas transient drain before the submit delta arrived); the
        // drained ring (`head == applied_tail`) proves the host ring thread has dispatched it, so the
        // fence issued now lands strictly after the application's own submit and cannot overtake it. This
        // is a structural trigger — no timing settle, and content-independent, so it is immune to
        // identical frames and to the races of sampling a buffer a cross-process GPU is writing (both of
        // which defeated earlier triggers; see docs/design/2026-07-19-c2-ringidx-decode.md §8).
        let new_submit_dispatched =
            drained && latest_submit.is_some() && latest_submit > last_delivered_submit;
        if delivery_pending && new_submit_dispatched {
            if let (Some(ctx), Some(ring_idx)) = (ctx_id, ring_idx) {
                // Wait for the GPU work to retire (through the actor — no deadlock), holding NO applier
                // lock across the wait.
                if let Err(e) = engine.wait_for_work_retired(ctx, ring_idx) {
                    // Cannot confirm the GPU finished: shipping now would hand the app stale/torn
                    // pixels. End the session rather than release it onto them.
                    eprintln!(
                        "rayland-s: readback fence failed ({e}); ending the session rather than \
                         releasing the application onto unfinished pixels."
                    );
                    return;
                }
                // Retired: the readback is complete. Ship the application blobs, readback largest-first
                // so the feedback word (which releases the app) lands after the pixels it reports on.
                let app = {
                    let mut session = applier.lock().expect("the applier lock is never poisoned");
                    session.take_app_blob_writes()
                };
                if ship(&tx, &app).is_err() { return; }
                delivery_pending = false;
                pending_since = None;
                // Remember which submit we just delivered, so only a *newer* one triggers the next fence.
                last_delivered_submit = latest_submit;
            }
            // else: a new submit is dispatched but the queue is not fencing-ready — handled just below.
        }

        // No registered queue to fence on (`retirement_ring_idx` is `None`) means either the queue is
        // not yet registered — during init, where a retirement carries no readback — or it has already
        // been destroyed at teardown. Either way there is no readback we could deliver, so drop the
        // pending delivery rather than let it spin to the deadline and print a misleading
        // "unsupported configuration" line on an otherwise clean shutdown. This never drops a
        // deliverable readback: a real frame's readback only becomes pending once its queue is
        // registered (its commands retire far past the early `vkGetDeviceQueue2`).
        if delivery_pending && ring_idx.is_none() {
            delivery_pending = false;
            pending_since = None;
        }

        if delivery_pending && pending_since.is_some_and(|t| t.elapsed() > QUEUE_REGISTER_DEADLINE) {
            // Stuck past the deadline *with a registered queue* (the `ring_idx.is_none()` drop above did
            // not fire): a frame's commands retired, but no `vkQueueSubmit` newer than the last
            // delivered was ever dispatched — the host ring thread wedged, or an unsupported app never
            // issued one. Leaving `delivery_pending` set would spin forever while the application hangs
            // in `vkWaitForFences`; end the session loudly instead.
            eprintln!(
                "rayland-s: a pending readback could not be completed within {QUEUE_REGISTER_DEADLINE:?} \
                 (no new queue submit was dispatched on the registered queue); ending the session. \
                 (This walking skeleton supports the single-queue Venus configuration only.)"
            );
            return;
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
) -> Result<()> {
    loop {
        // The framed byte count `read_msg` now returns is C's measurement seam (Task 9); S keeps its
        // own accounting out of this path, so it is discarded here rather than plumbed through.
        let msg: C2S = match read_msg(&mut rx) {
            Ok((m, _framed_bytes)) => m,
            Err(e) => {
                // Not necessarily an error: a clean shutdown ends here too.
                eprintln!("rayland-s: link from C ended: {e}");
                return Ok(());
            }
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

    // The poller: the only thing that ever releases the application's synchronous calls. It holds its
    // own `EngineClient` clone so it can drive the readback fence through the actor.
    std::thread::Builder::new()
        .name("rayland-s-progress".into())
        .spawn({
            let applier = Arc::clone(&applier);
            let engine = engine.clone();
            let tx = Arc::clone(&tx);
            move || progress_thread(applier, engine, tx)
        })
        .context("spawning the progress thread")?;

    // What to look for. Read before the session rather than after it, so a malformed
    // `RAYLAND_C1_PRESENT_SIZE` is a startup refusal naming the setting — not a surprise at the end
    // of a run that has already done all its work and cannot be repeated for free.
    let (present_width, present_height) = frame_size_from_env()?;
    let mut capture = FrameCapture::new(present_width, present_height);

    // `serve` needs `&mut` to call the `RenderEngine` trait methods through the client; the message
    // thread keeps this original `engine`, the progress thread got a clone above.
    let mut engine = engine;
    serve(rx, tx, applier, &mut engine, &mut capture)?;
    eprintln!("rayland-s: session ended");

    // Now that the session is over, put the frame on screen — and keep it there until a human closes
    // it. Presentation deliberately runs *after* the session rather than alongside it; the reasons
    // (one static frame, and a window that must outlive an application that exits the instant it has
    // its pixels) are on `rayland_s::present::present_frame`.
    present_the_frame(capture)
}
