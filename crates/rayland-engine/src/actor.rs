//! The engine actor: one thread owns `VirglEngine`; everything else reaches it by message.
//!
//! # Why this exists
//! virglrenderer is process-global and not thread-safe, so Rayland must serialise every call into it.
//! It did that with an `Arc<Mutex<VirglEngine>>`, and that **deadlocks** on the readback path: the
//! return path's GPU fence holds the lock while it waits, but the fence can only retire once the host
//! ring thread makes progress, which needs a doorbell (`submit`) from the message thread — which is
//! blocked on the very lock the fence holds. Two prototypes confirmed it (see
//! `docs/design/2026-07-18-c2-engine-actor.md` §1).
//!
//! The fix is a single owner. One thread owns the engine; the message thread and the progress thread
//! hold an [`EngineClient`] and *message* the owner. With one owner there is no lock to deadlock on,
//! and — crucially — the owner services incoming commands (the doorbell) *between* polls of an
//! in-flight fence, so the fence and the doorbell it depends on cooperate instead of competing.
//!
//! # A second constraint this module exists to satisfy: the GPU context is thread-affine
//! This was not in the original sketch and was found empirically while building this module: a
//! `VirglEngine` cannot be constructed on one OS thread and then driven from another. virglrenderer's
//! EGL/surfaceless winsys binds its rendering context to whichever thread was current when
//! `virgl_renderer_init` ran; a *different* thread calling into the engine afterwards hits
//! `vrend_winsys_make_context_current: Error switching context: EGL_BAD_ACCESS` and then a hard
//! `abort()` inside `epoxy_get_proc_address` ("Couldn't find current GLX or EGL context") the moment
//! any GL call needs to resolve a function pointer — this is not a recoverable `EngineError`, it is a
//! `SIGABRT` that takes the whole process down. It reproduced 100% of the time (not a race) once the
//! constructing thread and the using thread differed, and disappeared completely once construction,
//! every call, and `Drop` all ran on one dedicated thread with no hand-off. That is *exactly* what
//! [`spawn_engine`] does: it takes the render-node path, not an already-built `VirglEngine`, and calls
//! [`VirglEngine::new`] **on the actor thread itself** — so the thread that owns the engine for
//! message-passing purposes is the same thread that ever touches the GPU context, for the engine's
//! entire life, with zero exceptions. Do not "simplify" this by constructing the engine at the call
//! site and passing it in; that reintroduces the exact crash this design works around.

use crate::VirglEngine;
use rayland_vtest::{BlobResource, EngineError, EngineFrame, RenderEngine};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long the actor sleeps between polls of an in-flight fence. Mirrors the engine's own
/// `FENCE_POLL_INTERVAL`; small, since a doorbell arriving mid-fence is only serviced on the next
/// pass, so this bounds the doorbell's added latency.
const FENCE_POLL_INTERVAL: Duration = Duration::from_micros(200);

/// How long the actor drives a single fence before giving up, matching the engine's former blocking
/// wait. A stuck fence must not spin forever.
const FENCE_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// One request to the actor: a `RenderEngine` operation plus a channel for its typed reply.
///
/// Only the operations (c)1/(c)2 actually issue are represented; `create_resource`/`read_back` are
/// C0's offscreen path and never reach the actor (the client `unimplemented!()`s them).
enum EngineCommand {
    /// Mirrors [`RenderEngine::create_venus_context`].
    CreateVenusContext {
        ctx_id: u32,
        reply: Sender<Result<(), EngineError>>,
    },
    /// Mirrors [`RenderEngine::submit`]. `cmd` is owned (`Vec<u8>`) rather than borrowed because the
    /// command must outlive the call that sent it — it crosses a channel to a different thread.
    Submit {
        ctx_id: u32,
        cmd: Vec<u8>,
        reply: Sender<Result<(), EngineError>>,
    },
    /// Mirrors [`RenderEngine::venus_capset`].
    VenusCapset {
        version: u32,
        reply: Sender<Result<Vec<u8>, EngineError>>,
    },
    /// Mirrors [`RenderEngine::create_blob_resource`].
    CreateBlobResource {
        ctx_id: u32,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
        reply: Sender<Result<BlobResource, EngineError>>,
    },
    /// Mirrors [`RenderEngine::unref_resource`], which has no failure mode on the trait, so the
    /// reply carries `()` purely as a completion signal — the caller still needs to know the
    /// actor has processed the unref before it does anything that assumes the resource is gone.
    UnrefResource {
        resource_id: u32,
        reply: Sender<()>,
    },
    /// Mirrors [`RenderEngine::wait_for_work_retired`]. Unlike every other variant, handling this
    /// one does not send `reply` immediately — it arms an [`InFlightFence`], which stashes `reply`
    /// until the actor loop observes retirement (see `run_actor`'s doc comment).
    WaitForWorkRetired {
        ctx_id: u32,
        ring_idx: u32,
        reply: Sender<Result<(), EngineError>>,
    },
}

/// A handle to the engine actor that implements [`RenderEngine`] by messaging it. Cheap and `Clone`
/// (an `mpsc::Sender`); the message thread and the progress thread each hold one.
#[derive(Clone)]
pub struct EngineClient {
    /// The channel to the actor thread. Cloning `EngineClient` clones this `Sender`, which `mpsc`
    /// supports natively (many producers, one consumer) — that is what lets both the message
    /// thread and the progress thread message the same actor.
    tx: Sender<EngineCommand>,
}

impl EngineClient {
    /// Send one command built around a fresh reply channel and block on its answer.
    ///
    /// The actor is alive for the whole session; a send or recv error here means it has exited (the
    /// session is over), which is a panic-worthy invariant break on this synchronous path rather than
    /// something a caller can recover from.
    fn request<T>(&self, make: impl FnOnce(Sender<T>) -> EngineCommand) -> T {
        // A fresh one-shot reply channel per request: replies cannot be confused with each other or
        // with a different request's answer, since each has its own `Sender`/`Receiver` pair.
        let (reply_tx, reply_rx) = channel();
        self.tx
            .send(make(reply_tx))
            .expect("engine actor is alive for the session");
        // Block until the actor answers. For a `WaitForWorkRetired` request this blocks until the
        // fence actually retires (or times out) — the actor defers sending on `reply` until then.
        reply_rx.recv().expect("engine actor answered")
    }
}

impl RenderEngine for EngineClient {
    /// Messages the actor to create a Venus context; blocks for the reply. See
    /// [`RenderEngine::create_venus_context`] for the full contract.
    fn create_venus_context(&mut self, ctx_id: u32) -> Result<(), EngineError> {
        self.request(|reply| EngineCommand::CreateVenusContext { ctx_id, reply })
    }

    /// Messages the actor to submit a command buffer; blocks for the reply. This is the doorbell
    /// the actor's fence-driving loop must keep servicing promptly — see the module docs and
    /// `run_actor`. See [`RenderEngine::submit`] for the full contract.
    fn submit(&mut self, ctx_id: u32, cmd: &[u8]) -> Result<(), EngineError> {
        self.request(|reply| EngineCommand::Submit {
            ctx_id,
            cmd: cmd.to_vec(),
            reply,
        })
    }

    /// Messages the actor for the Venus capset; blocks for the reply. See
    /// [`RenderEngine::venus_capset`] for the full contract.
    fn venus_capset(&mut self, version: u32) -> Result<Vec<u8>, EngineError> {
        self.request(|reply| EngineCommand::VenusCapset { version, reply })
    }

    /// C0's offscreen classic-resource path never runs through (c)1/(c)2's actor — nothing calls it
    /// in that configuration — so there is no message worth defining for it. Panics if called, per
    /// the task brief.
    fn create_resource(
        &mut self,
        _ctx_id: u32,
        _width: u32,
        _height: u32,
        _format: u32,
    ) -> Result<u32, EngineError> {
        unimplemented!("create_resource is C0's offscreen path; not routed through the actor")
    }

    /// Messages the actor to create a blob resource; blocks for the reply. See
    /// [`RenderEngine::create_blob_resource`] for the full contract.
    fn create_blob_resource(
        &mut self,
        ctx_id: u32,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
    ) -> Result<BlobResource, EngineError> {
        self.request(|reply| EngineCommand::CreateBlobResource {
            ctx_id,
            blob_mem,
            blob_flags,
            blob_id,
            size,
            reply,
        })
    }

    /// Messages the actor to release a resource; blocks until the actor confirms it processed the
    /// message (there is nothing to fail — see [`EngineCommand::UnrefResource`]'s doc comment).
    fn unref_resource(&mut self, resource_id: u32) {
        self.request(|reply| EngineCommand::UnrefResource { resource_id, reply })
    }

    /// C0's offscreen readback path never runs through (c)1/(c)2's actor. Panics if called, per the
    /// task brief.
    fn read_back(&mut self, _resource_id: u32) -> Result<EngineFrame, EngineError> {
        unimplemented!("read_back is C0's offscreen path; not routed through the actor")
    }

    /// Messages the actor to wait for a context's work to retire; blocks until it does (or times
    /// out). This is the call that arms the actor's cooperative fence loop — see the module docs.
    /// See [`RenderEngine::wait_for_work_retired`] for the full contract.
    fn wait_for_work_retired(&mut self, ctx_id: u32, ring_idx: u32) -> Result<(), EngineError> {
        self.request(|reply| EngineCommand::WaitForWorkRetired {
            ctx_id,
            ring_idx,
            reply,
        })
    }
}

/// Spawn the actor thread, which constructs `VirglEngine` on `render_node` **on that thread** and
/// then owns it for the rest of its life. Returns a client to message it and the thread's handle.
/// The actor runs until every client is dropped (the channel closes), i.e. for the session's life.
///
/// # Why this takes a render-node path, not an already-built `VirglEngine`
/// An earlier version of this signature took `engine: VirglEngine`, built by the caller before
/// spawning. That crashes real hardware: see the module docs' "GPU context is thread-affine"
/// section. `VirglEngine::new` must run on the exact thread that will use the engine for the rest
/// of its life, so this function calls it internally, inside the spawned thread's closure, instead
/// of accepting a value that was necessarily constructed somewhere else.
///
/// # Inputs / outputs
/// - `render_node`: path to a DRM render node (e.g. `/dev/dri/renderD128`), forwarded verbatim to
///   [`VirglEngine::new`] on the actor thread. Owned (`PathBuf`) rather than borrowed because it
///   must outlive the call that sent it — it crosses onto a different thread.
/// - Returns `Ok((client, handle))` once `VirglEngine::new` has *actually succeeded* on the actor
///   thread — this function blocks until construction finishes, so a caller checking `virgl_available`
///   first and then calling this can still trust a returned `Err` (e.g. a race where the render node
///   disappeared between the two checks) rather than discovering construction failure only on the
///   first later command. `client` is cheap to `Clone` for every caller that needs to message the
///   engine; `handle` should be `join`ed at shutdown, after dropping every client, so the process
///   does not exit with the engine thread still mid-`Drop`.
///
/// # Failure modes
/// Whatever [`VirglEngine::new`] can fail with (`AlreadyActive`, `RenderNodeUnavailable`,
/// `InitFailed`) — reported synchronously here rather than on the first command sent to the client,
/// because there would otherwise be no [`EngineClient`] yet to report it through.
pub fn spawn_engine(render_node: PathBuf) -> Result<(EngineClient, JoinHandle<()>), EngineError> {
    // The one channel every `EngineClient` clone shares; `mpsc` supports multiple senders natively.
    let (tx, rx) = channel();
    // A second, one-shot channel purely to hand the *construction* result back to this function —
    // separate from `tx`/`rx` because construction happens before any `EngineCommand` could exist.
    let (ready_tx, ready_rx) = channel::<Result<(), EngineError>>();
    let handle = std::thread::Builder::new()
        .name("rayland-s-engine".into())
        .spawn(move || {
            // `VirglEngine::new` runs HERE, on the actor thread — never on the caller's thread — so
            // construction and every later call share one thread for the engine's whole life. See
            // the module docs' "GPU context is thread-affine" section for why this placement is not
            // optional.
            let engine = match VirglEngine::new(&render_node) {
                Ok(engine) => engine,
                Err(err) => {
                    // Construction failed: tell `spawn_engine` and exit. There is no engine to run,
                    // so this thread has nothing further to do — it never enters `run_actor`.
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            // Tell `spawn_engine` construction succeeded before doing anything else, so it can
            // return the client promptly; from here on this thread owns `engine` exclusively.
            let _ = ready_tx.send(Ok(()));
            run_actor(engine, rx);
        })
        .expect("spawning the engine actor thread");

    // Block until the actor thread reports whether construction succeeded. This makes `spawn_engine`
    // itself synchronous from the caller's point of view, matching `VirglEngine::new`'s own contract
    // (a plain `Result`, not something whose failure only surfaces later).
    match ready_rx.recv() {
        Ok(Ok(())) => Ok((EngineClient { tx }, handle)),
        Ok(Err(err)) => {
            // The actor thread already returned (it exits immediately after reporting failure);
            // join it so it is not left as an unjoined, finished thread.
            let _ = handle.join();
            Err(err)
        }
        Err(_) => {
            // The actor thread's sender was dropped without ever sending — it must have panicked
            // before reaching either `send` above. That would be a bug in this function, not a
            // normal `VirglEngine::new` failure (those are always reported via `Err(err)` above), so
            // surface it loudly rather than manufacturing a misleading `EngineError`.
            panic!(
                "engine actor thread exited without reporting its construction result \
                 (it likely panicked before calling VirglEngine::new)"
            );
        }
    }
}

/// The one in-flight fence, if any: the context/ring/id being driven, plus the reply to send once it
/// retires and the deadline past which it is a timeout.
struct InFlightFence {
    /// The context the fence was created on.
    ctx_id: u32,
    /// The ring within that context.
    ring_idx: u32,
    /// The fence's id, as returned by `VirglEngine::create_fence` — what `poll_fence` is asked about.
    fence_id: u64,
    /// The `WaitForWorkRetired` caller's reply channel; sent to only once, when the fence retires,
    /// times out, or errors.
    reply: Sender<Result<(), EngineError>>,
    /// The wall-clock instant past which an unretired fence becomes a [`EngineError::FenceTimeout`]
    /// rather than another poll — bounds how long a wedged GPU can hold this loop hostage.
    deadline: Instant,
}

/// The actor loop. Owns `engine`; services commands; drives an in-flight fence cooperatively.
///
/// With no fence in flight it blocks on the channel (no spin). With one in flight it never blocks: it
/// drains any ready commands first — this is where the doorbell the fence is waiting on gets serviced
/// — then advances the fence by exactly one `poll_fence` and sleeps a tick. That interleaving is the
/// whole point: the fence and the doorbell are driven by one thread, so neither starves the other.
fn run_actor(mut engine: VirglEngine, rx: Receiver<EngineCommand>) {
    // `Some` exactly when a `WaitForWorkRetired` request is being driven; its reply is deferred
    // until this loop observes retirement, a timeout, or a poll error.
    let mut fence: Option<InFlightFence> = None;
    loop {
        if fence.is_some() {
            // Service every ready command before polling — the doorbell must not wait behind the poll.
            loop {
                match rx.try_recv() {
                    Ok(cmd) => handle_command(&mut engine, cmd, &mut fence),
                    // No command waiting right now; stop draining and go poll the fence.
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    // Every client dropped mid-fence; nothing left to answer, so exit without
                    // finishing the fence (its reply channel is about to be dropped too, harmlessly).
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                }
            }
            // `fence` may have been cleared/replaced by a command just handled; re-check.
            if let Some(f) = fence.as_ref() {
                match engine.poll_fence(f.ctx_id, f.ring_idx, f.fence_id) {
                    Ok(true) => {
                        // Retired: take the fence out of `Option` so the outer loop sees "none in
                        // flight" next iteration, then answer the caller that has been waiting.
                        let f = fence.take().expect("just checked Some");
                        let _ = f.reply.send(Ok(()));
                    }
                    Ok(false) => {
                        if Instant::now() >= f.deadline {
                            // Wedged: stop waiting and tell the caller rather than looping forever.
                            let f = fence.take().expect("just checked Some");
                            let _ = f.reply.send(Err(EngineError::FenceTimeout {
                                ctx_id: f.ctx_id,
                                ring_idx: f.ring_idx,
                                fence_id: f.fence_id,
                            }));
                        } else {
                            // Not retired yet and not timed out: yield briefly before the next poll,
                            // rather than busy-spinning the actor thread.
                            std::thread::sleep(FENCE_POLL_INTERVAL);
                        }
                    }
                    Err(e) => {
                        // `poll_fence` is currently infallible in practice (see its doc comment) but
                        // the `Result` is honored here in case a future virglrenderer version can
                        // report a wedged/lost context.
                        let f = fence.take().expect("just checked Some");
                        let _ = f.reply.send(Err(e));
                    }
                }
            }
        } else {
            // Nothing pending: block on the channel rather than spin — the common case between
            // fence-bearing calls.
            match rx.recv() {
                Ok(cmd) => handle_command(&mut engine, cmd, &mut fence),
                // All clients dropped; the session is over. This is the actor's only exit path.
                Err(_) => return,
            }
        }
    }
}

/// Execute one command against the engine. Quick commands reply immediately; a fence request instead
/// arms `fence` (its reply is deferred to the loop, which sends it when the fence retires).
fn handle_command(engine: &mut VirglEngine, cmd: EngineCommand, fence: &mut Option<InFlightFence>) {
    match cmd {
        EngineCommand::CreateVenusContext { ctx_id, reply } => {
            // Reply may fail if the caller already gave up waiting (dropped its receiver); ignoring
            // that is correct — there is nobody left to tell.
            let _ = reply.send(engine.create_venus_context(ctx_id));
        }
        EngineCommand::Submit { ctx_id, cmd, reply } => {
            let _ = reply.send(engine.submit(ctx_id, &cmd));
        }
        EngineCommand::VenusCapset { version, reply } => {
            let _ = reply.send(engine.venus_capset(version));
        }
        EngineCommand::CreateBlobResource {
            ctx_id,
            blob_mem,
            blob_flags,
            blob_id,
            size,
            reply,
        } => {
            let _ = reply.send(engine.create_blob_resource(ctx_id, blob_mem, blob_flags, blob_id, size));
        }
        EngineCommand::UnrefResource { resource_id, reply } => {
            engine.unref_resource(resource_id);
            let _ = reply.send(());
        }
        EngineCommand::WaitForWorkRetired {
            ctx_id,
            ring_idx,
            reply,
        } => {
            // The skeleton drives one fence at a time (the progress thread awaits each before asking
            // for the next). If one is somehow already in flight, refuse the new one rather than drop
            // the old reply (which would hang its caller); this is an invariant check, not a path
            // taken in normal operation.
            if fence.is_some() {
                eprintln!(
                    "engine actor: WaitForWorkRetired(ctx_id={ctx_id}, ring_idx={ring_idx}) arrived \
                     while another fence was already in flight — refusing rather than dropping \
                     either caller's reply (this should be unreachable in the current skeleton)"
                );
                let _ = reply.send(Err(EngineError::FenceCreateFailed {
                    ctx_id,
                    ring_idx,
                    rc: -1,
                    reason: "a fence is already in flight on the engine actor".to_string(),
                }));
                return;
            }
            match engine.create_fence(ctx_id, ring_idx) {
                Ok(fence_id) => {
                    // Arm the fence; `reply` moves in and will be sent to exactly once, later, by
                    // the loop in `run_actor` once retirement/timeout/error is observed.
                    *fence = Some(InFlightFence {
                        ctx_id,
                        ring_idx,
                        fence_id,
                        reply,
                        deadline: Instant::now() + FENCE_WAIT_TIMEOUT,
                    });
                }
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `virgl.rs`'s GPU test infrastructure: the render node path and the mutex that serializes
    // GPU-touching tests within this `--lib` binary. Both modules' unit tests build into the same
    // test binary (and therefore the same OS process), and `VirglEngine::new` enforces a
    // process-global singleton (`ENGINE_ACTIVE`) — so this test must share `virgl.rs`'s lock, not
    // define its own, or it could race a `virgl.rs` GPU test and intermittently fail with
    // `EngineError::AlreadyActive` instead of exercising anything.
    use crate::virgl::tests::{GPU_TEST_LOCK, RENDER_NODE};
    use std::path::Path;

    /// Proves a command round-trips through the actor end to end: spawn it on a real render node,
    /// then — through the `EngineClient` only, never touching `VirglEngine` directly — issue
    /// `create_venus_context` and `venus_capset` and assert both succeed. That exercises the whole
    /// path this task exists to build: the message thread's send, the actor thread receiving and
    /// executing it against the real engine, and the reply crossing back.
    ///
    /// Does not exercise the fence-driving loop (`WaitForWorkRetired`'s interleaving with the
    /// doorbell) — that needs a live Venus command stream to generate real ring work, which is
    /// beyond this skeleton's smoke test; it is (c)2's next task's integration concern.
    #[test]
    fn commands_round_trip_through_the_actor() {
        let _serialize = GPU_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let node = Path::new(RENDER_NODE);
        if !crate::virgl_available(node) {
            eprintln!(
                "SKIP commands_round_trip_through_the_actor: no usable Venus render node at {RENDER_NODE}"
            );
            return;
        }

        let (mut client, handle) = spawn_engine(node.to_path_buf())
            .expect("spawn_engine's internal VirglEngine::new should succeed on a GPU host");

        // Round-trips a unit-payload reply through the actor.
        client
            .create_venus_context(1)
            .expect("create_venus_context should succeed through the actor on a GPU host");
        // Round-trips a `Vec<u8>`-payload reply through the actor, and proves the actor is still
        // servicing requests after the first one rather than having wedged.
        client
            .venus_capset(0)
            .expect("venus_capset should succeed through the actor on a GPU host");

        // Dropping every `EngineClient` closes the channel, which is the actor's only exit signal
        // (see `run_actor`'s `Err(_) => return` arm on the no-fence-in-flight branch). Joining
        // proves the thread actually exits rather than leaking.
        drop(client);
        handle
            .join()
            .expect("engine actor thread should exit cleanly once its client is dropped");
        eprintln!("OK: create_venus_context and venus_capset round-tripped through the engine actor");
    }
}
