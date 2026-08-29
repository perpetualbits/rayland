# Report to planning — WP0's rate, its traffic, and two defects a second application found

**Session:** 2026-08-30, dop561 (S) + apollo (C). **Branch:** `wp0-wayland-proxy`.
**Evidence:** `docs/data/2026-08-30-wp0-rate-and-traffic/`. **Harness:** `scripts/wp0-soak.sh`.

> **In short.** 59 of 60 runs clean, and the one failure is an artefact of my own failure definition,
> so **0 genuine defects in 60**. The C→S rise the last report could not explain **was not an effect**
> — it was an uncontrolled comparison. And the owner's suggestion to try another application found
> **two real defects in thirty seconds**, one of which segfaults `rayland-s`.

---

## 1. The headless-compositor question (decision 2) — verified, with three requirements

**Headless weston does import the dma-buf.** GL renderer, `dmabuf support: modifiers`, 801 attaches in
40 s (~20 fps), zero errors. Three things are required and each was found by getting it wrong:

| | Why |
|---|---|
| `--renderer=gl` | pixman cannot import a dma-buf at all |
| `__EGL_VENDOR_LIBRARY_FILENAMES` → Mesa | weston's EGL otherwise picks **NVIDIA** while `rayland-s` renders on Intel — every import cross-GPU |
| **`--idle-time=0`** | weston idles out after **300 s and stops compositing** |

The third cost an hour. Without it the first runs did 400+ attaches and every later one did exactly
**1**, with S's log showing the frame callback simply never arriving — indistinguishable from the
application stalling. **That is the second time in two days a compositor declining to draw has been
mistaken for a Rayland defect**, and I did not recognise the pattern the second time either.

`cosmic-comp` has no headless backend (winit and udev only), so the fallback to the desktop was never
viable, and nesting throttles as the brief predicted.

## 2. The rate

**60 runs × 20 s. 59 pass, 1 fail. Failure modes: `event_drop` × 1.**

Throughput: **261–489 attaches per run (median 438) = 13–24 fps.** No liveness failure anywhere; the
2 fps floor was never approached, which suggests it is set sensibly rather than generously.

**The single failure is my definition's fault, not the code's.** Run 13 dropped two events, and both
landed during the application's **final teardown** — after every object had been legitimately
destroyed and immediately before `session ended cleanly`. S had events in flight for objects the app
had finished with. Benign.

So the honest pair of numbers:

- **As defined: 1 in 60** → rate < 7.9% at 95% (upper bound for one event).
- **On inspection: 0 genuine defects in 60** → rate < 5% at 95% (rule of three).

**The definition needs a teardown guard** — ignore drops after the app begins destroying its objects —
before the first number means what it looks like. That is exactly the exclusion the brief warned about:
*where a soak quietly stops measuring anything.* I would rather report both numbers than pick one.

**A side result worth more than the rate:** that same run shows the recycled-id fix declining **470**
stale destroys in twenty seconds. It is not a rare edge case being guarded — it fires every frame.

## 3. Traffic, and the C→S answer

Five runs a side, frame count imposed by the harness, exclusion switched **inside one binary**
(`RAYLAND_S_SHIP_PRESENTED`), so a difference cannot be a rebuild artefact.

| per frame | exclusion ON | exclusion OFF |
|---|---:|---:|
| **C→S** | **3,594 B** (3,509–3,708) | **3,723 B** (3,671–3,758) |
| **S→C** | **219 B** (211–5,058) | **307,776 B** (306,299–309,172) |

**The C→S rise was not an effect.** Ratio **1.04×**, inside either arm's spread. The 2026-08-29 report
saw C→S go from 804,814 to 1,626,138 bytes and could not explain it; the explanation is that those two
runs were **120 frames and 96 frames** and were never frame-matched, so every per-frame figure derived
from them inherited the difference.

**The return-path saving is 1,406×**, not the 571× that unmatched pair suggested. Both of yesterday's
traffic numbers were wrong in the same way — not by much, and not flatteringly, but wrong because the
comparison was not controlled.

**One outlier, reported rather than smoothed:** one exclusion-ON run shipped 1,047,105 bytes where the
other four shipped ~47–51 KB — about **one 500×500×4 frame**. Hypothesis: the swapchain image is
written by S's GPU before `create_immed` claims it, so the first frame can cross before
`note_presented` marks the resource. One frame per session, not per frame. Not chased.

## 4. Two applications beyond vkcube — the most valuable thing in this session

This came from the repository owner asking what happened to our own icosa demo. It was worth more than
the soak.

### `vkgears` — **crashes `rayland-s`**, and it took thirty seconds

Two defects, in a chain:

**A. A bind capped on S is not propagated to its children.**

```
bound global xdg_wm_base v6 -> object 12       (C advertises the descriptor's max)
WP0 replay bound `xdg_wm_base` v5 (app obj 12) (weston offers v5, so the bind is capped)
panicked: xdg_wm_base@5.get_xdg_surface: expected version 5 but got 6
```

`handle_bind` caps correctly — binding above a global's maximum is a protocol error. But objects
created from that global still carry the version **C** stamped on the `NewId`, which is the
application's. A Wayland child inherits its parent's version.

**This is the third instance of the version-inheritance rule** (after `create_immed`'s `wl_buffer`
child and the params object), and the first where **S's own capping** creates the mismatch. vkcube
never exposed it because nothing was ever capped for it.

**B. `catch_unwind` does not save the session, and the log claims it does.**

```
WP0 replay: send_request (obj 12 opcode 2) panicked — request dropped, session continues
panicked: called `Result::unwrap()` on an `Err` value: PoisonError { .. }
<segfault>
```

The panic occurs with the `maps` mutex held, **poisoning** it; the next
`.expect("the WP0 id maps lock is never poisoned")` finds it poisoned and takes the process down. The
comment is false, the reassuring log line is worse than silence, and the protection relied on since
the token task protects nothing. **Yesterday's hazard, in code rather than in prose.**

**Neither fixed** — this was a measurement session, per the brief.

### `rayland-icosa-window` — refuses cleanly, and the refusal is right

`wl_shm unavailable: the requested global was not found in the registry`, exit 0, `rayland-s`
untouched. It presents via **`wl_shm`**, which the proxy does not advertise. That is correct:
`wl_shm.create_pool` passes a **file descriptor**, which cannot cross a network, and its contents are
**pixels** — ~1 MB/frame, exactly the traffic removed the day before. It is a `wl_shm` client by
construction; WP0 is a dmabuf mechanism.

## 5. Acceptance criteria

| | |
|---|---|
| 1. Headless weston dmabuf import verified or ruled out | ✅ **verified**, with the three requirements above |
| 2. A soak with the failure definition, stating n / failures / modes and what it bounds | ✅ 60 runs, 1 failure (`event_drop`), bounds in §2 — and **why that one does not count** |
| 3. Traffic over ≥5 fixed-frame runs, per-frame with spread | ✅ 5 a side, §3 |
| 4. The C→S rise explained by A/B | ✅ **not an effect**; 1.04×, and the earlier comparison was unmatched |
| 5. Artifacts committed under `docs/data/` | ✅ |

## 6. Deviations

1. **I installed `weston`** — the brief assumed it present. 4 packages, nothing removed or upgraded;
   `sudo apt remove weston` undoes it. Flagged because it is a change to the machine, not the repo.
2. **`vkcube --c N` stalls at a single attach** under this path, while the same binary free-running
   sustains ~20 fps (`--c 60` → 1 attach, never exits). So the harness imposes the frame count itself
   by watching C's request trace. A real observation about `--c`; recorded, not chased.
3. **`RAYLAND_S_SHIP_PRESENTED` remains in the tree** as declared debt — it is the A/B switch, and it
   will be wanted again the next time a traffic claim needs controlling. Documented at its definition
   as a measurement bypass, not a tuning knob.
4. The soak harness **refuses to run** if a previous sweep left processes on C, rather than silently
   measuring through them — added after one sweep did exactly that and produced stalled-looking runs
   that were an artefact of the harness.

## 7. What remains unverified

| Open | What would settle it |
|---|---|
| The two `vkgears` defects | The next task; both are precisely characterised |
| Whether the rate holds with a teardown guard | Re-run once the definition excludes post-teardown drops |
| The ~1 MB first-frame outlier | Instrument when `note_presented` fires relative to the first GPU write |
| Pacing / tearing | Untouched; the commit gate is still out of scope |
| Any application beyond these three | Three is not many. `vkgears` cost one crash to find |
