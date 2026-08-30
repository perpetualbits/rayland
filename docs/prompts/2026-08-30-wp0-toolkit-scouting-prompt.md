# WP0 — what does a real toolkit actually ask for?

## Goal

Replace the planning side's **predicted** list of what a normal application needs from
`rayland-c`'s fake desktop with a **measured** one, by running `solarsim` against the
proxy and recording exactly where it dies, and by building a minimal toolkit window probe
that separates "the toolkit works through Rayland" from "solarsim works through Rayland".

**This is a scouting session. Fix nothing.**

## Verification location

**Needs both machines.**

## Context

- **Front:** WP0, after 4.5.
- **The plan this serves:** `docs/design/2026-08-29-wp0-what-next-thin-line-and-rope.md`
  §3 (the inventory), §4 (why solarsim), §6 (ordering). If that note has not been filed
  yet, it is arriving in the same or an adjacent session; proceed without it.
- **The application:** `~/git/solarsim`, a separate GPL-3.0 repository. **Do not vendor
  it into `rayland`.** Build and run it where it lives.
- **The proxy's advertised globals:** `wayland_proxy.rs:995-998` — `wl_compositor`,
  `xdg_wm_base`, `zwp_linux_dmabuf_v1`, `wl_seat`. Four.

**Why a second application at all.** `vkcube` is bare Vulkan over libwayland and is
unusually undemanding: a window, some GPU images, nothing else. `solarsim` is `wgpu` 29 +
`winit` 0.30 + `egui` 0.35, so its Wayland client is smithay-client-toolkit over the
pure-Rust `wayland-client` — a **second client implementation** against a proxy that has
only ever faced libwayland.

## Decisions already made

Labelled with evidence class, because a decision in a prompt is an assumption carrying
authority, and the last unlabelled one here cost several sessions.

**1. Upgrading `winit` is not a fix. [Inferred, from reading solarsim's `Cargo.toml` and
the toolkit's requirements — high confidence.]**

The owner asked whether a newer `winit` would help. It will not, and the session should
not spend time on it. The missing pieces are not bugs that a later version fixed; they
are services every version of every toolkit expects a desktop to provide, because every
real desktop provides them. If the run shows otherwise, that is a genuine surprise worth
reporting.

**2. The probe is a standalone crate, outside the workspace. [Decided here.]**

Put it at `tools/wgpu-window-probe/` with its own `Cargo.toml`, and add
`exclude = ["tools/wgpu-window-probe"]` to the workspace manifest.

Reasoning: `wgpu` drags several hundred transitive crates. The 83-test pure set and
`cargo build --workspace` are run constantly and on machines with no GPU; making them pay
for `wgpu` to support a probe that is only ever run by hand on two machines is a bad
trade. Precedent for a non-fixture demo crate exists (`rayland-icosa-window`, annotated
"demo (NOT a fixture)"), but that one does not carry this dependency weight.

If the tree suggests a better home, take it and say so.

**3. The probe does the minimum. [Decided here.]**

A `winit` window, a `wgpu` surface, clear to a solid colour, present, count frames, exit
cleanly after a fixed count. **No `egui`, no textures, no input handling, no resize
logic.** It is the toolkit-stack analogue of `vkcube`: small enough that a failure is
unambiguous, and permanent enough to re-run after every change.

Match solarsim's major versions (`wgpu` 29, `winit` 0.30) so the probe exercises the same
stack it is standing in for.

**4. The planning side's prediction, recorded so it can be refuted. [Speculative.]**

The expectation is that solarsim dies at window creation for want of `wl_shm`, then
`wl_output`, then `wl_subcompositor` — the services behind CPU-drawn images, monitor
information, and the app's own window border.

**Do not shape the run to confirm this.** If it dies somewhere else entirely, that is the
more valuable result and the whole reason the session exists. Two predictions of this kind
made from the planning side this week have been wrong in their details.

**5. Fix nothing.** Not a missing global, not the keyboard, not the frame rate. Every
temptation here is a separate, scheduled task.

## The runs, in order

**A. Control: solarsim natively on apollo.** Confirms the binary, its assets and its GPU
path work at all on that machine, so a later failure is attributable to Rayland rather
than to the build. If it fails here, stop and report — nothing downstream means anything.

**B. solarsim through the WP0 proxy.** `WAYLAND_DEBUG=1` on the application, both
daemons' witnesses on, both logs captured. Record **the first failure precisely**: the
error text, which global or request it concerns, and how far the app got.

Then, if the failure is a missing global and the app can be coaxed past it by any means
that does not require code changes, do so and record the *next* failure — the list is
more useful than its first entry. If it cannot, one entry is fine; do not build
scaffolding to get further.

**C. Control: the probe natively on apollo.** Same reasoning as A.

**D. The probe through the WP0 proxy.** Same capture. Compare with B: anything the probe
also hits is the toolkit stack; anything only solarsim hits is solarsim's own complexity.

## Inputs and outputs

| File | Change |
|---|---|
| `tools/wgpu-window-probe/` | New standalone crate per decisions 2 and 3, with a README-grade header explaining what it is for and why it is outside the workspace. |
| `Cargo.toml` | The `exclude` entry. |
| `scripts/` | A runner for the probe through WP0, modelled on `scripts/wp0-vkcube-two-machine.sh`. Reuse its address derivation and its `--gpu_number`-style gotcha header conventions. |
| `docs/data/<dated>/` | Both applications' logs, both ends, both natively and through the proxy. |

## Constraints

- **No changes to `rayland-c` or `rayland-s`.** If a one-line diagnostic is the only way
  to see where something fails, add it, say so, and keep it separate from everything else.
- The standing constraints in `OVERVIEW.md` §7 still bind. Note in particular that the
  probe is **not** a fixture and is not governed by the fixture-discipline rule — but say
  so explicitly in its header so nobody later mistakes it for one.
- solarsim stays in its own repository.

## Conventions requirement

`CLAUDE.md`'s conventions bind in full for the probe and the script: doc-comments on every
function, type and module; intent comments on every non-trivial line explaining the *why*;
code and comments must agree. And the hazard from two sessions ago applies directly to the
script header: **do not write a quantity into a comment that nothing measures.**

## Acceptance criteria

1. The four runs above, with logs committed.
2. **A measured, ordered list** of what the toolkit stack asks for that the proxy does not
   provide — replacing the design note's §4 prediction. Ordered by the order they are hit,
   because that is the order they would have to be fixed in.
3. A clear statement of which failures the probe shares with solarsim and which are
   solarsim's alone.
4. The probe builds and runs natively on apollo, and is re-runnable by one command.

**Not expected:** a working solarsim, a working probe through Rayland, or any fix. A
session that ends with "here is exactly what is missing, in order, measured" is a complete
success.

## Out of scope

- Advertising any new global.
- The `wl_shm` design decision — that is the owner's, and it needs this session's evidence
  first.
- The keyboard (`wl_keyboard.keymap`), the frame-time question, the commit gate.
- Upgrading `winit`, `wgpu` or `egui` in solarsim.
- Anything in the rate-and-traffic session, which is separate.

## Licence to deviate

If the tree or the machines contradict this plan, **the tree wins** — do the right thing
and report the deviation.

Specifically invited: decision 4 is a guess. Refuting it is worth more than confirming it.
And if run B fails so early that the probe becomes the only useful vehicle, reorder freely
and say why.

## Reporting back

- **A diary entry**, including the measured list and anything that surprised you.
- **A project-map check.**
- **The design note**, if it has been filed: add a **dated addendum** recording the
  measured list. Do **not** edit §4's speculative prediction — the house pattern leaves the
  original standing so the record shows what was guessed and what was found.
- `docs/OVERVIEW.md` if this makes anything there false.

Then a report: what each of the four runs did, the ordered list of missing services, which
are toolkit-wide and which are solarsim-specific, and what the smallest next step is.

## Branch and git discipline

`wp0-wayland-proxy`. The laptop is primary; **never commit or push to `main` from a
non-laptop session.**
