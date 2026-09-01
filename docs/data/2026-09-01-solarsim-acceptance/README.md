# `wl_shm` acceptance: `solarsim` on milkv, rendering on dop561

The end-to-end criterion from the design's §13, run for the first time on 2026-09-01.

## Result against the four criteria

| # | criterion | result |
|---|---|---|
| 1 | `solarsim` starts without `WaylandError(Bind(NotPresent))` | **met** — it starts and reaches Vulkan |
| 2 | `solarsim` renders on dop561's display, driven from milkv | **met** — **169 frames presented** in ~120 s |
| 3 | `vkcube` and the offscreen `rayland-refapp` unaffected | **met** — vkcube 34/25 ms clean; refapp covered by the suite (69 binaries green) |
| 4 | the summary reports shm traffic, confirming or refuting §3 | **met, and it confirms §3 emphatically** — see below |

`GPU: Virtio-GPU Venus (Intel(R) Iris(R) Xe Graphics (RPL-P)) [IntegratedGpu, Vulkan]` — an unmodified
wgpu/winit application, running on a riscv64 board, drawn by dop561's GPU, on dop561's real desktop.

## Criterion 4: the furniture assumption, settled

§3 predicted that for a GPU application `wl_shm` carries *furniture* — cursors and decorations — not
frames, and §8 deferred content hashing, damage intersection and compression until that was measured
rather than assumed. Measured:

```
intercept wl_shm.create_pool -> pool 6  (2 bytes;    fd kept on C)
intercept wl_shm.create_pool -> pool 46 (1024 bytes; fd kept on C)
shm_bytes=0  shm_commits=0  shm_largest=0
```

**Two pools, of 2 and 1024 bytes** — 1024 being exactly a 16×16 ARGB cursor — and across two minutes
**not one byte was ever synced**, because the application never committed a surface with an shm buffer
attached. The prediction was that this path carries kilobytes; the reality is that it frequently
carries nothing at all.

So §8's three optimisations are not merely unnecessary — there is nothing here for them to optimise.
That question is now closed with a number instead of a guess, which was the entire point of shipping
v1 without them.

## Two bugs the acceptance run found, and nothing else could have

Both were invisible to unit tests, to the integration test, and to `vkcube`, because both live in the
S-side replay of a protocol only a real toolkit application exercises.

**1. `wl_shm` was missing from S's interface registry.** C intercepted `create_pool` and forwarded it
perfectly; S logged `no linked descriptor for wl_shm; bind skipped` and carried on. The application
did not care — its *GPU* frames go through dma-buf — so the only symptom was a cursor that never
appeared. A table of "interfaces we support" cannot be tested for an omission by a test that lists the
same interfaces; only running something that binds one you forgot will find it.

**2. The substitution expanded back into one argument where the wire needs two.**
`create_pool(new_id, fd, int size)` has its `fd` replaced by `WaylandArg::ShmPool { size }` on the
wire — so S must expand that back into *both* the descriptor and the size. It pushed only the
descriptor, and `wayland-client` refused the request outright:

```
Unexpected signature for request wl_shm@5.create_pool: expected [NewId, Fd, Int], got [NewId(...), Fd(33)]
```

That is a good failure: loud, immediate, and naming the exact shape mismatch. A compositor quietly
reading a wrong length would have been far worse.

## A third finding, about adapter selection

The first run landed on the **NVIDIA RTX A500** — which loses the device on 7 of 14 runs, silently —
because `solarsim` asks wgpu for `PowerPreference::HighPerformance` and **no environment variable
overrides a hardcoded preference**. `vkcube` can be steered with `--gpu_number`, but that is an index
into a list whose order moves, and it only helps applications that offer such a flag.

The fix belongs on our side: `scripts/milkv-demo.sh` now sets `VK_ICD_FILENAMES` for **`rayland-s`**,
so S's Venus enumerates only the Intel device and no application can choose the broken one. Override
with `S_ICD=` to enumerate everything, which is what a session hunting the NVIDIA device loss wants.

## Reproducing

```
sudo /mnt/build/chroot-mounts.sh up
sudo cp -p /mnt/build/cargo-target/release/solarsim /mnt/build/sid/opt/solarsim/solarsim
SECONDS_TO_RUN=120 APP=/opt/solarsim/solarsim APP_ARGS= scripts/milkv-demo.sh
```

`cp -p` is deliberate: `solarsim` reports its own build age from `current_exe()` mtime, and a plain
copy would reset it and always claim the build was seconds old.
