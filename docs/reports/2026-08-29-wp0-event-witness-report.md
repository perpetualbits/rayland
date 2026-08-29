# Report to planning — the WP0 event-stream witness

**Session:** 2026-08-29 evening, dop561 (S) + apollo (C). **Branch:** `wp0-wayland-proxy`.
**Evidence:** `docs/data/2026-08-29-wp0-event-witness/` (both ends' logs, and the screenshot).

> **The answer, in one line.** The application never receives its **second `wl_surface.frame`
> callback**, and it is lost in **`rayland-c`**, not S: a `wl_callback.done` is a *destructor* event,
> libwayland immediately reuses the freed id for the next callback, and the proxy's object map is
> keyed by bare `protocol_id` — so `destroyed()` for callback #1 runs after callback #2 has been
> registered under the same number, and deletes it.

---

## 1. The screen — the observation that split the investigation

**A window, with the cube correctly rendered, frozen.** This is the brief's first branch: the surface
mapped and composited, so the compositor *did* have reason to emit a frame callback, and the loss is
downstream of it.

`grim` fails on COSMIC as expected, but `cosmic-screenshot --interactive=false --modal=false` works,
so this is a committed artifact rather than a recollection: `cube-on-dop561.png`. An unmodified vkcube
on apollo, drawn by dop561's GPU, in its own window on dop561's compositor.

**Frozen, measured rather than eyeballed:** the 450×450 window interior was **pixel-identical — 0 of
202,500 differing, max channel delta 0 — across two captures 17 s apart**, in each of two runs. (A
naive crop showed 7,830 differing pixels; those were desktop bleed at the window edge. The interior is
exact.)

Only the application's window is committed. The full-screen captures show the machine owner's other
windows and were deliberately not kept.

## 2. What the instrument saw

Both ends now emit a comparable line, event **by name** — an opcode is an index into one interface's
event list and the two ends log against different id spaces, so `opcode 0` in both proves nothing.

| | S (compositor → link) | C (link → app) |
|---|---|---|
| events from S's compositor | 1004 | — |
| suppressed (dmabuf `format`/`modifier`, deliberate) | 960 | — |
| emitted toward C | 43 | — |
| delivered to the app | — | 42 |
| **drops** | **1** (`carries-fd`, `wl_keyboard.keymap`) | **1** (`unknown-object`, app_obj 24) |

S emitted `wl_callback.done` **twice**. C delivered **one**. That single missing event is the stall.

## 3. Where it is lost, from C's own log, in order

```
objects+ app_obj=24 wl_callback                    <- frame callback #1 created
delivered app_obj=24 wl_callback.done              <- #1 delivered; `done` DESTROYS a wl_callback
objects+ app_obj=24 wl_callback                    <- #2 created, REUSING the id just freed
objects- app_obj=24 wl_callback (was_known=true)   <- #1's destroyed() finally runs — removing #2
drop:unknown-object app_obj=24 opcode=0            <- #2's done has nothing to deliver to
```

**A Wayland protocol id is not unique over time.** Every component is individually correct: libwayland
is right to recycle a freed id, the backend is right to report the destruction, C is right to prune
destroyed objects. The bug is in the composition, resting on an assumption nobody wrote down.

**S is exonerated.** The previous session's stale-reverse-map theory (mine) is **refuted**: S emitted
both events and mapped both callbacks consistently — `map s_obj=13 app_obj=24`, twice — because both
ends recycle the same ids in step.

**Not fixed, per decision 4.** The shape: `destroyed()` must remove an entry only if the stored
`ObjectId` is still the object being destroyed, rather than removing by number.
**Note a latent twin:** `rayland-s`'s `IdMaps.reverse` is keyed the same way and is accidentally safe
today only because its `destroyed()` is a deliberate no-op — a fragile reason to be correct.

## 4. Two other findings, neither implicated in the stall

- **`wl_keyboard.keymap` is dropped on S** (`drop:carries-fd`). It carries a file descriptor, which
  cannot cross a network — the project's founding constraint. The app does not block on it, but **no
  relayed application will ever have a keymap** until this gets a token-style substitution like the
  buffer path did.
- **960 of 1004 events are suppressed dmabuf `format`/`modifier`.** The suppression is correct and
  documented, but ~96% of the return path's event traffic exists only to be discarded.

## 5. Deviations from the brief

1. **C's drops were *not* unconditional.** The brief said `deliver_event` logs every drop "matching C";
   in fact they all went through the env-gated `wp_log`. Since symmetry was the point, C got a
   `wp_drop` that always speaks. Both ends now genuinely match.
2. **A second instrumented run.** After the first run I had *where* but two candidate *whys* and could
   not separate them. Rather than report the ambiguity, I added two more witness lines (C: object-map
   insert/remove; S: each id mapping) and re-ran. Still instrument, not fix — and it turned a two-way
   guess into a fact. Worth noting: my instinct was to blame S, and the instrument said otherwise.
3. **A screenshot proved possible**, so the brief's fallback of a human description was not needed.

## 6. Acceptance criteria

| | |
|---|---|
| 1. `cargo test -p rayland-c -p rayland-s` passes; pure set still 83 | ✅ 18 test binaries all ok, 0 failures; pure set **83** |
| 2. Event-name lookup bounds-checked, unit-tested valid + out-of-range | ✅ `event_names_resolve_and_out_of_range_opcodes_do_not_panic` — asserts `wl_callback.done`, and that opcode `len()` and `u16::MAX` return `None` rather than panicking |
| 3. Two captured logs; report says which events were emitted, which arrived, where the rest were lost | ✅ §2–§3, artifacts committed |
| 4. Screen observation recorded | ✅ §1 — window with a frozen frame, photographed and measured |

**Also fixed in passing, as instructed:** `scripts/c1-two-machine.sh`'s hardcoded
`S_IP=192.168.1.192` now derives from the routing table.

## 7. Run count, precisely

**Two runs. Both stalled. Identical behaviour, the same single drop, the same frozen cube.** That is
not a rate, but it is not a one-off either.

## 8. What this overturned

- **"No S-side replay errors in the log" (previous report) was not evidence.** Four drop branches in
  `translate_and_emit` returned silently. The statement was true and meant nothing. This is now
  written up in `OVERVIEW.md` §6.4 as a general hazard, alongside the `create_immed` version bug —
  *a test written from the same belief as the code it tests can only confirm that belief.*
- **My stale-reverse-map theory for the stall was wrong.** The instrument said so.
- **4.5's status changed** from "not reached" to "half reached": the picture arrives, the animation
  does not.

## 9. What remains unverified

| Open | What would settle it |
|---|---|
| That fixing the recycled-id race makes the cube spin | The fix, then a re-run — the obvious next task |
| Whether any *other* event is also lost once that one is fixed | Re-run the witness after the fix; it stays in the tree |
| The keymap fd, and any other fd-bearing event | A token-style substitution; unscoped |
| Pacing/tearing once frames flow | Still the commit-gating task, still untouched |
| Anything about rates | Two runs |
