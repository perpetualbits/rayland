# Amendment to pre-registration 2 — written before any further data

## Why an amendment exists at all

The n = 31 run **aborted at run 13 of 62**, exactly as its protocol required: run B-7's fixture failed
Vulkan initialisation (`VK_ERROR_INITIALIZATION_FAILED`), never connected to `rayland-c`, and the
witness caught it. Per the protocol — *"any run that is not [relay-verified, 120 frames, 0 stale] is a
hard stop, not a dropped sample"* — the experiment stopped.

**That protocol is unrunnable.** One transient in 14 runs means a 62-run sweep with a hard stop
finishes essentially never. This is a defect in my design, not in the machines, and it is being fixed
in the open rather than by quietly re-running until a sweep completes — which would be the same thing
as dropping bad runs, with extra steps.

## What is being changed, and what is not

**Changed:** a run that produces **no timing sample** — the application fails to initialise, or the
witness shows it never relayed — is **re-attempted**, bounded at 5 per arm-repetition and **counted**.
This is the same distinction the soak harness already draws: a run that produced a verdict is data
whatever the verdict; a run that never got that far measured the machines, not the arm, and must
enter neither numerator nor denominator.

**Not changed:** a run that *completes* and shows **stale frames** remains a hard stop. That is a
correctness failure, it is data, and it must never be retried away.

**New reporting requirement, because the retry itself could hide the answer:** retries are counted
**per arm** and reported with the result. If arm B needs materially more retries than arm A, that is a
reliability signal about feedback and is more important than the timing number this experiment was
designed to produce. It would be reported as the primary finding.

## Restarting rather than continuing

The 13 runs already collected are **discarded, not merged**. They were gathered under a different
stopping rule, and including them would make the sample depend on when the protocol changed. The full
31 × 2 restarts under this amendment.

## What is not being touched

Sizing (n = 31 per arm), metric (per-run median `draw_readback_us`), interleaving, the primary
analysis, the outcome table, and the **pre-commitment against escalation**: if this is still
inconclusive, the answer is "too small to matter", not a larger n.

## An observation to carry, not a conclusion

One initialisation failure in 14 runs of `icosa-gpu` is a higher rate than the 0-in-400 measured twice
for `icosa-cpu` — different fixture, different harness, and **n = 1**. It is recorded here because if
the restarted run shows more of them, the retry counts will say so, and that would be worth more than
the ratio.
