# WP0 — derive child versions from the sender, and stop pretending the session survives

## Goal

Fix the two defects `vkgears` found in thirty seconds: child objects carrying the wrong
version when S caps a bind, and a `catch_unwind` that logs recovery it cannot deliver.
Fix the **first as a class**, not as its third instance.

## Verification location

**Needs both machines**, with **headless weston** on S — weston offers `xdg_wm_base` v5
against the descriptor's v6, so it caps naturally and reproduces the crash. COSMIC does
not cap and will not reproduce it. Part of the work is unit-testable anywhere and must be.

## Context

- **Front:** WP0, after the rate-and-traffic session.
- **The findings:** `docs/reports/2026-08-30-wp0-rate-and-traffic-report.md` §4 and
  `docs/data/2026-08-30-wp0-rate-and-traffic/second-applications.md`.
- **The crash:** `vkgears-crash-rayland-s.log` in the same directory.
- **Existing code:** `crates/rayland-s/src/wayland_client.rs` — `handle_bind`,
  `handle_request` (the `NewId` arm at ~line 990 and the `send_request` call at ~line
  1019), `synthesize_buffer`, `IdMaps`.
- **The harness:** `scripts/wp0-soak.sh`, and weston's three required flags
  (`--renderer=gl`, `__EGL_VENDOR_LIBRARY_FILENAMES` pinned to Mesa, **`--idle-time=0`**).

## Decisions already made

Labelled with evidence class.

**1. A child's version comes from its sender, never from the wire. [Measured — read from
`wayland-backend-0.3.15` source.]**

`client_impl/mod.rs:366` panics with `expected version {object.version} but got {version}`,
where `object` is the **sender**. In Wayland a `new_id` argument always inherits the
parent object's version; the sole exception is `wl_registry.bind`, which carries an
explicit version and which S already handles on a separate path (`handle_bind`).

So `handle_request`'s `NewId` arm must build `child_spec` from **the S-side sender
object's version**, and the `version` field on the wire `WaylandArg::NewId` must stop
feeding it. Keep the field — it is useful in the log, and it is the app's view — but it
must no longer decide anything.

**This removes the class rather than the instance.** The rule has now bitten three times:
`create_immed`'s `wl_buffer` child, the params object, and now `get_xdg_surface`. All
three are the same mistake, and after this change none of them can recur, including in
`synthesize_buffer` — which should be reworked to use the same lookup rather than keeping
its own special case.

**2. S must track versions, because `ObjectId` does not expose one. [Measured — the
client `ObjectId` API exposes `interface()` and `protocol_id()` only.]**

Add a version to what `IdMaps` records: seeded at bind time with the **capped** value
(`version.min(g.version)`, which `handle_bind` already computes), and propagated on every
child creation, where the child takes the sender's version.

Note the invariant this establishes, and put it in a comment: **every object's version
equals the capped version of the global it descends from.** That is what makes a single
lookup sufficient.

**3. `catch_unwind` around `send_request` cannot save the session. Stop claiming it does.
[Measured — read from `wayland-backend-0.3.15` source.]**

The report attributes the poisoning to rayland's `maps` mutex. **That is wrong.** The
second panic is at `client_impl/mod.rs:115`, a bare `unwrap()` inside
`ConnectionState::lock_protocol`, on wayland-backend's own `Mutex<ProtocolState>`. The
version panic fires at line ~366 **inside `send_request`, while that lock is held**. So a
protocol-violation panic poisons the backend's connection state permanently, and the next
backend call — any backend call — unwraps it and takes the process down.

No lock discipline on rayland's side changes this. The wrapper converts an immediate,
legible crash into a reassuring log line followed by a segfault one call later, which is
strictly worse than no wrapper.

Required behaviour instead:

- **Prevent** what can be prevented. With decision 1, the version mismatch becomes
  impossible by construction. Check what else is cheaply checkable before calling
  `send_request` — an interface mismatch against the message descriptor's
  `child_interface` is the other panic on that path.
- **On a panic that still gets through, treat the Wayland replay connection as dead.**
  Log it as fatal, stop replaying, and do not issue another backend call. Whether the
  process then exits deliberately or the relay continues without presentation is yours to
  choose — but **no log line may assert a recovery that did not occur.** That is the
  hazard from two days ago, now in code rather than prose.
- The comment `.expect("the WP0 id maps lock is never poisoned")` is a claim of the same
  kind. Re-examine whether it is still true after this change and say so either way.

**4. The soak's failure definition gets a teardown guard. [Decided here.]**

Run 13's two dropped events landed after the app had destroyed its objects and
immediately before `session ended cleanly`. Ignore drops that occur after the application
begins tearing down, so `1 in 60` stops meaning something it does not mean.

Do this carefully and say exactly what it excludes. The brief that commissioned the soak
warned that the exclusions are where a soak quietly stops measuring anything, and this is
that moment. A guard keyed on "the app has started destroying objects" is defensible; a
guard keyed on "near the end of the run" is not.

## Out of scope, deliberately

- **The ~1 MB first-frame outlier.** One exclusion-ON run shipped a frame's worth of
  bytes. It is one frame per session, the hypothesis is recorded, and it is real pixel
  traffic that deserves its own task rather than a corner of this one.
- The `wl_shm` decision, the keyboard, the commit gate, frame-time attribution.
- `vkcube --c N` stalling at one attach — a real observation about `--c`, not about
  Rayland.
- The toolkit-scouting session, which is separate.

## Inputs and outputs

| File | Change |
|---|---|
| `crates/rayland-s/src/wayland_client.rs` | Version tracking in `IdMaps`; `child_spec` from the sender; `synthesize_buffer` unified onto it; the `catch_unwind` behaviour per decision 3. |
| `crates/rayland-s/tests/` | The unit test below. |
| `scripts/wp0-soak.sh` | The teardown guard. |
| `docs/data/<dated>/` | Logs of the verification runs. |

## Constraints

- `OVERVIEW.md` §7's standing constraints all still bind.
- Do not change what is relayed, only the version stamped on children and the failure
  behaviour.
- `RAYLAND_S_SHIP_PRESENTED` stays as declared debt; it is a measurement bypass and will
  be wanted again.

## Conventions requirement

`CLAUDE.md`'s conventions bind in full. Two comments in particular carry knowledge that
cost three separate failures to acquire, and should be written for someone who has not
read this report: **why a child's version comes from its sender**, and **why
`catch_unwind` around `send_request` does not make the session survivable.**

## Acceptance criteria

**Anywhere:**

1. A unit test asserting that a child created from a **capped** bind gets the capped
   version, not the version the wire carried. **Show the mutation**: with `child_spec`
   built from the wire version the test must fail, and it must fail on that assertion.
2. `cargo test -p rayland-c -p rayland-s` passes; the pure set is still 83.

**On the two machines, against headless weston:**

3. `vkgears` no longer crashes `rayland-s`, and gets as far as it can. Report where it
   stops if it stops — the version fix may only reveal the next gap, and that is a fine
   outcome.
4. `vkcube` still works: a soak of at least 20 runs with the guarded definition, with the
   rate and the throughput range reported. This is a change to the replay path's core, so
   the previous rate is the baseline to not regress against.
5. `rayland-icosa-window` still refuses cleanly on `wl_shm`, `rayland-s` untouched.

**Not claimed:** that `vkgears` renders, that any pacing question is addressed, or a new
traffic figure.

## Licence to deviate

If the tree contradicts this plan, **the tree wins** — do the right thing and report the
deviation.

Specifically: decisions 1 and 3 were derived on the planning side by reading the
dependency's source, not by running anything. If the tree or the backend behaves
differently from that reading, say so — the last two prompts each contained one planning
assumption that the machine corrected, and that is the system working.

## Reporting back

- **A diary entry**, including a dated correction to the previous report's attribution of
  the poisoning to rayland's `maps` mutex. Leave the original standing; the point of the
  house pattern is that the record shows what was believed and when.
- **A project-map check.**
- **`docs/OVERVIEW.md`**: §6.4's hazard list should now record the version-inheritance
  rule as **closed by construction** rather than as a recurring hazard, and should record
  that a `catch_unwind` around a dependency's panicking API is not a recovery mechanism
  unless that dependency's locks survive the panic.

Then a report: what was fixed, what was verified and by which evidence, where `vkgears`
got to, the guarded rate, and what remains unverified.

## Branch and git discipline

`wp0-wayland-proxy`. The laptop is primary; **never commit or push to `main` from a
non-laptop session.**
