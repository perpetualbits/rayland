# Next-session prompt — execute the (c)1 incremental blob-sync plan

**You are picking up mid-flight. This document is your entry point.** Read it fully, then read the files it
names. It is written for a fresh session with no prior context. Branch: `wp0-wayland-proxy`.

---

## 0. Orient yourself first (before touching anything)

Read, in order:

1. `CLAUDE.md` — binding conventions. Note the **S/C vocabulary** (S = strong machine: GPU, display,
   compositor; C = weak machine: the app runs here, links no GPU stack), the **doc-comment-on-everything /
   intent-comment-on-every-nontrivial-line** rules, the **diary rule** (add a `docs/DIARY.md` entry every
   working turn), and the **project-map rule** (keep `project-map.js` in sync when status changes).
2. `docs/DIARY.md` — the last ~15 entries (2026-07-24 → 2026-07-25) are this arc: WP0 Task 4.4 landing, the
   long vkcube-stall investigation, and the blob-sync design. Read at least the four dated 2026-07-24/25 about
   the stall (they converge on "it's blob-sync bandwidth, not a bug") and the 2026-07-25 design entry.
3. **`docs/design/2026-07-25-c1-incremental-blob-sync.md`** — the design spec for the work you are about to
   execute. Read it fully; it is short and self-contained.
4. **`docs/superpowers/plans/2026-07-25-c1-incremental-blob-sync.md`** — the implementation plan. Three tasks,
   each with literal code and TDD steps.
5. `.superpowers/sdd/progress.md` — the master project ledger (git-ignored). Its tail has the full
   investigation trail behind the design. **Trust it over recollection.**

Then `git log --oneline -15` and skim the `wp0(...)` and `c1:` commits.

---

## 1. Where the project is (one paragraph)

Rayland runs an unmodified Vulkan app on **C** and renders it on **S** by relaying GPU commands (not pixels)
over the network. **WP0** put a Wayland proxy on C so a real app (vkcube) can present through S's compositor.
**WP0 Tasks 4.1–4.4 are done and correct** — the forward command/Wayland tunnel and the compositor→app event
return path all work; vkcube receives and acks its `xdg_surface.configure`. The one thing between that and a
live vkcube is **not a WP0 bug**: it is a (c)1 performance debt. That is what this plan fixes.

---

## 2. What was found (the whole reason this plan exists)

vkcube "hangs." It was root-caused exhaustively (the diary/ledger have the full chain; a précis):

- It is **not** a deadlock, **not** a WP0 bug, **not** the C-side release path (all Acks `Advanced`, C
  publishes head correctly), **not** a doorbell/park race (the ring executes — S's `head` keeps up with the
  app's writes), **not** delayed ACK (tested with quinn's immediate-ACK extension — no change), **not**
  `initial_rtt` (tested — no change). All those transport experiments were reverted; nothing shipped.
- It **is** a **blob-sync bandwidth explosion.** (c)1's C→S sync ships the *full contents of every application
  blob, whole, on every ring relay* (the deliberately dumb-but-correct v1 strategy, (c)1 spec §7 — Venus gives
  no signal for which bytes changed). vkcube has many large blobs; each of ~100 setup relays re-ships all of
  them. Measured (`RAYLAND_C1_METRICS=1`): **`c2s_blob_sync_bytes = 16,574,464` vs `c2s_ring_bytes = 22,955`**
  — 99.9% of C→S traffic is re-sends, growing to **2.28 MB per relay** with a single send blocking **2.9 s**.
  That is the entire "hang."

---

## 3. What to do — execute the plan (this is the whole task)

The fix is scoped, designed, and planned: **send only what changed.** C keeps a per-application-blob baseline
(its copy of what S holds), diffs the live mapping against it on each relay, ships only the changed byte-runs
(reusing `C2S::BlobData`'s existing `offset` field — no wire change), and folds S's return-path writes into the
baseline so C never ships S's own bytes back. It mirrors the proven S→C diff (Task 5b).

**Method: subagent-driven development** (the previous session chose this). The plan has three tasks:
- **Task 1** — `LocalBlob` baseline + `take_changed_runs`/`note_s_wrote` in `crates/rayland-c/src/shm.rs`
  (pure unit tests, no GPU).
- **Task 2** — `messages_for_delta` ships changed runs in `crates/rayland-c/src/blob_sync.rs`.
- **Task 3** — re-baseline on inbound `BlobData` in `crates/rayland-c/src/main.rs`, then the **loopback e2e as
  the correctness gate** (`rayland-s/tests/loopback_e2e.rs` — refapp + icosa must stay **bit-identical**), plus
  a before/after C→S-blob-bytes measurement.

**Setup already done for you** (from the previous session): `superpowers:subagent-driven-development` was
invoked, the plan's SDD workspace + ledger + Task 1 brief were created at
`.superpowers/sdd/2026-07-25-c1-incremental-blob-sync/` (ledger `progress.md` has only the identity line — no
task is complete, so **start at Task 1**). Re-invoke the skill; re-running its `sdd-workspace`/`task-brief`
scripts is idempotent. **No implementer subagent has been dispatched yet** — nothing is mid-flight.

Model note: the plan text carries complete code, so Task 1/2 implementers can run cheap–mid tier; Task 3
touches the e2e (judgment) — mid tier. Reviewers: mid tier scaled to the diff.

---

## 4. Gates and gotchas (do not skip)

- **BUILD TARGET (mandatory):** prefix every cargo call with
  `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target`. The default `/tmp` target is a tmpfs with a
  per-user quota; filling it makes the linker die with a bare **SIGBUS** (`collect2: ld terminated with
  signal 7`). If you see that, it's `df`, not your diff.
- **The correctness gate is the loopback e2e staying bit-identical** (Task 3 Step 3,
  `cargo test -p rayland-s --test loopback_e2e`, GPU-backed, ~6 min). A dropped or mis-offset run surfaces as a
  wrong pixel here. If any frame differs, the diff is losing bytes — do **not** weaken the e2e.
- **`rayland-c` must never link a GPU stack** (`tests/no_gpu_linkage.rs`). This change adds no dependency
  (`BlobRun` is in `rayland-relay`, already linked). Keep it that way.
- **This machine (dop561) is S:** it has the GPU (`/dev/dri/renderD128`) and a live compositor
  (`WAYLAND_DISPLAY=wayland-2`), so the e2e and any vkcube smoke run here on loopback.
- **The vkcube bandwidth measurement (Task 3 Step 4)** is a measurement, not a gate — record before/after
  `c2s_blob_sync_bytes` in the diary, don't block on an exact number. To reproduce the smoke, a working
  `wp-smoke.sh` exists under the previous session's scratchpad
  (`/tmp/claude-1000/.../scratchpad/wp-smoke.sh`, already carries the required
  `VN_PERF=no_multi_ring,no_fence_feedback,...` flags); if gone, the recipe is in
  `docs/design/2026-07-24-wp0-task4-next-session-prompt.md` §4.
- **PROCESS DISCIPLINE (hard rule, user's global CLAUDE.md):** NEVER `pkill`/`killall`/pattern-kill — a
  pattern once killed the user's Chrome *and* the Claude session. Launch daemons with `setsid`, capture the
  exact PID, and group-kill only that PID (`kill -TERM -- -"$PID"`). Verify no leftovers with `ps` by
  inspection, never by killing. **Never add `VN_DEBUG=no_abort`.**
- **Diary + ledger every working turn**; teeth-check every test (watch it fail, or break the thing under test
  and confirm the test catches it) before trusting green.

---

## 5. After the plan lands

When all three tasks pass and the final whole-branch review is clean, `superpowers:finishing-a-development-branch`
decides the merge. Then the remaining known follow-ups (all separate from this work):
- **Remote `vkMapMemory`** — a blob genuinely rewritten every frame (icosa-cpu's megabyte) still ships whole;
  that is **(c)2**, not this.
- **Content-addressing / dedup** (the fingerprint idea, identical content crossing once) — **(c)3**.
- The **readback fragments into ~5000 one-byte BlobData/frame** bandwidth follow-up and **multi-queue** — still
  open from (c)2.

WP0 4.3 (token → `wl_buffer`, zero-copy present) and a live vkcube proof sit downstream of this blob-sync fix,
because the app has to reach steady state without burying the link first.

---

## 6. Key files & commits

| File | Role |
|---|---|
| `docs/design/2026-07-25-c1-incremental-blob-sync.md` | the design spec |
| `docs/superpowers/plans/2026-07-25-c1-incremental-blob-sync.md` | the implementation plan (execute this) |
| `crates/rayland-c/src/shm.rs` | `LocalBlob` — add baseline + diff (Task 1) |
| `crates/rayland-c/src/blob_sync.rs` | `messages_for_delta` — ship runs (Task 2) |
| `crates/rayland-c/src/main.rs` | `apply_blob_data` — re-baseline (Task 3) |
| `crates/rayland-c/src/link.rs` | `record_send` classifies `BlobData` as `blob_sync` (metrics — unchanged) |

Recent commits: `git log --oneline | grep -E "c1:|wp0\("`. HEAD is the plan commit (`c1: implementation
plan …`). WP0 4.4 + the blob-sync spec/plan are all on `wp0-wayland-proxy`, pushed to origin.
