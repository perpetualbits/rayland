# (c)2 — the readback-completion gate: deliver only when S has produced a new frame's readback

**Status:** design/spec, 2026-07-19. The fix for the true-remote stale-frame defect pinned in
[`2026-07-19-c2-true-remote-mapped-sync.md`](2026-07-19-c2-true-remote-mapped-sync.md). Read that
findings document first: it establishes, with an independent forward-input witness, that the stale
frames are a **readback-completion lag on S**, not a forward mapped-blob relay race.

## 1. The confirmed defect, in one paragraph

Over a real network, ~2/120 `rayland-icosa-cpu` frames come back as the *whole previous frame*
(`frame N == native N−1`). A per-delivery correlation on S proved the cause: at every stale frame the
resident per-frame **uniform** (a forward input the draw reads directly) was already frame N while the
**delivered readback image** was frame N−1, and frame N's image was *never delivered at all*. S's
forward inputs were fresh; S's readback **delivery** shipped the previous frame's pixels and dropped
the current frame's. Loopback hides it (0/120) because the timing window never opens. The forward
relay in `rayland-c` and the QUIC wire are untouched by this fix — they were exonerated.

## 2. Why the current barrier is insufficient — the opaque-ring constraint

S's (c)2 readback completion barrier lives in `rayland-s`'s `progress_thread`. Its trigger is
deliberately **content-independent** (see [`2026-07-19-c2-ringidx-decode.md`](2026-07-19-c2-ringidx-decode.md)
§8): it fires when a `vkQueueSubmit` at a ring position newer than the last delivered has been
dispatched and the ring has drained, then issues a real completion fence
(`wait_for_work_retired(ctx, ring_idx)`) before shipping the application's blob writes.

That trigger is correct **for a one-submit-per-frame application** — there, every submit is a
draw-and-readback, so "a new submit retired" implies "a new readback exists." The icosa fixture breaks
that assumption: it issues **two submits per frame**, and only one of them writes the readback:

- **Submit A** — the fractal upload copy (staging buffer → texture image). Writes a `VkImage`. **Does
  not touch the readback blob.**
- **Submit B** — the draw plus `cmd_copy_image_to_buffer` (colour target → readback blob), in one
  command buffer, one submit. **This is the only submit that writes the readback.**

S treats the ring as opaque bytes (parsing it to classify submits is exactly the fragility the project
refuses — a decode bug would become a corruption bug). So **S cannot tell A from B.** The trigger
fires on both. When it fires on A (or before B's readback DMA has landed), the fence returns but the
readback blob still holds frame N−1, and the code ships that and releases the application anyway. The
current delivery logic (`progress_thread`, the `new_submit_dispatched` branch) calls
`take_app_blob_writes()` and then **unconditionally** clears `delivery_pending` and advances
`last_delivered_submit` — completing a "delivery" even when no new readback was produced.

**The constraint this exposes:** with an opaque ring and N submits per frame, the *only* signal
available to S that "a new readback exists" is that **the readback blob's contents advanced**. The
copy submit produces no such change. Some readback-content awareness is therefore not a regression
from the content-independent design — it is the **necessary generalization** that design did not
anticipate. This fix supplies it, reusing machinery S already has.

## 3. What S already has

`Applier::take_app_blob_writes` (`crates/rayland-s/src/apply.rs`) already returns **only the runs of
bytes S's GPU wrote into application blobs since the last call, and is empty when S wrote none.** Its
own doc-comment already names this exact defect: *"the readback buffer is written by the GPU's own
DMA, which can legitimately still be in flight after ... `head` ... a diff answers 'did these bytes
change?', never 'has the GPU finished?'."* There is also a Probe-A fingerprint path
(`take_blob_writes`/blob fingerprinting) built to "catch the GPU in the act" of a late DMA. The fix
composes these existing pieces; it does not invent new observation machinery.

## 4. The design: gate delivery on a new, stable readback

Change the delivery decision in `progress_thread` so that **a delivery completes only when S has
produced new, stable readback bytes since the last delivery.** Everything else about the barrier (the
structural trigger as a lower bound, the `wait_for_work_retired` fence, the largest-blob-first ship
order, the teardown/`ring_idx.is_none()` drop, the deadline) stays.

Precise behaviour, per delivery attempt (after the structural trigger fires and the fence returns):

1. **Determine whether the readback advanced.** Compare the current application-blob-writes against
   what was last delivered. In practice this is: did `take_app_blob_writes` produce a non-empty set of
   new S-written runs whose bytes differ from the last shipped readback? (Implementation may use the
   existing S-wrote diff directly, or a readback fingerprint compared to the last delivered
   fingerprint; the plan settles which, since both express the same "did the readback advance?"
   predicate.)
2. **Advanced → deliver.** Ship the app blob writes (largest-first, as now), clear `delivery_pending`,
   advance `last_delivered_submit`, and record the delivered readback identity (fingerprint) so the
   next comparison is against *this* frame.
3. **Not advanced → do not complete.** This trigger was a copy submit, or the draw's readback DMA has
   not landed yet. **Leave `delivery_pending` set and keep polling** (each poll re-issues the fence),
   so the delivery completes on a later poll once the readback actually advances.

This suppresses both observed symptoms: the spurious copy-submit delivery (no new readback → no
completion) and the stale/empty release that shipped frame N−1 into frame N's slot.

## 5. Correctness and generality

- **General to any application and any submits/frame.** The gate keys on *S-observed GPU writes into
  app blobs*, never on ring parsing or on the app's submit structure. A one-submit-per-frame app
  advances the readback every submit and is unaffected; an N-submit app delivers exactly on the
  submit(s) that actually produce readback.
- **No forward/wire/`rayland-c` change.** The forward path was proven fresh; this fix does not touch
  it, the relay, or the wire protocol.
- **Ships whole, real frames.** As before, the fence guarantees the delivered readback is a completed
  GPU result, not torn; the gate additionally guarantees it is the *new* frame's result, not a
  repeat of the last.

## 6. Edge cases and how they are bounded

- **Identical consecutive frames.** If two real frames produce byte-identical readback, a pure
  advance-gate would wait forever and the application would hang in `vkWaitForFences` until Mesa's
  ~3.5 s stall-abort. This never occurs in the icosa fixture (the fractal zooms and the MVP rotates
  every frame, so every frame differs) and is rare in real workloads, but it must not hang. **Bounded
  fallback:** if the readback has not advanced within a bound `B` after the trigger fired, deliver the
  current readback anyway. This is **correct**, because for a genuinely identical frame the bytes are
  identical; and it is harmless for a copy-then-draw sequence because the draw's write arrives well
  within `B` and delivers fresh. `B` must be chosen ≫ inter-submit round-trip latency and ≪ Mesa's
  stall-abort (the existing `PROGRESS_POLL` / `QUEUE_REGISTER_DEADLINE` constants frame the range).
  The exact value and whether it is a poll-count or a duration is an implementation detail for the
  plan; it must be justified against those two bounds in a comment.
- **Teardown (no readback to deliver).** Unchanged: the existing `ring_idx.is_none()` drop retires a
  pending delivery on device destroy, and `QUEUE_REGISTER_DEADLINE` still ends a session whose
  delivery can genuinely never complete.
- **The readback DMA landing after `head`** — the original (c)1 concern — is subsumed: an un-landed
  DMA reads as "not advanced," so the gate keeps waiting rather than shipping the stale bytes.

## 7. What this fix explicitly does **not** change

- `rayland-c`, `rayland-relay`, `rayland-transport`, the QUIC wire, and the forward mapped-blob relay
  (`blob_sync.rs`). All exonerated.
- The `wait_for_work_retired` fence mechanism and the engine actor.
- The structural trigger's role as the *lower bound* for when a delivery may be attempted.

## 8. Testing

- **End-to-end oracle (the primary proof).** The throwaway `RAYLAND_C1_FPLOG` correlation that pinned
  the defect is the oracle: after the fix, a stale-hunt of many two-machine runs (C = apollo, S =
  dop561) must show the `uniform = N, image = N−1` signature **gone** — no delivery ever ships a
  readback older than the resident forward inputs, and every frame 0..119 is delivered exactly once.
  This work promotes the ad-hoc scratchpad correlation into **committed test wiring** (a repeatable
  fixture-vs-native-over-the-relay comparison), which the CLAUDE.md and `icosa-fixtures.md` §8 notes
  already call for. Whether it is gated (network-only, like the existing two-machine script) is a plan
  decision; it must not run in the default `cargo test` unless it can do so hermetically.
- **Unit test (the gate logic in isolation).** Against a mock `RenderEngine` and real `HostBlob`s,
  assert that a poll shaped like a copy submit (no new S-write into the readback blob) does **not**
  complete a delivery, and a poll shaped like a draw submit (a new S-write) does — and that the
  bounded fallback delivers an unchanged readback after `B` rather than hanging.

## 9. Risk to confirm during implementation

The gate assumes the application's readback-wait is released **by the readback delivery** (so delaying
S's delivery until the readback advances genuinely delays the app's read of its own mapped readback
buffer). The 118/120 frames that already work over the network imply this holds; the end-to-end test
confirms it directly (if it did not hold, the fix would not move the stale rate). If the app were
instead released by the earlier `RingProgress`/head-advance, a second, C-side change would be needed —
the plan should keep this as an explicit checkpoint the first e2e run settles.

## 10. Instrumentation status

The `RAYLAND_C1_FPLOG` instrumentation and its correlation script are throwaway diagnostics, currently
**uncommitted**. The plan should either promote the correlation into the committed e2e oracle (§8) or
revert the instrumentation once the committed test subsumes it — the working tree must not keep dead
throwaway probes after the fix lands.
