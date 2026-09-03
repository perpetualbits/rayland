# Report to the planning session — Rayland, 2026-09-02 (night)

> **SUPERSEDED by [`REPORT-2026-09-03-to-planning.md`](REPORT-2026-09-03-to-planning.md).** Kept
> rather than deleted, per the house rule that a retired document stays visible as retired. Its "short
> version" describes the feedback soak as still running at 53 of 400; it finished at **400 clean / 400**,
> and the matched control arm has since also returned **400 clean / 400**. Read the newer report for
> anything you intend to act on.

Written for the Claude.ai session that plans this work and cannot see the repository. Self-contained:
every number below is stated with what produced it. Branch is **`wp0-wayland-proxy`**; `main` is 40+
commits behind and merging remains the owner's call.

---

## The short version

The session was handed three threads and told to take them in order. **Thread 1 — the overnight soak
that settles the Venus feedback question — was not run as planned, because the instrument that was
supposed to run it could not have answered the question, and had not been able to for weeks.**

The session therefore went: check the command → find it broken → find seven more defects in the same
harness → repair and verify → launch the real soak. It is running now. Separately, and not planned,
the owner asked for a test of the Forgejo server that had just moved to a Synology, which turned up a
CI breakage that is now fixed on three machines.

**What is not yet delivered: the answer to the feedback question.** The soak is at 53 of 400 runs, all
clean. It finishes in roughly 2.8 hours. Everything below it is instrument repair and infrastructure —
real work, but not the measurement the night was for.

---

## 0. What has actually been achieved — the arc, not the session

*Added after the first draft of this report was correctly criticised for scoping itself to one
night's work and omitting the results that matter. The section above describes an evening; this one
describes where the project got to.*

### 0.1 The thesis is proven, four times, by unmodified applications

**An application runs on a weak remote machine, is rendered by S's GPU, and appears in its own window
on S's real desktop — with commands crossing the network, not pixels.**

| Application | What it is | Status |
|---|---|---|
| `vkcube` | the standard Vulkan demo | apollo→dop561 **and milkv→dop561**, window on screen, spinning |
| `vkgears` | mesa-demos, a *second* independent app | milkv→dop561: **659/621/577/583 attaches in 30 s**, 4 runs of 4, gap 41–47 ms, zero stalls |
| `solarsim` | an unmodified **wgpu/winit** application — a real toolkit app | milkv→dop561 on the real desktop, 169 frames in ~120 s |
| `icosa-cpu` / `icosa-gpu` | our own fixtures | **bit-identical to native**, 0 differing frames in 1,200 over 10 runs with the board as C |

**Two different display paths, and the distinction matters to a planner:**

- **WP0** — the application's *own* Wayland session is proxied to S's compositor and the swapchain
  buffer is named by a `BufferToken` instead of an fd. **Zero-copy dma-buf.** This is `vkcube`,
  `vkgears`, `solarsim`.
- **(c)1/(c)2 presentation** — S presents the application's *readback* buffer via `wl_shm`. This is
  `icosa-remote-demo.sh`. It is the path WP0 exists to retire, and it still works.

The icosa fixtures earn their place separately: they are the only ones that prove **correctness**
rather than liveness, because they are bit-compared against a native run.

### 0.2 On a capable C, Rayland runs at NATIVE frame rate over a real network

**This is the single strongest result the project has and it was missing from the first draft.**

With **dionysus** (x86_64, 8 cores) as C → dop561, over a real network: **25 ms median inter-frame gap
in 8 runs out of 8.** Native `vkcube` on the same compositor with no Rayland in the path:
**25.39 ms (p10 25.23, p90 25.56) = 39.4 fps.**

The relayed application is sitting on **the compositor's repaint timer, not on Rayland's cost.** The
contrast with milkv states the project's premise in one line: the identical blob diff over identical
bytes is **0.98 ms / 17.4% of wall clock on dionysus** and **6.38 ms / 56.8% on milkv**.

(Caveat that must travel with the number: headless weston paces at ~25.4 ms, so **~40 fps is the
harness ceiling** and 60 fps cannot be demonstrated against it on any machine.)

### 0.3 No pixels cross the network — a claim that was false until it was measured

The return path was shipping **every rendered frame back to C at ~877 KB/frame** — a frame-sized
payload per frame, to a machine with no display, in the project whose thesis is that pixels do not
cross the network. The (c)2 return path ships whatever S's GPU wrote into any blob and cannot tell a
swapchain image from a readback; only WP0's token path knows which is which.

| | before | after |
|---|---:|---:|
| S→C per frame | ~877 KB | **~1.9 KB** |
| S→C, A/B'd inside one binary | 307,776 B/frame | **219 B/frame — 1,406×** |
| C→S, same A/B | 3,723 B/frame | 3,594 B/frame — **1.04×, i.e. unchanged** |

**Why it survived: the display was already zero-copy, so the waste had no symptom and every test
passed.** The harness's own header asserted "No pixels cross the network" — false on every run. It was
found because the owner asked.

### 0.4 The ~1 second mouse-crossing stall is fixed — with two honest asterisks

**Measured, milkv→dop561, 60 s each, pointer crossing only:**

| | configures | swapchain rebuilds | stalls | worst |
|---|---:|---:|---:|---:|
| stock `vkcube` | 178 | 92 | **9** | **1,117 ms** |
| patched | 392 (391 same-size) | **1** | **0** | 104 ms |

**and 4.3× more frames in the same wall clock.**

**Asterisk 1 — the fix is in the application, not in Rayland.** `vkcube` calls `demo_resize()` on
every `xdg_surface.configure` with no size check; COSMIC sends one on every focus change; focus
follows the pointer. Natively that costs ~1 ms and nobody notices; over the relay a swapchain
recreation is hundreds of synchronous round trips ≈ 1 s. **`vkcube` is the outlier, settled from
source:** `vkgears` already records the size, compares, and rebuilds only on a real change — the
ordinary WSI pattern toolkits follow. **Do not generalise vkcube's stall into a Rayland limitation.**
A proxy-side mitigation (withhold a same-size configure) was considered and **deliberately rejected**:
that is the proxy deciding an activation change is not worth telling the application, which is false
for anything that renders focus.

**Asterisk 2 — the patch is NOT upstream.** `docs/patches/vkcube-only-resize-on-actual-size-change.patch`
is unsubmitted, as is the `fini_display` fix for mesa-demos.

**And resizing still stalls, and that one IS ours** — 5.1 s and 4.7 s measured on milkv for larger
windows. A resize is a *legitimate* swapchain rebuild; there is no application bug to patch. It is the
synchronous round-trip cost applied to the few hundred calls a rebuild makes. **Unfixed.**

### 0.5 Performance work, including what did not pay

| Change | Result |
|---|---|
| Presented-resource exclusion | **1,406×** less return traffic |
| `shm::DIFF_CHUNK` 64 → 4096 on C | **1.48× on milkv** (release, real network, complete separation, p=0.0015); **clean null on x86_64** |
| `reply_arena_fence_signaled` → `memchr` | lock-held p50 **8×**, p99 **16×** |
| Forward message coalescing | **6.1×** fewer messages, **5.4×** less time in `send()` |
| Readback gap-threshold coalescing | ~5000 → **~180** messages/frame, still bit-identical |
| Teardown `SIGABRT` (libepoxy) | ~21% of teardowns → **0** |

**The honest half, which a plan needs more than the wins:** the two 5–8× mechanism fixes (S's lock
contention, C's message flood) **each moved the median frame gap by nothing measurable.** Frame time
is not bound by CPU work on either side — it is a serialized latency chain, and what remains in it is
the **number** of round trips per frame and each one's fixed cost. **Both poll intervals have been
swept factorially and neither is the term.** Four candidates are now eliminated by measurement rather
than left unexplored.

### 0.6 Correctness and robustness

- **0 stale frames across 20 real-network runs** after the G' completion barrier — the (c)2 readback
  return path, which took three recorded dead ends to get right.
- **59/60 WP0 runs clean**, the one failure an artefact of the failure *definition* → **0 genuine
  defects in 60**.
- **0 differing frames in 1,200**, 10 runs, board as C.
- **`wl_shm` implemented** — `winit`, GTK and Qt treat it as **fatal at event-loop creation**, so
  every toolkit application aborted before creating a window or reaching Vulkan. This is what lets an
  ordinary application start at all.
- **`wl_keyboard.keymap` fixed** — it was dropped because it carries an fd, and the cost was not "no
  keyboard" but a **hung application**: anything creating a `wl_keyboard` waits for its keymap.
- Recycled-id race, version inheritance across three instances, and a cached handle to a destroyed
  dmabuf global — all fixed and guarded.

### 0.7 What it would be dishonest to claim

- **Resize stalls, and it is ours.** Unfixed.
- **Both upstream patches are unsubmitted.**
- **60 fps on milkv is parked** — the board's ceiling with a *perfect* scan is ~36 fps, below the
  harness compositor's own floor, and riscv64 cannot do dirty-page tracking (kernel 5.15).
- **Multi-queue is unsupported.**
- **`rayland-icosa-window` over WP0 is untested.** It used to refuse because the proxy did not
  advertise `wl_shm`; **it now does**, so the stated reason has expired and nobody has re-tried.
- **The Venus feedback question is open** — see §2 and §3.

---

## 1. The queued experiment could not have run

The command printed in `OVERVIEW.md` §6.2, in the handover, in the diary, and in the harness's **own
usage line**:

```
TRIES=400 VN_PERF_SETTING=no_multi_ring,no_fence_feedback scripts/soak-failure-rate.sh
```

The script read **`VNPERF`**. The variable that selects the arm under test was connected to nothing.
A night on that command would have measured the **shipping** arm — the one already clean through 480
runs — and reported it as the feedback arm. Both sibling harnesses (`c2-icosa-two-machine.sh`,
`c2-icosa-milkv.sh`) use `VN_PERF_SETTING`; only this one diverged, and its own documentation agreed
with the siblings rather than with itself.

Looking for that defect's siblings found seven more. In rough order of how much damage each could do:

| # | Defect | Consequence |
|---|---|---|
| 1 | Arm selector read a different variable name than every document sets | The night measures the wrong arm and says so confidently |
| **8** | **S's port not released before the next iteration bound it** | **Scored a harness failure as a failure of the arm under test** |
| **6** | **`scp` deploy failure unchecked** | **Measured a binary it had failed to send; printed "6 clean, 0 failed" for it** |
| **7** | **A `rayland-c` leaked on C every iteration** | **400 daemons over a night on one vtest socket; also the cause of 6** |
| 4 | S's GPU not pinned (only harness of five not doing it) | NVIDIA loses the device silently on ~half its runs → scored as an arm failure |
| 2 | Never rebuilt; pointed at a target dir nothing else writes | Ran binaries 26 commits stale |
| 5 | Every run's S log truncated by the next, inside the repo | A rare failure's evidence survives only if it happens last |
| 3 | No provenance in the output | A 400-run result outlives the terminal it ran in |

Three of these were caught by *watching them happen*, not by reading code:

- **Defect 6** announced itself. `scp` died with `ETXTBSY` — a leaked `rayland-c` was holding the
  binary open — and the loop ran anyway, printing **"6 clean, 0 failed"** for a build that never
  reached C.
- **Defect 8** produced a failure in a six-run smoke test. An `rayland-s` died on `SIGSEGV` during
  teardown and still held its port a second later; the next iteration's S exited with *"Address
  already in use"*; C had nothing to talk to; 0 frames came back; **the attempt was scored as a
  failure of the arm.** The loop retired S with `kill; sleep 1` and started the next with `sleep 3`,
  with nothing checking either end.
- **Defect 7** was visible as a process on apollo with a 4-minute uptime that nothing had killed.

### One error of my own, same class

My first port check was `ss -ltn` — listening **TCP**. S's transport is QUIC, so its listener is a
**UDP** socket and the predicate matched nothing, ever. It failed *safely* only because I had written
the caller to abort rather than assume; a version reading "not listening" as "free" would have been
defect 8 rebuilt by the person fixing defect 8. It cost one run to find, because the harness stopped
and said what it saw.

### Verified after all eight

10 of 10 and 4 of 4 clean, no leaked daemons on apollo, port handling correct, provenance printed.
That is the instrument being made fit for the experiment — not the experiment.

---

## 2. What this does to the project's recorded numbers

**This is the part a plan must get right, and it is narrower than it first looks.**

- **The `0/480` shipping-arm figure is probably sound.** The stale binaries were dated 2026-07-27,
  and that soak ran on 2026-07-27 — they were current on the day. Defect 2 bites *future* runs.
- **The `1/92` is NOT thereby explained.** It came from `c2-icosa-two-machine.sh`, not from this
  harness, and that script *does* check S came up — it aborts the sweep rather than scoring a failure,
  so it is protected against defect 8 specifically.
- **But that script had defect 7.** It leaked a `rayland-c` per run, all bound to the same vtest
  socket the next run's application dials. An intermittently-surviving daemon whose S connection is
  gone is a mechanism that produces exactly *"one run lost entirely to a silent Venus SIGABRT"*.

  **That is a mechanism of the right shape that was present the whole time. It is not a demonstrated
  cause.** The leak is now fixed in all three harnesses, so the re-run is clean of it either way.

**The defensible position:** the instrument used to compare arms could invent failures, and a ~1%
effect cannot be settled by a denominator that may include invented events. Treat the feedback
question as **open on both sides**. Do not carry forward "feedback caused a failure" *or* "feedback is
now exonerated" — the running soak is what decides.

---

## 3. The soak is running

```
TRIES=400 VN_PERF_SETTING=no_multi_ring,no_fence_feedback scripts/soak-failure-rate.sh
```

- Arm: `no_multi_ring,no_fence_feedback` — i.e. **semaphore, event and query feedback ON**, fence
  feedback off (load-bearing; the (c)2 barrier works by spotting the app's `vkGetFenceStatus` reply,
  and fence feedback removes that poll — exit 134 and zero frames, every time).
- **53 of 400 attempts, 53 clean, 0 failures.** ~29.5 s/attempt, ETA ~2.8 h.
- Evidence in `/tmp/rayland-soak/2026-09-02-feedback-arm/`, one file per attempt, S log and gdb
  backtrace kept for any failure.

**A caveat for whoever reads the result:** this gives the *feedback* arm a proper denominator on the
fixed instrument. The shipping arm's `0/480` came from the **pre-fix** instrument. If tonight shows
failures, the honest comparison needs a matched control run on the fixed harness rather than a
cross-instrument comparison. That is the natural next queued item.

Also: Woodpecker's agent on apollo runs `backend: local`, so CI pipelines execute directly on the
machine acting as **C**. It will not distort a failure *count*, but no timing figure should be read
out of this run.

---

## 4. Unplanned: the Forgejo move, and a CI breakage it caused

The owner moved Forgejo from apollo to a Synology and asked for a test.

**Push works.** `2249b6f` is identical on GitHub and Forgejo. The only surprise was an 87-second first
push against GitHub's 1.6 s — that was the branch backlog being packed, not the server. Note the
address moved twice during the session (`192.168.1.184` → `172.16.20.16`); a check at 01:52 reported
"no route to host" purely from the stale IP.

**Woodpecker's `rt` pipeline #31 failed**, and the diagnosis is worth recording because the failure
mode is silent and recurrent:

- The last successful rv64 clone (2026-08-28) used `https://forge.lan`, verified by a Caddy **local
  root CA** installed on milkv on Aug 11.
- The Synology's Forgejo is fronted by a **different Caddy instance with its own root**. The owner had
  installed it on apollo at 00:32; milkv and dionysus never got it.
- **The trap: both roots have the identical CN**, `CN=Caddy Local Authority - 2026 ECC Root`. Only the
  fingerprint and the validity start differ (Aug 11 vs Sep 1 22:30). Anything comparing by name calls
  it a match.

Fixed by installing the new root on both missing agents. `Verify return code: 20 → 0 (ok)` on milkv
and dionysus; apollo already correct. **dionysus had an empty trust-anchor directory** — it had
neither root — and the `gate` workflow is labelled `arch: x86_64`, so it can schedule there as well as
apollo. That one had not bitten yet and would have.

**Correction to an earlier claim of mine in this session:** I first reported rt's failing `fmt` step as
a defect. It is `failure: ignore` **by design** — *"the codebase predates any fmt enforcement and a
66-file reformat would churn blame and conflict with in-flight branches."* It also failed in the last
**successful** pipeline. Nothing to fix there. Not verified end-to-end: a real pipeline run needs a
push, and rt's working tree is dirty on a feature branch.

---

## 5. What a plan must not re-propose

Carried forward from the handover, all still binding:

- **No `vkgears` riscv64 hang.** There is none; the instruments manufactured it.
- **Do not separate Mesa 26.0.8 from 26.1.6.** That confound existed only to explain a defect that
  does not exist.
- **No protocol-level filter for the blob scan.** Refuted with per-blob numbers: the 8 MiB Venus
  staging pool is 68–85% of the scan, genuinely changes on 41–47% of deltas, and ships 500–600 bytes
  when it does. Scanning 8 MiB to find 600 bytes is a *location* problem, and location comes only from
  the decoder (banned by (c)1 §7) or page tables. There is no third signal.

Added by this session:

- **Do not quote a failure rate from `soak-failure-rate.sh` without saying which build and which arm.**
  It now prints git rev, working-tree state, arm, GPU pin and binary timestamps before the first run.
- **`fmt` failing in rt's CI is not a defect.**

---

## 6. Still open, unchanged by this session

- **The frame-time fork, and it is genuinely the owner's call.** Dirty-page tracking is the only
  mechanism that addresses the dominant cost — the per-delta blob scan is 8.92 ms on the board even
  for a fixture changing ~80 bytes a frame, 82% of it `memcmp` over memory that did not change, worth
  **29% of `icosa-gpu`'s entire frame**. Scan bandwidth is already 0.93–1.02 GB/s, i.e. at the board's
  memory bandwidth: **there is nothing left in any constant.** But **riscv64 cannot do it** (kernel
  5.15, no `CONFIG_HAVE_ARCH_SOFT_DIRTY`; `UFFD_WP_ASYNC` needs 5.19+, `PAGEMAP_SCAN` 6.7+), while
  soft-dirty works on dionysus where the same scan is worth ~19%. **The machine that needs it least is
  the one that can have it.** If built, the `clear_refs` race must be solved first: a write between
  the pagemap read and the clear is *lost*, which is silent corruption, and this path's whole
  discipline is that silent staleness is the unacceptable failure.
- **Capability instead:** (c)3 content-addressed assets, (c)4 real apps / GL via Zink, multi-queue,
  SP4/SP5. **(c)4 is probably where the remaining risk lives** — every application run so far is a
  demo or a fixture, and `solarsim`, the one real toolkit app, found two defects nothing else could in
  an afternoon.
- **Two upstream patches still unsubmitted**: `vkcube-only-resize-on-actual-size-change.patch` (a ~1 s
  stall per focus change over any remoting layer) and the `fini_display` fix for mesa-demos.
- **A latent hazard, recorded and not fixed:** `Applier::take_app_blob_writes` coalesces at gap 256 on
  an argument true of `res6` but not of its filter. Does not fire today.

---

## 7. Honest assessment of this session

The planned deliverable was a measurement and it is not in hand yet. What is in hand is that the
measurement, had it been taken as instructed, would have been **wrong in a way nobody would have
caught** — it would have produced a clean-looking "0 failures in 400" for an arm that was never
selected, on software five weeks old, with a GPU that fails half its runs unpinned, on an instrument
that scored its own setup failures against the hypothesis.

That is the project's single most expensive recurring error — *reading an output without checking what
produced it* — caught this time **before** it produced the output rather than after. It is the eighth
instance in four days and the first that was stopped in advance.

The cost was one evening of the machines. The alternative was a number nobody could trust and no way
to know it.

---

*Session: https://claude.ai/code/session_0153BQRRKAJcRSxUxRuEHQQs*
