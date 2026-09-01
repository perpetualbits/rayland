# Why `vkgears` hangs — bounded to Venus-over-Rayland on riscv64

## The matrix, which is the whole finding

| C machine | arch | path | result |
|---|---|---|---|
| dop561 (loopback) | x86_64 | Venus over Rayland | **works** — 5,882 frames |
| apollo → dop561 | x86_64 | Venus over Rayland, real network | **works** — 7,446 frames |
| **milkv → dop561** | **riscv64** | **Venus over Rayland, real network** | **HANGS** |
| milkv (local) | riscv64 | **lavapipe, no Rayland** | **works — 60.0 FPS** |
| milkv → dop561, **`vkcube`** | riscv64 | Venus over Rayland | **works** |

Four hypotheses are eliminated by that table alone:

- **Not `vkgears`.** It runs at a flat 60 FPS on the same board, same Mesa, same compositor — with
  lavapipe instead of Venus and no Rayland in the path.
- **Not riscv64 as such.** Same board runs `vkgears` fine on lavapipe, and runs `vkcube` fine over
  Rayland.
- **Not Rayland's Venus path as such.** The same `vkgears` binary source runs over Rayland from two
  x86_64 machines, one of them over the real network.
- **Not the network.** apollo → dop561 crosses the same LAN and works.

It is the **conjunction**: `vkgears` + Venus + Rayland + riscv64.

## What the hang looks like from inside

`gdb` on the blocked process:

```
Thread 2 "vn_wsi[0,0]":  clock_nanosleep ... libvulkan_virtio.so   <- Venus WSI thread, in vn_relax
Thread 1 "vkgears":      pthread_mutex_lock ... libvulkan_virtio.so
```

**Venus's WSI thread sleeps in its back-off while holding a mutex the application's main thread is
blocked acquiring.** The process is `S (sleeping)`, never exits, and never attaches a buffer — so the
absent `wl_buffer.release` is a *symptom*, not a cause.

## The relay is not starved, which rules out the obvious explanation

The tempting theory was that Venus creates a **second ring** for WSI (the thread is literally named
`vn_wsi[0,0]`) and `rayland-c` watches only one — `rayland-c: watching command ring res_id=1`, and
`CLAUDE.md` records "a second ring stays latent". The measurement refutes it: during the hang C is
relaying hard.

```
c2s_ring_msgs=2332  c2s_inline_msgs=2119  c2s_blob_sync_msgs=635  s2c_blob_sync_msgs=5389
round_trips=18      elapsed_us=40298166
```

2,332 ring messages in 40 s for an application that draws nothing. Venus is spinning on something and
the relay is answering; whatever it is waiting for is not a message we failed to carry.

## The one confound still standing

Mesa differs across the working and failing machines, and it was not controlled for:

| machine | Mesa |
|---|---|
| dop561, apollo | **26.0.8** |
| milkv chroot | **26.1.6** |

So "riscv64" and "Mesa 26.1.6" are not yet separated. **The cheapest next experiment is to separate
them**, and there are two ways: put Mesa 26.1.6 on an x86_64 C (a Debian sid container on apollo), or
put an older Mesa in the milkv chroot. Either turns a two-variable difference into one.

`vkgears`'s own fence setup was checked and is not the cause: its frame fences are created with
`VK_FENCE_CREATE_SIGNALED_BIT`, so the first `vkWaitForFences` returns immediately.

## Two harness bugs found while doing this, both fixed

1. **`wp0-soak.sh` silently clobbered `VKCUBE`** when `C_HOST` was set — the two-machine path rewrites
   it to `/tmp/vkcube`. So the first apollo run **quietly ran `vkcube`** and produced a confidently
   wrong "vkgears hangs on apollo too", complete with an NVIDIA device-loss that had nothing to do with
   it. It now only rewrites the path when the caller did not name a binary.
2. **Neither harness pinned S's ICD**, so applications kept landing on the NVIDIA RTX A500 that loses
   the device on 7 of 14 runs. Both now default `VK_ICD_FILENAMES` for `rayland-s` to Intel, with
   `S_ICD=` to restore full enumeration.

## Reproducing

```
# hangs
SECONDS_TO_RUN=70 APP=/usr/local/bin/vkgears APP_ARGS= scripts/milkv-demo.sh

# works, same board, no Rayland
ssh milkv 'sudo chroot /mnt/build/sid /bin/bash -c "... VK_ICD_FILENAMES=.../lvp_icd.json vkgears"'

# works, x86_64 C over the real network
C_HOST=apollo PROFILE=release MODE=traffic RUNS=1 FRAMES=40 \
  VKCUBE=/tmp/vkgears APP_ARGS=" " scripts/wp0-soak.sh
```
