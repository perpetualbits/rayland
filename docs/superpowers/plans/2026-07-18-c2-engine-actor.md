# (c)2 engine-actor walking skeleton — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make S's readback correct *and* live — reach 120/120 on the icosa fixture across three consecutive runs with no deadlock — by moving `VirglEngine` behind a single owning **actor thread** so the readback fence and the ring doorbell cooperate instead of fighting one lock.

**Architecture:** One thread owns `VirglEngine` (no engine `Mutex`). Every engine call becomes a message. Because the `RenderEngine` trait is already the single seam, an `EngineClient` implements that trait by messaging the actor, so `apply()` and its callers are unchanged. The actor drives an in-flight fence cooperatively — servicing incoming commands (the doorbell) between `context_poll`s — which is what breaks the deadlock that sank both prior attempts.

**Tech Stack:** Rust 2024 (`rust-version = "1.85"`), `std::sync::mpsc` (no async — the engine path is synchronous), `rayland-engine` (owns `VirglEngine` + the FFI), `rayland-s` (holds an `EngineClient`). GPU e2e on `dop561`.

**The spec is [`docs/design/2026-07-18-c2-engine-actor.md`](../../design/2026-07-18-c2-engine-actor.md). Read it first** — it explains *why* the actor is the fix (the fence-vs-doorbell deadlock, proven by two prototypes) and what is deferred. Background: [`docs/design/2026-07-18-c2-readback-reachability.md`](../../design/2026-07-18-c2-readback-reachability.md) and [`docs/design/2026-07-17-fence-feedback-walking-skeleton.md`](../../design/2026-07-17-fence-feedback-walking-skeleton.md) §9–§11.

## Global Constraints

Every task's requirements implicitly include this section.

- Rust edition 2024, `rust-version = "1.85"` — **no let-chains** (nested `if`s).
- Comment discipline (`CLAUDE.md`): doc-comment block on every function/type/module; intent comments on non-trivial lines (the *why*); **code and comments must always agree**; if a change makes a design-doc statement false, fix it in the same change. **No Claude/AI attribution** anywhere, including commit messages.
- Use `CARGO_TARGET_DIR=/tmp/rayland-task9-target` on every cargo command (warm shared build).
- **Never judge the race from one run.** The `icosa` gate needs **three consecutive** passing runs; the failure modes to distinguish are: 120/120 (pass), frames-differ (wrong pixels), `SIGABRT` (ring stall), `vkWaitForFences` timeout, and hang.
- Work in the worktree `/tmp/rayland-c1-wt` on branch `c1-the-network`. Do not push or open a PR.
- The GPU e2e only does real work on a Venus host (`dop561`); it `SKIP`s elsewhere.

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/rayland-engine/src/virgl.rs` | `VirglEngine`. | Add `create_fence` / `poll_fence` (a non-blocking split of `wait_for_context_fence`) so the actor can drive a fence cooperatively. |
| `crates/rayland-engine/src/actor.rs` | **New.** The engine actor, its command set, and the `EngineClient` (`impl RenderEngine`). | Create. |
| `crates/rayland-engine/src/lib.rs` | Crate root. | `pub mod actor;` + re-export `EngineClient`, `spawn_engine`. |
| `crates/rayland-s/src/apply.rs` | `Applier`. | Add `take_venus_blob_writes` / `take_app_blob_writes` (split of `take_blob_writes` by classification). |
| `crates/rayland-s/src/main.rs` | S's daemon: `main`, `serve`, `progress_thread`. | `main` spawns the actor instead of `Arc<Mutex<VirglEngine>>`; `serve` and `progress_thread` take an `EngineClient`; `progress_thread` rewritten to the fence-gated delivery. |
| `crates/rayland-s/tests/apply.rs` | `Applier` tests (no GPU). | `poll_progress` helper recomposed from the two split methods. |

The `RenderEngine` trait and the `apply()` signature are **unchanged**.

---

### Task 1: `Applier` blob-write split

The delivery needs to ship Venus-internal writes (the reply arena) at ring retirement and application writes (the readback + feedback word) after the fence — separately. Split the existing `take_blob_writes` diff by classification. Pure data, no GPU — unit-tested against the `RecordingEngine` mock.

**Files:**
- Modify: `crates/rayland-s/src/apply.rs`
- Test: `crates/rayland-s/tests/apply.rs`

**Interfaces:**
- Consumes: `Applier`'s `blobs`, `rings`, `venus_internal` fields; `HostBlob::take_bytes_s_wrote`, `HostBlob::size`.
- Produces: `pub fn take_venus_blob_writes(&mut self) -> Vec<S2C>`, `pub fn take_app_blob_writes(&mut self) -> Vec<S2C>`. The existing `take_blob_writes` stays (or is expressed via a shared helper); the union of the two new methods' output must equal what `take_blob_writes` produces, minus ordering.

- [ ] **Step 1: Write the failing test** in `crates/rayland-s/tests/apply.rs`. It reuses `session_with_ring()` and creates two app blobs (the 64-byte vertex buffer, `blob_id: 16, size: 64`, and a venus-internal one, `blob_id: 0, size: 256`), writes S-observable bytes into each via the `RecordingEngine` double (use the existing `write_blob`/`write_blob_range` helper the file already has — read the file to find its exact name and signature), then asserts that `take_venus_blob_writes()` yields `BlobData` only for the venus-internal blob and `take_app_blob_writes()` only for the app blob, and that `take_app_blob_writes()` orders the larger blob first.

```rust
/// The blob-write split feeds the fence-gated return path: reply-arena (Venus-internal) ships at
/// ring retirement, the readback + feedback word (application blobs, largest first so the big
/// readback leads the tiny feedback word) ship after the GPU fence. This pins that partition and
/// that ordering.
#[test]
fn blob_write_split_partitions_venus_from_app_and_orders_app_largest_first() {
    let (mut applier, mut engine, _ring) = session_with_ring();

    // A Venus-internal blob (blob_id == 0) and two application blobs (blob_id != 0) of different
    // sizes, so the largest-first ordering is observable.
    let venus = create_blob(&mut applier, &mut engine, /*blob_id*/ 0, /*size*/ 256);
    let app_small = create_blob(&mut applier, &mut engine, /*blob_id*/ 16, /*size*/ 64);
    let app_big = create_blob(&mut applier, &mut engine, /*blob_id*/ 16, /*size*/ 1024);

    // Make S "write" a byte into each so each produces a run. (Use the file's existing engine-double
    // write helper; write at least one non-baseline byte per blob.)
    write_blob_byte(&engine, venus, 0, 0xAA);
    write_blob_byte(&engine, app_small, 0, 0xBB);
    write_blob_byte(&engine, app_big, 0, 0xCC);

    let venus_out = applier.take_venus_blob_writes();
    let venus_ids: Vec<u32> = venus_out.iter().filter_map(res_id_of_blobdata).collect();
    assert_eq!(venus_ids, vec![venus], "only the Venus-internal blob ships in the venus split");

    let app_out = applier.take_app_blob_writes();
    let app_ids: Vec<u32> = app_out.iter().filter_map(res_id_of_blobdata).collect();
    assert_eq!(app_ids, vec![app_big, app_small], "app split ships app blobs, largest first");
}
```

The `RecordingEngine` double in `crates/rayland-s/tests/apply.rs` already has `write_blob(res_id, fill)` (whole-blob) and `write_blob_range(res_id, offset, fill, len)` — use one of those for `write_blob_byte` (e.g. `engine.write_blob_range(res, 0, 0xBB, 1)`), do not invent a new engine-double API. Add only the small missing wrappers (`create_blob` wrapping a `C2S::CreateBlob` apply and returning the `res_id`; `res_id_of_blobdata` matching `S2C::BlobData { res_id, .. }`). **Read the file first** to match the exact signatures.

- [ ] **Step 2: Run it, verify it fails**
Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-s --test apply blob_write_split -- --nocapture`
Expected: FAIL — `no method named take_venus_blob_writes`.

- [ ] **Step 3: Implement the split** in `crates/rayland-s/src/apply.rs`. Refactor the per-blob diff out of `take_blob_writes` into a private helper, then express all three methods over it:

```rust
    /// Emit one `S2C::BlobData` per run of bytes S wrote, for the blobs named by `res_ids`, in the
    /// order given. Shared by the return path's three entry points; the only difference between them
    /// is *which* blobs they visit and *in what order*. Rings are never passed in — a ring's pages are
    /// C's command bytes and S's `head`, not S's writes to return (see `take_blob_writes`' history).
    fn emit_blob_writes(&mut self, res_ids: &[u32]) -> Vec<S2C> {
        let mut out = Vec::new();
        for &res_id in res_ids {
            // A blob may have been unref'd between listing and here; skip rather than panic on a poll
            // loop. Rings are excluded by construction (callers never pass them).
            let Some(blob) = self.blobs.get_mut(&res_id) else { continue };
            for run in blob.take_bytes_s_wrote() {
                out.push(S2C::BlobData { res_id, offset: run.offset, bytes: run.bytes });
            }
        }
        out
    }

    /// **Return path, retirement half:** the Venus-internal blob writes — the reply arena, whose bytes
    /// answer the application's non-readback synchronous calls and are needed for its forward progress.
    /// Shipped at ring retirement; the readback/feedback are not (the GPU has not finished them).
    pub fn take_venus_blob_writes(&mut self) -> Vec<S2C> {
        let ids: Vec<u32> = self
            .blobs
            .keys()
            .copied()
            .filter(|id| !self.rings.contains_key(id) && self.venus_internal.contains(id))
            .collect();
        self.emit_blob_writes(&ids)
    }

    /// **Return path, post-fence half:** the application's own blob writes — the readback buffer and
    /// the feedback word — **largest blob first**, so the megabyte-scale readback ships ahead of the
    /// tiny feedback word, and the feedback word (which releases the application) lands last, after the
    /// pixels it releases the application onto. Shipped only after the GPU fence has retired the work
    /// (see `rayland-s`'s `progress_thread`).
    pub fn take_app_blob_writes(&mut self) -> Vec<S2C> {
        let mut ids: Vec<(u32, u64)> = self
            .blobs
            .iter()
            .filter(|(id, _)| !self.rings.contains_key(id) && !self.venus_internal.contains(id))
            .map(|(&id, blob)| (id, blob.size()))
            .collect();
        // Largest first; ties broken by id purely for a reproducible order.
        ids.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let ids: Vec<u32> = ids.into_iter().map(|(id, _)| id).collect();
        self.emit_blob_writes(&ids)
    }
```
Keep the existing `take_blob_writes` working (re-express its body over `emit_blob_writes` with its existing app-then-venus ordering, or leave it untouched if it does not share the helper — but do not duplicate the run-emitting loop). If `HostBlob::size` returns `u64`, the code above is correct as written.

- [ ] **Step 4: Run it, verify it passes**
Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-s --test apply -- --nocapture`
Expected: the new test passes and all pre-existing `apply` tests still pass.

- [ ] **Step 5: Clippy + commit**
Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo clippy -p rayland-s --all-targets` (clean).
```bash
git add crates/rayland-s/src/apply.rs crates/rayland-s/tests/apply.rs
git commit -m "(c)2: split Applier blob writes into venus (retirement) and app (post-fence, largest-first)"
```

---

### Task 2: `VirglEngine` fence split (`create_fence` / `poll_fence`)

The actor must drive a fence *cooperatively* — it cannot call the blocking `wait_for_context_fence` (that would block the actor and re-create the very starvation we are removing). Split it into a non-blocking pair.

**Files:**
- Modify: `crates/rayland-engine/src/virgl.rs`
- Test: `crates/rayland-engine/src/virgl.rs` (the file's existing GPU-gated `#[cfg(test)]` block — this is the crate that tests against the real GPU)

**Interfaces:**
- Consumes: the internals of the existing `wait_for_context_fence` (`next_fence_id`, `ffi::virgl_renderer_context_create_fence`, `ffi::virgl_renderer_context_poll`, `self.cookie.fence_state.is_retired`).
- Produces: `pub fn create_fence(&mut self, ctx_id: u32, ring_idx: u32) -> Result<u64, EngineError>` (creates a fresh per-context fence, returns its id); `pub fn poll_fence(&mut self, ctx_id: u32, ring_idx: u32, fence_id: u64) -> Result<bool, EngineError>` (does **one** `context_poll`, returns whether that fence has retired). No sleeping, no loop.

- [ ] **Step 1: Add the split.** Read the current `wait_for_context_fence` (around `virgl.rs:1089`) and factor it:

```rust
    /// Create a fresh per-context fence on `(ctx_id, ring_idx)` and return its id. Non-blocking: it
    /// only submits the fence; retirement is observed later with [`Self::poll_fence`]. This is the
    /// half of [`Self::wait_for_context_fence`] the engine actor needs so it can drive completion
    /// cooperatively (polling between servicing other commands) instead of blocking a whole thread.
    ///
    /// # Failure modes
    /// [`EngineError::FenceCreateFailed`] if virglrenderer refuses the fence.
    pub fn create_fence(&mut self, ctx_id: u32, ring_idx: u32) -> Result<u64, EngineError> {
        let fence_id = self.next_fence_id;
        self.next_fence_id = self.next_fence_id.wrapping_add(1);
        // flags = 0: never mergeable, so this specific fence reliably invokes `write_context_fence`.
        // SAFETY: `ctx_id` names a live context the caller has already used.
        let rc = unsafe { ffi::virgl_renderer_context_create_fence(ctx_id, 0, ring_idx, fence_id) };
        if rc != 0 {
            return Err(EngineError::FenceCreateFailed { ctx_id, ring_idx, rc, reason: errno_name(rc) });
        }
        Ok(fence_id)
    }

    /// Pump fence completion once and report whether `fence_id` on `(ctx_id, ring_idx)` has retired.
    /// Non-blocking: exactly one `virgl_renderer_context_poll`, no sleep. The caller loops this at its
    /// own cadence, doing other work between calls.
    pub fn poll_fence(&mut self, ctx_id: u32, ring_idx: u32, fence_id: u64) -> Result<bool, EngineError> {
        // SAFETY: `ctx_id` names a live context; forcing retirement is always safe to call.
        unsafe { ffi::virgl_renderer_context_poll(ctx_id) };
        Ok(self.cookie.fence_state.is_retired(ctx_id, ring_idx, fence_id))
    }
```
Then rewrite `wait_for_context_fence` (still used by `read_back`) in terms of them, preserving its `FENCE_WAIT_TIMEOUT`/`FENCE_POLL_INTERVAL` behaviour:
```rust
    fn wait_for_context_fence(&mut self, ctx_id: u32, ring_idx: u32) -> Result<(), EngineError> {
        let fence_id = self.create_fence(ctx_id, ring_idx)?;
        let deadline = Instant::now() + FENCE_WAIT_TIMEOUT;
        loop {
            if self.poll_fence(ctx_id, ring_idx, fence_id)? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(EngineError::FenceTimeout { ctx_id, ring_idx, fence_id });
            }
            std::thread::sleep(FENCE_POLL_INTERVAL);
        }
    }
```
(If the current `wait_for_context_fence` signature or error variants differ, preserve them; only its body changes. Confirm `FenceCreateFailed`/`FenceTimeout` field names against the current code.)

- [ ] **Step 2: Add a GPU-gated test** to `virgl.rs`'s existing test module (model it on the existing `wait_for_context_fence`/`read_back` GPU tests — same `virgl_available()` skip guard): create a context, `create_fence`, then loop `poll_fence` with a short sleep until it returns `true` or a deadline, asserting it retires. This mirrors what the existing fence test already proves, via the split API.

- [ ] **Step 3: Build + the split test**
Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-engine --lib -- --nocapture` (on `dop561`: the fence test runs and passes; elsewhere it skips).
Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo clippy -p rayland-engine --all-targets` (clean).

- [ ] **Step 4: Commit**
```bash
git add crates/rayland-engine/src/virgl.rs
git commit -m "(c)2: split VirglEngine fence into non-blocking create_fence/poll_fence for the actor"
```

---

### Task 3: The engine actor + `EngineClient`

The core. A thread owns `VirglEngine`; `EngineClient` implements `RenderEngine` by messaging it; the actor drives an in-flight fence cooperatively.

**Files:**
- Create: `crates/rayland-engine/src/actor.rs`
- Modify: `crates/rayland-engine/src/lib.rs` (add `pub mod actor;` and re-export `EngineClient`, `spawn_engine`)
- Test: `crates/rayland-engine/src/actor.rs` (a GPU-gated smoke test)

**Interfaces:**
- Consumes: `VirglEngine` (Task 2's `create_fence`/`poll_fence`, plus its existing `RenderEngine` methods); `rayland_vtest::{RenderEngine, EngineError, BlobResource, EngineFrame}`.
- Produces: `pub fn spawn_engine(engine: VirglEngine) -> (EngineClient, std::thread::JoinHandle<()>)`; `pub struct EngineClient` (`Clone`, `impl RenderEngine`).

- [ ] **Step 1: Write `actor.rs`** — complete:

```rust
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

use crate::VirglEngine;
use rayland_vtest::{BlobResource, EngineError, EngineFrame, RenderEngine};
use std::sync::mpsc::{channel, Receiver, Sender};
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
    CreateVenusContext { ctx_id: u32, reply: Sender<Result<(), EngineError>> },
    Submit { ctx_id: u32, cmd: Vec<u8>, reply: Sender<Result<(), EngineError>> },
    VenusCapset { version: u32, reply: Sender<Result<Vec<u8>, EngineError>> },
    CreateBlobResource {
        ctx_id: u32, blob_mem: u32, blob_flags: u32, blob_id: u64, size: u64,
        reply: Sender<Result<BlobResource, EngineError>>,
    },
    UnrefResource { resource_id: u32, reply: Sender<()> },
    WaitForWorkRetired { ctx_id: u32, ring_idx: u32, reply: Sender<Result<(), EngineError>> },
}

/// A handle to the engine actor that implements [`RenderEngine`] by messaging it. Cheap and `Clone`
/// (an `mpsc::Sender`); the message thread and the progress thread each hold one.
#[derive(Clone)]
pub struct EngineClient {
    tx: Sender<EngineCommand>,
}

impl EngineClient {
    /// Send one command built around a fresh reply channel and block on its answer.
    ///
    /// The actor is alive for the whole session; a send or recv error here means it has exited (the
    /// session is over), which is a panic-worthy invariant break on this synchronous path rather than
    /// something a caller can recover from.
    fn request<T>(&self, make: impl FnOnce(Sender<T>) -> EngineCommand) -> T {
        let (reply_tx, reply_rx) = channel();
        self.tx.send(make(reply_tx)).expect("engine actor is alive for the session");
        reply_rx.recv().expect("engine actor answered")
    }
}

impl RenderEngine for EngineClient {
    fn create_venus_context(&mut self, ctx_id: u32) -> Result<(), EngineError> {
        self.request(|reply| EngineCommand::CreateVenusContext { ctx_id, reply })
    }
    fn submit(&mut self, ctx_id: u32, cmd: &[u8]) -> Result<(), EngineError> {
        self.request(|reply| EngineCommand::Submit { ctx_id, cmd: cmd.to_vec(), reply })
    }
    fn venus_capset(&mut self, version: u32) -> Result<Vec<u8>, EngineError> {
        self.request(|reply| EngineCommand::VenusCapset { version, reply })
    }
    fn create_resource(&mut self, _c: u32, _w: u32, _h: u32, _f: u32) -> Result<u32, EngineError> {
        unimplemented!("create_resource is C0's offscreen path; not routed through the actor")
    }
    fn create_blob_resource(
        &mut self, ctx_id: u32, blob_mem: u32, blob_flags: u32, blob_id: u64, size: u64,
    ) -> Result<BlobResource, EngineError> {
        self.request(|reply| EngineCommand::CreateBlobResource {
            ctx_id, blob_mem, blob_flags, blob_id, size, reply,
        })
    }
    fn unref_resource(&mut self, resource_id: u32) {
        self.request(|reply| EngineCommand::UnrefResource { resource_id, reply })
    }
    fn read_back(&mut self, _resource_id: u32) -> Result<EngineFrame, EngineError> {
        unimplemented!("read_back is C0's offscreen path; not routed through the actor")
    }
    fn wait_for_work_retired(&mut self, ctx_id: u32, ring_idx: u32) -> Result<(), EngineError> {
        self.request(|reply| EngineCommand::WaitForWorkRetired { ctx_id, ring_idx, reply })
    }
}

/// Spawn the actor thread owning `engine`. Returns a client to message it and the thread's handle.
/// The actor runs until every client is dropped (the channel closes), i.e. for the session's life.
pub fn spawn_engine(engine: VirglEngine) -> (EngineClient, JoinHandle<()>) {
    let (tx, rx) = channel();
    let handle = std::thread::Builder::new()
        .name("rayland-s-engine".into())
        .spawn(move || run_actor(engine, rx))
        .expect("spawning the engine actor thread");
    (EngineClient { tx }, handle)
}

/// The one in-flight fence, if any: the context/ring/id being driven, plus the reply to send once it
/// retires and the deadline past which it is a timeout.
struct InFlightFence {
    ctx_id: u32,
    ring_idx: u32,
    fence_id: u64,
    reply: Sender<Result<(), EngineError>>,
    deadline: Instant,
}

/// The actor loop. Owns `engine`; services commands; drives an in-flight fence cooperatively.
///
/// With no fence in flight it blocks on the channel (no spin). With one in flight it never blocks: it
/// drains any ready commands first — this is where the doorbell the fence is waiting on gets serviced
/// — then advances the fence by exactly one `poll_fence` and sleeps a tick. That interleaving is the
/// whole point: the fence and the doorbell are driven by one thread, so neither starves the other.
fn run_actor(mut engine: VirglEngine, rx: Receiver<EngineCommand>) {
    let mut fence: Option<InFlightFence> = None;
    loop {
        if fence.is_some() {
            // Service every ready command before polling — the doorbell must not wait behind the poll.
            loop {
                match rx.try_recv() {
                    Ok(cmd) => handle_command(&mut engine, cmd, &mut fence),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                }
            }
            // `fence` may have been cleared/replaced by a command just handled; re-check.
            if let Some(f) = fence.as_ref() {
                match engine.poll_fence(f.ctx_id, f.ring_idx, f.fence_id) {
                    Ok(true) => {
                        let f = fence.take().expect("just checked Some");
                        let _ = f.reply.send(Ok(()));
                    }
                    Ok(false) => {
                        if Instant::now() >= f.deadline {
                            let f = fence.take().expect("just checked Some");
                            let _ = f.reply.send(Err(EngineError::FenceTimeout {
                                ctx_id: f.ctx_id, ring_idx: f.ring_idx, fence_id: f.fence_id,
                            }));
                        } else {
                            std::thread::sleep(FENCE_POLL_INTERVAL);
                        }
                    }
                    Err(e) => {
                        let f = fence.take().expect("just checked Some");
                        let _ = f.reply.send(Err(e));
                    }
                }
            }
        } else {
            match rx.recv() {
                Ok(cmd) => handle_command(&mut engine, cmd, &mut fence),
                Err(_) => return, // all clients dropped; the session is over
            }
        }
    }
}

/// Execute one command against the engine. Quick commands reply immediately; a fence request instead
/// arms `fence` (its reply is deferred to the loop, which sends it when the fence retires).
fn handle_command(engine: &mut VirglEngine, cmd: EngineCommand, fence: &mut Option<InFlightFence>) {
    match cmd {
        EngineCommand::CreateVenusContext { ctx_id, reply } => {
            let _ = reply.send(engine.create_venus_context(ctx_id));
        }
        EngineCommand::Submit { ctx_id, cmd, reply } => {
            let _ = reply.send(engine.submit(ctx_id, &cmd));
        }
        EngineCommand::VenusCapset { version, reply } => {
            let _ = reply.send(engine.venus_capset(version));
        }
        EngineCommand::CreateBlobResource { ctx_id, blob_mem, blob_flags, blob_id, size, reply } => {
            let _ = reply.send(engine.create_blob_resource(ctx_id, blob_mem, blob_flags, blob_id, size));
        }
        EngineCommand::UnrefResource { resource_id, reply } => {
            engine.unref_resource(resource_id);
            let _ = reply.send(());
        }
        EngineCommand::WaitForWorkRetired { ctx_id, ring_idx, reply } => {
            // The skeleton drives one fence at a time (the progress thread awaits each before asking
            // for the next). If one is somehow already in flight, refuse the new one rather than drop
            // the old reply (which would hang its caller); this is an invariant check, not a path
            // taken in normal operation.
            if fence.is_some() {
                let _ = reply.send(Err(EngineError::FenceCreateFailed {
                    ctx_id, ring_idx, rc: -1, reason: "a fence is already in flight".into(),
                }));
                return;
            }
            match engine.create_fence(ctx_id, ring_idx) {
                Ok(fence_id) => {
                    *fence = Some(InFlightFence {
                        ctx_id, ring_idx, fence_id, reply,
                        deadline: Instant::now() + FENCE_WAIT_TIMEOUT,
                    });
                }
                Err(e) => { let _ = reply.send(Err(e)); }
            }
        }
    }
}
```
Confirm `RenderEngine`'s exact method signatures (arg names/types) and `EngineError::{FenceCreateFailed, FenceTimeout}` field names **and types** against the current source, and adjust the transcription to match — in particular `FenceCreateFailed`'s `reason` field: if it is a `String`, `"...".into()` is correct; if it is `&'static str`, drop the `.into()`. (The synthetic "already in flight" error reuses `FenceCreateFailed` only for an invariant that does not occur in normal operation; if reusing it is awkward against the real field types, a plain `eprintln!` + `let _ = reply.send(...)` of any existing `EngineError` variant is fine — it is unreachable in the skeleton.) `RenderEngine::create_venus_context`/`venus_capset` are assumed present (they are the methods `apply()` calls); if a method's real name differs, use the real one.

- [ ] **Step 2: Wire the module** in `crates/rayland-engine/src/lib.rs`: `pub mod actor;` and `pub use actor::{spawn_engine, EngineClient};` (place the re-export beside the existing `pub use virgl::{...}`).

- [ ] **Step 3: GPU-gated smoke test** in `actor.rs` (`#[cfg(test)]`, guarded by `virgl_available(RENDER_NODE)` the same way `virgl.rs`'s tests are — reuse that helper/const): `spawn_engine(VirglEngine::new(node)?)`, then through the returned client call `create_venus_context(1)` and `venus_capset(0)` and assert both return `Ok`, proving a command round-trips through the actor. Drop the client and join the handle at the end.

- [ ] **Step 4: Build + test + clippy**
Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-engine --lib -- --nocapture` (smoke test runs on `dop561`, skips elsewhere).
Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo clippy -p rayland-engine --all-targets` (clean).

- [ ] **Step 5: Commit**
```bash
git add crates/rayland-engine/src/actor.rs crates/rayland-engine/src/lib.rs
git commit -m "(c)2: the engine actor and EngineClient — one thread owns virglrenderer, others message it"
```

---

### Task 4: Wire the actor into `serve`, rewrite `progress_thread`, and pass the gate

Replace the engine `Mutex` with the actor, and restructure the return path to the fence-gated delivery. This is where the 120/120-×3 gate is met.

**Files:**
- Modify: `crates/rayland-s/src/main.rs`
- (`crates/rayland-s/Cargo.toml` already depends on `rayland-engine`; no new dep.)

**Interfaces:**
- Consumes: `rayland_engine::{spawn_engine, EngineClient, VirglEngine}`; `Applier::take_venus_blob_writes`/`take_app_blob_writes` (Task 1); `EngineClient::wait_for_work_retired` via the `RenderEngine` trait (Task 3); the existing `ship`/`send` helpers, `PROGRESS_POLL`.
- Produces: a `progress_thread(applier: Arc<Mutex<Applier>>, engine: EngineClient, tx: Arc<Mutex<QuicSend>>)`.

- [ ] **Step 1: `main` spawns the actor.** In `main` (around `virgl.rs` construction at `main.rs:528`), replace `let engine = Arc::new(Mutex::new(VirglEngine::new(...)?));` with:
```rust
    // One thread owns virglrenderer; `serve` and the progress thread hold clients and message it. This
    // replaces the `Arc<Mutex<VirglEngine>>` whose lock deadlocked the readback fence against the ring
    // doorbell (docs/design/2026-07-18-c2-engine-actor.md).
    let virgl = VirglEngine::new(&render_node).map_err(|e| { /* keep the existing error context */ })?;
    let (engine, _engine_thread) = rayland_engine::spawn_engine(virgl);
```
Pass `engine.clone()` where the code previously passed `&engine`/`Arc::clone(&engine)`. (Preserve the existing `.map_err(...)` context block verbatim inside the `VirglEngine::new` call.)

- [ ] **Step 2: `serve` takes an `EngineClient`.** Change its signature from `engine: &Arc<Mutex<VirglEngine>>` to `engine: &mut EngineClient` (or take it by value — it is `Clone`; `serve` needs `&mut` to call trait methods). In the message loop, replace
```rust
    let out = {
        let mut engine = engine.lock().expect("...");
        session.apply(&mut *engine, msg)
    };
```
with
```rust
    // No engine lock any more: `apply` drives the engine through the client, which messages the actor.
    // The applier lock is still held across `apply` and the sends below — its BlobCreated-before-
    // BlobData reason (§ this function's docs) is unchanged. `apply`'s engine calls block only on the
    // actor, which services them promptly even while a fence is in flight.
    let out = session.apply(engine, msg);
```
Update `serve`'s doc comment where it talks about "the engine lock" to describe the client/actor instead (code and comments must agree). Update the two `use` lines / imports (`VirglEngine` may become unused in `main.rs` except for `spawn_engine`'s argument — keep the import if still referenced, else drop it; let clippy decide).

- [ ] **Step 3: Rewrite `progress_thread`.** Replace the entire current `progress_thread` (the `cad5600` fingerprint-delivery version, its doc block through its closing brace) and delete the now-unused `ProbeBaseline`/`probe_a_resample`/fingerprint machinery it referenced. New version:

```rust
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
    // Whether a submitted frame's readback still needs delivering once its GPU work retires.
    let mut delivery_pending = false;
    loop {
        let (progress, ctx_id) = {
            let mut session = applier.lock().expect("the applier lock is never poisoned");
            (session.take_ring_progress(), session.ctx_id())
        };

        if !progress.is_empty() {
            // A frame's commands retired on the ring. Ship the reply arena, then the progress that
            // advances C's head — NOT the readback/feedback (the GPU has not finished them).
            let venus = {
                let mut session = applier.lock().expect("the applier lock is never poisoned");
                session.take_venus_blob_writes()
            };
            if ship(&tx, &venus).is_err() { return; }
            if ship(&tx, &progress).is_err() { return; }
            delivery_pending = true;
        } else if delivery_pending {
            // The application is now blocked awaiting its readback. Wait for the GPU work to retire
            // (through the actor — no deadlock), holding NO applier lock across the wait.
            if let Some(ctx) = ctx_id {
                if let Err(e) = engine.wait_for_work_retired(ctx, 0) {
                    // Cannot confirm the GPU finished: shipping now would hand the app stale/torn
                    // pixels. End the session rather than release it onto them.
                    eprintln!(
                        "rayland-s: readback fence failed ({e}); ending the session rather than \
                         releasing the application onto unfinished pixels."
                    );
                    return;
                }
            }
            // Retired: the readback is correct. Ship the application blobs, readback largest-first so
            // the feedback word (which releases the app) lands after the pixels.
            let app = {
                let mut session = applier.lock().expect("the applier lock is never poisoned");
                session.take_app_blob_writes()
            };
            if ship(&tx, &app).is_err() { return; }
            delivery_pending = false;
        }

        std::thread::sleep(PROGRESS_POLL);
    }
}
```

- [ ] **Step 4: Fix the spawn site.** Where `progress_thread` is spawned (around `main.rs:577`), pass an `EngineClient` clone:
```rust
    std::thread::Builder::new()
        .name("rayland-s-progress".into())
        .spawn({
            let applier = Arc::clone(&applier);
            let engine = engine.clone();
            let tx = Arc::clone(&tx);
            move || progress_thread(applier, engine, tx)
        })
        .context("spawning the progress thread")?;
```
Ensure `serve` is also passed its `EngineClient` (a clone) at its call site.

- [ ] **Step 5: Build + clippy + no-GPU tests**
Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo clippy -p rayland-s -p rayland-engine --all-targets` (clean; no unused `Mutex`/`VirglEngine` import warnings — remove what clippy flags).
Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-relay -p rayland-s -p rayland-c --lib --tests` — unit + no-GPU-linkage tests pass (the `apply` tests use the mock directly, unaffected by the actor).

- [ ] **Step 6: The gate — three runs.** Run **three times**:
```
CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-s --test loopback_e2e icosa_cpu_renders -- --nocapture
```
Expected each time: `test result: ok. 1 passed` (0 of 120 frames differ), and the run **completes** (no `SIGABRT`, no `vkWaitForFences` timeout, no >200 s hang). **All three must pass.** If any run:
- **wedges** (SIGABRT/timeout/hang) → a command path is missed or misordered; do not tune — capture which, and diagnose with `RAYLAND_C1_TRACE=1` + `scripts/c1-trace-analyze.py`, then report (this is the risk §7.1 names).
- **differs** (wrong pixels, completes) → the fence retires but is the wrong fence for the readback (spec risk §7.3); report the counts — that reopens *which* fence, a distinct question from a wedge.
Take at least three runs before believing a pass.

- [ ] **Step 7: Confirm the refapp still passes**
Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-s --test loopback_e2e refapp_renders -- --nocapture` → `1 passed`.

- [ ] **Step 8: Commit**
```bash
git add crates/rayland-s/src/main.rs
git commit -m "(c)2: route the engine through the actor; fence-gated readback delivery reaches 120/120

Replaces Arc<Mutex<VirglEngine>> with the engine actor: serve and progress_thread hold an
EngineClient and message the actor, so the readback fence (wait_for_work_retired, driven
cooperatively by the actor) no longer deadlocks the ring doorbell. progress_thread rewritten to
ship the reply arena + RingProgress at retirement and the application blobs (readback largest-first,
feedback word last) after the fence. icosa reproducer passes 120/120 across three runs; refapp
unaffected."
```

---

### Task 5: Record the result and full verification

Documentation + the whole-workspace sweep. (c)2's readback is now working; the docs that said it was open must say so.

**Files:**
- Modify: `docs/design/2026-07-18-c2-engine-actor.md` (status → implemented), `docs/c1-the-network.md` §3.1 (the disposition note — readback now solved via the actor), `CLAUDE.md` (the (c)1/(c)2 bullets).

- [ ] **Step 1: Mark the spec implemented.** Update the spec's **Status** line: implemented and verified (120/120 ×3, completing), deferred items (§3) still open.
- [ ] **Step 2: Update `docs/c1-the-network.md` §3.1's disposition** to note the stale/torn readback is now fixed in (c)2 by the engine actor (a `> **Resolved (c)2, 2026-07-18.**` note, matching the doc's existing note style), cross-referencing the actor spec. Keep the evidence; do not delete it.
- [ ] **Step 3: Update `CLAUDE.md`'s (c)2 bullet** to state readback is now solved by the engine actor (one thread owns virglrenderer; the fence/doorbell deadlock removed), pointing at `docs/design/2026-07-18-c2-engine-actor.md`; and note the actor as `rayland-engine`'s ownership model. Correct any statement that (c)2's readback is still open.
- [ ] **Step 4: Full sweep**
Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo clippy --workspace --all-targets` (clean).
Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-relay -p rayland-s -p rayland-c -p rayland-vtest --lib --tests` (all pass, incl. the `no_gpu_linkage` guards).
- [ ] **Step 5: Commit**
```bash
git add docs/design/2026-07-18-c2-engine-actor.md docs/c1-the-network.md CLAUDE.md
git commit -m "(c)2: record the readback return path solved by the engine actor"
```

---

## Self-review notes (for the implementer)

- **The no-deadlock property is the whole point** — preserve it: the actor must never take the applier lock, and `progress_thread` must never hold the applier lock across `wait_for_work_retired`. If you find yourself adding a lock, stop.
- **A wedge is a real bug, not a tuning problem.** If the gate wedges, the cause is a missed/misordered command or a reply never sent — diagnose it; do not paper over it with delays.
- **Do not add settle gates, resource_map, dma-buf sync, or fingerprint deltas** — all retired (spec §1, and `2026-07-18-c2-readback-reachability.md`). This plan is *only* the actor + the fence-gated delivery.
- **Scope:** one in-flight fence, one ring, one context. Multi-fence/multi-ring and per-call-allocation perf are deferred (spec §3); do not build them.
