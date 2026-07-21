# (c)2 — coalesce the readback's fragmented runs into few messages

**Status:** design, 2026-07-21. A performance follow-up to the shipped return-path fix
([`2026-07-21-c2-getfencestatus-completion.md`](2026-07-21-c2-getfencestatus-completion.md)), which is
correct but slow.

## Problem

`Applier::take_app_blob_writes` ships the readback buffer `res6` as one `S2C::BlobData` per
`WrittenRun`, and `HostBlob::changed_byte_ranges` (`blob.rs`) emits one run per **maximal run of
consecutive changed bytes**. Between two rendered frames many bytes are byte-identical (background, the
icosa's slow rotation), so the changed bytes are interspersed with tiny unchanged gaps — measured, the
readback shatters into **~5000 one-byte `BlobData` messages per frame**. Each is a framed QUIC message,
and the return path is **message-rate-bound** (the project's own finding: latency, not bandwidth, is
what hurts). Runs are visibly slow (minutes for 120 frames).

## Fix: gap-threshold coalescing on the readback path

Merge two runs `[a, b)` and `[c, d)` into `[a, d)` when the gap `c − b ≤ GAP` (256 bytes), re-shipping
the unchanged gap bytes `[b, c)`. This collapses the fragmentation to a handful of runs (for res6's
dense small-gap pattern, ~1 run) at the cost of re-shipping bounded runs of unchanged bytes. No wire
change.

### Why it is safe (and why only the readback path)
Re-shipping an *unchanged* byte is a clobber only if C holds a different value for it. `res6` is written
by S's GPU and **read-only on C** (the app maps it and reads its pixels; it never writes them), so an
unchanged gap byte S ships equals what C already has — idempotent. `take_app_blob_writes` only ever
carries `res6`: other application blobs (vertex/uniform) are forward-written by C and re-baselined to
empty by `copy_in`, so they produce no runs. The coalescing is therefore applied **only** to the
app-blob (readback) path and never to `take_venus_blob_writes` (the reply arena, where the fine grain's
clobber-avoidance still matters and which is small anyway). This preserves `changed_byte_ranges`'
fine-grain invariant everywhere it is load-bearing.

`take_bytes_s_wrote` already *adopts* each shipped run into the shadow; adopting a coalesced run's gap
bytes is idempotent (they equal the shadow already), so the shadow stays consistent.

## Mechanism

Add `coalesce_gap: usize` to the run-building path. `changed_byte_ranges` still returns the fine ranges;
a new pure helper `coalesce_ranges(ranges, gap)` merges adjacent ranges whose gap `≤ gap` before
`take_bytes_s_wrote` materialises them into `WrittenRun`s. `emit_blob_writes` passes the gap through:
`take_app_blob_writes` uses `GAP = 256`; `take_venus_blob_writes` uses `0` (no coalescing — identical to
today). `0` must be exactly inert (merge nothing), so the venus path is provably unchanged.

## Testing

- **Unit (the coalescing, pure):** `coalesce_ranges` — merges ranges within the gap, leaves ranges
  farther apart split, `gap = 0` is a no-op, empty input is empty, a single range is unchanged.
- **Unit (the diff still adopts correctly):** a coalesced `take_bytes_s_wrote` ships one run spanning the
  gap and adopting it, so a subsequent unchanged poll yields no runs.
- **Regression (correctness unchanged):** loopback `icosa_cpu` + `refapp` e2e still bit-identical;
  two-machine `icosa_cpu` still **0 stale** — and measurably **faster** (message count per frame drops
  from thousands to single digits).

## Out of scope

The byte-optimal multi-run `BlobData` (one message carrying many runs, no redundant bytes) is a
`rayland-relay` wire change deferred until WAN bandwidth, not message rate, is the constraint.
