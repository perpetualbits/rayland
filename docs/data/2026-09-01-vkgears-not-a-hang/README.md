# `vkgears` never hung on riscv64 — three counters and one missing flag did

**This directory retracts the finding recorded earlier the same day in
[`../2026-09-01-vkgears-riscv64/`](../2026-09-01-vkgears-riscv64/).** That finding — "`vkgears` +
Venus + Rayland + riscv64 hangs" — does not survive re-measurement. The application renders from the
milkv board at the board's ordinary frame rate. What was failing was the instrumentation, in three
separate places, plus one genuinely silent hardware failure mode that this project had already
documented and one harness had never been given the flag to avoid.

---

## The measurement that settles it

The board as C, dop561 as S, headless weston, `/usr/local/bin/vkgears`, the same cross-compiled
release `rayland-c` and the same release `rayland-s` in both arms, run back to back within a few
minutes of each other. **The only variable is whether S's Vulkan loader is pinned to the Intel iGPU.**

| S's `VK_ICD_FILENAMES` | run 1 | run 2 | run 3 | run 4 | median inter-frame gap |
|---|---|---|---|---|---|
| **unpinned** (Intel + NVIDIA enumerated) | 0 | 0 | 0 | 0 | — |
| **pinned to Intel** | **659** | **621** | **577** | **583** | **41–47 ms**, zero stalls |

Attaches are `wl_surface.attach` requests forwarded by C in 30 s. Complete separation, 4/4 against
4/4. 41–47 ms/frame is precisely the ~45 ms/frame this project already records for that board, so the
pinned runs are not merely non-zero, they are *normal*.

Raw: [`milkv-weston-UNPINNED.tsv`](milkv-weston-UNPINNED.tsv),
[`milkv-weston-PINNED.tsv`](milkv-weston-PINNED.tsv), and a full proxy log from a rendering run,
[`milkv-pinned-rendering-c.log.gz`](milkv-pinned-rendering-c.log.gz).

**The zero rows are the documented NVIDIA RTX A500 device loss** (`CLAUDE.md`, 2026-07-26): the real
`vkQueueSubmit` on S returns `VK_ERROR_DEVICE_LOST`, buffers get created, a commit or two happens, and
nothing is ever presented — with no error in any log on either side. From outside it is
indistinguishable from the application blocking, which is exactly what it was mistaken for.
`wp0-soak.sh` and `milkv-demo.sh` were both given the Intel pin on 2026-09-01. **`wp0-milkv-ab.sh`
was not**, and it is the harness that produced the riscv64 evidence.

## The archived "hang" log disagreed with its own conclusion all along

`../2026-09-01-vkgears-riscv64/milkv-hang-protocol.log.gz` is the protocol log of the run those
metrics came from — its span, 40.3 s, matches that directory's `elapsed_us=40298166` exactly. Scored
with [`../../scripts/attach-count.awk`](../../scripts/attach-count.awk):

```
attaches                 634      over 35.1 s = 18.0 FPS
wl_callback created      634
wl_buffer.release        632      delivered to the application
```

Its own README says the application "never attaches a buffer" and describes "an application that draws
nothing". It drew 634 frames and was handed 632 buffer releases. The claim was carried forward from
the *earlier* investigation in [`../2026-09-01-vkgears-blocked/`](../2026-09-01-vkgears-blocked/) —
whose four logs really do show 0 attaches over 4.4–5.9 s — and applied to a new run without re-scoring
that run's log. Those four earlier runs also predate the Intel pin, so the most likely explanation for
them is the same device loss; that is *strongly indicated but not proven*, because those runs cannot
now be re-taken under a known ICD.

## Why every counter said zero

All three harnesses counted frames as `grep -c 'forward obj 3 opcode 1'`.

**Object 3 is `vkcube`'s `wl_surface`. It is not a constant** — it is whatever id the application's
own Wayland client library happened to allocate, and `vkgears` allocates **6**. So every `vkgears` run
any of the three harnesses ever scored reported zero frames, identically for a run at 33 FPS and a run
that had genuinely stopped.

`milkv-demo.sh` printed that zero to the terminal every ten seconds — `frames presented on dop561: 0`
— while a human watched. That is where the word "hang" came from. Re-run today with the id read from
the log instead of assumed, the same command on the same board prints:

```
t=10s  frames presented on dop561: 270
t=60s  frames presented on dop561: 910
```

The fix is [`../../scripts/attach-count.awk`](../../scripts/attach-count.awk), now the single shared
scorer for all three harnesses: it reads `objects+ app_obj=N wl_surface` out of the proxy's own object
table and counts opcode-1 requests only on those ids. Two copies would drift, and a wrong frame count
does not look wrong — it looks like a result.

## A zero-frame run was scored PASS

In `wp0-soak.sh`, `grep -c` **exits 1 when the count is zero** and still prints `0`, so the `|| echo 0`
fallback fired as well and the variable became the two-line string `"0\n0"`. The liveness comparison
then failed with `[: 0\n0: integer expected`, the failure mode was never recorded, and the run was
scored **PASS**. The harness reported a passing run in which the application presented nothing — and
did so only in the zero case, which is the one case that matters.

## Found while fixing the above: the two-machine path ran no application at all

`wp0-soak.sh` guards against clobbering a caller-named binary like this:

```sh
VKCUBE="${VKCUBE:-/usr/bin/vkcube}"                                  # default applied FIRST
if [ -n "$C_HOST" ] && [ -z "${VKCUBE+set}" ]; then VKCUBE=/tmp/vkcube; fi   # ...then tested
```

The default assignment is what destroys the information the test needs. `${VKCUBE+set}` is therefore
never empty, the rewrite never fires, and every two-machine run asked C to execute `/usr/bin/vkcube` —
a path that exists on this laptop and **does not exist on apollo**. `nohup` had nothing to run, the app
log stayed empty, no application ever started, and the run was scored on an empty log. Combined with
the `grep -c` bug above, such a run was reported **PASS**.

Verified fixed, with the per-run witness (new, and see below) naming what actually executed:

```
vkcube  : witness /usr/lib/x86_64-linux-gnu/libvulkan_virtio.so /tmp/vkcube
          PASS attaches=836 median gap 25 ms
vkgears : witness /usr/lib/x86_64-linux-gnu/libvulkan_virtio.so /usr/bin/vkgears.x86_64-linux-gnu
          PASS attaches=811 median gap 25 ms
```

Raw: [`apollo-weston-vkcube.tsv`](apollo-weston-vkcube.tsv),
[`apollo-weston-vkgears.tsv`](apollo-weston-vkgears.tsv).

## The witness, so this class of error is visible next time

Every knob in these harnesses names a path, and a path that is wrong, stale or shadowed fails
**silently**: the loader falls back, the run completes, and a number is produced for a different
program than the caller believes they measured. `wp0-soak.sh` now records, per run, from the kernel
rather than from its own variables:

- which `libvulkan_virtio.so` the application actually mapped (`/proc/<pid>/maps`), and
- which executable the process actually is (`/proc/<pid>/exe`).

It also refuses to start when the application binary is not executable on C, rather than producing a
table of zeroes.

## What is NOT claimed here

- **Not** that the board is fast. 41–47 ms/frame is ~22 FPS, against 25 ms on an x86_64 C. The board
  being slow is a separate, real, and already-recorded fact.
- **Not** that the four `../2026-09-01-vkgears-blocked/` runs were device loss. That is the most
  likely explanation and it is consistent with all four of that session's negative controls — none of
  which touched the GPU selection — but it is inference, not measurement.
- **Not** that the Mesa 26.0.8 / 26.1.6 difference has been tested. It has not, and it no longer needs
  to be: it was the last confound of a defect that turns out not to exist. The `APP_ICD` knob added to
  `wp0-soak.sh` makes that experiment a one-liner if a future session ever wants it, and a Debian sid
  `libvulkan_virtio.so` (26.1.6, amd64) is extracted at `/tmp/mesa-sid/` on apollo with an ICD
  manifest beside it.

## Reproducing

```sh
# renders, ~22 FPS, 4/4
PAIRS=2 SECS=30 APP=/usr/local/bin/vkgears APP_ARGS=" " \
  A_SBIN=/tmp/rayland-c1-target/release/rayland-s B_SBIN=/tmp/rayland-c1-target/release/rayland-s \
  scripts/wp0-milkv-ab.sh

# the same thing with S enumerating both GPUs — zero attaches, no error anywhere
S_ICD= PAIRS=2 SECS=30 APP=/usr/local/bin/vkgears APP_ARGS=" " scripts/wp0-milkv-ab.sh

# on the owner's real screen
SECONDS_TO_RUN=60 APP=/usr/local/bin/vkgears APP_ARGS= \
  C_BIN=/tmp/rv/riscv64gc-unknown-linux-gnu/release/rayland-c \
  S_BIN=/tmp/rayland-c1-target/release/rayland-s scripts/milkv-demo.sh
```
