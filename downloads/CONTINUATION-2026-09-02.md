# Continuation prompt — Rayland, after 2026-09-01

Paste this into a fresh session. Everything below is on branch **`wp0-wayland-proxy`** (NOT `main` —
`main` is 40+ commits behind and merging is the owner's call). Read `docs/OVERVIEW.md` §5.3 and §6.3
first; they carry the measured numbers and the beliefs a plan must not re-propose.

## Where things stand

**Working end to end:** `vkcube` and `solarsim` both run on milkv (riscv64) and render on dop561's real
desktop. `solarsim` was the `wl_shm` acceptance test and it passed all four criteria. `vkgears` works
from x86_64 C machines but hangs from milkv.

**Frame rate, measured:** dionysus (x86_64) as C reaches **native frame rate** — 25 ms/frame against a
25.4 ms native baseline on the same compositor. milkv is ~45 ms/frame. **Note the harness caps at
~40 fps**: headless weston paces at 25.4 ms, so no 60 fps claim can be measured against it.

## The three open threads, in the order I would take them

### 1. `vkgears` hangs on milkv — one confound left to eliminate  (`docs/data/2026-09-01-vkgears-riscv64/`)

Bounded to a **conjunction**: vkgears + Venus + Rayland + riscv64. It works on loopback x86_64, works
apollo→dop561 over the real LAN, works on milkv itself at 60 FPS with lavapipe and no Rayland, and
`vkcube` works on milkv over Rayland. During the hang Venus's WSI thread sleeps in `vn_relax` holding a
mutex the main thread wants, **while C relays 2,332 ring messages and 5,389 blob syncs in 40 s** — so
it is not starved, and the "Venus makes a second WSI ring we do not watch" theory is refuted.

**The one variable not yet separated:** dop561 and apollo run **Mesa 26.0.8**; the milkv chroot runs
**Mesa 26.1.6**. Do this first — it is cheap and it halves the search space:
- put Mesa 26.1.6 on an x86_64 C (a Debian sid container on apollo), **or**
- put an older Mesa in the milkv chroot (`/mnt/build/sid`, 106 GB free).

If 26.1.6 on x86_64 also hangs, this is a Mesa regression and not ours.

### 2. Run our own icosa fixtures on milkv — the owner asked for this and it has not been done

`rayland-icosa-cpu` / `-gpu` / `-window` are *ours*, instrumented, and designed to isolate mapped-write
volume. Nothing this week ran them against milkv. They are the natural third data point beside vkcube
(works) and vkgears (hangs), and unlike either they can be modified and traced. Cross-compile for
riscv64 the same way `rayland-c` is built (see below).

### 3. Frame time — everything cheap is exhausted; the remaining lever is the round-trip COUNT

Five interventions each collapsed a mechanism by 5–8× and moved the frame rate by nothing: S's lock
contention, C's message count, C's send path, both poll intervals. The reason is now known — the
application makes **3.6 genuinely synchronous round trips per frame** (93% of consecutive ring deltas
have a reply between them; none is under 1 ms apart, so they are not C splitting a burst). On milkv the
blob diff is 56.8% of the wall clock; on dionysus it is 17.4%. **Dirty-page tracking is the structural
fix and riscv64 cannot do it** (no `CONFIG_HAVE_ARCH_SOFT_DIRTY`, proven by probe; `UFFD_WP_ASYNC`
needs 5.19+, `PAGEMAP_SCAN` 6.7+, milkv runs 5.15). **soft-dirty works on dionysus**, cross-process on
a shared memfd — so build it there if you build it. Solve the `clear_refs` race first: a write between
the pagemap read and the clear is *lost*, which is silent corruption.

## Smaller open items

- **Two upstream patches to submit.** `docs/patches/vkcube-only-resize-on-actual-size-change.patch`
  (mine, fixes a ~1 s stall per focus change over any remoting layer) and the solsim session's
  `fini_display` fix for mesa-demos.
- **A latent hazard, recorded and not fixed:** `Applier::take_app_blob_writes` coalesces at gap 256 on
  an argument true of `res6` but not of its filter — a blob both sides write would get S's stale copy
  at a 256-byte grain. Does not fire today.
- **`wl_shm` v1 optimisations are NOT needed** and that is now measured, not assumed: solarsim's pools
  were 2 and 1024 bytes and synced **zero** bytes in two minutes.

## Machines, and the traps each one has

| | |
|---|---|
| **dop561** | this laptop; S in every test. Live session is `wayland-1` (cosmic-comp). |
| **milkv** | riscv64, kernel 5.15. **The working stack is a Debian sid chroot on a second card**, `/mnt/build/sid` — the *host* root is a 2022 snapshot with 1.1 GB free and no Venus ICD. `sudo /mnt/build/chroot-mounts.sh up` after a reboot; `/mnt/build/README` documents it. **Never build on the host root.** |
| **apollo** | x86_64, 16 cores. |
| **dionysus** | x86_64, 8 cores, Ubuntu 26.04, **soft-dirty works**. |

**ssh:** the owner's config sets `IdentitiesOnly yes` globally with no `Host` entry for milkv,
dionysus or apollo, so a fresh connection offers no key and looks like "Permission denied" once a
`ControlPersist` master expires. **Use `-o IdentitiesOnly=no`.** The keys are authorised; this cost two
sessions an hour between them.

**`--gpu_number` is an INDEX into the list Venus exposes, not a name, and the order moves.** Landing on
the NVIDIA RTX A500 loses the device on 7 of 14 runs, **silently** — buffers get created, one commit
happens, nothing is ever presented, and no log says why. Both harnesses now pin S's
`VK_ICD_FILENAMES` to Intel instead (`S_ICD=` restores full enumeration), which is the right layer:
`solarsim` asks wgpu for `HighPerformance` and no env var overrides a hardcoded preference.

**Cross-compiling for milkv** (~2 min, and far better than a 43-minute native build):
```
CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER=riscv64-linux-gnu-gcc \
CC_riscv64gc_unknown_linux_gnu=riscv64-linux-gnu-gcc \
CARGO_TARGET_DIR=/tmp/rv cargo build --release -p rayland-c --target riscv64gc-unknown-linux-gnu
```

## Harnesses

- `scripts/wp0-soak.sh` — failure rate and traffic. `PROFILE=release` **for any timing figure**; it
  built `debug` for its whole life and that inverted a published conclusion once.
- `scripts/milkv-demo.sh` — the cube (or anything) on the owner's **real** session, for a human to
  look at. Not a measurement.
- `scripts/wp0-milkv-ab.sh` — interleaved A/B with the board as C.
- Instruments, all env-gated and cheap: `RAYLAND_C1_RELAXSTAT`, `RAYLAND_S_STAGES`,
  `RAYLAND_S_LOCKSTAT`, `RAYLAND_C1_LINK_LOG`/`RAYLAND_S_REPLY_LOG` (**this one perturbs** — it halved
  a frame rate).

## Method notes this week paid for

- **Interleave every A/B and decide n before looking.** A small sample flattered the hoped-for answer
  three times: 1.78× that was really a null, 1.28× that was really 1.03×, a "win" at n=3 that vanished
  at n=11.
- **Never chain sweeps into one ratio.** The same binary measured 49.5 ms and 71 ms in two sweeps.
- **Check what produced an output before reading it.** This week: an opcode without its interface
  (twice), an argument by type instead of position, one filesystem standing in for a machine, a debug
  build standing in for the software, and a harness silently substituting the program under test. Every
  one produced a confident wrong conclusion.
- **A test that lists the things it supports cannot find the one you forgot.** S's interface registry
  was missing `wl_shm`; the test asserted all eleven listed names resolve. Only running a real toolkit
  application found it.
