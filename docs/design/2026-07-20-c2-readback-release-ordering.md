# (c)2 — readback release ordering: hold the head-advance that releases a readback draw until its pixels ship

**Status:** design/spec, 2026-07-20. Fixes the ~1/11 residual left by the readback-completion gate
([`2026-07-19-c2-readback-completion-gate.md`](2026-07-19-c2-readback-completion-gate.md)), whose
cause is pinned in
[`2026-07-19-c2-true-remote-mapped-sync.md`](2026-07-19-c2-true-remote-mapped-sync.md). Read both
first. This is **Direction A** (keep fence feedback disabled); Direction B (make fence feedback work
over a real network) is a separate, larger track — the fence-feedback buy-back was only ever validated
on loopback and the app SIGABRTs over a real link (proven 2026-07-20, gate-independent).

## 1. The residual this fixes

With fence feedback disabled — the only config that renders over a real network — the readback-completion
gate takes the stale rate from *most runs losing 1–4 frames* to **10/11 runs fully clean**. A ~1/11
residual of the same `N == N−1` whole-frame signature remains. Its cause is **not** an S-side gate hole
(S writes only the readback blob among app blobs, so the gate is tight) but the **release ordering**:

- With feedback off, the application's `vkWaitForFences` is released by the ring **`head`** it spins on
  in `vn_ring_wait_seqno` — carried to C as `S2C::RingProgress { consumed_tail }`, which becomes C's
  local `head`.
- `progress_thread` ships that head-advance in **step 1** (the moment the ring retires) — *before* the
  gated readback delivery in **step 2** (after the GPU fence). The design intended the *feedback word*
  (step 2, after the pixels) to release the readback draw; with feedback off, the step-1 head-advance
  releases it instead.
- So the app is released, then reads its own local readback blob on C, and occasionally does so *after*
  C applies the step-1 `RingProgress` but *before* it applies that frame's readback `BlobData`. Stale
  previous frame.

The gate reduced the rate because step 2 now ships *fresh* pixels; it did not eliminate it because the
release still precedes the pixels.

## 2. The invariant

**The application's `head` must not advance past a readback-bearing draw's completion until that draw's
readback pixels have been delivered to C.** Everything *earlier* than that draw must still be released
promptly, or the frame cannot make progress.

## 3. Why the obvious fixes are wrong (they were tried on paper)

- **Withhold the entire step-1 head-advance while a delivery is pending.** Deadlocks. The fixture issues
  **two submits per frame** — an upload copy (submit A, ring position Pa) then the draw-and-readback
  (submit B, Pb > Pa) — and the app **waits on the upload copy's fence** before recording the draw. That
  wait also releases via the head. Holding the whole head-advance strands the app at the upload wait, so
  it never submits the draw, so no readback ever comes, so the head never advances. Dead.
- **Collapse the two steps: wait for the fence and ship the readback before any head-advance.**
  Reintroduces exactly what the two-step split exists to prevent — the fence wait blocks the head-advance
  and the doorbell, starving the message thread that feeds the ring (the (c)2 deadlock the engine actor
  and the split were built to avoid).

## 4. The design: cap the step-1 head-advance at the pending submit, release after step 2

`progress_thread` already runs the readback in two steps. Change **only where the head-advance is
reported**, so it is split around the readback-bearing submit rather than shipped whole in step 1.

**The cap.** While a readback delivery is pending for the latest submit at ring position `Pb`
(`Applier::latest_submit_pos`), report `consumed_tail` capped at `Pb` — i.e. `min(head, Pb)`. This
releases everything the ring has consumed *up to* that submit (crucially, the upload copy at Pa < Pb and
its fence-wait) while holding the head just below the point that would release the draw whose readback is
still owed. `head`, `applied_tail`, and `latest_submit_pos` are all **free-running** ring byte counters
in one coordinate space (the readback fence already compares `head` against `latest_submit_pos`), so the
cap is a direct `min`.

**The release.** In step 2, once the GPU fence has retired and the readback has been shipped, report the
**full** `head` (uncapped) for that ring, releasing the draw onto pixels that are already on the wire.

**The copy submit must release the head, not hold it.** S cannot tell the copy (A) from the draw (B) by
parsing the opaque ring. When the pending submit turns out to be a copy — the fence retires and
`take_app_blob_writes` returns **empty** (no readback advanced; the copy wrote a texture image, not an
app blob) — S must report the **full** head anyway, so the app is released from its upload-copy fence-wait
and proceeds to submit the draw. Capping-and-never-releasing a copy submit is the deadlock of §3 by
another route. So the rule is: **cap while pending-and-unresolved; release fully the moment the pending
submit resolves** — whether it resolved by shipping a readback (draw) or by proving it owed none (copy).

**Mechanism note for the plan (not the contract):** the ring mirror's `reported_head`
(`RingMirror::take_progress`) must not advance past what was actually reported, or the held-back remainder
never ships. The cap therefore cannot simply be applied to the return value of the existing
`take_progress`; the plan must either (a) add a capped variant that advances `reported_head` only to the
cap, or (b) track the cap and the true head in `progress_thread` and re-report on release. The plan
settles which; both express the same invariant.

## 5. Interaction with the landed gate (the subtle part)

The landed gate's "empty → keep the delivery pending and poll again" behavior (for the copy submit and
the in-flight-DMA case) is what makes the cap tricky: a capped head plus a still-pending delivery is
exactly the deadlock shape. The two must be reconciled so that **the head is released for the copy submit
even though the delivery does not complete on it.** Concretely, "release the head" (let the app proceed)
and "complete the delivery" (ship the readback, advance `last_delivered_submit`) become two distinct
events: the copy submit does the former but not the latter; the draw submit does both. The plan must make
that separation explicit and test it, because getting it wrong reproduces the deadlock rather than the
stale frame.

## 6. What does not change

- No `rayland-c`, wire, or forward-path change: `RingProgress { consumed_tail }` already carries a
  cappable frontier; this only changes *what value S reports when*.
- Fence feedback stays disabled (Direction A). The feedback-over-network path (Direction B) is untouched.
- The readback-completion gate (`delivery.rs`, the step-2 advance check) stays; this composes with it.

## 7. Testing

- **End-to-end (the proof):** `scripts/c2-icosa-two-machine.sh` over the real link must go from ~1/11
  residual to **0 stale across many runs** (≥ 20, since the residual is ~1/11 — the run count must make a
  surviving residual statistically visible). No new deadlock/`SIGABRT`/`QUEUE_REGISTER_DEADLINE`.
- **Unit (the cap logic):** the head-cap decision extracted pure and tested — given (head, pending
  submit position Pb, delivery resolved?), assert it reports `min(head, Pb)` while pending-unresolved and
  the full head once resolved (both draw-resolved and copy-resolved), and that a copy submit's full-head
  release cannot be skipped.
- **Regression:** the existing loopback `icosa_cpu_renders` e2e (fences on) and `refapp` e2e must still
  pass — the cap must be inert when nothing is pending / no readback is owed.

## 8. Risks to confirm during implementation

- **Does the upload copy's fence-wait actually release via the head** (making the copy-release handling of
  §4/§5 load-bearing), or via a different mechanism? Strong prior: yes — with feedback off, a
  `vkWaitForFences` is a `vn_ring_wait_seqno` on the head. The first two-machine run settles it: if the
  copy-release path is wrong, the app deadlocks at the upload wait (`QUEUE_REGISTER_DEADLINE`), a loud,
  unambiguous failure — not a silent regression.
- **Cap coordinate correctness across ring wrap.** `latest_submit_pos` and `head` are free-running and the
  ring wraps mid-run (~frame 82); the `min` must be in free-running space, never masked buffer offsets.
  The readback fence already relies on this invariant, so the plan reuses it rather than re-deriving it.
