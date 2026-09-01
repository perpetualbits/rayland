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
