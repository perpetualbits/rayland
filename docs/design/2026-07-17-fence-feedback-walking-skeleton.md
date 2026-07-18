# The stale-frame fix, minimal-correct: buy back fence feedback and deliver it

**Status:** design spec, written 2026-07-17; **superseded 2026-07-18.** Implemented as specified and it
failed (§9); the root cause was re-diagnosed twice (§10, §11); the final conclusion (§11) is that
**app-initiated readback cannot be fixed by patching S's observe-and-diff return path at all, and is
rescoped to (c)2.** Read §11 first — §3–§10 are the trail that led there, preserved, not current
guidance. The one durable result is §2's `T2 < T4` measurement, which stands.

It specifies the **walking-skeleton** fix for (c)1 Task
9's stale-frame race — the smallest change that is *genuinely correct* (120/120 every run), not the
smallest change that hides the symptom. It is the concrete implementation of the direction argued in
[`2026-07-17-return-path-completion.md`](2026-07-17-return-path-completion.md) §6, narrowed to what
the walking skeleton needs and grounded in two spikes run on 2026-07-17 (recorded in §2).

The full robustness machinery of that note's §6 — an explicit `FENCE_COMPLETE` message, immutable
`buffer_id`/`generation` versions, damage regions, `GENERATION_COMMITTED` accounting — is
**deliberately deferred** to a later hardening pass. This spec says why the smaller thing is correct,
and exactly where it is not yet robust.

---

## 1. The problem, in one paragraph

An unmodified Vulkan application on C renders offscreen on S's GPU and reads its pixels back
(`vkCmdCopyImageToBuffer` into its own mapped buffer, then `vkWaitForFences`, then a CPU read). Across
(c)1's relay, some frames arrive as the *previous* frame whole, others *torn*. The measured evidence
and the proof that the cause is `T2 < T4` (S ships the readback bytes before its GPU has finished
writing them) are in [`../c1-the-network.md`](../c1-the-network.md) §3.1. This spec assumes that
evidence and does not re-argue it.

## 2. What the mechanism actually is (proven, not assumed)

Two throwaway spikes on 2026-07-17 settled the question the fix rests on: *what, on S, signals that
the GPU's readback is complete (T4)?*

**Spike 1 — under `no_fence_feedback` (today's config), S has no GPU-completion signal at all.** With
S's own ring fences (`virgl_renderer_context_create_fence`, the "barrier" of the negative-result
commit `c787b52`) disabled, S's `write_context_fence` callback fired **zero times** across a full
120-frame run. The application's own queue fences never reach S through that callback. So today the
application's `vkWaitForFences` is satisfied only by the **ring head** advancing (`S2C::RingProgress`),
which S advances as soon as its ring thread *reaches* the work — before the GPU *finishes* it. That is
the `T2 < T4` race, at its root.

**Spike 2 — fence completion is a *buffer write*, and enabling feedback surfaces it (but nothing
delivers it).** With `no_fence_feedback` removed from the application's `VN_PERF`, the application
initialised, created its device and blobs, ran, and then **timed out** ("a wait operation has not
completed in the specified time"). `write_context_fence` *still* fired zero times. The conclusion:
Venus signals fence completion by having the host (vkr) **write the retired fence value into a
feedback buffer** — a Venus-internal shmem the guest polls locally — at GPU completion. That write is
the T4 signal we need. It exists only when feedback is enabled, and under (c)1 nothing carries it
across the network, so the application polls a word that never changes and hangs.

**Therefore:** enabling fence feedback is not an optimisation to buy back later; it is the *only* way
a T4 signal exists on S at all. The fix must enable it **and** deliver the resulting completion write
to C. Both spike edits were reverted; they live only in this document.

## 3. The fix

Three parts. The surprise is how little code each is — because the completion signal, once feedback is
enabled, is *ordinary blob data* that S's existing return path already knows how to ship.

### 3.1 Un-pin `no_fence_feedback` for the application

The application must run with fence feedback **on**. Concretely, its `VN_PERF` drops
`no_fence_feedback` and keeps the rest of (c)1's crutches
(`no_multi_ring,no_semaphore_feedback,no_event_feedback,no_query_feedback`).

This single change moves the readback wait off the ring head and onto a feedback word that vkr writes
**at GPU completion**. It closes the stale-frame path at its source: the head advancing early can no
longer release the readback wait, because the readback wait no longer watches the head.

`no_fence_feedback` is set in two places today, both in the loopback e2e test
(`crates/rayland-s/tests/loopback_e2e.rs`, the refapp and icosa launches). In the field the value is
chosen by whoever launches the application; (c)1's crutch table (spec §6) documents the recommended
set, and that documentation must drop `no_fence_feedback` in the same change. The refapp test renders
a single frame with no readback wait to speak of and may keep its current env; only the icosa test's
launch *must* change, and that is the test that gates this fix.

### 3.2 S delivers the completion write, gated on a cheap fingerprint

Today S ships a blob's changed bytes (`Applier::take_blob_writes`) **only when a ring retires**
(`Applier::take_ring_progress` returns non-empty). That gate is wrong for the feedback write: a
blocked application produces no ring traffic, so the write that would release it never ships — exactly
the spike-2 hang.

The change: in `progress_thread` (`crates/rayland-s/src/main.rs`), on **every** poll, fingerprint the
non-ring blobs (the cheap strided `rayland_relay::trace::fingerprint`, the same one Probe A uses); if
any fingerprint moved, run the full `take_blob_writes` and ship the changes. The full byte-diff — the
expensive part — runs only when the fingerprint says something changed, so an idle inter-frame gap
costs one strided hash per poll, not a 1 MiB `memcmp`.

**The watched set must be *all* non-ring blobs, not the already-S-written set.** Probe A's
`s_written` records only blobs S has *already shipped a run for*, and the feedback buffer enters it
only *after* its first write is shipped — so gating on `s_written` would never detect that first write
(a bootstrap deadlock: the buffer is not watched until it has been shipped, and it cannot be shipped
until it is watched). Fingerprinting every non-ring blob avoids this; the ring blobs are excluded for
the same reason `take_blob_writes` excludes them (their pages are C's command bytes and S's `head`,
not S's writes to return). A fingerprint move on a blob C wrote forward (its vertex/fractal memory) is
harmless: `take_bytes_s_wrote` re-baselines C's writes, so the full diff simply ships nothing for it —
a wasted diff, never wrong bytes.

The ordering that makes this correct is the one `take_blob_writes` already imposes: **application
blobs first, Venus-internal blobs last** (`Applier::venus_internal`). The readback buffer is an
application blob; the feedback buffer is Venus-internal. So the readback pixels are always shipped
ahead of the feedback word that releases the application to read them.

`S2C::RingProgress` on ring retirement stays exactly as it is — it still carries ring space and the
reply arena, which the application needs to make forward progress through non-readback synchronous
calls. The fingerprint-gated ship is *added alongside* it, not a replacement.

### 3.3 C does not change

The feedback buffer is an ordinary blob C already shadows (it arrives via the `C2S::CreateBlob` path
like any other, which spike 2 confirmed happens). `apply_blob_data` already installs arbitrary
`S2C::BlobData` into a shadow's mapped pages, and Mesa on C polls those pages locally. So the
completion write reaches the application through machinery that already exists. **No `rayland-c`
change is required.**

## 4. Why it is race-free

The correctness argument is one sentence: **the feedback word exists only after the GPU (including the
readback copy) is complete, and it is shipped last, so C never releases the application onto unfinished
pixels.**

Spelling out the two failure modes this closes:

- **Stale (whole previous frame).** Today the application is released by the head before its pixels
  land. With the fix, the application is released by the feedback word, which vkr writes only at
  completion and which S ships after the readback pixels. Released too early is impossible: the word
  it waits on does not exist until the pixels do.
- **Torn (partial).** A poll that samples the readback buffer mid-DMA ships torn bytes — but it does
  **not** ship the feedback word, because vkr has not written it yet. Later polls ship the corrected
  bytes, and the poll that finally ships the feedback word ships the final readback bytes with it (in
  the same batch, readback first). The application reads only after the feedback word installs, by
  which point every readback byte is the finished frame.

The `happens-before` edge the polling return path could never manufacture (note §2) is now supplied by
Mesa's own fence-feedback contract: `write(feedback word) happens-after GPU-complete`, and S preserves
it across the wire by ordering the feedback word last.

## 5. What is deferred, and the one place this is not yet robust

This is a walking skeleton. Named honestly, the gaps:

- **No buffer versions / generations.** If an application overlaps frames (submits frame N+1's work
  before reading frame N's readback), a single readback buffer's bytes could be mid-rewrite when the
  fingerprint moves. The icosa fixture is strictly synchronous (one fence per frame, read before the
  next submit), so this cannot arise for the fixture that gates the fix. A general application needs
  the immutable-generation machinery of note §6. **This spec's correctness claim is scoped to the
  synchronous one-fence-per-frame case.**
- **No explicit `FENCE_COMPLETE` message.** The completion signal rides the existing `BlobData` path.
  That works because S can *observe* vkr's feedback write in its own mapping. If a future engine or
  configuration wrote the feedback through a path S does not map, this would need the explicit message
  (note §6). The fallback is designed but not built.
- **Per-poll fingerprinting cost on S.** Bounded (a strided hash), but real, and additive to the
  byte-grain fragmentation already measured (`../c1-the-network.md` §3.1). Both are volume concerns
  for a hardening pass, not correctness.
- **The reply arena still ships on every command.** Unchanged by this spec.

## 6. Implementation, in order

**Step 0 — de-risk the load-bearing assumption first.** Before any other change, confirm S's blob
diff observes vkr's write to the feedback buffer. Concretely: un-pin `no_fence_feedback` in the icosa
test only, add a temporary log where `take_blob_writes` reports a run for a Venus-internal blob, run
the fixture, and confirm a Venus-internal blob's bytes change *after* the application blocks (i.e. with
no ring traffic). If they do, the design holds. If they do not, stop and switch to the explicit
`FENCE_COMPLETE` fallback before writing the rest. This step is throwaway and is reverted.

**Step 1 — S ships on fingerprint change.** In `progress_thread`, add the every-poll fingerprint gate
and the fingerprint-gated `take_blob_writes` ship, alongside the existing retirement-driven path. This
needs a fingerprint over **all non-ring blobs** (not `fingerprint_written_blobs`, whose `s_written`
scope has the bootstrap problem of §3.2) — add an `Applier` method that fingerprints every blob except
the rings, and reuse `take_blob_writes` for the actual ship. Keep the app-first/venus-last ordering.

**Step 2 — un-pin `no_fence_feedback`** in the icosa test launch, and update the crutch-table
documentation (spec §6 / the design note) to drop it from the recommended set.

**Step 3 — verify.** Run `icosa_cpu_renders...` at least **three** times (it is a race; one pass is
not evidence). Every run must be 120/120. Run once more with `RAYLAND_C1_TRACE=1` and confirm via
`scripts/c1-trace-analyze.py` that Probe A no longer fires (no readback blob changes after its final
ship) and that the feedback word's install (T7 on C) precedes the application's read.

**Step 4 — self-review and update the binding docs.** If any statement in `CLAUDE.md` or the design
note is made false by the change, fix it in the same commit (the repository's standing rule).

## 7. Success criteria

- `icosa_cpu_renders_across_the_network_the_same_120_frames_it_renders_natively` passes **120/120 on
  three consecutive runs**, with `no_fence_feedback` no longer set for that application.
- The refapp single-frame test still passes.
- `cargo clippy --workspace --all-targets` is clean; the unit and no-GPU-linkage tests pass.
- No `rayland-c` source change was required (if one turns out to be, that is a finding worth recording,
  because this spec predicts none).

## 8. Fallback if step 0 fails

If S cannot observe the feedback write in its own mapping, build the explicit signal instead:
`S2C::FenceComplete { ctx_id, ring_idx, fence_value }`, emitted by S when it can determine the fence
retired, and applied on C by writing `fence_value` into the feedback buffer at the offset Mesa polls.
This pulls forward part of note §6 and needs C to identify the feedback buffer and its layout — which
is why it is the fallback, not the plan. It is recorded here so the pivot, if forced, starts from a
design rather than a blank page.

## 9. Implemented and it FAILED — the cross-resource visibility wall (2026-07-18)

The plan ([`../superpowers/plans/2026-07-17-fence-feedback-walking-skeleton.md`](../superpowers/plans/2026-07-17-fence-feedback-walking-skeleton.md))
was executed. Tasks 1 and 2 landed as designed (the `fingerprint_nonring_blobs` helper; the de-risk
spike confirmed S can *see* a non-ring blob change with no wire notification). **Task 3 — the fix
itself — was implemented verbatim and made the defect dramatically worse:** the reproducer went from
~28/120 wrong (unfixed) to **119/120 and, on a traced run, 120/120 wrong.** The code is committed as a
documented negative result; the tree left in the feedback-enabled state so the follow-up spike builds
on it.

**What the wrong frames are.** Not staleness (no frame equalled the *whole* previous frame) and not
garbage: the correct geometry and rotation for the right frame index, with the readback texture **torn
~25%** — a spatial split, roughly the left half pixel-exact and the right half a different-but-
plausible fractal state. The signature of a **partial readback shipped and then never topped up before
the application was released to read it.**

**Why the design was wrong.** §3–§4 assumed that *when S observes the feedback word appear, the
readback pixels are already complete and coherent in S's view*, so shipping the readback just before
the feedback word is safe. That is false. S is a **third-party observer** of the app's GPU-written
memory — it maps the shmem and diffs it — and the two resources involved become visible **to S's
mappings in an order that does not track the GPU's real completion order**:

- the **readback buffer** is filled by GPU DMA;
- the **feedback word** is a CPU store by the render server's own thread, once *it* learns the fence
  retired.

The feedback store becomes visible to S's mapping *before* the DMA'd readback bytes finish becoming
visible to S's separate mapping of that other resource. So the poll that sees "feedback changed" reads
a still-incomplete readback in the same breath, ships both (readback first, feedback last — the wire
order was verified perfect: `T7 before T6: 0` across 1.59M packets), and releases the app onto torn
pixels. Task 2's spike proved changes are *eventually* visible to S; it never proved they are visible
in the right *relative* order between two resources — and that gap is the whole failure.

The deeper statement: **the application gets correct pixels natively because *it* waits on its own
fence, which is what establishes cache-coherency to *its* CPU. S never holds that fence→coherency
relationship — it is a bystander — so no amount of watching memory from outside manufactures the
coherency edge.** The return-path note's §2 ("a sampled property of a polling system cannot
manufacture a happens-before") turns out to bind not just *detecting* the write but the *coherency of
the bytes themselves*.

**One encouraging data point for the follow-up.** Probe A on the failing traced run still fired ~once
per frame, showing the readback buffer continuing to change **1.5–62 ms after** S declared it shipped.
So the correct bytes *do* reach S's view — just late, and after the feedback word already released the
app. That suggests the problem may be **relative timing** (feedback visible before readback settles),
not a permanent coherency loss — which, if confirmed, points at a fix that holds the feedback-word ship
until the readback has settled *after* the feedback signal (a bounded wait, because the feedback word
means the GPU is genuinely done). The next step is a spike that measures exactly this relative timing
before any more delivery code is written.

## 10. Corrected root cause — a coherency problem, from reading the source (2026-07-18)

> **Partly superseded by §11 (2026-07-18).** This section's fix direction — "read the resource through
> `virgl_renderer_resource_map` (option 1)" — was tried and is a **red herring**: the accessor does not
> fix it and contends worse. The real missing ingredient is a **GPU fence before the read**, and the
> real wall is that fence contending with the message-thread doorbell. Read §11 before acting on §10's
> "fix direction". The source facts §10 cites are still accurate; the *conclusion drawn from them* was
> not.

§9's "encouraging" guess (relative timing; the readback settles late but eventually correct) was
**refuted** by a spike: holding the ship until the non-ring blobs settled, swept from 1 ms to 50 ms of
extra wait, changed nothing (119–120/120 wrong at every threshold, no trend). The correct complete
readback **never** appears in S's view, so it is not a matter of waiting.

Reading the Mesa Venus + virglrenderer source (versions on this box: virglrenderer 1.2.0, Mesa 26.0.x)
gives the real mechanism, and it also refutes the `cad5600` commit message's claim that the feedback
word is a host CPU store racing the readback:

- **The readback blob is a genuine *shared* mapping of the app's real GPU `VkDeviceMemory`** — the fd
  handed over `SCM_RIGHTS` is a real `vkGetMemoryFdKHR` export (`vkr_device_memory.c:582-594`), mmap'd
  `MAP_SHARED` (`vn_renderer_vtest.c:685-687`). No wire command ever carries pixel bytes
  (`vn_renderer_vtest.c:340-349`); there is no copy/transfer model (no `VCMD_TRANSFER*` exists).
- **The vtest path carries no cache-coherency metadata and issues no flush/invalidate, anywhere.**
  `vtest_bo_flush`/`vtest_bo_invalidate` are literal no-ops (`vn_renderer_vtest.c:650-666`); the host's
  `vkFlushMappedMemoryRanges`/`vkInvalidateMappedMemoryRanges` dispatch is `NULL`
  (`vkr_device_memory.c:479-480`); the host even computes the memory's cache type
  (`CACHED` vs `WC`, `vkr_device_memory.c:515-528`) and then **drops it**, because the vtest reply has
  no field to carry it. Correctness rests entirely on the pages being hardware-coherent.
- **That coherence is only proven for one consumer: the app itself** — mapping via `vn_renderer_bo_map`
  and reading *after its own `vkWaitForFences`*. That is exactly C0's bit-identical result. **S is a
  second, independent `mmap` of the same fd by a process with no part in the Vulkan submission or the
  fence**, and the protocol guarantees it nothing. The feedback word (a single-cache-line 4-byte
  `vkCmdFillBuffer`) happens to become visible to S; the megabyte, multi-page readback does not — the
  classic signature of write-combined / GPU-cached `HOST_VISIBLE|HOST_COHERENT` memory read by a
  foreign mapping with no invalidate. Nothing in the stack will ever issue that invalidate, so it never
  converges. Full trace: `scratchpad/venus-memory-model-findings.md`.

**So the defect is not timing and not ordering; it is that S reads the app's GPU memory through a raw,
unsynchronized foreign `mmap` that has no coherency contract.** The "observe the app's memory by
diffing it" strategy is the thing at fault for readback, not any particular gating scheme on top of it.

**Fix direction (grounded in APIs the source exposes), in order of preference:**
1. **Read the resource through virglrenderer's own accessor, not a raw fd `mmap`.**
   `virgl_renderer_resource_map` (`virglrenderer.h:423`) is the library's blessed CPU view of a
   resource it created, and it is the party that *knows* the dropped `map_info` cache type — so it can
   map with the correct caching attribute a raw `mmap` cannot. S already embeds virglrenderer; it
   should use this for its own host-side reads.
2. **Obtain the bytes through an engine-side GPU transfer** into a buffer S's own driver calls
   allocated — making S a proper fenced consumer, the way C0's `read_back` already works — rather than
   a foreign passive reader.
3. If a raw mmap is kept, wrap every read in `DMA_BUF_IOCTL_SYNC_START/_END` (valid on the dma-buf fd
   regardless of what vtest does) — but only if the fd is actually a dma-buf.

The next experiment is option 1: it is small, principled, and either fixes it or proves readback must
go through the engine (option 2).

## 11. Option 1 tried, and the honest conclusion: readback is (c)2, not a (c)1 tweak (2026-07-18)

§10's option 1 was implemented and measured on the GPU. The result retires the whole "patch the
observe-and-diff return path" line of attack:

- **`virgl_renderer_resource_map` is a red herring.** It succeeds on the readback blob (the spike
  proved that), but with the real fix in place it does **not** improve correctness over a plain raw
  read, it lags the small feedback word, and its per-blob engine lock **contends worse**. The
  accessor was never the missing piece.
- **The missing piece is a GPU fence before the read.** A passive wait alone never converged (the §5
  settle sweep, 1–50 ms, all wrong); a raw `MAP_SHARED` read taken *after a retired GPU context
  fence* is (mostly) coherent. So §10's "foreign mmap has no coherency contract" over-stated it: the
  raw read tears when taken **concurrently** with the GPU's writes, and a fence both waits for
  completion and triggers the coherency flush. This reconciles with §2's original `T2 < T4` — it was
  a completion/coherency race all along, and the fence is what closes it.
- **But the fence cannot be made both correct and live in this architecture.** virglrenderer is
  process-global under one lock. The fence-wait either **holds** that lock and starves the message
  thread's per-delta doorbell (`engine.submit`) → S's ring thread parks → Mesa aborts with a
  ring-stall **SIGABRT**; or it is **polled with the lock released**, and the added latency times the
  application's `vkWaitForFences` out. Splitting the wait into create/poll did not resolve it.
- **The correctness win was real but could not be made to *run*.** In the rare completing runs the
  implementer saw `wrong=0` (no torn/stale frames). But an independent re-run of the committed
  approach was **0 for 7** — 4 SIGABRT, 3 timeout, zero completions — so even the correctness result is
  not reproducible end-to-end. The exploration diff is preserved at
  `.superpowers/sdd/phase1-exploration.patch` (git-ignored scratch); it was **reverted**, not
  committed, because it mostly crashes.

### The conclusion

**App-initiated readback cannot be fixed by patching the "S passively observes and diffs the app's
memory" return path.** Five attempts, each hitting a different wall (early release → torn cache →
foreign-mmap incoherence → fence-vs-doorbell contention), are the evidence that the *architecture* is
wrong for this case, not the tuning.

The right shape is **§10's option 2**: S stops being a foreign memory-watcher and becomes a **proper
fenced engine-side consumer** for the readback — copy the app's blob into a resource S's own driver
owns and read it back with `virgl_renderer_transfer_read_iov`, exactly the mechanism C0's `read_back`
already proves bit-identical. That removes the foreign-observer coherency problem *and* changes the
contention picture (the readback happens while the app is blocked on its fence and the ring is
quiescent). It needs blob→classic-resource support that virglrenderer 1.2.0 does not expose for the
vtest blob path, and careful doorbell orchestration — a design cycle, not a patch.

**Therefore readback is rescoped to (c)2** (the "apps map GPU-written memory and read it back"
problem), whose first step is investigating option 2's reachability on this virglrenderer. (c)1's
delivered scope is the **forward path** (unmodified app commands C→S, executed on S's GPU,
bit-identical on trivial workloads) and **presentation** — both of which work. The stale/torn readback
is a known, documented limitation handed to (c)2, not a regression in what (c)1 set out to prove.
