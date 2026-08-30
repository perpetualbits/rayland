# The 64-byte compare chunk was costing ~11 ms a frame — 2026-08-30

**Topology: loopback on dop561, COSMIC, `vkcube --gpu_number 0`.** Measured, not estimated.

## What was found

`Applier::take_venus_blob_writes` holds the applier lock — the one the ring relay needs — while
diffing every Venus-internal blob against its shadow. Instrumenting the existing `section_log` at a
1 µs threshold instead of its 50 ms default:

| section | calls / 20 s | median | total lock-held |
|---|---:|---:|---:|
| **`take_venus_blob_writes`** | 970 | **4.94 ms** | **5.36 s** |
| `reply_arena_fence_signaled` | 970 | 1.05 ms | 1.67 s |
| `take_ring_progress` | 45,942 | 2 µs | 0.15 s |
| | | | **7.18 s of 20 s = 36%** |

~4.85 calls per frame × 4.94 ms ≈ **24 ms of a ~40 ms per-frame Rayland budget, in one function.**

A non-destructive per-call log of *what it scans* showed 713 of 744 calls walking **9,437,184 bytes**:
`res=2` (1 MiB reply arena) + **`res=3` (8 MiB staging pool)**. And a log of what each *yields*:
**545 yield lines, every one `res2`. The 8 MiB staging pool never produced a single run.** It is
C-written; S never writes it, so diffing it cannot ever produce anything.

## Why the fix is the chunk size and not an exclusion

The obvious move — stop scanning the staging pool — needs a way to identify it, and there isn't a
sound one: both it and the reply arena are `blob_id == 0`, and the record already rejected using
`vkSetReplyCommandStreamMESA` to find the arena as *"silently unsound, because the reply pool mints a
new id when it grows"*. Size-based classification would be a guess.

The diff already had a chunked fast path, comparing whole chunks and byte-scanning only chunks that
differ — **so the chunk size cannot change what the function returns.** It was `64`. Over 9.4 MiB that
is ~147,000 slice comparisons, each far too short for `memcmp` to amortise: ~1.9 GB/s against the
>10 GB/s a vectorised compare reaches here.

`4096` makes it ~2,300 page-sized comparisons. Semantics are provably untouched; the project's
existing exact-run tests (which assert run counts and contents byte for byte) are the guard, and pass.

## Result — interleaved A/B, four runs each, two binaries

| CHUNK | runs (fps) | median |
|---|---|---:|
| 64 | 14.33, 14.13, 14.33, 15.20 | **14.33** |
| 4096 | 17.26, 16.66, 16.53, 18.00 | **16.96** |

**+18%, or −10.8 ms per frame.** No overlap between the two distributions.

Interleaving mattered: a first, non-interleaved pair gave 9.4 and 17.7 fps, which would have supported
any conclusion at all. Run-to-run variance here is large enough to swamp the effect.

Section timings after:

| section | median | total lock-held |
|---|---:|---:|
| `take_venus_blob_writes` | **1.23 ms** (was 4.94) | **1.51 s** (was 5.36) |
| `reply_arena_fence_signaled` | 1.18 ms | 2.49 s |
| | | **4.25 s of 20 s = 21%** (was 36%) |

## The next target, from the same measurement

**`reply_arena_fence_signaled` is now the largest**, at 1.18 ms median and 2.49 s of lock-held time per
20 s. It scans the 1 MiB reply arena for a contiguous `[38][0]` pattern on every poll. ~4.8 calls per
frame × 1.18 ms ≈ **5.7 ms/frame**, and it holds the same lock.

It should be possible to look only where the arena actually changed — the diff pass already knows
that — rather than rescanning a megabyte. **Not attempted here**: the barrier it implements is the
(c)2 correctness fix that took three dead ends to get right, and it deserves its own task with its
own tests rather than being altered in passing.
