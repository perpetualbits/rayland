# WP0 4.3 part 2 — the token carries the plane layout

## Goal

Carry the dma-buf plane layout the application actually declares — `stride` and
`offset` — on `BufferToken`, and make C refuse multi-plane buffers cleanly instead
of approximating them. This is the prerequisite decision for the S-side
`zwp_linux_buffer_params_v1.add` synthesis, which is a separate task.

## Verification location

**Anywhere.** No GPU, no compositor, no network. Everything this task changes is
covered by `cargo test -p rayland-relay -p rayland-c`.

## Context

- **Front:** WP0 (Wayland proxy first light), task 4.3 part 2.
- **Design documents that define the rules:**
  `docs/design/2026-07-22-wp0-wayland-proxy-first-light.md` §2 and §4 (buffer-by-token),
  and its `-plan` companion.
- **The record this comes from:** `docs/DIARY.md`, 2026-07-27, *"4.3 part 2: the design
  is now exact, and two details the plan does not mention"*, and `docs/OVERVIEW.md` §6.1.
- **Existing code this plugs into:** `crates/rayland-relay/src/message.rs` (`BufferToken`),
  `crates/rayland-c/src/wayland_proxy.rs` (`PendingParams`, `params_modifier`,
  `try_intercept_buffer`), `crates/rayland-c/tests/wayland_proxy_buffer_token.rs`.
- **Planned against commit `4c8ce52`** (2026-07-27). If the tree has moved, see
  "Licence to deviate".

## Decisions already made

These were decided in planning; do not re-open them in the session, and do not
substitute a derivation for any of them.

**1. `stride` travels on the token. It is never derived on S.**

Deriving `width × bpp` is the failure mode the plan flags as *garbling pixels rather
than failing cleanly* — a wrong stride produces a skewed image, not an error. C is in
a position to know the true value: Mesa passes `stride` to `params.add` before the
proxy drops the fd, and that value originates in the image layout Venus queried on
**S's own GPU**. So the token carries S's own layout, round-tripped through the
application, not an independent guess made on the wrong machine.

Note that `MOD_LINEAR`'s doc comment in `wayland_proxy.rs` records a 4.0-bis
measurement that these resources are LINEAR with `offset 0, stride = width·bpp`. That
is a record of what the value **happened to be** in one measured configuration. It is
not a licence to compute it. Leave that comment's claim intact but make sure nothing
in the new code reads as depending on it.

**2. `offset` travels too, for the same reason and at the same cost.**

`add` needs it, C observes it, and assuming zero is the same class of assumption as
assuming the stride. One `u32`.

**3. Multi-plane is refused, not approximated.**

`plane_idx` is deliberately **not** carried. Instead, C treats anything that is not a
single plane 0 as a case its assumptions do not cover, and refuses the buffer:

- an `add` whose `plane_idx` argument is not `0`, or
- a second `add` on the same `params` object,

must cause the subsequent `create_immed` to forward **nothing** — reusing the existing
unresolved path, which already logs and leaves the app with a locally valid `wl_buffer`
that S is simply never told to present.

The reason this is the right refusal: the proxy advertises exactly two single-plane
LINEAR formats (`ADVERTISED_FORMATS`), so a multi-plane `add` means an assumption
underneath WP0 has broken. Keeping the last plane's stride and presenting anyway would
garble. Refusing makes the broken assumption visible in the log at the moment it
breaks.

**4. Both new fields are `u32`**, matching the protocol's `uint` arguments, consistent
with `drm_format`.

## Inputs and outputs

**Read:** the design documents named above; the current `BufferToken` docs, which
already explain buffer-by-token at length and set the standard the new fields' docs
must match.

**Write:**

| File | Change |
|---|---|
| `crates/rayland-relay/src/message.rs` | `BufferToken` gains `stride: u32` and `offset: u32`, each with a doc comment explaining why the value is carried rather than derived. |
| `crates/rayland-c/src/wayland_proxy.rs` | `PendingParams` records stride and offset and whether the params object has been poisoned by an unsupported `add`; `try_intercept_buffer`'s `OP_PARAMS_ADD` arm reads them and applies the refusal rule; the `OP_PARAMS_CREATE_IMMED` arm assembles the fuller token. |
| `crates/rayland-c/tests/wayland_proxy_buffer_token.rs` | Extended per the acceptance criteria below. |
| `docs/OVERVIEW.md` | §6.1's second numbered detail currently states that `BufferToken` carries no stride and that this must be decided first. That becomes false with this change; update it to record the decision and its reasoning. |
| `docs/DIARY.md`, `project-map.js` | Per the standing rules. |

A helper alongside `params_modifier` for reading `add`'s plane arguments is welcome if
it makes the arm read in domain terms; follow that function's existing shape.

## Constraints

- **`rayland-relay` stays pure data** — no GPU, no sockets, no async. This change adds
  two integers and does not threaten that, but it is the reason the crate exists.
- **C links no GPU stack.** Untouched here, but `tests/no_gpu_linkage.rs` must still pass.
- **Both ends land in the same commit.** There are no deployed peers, so there is no
  compatibility question, but a `BufferToken` whose producer and consumer disagree is a
  silent wire mismatch. `rayland-s` does not read these fields yet; it must still compile.
- The `WaylandArg::Buffer(_)` arm in `crates/rayland-s/src/wayland_client.rs` is **not**
  this task's business. See "Out of scope".

## Conventions requirement

`CLAUDE.md`'s code and documentation conventions bind, in full:

- a doc-comment block on every function, type, trait and module, covering inputs,
  outputs, failure modes and domain pitfalls;
- an intent comment on every non-trivial line, explaining the **why** or the domain
  meaning, never restating the syntax;
- code and comments must agree — a comment made stale by this change is a bug to fix in
  the same edit, not a follow-up.

The two new `BufferToken` fields in particular are exactly where a future reader will
ask "why not just compute this?", so their doc comments must answer that question
rather than describe the field.

## Acceptance criteria

Checkable in this session:

1. `BufferToken` has `stride` and `offset`, documented as above.
2. C captures both from `params.add` and puts them on the forwarded token.
3. **The token test has teeth.** `wayland_proxy_buffer_token.rs` asserts the forwarded
   token's `stride` and `offset` equal what the synthetic `add` supplied, and the
   fixture's stride is deliberately **not** `width × 4`. A test whose stride happens to
   equal the derived value would pass against the very implementation this change
   exists to prevent.
4. A new test covers the refusal: an `add` with `plane_idx = 1`, and separately a second
   `add` on the same params object, each result in **no** `BufferToken` being forwarded
   at `create_immed`.
5. `cargo test -p rayland-relay -p rayland-c` passes.
6. `cargo test -p rayland-vtest -p rayland-relay -p rayland-venus-proto` still passes
   (the 83-test pure set).
7. `cargo build` for the whole workspace succeeds — `rayland-s` constructs no
   `BufferToken` but must still compile against the changed type.

Not checkable in this session, and not claimed:

- That S can build a real `wl_buffer` from these values. That needs a GPU and a
  compositor and is the next task. Do not assert in the diary, the map, or `OVERVIEW.md`
  that 4.3 is done.

## Out of scope

Do not touch, in this session:

- `crates/rayland-s/src/wayland_client.rs` — the `Buffer` arm, the params synthesis, the
  `create_params` origination. All of that is the next prompt.
- The `Applier` → `WaylandReplay` fd-resolution seam.
- Commit gating on frame completion.
- `plane_idx` as a carried field (decision 3 above).
- The v3 version cap, `ADVERTISED_FORMATS`, or anything in the format advertisement.

## Licence to deviate

If the tree contradicts this plan's decomposition, **the tree wins.** Do the right
thing and **report the deviation** rather than implementing a brief you have found to
be wrong; that has happened repeatedly and productively in this project and is expected
rather than awkward.

Two specific places where this plan may be out of date, since it was written from a
snapshot at `4c8ce52`:

- If `PendingParams` or `try_intercept_buffer` has already changed shape, adapt rather
  than reverting.
- If some part of this is already done, say so plainly instead of redoing it. The
  planning side has already made one error of this kind on this exact task — C's half of
  4.3 was called unfinished when it was complete.

## Reporting back

Per `CLAUDE.md`'s binding rules:

- **A diary entry** in `docs/DIARY.md` — the thinking, including anything that surprised
  you or any wrong turn taken on the way. Not the diff.
- **A project-map check.** Look at `project-map.js` against what changed; update it and
  bump `project.updated` only if something it depicts actually changed, and say in the
  report that you looked either way.
- **`docs/OVERVIEW.md`** — §6.1's stride bullet is made false by this change; update it
  and bump the "Last brought current" line.

Then produce a report to bring back to the planning side, stating:

1. What was built, and what was **verified** — separately, and with the evidence for
   each (which test, what it asserts).
2. Any deviation from this brief and why.
3. Anything this overturned, and where that was recorded.
4. What documents or map nodes it made false.
5. What remains unverified and what would verify it.

## Branch and git discipline

Work on `wp0-wayland-proxy`, where the rest of WP0 lives. **The laptop is primary;
never commit or push to `main` from a non-laptop session** — push to a clearly-named
side branch and leave merging to the human.
