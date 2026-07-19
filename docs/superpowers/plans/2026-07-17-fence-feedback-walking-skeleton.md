# The stale-frame fix (fence-feedback walking skeleton) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the application's readback wait release only after S's GPU has actually finished writing the pixels, so the `icosa_cpu_renders...` reproducer passes 120/120 every run — by buying back Venus's fence feedback and delivering the completion write across the network.

**Architecture:** Enabling fence feedback makes vkr write the retired fence value into a feedback buffer (a Venus-internal blob) *at GPU completion*; the application polls that word locally to learn its fence retired. S delivers that write to C on the existing `BlobData` return path, ordered after the readback pixels, driven by a cheap per-poll fingerprint so it ships even while the blocked application produces no ring traffic. C is unchanged. The now-wrong ring-fence "barrier" is removed.

**Tech Stack:** Rust 2024 (`rust-version = "1.85"`), the (c)1 crates `rayland-s` / `rayland-relay` / `rayland-engine`, `libvirglrenderer` via `rayland-engine`. The reproducer is `crates/rayland-s/tests/loopback_e2e.rs`, run on S (a Venus-capable GPU host — this is `dop561`).

**The spec is [`docs/design/2026-07-17-fence-feedback-walking-skeleton.md`](../../design/2026-07-17-fence-feedback-walking-skeleton.md). Read it first** — it explains *why* each step below is correct and where the walking skeleton's correctness is scoped (the synchronous one-fence-per-frame case). It rests on [`docs/c1-the-network.md`](../../c1-the-network.md) §3.1 (the `T2 < T4` proof) and [`docs/design/2026-07-17-return-path-completion.md`](../../design/2026-07-17-return-path-completion.md) (the analysis).

## Global Constraints

Every task's requirements implicitly include this section.

- **Rust edition 2024, `rust-version = "1.85"`.** Let-chains stabilised in 1.88; do not use them — write nested `if`s, as the surrounding code does.
- **Comment discipline (`CLAUDE.md`):** a doc-comment block on every function/type/module; an intent comment on every non-trivial line explaining *why*, never restating syntax; **code and comments must always agree** — a stale or contradicting comment is a bug fixed in the same edit. If any change makes a statement in `CLAUDE.md` or a design doc false, fix it in the same change.
- **No Claude/AI attribution** anywhere in code, comments, docs, or commit messages.
- **The instrumentation must stay zero-overhead when off.** `rayland_relay::trace::*` is gated by `RAYLAND_C1_TRACE`; do not add unconditional tracing.
- **Never judge a race from one run.** The reproducer's failure rate swings 3–39/120. Any pass/fail claim needs **at least three consecutive runs**. A single lucky run already fooled a prior session.
- **Where to run.** The GPU e2e test only does real work on a Venus host (`dop561`); it *skips* (prints `SKIP`, returns) elsewhere. Build is release on purpose (`build_binary_release`) — debug is ~4 s/frame and trips Mesa's stall abort. Use `CARGO_TARGET_DIR=/tmp/rayland-task9-target` to reuse the warm build.
- **Work in the worktree** `/tmp/rayland-c1-wt` on branch `c1-the-network`. Do not push or open a PR.

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/rayland-s/src/apply.rs` | `Applier`: turn C's messages into GPU work and produce what S owes back. | Add `fingerprint_nonring_blobs`. |
| `crates/rayland-s/src/main.rs` | S's daemon: the message thread and the progress (return-path) thread. | Rewrite `progress_thread`: remove the barrier + `engine` param, add the fingerprint-gated delivery, add the `ship` helper. Fix the spawn site. |
| `crates/rayland-s/tests/loopback_e2e.rs` | The reproducer (and a temporary diagnostic in Task 2). | Un-pin `no_fence_feedback` for the icosa launch (line 699). |
| `crates/rayland-engine/src/virgl.rs` | `VirglEngine`: the real engine. | Truth-up `wait_for_work_retired`'s doc comment (its caller is removed). |
| `docs/design/2026-07-15-c1-the-network.md` | The (c)1 spec, incl. §6's crutch table. | Record that `no_fence_feedback` is bought back. |
| `docs/c1-the-network.md`, `docs/design/2026-07-17-*.md`, `CLAUDE.md` | Findings + design docs. | Record the fix landed (Task 5). |

---

### Task 1: `Applier::fingerprint_nonring_blobs`

The delivery loop needs to detect, cheaply and every poll, that *any* non-ring blob's contents moved — including the feedback buffer's very first write. `fingerprint_written_blobs` (Probe A's helper) is scoped to blobs S has *already shipped a run for*, which the feedback buffer has not on its first write — so a new method over **all** non-ring blobs is required (spec §3.2, the bootstrap-deadlock note).

**Files:**
- Modify: `crates/rayland-s/src/apply.rs` (add a method to `impl Applier`, next to `fingerprint_written_blobs`)
- Test: `crates/rayland-s/tests/apply.rs` (this is where `Applier` is tested — against a no-GPU `RecordingEngine` mock and real memfds; **there is no in-file `#[cfg(test)]` module in `apply.rs`**)

**Interfaces:**
- Consumes: `Applier`'s `blobs: HashMap<u32, HostBlob>` and `rings: HashMap<u32, RingMirror>` fields; `HostBlob::bytes(&self) -> &[u8]`; `rayland_relay::trace::fingerprint(&[u8]) -> u64`.
- Produces: `pub fn fingerprint_nonring_blobs(&self) -> Vec<(u32, u64)>` — `(res_id, fingerprint)` for every blob that is **not** a ring. Later tasks call it once per poll and compare successive results.

- [ ] **Step 1: Write the failing test**

Add this test to `crates/rayland-s/tests/apply.rs` (an integration test file that drives `Applier::apply` against the `RecordingEngine` double already defined there). It reuses the existing `session_with_ring()` helper (which creates a context and a ring blob and returns the ring's `res_id`) and the established non-ring blob shape — the live capture's 64-byte vertex buffer, `blob_id: 16, size: 64`, exactly as `a_ring_delta_for_a_resource_that_is_not_a_ring_is_refused` builds it. `BLOB_MEM_HOST3D` and `CTX_ID` are constants already in scope in this file.

```rust
/// `fingerprint_nonring_blobs` reports every blob that is not a ring, and omits rings — the contract
/// the fence-feedback delivery loop relies on to watch the feedback buffer (a non-ring Venus-internal
/// blob) from its very first write, while never mistaking a ring's `head`/command bytes for a write to
/// return.
#[test]
fn fingerprint_nonring_blobs_covers_non_rings_and_omits_rings() {
    let (mut applier, mut engine, ring_res_id) = session_with_ring();

    // The app's 64-byte vertex buffer from the live capture: a real blob, but not a ring (its
    // power-of-two size is not ring-shaped, so `RingIdentity::from_blob_request` rejects it).
    let out = applier.apply(
        &mut engine,
        C2S::CreateBlob {
            blob_mem: BLOB_MEM_HOST3D,
            blob_flags: 0,
            blob_id: 16,
            size: 64,
        },
    );
    let plain_res_id = match out.as_slice() {
        [S2C::BlobCreated { res_id, .. }] => *res_id,
        other => panic!("expected exactly one BlobCreated, got {other:?}"),
    };

    let ids: std::collections::HashSet<u32> = applier
        .fingerprint_nonring_blobs()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(
        ids.contains(&plain_res_id),
        "a non-ring blob must be fingerprinted"
    );
    assert!(
        !ids.contains(&ring_res_id),
        "a ring blob must be omitted"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-s --test apply fingerprint_nonring_blobs -- --nocapture`
Expected: FAIL — `no method named fingerprint_nonring_blobs`.

- [ ] **Step 3: Add the method**

In `crates/rayland-s/src/apply.rs`, immediately after `fingerprint_written_blobs` in `impl Applier`:

```rust
    /// **(c)1 fence-feedback delivery support**: fingerprint every blob that is not a ring, cheaply.
    ///
    /// # What this is for
    /// The return path's delivery loop (`rayland-s`'s `progress_thread`) calls this once per poll and
    /// ships blob writes whenever a fingerprint moves. Unlike [`Self::fingerprint_written_blobs`],
    /// which is scoped to blobs S has already been *observed* to write (Probe A's set), this covers
    /// **every** non-ring blob — because the feedback buffer must be watched from its *first* write,
    /// before it has ever been in the S-written set (see the spec's §3.2 bootstrap-deadlock note).
    ///
    /// Rings are excluded for the same reason [`Self::take_blob_writes`] excludes them: a ring's pages
    /// are C's command bytes and S's `head`, not S's writes to return.
    ///
    /// # Inputs / outputs
    /// - Returns `(res_id, fingerprint)` for every non-ring blob, using the same strided
    ///   [`rayland_relay::trace::fingerprint`] as Probe A so the values are comparable across polls.
    ///   Cheap enough (a strided hash, microseconds per blob) to call on every 200 µs poll.
    pub fn fingerprint_nonring_blobs(&self) -> Vec<(u32, u64)> {
        self.blobs
            .iter()
            // A ring's pages are not S's writes to return — exclude them, exactly as `take_blob_writes`
            // does, so a `head` store is never mistaken for a completion write.
            .filter(|(res_id, _)| !self.rings.contains_key(res_id))
            .map(|(&res_id, blob)| (res_id, rayland_relay::trace::fingerprint(blob.bytes())))
            .collect()
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-s --test apply fingerprint_nonring_blobs -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Clippy and commit**

Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo clippy -p rayland-s --all-targets`
Expected: clean.

```bash
git add crates/rayland-s/src/apply.rs crates/rayland-s/tests/apply.rs
git commit -m "(c)1 fence-feedback: Applier::fingerprint_nonring_blobs, over all non-ring blobs"
```

---

### Task 2: De-risk — confirm S observes vkr's feedback write (throwaway spike)

Spec §6 step 0: before building the delivery, prove S can *see* vkr's write to the feedback buffer in its own mapping. If it cannot, this whole approach is wrong and the plan must pivot to the §8 `FENCE_COMPLETE` fallback (out of scope here). This task writes **no permanent code** — every edit is reverted at the end; its deliverable is a go/no-go decision plus the evidence for it.

**Files (all reverted at the end):**
- Modify: `crates/rayland-s/tests/loopback_e2e.rs:699` (drop `no_fence_feedback` for the icosa launch)
- Modify: `crates/rayland-s/src/main.rs` (a temporary diagnostic in `progress_thread`)

- [ ] **Step 1: Un-pin fence feedback for the icosa launch**

In `crates/rayland-s/tests/loopback_e2e.rs`, the icosa app launch (around line 699), change the `VN_PERF` value from
`"no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback"`
to
`"no_multi_ring,no_semaphore_feedback,no_event_feedback,no_query_feedback"` (drop `no_fence_feedback` only).

- [ ] **Step 2: Add a temporary every-poll diagnostic**

In `crates/rayland-s/src/main.rs`'s `progress_thread`, at the very top of the `loop {`, add a throwaway probe that logs when a non-ring blob changes while nothing retired. This uses Task 1's method and does **not** ship anything:

```rust
        // TASK9-SPIKE (throwaway): does a non-ring blob's content move while the app is blocked?
        // If a Venus-internal blob changes with no ring retirement, that is vkr's feedback write,
        // visible to S — which is exactly what the fence-feedback delivery will ship. Revert this.
        {
            use std::sync::OnceLock;
            static PREV: OnceLock<std::sync::Mutex<HashMap<u32, u64>>> = OnceLock::new();
            let prev = PREV.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
            let cur: HashMap<u32, u64> = {
                let session = applier.lock().expect("the applier lock is never poisoned");
                session.fingerprint_nonring_blobs().into_iter().collect()
            };
            let mut prev = prev.lock().expect("spike lock");
            for (res_id, fp) in &cur {
                if prev.get(res_id) != Some(fp) {
                    eprintln!("SPIKE_NONRING_CHANGE res={res_id}");
                }
            }
            *prev = cur;
        }
```

- [ ] **Step 3: Run and observe (kill early — it will hang)**

With feedback on but no delivery, the app hangs polling a word S never ships, so the test will not finish — run it in the background, watch for the marker, then stop it.

Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-s --test loopback_e2e icosa_cpu_renders -- --nocapture > /tmp/spike.log 2>&1 &`
Then, after ~60 s: `grep -c SPIKE_NONRING_CHANGE /tmp/spike.log` and `grep "res=" /tmp/spike.log | grep SPIKE_NONRING_CHANGE | sort -u`, then kill the run (`pkill -f loopback_e2e`).

Expected (go): `SPIKE_NONRING_CHANGE` lines appear for one or more `res` while the app is otherwise blocked — S sees vkr's feedback write. The design holds; proceed to Task 3.
If **no** such lines ever appear (no-go): stop. S cannot observe the feedback write; the fix must become the explicit `FENCE_COMPLETE` message of spec §8, which this plan does not cover. Record the finding and re-plan.

- [ ] **Step 4: Revert every spike edit**

Restore `crates/rayland-s/tests/loopback_e2e.rs:699` to include `no_fence_feedback`, and delete the `TASK9-SPIKE` block from `progress_thread`.

Run: `git diff --stat` — expected: **no changes** (working tree clean; only Task 1's commit is present).
Run: `grep -rn "SPIKE" crates/` — expected: no output.

Do **not** commit anything in this task.

---

### Task 3: The fix — deliver the feedback write, remove the dead barrier, un-pin feedback

This is the whole fix and is verified end-to-end by the reproducer. It rewrites `progress_thread` to (a) drop the ring-fence barrier (the wrong, now-superseded signal — its inline claim "the barrier guarantees the GPU's writes have landed" is false per Task 9), (b) deliver non-ring blob changes every poll gated on the cheap fingerprint, ordered readback-before-feedback, and (c) factor the send into a `ship` helper. It also un-pins `no_fence_feedback` for the icosa app and records that in the crutch table.

**Files:**
- Modify: `crates/rayland-s/src/main.rs` — `progress_thread` (currently lines 247–349), its spawn site (currently lines 601–610), and add a `ship` helper.
- Modify: `crates/rayland-s/tests/loopback_e2e.rs:699` — drop `no_fence_feedback` for the icosa launch (permanent this time).
- Modify: `crates/rayland-engine/src/virgl.rs` — `wait_for_work_retired`'s doc comment (its caller is being removed).
- Modify: `docs/design/2026-07-15-c1-the-network.md` — the §6 crutch table entry for `no_fence_feedback`.

**Interfaces:**
- Consumes: `Applier::take_ring_progress()`, `Applier::take_blob_writes()`, `Applier::fingerprint_nonring_blobs()` (Task 1), `Applier::applied_ring_deltas()`, `Applier::fingerprint_written_blobs()`; `probe_a_resample`, `ProbeBaseline`, `send`, `PROGRESS_POLL`; `rayland_relay::{S2C, trace}`.
- Produces: a new free function `fn ship(tx: &Arc<Mutex<QuicSend>>, msgs: &[S2C]) -> Result<(), ()>`; `progress_thread` now takes `(applier: Arc<Mutex<Applier>>, tx: Arc<Mutex<QuicSend>>)` — **no `engine` parameter**.

- [ ] **Step 1: Add the `ship` helper**

In `crates/rayland-s/src/main.rs`, add this free function just above `fn progress_thread`:

```rust
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
```

- [ ] **Step 2: Replace `progress_thread`'s doc comment and body**

Replace the entire `progress_thread` function — its `///` doc-comment block *and* the function body through its closing brace (`fn progress_thread` is around line 247; the doc block is the ~50 lines directly above it) — with the following. Read the current doc block first so you delete **all** of it, including the large "⚠️ This ordering is right and it DOES NOT FIX THE DEFECT" section, which the fix makes obsolete. Do **not** delete `struct ProbeBaseline` or `fn probe_a_resample`, which follow the function and are still used.

```rust
/// The progress thread: deliver what S's GPU wrote, and release the application only once it has.
///
/// **This is the only thing that ever releases the application's synchronous Vulkan calls**, so its
/// structure is the correctness argument, not a detail. It has two jobs each poll:
///
/// 1. **On ring retirement** — ship what S wrote (`take_blob_writes`) and then the `RingProgress`
///    that advances C's `head`. `head` carries ring space and the reply arena, which the
///    application's non-readback synchronous calls block on.
/// 2. **On every poll** — deliver GPU-completion writes. With fence feedback bought back (see
///    `docs/design/2026-07-17-fence-feedback-walking-skeleton.md`), the application's readback wait is
///    no longer released by `head`; it polls a **feedback word** that vkr writes *at GPU completion*.
///    That word, and the finished readback pixels, are ordinary blob writes S must ship — and it must
///    ship them even while the application is blocked and producing no ring traffic. So this thread
///    watches every non-ring blob's fingerprint and, when one moves with no retirement, ships the
///    change. `take_blob_writes` orders application blobs (the readback) ahead of Venus-internal blobs
///    (the feedback word), so C installs the pixels before the application is ever let go.
///
/// # Why there is no GPU barrier here any more
/// This thread used to wait on `RenderEngine::wait_for_work_retired` before shipping, in the belief
/// that a retired ring fence meant the GPU's readback had landed. `docs/c1-the-network.md` §3.1
/// measured that this is false — the ring fence retires when the ring thread *reaches* it, up to ~20
/// ms before the GPU's readback completes. The completion signal that *is* real is the feedback word,
/// delivered above; the barrier is gone.
///
/// # Inputs / outputs
/// - `applier`: shared with the message thread. Locked only for short reads, never across a send.
/// - `tx`: the link to C.
/// - Returns when the link fails; the session is over either way.
fn progress_thread(applier: Arc<Mutex<Applier>>, tx: Arc<Mutex<QuicSend>>) {
    // Probe A state (trace-only) — see `ProbeBaseline`. Unused unless `RAYLAND_C1_TRACE` is set.
    let mut probe_baseline: HashMap<u32, ProbeBaseline> = HashMap::new();
    // Fence-feedback delivery state: the fingerprint of every non-ring blob at the previous poll. A
    // fingerprint that moves with no ring retirement is S's GPU writing a completion (the finished
    // readback pixels and the feedback word Mesa polls); shipping it is what closes the stale-frame
    // race and what the spike-2 hang was missing.
    let mut prev_fp: HashMap<u32, u64> = HashMap::new();

    loop {
        // One short lock: the retirement frontier, plus a cheap strided fingerprint of every non-ring
        // blob. The fingerprint is the gate that keeps the full byte-diff off the idle path.
        let (progress, cur_fp) = {
            let mut session = applier.lock().expect("the applier lock is never poisoned");
            let progress = session.take_ring_progress();
            let cur_fp: HashMap<u32, u64> =
                session.fingerprint_nonring_blobs().into_iter().collect();
            (progress, cur_fp)
        };

        if !progress.is_empty() {
            // A ring retired. Ship what S wrote, then the progress that advances `head`. Blob bytes
            // FIRST, progress LAST: the reply arena rides in the blobs and the reply-ready signal is
            // `head`, so the arena must be on the wire before the update that releases a waiter on it.
            let blobs = {
                let mut session = applier.lock().expect("the applier lock is never poisoned");
                session.take_blob_writes()
            };
            if ship(&tx, &blobs).is_err() {
                return;
            }
            if ship(&tx, &progress).is_err() {
                return;
            }

            // Probe A baseline (trace-only), unchanged: record what was shipped so the idle resample
            // can catch a GPU write that lands after it.
            if rayland_relay::trace::enabled() {
                let ship_ns = rayland_relay::trace::monotonic_ns();
                let session = applier.lock().expect("the applier lock is never poisoned");
                let deltas = session.applied_ring_deltas();
                for (res_id, fp) in session.fingerprint_written_blobs() {
                    probe_baseline.insert(res_id, ProbeBaseline { fp, ship_ns, deltas });
                }
            }
        } else {
            // Nothing retired: the application is blocked, polling its feedback word. If any non-ring
            // blob moved since the last poll, S's GPU wrote a completion — ship it. `take_blob_writes`
            // orders the readback (an application blob) ahead of the feedback word (Venus-internal), so
            // C installs the finished pixels before the word that releases the application to read them.
            if cur_fp != prev_fp {
                let blobs = {
                    let mut session = applier.lock().expect("the applier lock is never poisoned");
                    session.take_blob_writes()
                };
                if ship(&tx, &blobs).is_err() {
                    return;
                }
            }
            // Probe A idle resample (trace-only), unchanged.
            if rayland_relay::trace::enabled() && !probe_baseline.is_empty() {
                probe_a_resample(&applier, &mut probe_baseline);
            }
        }

        // Remember this poll's fingerprints so the next poll can tell what moved.
        prev_fp = cur_fp;

        // Wait before looking again; see `PROGRESS_POLL`.
        std::thread::sleep(PROGRESS_POLL);
    }
}
```

- [ ] **Step 3: Fix the spawn site (drop the `engine` argument)**

In `crates/rayland-s/src/main.rs` (currently ~lines 601–610), change the progress-thread spawn so it no longer clones or passes `engine`:

```rust
    // The poller: the only thing that ever releases the application's synchronous calls.
    std::thread::Builder::new()
        .name("rayland-s-progress".into())
        .spawn({
            let applier = Arc::clone(&applier);
            let tx = Arc::clone(&tx);
            move || progress_thread(applier, tx)
        })
        .context("spawning the progress thread")?;
```

- [ ] **Step 4: Truth-up the `wait_for_work_retired` doc comment**

Its only caller is now gone. In `crates/rayland-engine/src/virgl.rs`, find `fn wait_for_work_retired` (around line 1002) and its doc comment (around lines 982–1004). Replace the sentence that says it exists "so that `rayland-s`'s return path can ask the same question before it ships pixels to C" with an honest note that the caller was removed:

```rust
    /// Block until every command already submitted on `(ctx_id, ring_idx)` has retired on the GPU.
    ///
    /// The whole implementation is [`VirglEngine::wait_for_context_fence`], which [`Self::read_back`]
    /// has used since C0 Task 3 to avoid reading a half-drawn frame.
    ///
    /// # No longer on the (c)1 return path
    /// This was briefly called by `rayland-s`'s progress thread as a pre-ship "barrier", but
    /// `docs/c1-the-network.md` §3.1 proved a virglrenderer *context* fence retires when the ring
    /// thread reaches it — not when the GPU's readback completes — so it was the wrong quantity and
    /// that caller was removed by the fence-feedback fix
    /// (`docs/design/2026-07-17-fence-feedback-walking-skeleton.md`). The method stays as a genuine
    /// engine capability (`read_back` relies on the same primitive for resources *S itself* submits),
    /// but nothing on the return path calls it today.
```

Keep the existing `# Thread-safety` and `# Failure modes` sections that follow. (If the exact wording differs, preserve the two sections and only replace the purpose paragraph.)

- [ ] **Step 5: Un-pin `no_fence_feedback` for the icosa app**

In `crates/rayland-s/tests/loopback_e2e.rs`, the icosa launch (line ~699), drop `no_fence_feedback`:

```rust
        .env(
            "VN_PERF",
            // Fence feedback is bought back (the stale-frame fix): the application must wait on the
            // feedback word vkr writes at GPU completion, which S delivers, rather than on the ring
            // head. The other feedback crutches stay pinned — this fixture uses only fences.
            "no_multi_ring,no_semaphore_feedback,no_event_feedback,no_query_feedback",
        )
```

Leave the refapp launch (line ~481) unchanged: it renders a single frame with no readback-wait race, so its env is not what this fix is about.

- [ ] **Step 6: Record the buy-back in the crutch table**

In `docs/design/2026-07-15-c1-the-network.md`, find the §6 crutch-table row for `no_fence_feedback` (grep `no_fence_feedback`). Add a note that it is now bought back for the stale-frame fix, cross-referencing `docs/design/2026-07-17-fence-feedback-walking-skeleton.md`. Keep the row (its history is useful); append the status. Match the table's existing formatting.

- [ ] **Step 7: Build and clippy**

Run: `CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo clippy -p rayland-s -p rayland-engine --all-targets`
Expected: clean. In particular no "unused variable `engine`" or "unused import" — if `VirglEngine` or `RenderEngine` is now unused in `main.rs`, remove it from the `use`; if it is still used by the message thread (it is — the message thread calls the engine), leave it.

- [ ] **Step 8: Run the reproducer three times**

Run (three times):
```
CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-s --test loopback_e2e icosa_cpu_renders -- --nocapture
```
Expected each time: `test result: ok. 1 passed` — the assertion "0 of 120 frames differ" holds. **All three must pass.** If any run reports differing frames, the fix is incomplete — do not proceed; return to systematic-debugging with the trace (Step 9) before changing anything.

If instead the test *hangs* to its 600 s timeout, the feedback write is not being delivered — re-run once under `RAYLAND_C1_TRACE=1` and check whether `T5` lines appear for a Venus-internal `res` while the app is blocked; if they do not, this is the Task 2 no-go surfacing late, and the pivot is spec §8.

- [ ] **Step 9: Confirm the ordering held, under trace**

Run once more with tracing and analyse:
```
RAYLAND_C1_TRACE=1 CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-s --test loopback_e2e icosa_cpu_renders -- --nocapture 2>&1 | grep RLTRACE | python3 scripts/c1-trace-analyze.py
```
Expected: the "Probe A" section reports **no late GPU writes** (or, if any fire, the run still passed 120/120 in Step 8 — Probe A watches the *readback* blob, which may still be re-diffed harmlessly; the pass/fail gate is Step 8, not this). The "Return-path ordering" section must still show `T7 seen before any matching T6: 0`.

- [ ] **Step 10: Commit**

```bash
git add crates/rayland-s/src/main.rs crates/rayland-s/tests/loopback_e2e.rs crates/rayland-engine/src/virgl.rs docs/design/2026-07-15-c1-the-network.md
git commit -m "(c)1 Task 9: fix the stale-frame race by buying back fence feedback

Enables fence feedback for the application and delivers vkr's completion write across the
network on the existing BlobData path, ordered after the readback pixels, so C never
releases the application onto unfinished pixels. Driven by a per-poll strided fingerprint
so the feedback+readback writes ship while the app is blocked and produces no ring traffic.
Removes the ring-fence barrier, which docs/c1-the-network.md §3.1 proved retires up to ~20
ms before the GPU readback and whose inline claim to the contrary was false. The icosa
reproducer now passes 120/120 across three runs; C is unchanged."
```

---

### Task 4: Record that the fix landed, and final verification

Documentation-only, plus the whole-workspace verification sweep. The findings and design docs currently say the bug is open; that is now false and must be corrected in the same branch.

**Files:**
- Modify: `docs/c1-the-network.md` — §3.1 (the bug is fixed).
- Modify: `docs/design/2026-07-17-return-path-completion.md` — §8 / status (the fix is implemented).
- Modify: `docs/design/2026-07-17-fence-feedback-walking-skeleton.md` — status (implemented and verified).
- Modify: `CLAUDE.md` — the (c)1 arc / Task status line, if it states the race is open.

- [ ] **Step 1: Mark the finding resolved in `docs/c1-the-network.md`**

In §3.1, add a short closing note (a `> **Resolved 2026-07-17.**` block, matching the doc's existing "Superseded"/"Resolved" note style) stating that the race is fixed by buying back fence feedback, cross-referencing `docs/design/2026-07-17-fence-feedback-walking-skeleton.md`, and noting the reproducer now passes 120/120. Do **not** delete the evidence — it is the record of why the fix is shaped as it is.

- [ ] **Step 2: Update the design-note and spec statuses**

In `docs/design/2026-07-17-return-path-completion.md`, update the top **Status** line and §8 to say the fix (not just the instrumentation) is now implemented, pointing to the walking-skeleton spec and plan. In `docs/design/2026-07-17-fence-feedback-walking-skeleton.md`, change the **Status** line from "design spec" to note it is implemented and verified (120/120 ×3), and that the deferred §5 hardening remains open.

- [ ] **Step 3: Truth-up `CLAUDE.md` if needed**

Grep `CLAUDE.md` for any statement that the stale-frame race / return path is unsolved (e.g. the (c)2 bullet or the arc-(c) description). If one is now false, correct it — noting (c)1 fixed the synchronous-readback case and the general (overlapping-frame) case remains (c)2's, per the spec's §5. If nothing is false, make no change and say so.

- [ ] **Step 4: Full verification sweep**

Run:
```
CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo clippy --workspace --all-targets
CARGO_TARGET_DIR=/tmp/rayland-task9-target cargo test -p rayland-relay -p rayland-s -p rayland-c --lib --tests
```
Expected: clippy clean; all unit and no-GPU-linkage tests pass. The `refapp_renders_across_the_network...` e2e must still pass (single frame, unaffected). The `icosa_cpu_renders...` e2e passes (already verified in Task 3 Step 8; the `--tests` run will re-run it once — for the record, not as the three-run gate).

- [ ] **Step 5: Commit**

```bash
git add docs/c1-the-network.md docs/design/2026-07-17-return-path-completion.md docs/design/2026-07-17-fence-feedback-walking-skeleton.md CLAUDE.md
git commit -m "(c)1 Task 9: record the stale-frame race resolved by the fence-feedback fix"
```

---

## Self-review notes (for the implementer)

- **Scope boundary:** correctness here is for the **synchronous, one-fence-per-frame** case (the icosa fixture). Overlapping-frame applications need the deferred immutable-generation machinery (spec §5) — do not claim otherwise in any doc.
- **The barrier removal is deliberate, not cleanup drift:** it is the signal Task 9 proved wrong, and leaving it would leave a false comment. If Step 8 fails only *after* removing it, that is information (it never helped), not a reason to restore it.
- **If Task 2 says no-go**, stop and re-plan around spec §8 (`FENCE_COMPLETE`); Tasks 3–4 assume go.
