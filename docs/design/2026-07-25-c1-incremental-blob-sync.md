# (c)1 incremental blob sync — send only what changed (C→S)

*Design spec. 2026-07-25. Branch context: `wp0-wayland-proxy`, but this is a (c)1 change, independent of WP0.*

## Why this exists (the measurement)

(c)1's C→S blob synchronisation ships **the full contents of every application blob, whole, on every ring
relay** — the deliberately dumb-but-correct v1 strategy pinned in the (c)1 spec §7 and implemented in
`crates/rayland-c/src/blob_sync.rs::messages_for_delta`. For the offscreen fixtures this is trivially cheap:
`rayland-refapp` has one 64-byte vertex blob and one 16 KiB readback; `rayland-icosa-*` have one mapped blob
each. Re-shipping a few kilobytes on every relay is invisible.

The first **real** application broke it. Running `vkcube` through the relay, the C→S traffic was measured
(env-gated `RAYLAND_C1_METRICS`) at:

| stream | messages | bytes |
|---|---|---|
| ring deltas (the actual Vulkan commands) | 83 | **22,955** |
| blob sync (application memory) | 109 | **16,574,464** |

**99.9% of everything C sends is blob resends** — 720× the command traffic — and it *grows*: instrumenting
the per-relay send showed each relay shipping 264 KB, then 1.27 MB, then **2.28 MB** of blob data as vkcube
built its pipeline, and a single 2.28 MB send blocking for **2.9 seconds** over the loopback link. That is
the entire "vkcube hangs" symptom, traced end to end: the ring executes fine, C's release path is correct,
the transport is not the bottleneck — the link is simply **buried under re-sent copies of blobs that did not
change**. A real WSI application has many large blobs (staging pools, swapchain images, textures), and
re-shipping all of them whole on each of ~100 setup relays is O(total blob size × number of relays).

This is not a transport bug and not a WP0 bug. It is (c)1's own deferred debt: spec §7 chose whole-blob
resend precisely because **Venus gives no signal for which bytes changed** — an application `vkMapMemory`s
once and then writes into that pointer forever with no API call to intercept (ring-findings §5.1, "no seam to
hook"). v1 paid bytes to avoid that problem. This design pays it back.

## The idea, in one sentence

**C keeps its own copy of what it last sent S for each application blob, and on each relay ships only the
bytes that differ from that copy — nothing at all for a blob that has not changed.**

This is not a new invention. It is the exact technique S already uses successfully for the *return* path
(S→C): S holds a baseline of each blob, diffs against it, and ships only the runs it actually wrote
(`rayland-s`, the `BlobRun` machinery, (c)1 Task 5b). This spec applies the same mirror-image trick to the
forward path — which the `blob_sync.rs` module docstring has anticipated in prose since it was written:

> *"snapshot a blob's shadow the instant C applies an inbound `BlobData`, diff live bytes against that shadow
> before every relay, and ship only the bytes that changed."*

### Why "keep a copy and diff", and not a fingerprint

An earlier option was to keep only a short fingerprint (hash) of each blob and re-send the whole blob when the
fingerprint changed. It was rejected for a concrete reason: to know whether a blob changed, C must **read the
whole blob anyway** — there is no change signal to consult (that missing signal is the core problem). Given
the read happens regardless, comparing directly against the saved copy answers *both* "did it change?" *and*
"which bytes?" in that one pass, and a byte compare is if anything cheaper than hashing. A fingerprint would
be a second pass answering only the yes/no. So keeping a copy already subsumes the fingerprint for this job.

A fingerprint earns its place at a **different** job — detecting when two *different* blobs hold the same
content, or when a blob reverts to content sent earlier, so identical content never crosses twice. That is
real value, but it is the content-addressing idea that belongs to **(c)3**, and it is not needed to unblock a
real application. It is explicitly out of scope here (see "What this does not do").

## The mechanism

### The baseline

Each **application** blob (`LocalBlob::is_application_memory()`) carries a **baseline**: C's copy of the bytes
S currently holds for that blob. It lives on the `LocalBlob` (`crates/rayland-c/src/shm.rs`), alongside the
memfd mapping it shadows, as a `Vec<u8>` the size of the blob. Only application blobs need one — Venus's own
shmems (the ring, the reply arena, the staging pool) are never shipped whole C→S (the ring crosses as deltas,
not blobs), so they carry no baseline and cost no extra memory.

The baseline is **initialised to zeros** at blob creation, because S's copy is a fresh, zero-filled memfd at
that moment too. So the first diff (live-vs-zeros) naturally ships exactly the application's initial
non-zero content and nothing more, and C's and S's copies agree from the first relay onward.

Memory cost: the baseline doubles the resident size of application blob memory on C. For vkcube's ~16 MB of
application blobs that is +16 MB — trivial even for the weak machine, and exactly the cost S already pays for
the symmetric return-path baseline.

### The diff, on each relay

`messages_for_delta` (`crates/rayland-c/src/blob_sync.rs`), for each application blob, walks the live mapping
and the baseline in lockstep in a **single pass**:

- Where the bytes match, nothing is emitted and the baseline is already correct.
- Where they differ, a **changed run** is opened; C copies the live bytes into a `C2S::BlobData { res_id,
  offset, bytes }` (the run's start offset and its bytes) **and** into the baseline at the same position, so
  the baseline now equals the live bytes as read in this pass. The run closes when the bytes match again.

A blob with no differences emits **zero** messages. A write-once blob emits **one** run (the whole content,
once). A partially-updated blob emits a run per changed region.

**No wire change is needed.** `C2S::BlobData` already carries `offset`; v1 only ever used `offset: 0` with the
whole blob. Shipping a changed run is the same message with the run's real offset and length. Several runs are
several `BlobData` messages, ordered ahead of the ring delta exactly as the whole-blob messages were.

Doing the diff and the baseline update in one pass, from a single read of the live bytes, is not just an
optimisation — it is what keeps C and S in sync. If C diffed against the live memory at one instant and then
copied the live memory into the baseline at a later instant, the application (which writes with no
interceptable call) could change bytes in between, and C's new baseline would no longer equal what it sent.
Reading once and using those same bytes for both the run and the baseline closes that gap. (The application
writing *during* the single pass is the same inherent raciness v1's whole-blob read already has — see
"Correctness".)

### The re-baseline — the one subtle interaction

When S writes a blob and sends those bytes back (the S→C return path — readback buffers, and the reply arena),
C's reader applies them into C's mapping (`apply_blob_data`, `crates/rayland-c/src/main.rs`). At that moment
**C must also fold those bytes into the blob's baseline.**

Without this, the next C→S diff would see C's live memory (now carrying S's writes, applied by C's reader)
differ from C's stale baseline, and C would ship **S's own bytes back to S** — a last-writer-wins wobble, and
exactly the mistake (c)1 Task 5b caught and fixed in the *other* direction (S must re-baseline on C's forward
writes so it never ships back what C wrote). This is the symmetric requirement, and it is the "snapshot the
shadow the instant C applies an inbound `BlobData`" the module docstring named. Concretely: `apply_blob_data`
writes S's bytes into the mapping and copies the same bytes into the baseline at the same offset.

### Trigger and ordering — unchanged from v1

- **Trigger:** the relay event, exactly as today — "C is about to ship ring bytes to S". It stays deliberately
  over-eager (it checks every application blob on every relay, even relays with no submit), because the only
  boundary C can observe is its own relay, and `vkQueueSubmit` is invisible inside the ring. Over-eagerness is
  now cheap: an unchanged blob costs one compare pass and ships nothing.
- **Ordering:** every `BlobData` (each changed run) is still returned **before** the `RingDelta` in the message
  list, because S's ring thread dispatches the instant `tail` moves and any delta must be assumed to read any
  application blob. The ordering guarantee `blob_sync.rs` exists to make is preserved byte-for-byte.

## Correctness

The invariant is: **after C ships a relay's messages and S applies them, C's baseline for each application
blob equals S's copy of that blob equals the application's bytes as C read them this relay.**

- C→S: S starts each blob as zeros; C's baseline starts as zeros; every changed run C ships moves S's copy to
  match C's new baseline. By induction the two copies stay equal after every relay.
- S→C: when S's own writes arrive, C applies them to the mapping *and* the baseline together, so the invariant
  holds across the return path too, and C never re-ships S's bytes.

The design does **not** make the concurrent-write raciness better or worse. The application can write to a
mapped blob at any instant with no call to intercept, so both v1's whole-blob read and this design's diff pass
can read a blob mid-update and ship a torn intermediate. That torn state is transient and self-correcting: the
next relay's diff catches whatever changed after, and S converges. This is inherent to remote `vkMapMemory`
and is (c)2's problem to solve properly, not this spec's — see below.

## What this does *not* do (scope)

- **It does not solve remote `vkMapMemory` (that is (c)2).** A blob the application genuinely rewrites every
  frame — `rayland-icosa-cpu` writes a megabyte of CPU-computed fractal into mapped memory each frame — still
  ships (nearly) a megabyte each frame, because (nearly) every byte really did change. This design removes the
  *re-shipping of unchanged* blobs; it does nothing for blobs that change wholesale, and it must not pretend
  to. The icosa fixtures remain (c)2's evidence.
- **It does not deduplicate across blobs or across time.** Identical content in two blobs, or a blob reverting
  to earlier content, still crosses again. That is the content-addressing / fingerprint work, and it is
  **(c)3**.
- **It does not decode the ring to decide which blobs a delta reads.** Shipping only the blobs a submit
  actually touches would cut still more, but reading the ring to make a correctness decision is exactly what
  (c)1 spec §7 forbids — a decode bug there becomes a silent corruption bug. The trigger stays the opaque
  relay event.

## The fragmentation caveat, and the metric

Byte-granular diffs **fragment**: where changed bytes are interleaved with unchanged ones, the run count
climbs. (c)1 Task 5b/Task 9 already met this on the return path — a flat-colour readback against a zero
baseline fragments into thousands of tiny runs. The forward path inherits the same property. Two consequences,
both carried over deliberately:

- **Do not coalesce across unchanged gaps.** Merging two runs across the unchanged bytes between them would
  re-ship exactly the bytes the diff exists to skip — "the attractive fix is the hole again" (Task 5b). Ship
  the exact runs.
- **Report message count and bytes separately.** The win here is overwhelmingly in *bytes*; a fragmented blob
  can trade fewer bytes for many messages. The metrics must keep the two columns distinct (they already do),
  so a future reader is never fooled into reading a bytes win as a regression in message count.

For the case that motivates this spec — vkcube's setup blobs, which are written once and then read — there is
no fragmentation: each ships as one run on the relay after it is written, then nothing. The expected result is
that C→S blob traffic drops from ~16.5 MB of re-sends to approximately the one-time size of the application's
actual content, and the per-relay send stops growing without bound.

## Where the pieces live

| piece | file | change |
|---|---|---|
| the baseline (per app blob) + the diff-and-rebaseline pass | `crates/rayland-c/src/shm.rs` (`LocalBlob`) | add a `baseline: Vec<u8>`; add a method that returns the changed runs vs the baseline and updates it in one pass |
| emit changed runs, skip unchanged, preserve ordering | `crates/rayland-c/src/blob_sync.rs` (`messages_for_delta`) | replace the whole-blob push with a per-run push driven by the new method |
| fold S's inbound writes into the baseline | `crates/rayland-c/src/main.rs` (`apply_blob_data`) | after writing S's bytes into the mapping, copy them into the baseline at the same offset |

The `C2S::BlobData` wire type is unchanged. The `RingDelta` path is unchanged. The trigger and the
BlobData-before-RingDelta ordering are unchanged.

## Testing

- **Unit (`blob_sync.rs`):** an unchanged blob emits zero `BlobData`; a blob changed in one region emits one
  `BlobData` for exactly that run; two disjoint changed regions emit two; a first-ever ship (baseline zeros)
  emits the application's non-zero content and only that. Ordering (every `BlobData` before the `RingDelta`)
  still holds. The existing whole-blob tests are rewritten to the diff contract.
- **Unit (re-baseline):** after `apply_blob_data` folds an inbound `BlobData` into the baseline, the next
  C→S diff of an otherwise-untouched blob emits nothing — C does not ship S's bytes back. Teeth-check by
  omitting the fold and confirming the test then sees S's bytes re-shipped.
- **Regression (the gate):** the (c)1/(c)2 loopback e2e (`rayland-s/tests/loopback_e2e.rs`, refapp and icosa)
  stays **bit-identical**. The diff must lose no byte; a dropped run would surface as a wrong pixel here.
- **Bandwidth proof:** a run (vkcube, or a synthetic app that creates a large blob and then only reads it)
  shows C→S blob-sync **bytes** fall by orders of magnitude versus the whole-blob baseline, with the command
  bytes unchanged — the metric that states the whole point.

## Expected outcome

vkcube's C→S blob traffic drops from ~16.5 MB (re-shipping unchanged blobs ~100 times) to roughly the
one-time size of its actual blob content. The per-relay send stops growing, the multi-second send stalls and
the `RingBarrier` timeouts they cause disappear, and the relay is no longer buried under re-sends — which is
what stands between the (correct) WP0 tunnel and a live vkcube. Remote `vkMapMemory` (blobs that truly change
every frame) and cross-time deduplication remain, by design, (c)2 and (c)3.
