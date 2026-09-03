# Report to the planning session — Rayland, 2026-09-03

**Supersedes `REPORT-2026-09-02-to-planning.md`**, whose "short version" describes a soak that was
still running. Written for the Claude.ai session that plans this work and cannot see the repository.
Self-contained: every number is stated with what produced it.

Branch **`wp0-wayland-proxy`** at `7d32b2f`, on GitHub and Forgejo. `main` is 40+ commits behind;
merging remains the owner's call. **15 commits, +3,471 lines** since the continuation prompt.

---

## 1. What the prompt asked, and what happened

`CONTINUATION-2026-09-03.md` handed over three threads and recommended thread 1: one night of soak to
settle whether Venus's semaphore/event/query feedback — worth a measured 1.23× — is safe to enable.

**That question is now answered.** It took two soaks, and getting there consumed most of the session,
because the instrument could not have answered it.

| | result |
|---|---|
| feedback arm (`no_multi_ring,no_fence_feedback`) | **400 clean / 400** |
| shipping arm (all five flags off) | **400 clean / 400** |

**Fisher exact p = 1.0 — no detectable difference.** Each arm < 0.75% at 95% (rule of three). Pooled
with earlier work: shipping **0 in 880**, feedback-on **1 in 492**. Both verified per-attempt (400
contiguous files, every one `rc=0 frames=120 cores=0`), not from the summary line.

### 1.1 What that settles, and what it does not

**Settled:** enabling semaphore, event and query feedback does not measurably harm reliability. That
was the *only* stated reason for keeping them off.

**Not settled — and this is the part a plan must carry:** the **1.23× has never been measured where it
would be claimed**. It is an `icosa-gpu` figure taken over **loopback**; both soaks ran `icosa-cpu`
over a **network**. The reliability question and the speed question have been answered on different
workloads and different topologies, and only one of them is answered.

**So the next step is not adoption — it is re-measuring 1.23× on the real network.** If it comes back
1.03×, as `ship()`'s batching did, the thread closes for a better reason than fear. This is the single
cheapest high-value experiment now on the board, and the machines are free.

**The original 1-in-92 remains unexplained.** What changed is that one unattributed loss now sits
against 400 clean runs of the arm it was charged to *and* a matched control, so "feedback causes
session loss" is a poor fit and a one-off is a good one.

---

## 2. Why the answer took two soaks: the instrument could not have produced it

**The queued command did not work, and had not for weeks.** Every document, the handover, and the
harness's *own usage line* set `VN_PERF_SETTING`; the script read `VNPERF`. The variable selecting the
arm was connected to nothing. A night on that command would have measured the **shipping** arm and
labelled it the feedback arm.

Looking for its siblings found **eight more**, across two rounds:

| # | Defect | What it would have produced |
|---|---|---|
| 1 | arm selector read a different variable than every document sets | the wrong arm, confidently labelled |
| 2 | never rebuilt; pointed at a target dir nothing else writes | binaries 26 commits stale |
| 3 | no provenance in the output | a 400-run result nobody can attribute |
| 4 | S's GPU unpinned (only harness of five) | NVIDIA loses the device ~half its runs → scored as arm failures |
| 5 | every run's S log truncated by the next | a rare failure's evidence lost |
| 6 | `scp` deploy failure unchecked | **printed "6 clean, 0 failed" for a build that never reached C** |
| 7 | a `rayland-c` leaked every iteration | 400 daemons a night on one socket; cause of 6 |
| 8 | S's port not released before the next bind | **setup failures scored as failures of the arm** |
| 9 | C unreachable also scored as an arm failure | **voided an entire 400-run control sweep** |

Three were caught by *watching them happen*, not by reading code. Defect 6 announced itself: `scp` died
with `ETXTBSY` and the loop reported six clean runs for a build it had failed to deliver.

**Defect 9 deserves its own note, because it was a hole in my own fix for 8.** Having found that the
harness scored its own setup failures against the arm, I fixed exactly one side of the link — abort
when *S* fails to start — and never asked the symmetric question about *C*. Sixteen hours later the
LAN was migrated onto a VLAN mid-sweep (apollo → `172.16.20.10/24`, S left on `192.168.1.0/24`) and
the harness invented **365 failures** of the shipping configuration. Only the absurdity of the number
prompted a look. Fixing the instance you watched, and writing the general rule in a comment above it,
is not enforcing the general rule.

**The rule now enforced on both sides: a harness may lose a run; it may never invent one.** And since
the link has dropped twice in two days, there is a third option between inventing a run and losing the
sweep — **re-attempt it**, bounded and counted. Both the retry and its cap were verified by induction:
holding the relay port made it retry twice and then abort with `Exhausted 2 setup retries`, and the
aborted run reported `0 clean, 0 failed, out of 0`.

The first control-arm attempt aborted at attempt 16 on a transient drop and reported
`PARTIAL RESULT: 15 clean, 0 failed, out of 15`. That is the same event that produced 365 fabricated
failures two days earlier. **The fix is worth more than the result it was collecting.**

### 2.1 A peer session reviewed the work and found nine more things

`rayland-f9` reviewed five commits without touching the tree. All nine findings verified and fixed
(`aa61f50`). Two were my own fixes from the same morning. The most valuable was a correction it made
to *itself*, which corrected me: I had "fixed" a hardcoded `S_IP` by deriving it — and `c1-sweep.sh`
has documented since July that `.192` is WiFi and `.150` wired, with the historical `0/480` taken on
`.192`. **Deriving would have moved future runs onto a link with 18× less latency and silently
unpooled them from every prior figure.** A fix that improves a harness and quietly invalidates its own
history is worse than the hardcode.

Also fixed: `rayland-c` could never dump a core (`ulimit` applied one line *after* the daemon
launched, and apollo's limit is 0) — so "no core was produced" during the feedback hunt was always
true of the *application* and never said anything about the daemon.

---

## 3. Where the project stands — the arc, not the session

### 3.1 The thesis is proven, four times, by unmodified applications

| Application | What it proves | Evidence |
|---|---|---|
| `vkcube` | the baseline | apollo→dop561 **and milkv→dop561**, spinning, human-confirmed |
| `vkgears` | a *second independent* app | milkv→dop561, 659/621/577/583 attaches in 30 s, 4 of 4, zero stalls |
| `solarsim` | an unmodified **wgpu/winit** toolkit app | milkv→dop561, real desktop, 169 frames |
| `icosa-cpu`/`-gpu` | **correctness**, not liveness | **bit-identical to native**, 0 differing frames in 1,200 |

Two display paths, and the distinction matters: `vkcube`/`vkgears`/`solarsim` go through **WP0** (the
app's own Wayland session proxied, swapchain named by a `BufferToken`, zero-copy dma-buf); the icosa
demo goes through **(c)1/(c)2 presentation**, which WP0 exists to retire.

### 3.2 On a capable C, native frame rate over a real network

dionysus → dop561: **25 ms median inter-frame gap, 8 runs of 8.** Native `vkcube` on the same
compositor with no Rayland: **25.39 ms**. The relayed application sits on the compositor's repaint
timer, not on Rayland's cost. The same blob diff over the same bytes is **17.4%** of wall clock on
dionysus and **56.8%** on milkv — that contrast is the project's premise.

### 3.3 No pixels cross the network — false on every run until measured

S→C fell from **~877 KB/frame to ~1.9 KB** (A/B'd inside one binary: 307,776 → 219 B/frame, **1,406×**).
It survived because the display was *already* zero-copy, so the waste had no symptom and every test
passed — while the harness header asserted the opposite.

### 3.4 The ~1 s mouse-crossing stall is fixed

Stock `vkcube`: 178 configures → 92 rebuilds → **9 stalls, worst 1,117 ms**. Patched: 392 configures →
**1 rebuild, 0 stalls**, worst 104 ms, **4.3× more frames**. The fix is in `vkcube`, not Rayland —
and it is now **submitted upstream: [Vulkan-Tools#1250](https://github.com/KhronosGroup/Vulkan-Tools/pull/1250)**,
open, awaiting the owner's CLA click. **Resizing still stalls (5.1 s, 4.7 s on milkv) and that one IS
ours** — a legitimate rebuild, no application bug to patch. Unfixed.

### 3.5 Performance, including what did not pay

Presented-resource exclusion **1,406×**; `DIFF_CHUNK` **1.48×** on milkv (clean null on x86_64);
`reply_arena_fence_signaled` lock-held **8×/16×**; forward coalescing **6.1×** fewer messages;
readback coalescing ~5000 → ~180 msgs/frame; teardown `SIGABRT` ~21% → **0**.

**The honest half:** the two 5–8× mechanism fixes each moved the median frame gap by **nothing
measurable**. Frame time is a serialized latency chain; what remains is the *number* of round trips
and each one's fixed cost. Both poll intervals swept factorially — neither is the term.

---

## 4. (c)4 is scoped, specced, planned, and one-third built

`(c)4` was one roadmap line: "real/complex applications; GL via Zink". **Scoping split it in two, on
evidence**, and the split was forced by a measurement.

**The acceptance application had to change, and the reason was a wrong inference of mine.** `rt` — the
owner's terminal — was chosen partly because I called it "wgpu/winit like solarsim". It uses `winit`
for windowing, but `choose_backend` returns `BackendKind::Gl` for any non-X11 display, and its trace
holds **zero** Vulkan lines against 2,143 on Mesa's EGL queues. **`rt` is an OpenGL application and
cannot run over Rayland until Zink exists** — which would have made (c)4b a prerequisite of (c)4a's own
acceptance test.

**The measured gap**, by tracing `wl_registry.bind` against the live compositor:

| | renderer | binds | WP0 offers |
|---|---|---|---|
| `solarsim` | Vulkan | **19** | **5** |
| `rt` | GL/EGL | 25 | 5 |

Both are `winit` apps with heavily overlapping lists, so this specifies the *toolkit's* demand.

**The framing that matters, and which I first got wrong:** those fourteen are **not** silently skipped.
C never advertises them, so the application adapts — correct Wayland behaviour. The defect is that the
absence is *unrecorded*, so a deliberate omission is indistinguishable from an oversight. `solarsim`
has been running without display scale, decorations, cursor shape, fractional scaling or presentation
timing, and nothing said so.

**Root defect:** the supported set is written down **twice** — C's `create_global` calls over
`wayland-server` descriptors, S's `interface_by_name` over `wayland-client` ones. *That drift is the
`wl_shm` bug*: added to C, forgotten in S, detected by a human noticing a missing cursor.

**Built so far (4 of 12 tasks):**

- **Task 1** — one shared `SUPPORTED` table in `rayland-relay` with an `FdPolicy` per interface.
- **Task 2** — S held to it in both directions. **Verified by mutation:** deleting the `wl_shm` arm
  reproduces the 2026-09-01 bug as a named test failure. The old test enumerated the names it
  expected, so it could only catch a name someone remembered to add in two places at once.
- **Task 3** — C advertises from the table and **logs what it withheld, with the reason**.
- **Task 4** — the **bind-gap report** (`scripts/wp0-bind-gap.sh`), which independently reproduced the
  spec's gap of **14**, name for name. It refuses to run against a headless compositor and fails
  loudly on zero parsed binds.

Remaining Tasks 5–10 add the interfaces in priority order, gap `14 → 12 → 11 → 10 → 8 → 7 → 1`. The
final `1` is `wp_linux_drm_syncobj_manager_v1`, `Refused` by design (a DRM syncobj fd — cross-machine
explicit GPU sync needs its own design).

Spec: `docs/superpowers/specs/2026-09-02-c4-protocol-breadth-design.md`.
Plan: `docs/superpowers/plans/2026-09-02-c4a-protocol-breadth.md`.

---

## 5. Infrastructure, unplanned but real

**Forgejo moved to a Synology.** Push works; both remotes hold `7d32b2f`. **It is unreachable as of
this writing** — neither `forge.lan` nor `git.stationoost.eu` resolves and both known IPs are dead.
GitHub has everything.

**Woodpecker CI was broken by the move and is fixed.** Root cause: milkv and dionysus trusted the
*old* Caddy local root; the Synology's Caddy has its own — with the **identical CN**, differing only
in fingerprint and issue date. Verified end-to-end by a fully green pipeline. A second, independent
defect: **dionysus was a half-provisioned agent** (registered with only `agent.conf`), making every rt
pipeline a coin flip. Now provisioned to match apollo, with all four gate steps verified to run.

**The network has changed topology three times in two days.** apollo moved to `172.16.20.10/24` and
back; dionysus's DNS is stale (`.148` vs the real `.65`); the forge moved twice. **This voided a
400-run sweep and is the single biggest operational risk to any future measurement.** The soak harness
now records S's address, what C sees, and the measured RTT per run.

---

## 6. What a plan must not re-propose

Carried forward, all still binding:

- **No `vkgears` riscv64 hang.** There is none; the instruments manufactured it.
- **Do not separate Mesa 26.0.8 from 26.1.6.** That confound explained a defect that does not exist.
- **No protocol-level filter for the blob scan.** Refuted with per-blob numbers: the 8 MiB staging pool
  is 68–85% of the scan and genuinely changes on 41–47% of deltas, shipping 500–600 bytes. Location
  comes only from the decoder (banned) or page tables. There is no third signal.
- **`fmt` failing in rt's CI is not a defect** — it is `failure: ignore` by design.

Added by this session:

- **Do not quote a soak figure without its arm, build and link.** The harness now prints all three.
- **`rt` cannot be an acceptance application for anything Vulkan** until Zink exists.
- **Neither soak can be attributed to a characterised link.** Both used `.192`, documented as WiFi in
  July but measuring 0.80 / 1.26 / 3.75 ms across three samples — spanning both classes. The path is
  variable; a single 5-ping sample cannot classify it. (I over-read one sample and said the label was
  stale; that was wrong, and the correction is recorded.)

---

## 7. Still open, ranked

1. **Re-measure the 1.23× on the real network.** Cheap, machines free, and it is the only thing
   between the feedback result and an actual decision. **Do this first.**
2. **(c)4a Tasks 5–10** — the interfaces, each with a measured gap reduction.
3. **The frame-time fork, still the owner's call.** Dirty-page tracking is the only mechanism that
   addresses the dominant cost (82% of the blob diff is `memcmp` over unchanged memory; **29% of
   `icosa-gpu`'s whole frame** on the board). Scan bandwidth is already at the board's memory
   bandwidth, so no constant is left to tune. But **riscv64 cannot do it** (kernel 5.15, no
   `CONFIG_HAVE_ARCH_SOFT_DIRTY`), while soft-dirty works on dionysus where the same scan is worth
   ~19%. *The machine that needs it least is the one that can have it.* If built, solve the
   `clear_refs` race first — a write between the pagemap read and the clear is **lost**.
4. **(c)4b — GL via Zink**, with `rt` as its acceptance app.
5. **Resize stalls** (5.1 s), **multi-queue**, and the **mesa-demos `fini_display` patch**, which is
   rebased and ready at `docs/patches/mesa-demos-vkgears-guard-null-seat.patch` but **cannot be
   submitted from this machine** — `gitlab.freedesktop.org` needs an account there are no credentials
   for. Half the original patch was already obsolete upstream; only rebasing revealed that.

---

## 8. Honest assessment

The session's planned deliverable was a measurement, and it produced two — but most of the time went
to the instrument, not the experiment. That was the right trade: run as documented at 01:00, the night
would have produced a clean-looking "0 failures in 400" for **an arm that was never selected**, on
**software five weeks old**, with **a GPU that fails half its runs unpinned**, on an instrument that
**scored its own setup failures against the hypothesis**. Nothing in the output would have said so.

Nine instrument defects, nine peer findings, and four document-vs-code contradictions were found in
two days. The recurring shape is always the same — *an output read without checking what produced it*
— and it caught me four separate times this session: a stale-binary assumption, a single-sample link
classification, a wrong inference about `rt`'s renderer, and markdown backticks inside a
double-quoted `ssh` string that ran `nohup` as a command substitution while the run still printed
"2 clean, 0 failed".

The project's measurement discipline is its real asset. It is also, on this evidence, the thing most
in need of the discipline.

---

*Session: https://claude.ai/code/session_0153BQRRKAJcRSxUxRuEHQQs*
