# WP0 — where do the 77 milliseconds go?

## Goal

Account for the time in a WP0 frame. Two numbers, in order of importance:

1. **How many synchronous round trips a frame costs**, because that is what will decide
   whether Rayland works over a real network.
2. **Where the wall-clock time sits** between the application's submission and the frame
   callback returning to it.

**Measure. Fix nothing.**

## Verification location

**Needs S only — and loopback on dop561 is the right place to start, not a fallback.**

Two reasons this is better than the two-machine setup for a first attribution: both
daemons share **one monotonic clock**, so timestamps compare directly with no clock-sync
apparatus; and it removes the network as a variable, so whatever remains is everything
else. The two-machine run, when apollo returns, then measures exactly one thing — the
wire — by difference.

## Context

- **Front:** the latency half of Rayland's thesis, untouched since 4.5.
- **The state:** `docs/reports/2026-08-30-wp0-version-inheritance-report.md`. `vkgears`
  reports **10–13 fps** by its own counter; the vkcube soaks give 236–393 attaches per
  20 s on loopback and 261–489 two-machine. Roughly 75–100 ms per frame either way.
- **Why this matters more than anything else queued:** the bandwidth half of the thesis
  is now settled and decisive (~3.6 KB/frame of commands, 1,406× less return traffic).
  The latency half has never been examined. It decides whether Rayland is a way to work
  or a demonstration, and every application after this one inherits whatever it is.

## What planning already established, so you need not re-derive it

- **The network is probably not dominant.** Loopback (236–393 attaches/20 s) is **no
  faster** than two-machine (261–489). Confounded — on loopback C and S share a CPU and a
  GPU — but it is evidence, and it points inward.
- **Polling granularity alone does not obviously explain it.** `PARK_SLEEP` is 500 µs
  (`rayland-c/src/main.rs:173`), `PROGRESS_POLL` 200 µs (`rayland-s/src/main.rs:130`),
  `FLUSH_POLL` 50 µs. To account for 77 ms, a frame would need on the order of 150
  synchronisation points. **If it does, that count is the finding.** If it does not, the
  time is somewhere else and the budget will say where.

## Decisions already made

**1. Round-trip count comes first, and is reported even if the timing work is
incomplete. [Decided here.]**

On loopback a synchronous round trip is nearly free, so a time budget measured here
*understates* what a network will cost. The **count** does not: *n* round trips per frame
is an *n* × RTT floor on any link. At 0.3 ms on a LAN that is invisible; at 15 ms to
another city it is fatal, and no amount of bandwidth saving compensates.

So: count every point in a frame where **C blocks waiting for something from S** — fence
waits, blob syncs, replies, the ring barrier, anything that turns the pipeline
synchronous. Report the count per frame and what each one is waiting for.

This is the number that transfers from loopback to the wire. It is more valuable than any
millisecond figure this session can produce.

**2. Baselines are mandatory, and the session is worthless without them. [Decided here.]**

13 fps means nothing without knowing what the same application does with the same
compositor and no Rayland. Measure at least:

- `vkgears` **native** on dop561 against the same headless weston — the ceiling;
- `vkgears` through WP0 on loopback — the current figure;
- `vkcube` both ways, since every prior number in the record is vkcube's.

If native `vkgears` against headless weston is itself capped near 60 (or near anything
low), say so before attributing anything to Rayland. A compositor's own pacing has fooled
this project three times in a week.

**3. Instrument at stations, with one clock. [Decided here.]**

Timestamp a frame's journey at the boundaries that already exist rather than inventing
new ones: the application's submission arriving at C, C's send, S's receive, S's apply to
virglrenderer, GPU completion (the existing G′ signal), the `wl_buffer` commit, the
compositor's callback, and its delivery back to the app.

Gate it behind an environment variable following the existing convention
(`RAYLAND_S_REPLY_LOG` is the sibling). Sample a bounded number of frames rather than all
of them — this measures a pipeline, and instrumenting every frame perturbs the thing
measured.

**4. Report a distribution, not a mean. [Decided here.]** A frame that is usually 20 ms
and occasionally 400 ms is a completely different system from one that is uniformly 77 ms,
and they have completely different causes. Give the spread and the shape.

**5. Fix nothing. [Decided here.]** Not a poll interval, not a round trip, not a buffer
count. If the answer is obvious and the fix is one line, that is the *next* task, and it
will be a better fix for having the measurement in front of it. Tuning a constant while
measuring is how a measurement becomes a story.

## Named suspects — to be discriminated, not confirmed

Listed so the instrument covers them, **not** so one can be selected. Two planning-side
predictions have been wrong this week; treat these as a checklist for coverage.

| Suspect | What would show it |
|---|---|
| Synchronous round trips | Decision 1's count, and time spent blocked in C |
| Polling granularity | Sync points per frame × the relevant sleep |
| S-side apply / virglrenderer | Time between S's receive and its submit |
| GPU render time | Submit to G′ |
| Compositor pacing (FIFO throttle) | The native baseline, and commit-to-callback |
| Blob sync volume on the forward path | Bytes and time in the C→S blob channel per frame |
| Loopback CPU/GPU contention | Whether native and relayed runs interfere when co-resident |

## Inputs and outputs

| File | Change |
|---|---|
| `crates/rayland-c/src/`, `crates/rayland-s/src/` | Gated station timestamps and a blocked-time / round-trip counter. Additive only. |
| `scripts/` | A runner that produces the budget and the baselines in one go, header explaining the loopback-clock reasoning. |
| `docs/data/<dated>/` | Raw per-frame samples, the baselines, and the summary table. |

## Constraints

- **Additive instrumentation only.** No change to what is relayed, when, or in what order.
- The instrument must not extend any lock hold — build strings outside the guard, as the
  event witness does.
- `OVERVIEW.md` §7's standing constraints all still bind.
- Record the topology in every artifact. The soak harness already does this; anything new
  must too, so a loopback figure can never later be mistaken for a two-machine one.

## Conventions requirement

`CLAUDE.md`'s conventions bind in full. And the hazard applies with particular force here:
**do not write a quantity into a comment that nothing measures.** This session exists to
put measurements behind numbers; it must not generate new unmeasured ones.

## Acceptance criteria

1. **Round trips per frame**, counted, with what each waits for.
2. **The three baselines** of decision 2, reported before any attribution.
3. **A time budget** from application submission to callback delivery, as a distribution
   with spread, summing to the observed frame time — or an explicit statement of how much
   is unaccounted for, which is itself a finding.
4. A statement of which suspects the evidence rules **out**.
5. Artifacts committed, topology recorded.

**Not expected:** a fix, a faster frame, or a two-machine figure. A session ending "here is
where the time goes, and here is what it will cost over a real network" is a complete
success.

## Out of scope

- Any optimisation.
- The `wl_shm` decision, the keyboard, the commit gate, the ~1 MB first-frame outlier.
- The toolkit-scouting session.
- The two-machine confirmation of `vkgears`, which is owed but needs apollo.

## Licence to deviate

If the tree contradicts this plan, **the tree wins** — do the right thing and report the
deviation. In particular, if the baselines show the ceiling is much lower than expected,
stop and report; the attribution question changes shape entirely.

## Reporting back

- **A diary entry**, including which suspects were ruled out and how.
- **A project-map check.**
- **`docs/OVERVIEW.md`**: §5 currently carries the bandwidth result with no latency
  counterpart. Add one, whatever it says.
- **A correction, dated, leaving the original standing:** the previous report's headline
  states that a second application "works end to end through WP0". That run was
  **loopback**; §6 says so but the headline does not, and §7's own rule is that loopback
  proves little about the forward break or feedback. The claim wants its qualifier
  attached wherever it is repeated, and the two-machine confirmation recorded as owed.

Then a report: the round-trip count, the baselines, the budget with its spread, what is
ruled out, and what the smallest useful next step is.

## Branch and git discipline

`wp0-wayland-proxy`. The laptop is primary; **never commit or push to `main` from a
non-laptop session.**
