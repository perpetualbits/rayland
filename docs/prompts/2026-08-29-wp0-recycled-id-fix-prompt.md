# WP0 — a protocol id names a slot, not an object

## Goal

Fix the recycled-id race in `rayland-c`'s proxy so the application's second and
subsequent frame callbacks survive, and re-run the witness to find out whether the cube
spins.

## Verification location

**Needs both machines.** The fix is small and its regression test may be writable
anywhere; the claim that matters — a moving cube — is only observable on dop561 with
apollo driving it.

## Context

- **Front:** WP0, after the event-witness session.
- **The finding:** `docs/reports/2026-08-29-wp0-event-witness-report.md` §3, and the
  captured logs in `docs/data/2026-08-29-wp0-event-witness/`. S's compositor emits
  `wl_callback.done` twice; C delivers one. `done` destroys a `wl_callback`, libwayland
  immediately recycles the freed id for the next callback, and
  `ObjectData::destroyed`'s removal is keyed by bare `protocol_id` — so callback #1's
  late `destroyed()` deletes callback #2's entry, and #2's `done` finds nothing to
  deliver to.
- **Existing code:** `crates/rayland-c/src/wayland_proxy.rs` — `ObjectData::destroyed`
  (~line 848), `ObjectData::request`'s `objects` insertion (~line 788), `deliver_event`.

**The general lesson, which is the point of the fix and belongs in its comments:** a
Wayland protocol id is a *slot number*, not an object identity. It is unique only among
objects alive at one instant. Every component here is individually correct — libwayland
is right to recycle, the backend is right to report the destruction, the proxy is right
to prune dead objects — and the bug lives entirely in the composition.

## Decisions already made

**1. Remove by identity, never by number.**

`destroyed()` must delete a map entry only when the entry still holds *the object being
destroyed*. Compare the stored `ObjectId` against the `object_id` argument and leave the
entry alone when they differ, because a difference means the slot has already been
refilled by a newer object that must survive.

Confirm from the `wayland-backend` version in the tree that `ObjectId`'s `PartialEq`
actually distinguishes two objects that share a `protocol_id` — it should, via an
internal serial, but that is the assumption the whole fix rests on and the previous
session's `create_immed` version bug is a fresh reminder of what an unchecked assumption
about this dependency costs. If equality turns out not to discriminate, stop and report
rather than working around it.

**2. Fix `pending` on the same line, not just `objects`.**

`destroyed()` also does `data.pending.remove(&object_id.protocol_id())`. Same bug, same
function, and the witness log proves the id reuse crosses interfaces: app id 24 is a
`zwp_linux_buffer_params_v1`, is destroyed, and is later reborn as a `wl_callback`. A
late params destroy arriving after a new params object took the same number would wipe
its accumulated `add` state, and `create_immed` would refuse the buffer as UNRESOLVED —
a missing frame behind a plausible-looking log line. Unobserved so far; identical class.

Note `pending`'s values are not `ObjectId`s, so it needs a different discriminator than
`objects` does. Work out an honest one — recording the owning `ObjectId` alongside the
pending state is the obvious route — rather than leaving the second half unfixed
because it is less convenient than the first.

**3. Do NOT add a `destroyed()` removal to S's `IdMaps`.**

The previous report calls S's map an "accidentally safe latent twin". That diagnosis is
backwards and acting on it would import this bug into S. `IdMaps.reverse` is
`HashMap<u32, u32>` keyed by S's `protocol_id`, and `insert` writes both directions at
creation, so a recycled id simply overwrites and the mapping is always fresh. **S is
safe precisely because nothing ever removes by number.** The no-op `destroyed()` is
load-bearing, not an oversight.

Neither map leaks either, despite never removing: growth is bounded by the app's peak
live-object count, because ids are recycled — the same property that caused C's bug
prevents S's leak.

So the S-side change here is **documentation only**: a comment on the no-op
`destroyed()` and on `IdMaps` explaining why removal is deliberately absent and what
would break if someone added it. Correct the report's characterisation in the diary
rather than leaving the wrong reason in the record.

**4. The witness instrumentation stays in the tree.** It has paid for itself once and
the next wall will want it. Keep it gated as it is; do not remove it after the run.

## Inputs and outputs

| File | Change |
|---|---|
| `crates/rayland-c/src/wayland_proxy.rs` | Identity-keyed removal in `destroyed()` for both `objects` and `pending`, with comments carrying the slot-vs-object lesson. |
| `crates/rayland-s/src/wayland_client.rs` | Comments only, per decision 3. |
| `crates/rayland-c/tests/` | A regression test, if an honest one is possible — see below. |
| `docs/data/` | The logs from the verification run. |

**On the regression test.** The valuable one fails against remove-by-number and passes
against remove-by-identity. If the existing proxy test harness can be made to create an
object, destroy it, and create a new object that reuses the id, that is the test. If it
cannot — if forcing deterministic id reuse from the test side is not achievable — then
**say so plainly and let the two-machine run be the evidence**. Do not write a test that
exercises the changed line without being able to fail against the old behaviour. The
last session's `create_immed` version bug is exactly what that produces: a green test
that only confirms the belief that wrote it.

## Constraints

- **No change to what is relayed or when.** This is a bookkeeping fix.
- The standing constraints in `OVERVIEW.md` §7 all still bind; none of them is near this
  change, which is itself worth a moment's check rather than an assumption.
- Do not touch the dmabuf `format`/`modifier` suppression, the keymap fd drop, or the
  commit gate.

## Conventions requirement

`CLAUDE.md`'s conventions bind in full: doc-comments on every function, type, trait and
module; an intent comment on every non-trivial line explaining the *why*; code and
comments must agree. The comment at the removal site should explain the recycling
mechanism, not describe the comparison — a reader who understands *why* the number is
insufficient will not reintroduce the bug.

## Acceptance criteria

**Checkable anywhere:**

1. `cargo test -p rayland-c -p rayland-s` passes; the pure set is still 83.
2. Either a regression test that demonstrably fails against remove-by-number (show the
   mutation, as the last two sessions did), or an explicit statement that one was not
   honestly constructible and why.

**Checkable on the two machines — the point:**

3. A run of `scripts/wp0-vkcube-two-machine.sh` with the witness on shows **no
   `drop:unknown-object`**, and `wl_callback.done` events delivered to the app matching
   those S emitted.
4. **Does the cube spin?** Two captures several seconds apart of the window interior,
   compared as the last session compared them. Differing pixels now mean success, where
   zero meant the stall. Report the count, not an impression.
5. If it spins, say how long it ran and whether it was still running when you stopped
   it. A cube that spins for two seconds and stalls again is a different result from one
   that runs until closed.

**Not claimed by any of the above:** a frame rate, a failure rate, or anything about
pacing and tearing. The commit gate is still untouched and frames may tear; that is
expected and is an observation, not a regression.

## Out of scope

- Commit gating on the G' signal.
- The `wl_keyboard.keymap` fd substitution — real, and blocking for any application that
  needs a keyboard, but not for this.
- The dmabuf probe-bind event volume.
- The `wl_shm` readback presentation path, which stays until WP0 is proven.

## Licence to deviate

If the tree contradicts this plan, **the tree wins** — do the right thing and report the
deviation.

Two specific invitations to deviate. First: if the fix does not make the cube spin, that
is a finding and the session should end with the *next* diagnosis, not with an attempt
to force this one. The witness is in the tree; use it. Second: if fixing `pending`
properly turns out to need a larger change than decision 2 anticipates, report the shape
rather than doing something expedient — it is a real bug with no observed symptom yet,
so there is no urgency justifying a shortcut.

## Reporting back

- **A diary entry** — including the correction to the previous entry's characterisation
  of S's `IdMaps` (decision 3). Do not edit the previous entry; the house pattern is a
  dated correction that leaves the original standing.
- **A project-map check** and update.
- **`docs/OVERVIEW.md`**: §6.4's hazard list should gain the slot-vs-object lesson
  alongside the two already there. If the cube spins, 4.5's status changes and §5 with
  it.

Then a report stating what was fixed, what was verified and by what evidence, whether
the cube moved and by how many pixels, how many runs, and what the next wall is if there
is one.

## Branch and git discipline

`wp0-wayland-proxy`. The laptop is primary; **never commit or push to `main` from a
non-laptop session.**
