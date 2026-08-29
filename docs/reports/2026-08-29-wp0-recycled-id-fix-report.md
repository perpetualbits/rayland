# Report to planning — the recycled-id fix

**Session:** 2026-08-29 late, dop561 (S) + apollo (C). **Branch:** `wp0-wayland-proxy`.
**Evidence:** `docs/data/2026-08-29-wp0-recycled-id-fix/`.

> **In one line.** The fix works and is guarded by a test that discriminates 10/10. The frame
> callbacks now flow — 9 emitted, 9 delivered, zero drops. **The cube still does not spin**, and the
> wall has moved somewhere this brief did not reach: in four runs of five, S's compositor emits **no
> `wl_surface.frame` callbacks at all**.

---

## 1. The assumption, checked first

`ObjectId`'s `PartialEq` **does** distinguish two objects that shared a `protocol_id`:
`InnerObjectId::eq` compares `id && serial && client_id && interface`
(`wayland-backend-0.3.15/src/rs/server_impl/mod.rs:59`), and the server allocates a fresh
`next_serial()` per object. The fix rests on solid ground.

## 2. What was fixed

- **`objects`** — `destroyed()` removes an entry only when it still holds the object being destroyed.
- **`pending`** — same rule, via a new `PendingParams::owner: Option<ObjectId>`, since its values are
  not `ObjectId`s and the key alone cannot discriminate. No observed symptom yet, but the witness log
  proves id reuse **crosses interfaces** (app id 24 lived as a `zwp_linux_buffer_params_v1`, died, and
  came back as a `wl_callback`), so a late params destroy wiping a new params object's `add` state is
  the same bug in different clothes.
- **`rayland-s`** — comments only, per decision 3.

## 3. The regression test, and two rounds of getting it wrong

`crates/rayland-c/tests/wayland_proxy_recycled_id.rs`. **10/10 fail against remove-by-number, 10/10
pass with the fix** — measured, not asserted.

It took two corrections to get there, both worth carrying forward:

1. **The first version passed against the buggy code.** I had put a `queue.roundtrip()` between the
   two callbacks, which let the backend run `destroyed()` *before* the new object was registered —
   quietly stepping around the race. It tested what I imagined the sequence to be. Real applications
   re-arm from **inside** the `done` handler, so the new request lands in the same dispatch batch;
   moving the re-arm there reproduced it. This is `OVERVIEW.md` §6.4's hazard, hit again, two sessions
   after writing it down.
2. **Even then it failed only 2 runs in 10.** Whether the race bites depends on dispatch timing, so
   one cycle is one sample — a coin flip that would sit green in CI with the defect present. The test
   now drives **thirty** cycles, as an animating app does, and asserts every callback got its `done`.

## 4. Correction to the previous report — I had S backwards

The 2026-08-29 event-witness report called `rayland-s`'s `IdMaps` an *"accidentally safe latent twin"*
that is *"safe today only because its `destroyed()` is a deliberate no-op, which is a fragile reason
to be correct."*

**That is wrong, and acting on it would have imported this bug into S.** `IdMaps::insert` writes both
directions at object *creation*, so a recycled id is refreshed by its new owner; nothing ever removes
by number, which is exactly why nothing can go wrong there. The no-op `destroyed()` is **load-bearing**.
Nor does never removing leak: growth is bounded by the app's peak live-object count, because ids are
recycled — the same property that made recycling dangerous on C makes it safe on S. Both facts are now
comments on `IdMaps` and on `destroyed()`, where the next person to tidy that no-op will meet them.

The brief caught this before I could act on it. Worth noting the diagnosis was mine, in a report whose
other findings were sound — a wrong reason attached to a right conclusion.

## 5. Acceptance criteria

| | |
|---|---|
| 1. `cargo test -p rayland-c -p rayland-s`; pure set 83 | ✅ all green, 0 failures; pure set **83** |
| 2. A regression test that demonstrably fails against remove-by-number | ✅ **10/10 fail / 10/10 pass**, mutation shown |
| 3. No `drop:unknown-object`; `wl_callback.done` delivered == emitted | ✅ **zero** drops in every run; in the run with callbacks, **9 emitted, 9 delivered** |
| 4. Does the cube spin? | ❌ **No.** 3 captures 6 s apart: **0 differing pixels of 202,500** |
| 5. How long it ran, still running when stopped | See §6 — 5 runs, 45–100 s each; the app was alive but idle at every stop |

## 6. The result, and the next diagnosis

**The plumbing is fixed and the witness says so exactly.** The app went from 1 attach to **9 attaches,
9 frame requests, 10 commits**, with 9 of 9 callbacks delivered and zero drops.

**The cube is still static** — and not for the reason just fixed. All nine attaches happened within
the first ~18 s; every photograph was taken after the app had already stopped.

**Five runs:**

| Run | attaches | frame reqs | `done` emitted by S | `buffer.release` | drops |
|---|---|---|---|---|---|
| 1 | 9 | 9 | **9** | 2 | 0 |
| 2 | 1 | 1 | **0** | 0 | 0 |
| 3 | 1 | 1 | **0** | 0 | 0 |
| 4 | 1 | 1 | **0** | 0 | 0 |
| 5 | 1 | 1 | **0** | 0 | 0 |

**Four of five runs: S's compositor emits no frame callback at all.** The fifth emits nine, the app
consumes all nine, and then stops *asking* — no tenth `frame` request — having seen only **2
`wl_buffer.release` for 9 attaches**.

**Both shapes point outside anything Rayland relays.** A compositor emits a frame callback only when
it actually **composites** the surface, and releases a buffer only when it stops using one. Zero
callbacks means S's compositor is not drawing the replayed surface. The run-to-run variance fits
**window placement and visibility** rather than the relay — and the one run that animated is the one
whose window was photographed and visibly on screen.

**That is a hypothesis, and I deliberately did not chase it**, per the brief's first invitation to
deviate.

## 7. What the next session needs first

**The observation that splits it: in a one-frame run, is there no window, or a window present but
never composited?** Everything downstream depends on which.

- *No window* → the xdg configure/ack/map handshake is incomplete, and the frame callback is a
  symptom.
- *Window present, not composited* → the compositor is legitimately withholding callbacks from a
  surface it is not drawing, and the question becomes why it is not drawing it (occlusion, placement,
  a surface it considers unmapped).

**A practical obstacle:** `cosmic-screenshot` became unresponsive partway through this session — it
worked for the early runs, then hung past a 30 s timeout — so the one-frame runs have **no
photograph**. That observation may need a human at the screen.

## 8. What remains unverified

| Open | What would settle it |
|---|---|
| Whether a window exists in the one-frame runs | A screenshot that works, or a human |
| Why S's compositor withholds frame callbacks | The above, then the xdg mapping sequence |
| The `pending` half of the fix | No symptom was ever observed; it is guarded by reasoning and the same identity rule, not by a test |
| Pacing / tearing | Still untouched; the commit gate is still out of scope |
| Any rate | Five runs |

---

## Addendum, same day — §7's question is answered: **window present, not visible**

The split §7 asked for is settled, and it is the second branch.

**A window exists and is mapped in every run, including the one-frame ones.** All six runs delivered
`wl_keyboard.enter` to the app — a compositor only gives keyboard focus to a **mapped** surface — each
followed by a `wl_keyboard.leave`. So "no window" is ruled out without needing a photograph.

**But the window is not on screen.** A sixth run (10 attaches, 10 `wl_callback.done`, the *animating*
case) was photographed at 22 s while the app was live: **no vkcube window anywhere in the full-screen
capture** — the desktop showed only the machine owner's other windows. The screenshot portal recovered
for this run, so this is direct evidence rather than inference.

**So the picture is:** the surface is mapped, is briefly given keyboard focus, then loses it, and is
**not composited on the visible workspace** — and a compositor emits frame callbacks only for surfaces
it actually draws. That accounts for both observed shapes: zero callbacks when it is never drawn, and
a handful when it briefly is. It also explains the run-to-run variance, which never fitted anything in
the relay.

**Most likely mechanism, untested:** COSMIC places the new toplevel on a **different workspace** (or
fully behind the full-screen terminals), so it is mapped but never drawn. The earlier session that
photographed the cube successfully is the case where it happened to land visibly.

**What this means for WP0:** the remaining stall is very likely **not a Rayland defect at all** — it is
correct compositor behaviour toward a window nobody is looking at. The next step is to make the window
land somewhere visible and watch it, rather than to debug the relay: run with the target workspace
empty and focused, or set an app id / use a compositor rule to place it, then re-measure whether frames
continue for as long as it stays visible.

**Not claimed:** that this is the *whole* explanation. In the 9-frame run the app also stopped asking
for frames with only 2 `wl_buffer.release` for 9 attaches, and buffer release is likewise tied to
compositing. Whether that resolves once the window is genuinely visible is the measurement to take.
