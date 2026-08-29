# WP0 — the event-stream witness

## Goal

Find out **which event vkcube is waiting for** after its first present, by making both
halves of the WP0 event return path say what they are doing — rather than by reasoning
about which component is at fault. Build the instrument; run it; report what it saw.
Do not fix the stall in this session.

## Verification location

**Needs both machines.** The instrument itself is a few log lines and compiles
anywhere, but it produces nothing without apollo running vkcube against dop561. This
task is worthless without the run.

## Context

- **Front:** WP0, immediately after 4.3.
- **The wall:** `docs/reports/2026-08-29-wp0-4.3-report.md`, "Criterion 5 — NOT
  reached". vkcube attaches, damages, requests a frame callback, commits once, then
  stalls **alive**, main thread on a futex held by Venus's `vn_wsi[0,0]` thread, which
  sits in `poll`. 12 events were delivered back to the app.
- **Existing code:** `crates/rayland-s/src/wayland_client.rs` (`translate_and_emit`,
  `ReplayObjectData::event`, `compositor_reader`), `crates/rayland-c/src/wayland_proxy.rs`
  (`deliver_event`), `scripts/wp0-vkcube-two-machine.sh`.

**Method note.** This is the project's standing pattern for a wall of this kind: after
the dead theories, stop generating explanations and build the independent witness. The
three-day wall broke on an instrument, not a theory, and the stale-frame diagnosis was
wrong until a second fingerprint of a *different quantity* was taken. Treat this as that
task.

## What planning already checked, so you do not repeat it

Two candidate causes were checked against the tree in the planning session and are
**dead**. Do not spend the session re-deriving them:

- **The synthesized objects do have `ObjectData`.** `synthesize_buffer` passes
  `Some(self.object_data())` for both the params object and the `wl_buffer`, so a
  `wl_buffer.release` on a synthesized buffer does reach `ReplayObjectData::event`.
- **C does register the intercepted `wl_buffer`.** `ObjectData::request` inserts every
  `Argument::NewId` into `data.objects` **before** `try_intercept_buffer` runs, so
  `deliver_event` can resolve it.

## The finding this task exists to act on

**The two halves of the return path are asymmetrically instrumented, and the silent half
is S's.**

`rayland-c`'s `deliver_event` logs *every* reason it drops an inbound event: unknown
object, unresolvable `Object` argument, a `NewId` it cannot deliver.

`rayland-s`'s `translate_and_emit` has **four bare `return`s and logs none of them**:

1. the sender object is not in the S→app map;
2. an `Argument::Object` argument is not in the map;
3. the event carries an `Argument::NewId`;
4. the event carries an `Argument::Fd`.

Plus a fifth, deliberate and already documented: the `zwp_linux_dmabuf_v1`
`format`/`modifier` events are suppressed on purpose.

So an event vkcube is blocked on could be dropped on S right now leaving no trace at
all, and the report's "no S-side replay errors in the log" is not evidence about a path
that is silent by construction. That is the gap to close.

## Decisions already made

**1. Drops always log; the full trace is gated.**

A dropped event is rare and each one is a finding, so it logs unconditionally, matching
C. The event-by-event trace is high-volume, so gate it behind an environment variable
following the existing convention on S (`RAYLAND_S_REPLY_LOG` is the sibling — read how
it is used in `main.rs` and match that shape rather than inventing a new one).

**2. Log events by name, not by opcode.** An opcode number is useless across two id
spaces and two logs. `wayland-backend`'s `Interface` carries `events: &[MessageDesc]`,
each with a `name`; `msg.sender_id.interface()` gives the interface. Log
`wl_callback.done`, not `opcode 0`. Do the same on C's side of the diff. Guard against
an opcode outside the descriptor's range rather than indexing blindly.

**3. Both ends emit a comparable line, so the two logs diff directly.** Same field
order, same naming, each carrying the object id in *its own* id space plus the interface
and event name. The question the diff must answer without further reasoning is: *for
each event S's compositor emitted, did it reach the app, and if not, where did it stop?*

**4. Do not fix anything you find.** If the diff shows an event dropped at a specific
branch, that is the finding and the report is the deliverable. Fixing it is the next
task, specified against evidence. The exception is a fix so small and so obviously
correct that leaving it in would corrupt the very run you are measuring — if you hit
one, say so explicitly and separate it in the report.

## The human observation this session must capture

**Before or during the run, look at dop561's screen and record what is there.** This is
not optional and it is not a formality — it splits the investigation in half:

- **A window with one frozen frame** → the surface mapped and composited, so the
  compositor *did* have reason to emit a frame callback, and the loss is downstream of
  it: in `translate_and_emit`, on the link, or in `deliver_event`.
- **No window at all** → the surface never mapped, and the missing callback is a
  *symptom*. The investigation moves to the xdg configure/ack/commit sequence, which is
  a different task entirely.

`grim` fails on COSMIC (no `wlr-screencopy-unstable-v1`). If `cosmic-screenshot` or an
`xdg-desktop-portal` screenshot path works on this machine, use it and commit the image
under `docs/data/` — a permanent artifact beats a human's recollection. If neither
works, a human's description in the diary is acceptable evidence for this one question;
say plainly in the report that that is what it is.

## Inputs and outputs

| File | Change |
|---|---|
| `crates/rayland-s/src/wayland_client.rs` | Unconditional drop logging at all four `return`s in `translate_and_emit`, each naming *which* branch and the interface/event; gated per-event trace of everything delivered to `ReplayObjectData::event` and everything emitted. |
| `crates/rayland-c/src/wayland_proxy.rs` | Event names in `deliver_event`'s existing logs; a gated trace line for each event successfully delivered to the app, so "reached the app" is directly observable rather than inferred from the absence of a drop. |
| `scripts/wp0-vkcube-two-machine.sh` | Set the trace variables on both ends; capture both logs to files under a run directory so the diff is reproducible rather than scrollback. |
| `docs/data/` | The captured logs from the run, and a screenshot if one can be taken. |

**Also fix, in passing:** `scripts/c1-two-machine.sh` hardcodes `S_IP=192.168.1.192`,
which is no longer dop561's address, so that script now fails with a connection timeout
that looks like a network fault. Derive the address the way the new WP0 script does. It
is two lines, it is a live landmine in a script this project relies on, and the previous
session correctly left it alone only because it was out of that brief's scope.

## Constraints

- **The instrument must not perturb what it measures.** `translate_and_emit` runs on the
  compositor-reader thread; the existing comment is explicit that the map lock is
  released before `emit` so it cannot contend with the message thread. Logging must not
  extend that hold — build the string after the lock is released, or log before taking it.
- Unconditional drop logging must stay genuinely rare. If a run produces a flood of
  drops from one interface, that is itself the finding — report it rather than
  suppressing it to keep the log tidy.
- No change to what is *relayed*. This task changes only what is *said*.

## Conventions requirement

`CLAUDE.md`'s conventions bind in full: doc-comments on every function, type, trait and
module; intent comments on every non-trivial line explaining the why; code and comments
must agree. The four drop branches in particular each need a comment saying what
dropping there *means* for the app, since that is the knowledge this session is buying.

## Acceptance criteria

**Checkable anywhere:**

1. `cargo test -p rayland-c -p rayland-s` passes; the pure set is still 83.
2. Event-name lookup is bounds-checked and has a unit test covering a valid opcode and
   an out-of-range one.

**Checkable on the two machines — the actual point:**

3. A run of `scripts/wp0-vkcube-two-machine.sh` produces two captured logs, and the
   report answers: **which events S's compositor emitted after the first commit, which
   of those reached the app, and where any that did not were lost.**
4. The screen observation is recorded: window with a frozen frame, or no window.

**Explicitly not expected:** that the stall is fixed, or that 4.5 is reached. If the
diff shows the answer immediately, say what it is and stop.

## Out of scope

- Fixing the stall (decision 4).
- Commit gating on the G' signal.
- The `wl_shm` readback presentation path.
- Any change to which events are relayed, or to the dmabuf `format`/`modifier`
  suppression.
- The `presentation-time` protocol, or advertising any new global.

## Licence to deviate

If the tree contradicts this plan, **the tree wins** — do the right thing and report the
deviation.

The standing warning for WP0, now with four instances behind it: **the written plan has
been short of what the code needs every single time.** Treat `OVERVIEW.md` §6 and the
WP0 design documents as a lagging record of intent, not a specification. If the
instrument shows the return path is fine and the problem is somewhere this brief does
not mention, that is a finding and a better outcome than confirming a guess.

## Reporting back

- **A diary entry** — including the screen observation, and the event diff itself
  (or a pointer to the committed logs).
- **A project-map check**, and an update if a node's status changed.
- **`docs/OVERVIEW.md`** if this makes anything there false. Add a heading for the
  epistemic finding from the last session if it has no home yet: *a unit test written
  from the same belief as the code it tests can only confirm that belief* — the
  `create_immed` child-version case is the worked example, and it is a general hazard,
  not a WP0 detail.

Then a report stating: what the instrument saw, in evidence terms; whether the screen
had a window; which event is missing and where it was lost, or that the diff was
inconclusive and what that rules out; anything this overturned; what remains unverified.

One run is one run. If the stall does not reproduce on the first attempt, that is itself
important — say how many runs and how many stalled.

## Branch and git discipline

`wp0-wayland-proxy`, which is now level with `main`. The laptop is primary; **never
commit or push to `main` from a non-laptop session.**
