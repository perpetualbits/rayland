# The icosa fixtures on the riscv64 board — where the mapped-write cost actually lands

Until now every measurement of `rayland-icosa-cpu` / `rayland-icosa-gpu` had used an x86_64 machine
as C, where C has CPU to spare — which is the case Rayland's premise says is *not* the interesting
one. The board is the weak C the whole design is aimed at. `vkcube` and `vkgears` had been run
against it; those are somebody else's programs, cannot be instrumented, and cannot be changed. These
are ours, and they carry per-frame timers that split the frame into the three parts that matter.

Harness: [`scripts/c2-icosa-milkv.sh`](../../scripts/c2-icosa-milkv.sh) (new) and
[`scripts/c2-icosa-two-machine.sh`](../../scripts/c2-icosa-two-machine.sh) (now takes `APP=cpu|gpu`).

---

## 1. Correctness first: every relayed frame is bit-identical

Each relayed frame is compared against the same fixture run **natively on S**, on S's Intel GPU with
no Venus in the path, so only the transport differs.

| configuration | runs | frames | differing | missing |
|---|---|---|---|---|
| `icosa-gpu`, milkv (riscv64) → dop561 | 6 | 720 | **0** | 0 |
| `icosa-cpu`, milkv (riscv64) → dop561 | 4 | 480 | **0** | 0 |
| `icosa-gpu`, apollo (x86_64) → dop561 | 1 | 120 | **0** | 0 |
| `icosa-cpu`, apollo (x86_64) → dop561 | 1 | 120 | **0** | 0 |

**0 of 1,200 frames over 10 runs with the board as C.** Read that at the frame level (a rate under
0.25% at 95% confidence, rule of three); at the *run* level 10 runs bound the rate only under 30%,
which is far too loose to call the (c)2 return path proven on this architecture. It is consistent
with the post-`G'` state and does not contradict the ~1/11-runs residual that `G'` retired — it is
not, by itself, strong evidence about it either.

## 2. The arithmetic contract, checked on a second architecture for the first time

The comparison above spans two architectures: S computes the fixture's per-frame fractal on x86_64,
the board computes it on riscv64, and the two must agree **bit for bit** or every frame differs for a
reason that has nothing to do with the relay. That works only because `rayland-icosa-core` builds its
`log2`/`sin`/`cos` out of IEEE-754 basic operations for exactly this purpose — and **that contract
had only ever been executed on x86_64.**

Checked before any of the above was trusted, inside the board's sid chroot:

```
rayland_icosa_core unit tests   23 passed
tests/log2_table.rs              3 passed
tests/sin_cos_table.rs           3 passed
```

Those table tests are not self-consistency checks against the host's own libm — `CASES` pairs each
input with a **committed raw `f64` bit pattern**, generated once on one machine. Passing them on
riscv64 is the cross-host claim being met on a second architecture. Re-run them before believing any
future diff from this harness; if they ever fail there, every number here becomes a statement about
arithmetic instead of about Rayland.

## 3. Where the time goes — median per frame, frame 0 excluded (bring-up)

| | fractal | upload | draw+readback | total |
|---|---|---|---|---|
| **`icosa-gpu`** — fractal in the shader, ~80 B/frame | | | | |
| native on S (x86_64) | 0.0 | 0.0 | 1.7 | **1.7 ms** |
| apollo (x86_64) → S | 0.0 | 0.0 | 10.1 | **10.1 ms** |
| milkv (riscv64) → S | 0.0 | 0.0 | 51.9 | **51.9 ms** |
| **`icosa-cpu`** — fractal on C's CPU into mapped memory, ~1 MiB/frame | | | | |
| native on S (x86_64) | 148.8 | 6.4 | 2.1 | **163.2 ms** |
| apollo (x86_64) → S | 56.7 | 20.8 | 15.8 | **96.1 ms** |
| milkv (riscv64) → S | 683.5 | 101.1 | 50.9 | **850.5 ms** |

What each column is, from the fixture's own frame loop:

- **`fractal`** — `fractal::render_into(staging.bytes(), …)`, a pure CPU write of a megabyte into
  persistently-mapped `HOST_COHERENT` memory. **There is no Vulkan call in it at all.** This is the
  uninterceptable mapped write (c)2 exists for.
- **`upload`** — `texture.upload()`, the single Vulkan call that says "copy this buffer into that
  image", fence-waited to completion. It says nothing about *which* bytes changed.
- **`draw+readback`** — the draw and the read-back of the rendered pixels: the synchronous round trip.

### 3.1 The result: the round trip does not care about mapped-write volume; the upload does

**On one machine, `draw+readback` is essentially the same for both fixtures** — 51.9 against 50.9 ms
on the board, 10.1 against 15.8 on apollo — even though one of them has just written a megabyte
through mapped memory and the other has written eighty bytes. The relay's per-frame round-trip cost
is a property of the round trip, not of the data volume.

**The megabyte's cost lands almost entirely on `upload`**: 6.4 ms native → 20.8 ms from apollo →
101.1 ms from the board. That is the one call at which C must ship what the fractal wrote, and it is
where (c)2's mapped-memory problem shows up as a number: **roughly 20 ms per MiB from a strong C and
100 ms per MiB from the weak one.**

### 3.2 Rayland's costs scale with the board's general slowness, not superlinearly

apollo → milkv, for the two parts that are Rayland's:

- `upload` 20.8 → 101.1 ms = **4.9×**
- `draw+readback` 10.1 → 51.9 ms = **5.1×**

Two independent mechanisms moving by the same factor is the board simply being about five times
weaker on this path. Nothing about the weak C makes the relay disproportionately worse, which is the
outcome the design wanted and had not previously been able to claim from measurement.

### 3.3 On the weak C, the application is the bottleneck — not the relay

`icosa-cpu`'s 850.5 ms frame on the board is **683.5 ms of the application's own CPU** computing the
fractal (80% of it), against 152 ms for everything Rayland does. The board's CPU is 12× slower than
apollo's at that float work, against ~5× on the relay path.

**Do not quote 850 ms as a Rayland cost.** It is mostly a fixture deliberately built to burn CPU.
The corresponding honest figure for the relay on that board is `upload + draw+readback` = ~152 ms
for a megabyte a frame, and ~52 ms when there is no mapped-write volume at all.

## 4. What this does NOT settle

- **It is not the mapped-memory break.** Both machines' fixtures still reach S with their mapped
  writes intact, because C relays them. The case this pair was originally built to expose — writes
  that *cannot* be carried — is not what is exercised here; this measures what carrying them costs.
- **`fractal` is not comparable across C machines as a baseline.** apollo's 56.7 ms beats dop561's
  native 148.8 ms because apollo's CPU is faster, not because of anything in the relay.
- **n is small.** One run per configuration for the timing table; the medians are over 119 frames
  each, but a second run could move them. The correctness table is the one with repeats.

## 5. Reproducing

```sh
# the board (needs the sid chroot up: sudo /mnt/build/chroot-mounts.sh up)
CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER=riscv64-linux-gnu-gcc \
CC_riscv64gc_unknown_linux_gnu=riscv64-linux-gnu-gcc CARGO_TARGET_DIR=/tmp/rv \
  cargo build --release -p rayland-icosa-cpu -p rayland-icosa-gpu -p rayland-c \
              --target riscv64gc-unknown-linux-gnu
APP=cpu RUNS=3 scripts/c2-icosa-milkv.sh
APP=gpu RUNS=5 scripts/c2-icosa-milkv.sh

# x86_64 C, same fixtures, same comparison
APP=gpu scripts/c2-icosa-two-machine.sh 1
APP=cpu scripts/c2-icosa-two-machine.sh 1

# the arithmetic contract on the board
cargo test --release --no-run -p rayland-icosa-core --target riscv64gc-unknown-linux-gnu
# scp the three test binaries into /mnt/build/sid/tmp and run them under chroot — NOT on the
# host root, whose glibc is 2.36 and which fails with `GLIBC_2.39 not found` before main.
```

Raw per-frame CSVs are in [`csv/`](csv/); the correctness counts are in
[`run-ledger.csv`](run-ledger.csv).

---

# 6. Sizing the prize for dirty-page tracking — and it is scanning, not shipping

The handover names dirty-page tracking as the structural fix for frame time, notes it carries a
silent-corruption hazard (`clear_refs` racing a write), and observes that **riscv64 cannot do it**.
Before building a mechanism with that risk, the prize is worth measuring on the machine that matters.

C's stage recorder (`RELAXSTAT=1` on [`../../scripts/c2-icosa-milkv.sh`](../../scripts/c2-icosa-milkv.sh))
brackets each ring delta as `RingShipped → SyncPrepared → SyncSent`. The first interval is the
forward path's *work*: **every blob diffed against its baseline**, then serialized. The second is
writing and flushing. Both fixtures, board as C, one run each — the runs were still 120/120
bit-identical, so the recorder did not disturb correctness.

| | deltas | per frame | **diff**, median | diff total | send, median | send p90 | send total |
|---|---|---|---|---|---|---|---|
| `icosa-cpu` (~1 MiB/frame changes) | 526 | 4.4 | **10.86 ms** | 5.61 s | 0.32 ms | 62.5 ms | 7.10 s |
| `icosa-gpu` (~80 B/frame changes) | 206 | 1.7 | **8.92 ms** | 1.79 s | 0.13 ms | 0.93 ms | 0.09 s |

## 6.1 The finding: 82% of the diff is scanning memory that did not change

**`icosa-gpu` changes about eighty bytes per frame and still pays 8.92 ms per delta.** The fixture
that writes a full megabyte through mapped memory pays 10.86. So the megabyte's *own* contribution to
the diff is about **1.9 ms**, and the other **8.9 ms — 82% of the cost — is `memcmp` over memory that
did not change**: the 8 MiB Venus staging pool, the 1 MiB reply arena, and the swapchain images the
application never CPU-writes, re-scanned on every delta to rediscover that they are the same.

That is precisely the cost dirty-page tracking removes, and it is now a number rather than a
prediction:

- `icosa-gpu`: 8.92 ms × 1.7 deltas/frame = **15.2 ms/frame, 29% of that fixture's entire 51.9 ms
  frame on the board.**
- `icosa-cpu`: 10.86 × 4.4 = **47.8 ms/frame, 31% of Rayland's ~152 ms share.**

`icosa-gpu` is the shape of an ordinary application — one that does not push megabytes through
mapped memory — so **29% of frame time on the weak C** is the honest size of the prize.

## 6.2 The cruel part

The mechanism cannot run where it is worth the most. The board is kernel 5.15 with no
`CONFIG_HAVE_ARCH_SOFT_DIRTY` (proven by probe); `UFFD_WP_ASYNC` needs 5.19+ and `PAGEMAP_SCAN` 6.7+.
soft-dirty works on dionysus (x86_64), where the same scan costs proportionally less — the machine
that needs it least is the one that can have it.

## 6.3 What this suggests instead, and it needs no kernel support

The waste is not "we cannot tell which *pages* changed". It is that **every blob is scanned on every
delta**, including ones that cannot have changed. A cheaper filter than page tables may exist above
the kernel — the relay already knows which blobs are rings, which are `presented`, and which the
application has mapped writable at all. That is a protocol-level question, it is portable to riscv64,
and it is *not* the design the handover proposed. It is untested and is recorded here as a direction,
not a claim.

## 6.4 Also visible: the send tail is the megabyte, and it is real

`icosa-cpu`'s `SyncPrepared → SyncSent` has a median of 0.32 ms and a p90 of **62.5 ms**, totalling
7.10 s — *more* than its diff. `icosa-gpu`'s totals 0.09 s. That is backpressure from actually
shipping a megabyte a frame over the link, and unlike the scan it is **not** waste: for this fixture
every one of those bytes genuinely changed. Dirty-page tracking would not reduce it by a byte. Any
future claim that dirty-page tracking speeds up `icosa-cpu` should be checked against this column
first.

Raw traces: [`relaxstat-milkv-cpu.dat.gz`](relaxstat-milkv-cpu.dat.gz),
[`relaxstat-milkv-gpu.dat.gz`](relaxstat-milkv-gpu.dat.gz) (`t_ns stage`, one event per line).

---

# 7. The protocol-level filter: proposed here, and refuted here (2026-09-02)

§6 ended by naming a direction — skip diffing blobs that *cannot* have changed, using a signal that
is not a kernel page table and not a decode of the ring. It was recorded as a direction and not a
claim. It was then spiked, and **it does not pay.** This section is the refutation, written up so
nobody re-proposes it.

The probe is `blobscan`, an env-gated instrument in
[`crates/rayland-c/src/blob_sync.rs`](../../crates/rayland-c/src/blob_sync.rs) that attributes the
per-delta scan to individual blobs. Both fixtures stayed **120/120 bit-identical** with it armed.

```
BLOBSCAN=1 APP=gpu scripts/c2-icosa-milkv.sh
BLOBSCAN=1 APP=cpu scripts/c2-icosa-milkv.sh
```

## 7.1 Where the scan time is

Per delta, board as C (raw: [`blobscan-milkv-gpu.txt.gz`](blobscan-milkv-gpu.txt.gz),
[`blobscan-milkv-cpu.txt.gz`](blobscan-milkv-cpu.txt.gz)):

| blob | size | `icosa-gpu` | `icosa-cpu` | changed on |
|---|---|---|---|---|
| Venus staging pool | 8 MiB | **8.06 ms — 85%** | **7.91 ms — 68%** | 41–47% of scans |
| the app's own staging buffer | 1 MiB | — | 2.20 ms — 19% | 23% of scans |
| a non-application blob | 1 MiB | 1.09 ms — 11% | 1.12 ms — 10% | **never** |
| an application blob | 256 KiB | 0.34 ms — 3% | 0.34 ms — 3% | **never** |
| the rest | ≤2 KiB | 0.01 ms | 0.01 ms | — |
| **total** | | **9.50 ms** | **11.59 ms** | |

Scan bandwidth is **0.93–1.02 GB/s** in both, so the cost is exactly `bytes ÷ 1 GB/s`. The scan is
already running at the board's memory bandwidth: **no chunk-size or constant tuning can help it, and
the only lever is scanning fewer bytes.**

## 7.2 Why no filter gets those bytes

1. **The dominant blob cannot be skipped.** The 8 MiB Venus staging pool genuinely changes on 41–47%
   of deltas — and ships only **~500–600 bytes** when it does. We scan 8 MiB to find 600 bytes. That
   is a *narrowing* problem, not a *skipping* one, and narrowing needs to know which region changed:
   either the decoder — banned by (c)1 §7 and enforced by
   [`decoder_is_not_load_bearing.rs`](../../crates/rayland-c/tests/decoder_is_not_load_bearing.rs) —
   or dirty-page tracking, which riscv64 does not have. **There is no third signal.**
2. **The filterable remainder is only 13–15%, and filtering it is unsound.** "Has not changed yet"
   does not imply "will not change". Establishing that a blob has not changed *is* the scan, so the
   check costs exactly what it would save, and getting it wrong ships nothing while the application
   believes it shipped — silent staleness, on the relay path.
3. **No static property discriminates.** `is_application_memory` is useless here: application memory
   both never-changes (the 256 KiB blob) and changes every frame (`icosa-cpu`'s 1 MiB staging
   buffer). Resource ids are not stable either — the pool is `res 6` in one fixture and `res 7` in
   the other.

Also checked: the pool's size is not a knob. Mesa grows it through
`vn_renderer_shmem_pool_grow_locked` and exposes no environment variable for it, so shrinking the
dominant term would mean patching Mesa.

## 7.3 The recommendation

**Do not build a protocol-level filter.** Dirty-page tracking is the right mechanism — it is the only
thing that addresses the 68–85% — and the honest position is the uncomfortable one from §6.2: it
works on x86_64, where the scan is worth ~19% of frame time, and cannot run on the riscv64 board,
where it is worth ~29%. Anyone picking this up should start from that sentence, not from a filter.

## 7.4 One unverified observation, flagged as unverified

The pool *grows* (`vn_renderer_shmem_pool_grow_locked`), which raises the possibility that the 1 MiB
non-application blob that **never changes in either fixture** is a *retired* pool chunk Venus has
stopped writing — about 10% of the scan spent on abandoned memory. That is a guess from a function
name and a zero. It has not been verified, and even if true there is no signal by which C could learn
that Venus is finished with a shmem, so it would not by itself yield a filter.
