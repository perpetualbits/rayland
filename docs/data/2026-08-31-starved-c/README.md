# The forward-coalescing change under a CPU-starved C, 2026-08-31

## Why this exists

Two mechanism fixes landed on 2026-08-31 — S's `reply_arena_fence_signaled` (8× at the median) and
C's forward message coalescing (6.1× fewer messages) — and **neither moved the frame rate on this
laptop**, where C has CPU to spare. The claim the second one actually makes is about a machine where
C does *not*: the riscv64 milkv board that manages 5 fps.

**CORRECTION, 2026-08-31, later the same day. The paragraph that stood here was wrong, and it is
left below so the error is visible rather than erased.**

The rig exists. It is in a **Debian sid riscv64 chroot on a second card** — `/mnt/build`,
`/dev/mmcblk1p1`, **117 GB with 106 GB free**, mounted from `/etc/fstab` by UUID — and it holds
`virtio_icd.json` (Venus), `lvp_icd.json` (lavapipe), `vkcube`, a patched `vkgears`, Mesa **26.1.6**
(newer than the 26.0.3 these docs record as the working C side), and a prebuilt release `rayland-c` at
`/mnt/build/rl-c1-target/release/`. Verified by direct inspection, not taken on report. The chroot
exists precisely *because* the host cannot hold it: the host is a Debian ports snapshot frozen at
2022-12-25 whose apt reaches nothing newer.

So **the decisive weak-C test is available and was never blocked.** What was blocked was only the
thing I looked for, on the filesystem I looked at.

**What I got wrong, and it is a method error worth more than the fact.** I ran `ls` on the *host* root,
found no Venus ICD and 1.3 GB free, and concluded "the board cannot run this". I had checked one
filesystem and reported a property of the machine. The whole rig was one `df` away — and `df` is the
command you run *before* concluding a machine is out of space, not after. A negative result about
someone else's environment deserves at least the effort of a positive one.

The decision not to install onto the host stands and was right — 1.1 GB free — but "do not install
here" was never the same claim as "cannot be tested", and I stated the second while only having
evidence for the first.

(The cross-compilation datum below still holds and is still useful: `rayland-c` builds cleanly for
`riscv64gc-unknown-linux-gnu` from this laptop, which is how an A/B pair gets onto the board without a
43-minute native build.)

> ~~**The real test could not be run.** milkv has rebooted, `/tmp` was cleared, and it now has neither
> `vkcube` nor Mesa's virtio (Venus) ICD — only `radeon_icd.riscv64.json`. Installing a Vulkan stack
> onto a board whose root filesystem is 92% full (1.3 GB free) was not something to do uninvited; a
> full debug target directory for this workspace is 9 GB, so building there is out too.~~ — **wrong,
> see the correction above.**

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
that split. The decisive test remains a real run on milkv — which, per the correction at the top of this
file, **is available** in the `/mnt/build/sid` chroot and has not been run.
