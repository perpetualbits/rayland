# Dirty-page tracking, and why milkv cannot reach 60 fps this way

## The brief and the answer

*"Do the dirty-page tracking. Please get milkv comfortably above 60 fps."*

**Dirty-page tracking cannot run on milkv, and 60 fps is not reachable by removing the diff.** Both are
measured below rather than argued. What was delivered instead — two changes that need no kernel
support — took the board's diff from 8.79 ms to 6.59 ms per delta and its frame rate to ~22 fps.

## Finding 1 — soft-dirty does not exist on the target machine

Soft-dirty (`/proc/PID/clear_refs` + bit 55 of `/proc/PID/pagemap`) is the standard way to learn which
pages another process wrote. Probed directly rather than inferred (`probe-softdirty-basic.py`):

| machine | kernel | result |
|---|---|---|
| laptop, x86_64 | 7.0.0 | write page 3 → soft-dirty `[0,0,0,1,0,0,0,0]` — **works and discriminates** |
| **milkv, riscv64** | **5.15.0** | write page 3 → `[0,0,0,0,0,0,0,0]` — **no bit ever set** |

riscv64 does not select `CONFIG_HAVE_ARCH_SOFT_DIRTY`. The two modern alternatives are also out on
5.15: `UFFD_FEATURE_WP_ASYNC` needs 5.19+, and the `PAGEMAP_SCAN` ioctl needs 6.7+.

The mechanism *would* work for our exact case on a machine that has it — `probe-softdirty-shared-memfd.py`
forks a child that writes a `MAP_SHARED` memfd and reads the child's pagemap from the parent, which is
precisely how `rayland-c` would watch the application: **pages 7 and 40 written, pages 7 and 40
reported, nothing else.** So this is not a design dead end; it is unavailable on this hardware.

**It was not built blind, for a second reason worth recording.** `clear_refs` has an unavoidable race:
between reading the pagemap and clearing it, a write sets a bit that the clear then destroys, and that
write is lost rather than deferred — silent corruption. Closing it needs care that is only worth
spending once there is a machine that can use the result.

## Finding 2 — where milkv's frame actually goes

C-side stage timeline on the board (`RAYLAND_C1_RELAXSTAT`):

| | before today | after today |
|---|---|---|
| diff per ring delta | 8.785 ms | **6.586 ms** |
| ring deltas per frame | 3.77 | 3.58 |
| **diff per frame** | **~33 ms** | **~23.6 ms** |
| frame | ~53 ms (18.8 fps) | **~45 ms (22.2 fps)** |

**The ceiling this route has:** removing the diff *entirely* leaves **21 ms per frame — 47 fps.**
60 fps needs 16.7 ms. So even perfect dirty tracking, on a machine that supported it, would not reach
the target. The remainder is ~3.58 synchronous round trips per frame, each carrying S's own ~5 ms of
per-delta handling.

**The real lever for 60 fps is therefore the round-trip *count*, not the diff.** At one delta per frame
the diff falls to 6.6 ms and S's share from ~17.8 ms to ~5 ms, projecting ~12 ms per frame. That is the
"synchronous round trip" seam this project has carried for weeks, arriving from a new direction.

## What was delivered

### 1. A presented blob is not diffed at all

Correctness first: a presented blob is one C published as a `BufferToken`, so S's GPU renders into it
and never reports those writes; C's baseline is stale **by design** and anything C shipped for it would
overwrite S's pixels. C must never ship one, so there is nothing to learn by diffing one.

Saving: the four 1 MB swapchain images are 4 MiB of the 13.2 MiB walked per delta.

Measured on the board: **8.80 → 7.51 ms (−15%)**. Predicted −30% from the byte count, and the gap is
itself the finding — see below.

### 2. A second level in the diff

Skipping 30% of the *bytes* bought only 15% of the *time*, because **unchanged megabytes are cheap
`memcmp` and the cost is byte-walking the chunks that differ.** With a 4096-byte chunk, one changed
byte drags a 4096-byte byte-loop behind it, and the staging pool's changes are exactly that shape — a
census found 6,560 changed bytes arriving as 4,564 separate runs, i.e. nearly all isolated.

So a differing chunk is now subdivided into 256-byte blocks before any byte is examined individually.
An isolated changed byte costs one 4096-byte `memcmp`, sixteen 256-byte `memcmp`s and a 256-byte
byte-loop instead of a 4096-byte byte-loop. Output identical, guarded against a naive reference with
cases derived from **both** constants.

Measured together: **8.785 → 6.586 ms per delta (1.33×)**.

## End-to-end result, and its honest weight

Eight interleaved pairs on the board, yesterday's shipped state against today's:

| | median gap | fps |
|---|---|---|
| BEFORE | 49.5 ms | 20.2 |
| AFTER | **45.0 ms** | **22.2** |

**1.10×, Mann-Whitney p = 0.17 — not significant at n = 8.** The *mechanism* is solid and directly
measured (8.79 → 6.59 ms, 1.33×); the end-to-end effect is smaller than the run-to-run spread of this
board and this sweep does not establish it. Reported as what it is.

**Do not chain the sweeps into a single ratio.** This sweep's BEFORE arm ran at 49.5 ms while an
earlier sweep of the *same binary* ran at 71 ms: board conditions differ between sweeps, which is
exactly why every comparison here is interleaved and within one sweep. The defensible statements are
per-sweep: yesterday's chunk fix was **1.48× (p = 0.0015)**, today's pair is **1.10× (p = 0.17)**, and
the board now runs vkcube at **~22 fps** against **~9.5 fps** before any of this week's work.

## Reproducing

```
python3 probe-softdirty-basic.py             # on each machine
python3 probe-softdirty-shared-memfd.py      # the cross-process, shared-memfd case
PAIRS=8 SECS=25 A_BIN=... B_BIN=... scripts/wp0-milkv-ab.sh
PAIRS=1 RELAXSTAT=1 ... scripts/wp0-milkv-ab.sh   # then the stage analysis
```
