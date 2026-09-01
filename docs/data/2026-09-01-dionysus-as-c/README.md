# dionysus as machine C — and Rayland reaching native frame rate over a real network

## The machine

| | |
|---|---|
| OS / kernel | Ubuntu 26.04.1 LTS, 7.0.0-30-generic |
| CPU / RAM | x86_64, 8 cores, 28 GB |
| GPU node | `renderD128` (AMD Vega), `virtio_icd.json` present |
| **soft-dirty** | **works, including the cross-process shared-memfd case** |

It needed only `vkcube` (copied from dop561 — same Ubuntu 26.04, glibc 2.43, so the binary runs as
is). No chroot, no cross-compilation: `scripts/wp0-soak.sh` drives it directly with `C_HOST=dionysus`.

## The headline

| | median inter-frame gap | fps |
|---|---|---|
| **native `vkcube`** on this compositor | 25.39 ms (p10 25.23, p90 25.56) | **39.4** |
| **relayed, dionysus → dop561** | 25 ms, **8 runs out of 8** | **40.0** |

**With a capable C, Rayland over a real network runs this application at native frame rate.** The
40 fps is the compositor's pacing, not Rayland's cost — headless weston repaints on a ~25.4 ms timer
and both the native and the relayed application sit on it.

The p10/p90 spread of the native run (25.23–25.56 ms) shows how hard that floor is, and the relayed
runs landing on *exactly* 25 ms eight times out of eight is the same floor seen from the other side.

## What owns the frame on dionysus

| owner | share |
|---|---|
| Rayland round trip (wire + S) | 46.5% |
| Application | 18.0% |
| **C: diffing blobs** | **17.4%** (0.98 ms/delta) |
| Compositor | 17.5% |
| C: writing to the link | 0.6% |

Compare milkv, where the same diff is **6.38 ms/delta and 56.8%** of the wall clock. Same code, same
bytes; the difference is entirely the machine. This is the clearest statement yet of what "C may be
weak" costs.

## Consequence for the 60 fps goal — the harness itself caps at 40

**60 fps cannot be demonstrated against this headless weston on any machine**, because native peaks at
39.4 fps on it. Any future 60 fps target must be measured against a 60 Hz compositor (the live COSMIC
session), and the parked milkv item inherits this: its computed ceiling with a *perfect* scan was
~36 fps, which is below this compositor's floor anyway, so that goal needs **both** a kernel that can
do dirty-page tracking **and** a faster compositor to measure against.

## Two traps found here, both now guarded

1. **`--gpu_number` is an index into the list Venus exposes, not a stable name.** The project rule
   "vkcube must run with `--gpu_number 0`" was written when index 0 was S's Intel node; its *intent* is
   "do not land on the NVIDIA RTX A500", which loses the device on 7 of 14 runs. On this path index 0
   **is** the NVIDIA card: a first run selected `Virtio-GPU Venus (NVIDIA RTX A500 Laptop GPU)` and
   produced **zero attaches in 195 s** — four swapchain buffers created, one commit, nothing ever
   presented, which is what device loss looks like from outside, with **no error in any log**. Index 1
   is the Intel device and works. `wp0-soak.sh` now takes `APP_ARGS`, and the run log must always be
   checked for which device was actually chosen rather than trusting the number.
2. **ssh to dionysus is intermittent-looking for the same reason milkv was.** The owner's config sets
   `IdentitiesOnly yes` globally with no `Host dionysus` entry; sessions work while a `ControlPersist`
   master is alive and appear to be "permission denied" afterwards. The key was authorized all along.

## Reproducing

```
C_HOST=dionysus PROFILE=release MODE=traffic RUNS=8 FRAMES=60 \
  WESTON_SOCKET=wl-dio APP_ARGS="--gpu_number 1" OUT=/tmp/dio scripts/wp0-soak.sh

# the native control, on the same compositor
WAYLAND_DISPLAY=wl-dio WAYLAND_DEBUG=1 vkcube --gpu_number 0 2>&1 | grep 'wl_surface#.*attach'
```
