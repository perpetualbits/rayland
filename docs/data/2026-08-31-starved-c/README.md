# The forward-coalescing change under a CPU-starved C, 2026-08-31

## Why this exists

Two mechanism fixes landed on 2026-08-31 — S's `reply_arena_fence_signaled` (8× at the median) and
C's forward message coalescing (6.1× fewer messages) — and **neither moved the frame rate on this
laptop**, where C has CPU to spare. The claim the second one actually makes is about a machine where
C does *not*: the riscv64 milkv board that manages 5 fps.

**The real test could not be run.** milkv has rebooted, `/tmp` was cleared, and it now has neither
`vkcube` nor Mesa's virtio (Venus) ICD — only `radeon_icd.riscv64.json`. Installing a Vulkan stack
onto a board whose root filesystem is **92% full (1.3 GB free)** was not something to do uninvited; a
full debug target directory for this workspace is 9 GB, so building there is out too. (`rayland-c`
*does* cross-compile cleanly for `riscv64gc-unknown-linux-gnu` from this laptop — the toolchain and
Rust std are both installed — so only the board's Vulkan stack is missing.)

So this measures the *causal claim* instead, on hardware that is available: put `rayland-c` under a
hard CPU quota and see whether the reduced send cost converts into frame rate.

`C_WRAP` in `scripts/wp0-soak.sh` is the hook, here:

```
C_WRAP='systemd-run --user --scope -q -p CPUQuota=15% -p AllowedCPUs=0 --'
```

15% of one core takes the frame gap from ~61 ms to ~300 ms, so C is unambiguously the bottleneck.

## Result — 13 interleaved pairs

| | before | after | |
|---|---|---|---|
| C→S messages per run | 4,124 | **709** | **5.8×** |
| time inside `send()` per run | 4,362 ms | **1,662 ms** | **2.6×** |
| median frame gap | 307 ms | 298 ms | **1.03× — essentially unchanged** |

**The medians barely move, and yet the distributions differ significantly** (Mann-Whitney U = 130 of
169, two-tailed **p = 0.021**). The reason is visible in the raw values:

```
before: 299 299 301 304 304 306 307 316 | 395 398 401 402 403
after:  206 289 292 293 297 297 298 302 309 310 312 314 | 384
```

The un-coalesced arm falls into a **~400 ms regime in 5 of 13 runs**; the coalesced arm enters it
**once in 13**. So what coalescing buys a starved C is not a faster median — it is not falling into
the slow regime. That is worth having, but it is a different claim from "the frame rate goes up", and
it should not be quoted as one.

**Honest limits.** A CPU quota is a *simulation* of a weak C, not a weak C: it starves cycles without
reproducing riscv64's memory bandwidth, cache, or instruction throughput, and it starves `rayland-c`
alone while the application still runs at full speed. The 5/13-vs-1/13 regime split is itself only
p ≈ 0.16 by Fisher's exact test taken on its own; the p = 0.021 above is the whole distribution, not
that split. The decisive test remains a real run on milkv, and it is blocked on the two missing
packages above rather than on anything in Rayland.
