# Report to the planning session — Rayland, 2026-09-02 (night)

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
