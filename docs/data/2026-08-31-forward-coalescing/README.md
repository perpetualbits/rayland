# Forward-path (C→S) run coalescing, 2026-08-31

Evidence for the diary entry *"Coalescing the forward path, and the safety check that decided its
shape"*.

## The census that prompted it

From the same loopback link trace as `../2026-08-31-round-trip-attribution/`: one 60-frame `vkcube`
run sent **5,495 `C2S::BlobData`**, of which **5,409 carried one to three bytes each** — almost all of
them the 8 MiB Venus staging pool fragmenting under a byte-granular diff. That cost C roughly a second
inside `send()` per run to move a few kilobytes.

## The safety check, stated as the rule it produced

C keeps a per-blob **baseline** that is *C's model of what S holds*, kept in step by `note_s_wrote`.
Coalescing merges two changed runs across a gap and re-ships the gap **from that baseline**, so:

> Re-shipping an unchanged byte is safe exactly when the baseline is a faithful model of the
> receiver's copy for that byte.

The question is therefore *where can S's copy change without C being told?* Answered from S's code:
`emit_blob_writes` skips `self.rings`; `take_venus_blob_writes` skips `self.rings` and
`self.presented`. Rings are already excluded from the forward path, so **`presented` is the whole
hazard** — for those blobs S's GPU writes and deliberately never reports (the 2026-08-29 exclusion),
so C's baseline is stale by design.

Rule: **coalesce everything except blobs C has published as a `BufferToken`.** The mark is set in
`BlobInodeResolver::resolve_inode`, at `params.add` rather than `create_immed`, so it marks a superset
— an over-mark costs a missed optimisation, an under-mark costs pixels.

## `ab-BEFORE.txt`, `ab-AFTER.txt`

Interleaved A/B, `scripts/wp0-soak.sh` in `MODE=traffic` on loopback with `NO_BUILD=1` so each arm
runs its own preserved binary. Each block holds the run's `runs.tsv` row and the final `C1METRICS`
line from C.

| | before | after | |
|---|---|---|---|
| C→S messages per frame | 90.3 | **14.8** | **6.1×** |
| time inside C's `send()` per run | 1,644 ms | **303 ms** | **5.4×** |
| C→S bytes per frame | 5,645 | 6,027 | +6.8% (the bounded trade) |
| median frame gap | 61 ms | 61 ms | **unchanged** |

**Read `attaches` with care and do not derive a frame rate from it.** In `MODE=traffic` the harness
polls C's log every 2 s and stops once it sees `FRAMES` attaches, so the final count overshoots by a
variable amount and is dominated by poll granularity. `median_gap` is the rate measurement.

**The unchanged frame gap is the finding, not a disappointment.** Together with the morning's
`reply_arena_fence_signaled` fix, two independent 5–8× reductions in CPU work moved the frame rate by
nothing measurable — which says frame time here is a serialized latency chain, not CPU-bound on either
side.

## Reproducing

```
cargo build -p rayland-c -p rayland-s          # arm A: current tree
git worktree add --detach /tmp/wt <before-rev> # arm B
C_HOST= MODE=traffic RUNS=1 FRAMES=60 NO_BUILD=1 OUT=/tmp/fw scripts/wp0-soak.sh
grep -o 'C1METRICS.*' /tmp/fw/run1/c.log | tail -1
```
