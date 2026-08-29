# WP0 4.3 part 2 — token → `wl_buffer` on S

## Goal

Make S turn a `BufferToken` into a real `wl_buffer` on its own compositor, by
**originating** the `zwp_linux_dmabuf_v1` buffer-creation sequence against the dma-buf
S already exported for that resource. This is the last piece of 4.3, and it is what
lets the app's `attach`/`commit` — which already replay — put a real frame on S's
screen.

## Verification location

**Needs S, and the end-to-end criterion needs both machines.** The request-construction
half is unit-testable anywhere and must be; the compositor-facing half cannot be
checked anywhere but dop561, with apollo running the app.

Both machines are available for this session, so the run is expected to happen here
rather than be deferred.

## Context

- **Front:** WP0 (Wayland proxy first light), task 4.3 part 2 — the last unwritten piece.
- **Design documents that define the rules:**
  `docs/design/2026-07-22-wp0-wayland-proxy-first-light.md` §2–§4, and its `-plan`
  companion (note Task 4.0's outcome is **superseded** by 4.0-bis in the same document:
  presentation *is* zero-copy dma-buf).
- **The record:** `docs/DIARY.md`, 2026-07-27, *"4.3 part 2: the design is now exact,
  and two details the plan does not mention"*, and the session-close entry after it.
  `docs/OVERVIEW.md` §6.1.
- **Existing code:** `crates/rayland-s/src/wayland_client.rs` (`WaylandReplay`,
  `handle_bind`, `handle_request`, `interface_by_name`), `crates/rayland-s/src/apply.rs`
  (`Applier::exported_fd`), `crates/rayland-s/src/main.rs` (`serve`, and where
  `WaylandReplay` and the `Arc<Mutex<Applier>>` are constructed),
  `crates/rayland-s/tests/wayland_replay.rs` (the skip-if-no-compositor harness).
- **Depends on:** the prompt that puts `stride` and `offset` on `BufferToken`. If those
  fields are not present in the tree, that work is not done and this task is blocked —
  say so and stop rather than deriving them.
- **Planned against commit `4c8ce52`** plus that stride change.

## Decisions already made

**1. S must originate `create_params` as well as `add`. The plan is short by one request.**

This is the correction that matters most, and neither the spec nor `OVERVIEW.md` §6.1
states it. `wayland_proxy.rs`'s `OP_CREATE_PARAMS` arm intercepts
`zwp_linux_dmabuf_v1.create_params` and **does not forward it** — the comment says "S
builds its own params object from the token in Task 4". So when a `create_immed`
arrives, S has **no `zwp_linux_buffer_params_v1` object at all**, and the app-side
params id it names is **not in the id map**. As written, `handle_request` would refuse
it at the sender lookup (`wayland_client.rs:545`, "request for unmapped object") before
ever reaching the `WaylandArg::Buffer` arm.

So the synthesized sequence is **three** requests, all originated by S:

1. `zwp_linux_dmabuf_v1.create_params` (opcode 1) on S's bound dmabuf object, with a
   `child_spec` of `zwp_linux_buffer_params_v1` at the bound version. Map the app's
   params id to the resulting S-side object.
2. `zwp_linux_buffer_params_v1.add` (opcode 1) with
   `[Fd, Uint(0) /* plane_idx */, Uint(offset), Uint(stride), Uint(mod_hi), Uint(mod_lo)]`.
3. `zwp_linux_buffer_params_v1.create_immed` (opcode 3) with
   `[NewId(null), Int(width), Int(height), Uint(drm_format), Uint(flags = 0)]` and a
   `child_spec` of `wl_buffer` v1. Map the app's buffer id to the resulting object.

`plane_idx` is `0` because C refuses anything else (see the stride prompt's decision 3);
`offset` and `stride` come from the token, never from `width × bpp`. The modifier is
split `hi = (modifier >> 32) as u32`, `lo = modifier as u32`, mirroring C's
`params_modifier` in reverse.

**2. The `Applier` → `WaylandReplay` seam is a trait returning an owned fd.**

The record left this open deliberately, "to be made with a test in front of it". Make it:

```rust
/// Resolves a BufferToken's resource id to a *duplicate* of the dma-buf descriptor S
/// exported for that resource at creation.
pub trait ExportedFdSource: Send + Sync {
    fn dup_exported_fd(&self, resource_id: u32) -> Option<OwnedFd>;
}
```

Implement it for a small newtype wrapping `Arc<Mutex<Applier>>`, whose body takes the
applier lock, calls the existing `Applier::exported_fd`, `try_clone_to_owned`s the
borrow, and returns — releasing the lock on the way out. `Applier` itself needs no
change.

**Why this shape rather than handing `WaylandReplay` the `Arc<Mutex<Applier>>`.** The
lock discipline the plan requires — *resolve and clone the fd under the applier lock,
release it before any `send_request`* — becomes a property of the type rather than a
comment someone can violate later: the lock guard cannot escape `dup_exported_fd`, so
there is no way to hold the applier across a compositor round trip. It also mirrors the
house pattern on the other side of the wire (`ResourceResolver` and `WaylandSink` in
`rayland-c`'s proxy), and it gives the pure test a fake to inject.

Note this is a lock the message thread does not currently hold at that point:
`serve` routes `C2S::WaylandRequest` to `wl_replay` and `continue`s **before** taking
`applier.lock()`. So this introduces a new lock acquisition on that thread, not a nested
one. Keep it that way.

**3. Commit gating on frame completion is OUT of scope.**

`OVERVIEW.md` §6.1 step 5 says the commit wants gating on the (c)2 G' signal. It does,
eventually. It is not needed to demonstrate that a `wl_buffer` is built and accepted,
it reaches into `progress_thread`'s existing barrier, and shipping both together makes
any failure ambiguous between "the token path is wrong" and "the gating is wrong". A
first version that presents a possibly-torn or possibly-early frame is the right first
version. Gating gets its own task and its own measurement.

**4. The bound `zwp_linux_dmabuf_v1` object is recorded at bind time.**

`create_immed` names the *params* object, so nothing in the relayed message identifies
the dmabuf global. `handle_bind` already sees every bind; when the interface is
`zwp_linux_dmabuf_v1`, stash the resulting S-side `ObjectId` and the bound version on
`WaylandReplay`. If a token arrives and that is `None`, refuse and log — do not guess.

**5. Every refusal is loud and total.** An unresolvable resource id, a missing dmabuf
global, a `send_request` error at any of the three steps: log with the resource id and
the reason, and do not attach a partially built buffer. The existing arm's habit of
skipping the whole request is right; keep it, but with a message that names *which* of
the three steps failed.

## Inputs and outputs

**Write:**

| File | Change |
|---|---|
| `crates/rayland-s/src/wayland_client.rs` | The `ExportedFdSource` trait; the dmabuf-global record; the `WaylandArg::Buffer` path replacing the skip at ~line 592; a **pure** request-builder (see acceptance criterion 1). |
| `crates/rayland-s/src/main.rs` | Construct the `Applier`-backed `ExportedFdSource` and pass it to `WaylandReplay::new`. |
| `crates/rayland-s/tests/wayland_replay.rs` | The pure builder test, and — following the existing skip-if-no-compositor pattern — whatever compositor-facing coverage is honestly achievable. |
| `scripts/wp0-vkcube-two-machine.sh` | New. See below. |
| `docs/OVERVIEW.md`, `docs/DIARY.md`, `project-map.js` | Per the standing rules. |

**The script.** There is no WP0 runner in `scripts/`; the e2e is currently a sequence of
remembered commands, which is exactly the thing this repository turns into a script with
an explanatory header. Model it on `scripts/c1-two-machine.sh`: same topology (S =
dop561, C = apollo), `RAYLAND_C1_WAYLAND_DISPLAY` set on C so the proxy runs, the app's
`WAYLAND_DISPLAY` pointed at it. **`vkcube` must be run with `--gpu_number 0`** — it
defaults to the discrete NVIDIA GPU and provokes `VK_ERROR_DEVICE_LOST`, which cost this
project three days; put that in the header with the reason, not just in the command line.
`VN_DEBUG=no_abort` stays absent, for the reason `c1-two-machine.sh`'s header gives.

## Constraints

- **The applier lock is never held across a `send_request`.** Decision 2 makes this
  structural; do not add a path that reopens it.
- **`rayland-s` may depend on `rayland-engine`** — it is the GPU machine — but nothing
  here should need to.
- The exported descriptor is a **borrow, never ownership**: `mem->exported` permits
  exactly one export per resource and it already happened at creation. `dup_exported_fd`
  hands out duplicates; `Applier`'s own `OwnedFd` must stay alive and unmoved.
- **Determine, from the `wayland-backend` version actually in the tree, whether
  `send_request` closes the `RawFd` it sends.** A double close and a leak are both real
  and both silent. Record what you found in the diary; do not assume either way.

## Conventions requirement

`CLAUDE.md`'s code and documentation conventions bind in full: a doc-comment block on
every function, type, trait and module covering inputs, outputs, failure modes and
domain pitfalls; an intent comment on every non-trivial line explaining the **why**;
code and comments must agree, and a comment this change makes stale is a bug fixed in
the same edit.

The module's own header currently says, under "What this module does *not* do yet",
that a `Buffer` request is logged and skipped. That becomes false — fix it in the same
edit, and explain the three-request synthesis there, since it is the module's one
request that S originates rather than replays.

## Acceptance criteria

**Checkable anywhere:**

1. The request sequence is built by a **pure function** of the token (plus the bound
   dmabuf object/version), separate from the sending, and a test asserts its shape: the
   three opcodes in order, `add`'s six arguments in the right positions, and a modifier
   split correctly into hi/lo. Use a token with a **non-zero modifier**, a stride that is
   **not** `width × 4`, and a **non-zero offset**, so that a derivation or a swapped
   hi/lo fails the test rather than passing by coincidence.
2. `cargo test -p rayland-vtest -p rayland-relay -p rayland-venus-proto` still passes.

**Checkable on dop561:**

3. `cargo test -p rayland-s` — the existing `wayland_replay.rs` tests still pass against
   the real compositor and do not regress.
4. Running `scripts/wp0-vkcube-two-machine.sh`: S's log shows the three synthesized
   requests issued for at least one token, **no protocol error from the compositor**,
   and the app-side `wl_buffer` id mapped. That is this task's real bar.

**Checkable if criterion 4 passes, and it is 4.5, not 4.3:**

5. vkcube's cube appears on dop561's screen. Report it as 4.5 reached, separately from
   4.3 being done, and do not mark 4.5 done in the map on the strength of one run.

**Explicitly not claimed by any of the above:** that presentation is correctly paced or
tear-free. That is the commit-gating task (decision 3). If frames tear or appear early,
that is an expected result of this scope, and it should be recorded as an observation
rather than chased.

## Out of scope

- Commit gating on the G' completion signal.
- `zwp_linux_buffer_params_v1.create` (the async sibling); C still refuses it.
- The dmabuf v3 version cap, `ADVERTISED_FORMATS`, or the format advertisement.
- Multi-plane buffers.
- Anything in the (c)1/(c)2 return path, `progress_thread`, or the readback barrier.
- Retiring the `wl_shm` readback presentation path. It stays until WP0 is proven; two
  working paths beat one path and a regression.

## Licence to deviate

If the tree contradicts this plan's decomposition, **the tree wins.** Do the right thing
and **report the deviation** rather than implementing a brief you have found to be wrong.

The specific warning for this task: the planning side has now twice found that WP0's
written plan is short of what the code requires — first that C's half of 4.3 was already
complete, and now that S must originate `create_params` and not merely `add`. Assume
there is a third such gap. If the three-request sequence turns out to be four, or the
compositor rejects something the plan asserts it will accept, that is a finding to
report, not a brief to force.

## Reporting back

- **A diary entry** — the thinking, including anything that surprised you, whatever you
  found out about `send_request`'s fd ownership, and any wrong turn.
- **A project-map check**, and an update if a node's status genuinely changed.
- **`docs/OVERVIEW.md`**: §6.1's task table and its "specified sequence" both change.
  The sequence as written there is **wrong by one request** — correct it, and note the
  correction rather than silently rewriting, since the planning side reasoned from it.
  Bump "Last brought current".

Then a report for the planning side stating: what was built vs. what was **verified**
and by which evidence; whether criterion 4 passed and on how many runs; any deviation
and why; what this overturned; what it made false elsewhere; what remains unverified.

Be precise about run counts. One clean run is one clean run, not a rate.

## Branch and git discipline

Work on `wp0-wayland-proxy`. The laptop is primary; **never commit or push to `main`
from a non-laptop session.**
