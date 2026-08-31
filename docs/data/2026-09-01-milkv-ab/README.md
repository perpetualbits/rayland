# The milkv A/B — the chunk fix on a real weak C, and a published number corrected

## What was tested

The 2026-09-01 blob-diff chunk change (`shm::DIFF_CHUNK` 64 → 4096) removes ~10 ms of CPU per ring
delta **on C**, and C is the machine Rayland's premise says may be weak. This is that change measured
on the machine the premise is about: the **riscv64 milkv board**, 4 cores, over a real network to
dop561.

Both arms **cross-compiled on the laptop** (`--target riscv64gc-unknown-linux-gnu`, **release**, ~2
minutes each) and copied in. Nothing is ever built on the board: its host root has ~1.1 GB free
against a 9 GB debug target directory. Cross-building also guarantees one toolchain produced both
arms, which matters more for a comparison than either arm's absolute speed.

The application and `rayland-c` run inside the board's **Debian sid chroot** (`/mnt/build/sid`, Mesa
26.1.6, `virtio_icd.json`), built by the solsim session; `/mnt/build/README` documents it. The host OS
is a Debian ports snapshot frozen at 2022-12-25 and cannot run this stack at all.

Harness: `scripts/wp0-milkv-ab.sh`. n = 8 pairs, fixed before running, interleaved, exact-PID cleanup.

## Result — the fix is real, and large, on a weak C

| | BEFORE (chunk 64) | AFTER (chunk 4096) |
|---|---|---|
| median inter-frame gap | **105 ms** | **71 ms** |
| all runs | 97, 97, 102, 104, 106, 110, 111, 115 | 68, 69, 71, 71, 79, 81, 82 |
| frame rate | 7.45 /s | **9.98 /s** |

**1.48×, and the distributions do not overlap at all** — every AFTER run is faster than every BEFORE
run. Mann-Whitney **U = 56 of 56, p = 0.0015**. Zero stalls in any run.

## The correction this forced

The same change measured **1.78×, p = 0.0025** on the laptop hours earlier, and that figure was
published as a headline. It was measured on **debug** binaries, because `scripts/wp0-soak.sh` had
built `debug` for its whole life.

Re-run on **release** binaries on the same laptop, n = 13 per arm:

| | BEFORE | AFTER |
|---|---|---|
| median inter-frame gap | 35 ms | 37 ms |
| all runs | 25, 25, 25, 26, 29, 31, 35, 36, 41, 42, 49, 54, 68 | 25, 25, 27, 31, 37, 37, 37, 39, 45, 49, 64, 66, 81 |

**p = 0.38 — a clean null.** No benefit and no harm on a fast x86_64 machine.

Debug Rust keeps the bounds checks and per-iteration overhead of a byte loop, so it exaggerates
exactly the cost this change is about. The 1.78× was a true statement about a debug build and wrong to
publish as a result.

**This does not weaken the change; it locates it.** A fixed 13.2 MiB per-delta scan costs a 4-core
riscv64 board far more than it costs a laptop whose optimiser hides the loop overhead — which is
precisely the asymmetry Rayland exists for, since **C is the weak machine by design**. A fix that is
neutral on the developer's laptop and 1.48× on the target hardware is the right shape, and would have
been dismissed if only the laptop had been measured in release.

## What else this invalidates, stated plainly

**Every duration ratio produced by `wp0-soak.sh` before today is a debug-build figure** unless it says
otherwise — including this week's fence-scan and coalescing timings. Ratios of *counts* (messages,
bytes, lock acquisitions, stage occurrences) do not depend on the build profile and stand unchanged.
The harness now takes `PROFILE=release`, and timing work must set it.

## One unexplained early exit, reported rather than dropped quietly

Of the 16 runs, one — `pair6-AFTER` — ended after **9.9 s** of a 25 s run with **zero frames**. Every
other run ran 29.5–29.8 s. Its logs (`early-exit-run-c.log.gz`) show **no error, no panic, no protocol
refusal**: the application printed its two startup lines, `rayland-c` reported a normal final metrics
line, and the proxy tore its objects down cleanly. `pair6-BEFORE`, immediately before it, was also the
slowest BEFORE run (155 attaches against 208–232), so that period on the board looks disturbed.

It is excluded from the rate statistics above and reported here. **1 in 8 against 0 in 8 is not a
significant difference** (Fisher p = 1.0) and it is not attributed to the change. It resembles the
already-recorded, still-unfixed startup-path abort seen at ~7% on the laptop, but nothing in these
logs confirms that, so it is left as unexplained.

## Reproducing

```
# cross-build both arms (release) on the laptop
CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER=riscv64-linux-gnu-gcc \
CC_riscv64gc_unknown_linux_gnu=riscv64-linux-gnu-gcc \
CARGO_TARGET_DIR=/tmp/rv-ab cargo build --release -p rayland-c --target riscv64gc-unknown-linux-gnu

# the board needs its chroot bind mounts after a reboot
ssh milkv 'sudo /mnt/build/chroot-mounts.sh up'

PAIRS=8 SECS=25 A_BIN=/tmp/rv-c-BEFORE B_BIN=/tmp/rv-c-AFTER scripts/wp0-milkv-ab.sh
```

Note the board is reached with `-o IdentitiesOnly=no`: the owner's ssh config sets `IdentitiesOnly
yes` globally with no `Host milkv` entry, so a fresh connection offers no key and is refused. Earlier
sessions only worked by riding a `ControlPersist` master. The harness handles this and does not edit
the owner's configuration.
