# (c)2 walking skeleton — the engine actor: one thread owns virglrenderer

**Status:** design spec, 2026-07-18; **implemented and partly validated 2026-07-19 (see §8).** The
actor (§2) was built (Tasks 1–3, committed) and wired in (Task 4): it **solves the deadlock** — the
refapp e2e passes through the actor with no wedge — but the icosa fixture still wedges because
`wait_for_work_retired` is the **wrong fence** (ring retirement `T2`, not GPU-DMA completion `T4`), the
open question §7.3 anticipated. The deadlock half is done; the **T4-completion-barrier half is the
remaining (c)2 work.** Read §8 before acting.

It specifies the **walking-skeleton** fix — the smallest change that reaches **120/120 on the icosa
fixture without deadlocking** — deferring performance and generality. It rests on a chain of retired
alternatives; read [`2026-07-18-c2-readback-reachability.md`](2026-07-18-c2-readback-reachability.md)
and [`2026-07-17-fence-feedback-walking-skeleton.md`](2026-07-17-fence-feedback-walking-skeleton.md)
§9–§11 first — they are why the design is what it is.

## 1. The problem this fixes (proven, not assumed)

Correct readback requires **retiring the host GPU work through an engine call** — a context fence /
`virgl_renderer_context_poll`. No lock-free substitute exists: a passive CPU read (raw `mmap`, or
`DMA_BUF_IOCTL_SYNC`'d — a measured no-op) does not drive completion, and the engine-side transfer is
a hardcoded stub. That fence call takes Rayland's single global engine lock
(`Arc<Mutex<VirglEngine>>`, forced because virglrenderer is not thread-safe), and it **deadlocks**
against the message thread: the fence cannot retire until the host ring thread processes more work,
which needs a **doorbell** (`engine.submit(notify)`) from the message thread — which is blocked
waiting for the very lock the fence holds. Two prototypes confirmed it (Phase 1: `SIGABRT`/timeout;
(c)2 prototype A, fence confined to the app-blocked window: 3/3 `vkWaitForFences` timeout, wedging
after a burst of correct frames). **The lock is the problem.**

## 2. The design: a single engine-owning actor

**One dedicated thread owns `VirglEngine`. There is no engine `Mutex`.** Every engine operation
becomes a message to that thread. With a single owner there is no lock to deadlock on, and the owner
can **interleave** a long fence-wait with incoming doorbells cooperatively — which is exactly what the
two threads could not do.

### 2.1 `EngineClient` — the seam is already there

Every engine call in Rayland goes through the `RenderEngine` trait (`apply()` takes
`&mut dyn RenderEngine`). So B needs no change to `apply()` or its callers: introduce an
**`EngineClient` that *implements* `RenderEngine`** by, for each method, sending an `EngineCommand`
to the actor over a channel and blocking on a per-call reply channel for the result. The message loop
holds an `EngineClient` (cheap, `Clone`) instead of a locked `VirglEngine`.

- `EngineCommand` — an enum with one variant per `RenderEngine` method actually used on (c)1's path
  (`create_venus_context`, `venus_capset`, `create_blob_resource`, `submit`, `unref_resource`,
  `wait_for_work_retired`), each carrying its arguments and a `std::sync::mpsc::Sender` for its typed
  reply. (Methods (c)1 never calls — e.g. `create_resource`/`read_back`, C0's offscreen path — may be
  `unimplemented!()` in the client for the skeleton, with a comment; nothing on this path reaches
  them.) Reply payloads are all `Send` (`Vec<u8>`, `u32`, `BlobResource` incl. its `OwnedFd`,
  `Result<(), EngineError>`).
- No async runtime: plain `std::sync::mpsc`, blocking sends and receives. This matches the engine
  path, which is synchronous today.

### 2.2 `EngineActor` — cooperative scheduling, and why it breaks the deadlock

The actor owns the real `VirglEngine` and the command `Receiver`. Its loop:

- **No fence in flight:** block on `rx.recv()`; handle each command as it arrives — a single engine
  call plus its reply. (Quick: a doorbell is one `engine.submit`; a blob create is one call.)
- **A fence in flight** (from `wait_for_work_retired`): do **not** block. `try_recv()` and handle any
  ready commands *first* (this is the doorbell the fence is waiting on), then advance the fence by
  **one `context_poll`** and check retirement; if retired, send the deferred fence reply; otherwise
  sleep one `FENCE_POLL_INTERVAL` and loop.

So `wait_for_work_retired` on the client sends a `Fence` command and blocks on its reply; the actor
starts the fence, keeps servicing commands (crucially the doorbell) between `context_poll`s, and
sends the fence's reply only when it retires. **The doorbell the fence depends on is now serviced by
the same thread that is polling the fence — they cooperate instead of competing, and the circular
wait is gone by construction.** The actor never takes the applier lock, so it cannot participate in
any other cycle either.

### 2.3 The readback delivery (carried over from prototype A, which built it correctly)

The return path (`progress_thread`) keeps the structure prototype A validated as *correct* (it only
failed on the fence deadlock, which §2.2 fixes):

- **On ring retirement:** ship only the **Venus-internal** blob writes (the reply arena — needed for
  the app's forward progress on non-readback synchronous calls) and then `RingProgress`. The
  readback/feedback are **not** shipped here (the GPU has not finished them). Mark a readback frame in
  flight.
- **While the app is blocked** (the delivery is pending): call `wait_for_work_retired` **through the
  `EngineClient`** (which no longer deadlocks). On success, ship the **application** blob writes,
  **largest blob first** — so the big readback (res=8, 262144 B) lands ahead of the tiny feedback
  word, and the feedback word (which releases the app) lands last, after the pixels it releases the
  app onto.
- The `Applier` split for this is two methods over the existing per-blob diff:
  `take_venus_blob_writes` (only `venus_internal` non-ring blobs) and `take_app_blob_writes` (only
  non-ring, non-`venus_internal`, ordered largest-first). Blob and ring-head reads are plain memory
  reads and take **no** engine command.

## 3. What is deferred (walking skeleton)

- **Performance.** Every engine call is now a channel round-trip; every ring delta's doorbell is one
  such round-trip. Bounded and small, but real. `FENCE_POLL_INTERVAL` cadence, command batching, and
  avoiding a reply channel allocation per call are hardening, not skeleton.
- **Generality.** One in-flight fence at a time (the progress thread awaits each before the next);
  one ring, one context (matching (c)1's pinned `no_multi_ring`). Multiple concurrent fences,
  multi-ring, and queue-depth/backpressure are deferred.
- **The `unimplemented!()` client methods** (`create_resource`/`read_back`) get real bodies only when
  a path needs them.

## 4. Why it is correct and deadlock-free

- **Deadlock-free:** the only shared engine is the actor, reached solely by message; the actor never
  waits on the applier lock; the message and progress threads hold the applier lock only briefly and
  never while the actor waits on them. There is no cycle. The fence and the doorbell are serviced by
  one thread that interleaves them, so the specific cycle that wedged A/Phase 1 cannot form.
- **Correct pixels:** unchanged from prototype A — the fence retires the host work before the readback
  is read, and the largest-first ordering puts the pixels ahead of the releasing feedback word. (That
  the fence yields correct pixels is Phase 1's measured result.)

## 5. Component changes

| File | Change |
|---|---|
| `crates/rayland-engine/src/` (new module, e.g. `actor.rs`) | `EngineCommand`, `EngineActor` (owns `VirglEngine`, the cooperative loop), `EngineClient` (`impl RenderEngine`), and a `spawn` that returns a client + a `JoinHandle`. |
| `crates/rayland-s/src/main.rs` | `serve` spawns the actor instead of wrapping the engine in a `Mutex`; hands an `EngineClient` to the message loop and the progress thread; `progress_thread` takes an `EngineClient` (not `Arc<Mutex<VirglEngine>>`) and calls `wait_for_work_retired` through it; the delivery restructure of §2.3. |
| `crates/rayland-s/src/apply.rs` | `take_venus_blob_writes` / `take_app_blob_writes` (§2.3); `apply()` is otherwise unchanged (still `&mut dyn RenderEngine`). |
| `crates/rayland-s/tests/apply.rs` | `poll_progress` helper recomposed from the two split methods; the mock engine is unaffected (no actor in the no-GPU tests — the `RecordingEngine` is used directly, synchronously). |

The `RenderEngine` trait and `VirglEngine` are unchanged. The actor lives in `rayland-engine`
(it owns `VirglEngine`); `EngineClient` is what `rayland-s` holds.

## 6. Testing / success criteria

- `icosa_cpu_renders...` passes **120/120 on three consecutive runs**, each running to completion (no
  `SIGABRT`, no `vkWaitForFences` timeout, no hang). This is the whole point; it is the gate.
- The refapp single-frame e2e still passes.
- `cargo clippy --workspace --all-targets` clean; unit and no-GPU-linkage tests pass (the actor lives
  in `rayland-engine`, which already links the GPU — no new linkage exposure for `rayland-c`).

## 7. Risks

- **The deadlock is gone but a new stall could hide elsewhere** (e.g. a reply channel that is never
  answered because a command path is missed). The three-run completion gate is what catches it; a
  wedge means a missing/misordered command, to be diagnosed with the stage trace, not tuned around.
- **Per-call channel overhead** could regress the already-message-rate-bound forward path enough to
  matter; if a run *slows* rather than wedges, that is a perf finding for the hardening pass, not a
  skeleton failure.
- **`wait_for_work_retired` is `virgl_renderer_context_create_fence` on ring 0**, which Task 9 argued
  fences the ring rather than the GPU readback — yet Phase 1 measured it yielding correct pixels. If
  the actor makes the fence reliable and frames are still wrong (not wedged), that reopens *which*
  fence is needed — a correctness question the skeleton's gate would surface distinctly from a wedge.

## 8. Implemented — the actor solves the deadlock; the T4 barrier remains (2026-07-19)

Tasks 1–3 (committed): the `Applier` venus/app blob-write split, the non-blocking
`VirglEngine::create_fence`/`poll_fence`, and the **engine actor + `EngineClient`** (`crates/rayland-engine/src/actor.rs`).
Task 4 (the wiring — `serve`/`progress_thread` on the client, `main` spawning the actor) was
implemented and run but **not committed**; it is preserved as `scratchpad/task4-wiring.patch`.

**What was proven — the deadlock is gone.** The **refapp e2e passes 1/1 through the actor**: the whole
message-thread → actor → progress-thread path delivers a correct readback with no wedge, and S's
`wait_for_work_retired` never times out and never `SIGABRT`s. The two-threads-one-lock deadlock that
killed (c)1 Phase 1 *and* (c)2 prototype A — the thing that made "embed non-thread-safe virglrenderer
for this" look impossible — is **solved by construction**. A real, hardware-affine subtlety was found
and fixed along the way: virglrenderer's EGL context is **thread-affine**, so the engine must be
*constructed on the actor thread* (`spawn_engine(render_node: PathBuf)`, not a pre-built engine handed
across the boundary, which reliably `SIGABRT`s).

**What is not done — the completion barrier.** The icosa fixture wedges (3/3, `vkWaitForFences`
timeout at a non-deterministic frame). The cause is *not* the actor and *not* wrong pixels: it is that
`wait_for_work_retired` waits on the **ring** fence (`virgl_renderer_context_create_fence` on ring 0),
which retires when the ring thread *reaches* the work — Task 9's `T2`, up to ~20 ms before the GPU's
DMA and the feedback-word write actually land (`T4`). When the ring fence wins that race,
`take_app_blob_writes` finds the feedback word unchanged, ships nothing, and clears the pending flag;
with the old idle re-check removed, the late write is never delivered and the app hangs. So §7.3's
"the fence retires but is the wrong fence" is **confirmed**, now surfacing as a wedge rather than a
tear (because the single-shot delivery gives up instead of re-sampling).

**The remaining problem, precisely.** S needs a barrier that is true only at **`T4`** — GPU-DMA
completion for the app's readback/feedback-word — not `T2`. The actor has removed the *deadlock
constraint*, so such a barrier no longer has to be lock-free or cheap; what is missing is the barrier
itself. The feedback word *is* the `T4` signal (vkr writes it at completion), so gating the ship on it
is the principled shape; the recurring difficulty has been detecting it reliably. Whether virglrenderer
exposes a `T4` fence directly (rather than the `T2` ring fence) is the next thing to establish — an
investigation, not more delivery code. Until it is answered, the daemon is left on the pre-actor path
(the branch tip runs, with the known stale/torn readback), and the actor stands as a committed,
smoke-tested building block that the wiring patch activates once the barrier is right.
