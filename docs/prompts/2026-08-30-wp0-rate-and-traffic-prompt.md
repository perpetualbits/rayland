# WP0 — a rate, and a traffic number that survives repetition

## Goal

Turn WP0's two headline claims into measurements that survive repetition: a **failure
rate** for the end-to-end path, and a **per-frame traffic characterisation** with spread,
including an explanation of why C→S rose.

No new features. This session applies the hazard the last one wrote down — *a claim in a
comment is not a measurement* — to the claims the last one made.

## Verification location

**Needs both machines** throughout. The soak is the scarce-resource use: it wants hours
when nobody is working, and it wants them unattended.

## Context

- **Front:** WP0, immediately after 4.5.
- **The state:** `docs/reports/2026-08-29-wp0-end-to-end-report.md` and
  `docs/data/2026-08-29-wp0-recycled-id-fix/traffic-before-after.md`.
- **Existing instruments:** `scripts/soak-failure-rate.sh` (never pointed at this path),
  `RAYLAND_C1_METRICS=1` on C, the event witness in both daemons.

**What planning checked in the tree, so you need not:** the `presented` exclusion is
applied in both blob paths (`apply.rs:1388` and `apply.rs:1530`) and is narrow in both,
so the (c)2 readback path is untouched structurally and not merely by passing its test.
The residual S→C blob traffic is the `venus_internal` reply arena at roughly 1.3 KB per
frame — expected, and genuinely C's news.

## The question the last report left

**C→S rose from 804,814 to 1,626,138 bytes over *fewer* frames** — 120 down to 96 — which
is 6.7 KB/frame before and 16.9 KB/frame after, a 2.5× rise per frame. The report does not
mention it.

C→S is now roughly 90% of all traffic, and the size of the command stream is the whole
thesis. Either it is run-to-run noise, in which case repetition says so, or the
presented-exclusion changed the forward path, in which case that is a design finding. It
must not stay unexplained.

## Decisions already made

Each is labelled with its evidence class, because a decision in a prompt is an assumption
carrying authority and the last one that was not labelled cost this project several
sessions.

**1. A WP0 failure must be defined from the logs, not from a screen. (Decided here;
reasoning below.)**

A soak cannot have a human watching for a cube. Define a run as **failed** if any of:

- any `Invalid ObjectId` on either daemon;
- any event drop other than the known-and-accepted `carries-fd` on `wl_keyboard.keymap`;
- any Wayland protocol error, any `catch_unwind` trip, any panic, any daemon exit before
  the harness stops it;
- **liveness**: fewer than *K* attaches in *T* seconds, with *K* and *T* chosen from the
  measured healthy rate (~14 fps in the reference run — pick a floor well under it, and
  say what you picked and why).

Report the failure *modes* separately, not just a count. A rate that mixes a protocol
error with a liveness miss is two rates wearing one number.

**2. The soak runs against a headless compositor on S, not against COSMIC. (Inferred —
and the inference is load-bearing, so test it before committing to the run.)**

This session's own discovery is that a compositor emits frame callbacks only for surfaces
it actually composites. An overnight soak on COSMIC would therefore produce **false
failures every time the screen blanks or locks**, and the liveness criterion above would
fire on correct behaviour.

Nesting a compositor inside COSMIC does not fix it — a nested compositor that stops
receiving frame callbacks from its host will throttle its own repaint and withhold
callbacks from its clients in turn. The property actually needed is a compositor that
composites on a timer regardless of any screen: **weston's headless backend**.

`rayland-s` reads plain `WAYLAND_DISPLAY` (`main.rs:143`), so pointing it at a headless
weston is an environment change and no code.

**The uncertainty to resolve first:** headless weston must still accept a
`zwp_linux_dmabuf_v1` import, which needs the GL renderer rather than the pixman one.
Verify that with a single short run before starting any soak, and report what you found —
this is exactly the kind of dependency assumption that has bitten twice this week. If
headless weston cannot import the dma-buf, say so and fall back to COSMIC with blanking
and locking disabled, noting that the soak is then only as trustworthy as that setting.

**3. Traffic runs are fixed-frame-count, not fixed-time. (Decided here.)** The
before/after pair compared 120 frames against 96 and then reported per-frame figures
derived from different runs. Fix the frame count so runs are comparable, and report
per-frame numbers with a spread across repeats rather than a single ratio.

**4. The C→S question gets an A/B, not an argument.** Run the same fixed-frame workload
with the presented-exclusion **enabled** and **disabled** — the exclusion is a set
membership test, so a temporary env-gated bypass is a small, clearly-separable change.
That distinguishes "the forward path is coupled to the return-path change" from "the two
earlier runs differed for unrelated reasons" without anyone having to reason about it.

If the bypass turns out not to be cleanly gateable, say so and compare against the
recorded before-figures instead, stating that the comparison is across a code change
rather than a switch.

## Inputs and outputs

| File | Change |
|---|---|
| `scripts/` | A WP0 soak harness, or `soak-failure-rate.sh` extended to drive this path, implementing the failure definition above. Follow the existing script's header conventions — the reason each choice was made, not just the command. |
| `docs/data/<dated>/` | Raw logs, the per-run table, and the traffic figures. |
| `docs/DIARY.md`, `project-map.js`, `docs/OVERVIEW.md` | Per the standing rules. |

## Constraints

- **No feature work.** If the soak surfaces a defect, that is a finding and the next
  task; do not fix it in this session unless leaving it in makes the measurement
  impossible, and say so if that happens.
- The standing constraints in `OVERVIEW.md` §7 all still bind.
- Any temporary bypass added for decision 4 must be gated, documented, and either removed
  before the session ends or explicitly listed as debt in the report.

## Conventions requirement

`CLAUDE.md`'s conventions bind in full: doc-comments on every function, type, trait and
module; intent comments on every non-trivial line explaining the *why*; code and comments
must agree. The script's header carries the reasoning for the failure definition and for
the headless-compositor choice, since both are non-obvious and both will be questioned
later.

Note also that this session's script is a place where the new hazard applies directly:
**do not write a quantity into a header that the script does not measure.**

## Acceptance criteria

1. Headless-weston dmabuf import verified or ruled out, with the finding stated.
2. A soak run with the failure definition above, of a size that actually bounds
   something. State *n*, the failures, and the modes. Recall that absence of failure in
   *n* runs bounds the rate and does not establish zero — say what it bounds it to.
3. Traffic measured over **at least five** fixed-frame runs, reporting per-frame C→S and
   S→C with the spread, not a single figure.
4. The C→S rise explained: coupled to the presented-exclusion, or not, with the A/B that
   settles it.
5. Artifacts committed under `docs/data/`.

**Not claimed by any of this:** freedom from tearing or correct pacing. The commit gate is
still untouched and out of scope.

## Out of scope

- The commit gate on the G' signal.
- `wl_keyboard.keymap` and the fd-bearing events.
- The dmabuf probe-bind volume.
- The identifier-hazard sweep — real and overdue, but it is "anywhere" class work and
  should not consume two-machine time.

## Licence to deviate

If the tree or the machines contradict this plan, **the tree wins** — do the right thing
and report the deviation.

Specifically: decision 2 is an inference about weston's headless backend made from here,
without access to the machine. If it is wrong, that is expected and reporting it is the
right outcome; do not build elaborate scaffolding to rescue a planning assumption.

## Reporting back

- **A diary entry** — including what the soak's failure definition had to exclude and
  why, since those exclusions are where a soak quietly stops measuring anything.
- **A project-map check.**
- **`docs/OVERVIEW.md`**: §5's account of WP0 gains the rate and the traffic figures,
  replacing any number that currently rests on a single pair of runs.

Then a report: the rate and what it bounds, the traffic numbers with spread, the C→S
answer, the failure modes seen, and what remains unverified.

## Branch and git discipline

`wp0-wayland-proxy`. The laptop is primary; **never commit or push to `main` from a
non-laptop session.**
