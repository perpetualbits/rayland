# The Rayland Diary

*The story of how Rayland got built — the plans, the doubts, the wins, and the wrong turns.*

## Why this file exists

Most of what a software project records is evidence: commit logs, design specs, test results. This is
not that. This is the **story** — the reasoning as it actually unfolded, including the parts that
turned out wrong. It exists for two audiences.

If Rayland fails, this is for **whoever tries the idea again**. The dead ends here are hard-won; you
should not have to re-walk them. A negative result that is *understood* is worth more than a green test
whose reason nobody wrote down.

If Rayland succeeds, this is **accompanying material for a piece of open-source infrastructure** that
people and companies might come to depend on — and it was written by an AI working under human
supervision. That fact deserves daylight, not concealment. Trust in such software cannot be asserted;
it has to be *earned*, and part of earning it is showing the work honestly: where the machine was
confident and right, where it was confident and wrong, where a human redirected it, and how the errors
were caught. A story that only recorded the triumphs would be the least trustworthy thing we could
publish.

So the rules of this diary are: **tell it straight.** Record the uncertainty while it is still
uncertain. When something we believed turns out false, leave the belief in and mark it corrected rather
than quietly editing history. Entries written after this file was created (2026-07-20) are first-hand,
written the same day the work happened. Entries *before* that date are reconstructed faithfully from the
project's own record — the design documents, the ledgers, the code — and are narrated at the honesty
level that record supports, with no invented drama.

---

## Part I — The bet (reconstructed from the record)

Rayland starts from a contrarian reading of a settled problem. "Remote graphics" almost always means
*shipping pixels*: render on one machine, encode a video stream, decode it somewhere else. Rayland bets
the other way — **ship the commands, not the pixels.** An application runs on a weak or headless
machine (call it **C**, X11's "client," where the *program* runs); the drawing happens on the strong
machine with the good GPU and the monitor the user is actually looking at (**S**, X11's "server").
Rendering crosses the network as *language* — a stream of GPU commands — and only the final frame is
ever a picture, produced on the machine that already has to display it.

The bet has a catch, and the project has never pretended otherwise: **Wayland made remoteness hard on
purpose.** X11 was a network protocol wearing a graphics library as a coat; Wayland is the reverse, and
deliberately so — it handed rendering to the client and the GPU, which is exactly the thing that does
not travel. So Rayland is not a clever hack on top of a friendly substrate. It is a bet that the
missing pieces can be *grown*, and that the hardest of them — driving a host GPU from an untrusted,
remote party — does not have to be invented, because the virtual-machine world already built it
(Venus, virglrenderer) and hardened it against precisely this threat model.

## Part II — The walking skeleton, arc (s) (reconstructed)

The first arc did not try to be right; it tried to be *alive*. Across four sub-projects (SP0–SP3) the
team hand-rolled a small command protocol and pushed it end to end. **SP0** got a trivial triangle to
render across a plain TCP socket and land as a bit-identical PNG on S — the whole loop, proven. **SP1**
put it in a real Wayland window. **SP2** swapped TCP for QUIC. **SP3** made presentation zero-copy via
dmabuf, with a `wl_shm` fallback. None of this was the real product; the hand-rolled protocol could
never speak for arbitrary applications. It was the skeleton you build first so that everything after it
has somewhere to stand. It all works, and its tests still pass.

## Part III — The pivot, and the shock, arc (c) (reconstructed)

Then the real bet had to be paid: run **unmodified** applications. That meant retiring the hand-rolled
protocol and adopting Mesa's Venus path — the ICD that already serializes an application's Vulkan into
a command stream — and replaying it on S through virglrenderer. **C0** proved this could be
bit-identical to native, same machine, offscreen.

**(c)1 was supposed to be "just add the network." It was not.** C0's own instrumentation delivered the
project's most important early finding: the vtest socket everyone assumed carried the application's
commands carries **almost none of them**. The commands live in a **shared-memory ring** whose file
descriptor is passed once over a Unix socket; the socket after that is essentially a doorbell. *A shared
page does not survive a network, and neither does a file descriptor.* The comfortable task ("swap the
socket for QUIC") evaporated, and (c)1 became a protocol-design problem: watch the ring, relay its
deltas and the blobs the commands read, and reconstruct on S the memory the application never knew it
was sharing. That work landed the forward path — unmodified commands crossing a real network and
executing on S's GPU, bit-identical on trivial workloads — and on-screen presentation. It handed one
thing forward, unfinished: the **readback return path**, the direction where the GPU's *pixels* have to
come home.

## Part IV — (c)2, and the return path (first-hand from here)

This is where the diary catches up to itself.

(c)2 owns the genuinely hard half: **memory the application writes with no API call to intercept**
(`vkMapMemory`), and the **readback** — an application that renders and then reads the result back. Two
fixtures, `rayland-icosa-cpu` and `rayland-icosa-gpu`, were built to make the mapped-memory problem
bite. Run through the loopback path, they *did not bite* — which was itself a finding: on one machine
the shared page is real, so the uninterceptable writes simply arrive. The problem only becomes visible
where a shared page genuinely cannot exist: a true network.

And over a true network, it bit — but not where anyone was looking, and this is the part of the story
most worth telling honestly, because it is a case of the machine being **confidently wrong and then
catching itself**.

Roughly two frames in a hundred came back, over the real link, as the *whole previous frame*. A first
investigation dumped what S rendered and concluded, reasonably, that S was rendering against **stale
forward inputs** — that the application's mapped writes were arriving a frame late. It was written up.
It was committed. It was, in three separate documents, wrong.

The correction came from a discipline the project keeps relearning: **do not design a fix against an
unverified cause.** Asked to build that fix, the honest move was to first confirm the mechanism — and
the confirmation inverted it. A second, independent witness was added: not just *what S delivered*, but
*what forward inputs S already held* when it delivered, read from a value the draw consumes directly.
Across every stale frame the witness said the same thing: the forward inputs were already the **new**
frame; the *delivered pixels* were the **old** one. S was not rendering stale. Its **readback delivery**
was lagging. The single-witness dump could not tell "a stale producer" from "a stale delivery of a
fresh producer," and had guessed the wrong one. The three documents were corrected — the mistaken
reasoning left in, marked, as a lesson — and the real fix, a **readback-completion gate**, was built,
reviewed, and shipped. It took the failure rate from *most runs losing several frames* to **ten runs in
eleven perfectly clean.**

But not eleven in eleven. And the last frame in eleven is where the story currently rests, because
chasing it produced the session's second honest lesson: **a well-reviewed fix can still be wrong, and
the network is the only judge that matters.** A follow-up design — hold the signal that releases the
application until after its pixels have shipped — was specced, built, and passed two rounds of code
review including a careful one on its most delicate logic. Then it ran over the real network and made
things **worse**. Root-caused, the reason was deep and clarifying: the moment S must decide is
*ambiguous*, and it is ambiguous because the completion fence it relies on **does not reliably promise
that the pixels are actually visible when it fires** — a gap the earlier record had already named
(`T2 < T4`) and only partly closed. The fix could not distinguish "nothing to send" from "the pixels
are landing this instant," and either choice is wrong for one of the two cases. It was not merged. The
dead end was documented — thoroughly, so the next attempt starts from the understanding rather than the
idea — and the shipped ten-in-eleven gate was left standing.

That is the true state as this diary opens: a real, measured win in hand; one hard residual left; and
the residual precisely located in the fence semantics, which is the deepest part of the return path and
almost certainly where the next real progress will come from.

## Things we have learned so far

- **Wayland's difficulty is the premise, not a bug.** Every hard problem here traces back to rendering
  having been handed to the client and the GPU on purpose.
- **The wire is not where you think it is.** The commands were in shared memory, not the socket. The
  release signal was the ring head, not a feedback word. Twice, the real channel was somewhere other
  than the obvious one.
- **Pin the mechanism before designing the fix.** The most expensive error in this project so far was a
  correct-sounding cause that was never verified. The cheapest good decision was refusing to design
  against it.
- **One witness lies.** A single measurement could not separate a stale producer from a stale delivery.
  The truth needed a second, independent signal on the axis being exonerated.
- **The network is the only oracle.** Loopback hid the mapped-memory problem entirely and hid a
  regression behind its own timing; a fix that passed every local test and two code reviews still failed
  on a real link. And logging can be a Heisenbug — slowing S enough to hide the very defect being hunted.
- **Negative results are the point.** Two design directions are now known dead ends *with reasons*. That
  is not wasted work; it is the map.

## How this diary continues

From here, **each working turn adds an entry** — dated, short, in the project's own voice. Not a commit
log (git already has that) and not a status report (the design docs and `CLAUDE.md` carry the current
truth). An entry records the *thinking*: what we were unsure of, what we tried, what surprised us, what
we now believe and how confident we are. When a belief is later overturned, the entry stays and the
overturning gets its own entry. The story is allowed to be wrong in places, as long as it is never
dishonest about it.

---

## Entries

### 2026-07-20 — The diary begins

Started this file. The occasion is a natural pause: the readback-completion gate is shipped and pushed
(ten runs in eleven clean over a real network), a follow-up fix has just been proven a dead end and
documented, and the remaining residual has been pinned to the completion fence's `T2 < T4` gap. A
handoff document and a bootstrap prompt for the next session were written so the thread is not lost.

The honest feeling at this point is *earned optimism with a hard problem still open*. The core bet —
commands, not pixels; borrow the hardened GPU engine rather than reinvent it — keeps surviving contact
with reality. The forward path works over a real network. The readback path works nine-plus times in
ten and fails in a way we now understand rather than a way we don't. The thing standing between here and
"correct, not just usually-correct" is a question about what a GPU fence actually guarantees about host
memory visibility — which is a real systems question with a real answer, not a mystery.

Writing this entry is itself the small meta-moment worth marking: the project decided its story was
worth telling *before* knowing how it ends. That is either confidence or foolishness, and the diary
exists partly so a later reader can judge which.

### 2026-07-20 — Making the diary keep itself

A gap, caught by the human within minutes of the diary being created: the rule "add an entry every
turn" lived only *inside* this file, and a new session loads `CLAUDE.md`, not necessarily this. So the
diary would have quietly died the moment the session that started it ended — an irony worth recording,
since a story about honest continuity that failed to continue would have proved the opposite of its
point. Fixed by writing the obligation, and the reason for it, into `CLAUDE.md` itself, where every
future session is guaranteed to see it. Small entry, but the load-bearing one: it is what turns a
single-session artifact into a habit the project keeps. This entry exists partly to test that the habit
now holds — the first turn to follow the rule it just wrote down.

### 2026-07-20 — Reading the fence code disagrees with our own conclusion

Picked up the (c)2 residual to hunt the `T2 < T4` fence gap the handoff names as the blocker. First
confirmed the state over the real network — but the batch was *worse* than the documented ~1/11: two
runs clean, three stale (nine stale frames in five runs). That variance is itself a clue; a defect that
swings from 1-in-11 to 3-in-5 between sessions is timing- and load-sensitive, which is what a race looks
like, not a fixed logic hole.

Then I read the actual fence path in virglrenderer 1.3.0 (`vkr_ring.c`, `vkr_queue.c`) line by line, and
it points somewhere uncomfortable: **the current real-`ring_idx` fence looks like it should already
cover the readback.** The ring thread advances `head` *after* `vn_dispatch_command` returns, and
`vkr_dispatch_vkQueueSubmit` calls `vk->QueueSubmit` **synchronously, inline** (under `queue->vk_mutex`)
before it returns. So when S observes `head == applied_tail` (drained) and fences, the app's own submit
has already been enqueued on the VkQueue; the fence's empty `vkQueueSubmit` — on the *same* queue, same
mutex — is FIFO-ordered strictly after it, and its retirement should therefore imply the readback copy
in that submit has completed. If that reasoning holds, a post-fence *empty* can only be a copy submit or
an identical frame — never a draw whose DMA is still in flight — and Direction A's "empty is safe to
release" would have been *true*.

But Direction A demonstrably regressed, which says empty-is-a-pending-draw *does* happen. Two things
can't both be right. The most likely reconciliation: the `T2 < T4` evidence we lean on was measured on
2026-07-17, **before** the real-`ring_idx` fence existed — back when the fence fired on `ring_idx = 0`,
which retires immediately and waits on no GPU work at all. That measurement characterises the *old*
broken fence, not today's. So we may have carried forward a conclusion that the current code has already
outgrown, and mis-attributed a C-side release-ordering residual (the head-advance in step 1 releasing
the app before the step-2 readback lands on C) to a fence gap that no longer bites.

Two hypotheses, and I refuse to design against either until measured — this project's most expensive
mistake was exactly that. **H1 (the recorded belief):** the current fence still retires before the
readback DMA, so empty is genuinely ambiguous. **H2 (what the code reads like):** the fence covers the
readback; the residual is pure C-side release ordering. The decisive experiment is a single field:
instrument S so that, on a post-fence *empty* poll, it watches whether `res6` changes **without a new
submit crossing the ring**. H1 predicts yes (the same submit's DMA lands late); H2 predicts never (only
the next draw's copy moves `res6`). Env-gated, in-memory, dumped once at session end — because the
handoff's own hard-won lesson is that per-poll logging on S is a Heisenbug that hides this defect.
Confidence right now: ~60% H2, but that is a reading, not a measurement, and the whole point is to make
it one.

### 2026-07-20 — Measured it. I was wrong; the fence really does retire early — and now we know why

The measurement came back and refuted my own H2. It is not close: on ~**60% of every 120-frame run**, the
readback buffer changes **1.7–16 ms after** the completion fence retired, at a *constant* submit — the DMA
for a submit S had already fenced lands *after* the fence said done. `T2 < T4` is not a stale 2026-07-17
artifact; it is the common case with today's real-`ring_idx` fence. The handoff was right and my clever
FIFO reading was wrong. Good — this is exactly the failure mode the "pin the mechanism before designing"
rule exists to catch, and this time we caught it on the measurement instead of three fixes later.

The satisfying part is *why* the FIFO argument was wrong, because the answer is precise. The argument
proved the empty fence submit is *enqueued* after the app's submit B. It is. But enqueue order is not
completion order: **an empty `vkQueueSubmit(queue, 0, NULL, fence)` waits only for its own zero work, never
for prior submissions.** So it signals the instant the queue reaches the workless submit, before B's
readback copy drains. And this does not mean venus is broken for the whole world — the app's *real*
`VkFence` rides its *real* submit and waits correctly; the empty-submit `create_fence` is a separate
ring-timeline thing ordinary venus never uses for app-visible completion. We *repurposed* it as a
"readback done?" barrier, and for that it is the wrong tool. That is a clean, teachable reason, not a
shrug.

Two more things the data settled. First, the gate is doing more than the ~10/11 headline implied: it
re-polls until `res6` genuinely changes, absorbing that pervasive early-fence storm on almost every frame
— the clean runs each swallow ~70 of these silently. The stale frame is not the early fence; it is the
rare escape on the C side before the gate ships the fresh readback. Second, the Heisenbug is real and I
walked straight into it: the first probe fingerprinted 1 MiB under the applier lock ~20×/frame and
collapsed a run to 109/120 stale — the instrument inflating its own defect. Too-light a probe went blind
instead (a spinning object on a constant background hides from 64 sparse samples). ~4096 samples is the
seam that sees the frames without starving the thread. "Measure carefully" was not advice; it was the
difference between an answer and an artifact.

So the mechanism is pinned with evidence, written up in
`docs/design/2026-07-20-c2-fence-empty-submit-finding.md`. The fence needs to become a barrier that waits
for B's *completion*, which the public virglrenderer fence API does not express — so the next turn is a
real fix brainstorm across three directions (a genuine engine-level `vkQueueWaitIdle`-class barrier;
tolerating the weak fence and fixing only the C-side release by the gate's *resolution outcome* rather
than the ambiguous instantaneous empty; or a race-free content-stability signal), not another spike.
Confidence in the mechanism now: high — code path, elimination of the alternatives, and 357 consistent
events across five runs all point the same way.

### 2026-07-21 — The fix was hiding in the application's own fence

Spent the fix brainstorm first proving what *isn't* available: virglrenderer's public API has no
queue-completion barrier at all — 60 exports, and the only fence path is the empty-submit one we just
proved weak. So the "correct" fix (a real `vkQueueWaitIdle`-class barrier) would mean patching
virglrenderer, i.e. forking the engine we deliberately borrow. That felt like a dead end, and the
fallbacks were the timing-heuristic class the diary keeps burying.

Then the reachability survey turned up the answer in the opposite place. The application isn't relying on
S's proxy fence — it waits on its *own* `VkFence`, and on S that `vkWaitForFences` is dispatched
**blocking, on the ring thread** (`vkr_dispatch_vkWaitForFences`). The ring `head` only advances past that
command once the wait returns. So the moment the ring drains *past* a `vkWaitForFences` is a genuine
completion barrier — stronger than anything the fence API offers, already sitting in the stream, free. The
gate never used it: it fires a beat earlier, at the transient drain between the submit delta and the wait
delta, exactly where `res6` is still last frame. That single "a beat too early" is the whole residual.

So direction G: key the delivery on the wait-drain, read `res6` there (provably fresh or provably
unchanged — the copy-vs-draw call that was ambiguous under the weak fence is now reliable), ship the
pixels before the head-advance that releases the app. And the risky half — the wrap-safe head cap —
already exists, built and twice-reviewed on the abandoned Direction A branch; only its trigger was wrong.
That is a good feeling: not a clever new mechanism, but the realization that the correct signal was one
the system was already producing and we were reading the wrong edge of.

One honest unknown remains, and it is a code-reading question, not a mystery: whether Mesa's venus encoder
puts `vkWaitForFences` inline in the ring (where a byte-scan like `find_queue_submit` can see it) or in an
out-of-line execute stream. Submit is inline — the scan works today — so the prior is good, but the wait
must be confirmed against Mesa's `vn_ring`/`vn_cs_encoder` before building. Spec written
(`docs/design/2026-07-21-c2-waitdrain-completion.md`); that read is the first task of the plan.
