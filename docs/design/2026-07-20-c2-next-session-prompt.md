You're picking up **Rayland sub-project (c)2 — the GPU-readback return path over a real network**.
Rayland does native remote GPU rendering for Wayland: an app runs on machine **C** (apollo) and is
rendered/read-back on machine **S** (dop561, this host, which has the GPU), with **commands, not
pixels** crossing a QUIC network. (The S/C labels are X11-era and reversed from cloud usage — S is the
strong machine where the user sits; see `CLAUDE.md`.)

## Read first, in order
1. `docs/design/2026-07-20-c2-handoff.md` — the orientation for exactly this work: current state,
   what's shipped, the open problem, and the **two proven dead ends** (do not re-attempt them).
2. The docs it links — especially the **"Outcome" section** of
   `docs/design/2026-07-20-c2-readback-release-ordering.md`, and
   `docs/design/2026-07-17-return-path-completion.md` §8 +
   `docs/design/2026-07-19-c2-ringidx-decode.md` §8 (the fence `T2 < T4` background).
3. `.superpowers/sdd/progress.md` (the (c)2 sections, newest last) for the blow-by-blow ledger.

## State
The **readback-completion gate** is landed and pushed on `main`. An unmodified Vulkan app
(`rayland-icosa-cpu`) renders and reads back **10/11 runs perfectly clean** over the real network. A
**~1/11 residual** delivers the whole previous frame (`N == N−1`).

## The open problem (the hard half of the return path)
The residual is enabled by virglrenderer's completion fence **not reliably guaranteeing the readback is
host-visible when it retires** (`T2 < T4`). Two release-ordering fixes are already **proven dead ends**:
Direction A (hold the head-advance until after the readback) *regressed* — the step-2 "empty" state is
ambiguous between a copy submit (must release) and a draw whose DMA hasn't landed (must hold), because
of exactly this fence gap; Direction B (fence feedback over the network) deadlocks/SIGABRTs (a
pre-existing gap, not a gate bug). The handoff doc's "Where the next attempt should start" section lays
out the concrete next steps.

## Your task
Investigate the fence (`T2 < T4`) as the handoff doc directs. Start with **evidence, not a fix**:
1. Instrument S's `progress_thread` to quantify how often a *post-fence empty* readback poll is
   actually a draw whose DMA lands a few ms later (and the residual lag), env-gated and throwaway.
2. Understand whether `virgl_renderer_context_create_fence` (`crates/rayland-engine/src/virgl.rs`)
   FIFO-covers the app's readback submit or can retire earlier — read virglrenderer's
   `vkr_context_submit_fence` / render-server fence path.
Then, only once the mechanism is pinned, brainstorm a fix. Do **not** re-attempt Direction A/B as
designed; the content-based completion signal was also already tried and is race-prone (see the §8 doc).

## Working rules (important)
- **Never `pkill`/pattern-kill** — global rule; kill only by the exact PID you captured (`cmd & PID=$!`).
- Use **systematic-debugging** (root cause before fixes) and **brainstorm any design before building** it.
- Verify over the network with `scripts/c2-icosa-two-machine.sh 5`; run several batches for **≥20 runs**
  (the residual is ~1/11). `ssh apollo` works; this host is S. A single long invocation can be
  wall-clock-killed — use batches of ≤5.
- Don't add `VN_DEBUG=no_abort` (Mesa's ~3.5 s stall-abort is the stall detector).
- Judge the flaky residual over many runs, never one. **Watch for the Heisenbug:** per-poll logging on
  S slows it enough to hide the defect — measure carefully.

Begin by reading the handoff doc and confirming the current state (git log on `main`, and one clean/one
stale two-machine batch), then propose your first investigation step before changing code.
