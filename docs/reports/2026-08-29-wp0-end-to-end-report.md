# Report to planning — WP0 is end to end, and the demo was shipping a pixel stream

**Session:** 2026-08-29 evening/night, dop561 (S) + apollo (C). **Branch:** `wp0-wayland-proxy`.
**Evidence:** `docs/data/2026-08-29-wp0-recycled-id-fix/` (logs, and `traffic-before-after.md`).

> **Headline. WP0 4.5 is reached:** an unmodified `vkcube` on apollo, rendered by dop561's GPU,
> **spinning in its own window on dop561's screen** — confirmed by a human watching it. **And the
> return path was shipping every rendered frame back to C at ~877 KB/frame**, which is now fixed:
> S→C fell **571×**, from 105.25 MB to 184 KB over the same ~60 s workload.
>
> **This invalidates the prompt you had readied.** The stall it was written against is gone, and the
> two defects behind it were not where anyone was looking.

---

## 1. What was wrong, and neither was where the last report pointed

### Defect A — the vanishing window: a cached handle to a destroyed object

`WaylandReplay` recorded the S-side `ObjectId` of the `zwp_linux_dmabuf_v1` global at the moment the
**application** bound it — which is what the token→`wl_buffer` brief's decision 4 asked for, and it is
wrong. The application binds that global repeatedly while probing formats (**twelve times** in one
measured run) and **destroys each one**. The cached id named a dead object as soon as the app moved on:

```
WP0 4.3: step 1/3 create_params failed for resource 19: Invalid ObjectId
WP0 4.3: step 1/3 create_params failed for resource 20: Invalid ObjectId
WP0 replay: send_request (obj 3 opcode 1) failed: Invalid ObjectId   ← wl_surface.attach
```

No params object → no `wl_buffer` → the app's `attach` fails → and **a `wl_surface` with no valid
buffer is unmapped by definition**, so the compositor removed the window while the application carried
on unaware. That is the "appears briefly, then disappears" the owner reported.

**Fix:** S binds **its own** dmabuf global, once, and never destroys it. It needs *a* factory, not the
application's factory. This is the recycled-id lesson in its second form — *a handle you cached is not
a handle you still have* — introduced in `rayland-s` on the same day its twin was fixed in `rayland-c`.

**Result:** 4 buffers then dead → **12–36 buffers and 0 `Invalid ObjectId`**, 279 attaches in the first
20 s (~14 fps), and a cube that turns.

### Defect B — the demo was crossing the network with pixels

With the cube finally turning, the repository owner asked: *"We are cheating now, if I understand
correctly; because pixels are now crossing the wire, true?"* They were right.

| | Before | After |
|---|---:|---:|
| Frames | ~120 | 96 |
| C→S total (commands) | 804,814 B | 1,626,138 B |
| **S→C total** | **105,254,034 B** | **184,311 B** |
| **S→C per frame** | **~877 KB** | **~1.9 KB** |

A 500×500×4 frame is 1,000,000 bytes. **S was shipping every rendered frame back to a machine with no
display**, where nothing consumed it.

**Mechanism.** The (c)2 return path ships back whatever S's GPU wrote into any blob. That is correct
for a **readback** — an app that maps a GPU-written buffer and reads it — and it cannot distinguish
that from a **swapchain image**, which the app only ever *shows*. Only the WP0 token path knows which
is which.

**Fix.** Building a `wl_buffer` from a resource marks it **presented**; presented resources are
excluded from the return path exactly as rings already are. Narrow by construction: an offscreen
fixture never populates the set, so **(c)2's readback path is untouched and its GPU loopback e2e still
passes**.

**Known limit, written into the code:** an app that both *presents* a buffer and *reads it back* would
now be denied the readback. None is known here — a presented swapchain image is `DEVICE_LOCAL` and
never mapped — but the assumption is recorded rather than implicit.

## 2. Why B survived every test, which is the finding worth generalising

**The display was already correct.** S's compositor imports S's own dma-buf; no pixel is needed to make
the window appear. So the waste had **no symptom**: every test passed, the demo looked exactly like the
thesis working, and the measurement nobody took was the only thing that would have shown it.

Worse, `scripts/wp0-vkcube-two-machine.sh`'s own header — which I wrote — asserted **"No pixels cross
the network"**, and `CLAUDE.md` repeated it. Nothing was lying: the sentence described the *design*, and
no test distinguished design from behaviour. Both are now corrected in place, and `OVERVIEW.md` §6.4
gains the hazard: **a claim in a comment is not a measurement.** Where a document states a quantity —
no pixels, zero copies, bounded memory — something must measure it, or it is a wish with good grammar.

## 3. Verification

| | |
|---|---|
| `cargo test -p rayland-c -p rayland-s` | ✅ all pass, including the GPU-backed (c)2 loopback e2e |
| Pure set | ✅ **83** |
| Workspace build | ✅ |
| Cube on screen, turning | ✅ **human-confirmed**, twice, on separate runs |
| Traffic before/after | ✅ C's own per-channel counters, same workload, `traffic-before-after.md` |

**Run counts:** the two fixes were exercised across roughly a dozen runs this session. **No failure
rate is claimed**, no frame rate is claimed, and pacing/tearing is untested — the commit gate is still
untouched, exactly as scoped.

## 4. What this means for the prompt you had ready

The stall it targets no longer exists, and the causes were a cached-handle bug and a traffic bug, not
anything in the frame-callback path it would have investigated. Suggested next fronts, in the order
they now look valuable:

1. **The commit gate** (the (c)2 G' signal) — the only remaining *known* correctness gap in
   presentation. Frames may currently be early or torn; nobody has looked.
2. **A rate for WP0.** `soak-failure-rate.sh` exists and has never been pointed at this path. A dozen
   ad-hoc runs is not a number.
3. **`wl_keyboard.keymap` is still dropped** (`carries-fd`). No relayed application can have a keyboard
   until that gets a token-style substitution — real, and blocking for anything interactive.
4. **The dmabuf probe-bind volume**: 960 of 1004 compositor events in one run were the deliberately
   suppressed `format`/`modifier` pair, and the app binds the global a dozen times per session.
5. **Audit for the third instance of the identifier hazard.** It has now appeared twice in two days, in
   both daemons. A deliberate sweep for cached handles and number-keyed maps is cheap and overdue.

## 5. A process note

The owner was working on their roof and kept stepping in to look at the screen. In one afternoon their
glances overturned two of my conclusions: that the window was "mapped but never composited" (I had
built a mechanism on a *single* screenshot; they had seen the window), and that the demo was finished
(it was shipping a megabyte a frame). I had written, that same afternoon, *when the missing evidence is
something a person can simply see, ask the person* — and then did not, twice.

Both corrections are in the diary as dated entries with the wrong conclusions left standing.
