# Continuation prompt — Rayland, after 2026-09-02

Paste this into a fresh session. Everything below is on branch **`wp0-wayland-proxy`** (NOT `main` —
`main` is 40+ commits behind and merging is the owner's call). Read `docs/OVERVIEW.md` §5.3, §5.4 and
§6.3 first; they carry the measured numbers and the beliefs a plan must not re-propose.

## Read this before planning anything

**Yesterday retracted a recorded finding and closed a direction. Both were mine.** The session before
had concluded `vkgears` "hangs" on the riscv64 board; it does not, and the evidence directory's own
archived log disproved it. Then I proposed a protocol-level filter as the way to attack frame time,
spiked it, and refuted it. Neither is a defeat — but a planner working from the older documents will
re-propose both, so:

- **Do not chase the `vkgears` riscv64 hang.** There is none. `docs/data/2026-09-01-vkgears-riscv64/`
  and `.../vkgears-blocked/` carry retraction notices at their heads; the correction is
  `docs/data/2026-09-01-vkgears-not-a-hang/`.
- **Do not separate Mesa 26.0.8 from 26.1.6.** That confound existed only to explain a defect that
  does not exist. (A sid `libvulkan_virtio.so` 26.1.6 is extracted at `/tmp/mesa-sid/` on apollo and
  `wp0-soak.sh` takes `APP_ICD=`, if it is ever wanted for another reason.)
- **Do not propose a protocol-level filter for the blob scan.** Refuted with per-blob numbers; see
  §3 below and `docs/data/2026-09-01-icosa-on-riscv64/` §7.

## Where things stand

**Working end to end:** `vkcube`, `vkgears` and `solarsim` all run on milkv (riscv64) and render on
dop561's real desktop. Our own fixtures now do too, and they are bit-identical to native.

**WP0 4.3 is complete** — S builds real `wl_buffer`s from relayed tokens. Both `OVERVIEW.md` and
`CLAUDE.md` claimed otherwise in prose while their own task tables said done; corrected 2026-09-02
against the code. If you find another such contradiction, believe the code and fix the document.

**The icosa fixtures ran against the weak C for the first time** (2026-09-02), and the frame
decomposes cleanly. Medians per frame, the fixtures' own timers:

| | fractal | upload | draw+readback |
|---|---|---|---|
| `icosa-gpu` apollo → S | 0.0 | 0.0 | 10.1 ms |
| `icosa-gpu` milkv → S | 0.0 | 0.0 | 51.9 ms |
| `icosa-cpu` apollo → S | 56.7 | 20.8 | 15.8 ms |
| `icosa-cpu` milkv → S | 683.5 | 101.1 | 50.9 ms |

- **The synchronous round trip does not care about mapped-write volume** — `draw+readback` is the
  same whether the frame pushed 80 bytes or a megabyte.
- **The megabyte is charged at `upload`**, the one call that ships it: ~20 ms/MiB from a strong C,
  ~100 ms/MiB from a weak one. That is (c)2's cost with an address.
- **Rayland scales with the board's general slowness, not superlinearly:** 4.9× and 5.1×.
- **Do NOT quote `icosa-cpu`'s 850 ms/frame as a Rayland cost.** 683 ms of it is the application's
  own CPU, with no Vulkan call in it. Rayland's share is ~152 ms, or ~52 ms with no mapped writes.
- **Correctness: 0 differing frames in 1,200 over 10 runs with the board as C.** Read at the frame
  level; 10 runs bound the *run*-level rate only under 30%, which does not prove the (c)2 return
  path on riscv64.

## The open threads, in the order I would take them

### 1. The overnight soak that settles the feedback question — best information per unit effort

```
TRIES=400 VN_PERF_SETTING=no_multi_ring,no_fence_feedback scripts/soak-failure-rate.sh
```

Semaphore/event/query feedback measured **1.23×** and is held back by exactly **one** unexplained
failure in 92 runs, against a shipping arm clean through 480. That is not a significant difference
and the flags are off because "we do not know what that was", not because feedback breaks anything.
One night of soak converts a superstition into a number in either direction. It has been the queued
experiment for weeks and still is; it needs no cleverness, only the machines and a night.

### 2. Frame time — the answer is known and uncomfortable; decide what to do about it

Dirty-page tracking is **the only mechanism** that addresses the dominant cost, and everything else
has now been eliminated by measurement rather than left unexplored:

- The per-delta blob scan is **8.92 ms on the board even for a fixture changing ~80 bytes a frame** —
  82% is `memcmp` over memory that did not change. Worth **29% of `icosa-gpu`'s whole frame**.
- Per-blob attribution: the **8 MiB Venus staging pool is 68–85% of the scan.** It genuinely changes
  on 41–47% of deltas and ships **500–600 bytes** when it does. Scanning 8 MiB to find 600 bytes is a
  *location* problem, and location comes only from the decoder (banned by (c)1 §7) or page tables.
- **Scan bandwidth is 0.93–1.02 GB/s**, so cost is exactly `bytes ÷ 1 GB/s`. The scan already runs at
  the board's memory bandwidth. **There is nothing left in `DIFF_CHUNK` or any other constant.**
- **riscv64 cannot do dirty-page tracking** (no `CONFIG_HAVE_ARCH_SOFT_DIRTY`, kernel 5.15;
  `UFFD_WP_ASYNC` needs 5.19+, `PAGEMAP_SCAN` 6.7+). **soft-dirty works on dionysus**, where the same
  scan is worth ~19% of frame time. The machine that needs it least is the one that can have it.

So the decision is the owner's, and it is a genuine fork: **build it on x86_64 for a ~19% win and
accept that the weak machine cannot have it, or leave frame time alone and spend the effort on
capability instead.** If you build it, solve the `clear_refs` race first — a write between the pagemap
read and the clear is *lost*, which is silent corruption, and this path's whole discipline is that
silent staleness is the unacceptable failure.

Use `BLOBSCAN=1` (kept, env-gated, in `blob_sync.rs`) before and after anything here; its module doc
carries the table above.

### 3. Capability, if frame time is set aside

`(c)3` content-addressed assets, `(c)4` real apps / GL via Zink, multi-queue support, SP4/SP5. None
has been scoped recently. `(c)4` is probably where the project's remaining risk actually lives — every
application run so far is a demo or a fixture, and `solarsim` (one real toolkit app) found two defects
nothing else could, in an afternoon.

## Smaller open items, carried forward

- **Two upstream patches still unsubmitted.** `docs/patches/vkcube-only-resize-on-actual-size-change.patch`
  (fixes a ~1 s stall per focus change over any remoting layer) and the solsim session's
  `fini_display` fix for mesa-demos.
- **A latent hazard, recorded and not fixed:** `Applier::take_app_blob_writes` coalesces at gap 256 on
  an argument true of `res6` but not of its filter — a blob both sides write would get S's stale copy
  at a 256-byte grain. Does not fire today.
- **An unverified guess, kept as a guess:** the Venus staging pool *grows*, so the 1 MiB blob that
  never changes in either fixture might be a *retired* chunk (~10% of the scan on abandoned memory).
  Inference from a function name and a zero. Even if true there is no signal by which C could learn
  Venus is done with a shmem.
- **`wl_shm` v1 optimisations are NOT needed** and that is measured, not assumed: solarsim's pools
  were 2 and 1024 bytes and synced **zero** bytes in two minutes.

## Machines, and the traps each one has

| | |
|---|---|
| **dop561** | this laptop; S in every test. Live session is `wayland-1` (cosmic-comp). |
| **milkv** | riscv64, kernel 5.15. **The working stack is a Debian sid chroot on a second card**, `/mnt/build/sid` — the *host* root is a 2022 snapshot (glibc 2.36) with no Venus ICD. `sudo /mnt/build/chroot-mounts.sh up` after a reboot. **Never build on the host root, and never run a cross-built binary there** — it dies with `GLIBC_2.39 not found` before `main`. |
| **apollo** | x86_64, 16 cores. **Has no `/usr/bin/vkcube`** — the harness copies one to `/tmp`. |
| **dionysus** | x86_64, 8 cores, Ubuntu 26.04, **soft-dirty works**. |

**ssh:** the owner's config sets `IdentitiesOnly yes` globally with no `Host` entry for milkv,
dionysus or apollo, so a fresh connection offers no key and looks like "Permission denied" once a
`ControlPersist` master expires. **Use `-o IdentitiesOnly=no`.** The keys are authorised.

**Pin S's ICD to Intel, always.** The NVIDIA RTX A500 loses the device on 7 of 14 runs, **silently**:
buffers are created, a commit or two happens, nothing is ever presented, and no log on either side
says why. It is indistinguishable from the application blocking, and on 2026-09-02 it cost a session
four zero-attach runs that were briefly read as a riscv64 defect. All four harnesses now default
`VK_ICD_FILENAMES` for `rayland-s` to Intel; `S_ICD=` restores full enumeration. `--gpu_number` is an
*index* into a list whose order moves, and most applications have no such flag at all — the S-side pin
is the only lever that works for all of them.

## Harnesses

- `scripts/wp0-soak.sh` — failure rate and traffic. `PROFILE=release` **for any timing figure**.
  Knobs: `APP_ICD`, `VKCUBE`, `APP_ARGS`, `S_ICD`, `LINK_LOG`, `LOCKSTAT`, `STAGES`.
- `scripts/c2-icosa-milkv.sh` — **new**: our fixtures with the board as C, bit-compared against
  native-on-S. `APP=cpu|gpu`, `RUNS=`, `RELAXSTAT=1`, `BLOBSCAN=1`.
- `scripts/c2-icosa-two-machine.sh` — the same for an x86_64 C. Now takes `APP=cpu|gpu`.
- `scripts/wp0-milkv-ab.sh` — interleaved A/B with the board as C.
- `scripts/milkv-demo.sh` — anything on the owner's **real** session, for a human to look at. **Not a
  measurement**: an unfocused window is throttled by the compositor, and the same `vkgears` run gave
  910 frames in 60 s once and ~39 in 40 s twice on window focus alone.
- `scripts/attach-count.awk` — the **one** shared frame scorer. It reads the application's
  `wl_surface` object id out of the proxy log. Never hardcode an object id again; see below.

## Method notes, and the ones that were paid for twice

- **Check what produced an output before reading it.** This is now the single most expensive recurring
  error in the project's history. In two days: an opcode without its interface (twice), an argument by
  type instead of position, one filesystem standing in for a machine, a debug build standing in for the
  software, a harness silently substituting the program under test, a hardcoded object id standing in
  for every application's surface, and a probe run without the daemon it was probing.
- **A number that is identically zero for a whole class of input is not a measurement.** Three
  harnesses counted frames as `forward obj 3 opcode 1` — vkcube's surface id — so *every* `vkgears`
  run any of them ever scored read zero, identically for 33 FPS and for a stop. A human watched that
  zero for ten seconds at a time and concluded "hang".
- **`grep -c` exits 1 when the count is zero and still prints `0`.** With `|| echo 0` the variable
  becomes `"0\n0"`, the comparison dies with "integer expected", and the run is scored **PASS**. It
  fires only in the zero case — the one case the check exists for.
- **Sample "was it set by the caller" before applying a default.** A guard testing `${VAR+set}` on the
  line after `VAR="${VAR:-default}"` is dead code, and this one silently ran no application at all.
- **Measure the prize before building the mechanism.** It paid twice in one day: once sizing
  dirty-page tracking, once killing the filter I had proposed to replace it. Two board runs against
  three weeks inside an implementation.
- **Interleave every A/B and decide n before looking.** A small sample flattered the hoped-for answer
  three times: 1.78× that was a null, 1.28× that was 1.03×, a win at n=3 that vanished at n=11.
- **A test that lists the things it supports cannot find the one you forgot.** S's interface registry
  was missing `wl_shm`; the test asserted all eleven listed names resolve. Only a real toolkit found it.
