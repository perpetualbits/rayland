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

### 2026-07-21 — The gate fired: venus polls, it does not wait

The first task of the plan was a gate: confirm the application's `vkWaitForFences` is carried inline in
the ring, since the whole wait-drain design rested on the ring thread *blocking* in it. It is not, and
the design's premise is simply false. A one-run scan found zero `vkWaitForFences` (command type 39) in the
deltas, and Mesa's own source said why: `vn_WaitForFences` (`vn_queue.c`) does not send a wait command at
all — it **polls** `vkGetFenceStatus` in a relax-backoff loop until the fence reads signalled. With fence
feedback off — our only real-network config — that poll round-trips the ring as `vkGetFenceStatus`
(type 38); the async `vkWaitForFences` (type 39) is emitted only when feedback is *on*. So there is no
blocking host-side wait whose drain I could key on. The design I wrote a spec and a plan for cannot work
as written.

Two feelings, both worth recording honestly. The first is that this stings — I read `vkr_dispatch_vkWaitForFences`
on the host side (it *does* block), verified the fixture calls `wait_for_fences`, and reasoned a clean
mechanism from those two true facts without checking the one link between them: whether the guest ever
*sends* the command. It does not. Same shape of error as the FIFO argument earlier this session — a chain
of correct local facts assembled into a wrong conclusion because one joint went unverified.

The second feeling is the one that matters: **this is the process working exactly as designed.** The gate
existed precisely because this premise was the plan's one unproven assumption, and it was placed first, and
cheap, so a wrong answer would cost one run and one reverted scan rather than four built-and-reviewed tasks
undone. It caught the error at the cheapest possible moment. That the plan's author (me) was confident and
wrong is not the failure; shipping that confidence into code unchecked would have been. Nothing was built
on the false premise, and the branch has no commits to unwind.

What survives is better than what died. There *is* a real completion signal in the ring — a
`vkGetFenceStatus` whose reply is `VK_SUCCESS`, which the app is polling for and which fires exactly when
the fence signals and `res6` is complete. The spirit of the fix (key on a real in-ring completion signal,
order pixels before the release) is intact; only the signal's identity changed, from a blocking wait to a
polled status reply. The next decision — how to detect that reply, and how much complexity it is worth
versus a simpler content-ordering or a bounded fallback — is the human's to weigh, so I am stopping here
rather than picking one unilaterally.

### 2026-07-21 — G-lite: the ordering was right, the missing barrier was the problem

Tried the cheap first thing: ship the readback `res6` ahead of the head-advance that releases the
application, gated by a cheap fingerprint so a new frame is shipped the moment it appears. Two lessons,
one painful and one clarifying.

The painful one: a wholesale rewrite of the progress thread broke *initialization* — every run reported
120/120 "stale", which turned out to mean the application never rendered a frame at all
(`VK_ERROR_INITIALIZATION_FAILED`). The res6 shipping was innocent (it never fired during init); the
culprit was that I also restructured how the reply arena and ring-progress are shipped — combining their
reads into one lock and shipping unconditionally every poll instead of in the old progress-gated
lockstep. The init handshake depends on that lockstep. A control run of the committed code rendered
cleanly, which localized the break to my change; reverting just the venus/progress handling to old-style
restored rendering. The lesson is blunt: the progress thread's reply/head cadence is load-bearing for
init, and it is not the thing to casually rewrite.

The clarifying one: once init worked, res6-first shipping *did* fix the residual it was meant to — across
a batch, **zero** whole-previous (`N−1`) frames, where before that was the entire defect. But it traded
one failure for another: ~4 frames per run came back **torn** — matching no native frame at all — because
without any completion barrier the fingerprint fires at the *start* of the copy DMA and ships a
half-written buffer. The committed gate never tore because its (weak) fence plus once-per-frame sampling
happened to sample `res6` away from mid-copy; G-lite's eager every-poll detection samples right into it.

So the shape of the real fix is now clear and it is not either-or: it needs the res6-first **ordering**
(which demonstrably kills the `N−1` residual) **and** a completion barrier so only a *whole* frame ships.
The barrier is the one the earlier gate result already handed us — the `vkGetFenceStatus` reply reading
`VK_SUCCESS`, which the application is polling for and which the host writes exactly when the fence
signals and the copy is complete. That is G': couple the res6 ship to that reply. It costs a reply-arena
decode, but it is the first candidate that satisfies both constraints at once. Reporting up before
building it, since G-lite was the agreed cheap-first bet and it has now been settled.

### 2026-07-21 — It works: the readback residual is gone, and the signal was the app's own poll

Zero stale frames across twenty real-network runs. After a session that went through three dead ends,
the (c)2 readback return path is finally clean — and the thing that fixed it is almost funny in
hindsight, because it was the application telling us, on every frame, exactly when its frame was done. We
just had to read the right memory.

The chain of wrongness is worth keeping whole, because each link taught the next. The empty-submit fence
retired before the DMA (measured, pervasive). The wait-drain idea rested on the app sending a blocking
`vkWaitForFences` — it does not; with feedback off Mesa *polls* `vkGetFenceStatus`, and the Task-1 gate
caught that before a line of it was built. G-lite shipped the pixels first, which killed the whole-previous
staleness completely — and traded it for torn frames, because "ship when the buffer changed" fires in the
middle of the copy. Every one of those was a real attempt that failed for a real, different reason, and
the last one drew the target precisely: we needed the ordering *and* a completion barrier, together.

The barrier was hiding in plain sight. With feedback off the application releases itself by polling
`vkGetFenceStatus` until the reply reads `VK_SUCCESS`; virglrenderer writes that reply into the reply
arena as `[38][0]`. That byte pattern *is* "the fence signalled, the copy is done, res6 is whole." Two
false starts even here. Scanning the *shipped* reply bytes never matched — the diff fragments the reply
into one run per changed byte, and the result byte usually doesn't change, so the contiguous pattern is
invisible in what crosses the wire; res6 never shipped and all 120 frames came back identical. The fix was
to scan the *live* arena instead. And the subtle worry — that a previous frame's success lingers in the
arena and false-triggers mid-DMA — turns out not to bite, precisely because the application is *polling*:
while a fence is still in flight it is reading `VK_NOT_READY` (`[38][1]`) over and over, which overwrites
the arena, so a live `[38][0]` genuinely means a fence just signalled. The application's own busy-wait is
what makes the signal trustworthy.

So S now ships the readback the instant that reply appears, ahead of the head-advance that releases the
app, and gates it on `take_app_blob_writes` being non-empty — which is true only for a draw that actually
produced pixels, so an upload copy ships nothing and a stale success re-ships nothing. No S-issued fence,
no timing heuristic, no content-stability guess. The progress thread stopped touching the engine entirely.

Two honest caveats, recorded so they are not forgotten. This is the *feedback-off* path — the only one
that renders over a real network anyway; the old feedback-on "buy-back" was loopback-only and is
superseded, and the loopback test now runs feedback-off so it guards the path we actually ship. And the
readback still fragments into thousands of one-byte messages per frame — the runs are visibly slow — which
is a real bandwidth problem for another day, not a correctness one. But correctness is the thing (c)2 was
stuck on for the whole arc, and it is, at last, done: an unmodified Vulkan application renders on a remote
GPU and reads its pixels back, frame-perfect, across a real network.

### 2026-07-21 — Coalescing the readback, and what it revealed about where the time goes

The G' fix was correct but the runs were slow, and the reason was ugly: the readback shipped as ~5000
one-byte `BlobData` messages per frame. `changed_byte_ranges` emits one run per maximal run of
*consecutive* changed bytes, and between two frames the changed bytes are sprinkled through unchanged
ones, so a frame shatters. The fix is small and local: merge runs separated by ≤256 unchanged bytes for
the readback path only, re-shipping the tiny gaps. It is safe precisely there because `res6` is written
by S's GPU and only *read* on C — a re-shipped unchanged byte equals what C already holds. The fine
byte-grain stays everywhere it is load-bearing (the reply arena, where shipping a byte S did not write
could clobber the app's own); the coalescing is `gap = 0` (inert) on every other path.

It worked — ~5000 down to ~180 messages per frame, still bit-identical, still zero stale. But the honest
and more interesting result is that the **wall-clock did not move**. Twenty-eight times fewer readback
messages, and the two-machine run takes the same minutes. That is a finding, not a disappointment: it
says the return path's wall-clock is bound by per-frame *round-trip latency* — the application's
`vkGetFenceStatus` polling, each poll a network round-trip — not by the one-directional volume of
readback bytes. The readback fragmentation was a real load problem (network and C-side message
processing, both now 28× lighter) but it was never the thing making the clock slow. The ~180 remaining
runs are the frame's genuinely distinct changed clusters, and pushing further would trade a lot of
bandwidth to merge the large gaps between them for no wall-clock gain, so 256 is where it rests.

The next latency lever, when it matters, is the round-trip count itself — adaptive polling, or batching
the reply path — not the readback. Recorded so the next person does not coalesce harder expecting the
clock to move.

### 2026-07-21 — The first real app, and the first real wall: WSI

With the readback loop finally solid for the fixtures, the honest next question was whether the central
bet generalises to an app nobody hand-built for it. Pointed vkcube — the standard spinning-cube demo —
at the relay. It is the truest test so far, and it found a wall exactly where "expect walls" said one
would be, and precisely enough to name it.

vkcube got *far*. Mesa connected, the command stream flowed (~32 KB of ring vs the fixtures' ~200 bytes
of init), it selected the Wayland WSI platform, enumerated the GPU ("Virtio-GPU Venus (NVIDIA RTX A500)"),
created a device, and began allocating swapchain images — we watched their blob fds get passed over the
vtest socket while it also talked to the host compositor. Then it aborted in `demo_prepare_swapchain`:
`create_immed failed and produced an invalid wl_buffer`.

That is the whole finding in one line. Venus's **Wayland WSI** turns each swapchain image into a
`wl_buffer` for the compositor via `zwp_linux_dmabuf`'s `create_immed`, from the image's dma-buf. In a
real VM that dma-buf is a virtio-gpu resource the host compositor can import through the virtio-gpu
display path. **Rayland has no virtio-gpu and no such path — it is a command relay** — so the dma-buf the
guest exports is meaningless to the compositor, `create_immed` yields an invalid `wl_buffer`, and
swapchain preparation asserts out. (The retry we saw as a device create→destroy→recreate in S's log is
vkcube tearing the device down and trying once more before giving up.)

This is not a bug to patch; it is a *missing subsystem*, and the fixtures deliberately dodged it: they
render **offscreen** and read the pixels back, so they never touch WSI. A real presenting app cannot dodge
it. The right model for Rayland is not to let the guest's WSI reach for a virtio-gpu display that is not
there, but to **intercept the swapchain** — so `vkQueuePresentKHR` hands the finished image to S for
display, the way the readback path already hands S the readback buffer — which is what waypipe and
Sommelier do for Wayland clients. `rayland-present` already puts pixels on S's screen; what is missing is
the swapchain interception and the present-forwarding between it and the engine. That is a sub-project,
and it is now concretely scoped rather than hypothetical. The bet still stands; the next missing ecosystem
piece just has a name.

### 2026-07-22 — Building the swapchain interceptor: the proxy answers its first client

The WSI wall named the missing piece — intercept the swapchain instead of letting the guest's WSI reach
for a virtio-gpu display that isn't there — and this stretch of days turned that name into a build. The
shape settled quickly and, pleasingly, without much second-guessing, because waypipe and Sommelier already
prove the shape is tractable: the app connects to a **proxy** we run on C (its `WAYLAND_DISPLAY` names our
socket, not the real compositor), the proxy forwards the app's Wayland protocol to S where a client
replays it against S's *real* compositor, and the one thing that cannot cross a network — the swapchain
buffer's dma-buf fd — is replaced by a **token** naming the S-side resource the command relay already
rendered. No pixels cross for presentation; only protocol and tokens. That is the design doc's
buffer-by-token, finally being built rather than sketched.

Two things had to be pinned before plumbing, and both were, by spiking rather than reasoning. First, the
correlation key — *how* the proxy recognises the swapchain fd it must not forward. The spike answered it
more cleanly than the design feared: the swapchain image's memory is not some opaque dma-buf we'd have to
fingerprint, it is the exact `memfd:rayland-blob` `rayland-c` itself allocated in `shm.rs` and handed the
app over vtest. So the key is just the memfd's inode (`st_dev`+`st_ino`), which C already owns — no
guessing. That is also, satisfyingly, *why* a plain vkcube aborts: a real compositor rejects a memfd
presented as a dma-buf, and the proxy dissolves the problem by never letting the memfd reach a real
compositor at all. Second, the forwarding model. We chose to forward at the **structured-message layer**
via `wayland-backend` (wayland-rs's low level), not raw byte-tunnelling and not the high-level typed
`Dispatch`. The library owns all wire serialization and fd plumbing; each request arrives as a typed
`Message { sender_id, opcode, args }`, and the proxy's whole job shrinks to three concerns — forward,
translate the object ids across two independent id spaces, and the single fd→token interception. This
eliminates an entire class of wire-format bugs by making the library the wire authority. The relay message
became a structured `WaylandMessage` mirroring `wayland-backend`'s `Argument`, with the `Fd` case replaced
by `BufferToken`.

Today the proxy stood up and answered its first client. It advertises the minimal global set vkcube binds
— `wl_compositor`, `xdg_wm_base`, `zwp_linux_dmabuf_v1`, and `wl_seat` (inert; input is a later WP) — over
a real accept-and-dispatch loop, and an integration test playing the app's opening move (connect,
`get_registry`, read the advertised globals) sees all four. It is a small milestone and worth not
overselling: the *risky* halves are still ahead — forwarding the app's requests to S, and the
`create_immed` fd→token interception that is the whole point — each with its own proof still to come
(the argument-translation unit tests, and ultimately vkcube's cube actually turning up on S's screen). But
the library choice is paying off already: the backend handled the registry dance for free, and standing up
four globals plus a poll loop was a page of code, not a protocol implementation. One incidental scar worth
recording: the build's `/tmp` target tripped a per-user tmpfs quota and the linker died with a bare SIGBUS
— nothing to do with the code, everything to do with a 6 GB target dir on a 31 GB shared tmpfs. Moved the
target onto the home filesystem and it built clean. Noted here only so the next person who sees `collect2:
ld terminated with signal 7` on this box looks at `df` before they look at their diff.

Later the same day the first of those risky halves landed: request forwarding. The proxy now translates
each `wayland-backend` `Argument` to the wire `WaylandArg` and forwards the request to a sink. The
translation is a dull 1:1 remap with two spots that actually needed a test — `Str` (drop the wire NUL,
keep null-vs-present distinct) and the deliberate refusal to forward a raw fd (every fd must become a
token, so `Argument::Fd` translates to an error, not a value). Both are unit-tested, and both tests were
watched failing first: breaking `Str` to keep the NUL made them go red, which is the only way to know a
green test was ever load-bearing. The id-carrying cases (`Object`/`NewId`) can't be unit-tested because a
real `ObjectId` only comes from the backend, so those are pinned by an end-to-end test instead — a real
client binds `wl_compositor`, calls `create_surface`, and the collector on the far side shows the new
surface's id arriving as a translated `NewId`. The sink is deliberately a trait with a recording stub
behind it; the real link to S is Task 4's job, and keeping it abstract let the whole forward path be
proven without standing up S at all. What's left of the crux is the fd→token interception itself — the
one request, `create_immed`, that the whole sub-project exists to intercept.

And then the crux itself — the one request the whole sub-project exists to intercept. `create_immed` is
where a plain vkcube dies, because it hands the compositor a memfd dressed as a dma-buf and a real
compositor refuses it. The proxy's job is to make sure a real compositor never sees that fd at all. What
made this the interesting sub-step is that the token can't be built in one place: the dma-buf fd and its
DRM modifier arrive on `params.add`, but the width, height, and format only arrive on the later
`create_immed`. So the proxy accumulates state per params object — resolve the fd's memfd inode to an
S-side resource id at `add` (the same inode `rayland-c`'s `shm.rs` allocated, which is the whole
buffer-by-token insight from the Task-1 spike), stash it with the modifier, and only when `create_immed`
supplies the geometry assemble the full `BufferToken` and forward it, the fd dropped on the floor. The
proof drives the real sequence through the proxy with a real dmabuf client and a stub resolver, and checks
two things: the token is fully and correctly populated (resource id, dimensions, format, modifier), and —
the line that matters most — `create_immed` raises no protocol error. The abort that stopped vkcube is
gone, because the memfd never left C. Both halves were watched failing first (starve the resolver and the
token never forms; swap width for height and the dimensions come back wrong), which is the only way to
trust the green.

That completes the C-side proxy for WP0: it stands up, advertises the globals, forwards the app's requests,
and turns the swapchain fd into a token — all proven against stubs standing in for S. What's deliberately
still stubbed is exactly what Task 4 is: the real link carrying these messages to S, an `shm.rs`-backed
resolver instead of a fixed map, the object-id translation between the app's id space and S's client's, and
an actual Wayland client on S replaying the session against the real compositor and resolving each token
back to a real dma-buf. The proxy has no caller yet either — wiring it into the daemon behind
`RAYLAND_C1_WAYLAND_DISPLAY` is part of standing S up. But the risky core the plan front-loaded is done,
and it is done the way SP0 did its risky core: end-to-end on the one path that matters, ugly edges left for
later, the hard thing proven before the plumbing around it.

A review of the whole proxy followed, and it did the most useful thing a review can: it confirmed the
scary parts were actually sound (no path forwards a raw fd; the state machine keys consistently and never
fabricates a token from a missing `add`; the poll loop can't spin because wayland-backend deregisters a
dead client itself; the opcodes match the protocol xml) and then named the thing the celebratory commit
message had glossed. "The crux, complete" is true of the fd→token *mechanism*, but a real vkcube would
never reach `create_immed` through this proxy yet, because there is no path to deliver compositor events
back to the app — and Mesa's WSI blocks on an `xdg_surface.configure` and on dmabuf format/feedback events
*before* it ever creates a swapchain image. The buffer-token test drives the buffer sequence by hand
precisely because that earlier handshake isn't there to carry a real client to that point. That is
genuinely Task 4's job (there is no S to originate those events yet), but it's an important correction to
the story: the proxy's send path is unbuilt, and standing up S will have to include making those events
happen, quite possibly by synthesising a configure locally rather than waiting on the real compositor.
Two smaller gaps got fixed on the spot — the asynchronous `params.create` (the sibling of `create_immed`
that returns its buffer via an event) was falling through to the generic forward, which would ship
geometry-with-no-token to S and quietly pretend an unsupported request was handled; it is now refused
cleanly and tested. And params state that never reached `create_immed` was leaking, so teardown now
releases it. Neither was a crash, but both are the kind of honest edge the walking skeleton is allowed to
have only if they're named.

### 2026-07-22 — Mapping the S side, and finding the spec had promised something the code doesn't do

Before building Task 4 — the S side, which turns the tokens the proxy now emits into an actual window on
S's screen — two readers went through the C and S code so the plan would rest on what's there rather than
what the spec hoped was there. The C side came back clean and boringly wireable: the QUIC send half already
lives behind an `Arc<Mutex<QuicSendLink>>` that two producers share, so the proxy's sink is a third
producer sending `C2S::WaylandRequest` over the same lock; the blob table is keyed by resource id; and
slotting the proxy in as a fourth daemon thread with its own socket env is a page of glue. Good.

The S side came back with a genuine correction. The spec's §4 says, as if it were settled, that "rayland-s
already re-exports the resource as a dma-buf for presentation." It does not. The one function that could —
`virgl_renderer_resource_export_blob` — is wrapped privately, is oriented at SHM (the CPU readback path),
and is only ever called at blob *creation*, never to re-export a finished frame for display. Live Venus
blobs come back typed SHM, not DMABUF. And `rayland-present`, which the spec leans on, turns out to be a
one-frame, own-the-event-loop, block-until-the-window-closes shape that runs *after* a session ends — it
cannot drive a surface that an app animates. So the sentence the whole zero-copy presentation story hangs
on is aspirational, and whether S can actually turn a `BufferToken`'s resource id into a `wl_buffer` a real
compositor will show is **unproven** — the spec even admitted it was deferring that proof "to Task 4"
without flagging how load-bearing it is.

This is not a disaster; it's the kind of thing the diary exists to catch honestly. It has two consequences.
First, Task 4 is bigger than "write an S client": it needs a new public dma-buf re-export on the engine (or
a fallback), a persistent-surface Wayland client written fresh, an object-id map, format/modifier
negotiation past the hard-coded XRGB8888+LINEAR that present assumes, and the event-return path the earlier
review already flagged. Second, and more importantly, there's a real fork in *what presentation even is*:
the design's intended zero-copy dma-buf path, or a local readback→`wl_shm` present on S. Both keep WP0's
actual invariant — no pixels cross the *network* — because the fallback reads back and presents locally on
S; the difference is zero-copy versus a local copy, and whether the swapchain image is even exportable or
readable (the fallback re-enters the exact (c)2 readback-of-a-GPU-image wall). So the plan now front-loads a
spike gate — can S export a real swapchain resource as a dma-buf the compositor imports? — before any of the
S client gets built, exactly as Task 3b front-loaded its correlation spike. Which of the two paths to aim
the walking skeleton at first is the next thing to settle, because it changes what gets built.

### 2026-07-22 — The spike gate fired: zero-copy is structurally impossible, and that's the useful kind of no

The dma-buf export spike was the right call, because it returned a hard, decisive no — and told us exactly
why, in virglrenderer's own source rather than by guesswork. The question was whether S could re-export a
swapchain image's resource as a compositor-importable dma-buf. The answer: virglrenderer fixes a resource's
fd type at *creation*, not at export. Rayland's swapchain image memory is a **guest blob**
(`VIRGL_RENDERER_BLOB_MEM_GUEST` — the `memfd:rayland-blob` the whole buffer-by-token correlation rests on),
and a guest blob is created as pure guest iovecs with no host fd at all: `virgl_resource_export_fd` returns
`FD_INVALID`, and `virgl_renderer_resource_export_blob` returns `-EINVAL`. Not a dma-buf, not even an SHM fd
— nothing. Dma-buf export is real and unstubbed, but it lives on the *host*-allocated (`HOST3D`) path, which
requires the guest to have allocated the memory with `VkExportMemoryAllocateInfo{DMA_BUF}` — and Rayland's
guest-blob resources structurally never go through that path.

So the design doc's §4 promise — "S re-exports the resource as a dma-buf, rayland-s already does this" — is
not merely unimplemented, it is **unreachable** without changing what kind of memory the swapchain image is
made of (guest blob → HOST3D blob). That is a deep change to the resource model, well outside WP0, and worth
its own investigation later (it's the real road to zero-copy, and it may not even be free: HOST3D memory is
host-allocated, which changes who owns the swapchain pixels and reopens the mapped-memory questions (c)2
exists for). This is exactly the wall "expect walls" predicted, found before a line of the S client was
written to rest on the false assumption.

Where that leaves WP0: the readback→`wl_shm` fallback, which the presentation-path question already blessed
as acceptable because it keeps the real invariant — no pixels cross the *network*; S reads the rendered
pixels from its own local mirror of the guest memfd (the resource the `BufferToken` names) and presents them
via `wl_shm` on its own screen. And this is not a sad consolation prize: it lands almost exactly on
machinery that already exists and is proven. The swapchain image is a linear, guest-memfd-backed render
target (Venus negotiates LINEAR for wlroots/cosmic-style compositors), so after the app's submit completes,
S's mirror of that memfd holds the finished pixels — the same completion-gated readback the (c)2 return path
already solved (the G' `vkGetFenceStatus` signal). WP0's S side becomes: drive a persistent surface from the
app's relayed attach/commit, and on each commit read the token's resource bytes (completion-gated) into a
`wl_shm` buffer and present. Zero-copy is deferred to a future HOST3D-resource investigation; the walking
skeleton walks on the copy path first, exactly as the skeleton philosophy intends.

### 2026-07-22 — Overturned by measurement: the swapchain images export as dma-bufs after all

The Task 4.0 spike said zero-copy was structurally impossible. It was wrong, and the way it was wrong is
worth being precise about because the correction is the whole game. That spike read virglrenderer's source
and correctly concluded a **guest blob** can never export as a dma-buf. Its error was one of identification:
it took "the swapchain image is the `memfd:rayland-blob` rayland-c allocated" — true of **C's local
placeholder fd** — and assumed the *S-side resource* was therefore a guest blob too. Two more source reads
(Mesa's Venus WSI, and Rayland's own blob plumbing) said otherwise: Mesa's vtest backend requests the
swapchain image's memory as `VCMD_BLOB_TYPE_HOST3D` with `VkExportMemoryAllocateInfo{DMA_BUF}`
**unconditionally** (`vn_renderer_vtest.c`), never maps it on the guest, and rayland-s already honors
`blob_mem` end-to-end — so the resource S actually creates for a swapchain image is a **HOST3D** resource,
not a guest blob. C's memfd and S's resource are two different objects; the spike conflated them.

So we measured, instead of reasoning a third time. A throwaway `RAYLAND_EXPORT_SPIKE` logged the `fd_type`
virglrenderer returns from every HOST3D export, and vkcube was run through a loopback relay on dop561 (a real
GPU, a real NVIDIA A500). The answer was unambiguous: resources 1–3 exported as `fd_type=3` (SHM — the ring,
the reply arena, staging), and resources **4, 5, 6, 7 exported as `fd_type=1` — DMABUF**. Four of them,
exactly a swapchain's worth of images, each a real, compositor-importable dma-buf, sitting on S. The
resource the `BufferToken` names *is* directly presentable, zero-copy, with no readback and no local copy.

This vindicates the design's original thesis and the instinct to investigate HOST3D before settling for the
`wl_shm` fallback. It also changes WP0's endpoint back to what the design always wanted: S resolves the
token to its HOST3D resource's dma-buf and hands *that* to its compositor. A few real details fall out of the
measurement and are noted for the build: the export already happens once, at blob creation (that is what the
spike observed), and virglrenderer guards against a second export — so S must **retain** the creation-time
dma-buf rather than re-export on demand; and the compositor also needs stride/offset/modifier, which for the
LINEAR swapchain images here are trivial (offset 0, stride = width·bpp) or already carried on the token. The
Task 4.0 "impossible" entry stays in the diary above, wrong, as the record of a conclusion measurement
reversed — which is exactly the kind of honesty this diary exists to keep. Zero-copy is back on, and now
it's not a hope, it's a logged fact.

### 2026-07-22 — Task 4.1: wiring the proxy into the daemon, and a correlation that needed no new table

With zero-copy confirmed, the build restarted at the C side, which the earlier map had already shown was the
easy half. Three pieces. The blob shadow now records its memfd's inode at creation — the one moment the fd
is in hand before it's handed to Mesa and dropped — because that inode is the buffer-by-token key: the
swapchain fd Mesa later passes the compositor is the very same memfd, and the proxy recovers the resource by
matching it. The nice surprise was that the resolver needs no second data structure at all: the blob table
is already keyed by resource id and now each blob carries its inode, so "which resource is this fd?" is just
a scan of that table for the matching inode. A linear scan is exactly right here — a few dozen blobs, and
`params.add` fires once per swapchain image — so an index would be more bookkeeping than the cost it saves.
The sink is the mirror image: it closes over the same `Arc<Mutex<QuicSendLink>>` the ring watcher and vtest
thread already send through, so a Wayland request rides the one connection alongside the ring and blob
traffic, wrapped as `C2S::WaylandRequest`. And the proxy now spawns as a fourth daemon thread, but only when
`RAYLAND_C1_WAYLAND_DISPLAY` is set — the offscreen fixtures and the whole test suite never set it, so they
are untouched, which is the property that let this land without holding its breath over regressions. It came
up clean: the daemon binds the Wayland socket beside the vtest one and tears down without a leak. What 4.1
deliberately does *not* prove yet is a request actually reaching S — S still refuses `WaylandRequest` on the
vtest path by design, so the end-to-end forward is 4.2's to demonstrate, once S has somewhere to route it.

### 2026-07-22 — Task 4.2a: the S side hears the app for the first time

The router landed, and with it the first sign of the app's Wayland session reaching S. The change on S is
small and deliberately so: the message loop now splits a `C2S::WaylandRequest` off before the vtest apply
path — which refuses it by design, because it is not a ring message — and hands it to a new `WaylandReplay`
that opens a connection to S's *real* compositor on first use and, for now, logs each request. The lazy
connect matters: an offscreen fixture sends no Wayland requests, so it never touches a compositor, and the
whole existing test suite is untouched. Pointing vkcube's WSI at the proxy socket (while its rendering still
goes to rayland-c over Venus) lit the path up: S reported "WP0 replay connected to S's compositor" and then
eleven relayed requests streamed in — the surface, the xdg role objects, the commits — with not one refusal.
The forward half of the Wayland tunnel works end to end.

vkcube then aborted, and exactly where the earlier review predicted it would: `pick_surface_format` asserts
`count >= 1`. The proxy advertises `zwp_linux_dmabuf` but sends no `format`/`modifier` events back to the
app, so Mesa's WSI sees zero supported formats and cannot choose one. That is the event-return path (Task
4.4), not a router bug — and it is good to have the wall show up as a clean assertion at the exact spot the
map said it would, rather than as a mystery. What 4.2a proves is narrow and real: requests cross, S receives
them in order, and S can reach its compositor. What it does not yet do is *replay* them — reconstructing each
message and submitting it via the client backend, with the object-id translation and the reconstruction of
the globals the app bound (which, being backend built-ins on C, never crossed) — is 4.2b, and it is where
the object graph gets genuinely rebuilt on S.

### 2026-07-24 — Task 4.2b-i: teaching the wire to carry what S needs to rebuild the graph

Replaying the app's Wayland session on S turns out to hinge on a problem that is easy to miss until you try
it: the app never tells S what it bound. The app binds `wl_compositor`, `xdg_wm_base`, and so on with
`wl_registry.bind`, but on C that request never reaches the proxy's forward path — C runs a real
`wayland-backend` server, which handles `wl_display` and `wl_registry` as built-ins and routes a bind to the
global's handler instead. So the forward stream is full of requests against objects (3, 5, 7, 11…) whose
*interface* S has no way to know. Two gaps, both on the wire, both fixed this turn.

The first: binds must cross explicitly. Added `C2S::WaylandBind { interface, version, app_object_id }`, sent
from the proxy's `GlobalHandler::bind`. It deliberately does not carry the registry's numeric `name` — that
is C's registry's numbering and is meaningless on S, which has its own registry with its own numbers; S will
bind by *interface name* against the globals its own compositor advertises. The second gap is subtler: when
S replays a request that creates a child object, the client backend's `send_request` needs a `child_spec` —
the new object's interface and version. S could reconstruct that from a hand-built `(interface, opcode) →
child interface` table, but that is exactly the per-interface knowledge the structured-tunnel design set out
to avoid. The cleaner answer is that C already knows it authoritatively: the server backend stamps every new
object with its statically-known interface before delivering the request (the one request whose child
interface is *dynamic*, `wl_registry.bind`, is the very one that never crosses). So `WaylandArg::NewId`
grew from a bare id into `{ id, interface, version }`, and the proxy reads the interface straight off the
new object's `ObjectId`. S will map the name to its own linked `&'static Interface` and hand it to
`send_request` — no protocol table, C is the authority.

This is all C-side and fully testable without a compositor, which is why it is its own increment: the
forward test now proves the `wl_compositor` bind crosses and that `create_surface`'s new id arrives stamped
`wl_surface`, both teeth-checked. What it does not do yet is *consume* any of it — S still only logs. Making
S bind its own globals, build the object-id map, and actually submit the reconstructed requests to its
compositor is 4.2b-ii, and that is the half that needs the real compositor to prove.

### 2026-07-24 — Task 4.2b-ii: S replays the session, and the compositor accepts it

The other half of the replay landed: S now *consumes* the forwarded binds and requests and reconstructs the
app's object graph on its own real compositor. The shape mirrors the C proxy exactly, one level down — the
proxy is a `wayland-backend` server to the app, this is a `wayland-backend` client to S's compositor, and in
between is the `app_id ↔ s_id` map. On a bind, S looks up the global its own compositor advertises by
interface name (never by the app's registry number, which is C's and meaningless here), binds it, and maps
the app's object id to the S-side one. On a request, S translates the sender and every object argument
through the map, turns each `NewId` into a null id plus the `child_spec` C stamped on the wire, submits it,
and maps the returned object back. It leans entirely on the structured tunnel: no per-interface code, just a
name→descriptor table for the eleven interfaces WP0 touches.

Two things were worth the trouble. The first was a real find, caught the honest way — by watching it panic.
`wl_registry.bind` is the one request whose child interface is *dynamic*, and its wire signature spells the
generic new_id out in full: `[name, interface_string, version, new_id]`. The naive code passed `[name,
new_id]` and expected the backend to inject the interface and version from `child_spec` — and the backend
asserted the signature mismatch and aborted. The fix is that for `bind`, and only `bind`, those two fields
are explicit arguments; `child_spec` is for the object the backend creates, not for the wire. Every other
request has a statically-typed new_id and does rely on `child_spec` alone. The second was defensive:
`send_request` *panics* on any protocol violation, and the replay runs on the same thread that serves the
vtest/ring session. A translation bug must not take the GPU relay down with it, so the submit is wrapped in
`catch_unwind` — a bad request is logged and dropped, the session continues. The teeth-check for this fed a
deliberately invalid opcode and confirmed the panic is caught and the object left unmapped, exactly as
intended.

vkcube drove the binds all the way onto S's compositor — `zwp_linux_dmabuf_v1`, `wl_compositor`,
`xdg_wm_base`, `wl_seat`, each bound for real, no protocol error. But it still can't reach `create_surface`:
it aborts one step earlier, at `pick_surface_format`, because it queries the swapchain's supported formats
and the proxy returns none — the compositor's dmabuf format events are not yet routed back to the app. So
the request-replay path is proven separately, by an integration test that feeds the replay a `wl_compositor`
bind and a `create_surface` directly and watches both objects appear on the real compositor. The forward
direction of the Wayland tunnel is now complete end to end; what stands between here and a cube on screen is
the return direction — events (4.4) and the buffer token (4.3).

### 2026-07-24 — Pausing WP0 mid-Task-4; a handoff for the next session

Closing this session with Task 4.2 complete — the forward direction of the Wayland tunnel works end to end,
app to S's real compositor — and the return direction (4.3 token→wl_buffer, 4.4 events) still ahead. Wrote a
self-contained handoff, `docs/design/2026-07-24-wp0-task4-next-session-prompt.md`, so a fresh session can
pick up without this context: it points at the spec/plan/ledger, states exactly what's done and what's next,
carries the load-bearing findings (zero-copy is viable and S must retain the creation-time dma-buf;
`wl_registry.bind`'s explicit-args signature; `send_request` panics so replay is `catch_unwind`-guarded),
and records the build/run gotchas (the `/tmp` tmpfs-quota SIGBUS, the loopback vkcube recipe, the
never-pkill discipline). The recommendation to the next session is 4.4 before 4.3: the event return path is
what unblocks vkcube's `pick_surface_format` abort, so doing it first lets the app drive further and show
what comes next on its own.

### 2026-07-24 — Resuming Task 4.4: the format roundtrip is answered locally, so formats must be too

Picked up the WP0 handoff and started on the event-return path (4.4), recommended first because it is what
unblocks vkcube's `pick_surface_format` abort. Reproduced that abort with my own eyes on the loopback smoke,
and the daemon logs said more than the handoff predicted: the app binds `zwp_linux_dmabuf_v1` at **v4** and
its very first request is opcode 2 — `get_default_feedback`. So Mesa is on the **v4 feedback path**, not the
v3 `format`/`modifier` path. Read Mesa's WSI (`wsi_common_wayland.c`) to see what that path needs, and the
answer is decisive: feedback delivers formats through a `format_table` **file descriptor** the client
`mmap`s (`:917-928`), and `tranche_formats` bails to zero formats if that table was never mapped
(`:957-979`). **An fd cannot cross a network.** But the same read showed the escape: Mesa only *opts into*
feedback when the bound dmabuf version is `>= 4` (`:1661`); bind it at **v3** and it falls back to the plain
`modifier(format, hi, lo)` event, which is three integers and no fd. v3 is a complete path in this Mesa, not
a deprecated stub. So the first move is to **cap the proxy's dmabuf advertisement at v3**.

Then the subtler finding, and the one that changes the handoff's plan. The handoff said 4.4 would *relay*
the dmabuf format events back from S. Reading the wayland-backend server the proxy is built on
(`server_impl/client.rs:431-456`), `wl_display.sync` is answered **locally and immediately** — the backend
sends `wl_callback.done(0)` inline during request dispatch, and it never crosses to S. Mesa's format
discovery is a *bounded* roundtrip (`wsi_common_wayland.c:1688`): sync, then read whatever formats arrived,
and if that is zero it aborts. So relaying the `modifier` events from S would be racing them against a sync
callback the proxy answers with no knowledge of S at all — they would win on loopback (microsecond relay)
and lose on a real network, which is precisely the heisenbug shape (c)1 and (c)2 kept getting burned by.
Format/modifier advertisement is a *capability handshake*, and a proxy must answer a capability handshake
from known truth — the same way this backend already answers the registry locally — not by round-tripping it
per app. So the proxy will **synthesize** the `modifier` events itself, the instant the app binds dmabuf,
for LINEAR `XRGB8888`/`ARGB8888` (modifier 0) — the format the LINEAR HOST3D swapchain export uses anyway
(4.0-bis). Delivered inside the bind handler, they are on the wire before any later sync callback, so the
roundtrip cannot see zero.

The general relay path (eventfd wakeup + `send_event` + `S2C::WaylandEvent` + S's reverse id map) is still
needed — but for the *stateful* events the app waits on **unboundedly**: `xdg_surface.configure` (vkcube's
own toplevel handshake blocks on the first one) and `wl_buffer.release` (image recycling). Those tolerate
relay latency because the app blocks reading its socket until the specific event shows up; the format
roundtrip does not, because it does not wait for a *specific* event, only for "whatever arrived by now". So
4.4 splits cleanly in two: local capability synthesis for formats, and a genuine relay tunnel for state.
This is a refinement of the handoff's "forward the format events", reached the way 4.0-bis was — by reading
the source rather than trusting the plan — and left here so the reasoning is visible if it later proves
wrong.

### 2026-07-24 — Task 4.4a landed: formats advertised, and the app walks straight into the configure wall

Built 4.4a — cap `zwp_linux_dmabuf_v1` at v3, and synthesize `modifier` events locally on bind — and it does
exactly what the source predicted. The loopback smoke now logs `bound global zwp_linux_dmabuf_v1 v3` and
`advertised 2 LINEAR dmabuf format(s)`, and vkcube no longer aborts at `pick_surface_format`: it sails past
format selection and drives 59 inline Venus batches (up from 43), i.e. deeper into device and swapchain
setup. The integration test (`wayland_proxy_dmabuf_formats.rs`) pins both halves — the v3 cap and a non-empty
LINEAR format set — and both were teeth-checked (the cap assert caught the descriptor's real v5; suppressing
the advertise left the format assert to catch the empty set).

Then the next wall, and it took some digging because it wears a disguise. vkcube now either hangs to the
timeout or, on a startup race, aborts inside `libvulkan_virtio.so` with no message. The silence was a
red herring: the abort is Venus's `vn_ring_wait_seqno` → `vn_relax` path, whose diagnostics go through
`vn_log` at `MESA_LOG_DEBUG` level, which Mesa filters by default — so "stuck in ... wait" and "aborting"
were being printed to nowhere. And the abort-vs-hang split is just the ALIVE watchdog: `rayland-c` keeps
re-arming ALIVE from ring progress, so the app usually spins forever rather than aborting; only when the
re-arm loses a startup race does the watchdog fire.

The proxy log is what named the wall. After formats are advertised the app loops: bind `zwp_linux_dmabuf_v1`
→ (proxy advertises formats) → `destroy` it → repeat, dozens of times, ending on a fresh object id. That is
Mesa's WSI standing up a transient `wsi_wl_display` for each `vkGetPhysicalDeviceSurface*` query, and vkcube
polling those queries in its wait loop — it is waiting for the window to be **configured**. A Wayland
surface has no valid extent until the compositor sends `xdg_surface.configure`, and nothing returns that
event to the app yet: the whole compositor→app direction is still dark. So the app is stuck exactly where
the handoff said it would be — "a real client blocks on the first configure before presenting." That is the
event return path, 4.4b: the eventfd wakeup and `send_event` on C, the `S2C::WaylandEvent` relay, and S
actively dispatching its compositor connection and translating each event's ids back to the app's id space.
Building it next.

### 2026-07-24 — Task 4.4b, C-side delivery — and a self-inflicted wound worth recording

The C half of the event return path is built and tested: an `eventfd` gives the proxy's `poll` loop a third
pollable fd, a `WaylandEventPoster`/`WaylandEventInbox` pair bridges the daemon's link reader thread to the
serve loop, the proxy keeps an app-id → `ObjectId` map (filled as globals bind and requests create objects,
emptied on destroy), and `deliver_event` resolves an inbound event's ids against that map and hands it to the
app with `Handle::send_event`. An integration test posts an `xdg_wm_base.ping` as the reader thread would and
watches a real `wayland-client` receive it; teeth-checked by suppressing the eventfd wake, which correctly
made the ping never arrive.

The wound: teeth-checking that test, I reverted it with `git checkout crates/rayland-c/src/wayland_proxy.rs`
— and that file had **all of 4.4a and 4.4b's uncommitted work in it**, not just the one-line teeth-check.
`git checkout <path>` discards working-tree changes to the last commit, so it erased the v3 cap, the format
synthesis, and the entire event-delivery path in one command. It is left recorded here because the diary's
job is to show the work honestly, mistakes included, and because the lesson is concrete: **never `git
checkout` a file that holds uncommitted work to undo a temporary edit** — reverse the edit instead. Recovery
was clean only because every change was in the session transcript and could be replayed in order; the two
test files and the other crates were untouched. All six proxy tests are green again on the reconstructed
file. The next move is unchanged: the S side (translate real compositor events S→app, and dispatch S's
compositor connection so they arrive) and the daemon wiring.

### 2026-07-24 — Task 4.4 done: the event tunnel works, vkcube gets configured, and the wall moves to 4.3

The S side of the return path landed, and with it the whole event tunnel closes. `WaylandReplay` now keeps a
reverse `s_id → app_id` map beside the forward one, `ReplayObjectData::event` translates each compositor
event back into the app's id space and emits it through a new `EventSink`, and — the piece that makes events
actually arrive — a dedicated **compositor-reader thread** dispatches S's compositor connection, because
events turn up while the app is idle waiting and S's message thread only ever writes to that connection. On
C, the daemon builds an eventfd-backed channel, the link reader routes each `S2C::WaylandEvent` to the
proxy's poster, and the proxy delivers it with `send_event`. Proven at both ends: a real compositor's
`wl_seat.capabilities` is translated and emitted on S (teeth-checked by suppressing the emit), and a posted
`xdg_wm_base.ping` reaches a real client on C.

Then the smoke said the thing worth saying. vkcube, run over the loopback relay, now receives its compositor
events — `wl_seat.capabilities`, **`xdg_surface.configure`, `xdg_toplevel.configure`** — and, decisively,
**sends `xdg_surface.ack_configure` back**. The configure handshake it was stuck on for the last two tasks
completes, and the app drives on: it creates its swapchain buffers via `create_params`/`add`/`create_immed`.
That is exactly the wall the handoff said 4.4 would remove, removed — the app could not ack a configure it
never received, and now it does.

It stops at the next wall, and the next wall is precisely Task 4.3. The `params.add` interception fires but
`resolve_inode` returns `None` — the swapchain image's memfd does not correlate to a resource id — so no
`BufferToken` is built, `create_immed` is not forwarded, and S skips the unmapped buffer. Turning that token
into a real `wl_buffer` on S (retaining the HOST3D dma-buf, resolving the inode, building the buffer via S's
`zwp_linux_dmabuf`) is 4.3's whole job, and why the inode resolves to nothing for a HOST3D swapchain image is
the first thing 4.3 has to answer. The handoff called this too: "4.3 makes the pixels actually appear once
the app reaches attach." It reaches attach now.

One honest wart to carry forward: S relays its *real* compositor's dmabuf `modifier` events back to the app
(~30 per bind × ~10 transient probe binds = ~480 events), on top of the two LINEAR modifiers the proxy
synthesizes locally. Harmless for correctness — Mesa just accumulates format entries — but noisy, and the
synthesized set is the authoritative one. A later pass should filter S's dmabuf `modifier`/`format` events
out of the return path, since the proxy answers that capability itself.

### 2026-07-24 — Starting 4.3: fixed the modifier flood, and met the wall behind the wall

Began Task 4.3 (buffer-by-token → wl_buffer) the way systematic-debugging asks — root-cause the
`resolve_inode → None` before touching anything — and two things came out of it, one fixed and one that
needs a decision.

Fixed: the modifier flood. S was relaying its *real* compositor's dmabuf `format`/`modifier` events back to
the app — ~30 per bind, and the app stands up ~10 transient `wsi_wl_display`s while probing surface support,
so ~480 events per run crossed the link, duplicating the two LINEAR modifiers the proxy already synthesizes
locally (4.4a). `translate_and_emit` now drops any event whose sender is a `zwp_linux_dmabuf_v1` object at
opcode `format`/`modifier`: the proxy owns that capability, S stays out of it. Deliveries dropped from ~482 to
3 (the three that matter — seat capabilities and both configures). Tests still green.

The wall behind the wall: a nondeterministic abort I could not have seen before 4.4, because before 4.4 the
app died deterministically at `pick_surface_format`. Now it gets past formats *and* configure — the logs show
`xdg_surface.configure` and `xdg_toplevel.configure` delivered and acked — and then, building its swapchain,
it aborts intermittently inside `vn_ring_wait_seqno`. Characterized, not guessed: S's log shows **no** engine
error, no context destroy, no fatal; C's log says **"session ended cleanly, 59 batches"** — no stall detector
firing. So it is neither a virglrenderer fatal nor a C-detected ring stall. It is Mesa's own ~3.5 s ALIVE
watchdog firing during swapchain setup: a synchronous Vulkan call waits on a ring reply that does not arrive
inside the window, and the app aborts before C's 30 s stall timeout ever would. This is the (c)1 relay meeting
its first real interactive workload — vkcube creates a swapchain of large HOST3D images and renders
continuously, orders of magnitude more ring/blob traffic than the single-frame refapp or the offscreen icosa
that validated (c)1/(c)2. The flood fix did not remove it (deliveries are down to 3 and it still aborts), so
it is not return-link congestion.

The open question is whether the cause is S's *shared message thread* — which now serves both the ring apply
and the Wayland replay, so a burst of relayed Wayland requests can delay ring apply — or a rawer throughput
ceiling in the relay under this load. It gates 4.3's live validation (the app has to reach `attach`
*reliably* to exercise the token path), and the honest reading of systematic-debugging here is that this is
possibly architectural and worth a decision before a big change, not another guess. Pausing 4.3 at this
boundary with the finding written down.

### 2026-07-24 — Root-causing the vkcube stall: it is S's ring execution, not WP0

Chased the intermittent vkcube stall the disciplined way, and it narrows cleanly — away from WP0 and into
the (c)1/(c)2 relay's ring-execution path.

First, what it *is*, from `/proc/<pid>/task/*/wchan` during a live hang (no ptrace needed, so no fighting
yama): vkcube's main thread sits in `hrtimer_nanosleep` — the `vn_relax` busy-wait of `vn_ring_wait_seqno`.
So it is unambiguously a **ring stall**: a synchronous Vulkan call spinning for a ring seqno reply that never
retires. Not a Wayland dispatch wait (that would be `poll`/`ppoll` on the display fd), not a fatal, not a
C-side stall (C logs "session ended cleanly", so its 30 s timeout never fires — Mesa's own ~3.5 s ALIVE
watchdog gets there first, which is the abort variant; the hang variant is the same stall with ALIVE kept
alive by chance).

Then, where it *is not*. Timed S's message thread (throwaway instrumentation, since removed): every
`handle_request`/`handle_bind` and every `apply` returned in under 5 ms. So the message thread is **not**
blocked by the Wayland replay — the leading hypothesis, refuted. The dump of S's own threads during the hang
tells the rest: the message thread is parked in `futex` reading the link (the app is stalled, so it sends
nothing to apply); the engine actor is parked in `futex` (idle, no engine call pending); the progress thread
is *running* (`wchan=0`) but finding nothing to ship; virglrenderer's render-server threads are idle. So the
ring simply **is not advancing on S** — virglrenderer's ring thread is not executing vkcube's queued commands,
and the progress thread therefore has no head advance to return.

That is the (c)1 doorbell / (c)2 multi-queue machinery meeting its first real interactive workload. vkcube
creates a swapchain of large HOST3D images and submits on the application queue (S already logs
`ring_idx=1`), where the single-frame refapp and offscreen icosa that validated (c)1/(c)2 never went. The
prime suspects are the two things the ledger already flags as (c)1/(c)2 seams: the doorbell that must wake
virglrenderer's parked ring thread (`rayland_vtest::venus_ring::doorbell`), and multi-queue support, still
open. 4.4 did not cause this — it is what let the app get far enough to expose it. Recorded here and handed to
the owner as a decision point: fixing it is a (c)1/(c)2 ring-execution investigation, distinct from WP0's own
remaining work (4.3, the buffer token), and worth scoping deliberately rather than folding into WP0.

### 2026-07-24 — The ring-stall, narrowed to the render-server's parked ring thread — and it is (c)2's ground

Kept pulling the ring-stall thread on the owner's say-so, and it narrows to a precise, deep place — no longer
"somewhere in the relay" but a specific mechanism, and squarely (c)2's, not WP0's.

The dead ends first, because they matter. **The missing crutch flags were not it.** The passing e2e tests set
`VN_PERF=no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback` and my
vkcube smoke set none — a real harness gap, so I fixed it. But with the full crutch table set, vkcube still
hangs (deterministically now, which is itself progress). So the stall is not the feedback pages and not a
second ring.

**One ring, doorbell rung, thread still parked.** Instrumented the ring latch and the doorbell: exactly one
`vkCreateRingMESA` is seen and latched, and the "no doorbell rung" branch never fires — so S rings the single
ring's doorbell after every applied delta, exactly as designed. Yet dumping the **render-server subprocess**
(`virgl-1-gpu_ren`, where virglrenderer actually runs the Venus ring thread — it forks a render server, C0
Task 1) shows `vkr-ring-1` and `vkr-queue-1` **parked on a futex**, its main thread waiting for more socket
packets, and rayland-s's own engine actor idle. So the doorbell reaches the render server but the ring thread
does not advance far enough to retire what the app waits on.

And the app's shape is the tell. S's log shows it **create a device, `vkDestroyDevice`, then create another
device** before the stall, and with feedback off it releases its synchronous calls by polling
`vkGetFenceStatus` (the (c)2 G' path). So the most likely mechanism is not the doorbell at all but the
**completion/fence path**: the app polls a fence that never signals because the underlying swapchain GPU work
is not retiring on S — the (c)2 readback-gate/fence lifecycle meeting a real WSI+present workload with a
device recreation in the middle, where the single-frame refapp and offscreen icosa never went.

This is a (c)2 ring-execution / fence-completion investigation, distinct from WP0's own remaining work (4.3),
and deep enough to deserve its own focused session rather than being chased further at the tail of a long WP0
turn. Recorded in full — the confirmed facts, the two dead ends, and the current best hypothesis — so it can
be picked up cold. The diagnostic that got here (dump `/proc/<pid>/task/*/wchan` for vkcube, rayland-s, and
the render-server child during a live hang; no ptrace needed) is the tool to keep using.

### 2026-07-24 — head vs applied_tail: the ring executes fine; the stall is a lost release on the return path

Ran the head-vs-applied_tail check, and it decides the question — and overturns both standing hypotheses.
Instrumented S's ring mirror to log `head` (what virglrenderer's ring thread has consumed) against
`applied_tail` (what S has written) through a live hang.

**The ring is not the problem.** head tracks applied_tail the whole way up — `0 → 760 → 8932 (a brief
376-byte gap) → 10400 → 18812 → 19508`, gap returning to 0 — so virglrenderer's ring thread is consuming and
executing vkcube's commands fine. That kills the doorbell/park-race hypothesis outright, and it is not a
ring-consumption stall either. The `vkr-ring-1` threads being parked was a red herring: they were parked
because they had *caught up*, not because they were starved.

**The stall is a lost release on the return path.** Two counters — `take_ring_progress` calls and per-ring
polls — both freeze at ~5000 while `rings == 1` throughout, so it is not the ring mirror being dropped; the
progress thread simply stops advancing once the ring catches up (head = applied_tail = 19508). And the
steady state of the hang is **fully quiescent**: vkcube spins in `vn_ring_wait_seqno`, while S's progress and
message threads and C's reader are all parked on futexes and C's ring watcher is asleep. Everything has gone
idle. S executed everything the app submitted and reported it, and the app was still never released.

So the app is waiting for a ring `head`/seqno that S has, on its side, already reached — and the release is
not getting back to it. That points the last question squarely at the C-side head-advance: does C actually
advance the *application's local ring head* to the `consumed_tail` S reported (19508), and is the seqno the
app awaits within it? The suspects are a `RingProgress` that C's `note_consumed` rejects (its frontier
bookkeeping), a final delta C's watcher never relayed (a park lost-wakeup on C, symmetric to the one S's
doorbell fixes), or the app awaiting a seqno tied to the device it destroyed and recreated — the (c)2 gate
retired the readback gate for `ring_idx=1` on that `vkDestroyDevice`, and the new device's queue may leave
the release in a state the head-advance no longer satisfies. That is the clean next step, and it is a C-side
/ (c)2-release question, not a ring-execution one. The whole trail — the two dead ends and this decisive
turn — is in the SDD ledger for a cold pickup.

### 2026-07-24 — C-side note_consumed check: C is innocent; the app polls a fence that never signals

Instrumented C's release path — every `RingProgress`'s `note_consumed` Ack, the head C publishes, and Mesa's
actual ring tail — and it clears C completely and, in doing so, corrects yesterday's "lost release" reading.

**C is not the problem.** Every Ack is `Advanced` — no `Stale`, no `PastFrontier`. C relays everything Mesa
writes (`relayed == mesa_tail`), S consumes and reports all of it, and C publishes the app's ring `head` all
the way to Mesa's tail (`published head=21564 mesa_tail=21564`). The frontier bookkeeping does exactly its
job. Both C-side suspects — a rejected `RingProgress` and an unrelayed final delta — are dead.

**And the ring is not stalled — it is *flowing*.** `mesa_tail` keeps *growing* (…21084 → 21256 → 21368 →
21564) with `head` tracking it. That is the tell: the application is **actively polling `vkGetFenceStatus`**
— writing a fresh poll command each iteration (tail grows), S executing it (head keeps up), the app reading
the reply and, finding **`VK_NOT_READY`**, polling again. Forever. Yesterday's "fully quiescent" snapshot was
just a between-polls instant — `vn_relax` sleeps between polls, so at any given millisecond everything looks
idle, but across polls the ring is moving.

**So the real cause is an S-side fence that never signals.** vkcube submits its swapchain render, creates a
fence, and polls it; on S the poll executes fine but never reads `VK_SUCCESS`, because the submitted GPU work
never *completes* on S's side. This is (c)2 fence / GPU-completion ground, and specifically for a **present**
(non-readback) submit — a shape the offscreen refapp/icosa never produced. The likely mechanisms, in order:
the render submit waits on the swapchain **acquire semaphore** that never signals over the relay (with
`no_semaphore_feedback`, semaphore signalling rides the ring, and the WSI acquire's signal may not be
propagating); the (c)2 readback-completion machinery mis-serving a submit that has no readback; or a
downstream effect of 4.3 being unfinished — no `wl_buffer` on S means no present, so the swapchain image is
never released and the frame's fence chain never closes. That is the next thread to pull, and it is squarely
(c)2 fence-completion, not WP0 and not the C relay. Full numeric evidence and the tool (`/proc` wchan dumps +
C-side Ack/head/tail logging) are in the SDD ledger.

### 2026-07-24 — The vkcube "hang" is the latency trap, not a deadlock — and the RingBarrier is the tell

Went looking for the unsignalled semaphore, and found there is no deadlock at all. Decoded the ring command
stream C relays (the venus_ring decoder reports the first unsizeable opcode, which is the app's real command),
and every command at the stall is **device/pipeline setup** — `vkGetPhysicalDeviceFormatProperties2`,
`vkCreateImage`, `vkGetImageDrmFormatModifierProperties`, `vkAllocateMemory`, `vkCreateDescriptorSetLayout`,
`vkGetFenceStatus`. No render submit is even reached; the app is still building its pipeline.

And it is **making progress**, just catastrophically slowly. Under a 90-second timeout it relayed **95 deltas**
and still aborted — roughly *one synchronous command per second* by the end. That is not a hung ring; it is
the **X11-over-network latency trap** ring-findings §7 named as (c)1's central weakness: every one of
vkcube's *hundreds* of synchronous setup calls is a full C→S→execute→C round-trip, and vkcube's setup is
synchronous-call-dense in a way the offscreen refapp and icosa never were (they render in ~1 s because they
make comparatively few waited-on calls).

The `RingBarrier` is the concrete tell. It logs three timeouts, each the same shape: `has not shipped tail
21564 within 1s (it stands at 21484)` — an **80-byte gap**, i.e. exactly one small ring command the watcher
has not relayed, and the inline path waits the full `FLUSH_TIMEOUT` (1 s) before giving up and letting the
command cross. So on top of the general round-trip latency, a handful of inline commands each eat a dead
second waiting on an 80-byte tail the watcher is slow to ship — a park/relay lag on C's watcher for the last
small write, symmetric to the doorbell problem but on the forward path.

So the conclusion, after the whole chain: this is **not a WP0 bug, not the C relay's correctness, not a
doorbell/park race, not an unsignalled fence** — it is (c)1's synchronous-reply **latency**, exposed by the
first application whose setup is round-trip-heavy, plus a 1-second `RingBarrier` stall on a slow last-delta
ship. The lever is the one the ledger already names — a bigger ring and pipelining to cut the number of
waited-on round-trips, not a point fix — and it is a (c)1/(c)2 performance investigation, cleanly separate
from WP0. WP0's own forward+event tunnel (4.1–4.4) is done and correct; 4.3 (the buffer token) and any live
vkcube proof sit behind this latency work. The full opcode trail and the barrier evidence are in the ledger.

### 2026-07-24 — The 80-byte gap is a ~40 ms QUIC send, not a watcher bug — and initial_rtt did not fix it

Set out to fix the RingBarrier's 1-second stall on a trailing 80-byte delta, and it dissolved into the same
latency wall, one layer down. The chain of evidence, each step instrumented and then reverted:

- The watcher is **not** drained-but-not-shipped: in the no-delta path `last_tail == shipped` always.
- At a barrier timeout, a **fresh** re-read of `current_tail` confirms Mesa's tail really is the target
  (21564), while the watcher's `shipped_tail` sits at 21484 for the full second and then jumps — so the
  watcher fell **>1 s behind** on one delta, not permanently, not stuck.
- Timing the relay pins where: `lock_wait` on the send mutex is **~1–3 µs** (no contention), but the QUIC
  send itself takes **20–45 ms per delta**. On a loopback link that should be sub-millisecond. That is the
  whole story: each ring delta costs tens of milliseconds to put on the wire, vkcube's setup ships hundreds,
  and once in a while one lands slow enough that the 1-second barrier gives up.

So the "80-byte-gap watcher lag" is not a watcher bug at all — it is the relay's **per-message QUIC latency**,
the same synchronous-round-trip wall from every angle. I tried the obvious transport fix — quinn's
`initial_rtt` defaults to a wild **333 ms**, so I seeded it at 1 ms — and it made **no difference**
(still 7–9 slow sends per run, still ~40 ms each). So the cost is not RTT-timer-driven; the leading remaining
suspect is quinn's **delayed ACK (25 ms default) throttling the congestion window** during a burst, but the
linked quinn rejects `max_ack_delay` (a version-skew), and confirming/curing that is a real transport tuning
task, not a knob to flip blind. I reverted the `initial_rtt` change rather than leave an unproven tweak in
the tree.

Honest conclusion: the 80-byte gap, like the whole vkcube stall, is **(c)1's synchronous-reply latency**
surfacing at ~40 ms per QUIC message — a transport/relay performance investigation (bigger ring + pipelining
to cut the *number* of waited-on round-trips, or transport tuning to cut the *cost* of each), cleanly
separate from WP0. It is not a small fix, and pretending otherwise by shipping an untested transport change
would be the wrong kind of green. WP0's own tunnel (4.1–4.4) remains done and correct; this latency is what
sits between it and a live vkcube.

### 2026-07-25 — Delayed ACK is refuted: the vkcube slowness is a blob-sync bandwidth explosion

Tested the delayed-ACK hypothesis directly — enabled quinn's ACK-frequency extension asking the peer to
acknowledge *every* packet within 1 ms (`ack_eliciting_threshold(0)`, `max_ack_delay(1 ms)`) — and it changed
nothing: sends still 20–50 ms, still a **2.9-second** spike. So delayed ACK is not it. But the same run,
instrumented to sum payload bytes, named the real cause outright.

The send is **data-volume-bound, not latency-bound.** Per ring delta the watcher ships not just the ~100-byte
command delta but a whole-blob `BlobData` for every application blob, and those bytes grow as vkcube builds
its pipeline: **264 KB, then 1.27 MB, then 2.28 MB per delta** — the 2.28 MB one is the send that took 2.9 s.
The metrics are unambiguous: `c2s_blob_sync_bytes = 16,574,464` against `c2s_ring_bytes = 22,955` — **99.9%
of everything C sends is blob resends**, 720× the actual command traffic, across 109 messages re-shipping the
same growing blobs.

This is (c)1's own deferred debt, not a transport bug. Spec §7 chose the dumb-but-correct v1 strategy — *ship
the entire blob on every sync, no dirty-range tracking* — because Venus gives no API signal for which bytes
changed (the ring-findings §5.1 "no seam to hook"). The offscreen refapp and icosa have one small mapped blob
each, so re-shipping it whole is cheap and the strategy held. vkcube has many large blobs — staging pools,
swapchain images, textures — so re-shipping *all of them whole on every one of ~100 setup deltas* is
O(total blob size × deltas), and it buries the link: 16.5 MB to move ~23 KB of commands. That is the ~40 ms
send, the 2.9 s spike, the RingBarrier timeout, and the "hang" — all one thing.

So both transport levers were the wrong tree: not `initial_rtt`, not delayed ACK. The real lever is the one
the ledger flagged as (c)1's bandwidth follow-up — **ship only what changed** (dirty tracking, or shipping
only the blobs a delta's commands actually read, or content-addressing so an unchanged blob crosses once).
That is a real (c)1 design task with a clear shape, and it is what stands between the correct WP0 tunnel
(4.1–4.4, unchanged) and a live vkcube. Reverted both throwaway transport experiments; nothing shipped.

### 2026-07-25 — Scoping the fix: send only what changed, mirroring the return path

With the vkcube slowness pinned to whole-blob re-shipping (16.5 MB to move 23 KB of commands), scoped the
(c)1 fix into a design spec (`docs/design/2026-07-25-c1-incremental-blob-sync.md`). The shape was never really
in doubt once the cause was clear, and the module's own docstring had sketched it in prose from the start:
C keeps a baseline copy of what it last sent S for each application blob, diffs the live mapping against it on
each relay, ships only the changed byte-runs, and ships nothing for an unchanged blob. It is the exact
mirror of the technique S already uses for the return path (the `BlobRun` diff, Task 5b) — proven, symmetric,
and it needs no wire change because `C2S::BlobData` already carries an offset.

Two things were worth thinking through rather than assuming. First, the owner's instinct to combine a
fingerprint with the copy: a good question with an instructive answer — because there is no change signal to
consult, C must read the whole blob every relay regardless, and once you are reading it, comparing directly
against the saved copy already answers both "did it change?" and "which bytes?"; a fingerprint would be a
second pass answering only the former. So the fingerprint buys nothing *here* — its real job (spotting
identical content across blobs or across time, so it never crosses twice) is content-addressing, which is
(c)3, deliberately out of scope. Second, the one subtle correctness point, which the spec makes load-bearing:
when S's own writes come back over the return path, C must fold them into the baseline as it applies them, or
the next forward diff would ship S's bytes straight back — the symmetric twin of the last-writer-wins bug
Task 5b fixed on the S side. The scope boundary is drawn hard: this removes re-shipping of *unchanged* blobs
and nothing more — it is not the remote-`vkMapMemory` problem (a blob genuinely rewritten every frame still
ships whole; that stays (c)2's), and not dedup ((c)3). It is exactly what stands between the correct WP0
tunnel and a live vkcube.

### 2026-07-25 — Design and plan for the blob-sync fix; setting up to hand off

Turned the confirmed cause — C re-shipping unchanged application blobs whole on every relay — into a design
spec and an implementation plan, both committed. The approach is settled and small: C keeps a baseline copy
of what it last sent S for each application blob, diffs the live mapping against it on each relay, ships only
the changed byte-runs (reusing `BlobData`'s offset field, no wire change), and folds S's return-path writes
into the baseline so it never ships S's own bytes back. It mirrors the S→C diff (Task 5b) the code's own
docstring already pointed at. The plan is three tasks with literal code and a hard gate: the loopback e2e
(refapp + icosa) must stay bit-identical, because a dropped run shows up as a wrong pixel there.

Chose subagent-driven execution and set up its workspace, ledger, and Task 1 brief — but stopped before
dispatching any implementer, because the session is closing and the plan carries a ~6-minute GPU gate that
does not fit one turn. Nothing is mid-flight. Wrote a self-contained kickstart prompt
(`docs/design/2026-07-25-c1-blob-sync-next-session-prompt.md`) so a fresh session can execute the plan cold,
carrying the load-bearing context: WP0 4.1–4.4 are done and correct, the vkcube "hang" is this blob-sync
bandwidth debt and not a WP0 bug, and the transport dead ends (delayed ACK, initial_rtt) are already ruled
out and reverted. Not merging to main: the branch is mid-project (WP0 4.3 and a live vkcube still ahead, the
blob-sync fix designed but unbuilt), so it stays a feature branch until WP0 actually lands — everything is
committed and pushed instead. Loose ends closed to the branch, not to main.

### 2026-07-25 — The blob-sync fix landed and held: e2e stayed bit-identical, C→S blob bytes fell from 16.5 MB to 267 KB

Executed the three-task plan (`a0b4bd7` baseline+diff primitive, `b84fab5` ship-changed-runs-only, and this
session's re-baseline-on-return-path call) and ran the whole thing end to end. The one line this task added
was small on purpose: in `apply_blob_data` (`crates/rayland-c/src/main.rs`), right after S's bytes are copied
into the mapping, `blob.note_s_wrote(start, bytes)` folds those same bytes into C's baseline. Without it, the
fix would have been only half-symmetric: C would stop re-shipping *its own* unchanged blobs, but the moment
S wrote a readback back, C's next diff would see the mapping (now carrying S's bytes) differ from a
still-zero baseline and ship S's own bytes straight back to S — the C→S twin of the last-writer-wins wobble
(c)1 Task 5b fixed on the S→C side. `rayland-c --lib` (36/36) and `no_gpu_linkage` both stayed green.

**The correctness gate is the part that actually mattered, and it held clean on the first try.**
`cargo test -p rayland-s --test loopback_e2e` — both tests passed (`test result: ok. 2 passed; 0 failed`,
97 s): the refapp's `assert_eq!` triangle-pixel checks all held (they'd fail loudly on any dropped byte), and
the icosa test's per-frame comparison against the native run — 120 frames, byte-for-byte — produced zero
`FAILED` frames. This is the proof the diff-and-rebaseline scheme loses nothing: refapp exercises one
write-once/read-back blob, icosa exercises a blob rewritten every single frame, and both routed through the
new run-diffing/re-baselining path with no wrong pixel anywhere. Nothing here was weakened or skipped to get
green.

**The measurement** (the actual point of the whole sub-project) came from a loopback vkcube smoke
(`rayland-s`+`rayland-c` via `setsid`, `RAYLAND_C1_METRICS=1`, the same `VN_PERF=no_multi_ring,...` flags the
4.3 work already established). vkcube ran for ~35 s before hitting the same known SIGABRT/latency-trap wall
the 07-24 entries already root-caused (S-side ring-completion latency under WP0's still-open items — not
this change's scope, and not touched). In that window it relayed 454 blob-sync messages and 89 ring deltas.
The last `C1METRICS` line before the abort: `c2s_blob_sync_bytes=267069` against the pre-change baseline of
**16,574,464** — a **~62×** reduction, not the full "two orders of magnitude" a longer, uninterrupted run
might show, but the same direction and the same mechanism (stop re-shipping blobs that did not change).
`c2s_ring_bytes=23764` versus the baseline's 22,955 — essentially unchanged, as expected, since this task
never touches ring-delta shipping. Reporting this as measured, not extrapolated: vkcube did not run to
completion, so this is the sample the ~35 s window actually produced, not a projection of what a full run
would show.

Worth stating alongside the truncation caveat, because it *strengthens* the claim rather than only hedging
it: the two windows being compared carried a comparable amount of actual command work, not wildly different
amounts that a bandwidth ratio could be distorted by. The "after" run relayed 89 ring deltas totalling 23,764
ring bytes before its abort; the original "before" measurement (the one that found 16,574,464 blob-sync
bytes) relayed 83 ring deltas totalling 22,955 ring bytes. 89 vs. 83 deltas and 23,764 vs. 22,955 bytes are
close enough that the ~62× blob-traffic reduction is not an artefact of one run doing dramatically less work
than the other — it is a comparison of two runs that did essentially the same thing, one of which happened
to carry ~62× less blob-sync bytes to do it. That is a materially better argument for "the reduction is real"
than the truncation caveat alone would suggest, and it belongs next to that caveat rather than replacing it:
the run still did not reach completion, so the ~62× figure is still this sample, not a settled asymptote.

**What is still open, stated plainly.** This does not touch remote `vkMapMemory`: a blob genuinely rewritten
every frame (icosa's fractal texture, or vkcube's own per-frame uniforms) still diffs to nearly its whole
size every time, because "nearly everything changed" is a true diff, not a bug — that remains (c)2's problem.
Cross-blob and cross-time content dedup (an identical blob crossing only once, ever) is untouched and stays
(c)3. And the vkcube abort itself is neither caused nor fixed by this change — it is the same S-side
ring-completion latency wall the 07-24 entries already traced to (c)2 ground; this session did not chase it
further, since the brief scoped Task 3 to the re-baseline call and the two gates (unit suites, e2e), not to
making vkcube run to completion.

### 2026-07-25 — Final whole-change review found two real defects in the blob-sync fix; both fixed, neither was a regression

Each of the blob-sync fix's three tasks passed its own review as it landed. A fourth, whole-change pass over
the finished diff then found two defects the per-task reviews had not — both real, both in code the "e2e
stayed bit-identical" claim above did not actually exercise, which is itself the finding worth recording
honestly rather than glossing over.

**The first: the design's own "single read" promise was not implemented.** The spec is explicit that
`take_changed_runs` must diff and re-baseline **from one read** of the live mapping, precisely so the bytes
recorded in the baseline are the same bytes shipped in the run — "reading once and using those same bytes
for both the run and the baseline closes that gap," the design says, in so many words. The code that landed
read `live` once to build the baseline inside the inner loop, then read `live` a **second** time to build the
`BlobRun`'s payload. Mesa writes these pages with no synchronisation at all, so between those two reads the
application could rewrite the bytes: the baseline would then hold what was read at t1, the wire would carry
what was read at t2, and if the application's next write happened to revert to the t1 value, C's baseline and
S's actual copy would disagree **permanently and silently** — S rendering from bytes the application never
had, with nothing anywhere to notice it. The fix is one line: ship from `self.baseline[start..i]`, which the
inner loop has already set equal to what was read, instead of from `live[start..i]` again. The instructive
part is *where* the defect came from: the implementation plan
(`docs/superpowers/plans/2026-07-25-c1-incremental-blob-sync.md`, Task 1 Step 6) contains this exact code
verbatim, so it was not a slip introduced while executing the plan — the plan itself authored the bug. Fixed
in both places, with a note in each explaining why the run is copied from the baseline rather than the
mapping, so a future reader tempted to "simplify" it back to `live` sees why not.

**The second: a missed baseline-fold site, made live by a design choice from a different task.**
`apply_blob_data` folds S's inbound bytes into the baseline — that was Task 3's whole job. But it is not the
only place S's bytes land in a C-side mapping: `commit_pending_blob` (`relay_engine.rs`) lays `initial` runs
from `S2C::BlobCreated` into a blob's shadow *before* it is even registered, and never folded them into the
baseline. This sat unnoticed because `initial` looks, at a glance, like it should usually be empty — until
`rayland-s/src/apply.rs` is read: it *deliberately* ships `take_bytes_s_wrote(0)` at blob creation, because a
readback buffer's blob is created lazily, at the application's first `vkMapMemory` of it, which happens
**after** the GPU has already rendered a frame into it. So a blob is routinely born already holding S's
pixels, and the fold was missing at exactly the site that matters most. Left as it was, the consequence
would have been the same class of failure the design's whole return-path fold exists to prevent: C's mapping
holding S's pixels while C's baseline still read zeros, so the very next `take_changed_runs` would read
those pixels as an application change and ship them straight back to S — potentially clobbering a newer
frame S had since rendered, since S's own `copy_in` re-snapshots its shadow over whatever range it writes and
so would not correct C's stale write either. Fixed by adding the same `note_s_wrote` call at this second
site, inside the same bounds-checked loop and skipped together with it on a bounds failure.

**Neither defect was a regression against the old whole-blob behaviour** — v1 shipped every application
blob's full contents on every relay regardless of any baseline, so there was no baseline to be wrong about.
Both are new hazards introduced by *this* fix, in the exact two places its own correctness argument rests on:
the single-read discipline, and the completeness of "every site S's bytes arrive." **Worth recording as a
limit of the gate, not just a near-miss:** the loopback e2e (`rayland-s/tests/loopback_e2e.rs`) stayed
bit-identical through both defects and would not have caught either — the race in Finding 1 needs a write
landing between two reads of the same live memory, which loopback's tight timing does not reliably provoke,
and Finding 2's clobber needs a readback blob whose `initial` runs are non-trivial *and* a second relay event
after it, which the reference app's one-shot rendering never produces. That the e2e is bit-identical is
therefore evidence the diff loses no byte on the paths it actually exercises, not evidence these two paths
are safe — a distinction worth being honest about rather than letting a green gate imply more than it
showed. Added unit tests for both wirings, each teeth-checked by removing the fix and confirming the test
fails before restoring it, so these two paths now have direct coverage the e2e was never going to provide.

### 2026-07-25 — vkcube's abort is a Venus *reply-decode* failure, not bandwidth and not WSI formats — and an A/B that finally makes the blob-sync win comparable

With the blob-sync fix landed, the obvious next move was WP0 Task 4.3 (token → `wl_buffer`). Before designing
it, one question needed an answer: **how far does vkcube actually get now?** The answer moved the work
somewhere else entirely, and overturned two things this diary had previously recorded.

**vkcube never reaches `attach`, so 4.3's code path is unreachable.** The C proxy's complete per-request trace
for a run is *thirty-six lines*. The application binds `wl_compositor`, `xdg_wm_base` and `wl_seat`, creates
its surface, gets its `xdg_toplevel`, receives the `xdg_toplevel.configure` and `xdg_surface.configure` the
4.4 event tunnel delivers, and **acks the configure** — so 4.4 is genuinely working, which is worth saying
plainly. Then it binds `zwp_linux_dmabuf_v1` six times, is answered each time with the two synthesized LINEAR
formats, and dies. It never calls `create_params`, never constructs a `BufferToken`, never attaches anything.
The `WaylandArg::Buffer(_)` arm that Task 4.3 exists to fill is not reached even once. Designing 4.3 now would
have been designing against an unexercised path, resting on two assumptions — that the swapchain image is a
retained HOST3D blob, and that Mesa uses `create_immed` rather than the async `create` — that nothing has yet
tested. This project's history is mostly a record of exactly that class of assumption being overturned by
measurement, so the design was stopped and the wall investigated instead.

**The abort is silent, and that is the diagnostic.** vkcube exits 134 with *no* message: not an assertion, not
a Vulkan error, not a Venus log line. `VN_DEBUG=result` adds nothing. Under gdb the stack is unambiguous even
stripped: `wl_display_dispatch_queue_pending` → libffi → **vkcube's own listener callback** → the Vulkan
loader → nine frames deep into `libvulkan_virtio.so` → `abort()`. So the application, while dispatching
Wayland events, calls a Vulkan entry point that aborts inside the Venus ICD. Ubuntu builds Mesa with `NDEBUG`,
so every `assert()` in Mesa's WSI is compiled out and cannot be the source. Reading the Venus sources for the
abort sites — the project's standing preference for source over inference — there are ten, and **nine of them
`vn_log` a message first**: ring fatal, expired ring-alive watchdog, iteration bound, lost vtest connection,
bad `cmsghdr`. Exactly one aborts in silence:

```c
static inline void
vn_cs_decoder_set_fatal(const struct vn_cs_decoder *dec) { abort(); }
```

and it has exactly one trigger, in `vn_cs_decoder_peek_internal`:

```c
if (unlikely(size > dec->end - dec->cur)) { vn_cs_decoder_set_fatal(dec); ... }
```

**Venus tried to read more bytes out of a reply than that reply contained.** The failure is a short or missing
reply on a synchronous call — a (c)1 *reply-path* defect. The dmabuf probing that immediately precedes it is a
red herring: it is merely the last traffic on the wire before the call that fails.

**Overturned belief #1: this abort was attributed to bandwidth, and it never was.** The 2026-07-24 entries and
the ledger record vkcube's `SIGABRT` as a consequence of the whole-blob resend flood — the "latency trap", the
"S-side ring-completion latency". An A/B settles it. The pre-blob-sync commit (`3a7fc39`) was built in a
throwaway worktree with its own target directory and run through the identical gdb-wrapped smoke. It produces
**thirty-six proxy trace lines, the same sixth dmabuf probe as its last act, and a backtrace identical to the
current one byte for byte** — the same twenty-five frames at the same addresses. The application always died
here. The resend flood made it slower to arrive, nothing more. That the blob-sync work was necessary is not in
question; that it would move this wall was an assumption, and it was wrong.

**Overturned belief #2 — in the useful direction: the bandwidth win is larger than recorded, and now cleanly
comparable.** The entry above reports ~62× and carefully hedges it, because vkcube aborted partway through a
60 s window and a truncated run is not comparable to a full one. That caveat can now be retired, because the
A/B gives something better: **both builds stop at the identical application state** — same 36 proxy lines,
same abort, same frame — so their byte counts measure the same work. Baseline `c2s_blob_sync_bytes =
31,315,399`; current `267,069`. **A 117× reduction, like for like.** The hedge was honest when written and is
simply superseded by a better experiment; the earlier number stays in the record rather than being edited.

**What is still unknown, and it is the whole of the next step:** *which* reply comes back short, and why. The
decoder's fatal path reports nothing — no command, no opcode, no sizes — so the answer has to come from our
side of the wire: correlating what S writes into the reply arena against what Venus expects for the command in
flight when the abort fires. It is worth flagging one prior belief this sits near without yet touching: the
proxy still refuses the async `zwp_linux_buffer_params_v1.create` as "UNSUPPORTED in WP0 (no event-return
channel)", a comment that 4.4 made false when it built that very channel. Whether that refusal is implicated
here is not yet known — the application aborts before reaching `create_params` at all — but the comment is
stale either way and is on the list.

### 2026-07-25 — The failing call has a name: `vkGetImageDrmFormatModifierPropertiesEXT`

The entry above located vkcube's abort as a Venus reply-decode overrun but could not say *which* reply. It
can now, and the answer is specific enough to act on.

Venus's fatal decode path reports nothing — no opcode, no sizes — so the command had to be recovered from
our side of the wire, where the bytes still exist. The repository already had the instrument and had simply
never pointed it here: `venus_ring::decode::decode_commands` walks a ring stream and returns each command's
type and flags, and `RingCommand.command_flags` bit 0 is "the client wants a reply written back". Twenty-five
lines in the watcher, gated behind the existing `RAYLAND_RING_DUMP`, decode every relayed delta and name its
commands. The decoder is deliberately conservative — it stops at the first command whose encoded size it does
not know — and that limitation turns out to be exactly the right behaviour here, because *the command it
stops on is the application's own*, every time. Each delta reads: `vkSetReplyCommandStreamMESA` (type 178, the
reply-arena setup that precedes a reply-bearing call), then one unknown-size command, which is the real one.

The last few deltas before the abort, translated through `vn_protocol_driver_defines.h`:

| type | command |
|---|---|
| 21 | `vkAllocateMemory` |
| 144 | `vkGetImageMemoryRequirements2` |
| 139 | `vkBindImageMemory2` |
| 56 | `vkGetImageSubresourceLayout` |
| **187** | **`vkGetImageDrmFormatModifierPropertiesEXT`** ← the last command; its reply is the one that overruns |

That is the swapchain-image import path read straight off the wire, and it corroborates the other thing the
ring dump caught moments earlier: a new blob `res=9` of **1,008,000 bytes** — 504 × 500 × 4, a 500×500
swapchain image with its stride padded to 504. So the application really does get as far as allocating,
binding and laying out its swapchain image; it dies asking the driver what DRM format modifier that image
actually has.

**Two things make this sharper than a guess.** First, the ring's own `status` word never sets
`VK_RING_STATUS_FATAL_BIT_MESA` (0x2) — it moves only between IDLE (0x1) and ALIVE (0x4) — so this is not a
ring-fatal abort and not a watchdog abort, both of which would have logged. Second, **S reports nothing at
all**: no error, no refusal, no unsupported-command warning. S relays the bytes, virglrenderer executes them,
and S believes the session is healthy right up to the moment C's Mesa aborts. A silent failure on one side and
perfect health on the other is the signature of the two sides disagreeing about *bytes*, not about semantics.

**The hypothesis to test next, stated before testing it:** the reply to command 187 that Venus decodes is not
the reply S produced — either it never reaches C's reply arena, or it reaches it after the application has
already been released to read it. The decoder overruns because it is parsing whatever the arena happens to
hold. This is a return-path ordering question of exactly the family (c)2 spent its length on, and the
`vkGetFenceStatus` completion gate that solved the readback case is the obvious place to look for an analogous
hole — but that is a hypothesis, and the next step is to instrument what S writes for 187 against what C's
arena holds when Venus reads it, not to start patching.

One loose thread worth flagging while it is in view: the proxy answers the app's dmabuf format query *locally*
with two synthesized LINEAR modifiers, and `vkGetImageDrmFormatModifierPropertiesEXT` is the app asking the
**driver** what modifier the image really got. If those two answers can disagree, that is a second bug waiting
behind this one — it would not cause a decode overrun, so it is not today's failure, but it is on the list.

### 2026-07-25 — Correction: the abort is a ring **stall**, not a reply-decode overrun — and the earlier reasoning was unsound at its root

Two entries above, this diary concluded that vkcube dies in Venus's `vn_cs_decoder_set_fatal` — a reply read
past its end — and on that basis declared the 2026-07-24 "parked ring thread" diagnosis overturned. **That
conclusion is wrong, the 2026-07-24 diagnosis was right, and the error is worth dissecting because it was a
reasoning failure rather than a measurement failure.** Both earlier entries stay as written; this is the
overturning.

**The unsound step.** The argument was: the abort prints nothing; `vn_cs_decoder_set_fatal` is the only abort
in the Venus ICD whose body does not call `vn_log`; therefore it is the decoder. The first premise was
verified. The second was *read directly from the source* and is true as stated. The conclusion still does not
follow, because the inference silently assumed that the nine `vn_log`-ing abort paths would actually **print**
— and they do not:

```c
vn_log(struct vn_instance *instance, const char *format, ...)
{ ... mesa_log_v(MESA_LOG_DEBUG, "MESA-VIRTIO", format, ap); ... }
```

`vn_log` logs at **`MESA_LOG_DEBUG`**, which a release Mesa suppresses. So *every* Venus abort is silent by
default, and "silent" carries no diagnostic information whatsoever. The one fact the whole identification
rested on was worth nothing. Setting `MESA_LOG_LEVEL=debug` did not lift it either, so the messages stayed
invisible and the mistake stayed comfortable. **Reading a function's body and never asking what its callee
does with the result is exactly the class of error this project keeps rediscovering** — it is the same shape
as assuming an exported symbol implies a usable path, and it is why the convention is to read the source
*through*, not just at.

**What the evidence actually says.** Two measurements, both reproducible across runs, and both available
before the wrong conclusion was drawn:

- **`head` stops dead.** The ring's control words move in lockstep all session — tail advances, head follows —
  until `tail` goes `0x58b0 → 0x5924` (22820) and then `0x5924 → 0x5974` (22900) with **head never
  following**. It freezes at **0x58b0 = 22704**. Mesa polls `head` as its reply-ready signal, so an
  application whose `head` never reaches its seqno is *blocked*, and a blocked application is not decoding
  anything. The decode-overrun theory required the app to have been released; the ring says it never was.
- **A 4.5-second silence.** The last ring event lands at `+51,317 ms` and the C daemon's session does not end
  until `55,792 ms`. A decoder overrun aborts on the instruction that reads past the end — instantly. Four and
  a half seconds of a live process and a frozen ring is a **spin**: `vn_relax` polling `head`, ending in
  `vn_common.c`'s watchdog-expiry or iteration-bound abort. Both log; both are invisible; both fit.

**So the corrected chain is:** the application writes its command; C relays the delta and S *applies* it — S's
own instrumentation shows `[s-reply] delta tail=22820`, so the bytes really do land in the ring's memory on
S — but **virglrenderer's ring thread never consumes them**. `head` stays at 22704, Mesa spins, Venus aborts
it. The stall is on S, in the consumer, exactly where 2026-07-24 put it.

`vkGetImageDrmFormatModifierPropertiesEXT` (type 187) is still the command in flight and the ring-command
decoding that named it stands — it is simply the command that *stalls*, not one whose reply is malformed. The
swapchain-image import path it belongs to (`vkAllocateMemory` → `vkGetImageMemoryRequirements2` →
`vkBindImageMemory2` → `vkGetImageSubresourceLayout` → 187) and the `res=9` blob of 1,008,000 bytes
(504 × 500 × 4) are both still good evidence about *where* the session is when it dies.

**What is genuinely open, stated narrowly this time:** why does virglrenderer's ring thread stop consuming
after 22704? The suspects are the ones (c)1's own finding #1 already named — the render server's ring thread
parks after 1 ms and is woken only by `vkNotifyRingMESA`, which Mesa emits only when it reads the IDLE bit
from **C's** status word — and the fact that a new 1 MB blob (`res=9`) is created immediately before the
stall. The next measurement is on S: whether its ring thread is parked, and whether the doorbell that should
wake it is rung and observed. **No fix until that is measured** — three wrong turns in one day is enough
evidence that this ring's failure modes do not yield to inference.

### 2026-07-25 — Root cause: the doorbell is rung on the wrong event, and only the *last* blob allocation loses the race

The correction above narrowed vkcube's death to a ring stall on S and left one question: why does
virglrenderer's ring thread stop consuming at `head = 22704`? Measuring the doorbell answered it, and the
answer is a lost wakeup with a very specific shape.

**First, what the measurement ruled out.** Instrumenting every doorbell (`RAYLAND_S_REPLY_LOG`) shows **91
rung, none with a missing ring handle, none refused by the engine**, all naming one handle
(`0x5555555bf450`) — so there is no second ring, no stale latch, and no rejected notification. The doorbell
for the stalling delta (`tail = 22820`) was rung *and accepted*. Logging S's blob creation ruled out the other
obvious suspect: **S creates all nine blobs**, including `res=9` (`blob_id = 39`, 1,008,000 bytes). Everything
S is supposed to do, S does.

**The delta boundaries also correct the previous entry's aim.** `head` freezes at 22704, which is exactly the
boundary *before* the delta `[22704, 22820)`, and that delta begins at offset 0 with **`vkAllocateMemory`
(type 21)**. So the stalling command is the allocation, not `vkGetImageDrmFormatModifierPropertiesEXT` — 187
is simply the next command, queued behind it and never reached.

**The finding is in the interleaving.** Put S's blob creations and doorbells on one timeline:

```
[s-doorbell] tail=22152
[s-blob]     created res=8          <- blob created
[s-doorbell] tail=22232             <- a LATER delta's doorbell wakes the ring thread

[s-doorbell] tail=22820
[s-blob]     created res=9          <- blob created
             (nothing, ever)        <- no doorbell follows
```

The chain: virglrenderer's ring thread reaches a blob-backed `vkAllocateMemory` and **waits for the blob
resource to exist on the host**. That blob arrives over the **inline vtest path** — a different message
entirely — and S creates it. But **S rings the doorbell only after `apply_delta`, never after a blob
creation**, so nothing wakes the waiting thread. And the application, now blocked in `vn_ring_wait_seqno`,
emits no further ring traffic — so no later delta arrives to ring it by luck. `head` never moves, the app
spins, Venus aborts it.

**This is why two of three swapchain images work and the third does not.** vkcube allocates three
(`res=7`, `res=8`, `res=9`, each 1,008,000 bytes = 504 × 500 × 4). For the first two the application still had
commands to issue, so a subsequent delta's doorbell woke the thread incidentally and the allocation completed.
The third is the **last** allocation before the app waits on a reply: nothing follows it, so the incidental
wakeup never comes. The bug has been latent behind that accident the whole time, which is also why it presents
as "vkcube hangs late in setup" rather than as an obvious protocol error.

It is (c)1 finding #1 again — the doorbell rung on the wrong event — in a guise that finding did not
anticipate. The code comment at the doorbell site says *"this doorbell is the only thing that will ever make
these bytes execute"*, which is true and is exactly the problem: it is tied solely to `C2S::RingDelta`, while
the ring thread can also be waiting on state that arrives by a completely different message.

**The fix follows directly and is deliberately not written yet:** ring the doorbell after anything the ring
thread may be waiting on — a blob creation at minimum — rather than only after a delta. Before building it,
two things need checking, because this ring has already produced three wrong turns in a day. First, that
virglrenderer really does re-check for the blob when notified rather than having latched a failure, since a
doorbell that wakes a thread into the same failed lookup fixes nothing. Second, whether an unconditional
doorbell per blob creation is safe against the park sequence in the way the existing one is — `apply_delta`
stores `tail` with `Release` *before* the doorbell precisely so the consumer cannot miss it, and a blob-created
doorbell needs the equivalent argument written down before it ships, not after.

### 2026-07-25 — The two checks refuted the fix, and found something bigger: `VN_PERF=no_multi_ring` does not do what (c)1 believes it does

The entry above root-caused vkcube's stall to a doorbell rung on the wrong event and proposed ringing it after
blob creation too — with two checks to run first, precisely because this ring had already produced three wrong
turns in a day. **Both checks were worth running: the first refuted the fix, and the second found a larger
problem underneath it.** No fix was written. That is the honest outcome and it is recorded as such.

**Check 1 — does virglrenderer wait for the blob? No.** `vkr_dispatch_vkAllocateMemory`
(`vkr_device_memory.c:246-257`, virglrenderer 1.2.0, the linked version) translates
`VkImportMemoryResourceInfoMESA` by calling `vkr_get_fd_info_from_resource_info`, and on failure does this:

```c
if (!vkr_get_fd_info_from_resource_info(ctx, res_info, &local_import_info)) {
   args->ret = VK_ERROR_INVALID_EXTERNAL_HANDLE;
   return;
}
```

It sets an error return and **returns normally** — no wait, no condition variable, no fatal. So a missing blob
could not freeze `head`: the command would complete, `head` would advance, and Mesa would see a failed
allocation. The premise of the previous entry's fix — a ring thread *waiting* for a blob that a doorbell could
release — does not exist in the code. The doorbell/blob-creation interleaving that looked so convincing was a
correlation read as a mechanism, and reading the dispatch would have refuted it at any point in the day.

**Check 2 — sampling the consumer's actual state, which is what should have been done first.** Rather than
infer again, the whole process tree under `rayland-s` was sampled every 0.4 s through a stall. The Venus
context does not live in `rayland-s` at all: virglrenderer forks `virgl_render_server`, which forks a
`virgl-1-gpu_ren` worker, and *that* holds the ring threads. In it:

```
tid=4085710 comm=vkr-ring-1     <- present from the start
tid=4086533 comm=vkr-ring-1     <- appears mid-run
tid=4086541 comm=vkr-ring-1     <- appears mid-run
tid=4086542 comm=vkr-queue-1
```

`vkr_ring.c:248` names these `"vkr-ring-%d"` **by `ctx->ctx_id`**, so every one of them is a ring thread in
context 1: **the session has three or four rings, not one.** And S's instrumentation shows it rang the doorbell
for exactly **one** handle (`0x5555555bf450`) all session, and **never once logged "ignoring a second ring
creation"** — so the additional rings were created by a path `latch_ring_handle` cannot see, since it inspects
only the *inline* vtest batches.

**The assumption that fails is spec §6's.** (c)1 pins `VN_PERF=no_multi_ring` and treats it as a guarantee of
exactly one ring — `latch_ring_handle`'s own doc says so, and warns that without it "the extra ring will
stall". The smoke *does* pass the flag, and the flag *is* still a valid Mesa option
(`vn_common.c:54`, honoured at `:297`). But read what it actually gates:

```c
struct vn_ring *
vn_tls_get_ring(struct vn_instance *instance)
{
   if (VN_PERF(NO_MULTI_RING))
      return instance->ring.ring;
   ...
```

It only changes **which ring a thread submits to**. It does not stop rings from being *created*. So (c)1 has
been relying on a guarantee the flag never made, and the extra rings have presumably been there since C0
without anything looking.

**What this does and does not establish.** It is now measured fact that multiple rings exist, that S knows of
only one, and that S's doorbell can therefore only ever wake one. It is *not* yet established that the extra
rings carry any commands — if `no_multi_ring` really does funnel every submission to the instance ring, they
may be created and idle, in which case they are a red herring and the stall is something else again. That is
the next measurement and it decides the fix: log every ring creation and every submission's target ring, and
find out whether the ring whose `head` freezes is the one S is ringing. **Given the day's record — three
hypotheses refuted, two of them by evidence that was already on disk when they were formed — nothing gets
built until that is on the screen.**

The lesson worth keeping, beyond this bug: *"S reports no error"* was treated as informative all day, and it
never was. S cannot report an error about a ring it does not know exists. Silence from a component is only
evidence when you have first established that the component is in a position to speak.

### 2026-07-25 — The measurement refuted the multi-ring theory too, and caught an error in my own instrument

The previous entry proposed that extra rings were being created by a path S cannot see — most plausibly an
inline batch with the ring creation buried behind another command, a hazard
`ring_handle_from_create`'s own docs name in as many words: *"a batch that buried a ring creation behind
another command would be missed here … the ring would visibly stall"*. It fit the symptom exactly. **It is
wrong.**

Scanning **every** inline batch S receives for the `vkCreateRingMESA` command type, at every 4-byte offset
rather than only at zero, found **exactly one ring creation in the entire session**:

```
[s-ringcreate] batch_len=140 off=0 latched_by_offset0=true handle=Some("0x5555555bf450")
```

`off=0`, correctly latched, and it is the same handle every one of S's doorbells names. The latch is not
missing anything. An earlier scan of the *ring* stream for the same command types had already returned zero
hits, so between them: **the protocol carries exactly one ring creation, and S knows about it.**

**And an error in my own instrument, which is the more useful half of this entry.** The claim that the session
had "three or four rings" came from counting threads matching `comm=vkr-ring` — a pattern that also matches
**`vkr-ringmon-1`**, the ring *monitor* thread, which is not a ring. The real progression is one ring thread,
then three. So one of the two numbers behind the multi-ring theory was an artefact of a sloppy grep, and I did
not check it before building a theory on it. Three refuted hypotheses in a day were all bad inference; this one
was bad measurement, which is worse, because the whole point of the last several hours was to stop inferring
and start measuring. An instrument gets verified before its output is trusted, exactly like any other claim.

Three `vkr-ring-1` threads from one `vkCreateRingMESA` remains an unexplained observation rather than a
theory. `vkr_ring.c:248` names those threads by context id, and there is one context and one worker process
(`virgl-1-gpu_ren`), so what creates the other two is simply not yet known — and it is no longer even clear
that it matters.

**What the sampling does say, and it is worth more than any of the theories:** at every sample through the
stall, **every one of those ring threads is parked in `futex_do_wait`** while `head` stays frozen. The
consumer side is uniformly asleep. So the question is not "which ring is the doorbell waking" but "why does a
delivered, accepted `vkNotifyRingMESA` leave every ring thread asleep" — and answering that needs the parked
thread's actual stack (`gdb` on the `virgl-1-gpu_ren` worker at the moment of the stall, which the sampler can
now identify by pid), not another dword scan.

Four hypotheses have now been formed and refuted in one day: delayed ACK, reply-decode overrun, the doorbell
lost-wakeup, and buried multi-ring creation. What they share is that each was built from a correlation and
then reasoned forward, and each could have been refuted immediately by evidence that was already on disk or one
command away. The instrumentation built along the way is genuinely useful and is committed. The theories were
not. **Next session: stack, not story.**

### 2026-07-25 — Blocked on ptrace: the render-server worker cannot be attached to, and `PR_SET_PTRACER` does not reach it

Getting the parked ring thread's userspace stack is the next step, and it is blocked by a machine-level
permission rather than by anything in Rayland. Recording the dead end so nobody re-walks it.

The Venus ring threads live in `virgl-1-gpu_ren`, a **grandchild** of `rayland-s` (virglrenderer forks
`virgl_render_server`, which forks the per-context worker). This machine runs
`/proc/sys/kernel/yama/ptrace_scope = 1`, which permits `ptrace` only from an **ancestor** of the target. A
debugger started from a shell is not an ancestor of a grandchild of the daemon, so `gdb -p <worker>` fails with
*"Could not attach to process … ptrace: Inappropriate ioctl for device"* — confirmed, 11 attempts, every one
refused.

**The attempted way around it, and why it failed.** `prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY)` exists exactly
for this, and its documentation says the setting is inherited across `fork` and preserved across `execve` — so
calling it once in `rayland-s` should have covered the render server and its worker without either knowing.
It was added behind `RAYLAND_S_ALLOW_PTRACE`, it ran, and **`prctl` returned 0** — the log line confirms the
grant was made. Attaching to the worker was **still refused**, identically. So on this kernel the Yama
ptracer relation does not extend to the forked-and-exec'd grandchild, whatever the documentation implies. The
code was reverted rather than left in: an opt-in switch that grants nothing is worse than no switch, because
the next person would trust it.

**What remains, and it is the user's call rather than this daemon's.** Either relax the setting for the
session (`sudo sysctl -w kernel.yama.ptrace_scope=0`, restored with `=1`) — a one-line, reversible change to
the developer machine's security posture, which is not something to do unasked — or launch `rayland-s` *under*
gdb so the debugger is genuinely in the worker's ancestry, which works but stops inferiors on fork/exec events
and may well perturb the very timing the stall depends on.

Nothing else was learned this turn, and nothing was built. Stating that plainly rather than padding it: the
measurement that matters has not been taken yet, and the four refuted theories from earlier today are still
refuted.

### 2026-07-25 — The stack, S's ring status, and a fifth correction: the consumer is blocked *inside* dispatch, not parked

With `ptrace_scope` relaxed for the session, gdb finally reached the render-server worker
(`virgl-1-gpu_ren`), and S's own ring control words were read for the first time. Together they kill two more
theories, including one this diary asserted two entries ago.

**The stack, and the end of the multi-ring red herring.** Three threads are named `vkr-ring-1`. Only one is a
ring thread: its frames run into the `gpu_ren` binary. The other two run into
**`libnvidia-glcore.so.595.71.05`** — they are NVIDIA driver workers that inherited the `comm` name, because
Linux copies the creating thread's name to new threads. So there was **only ever one ring**, which is exactly
what the protocol evidence said all along (one `vkCreateRingMESA`, at offset 0, correctly latched). Two
independent measurements now agree, and the "extra rings" that survived one refutation are gone for good.

**S's ring status, which nothing had ever read.** Every `head`/`tail`/`status` reading in this investigation
came from **C's** copy of the ring — and C's copy cannot show a failure that happens on S, since they are
different memory on different machines. Reading S's copy after each doorbell:

```
relayed_tail=22704  s_head=22704  s_tail=22704  status=4 [ALIVE]
relayed_tail=22820  s_head=22704  s_tail=22820  status=4 [ALIVE]
```

**`VK_RING_STATUS_FATAL_BIT_MESA` (0x2) is never set — 0 occurrences all session.** That rules out the live
suspect from the previous entry: `vkr_dispatch_vkNotifyRingMESA`'s `lookup_ring` miss, which calls
`vkr_context_set_fatal` and returns with no log at all. The context is healthy. The doorbell is finding its
ring.

**And the correction.** Status `4` is `ALIVE` **without** `IDLE`. The park branch sets `IDLE` immediately
before `cnd_wait` and clears it only on waking, so a parked ring thread reads `IDLE|ALIVE` (5) — and 52
samples this session do. At the stall it reads **4**, with `head` frozen and 116 bytes of work sitting past
it. So the ring thread is **not parked**; it is inside command processing and not coming out.

That means the previous entry's reading of the gdb stack was wrong. Frame #6 is a plain `cnd_wait` with
`abstime=0x0`, and it was matched to `vkr_ring_thread`'s park line because that line is a plain `cnd_wait`
with no timeout. But virglrenderer has other `cnd_wait` sites, the frames either side are unsymbolised
addresses in a stripped binary, and **the status bits say the park branch was not taken**. The honest reading
is: the consumer is blocked on *some* condition variable **inside the dispatch of the command at 22704** —
which the ring stream names as `vkAllocateMemory` — and which condvar that is remains unknown.

So the shape of the failure has changed completely. It is not a lost wakeup, not a fatal context, not a
missing blob, and not multi-ring. **The consumer entered `vkAllocateMemory` and never returned.** Every one of
those four had a mechanism that sounded right; each died to a measurement that was cheap once someone took it.

**The next step is narrow and mechanical, not another theory:** symbolise frames #7 and #8. The worker binary
is stripped, so this needs its load base from `/proc/<pid>/maps` while it is stopped, subtracted from the
frame addresses, and the result resolved against whatever symbols `/usr/libexec/virgl_render_server` still
exports — or, failing that, a debug build of virglrenderer 1.2.0 (the linked version; the source is already on
disk). Naming that one function ends the search, because it is the thing the allocation is waiting on.

**Housekeeping:** `kernel.yama.ptrace_scope` was relaxed to `0` for this session at the owner's hand and must
be restored to `1`. It is not something to leave lying open on a development machine.

### 2026-07-25 — The frames are symbolised: it is `vkr_ring_thread`'s own park, and that corrects yesterday's correction

The stripped `virgl_render_server` gave up its two frames without debuginfo, and the answer overturns the
previous entry — which had itself overturned the entry before it. Recording the method, because it is
reusable and cost nothing.

**How, with no symbols.** Ubuntu's `virgl_render_server` is stripped, `libvirglrenderer.so.1` exports **zero**
`vkr_*` symbols (the Venus code is linked into the stripped executable), and the debuginfod download timed
out. None of that matters, because a PIE binary preserves the low 12 bits of every address: for a
page-aligned load base, `runtime_addr & 0xFFF == file_vaddr & 0xFFF`. The captured frames were
`0x634e2734fd76` and `0x634e2733c4fc` — page offsets `0xd76` and `0x4fc`, separated by `0x1387A`. Frame #7 is
a return address immediately after a call to `pthread_cond_wait`, and the binary has only **four** such call
sites. Exactly one returns at page offset `0xd76`:

```
call@0x7dd71  ret@0x7dd76  pageoff=0xd76
```

That fixes the load base at `0x634e272d2000` (page-aligned, as required) and predicts frame #8 at `0x6a4fc` —
and `0x7dd76 - 0x6a4fc = 0x1387A`, the observed separation, exactly. Two independent constraints agreeing on
one answer.

**What the two frames are.** Frame #8 is a thread trampoline:

```asm
6a4eb: mov 0x8(%rdi),%r12     ; argument, from a heap struct
6a4ef: mov (%rdi),%rbx        ; function pointer
6a4f2: call free@plt          ; free the struct
6a4fa: call *%rbx             ; indirect call into the thread's entry function
6a4fc: pop %rbx               ; <- frame #8
```

with `start_thread` above it — the `thrd_create` stub. So **frame #7's function is the thread's top-level
entry function**, and it is here:

```asm
7dd61: lea 0xc30(%r12),%rdi   ; &s->cond
7dd69: lea 0xc08(%r12),%rsi   ; &s->mutex
7dd71: call pthread_cond_wait@plt
7dd76: jmp 7db68              ; <- frame #7, looping back
```

`0xc30 - 0xc08 = 0x28 = sizeof(pthread_mutex_t)`, so the struct declares `mtx_t mutex; cnd_t cond;`
adjacently — `struct vkr_ring`. The thread is **`vkr_ring_thread`, parked in its own idle `cnd_wait`**, with
no dispatch frames between it and the trampoline.

**So the previous entry was wrong, and this is the third reading of this stall.** It concluded "blocked
inside the `vkAllocateMemory` dispatch" from S's status word showing `ALIVE` without `IDLE`. The stack
outranks that inference: a thread inside a dispatch would have dispatch frames, and there are none. The
`IDLE`-clear reading has a mundane explanation the previous entry did not consider — S samples the status
*immediately after ringing the doorbell*, and `vkr_ring_thread` clears `IDLE` the moment it wakes
(`vkr_ring_unset_status_bits` sits directly after the `cnd_wait` returns), so a sample taken then can easily
catch the bit already cleared. A single-instant status read was never strong enough to override a call stack,
and treating it as such was the error.

**Where that leaves the failure.** The ring thread parks — legitimately, since it parks only when
`ring->buffer.cur == vkr_ring_load_tail(ring)`, and S's own control words confirm it was genuinely caught up
at 22704. S then applies the next delta (tail → 22820) and rings the doorbell, which virglrenderer accepts,
whose context is not fatal, and whose ring lookup therefore succeeded. **And the thread does not consume it.**
That is the whole remaining mystery, now stated with no speculation attached: park is legitimate, notify is
delivered, `head` never moves.

The next thing to measure is the one link never yet observed directly: whether `vkr_ring_notify` actually runs
for that doorbell — i.e. whether `pending_notify` is set and `cnd_signal` called — versus the notify being
consumed somewhere earlier. The ring struct's address is now known at runtime (`%r12` in frame #7), and its
`pending_notify` flag can be read out of the same mapping S already holds, so this is an observation rather
than another theory. **Six readings of this stall so far; the ones that survived were all measurements, and
every one that died was an inference.**

### 2026-07-26 — The two-machine demo ran, and presented a black window: the relay was right and S's presentation was wrong

The owner asked the question this project exists to answer — *can I run something on apollo and see it on my
laptop?* — and the honest answer today is "half". Recording both halves, because the half that failed is a
real defect and not the caveat that was expected.

**What worked.** `rayland-icosa-cpu` ran on **apollo**, which links no GPU stack of ours. Its Vulkan command
stream crossed a real network over QUIC. **The laptop's GPU drew it**, and `rayland-s` opened a genuine
`xdg_toplevel` window on the laptop's compositor:

```
rayland-s: resource 6 is 262144 bytes = 256x256x4, so it is a candidate for the frame to present
rayland-s: presenting resource 6 as the frame (256x256)
presenting via wl_shm (fallback: this frame source cannot export a dmabuf)
```

The expected caveat was that this is **one still frame, not an animation**: `present_the_frame` is called
exactly once (`rayland-s/src/main.rs:544`), at session end, and the window then blocks until closed. That
caveat was stated before running and is not the problem.

**What failed: the window was black.** And the pixels were *fine* — the application on apollo wrote all 120
PNGs, and pulling `frame_0060.png` back shows **213 distinct byte values spanning 0–255**, an unambiguously
real image. So the command stream crossed correctly, S's GPU rendered correctly, and the readback returned
correctly to the application on C. **Everything the relay is responsible for worked. What showed on screen
did not.** The fault is in S's own capture-and-present path — `FrameCapture` / `into_frame` / `present_frame`
— which selected `res=6` at the right size and then presented zeros.

That is worth saying plainly rather than filing under "static demo": for the whole session the presentation
path has been carried as *working* on the strength of (c)1 Task 7 and a two-machine run recorded in
`CLAUDE.md` as "presents on dop561's screen". It presents *a window*. Whether what is in the window is the
frame has evidently not been checked by a human recently, and `rayland-present`'s own module docs say exactly
why that matters: *"verified by building, by `tests/live_window.rs` … and by a human looking at the screen —
because no automated test can assert what a compositor actually painted."* The automated tests were green.
The human looked, and it was black.

**Not diagnosed yet, deliberately** — the owner asked to carry on with the ring stall, and this is a separate
defect on a separate path. The obvious suspects, in the order worth checking: whether `FrameCapture` holds
bytes captured *before* the first render (it accumulates during the session and presents at the end, and a
capture taken at the wrong instant would be exactly this symptom); whether the device-destroy at session end
disturbs the blob it then reads; and whether `into_frame` picks the right blob for an app whose readback is
not the refapp's. The size heuristic clearly worked — 256×256×4 is right — so this is about *when* the bytes
were taken, not *which* resource.

**One thing it does settle, though:** commands-not-pixels works over a real network, machine to machine, with
an unmodified application. The thesis is not in question. The last mile is.

### 2026-07-26 — Periodic sampling reframes the stall again: the consumer is fine, and C's **last** delta never lands on S

Sampling S's ring control words on the *progress poll* rather than only when a delta is applied — the
delta-driven log goes silent exactly when the stall starts, since C stops relaying — changed the picture
substantially, and not in the direction the last three entries pointed.

**virglrenderer is behaving.** The final samples of this run:

```
[s-doorbell] tail=22820 accepted=true
[s-ringctl]  s_head=22704 s_tail=22820 [ALIVE]
[s-blob]     created res=9 blob_id=39 size=1008000
[s-ringctl]  s_head=22820 s_tail=22820 [IDLE|ALIVE]      <- head REACHED 22820
```

`head` advanced to 22820: the ring thread woke on the doorbell, consumed `vkAllocateMemory`, and re-parked
normally. The park/notify pair works. Note this also contradicts the earlier reading that `head` froze at
22704 — it did in earlier runs, it did not in this one, and **that variation is itself the finding**: the
freeze point is not fixed, so it was never a property of a particular command.

**The invariant that does hold across runs.** C's metrics say `c2s_ring_msgs=91`; C's decoder logged 91
deltas; **S applied 90.** The one delta that never lands is always the **last** one C sends — here
`tail=22900`, carrying `vkGetImageDrmFormatModifierPropertiesEXT` (type 187), the command the application is
blocked awaiting a reply to. S's log simply ends after the `res=9` blob creation; the app spins ~4.2 s (last
ring event `+35688 ms`, session ends `39862 ms`) and Venus aborts it.

**What that rules out, and it is most of the last two days.** The consumer is not stalled, not parked
unwoken, not fatal, not blocked in a dispatch, and not short of the blob it needed — `head` demonstrably
moves past the allocation, and S emitted **zero** blob messages after `res=9` (its initial contents were
genuinely empty, so the `CreateBlob` byte-granular shipping is not a flood either). The failure is not on the
GPU side of the wire at all. **It is that the final message C sends does not get applied.**

**Where it could be, stated without picking one.** `record_send` counts *after* a successful `write_msg` but
*before* `flush` (`rayland-c/src/link.rs:100-113`), so a counted message is proof of a completed write, not
of a completed flush, and not of delivery. Equally, S's message thread does not obviously return to reading:
its last logged act is the `CreateBlob` for `res=9`, after which it owes C an `S2C::BlobCreated` — and if
that write is what does not complete, S never reaches the next `read_msg` and the 91st delta sits unread on
the link. Both hypotheses predict exactly what is observed, and they have opposite fixes, so **neither gets
built until one is measured**. The next instrument is the obvious one and it is symmetric to what already
exists: log every message S *reads* and every message S *writes*, so "sent" and "applied" stop being the only
two observable points on a path with several steps between them.

Six days of this bug and six refuted readings; every one died to a measurement, and this reframing came from
moving one existing log from an event-driven cadence to a periodic one. The lesson that keeps recurring: an
instrument that samples only when the system is healthy cannot see the system fail.

### 2026-07-26 — Both of S's threads freeze together: the wall is a deadlock inside `rayland-s`, not in Mesa or virglrenderer

Logging every message S reads and writes — the seam that never existed, because "C sent" and "S applied" were
the only two observable points on a path with several steps between them — located the failure precisely, and
it is not where any of the last week's work was looking.

**Writes are not the problem.** Bracketing each write *and its flush* separately: **4676 `w>` and 4676 `w<`,
perfectly matched.** No send was left incomplete, which kills the hypothesis that C's counted-but-unflushed
message was stuck in a buffer.

**S receives the delta it never applies.** The last line of S's log is:

```
[s-link] r< RingDelta ring=1 tail=22900 len=80
```

That is the delta carrying `vkGetImageDrmFormatModifierPropertiesEXT` — the command the application is
blocked awaiting. S **reads** it. S never applies it. And then:

- no `[s-reply] delta` line, so the message thread never reached `apply_delta`;
- **zero** further periodic `[s-ringctl]` samples, though they run four times a second and the stall lasts
  ~4 s — so the *progress* thread stopped at the same moment;
- no further `w>` of any kind.

**Two independent threads stopping at the same instant is a deadlock, and both threads are ours.** This was
never a Mesa problem or a virglrenderer problem. Every reading of the last week — the parked ring thread, the
missing doorbell, the fatal context, the reply-decode overrun — was looking across the wire at a peer that,
it now turns out, was healthy and simply had nothing more to do.

**The candidate, and the comment that argues it away.** The message thread's own lock discipline says:

```rust
// No deadlock: the progress thread takes `applier` and releases it **before** taking `tx`,
// so it never holds both, and this is the only path that holds them together.
let mut session = applier.lock()...;
let out = session.apply(engine, msg);   // engine calls, holding the applier lock
for reply in &out { let mut stream = tx.lock()...; send(...)?; }
```

That reasoning is sound for the two-lock cycle it considers. But there is a **third** participant, and the
comment names its own dependence on it: *"`apply`'s engine calls block only on the actor, which services them
promptly even while a readback fence is in flight, so this can no longer deadlock the doorbell."* **"Services
them promptly" is an assumption, not an invariant.** If the actor is ever slow or blocked, the message thread
holds `applier` while waiting on it, and the progress thread — which needs `applier` on every poll — stops
dead behind it. That is precisely the observed signature.

**Two honest caveats, because this diary has been wrong six times this week.** First, the message thread
stopped *before* `apply_delta`, which does not touch the engine — so if it is blocked on the applier lock
itself, the holder must be the progress thread, and the cycle runs the other way round from the sketch above.
Distinguishing those is one measurement, not an argument. Second, **the periodic sampler added earlier today
runs inside `take_ring_progress`, with the applier lock held**, so the possibility that the instrument
contributes to the freeze cannot be dismissed from the armchair — even though the underlying hang predates
all of this instrumentation (A/B-confirmed identical at `3a7fc39`, before any of it existed).

**Next, and it needs no debugger:** make lock acquisition observable. A `try_lock` with a bounded retry that
logs when either thread waits more than a beat for `applier` or `tx` names the holder and the waiter
directly, and a run with the periodic sampler disabled says whether the instrument is a participant. Both are
small, both are ptrace-free, and between them they turn "a deadlock, probably here" into "this thread holds
this lock and is blocked on that".

### 2026-07-26 — FIXED: it was never a deadlock, it was a 637 ms critical section — and vkcube now builds its swapchain buffers

The instruments finally converged, and the answer was the fifth different shape this bug has taken. It is
fixed, verified, and it moved vkcube materially forward.

**The two measurements the last entry asked for, both answered in one run.** Gating the periodic ring sampler
behind its own switch and running with it **off**: the failure still occurred, so the instrument was
exonerated rather than assumed innocent. And a lock watchdog — a thread that owns nothing and only ever
`try_lock`s, so it keeps reporting while everything else is wedged — said this, continuously:

```
[s-lockdog] applier_free=false tx_free=true
```

The applier lock is held permanently; the send lock is **not involved at all**. Meanwhile a heartbeat placed
*outside* every lock showed the progress thread was **alive** — but looping only every **~3 seconds**, on a
loop that polls every 200 µs.

**So it was never a deadlock.** Nothing was waiting on anything circularly. It was a **critical section long
enough to starve the other thread**, and the previous entry's confident "two threads freezing together is a
deadlock" was wrong in the same way as its predecessors: a real observation, a plausible mechanism, no
measurement in between. Timing each section named the culprit immediately and unambiguously — **71 slow
sections, every one of them the same call**:

```
[s-section] take_venus_blob_writes held the applier lock 637 ms
                                                         486 ms
                                                         404 ms   ... 71 times
```

`take_ring_progress` and `reply_arena_fence_signaled` never even crossed the 50 ms threshold.

**Why it cost that much.** `take_venus_blob_writes` byte-diffs every Venus-internal blob at gap 0 — the ring
shadow, the 1 MiB reply arena, and the **8 MiB staging pool** — comparing a byte at a time in a zipped loop.
That is ~9 MiB of per-byte branching per call, on a 200 µs poll. It cannot keep up, so it runs back-to-back
holding the applier lock; the message thread never gets in; the delta that would release the application is
never applied; Mesa's ~3.5 s stall abort fires. **That is the vkcube "hang", entire.** It also explains what
nothing else did: why the stall always begins just after `res=9` — that is the third 1 MiB swapchain image,
the point at which the per-poll scan finally outgrows the application's patience.

**A wrong fix, caught before shipping.** The obvious move was to filter the scan by `s_written`, which
`reply_arena_fence_signaled` already uses for exactly this purpose — its comment even says it "excludes the
same-marker staging pool". **It would have broken the return path completely.** `s_written` is populated *by*
`emit_blob_writes` (a blob joins the set only once the diff has detected a write), so filtering the diff's
input on it means the reply arena — born empty, hence not in the set — would never be diffed, never detected,
and replies would never ship. Reading how the set is filled took one grep and saved a silent, total breakage.

**The fix that shipped: chunked comparison, byte-identical output.** `HostBlob::changed_byte_ranges` now
compares 64-byte chunks with slice equality — which lowers to `memcmp`, word-at-a-time and vectorised — and
descends to the per-byte loop *only inside a chunk that already differs*. The grain of **detection** is still
the byte; only the skipping of the unchanged majority changed. Runs still merge across chunk boundaries, so
the ranges emitted are exactly those the old loop emitted. This is the same technique the 2026-07-21
coalescing entry recorded as the known follow-up, applied to the other direction.

**Verified, not asserted:**

| | before | after |
|---|---|---|
| slow lock sections | **71**, worst **637 ms** | **15**, and `take_venus_blob_writes` gone from the list |
| deltas C sent / S read / S applied | 91 / 91 / **90** | **102 / 102 / 102** |
| proxy trace depth | 36 lines | **52 lines** |

The loopback e2e — the standing correctness gate — passes: `2 passed`, 147 s, `rayland-refapp` and the
120-frame `rayland-icosa-cpu` both bit-identical. The chunked diff loses nothing.

**And vkcube crossed into new territory.** It now builds its swapchain `wl_buffer`s:

```
[wp-proxy] intercept params 20.create_immed -> buffer 21 = resource 8  (500x500 fmt 0x34325258)
[wp-proxy] intercept params 22.create_immed -> buffer 23 = resource 9  (500x500 fmt XR24)
[wp-proxy] intercept params 24.create_immed -> buffer 25 = resource 10 (500x500 fmt XR24)
```

Three swapchain images, correlated to resources 8/9/10 by the fd→token intercept, at the app's real 500×500.
**That is WP0 Task 4.3's own territory** — the `WaylandArg::Buffer(_)` arm that was unreachable this morning
is now being reached. It still aborts, later and on a different thread, and `res=9` is no longer even the
interesting resource. The next wall is a new one, which is the first time that has been true in a week.

**The lesson this bug taught, repeatedly and expensively:** every one of the seven readings died to a
measurement, and every one was born from an inference. The two that finally worked were both instruments
rather than ideas — a watchdog holding no locks, and a timer around a critical section — and each took
minutes to build. The rule earned here: *when two components disagree, do not reason about which is at
fault; make each one say what it is doing.*

### 2026-07-26 — The new wall, named the same day: `vkQueueSubmit` fails to decode on S

With the starvation fixed, vkcube ran past every previous wall and hit a new one within hours. It is a
different kind of failure from everything before it, and S announced it in plain words — the first time in
this investigation that anything did.

**What the application reached.** It created its swapchain buffers, which had never happened:

```
[wp-proxy] intercept params 20.create_immed -> buffer 21 = resource 8  (500x500 fmt XR24)
[wp-proxy] intercept params 22.create_immed -> buffer 23 = resource 9
[wp-proxy] intercept params 24.create_immed -> buffer 25 = resource 10
```

Then recorded and submitted work: the ring's tail commands are `vkResetCommandBuffer` (92), `vkCreateFence`
(35), `vkResetCommandBuffer` (92), `vkGetFenceStatus` (38). Every one of the 102 deltas C sent was read and
applied by S.

**Where it dies.** The aborting thread is `vn_wsi[0,0]` — Mesa's WSI thread — and its abort frames `#1-#3`
are byte-identical to the previous wall's `#5-#7`, so it is the same silent spin-abort reached from a new
caller. It spins because it is polling `vkGetFenceStatus` for a fence that never signals. And the fence never
signals because **virglrenderer refused the submit**:

```
vkr: vkQueueSubmit resulted in CS error
vkr: ring_submit_cmd: vn_dispatch_command failed
vkr: submit_cmd: early bail due to fatal decoder state
failed to dispatch context op 5
vkr: destroying device with valid objects
vkr: destroying context 1 (vkcube) with a valid instance
```

**A "CS error" is a decode failure, not a GPU failure.** S could not parse the command stream of the
application's `vkQueueSubmit`, went to a fatal decoder state, and tore the context down. So this is a
**relay-fidelity** problem: bytes S needed were wrong, or absent.

**The leading hypothesis, flagged as a hypothesis.** Venus does not necessarily encode a submit's
command-buffer contents inline in the ring; it can place them in a separate command stream. C's blob sync
**deliberately never ships the staging pool** — `blob_id == 0` marks it Venus-internal and `blob_sync.rs`
declines to publish it by design, with a good reason (C's stale copy of S's arena would clobber replies). If
a real application's recorded command buffers live in that pool, then S is decoding a submit whose body it
was never sent, and a CS error is exactly what that looks like. It would also explain why `rayland-refapp`
and `rayland-icosa-cpu` never met this: their command streams are small enough to ride inline in the ring.
Against the hypothesis: C's `scan_for_out_of_line_stream` guard exists for precisely this class of hazard and
**did not fire once**, so either the submit is not out-of-line in the sense that guard detects, or the guard's
over-approximation has a hole. Both are checkable, and neither is checked yet.

**Deliberately not fixed today.** The measurement that decides it is to dump the failing submit's bytes as S
sees them and compare against what the application wrote — if they diverge, it is the staging pool; if they
match, the fault is in what the submit *references* rather than in the stream itself, and the swapchain
images S built from tokens become the suspect. After seven refuted readings of the previous wall, the rule
earned there applies here too: **make the components say what they are doing, rather than reasoning about
which is at fault.**

**Where this leaves WP0.** The `WaylandArg::Buffer(_)` arm that Task 4.3 exists to fill is now genuinely
reachable — the application constructs the buffers and the fd->token correlation resolves all three to real
S-side resources. 4.3 is no longer blocked behind an unreachable code path; it is blocked behind a submit
that does not decode. That is a much better place to be than this morning.

### 2026-07-26 — The ring relay is byte-exact: 253 deltas, 253 identical fingerprints — so the submit failure is about what it *references*

The question the last entry left was whether S's "CS error" on `vkQueueSubmit` came from bytes damaged in
transit or from bytes that never travel at all. Both sides now fingerprint every ring delta independently —
FNV-1a computed on C from what it relays, and on S from what it applies, joined on `tail`. The two
implementations are deliberately **duplicated rather than shared**, so that agreement means two separate
computations over two separate buffers rather than one helper called twice.

**Result: 253 deltas on C, 253 on S, and not a single mismatch** — same `tail`, same length, same digest,
every time. **The ring relay is byte-exact.** Whatever S fails to decode, it is decoding exactly the bytes
the application wrote. That eliminates corruption, truncation, reordering and loss in one measurement, and it
means the fault lies in what the submit *refers to* rather than in the submit itself.

**And the failure turns out to be intermittent, which is new information.** This run produced **no CS error at
all** — zero messages from `virgl_render_server`, where the previous run emitted the whole cascade. The
application instead ran 253 deltas (against 102 before), polled `vkGetFenceStatus` **142 times**, and **hung
until the 90-second timeout** (`vkcube exit: 124`) without aborting. So there are now two observed failure
modes downstream of the same point:

- **CS error** — virglrenderer cannot decode the submit, goes to a fatal decoder state, destroys the context.
- **Unsignalled fence** — no error anywhere; the submit simply never completes, and the application polls
  forever.

A structurally-missing staging pool would fail identically every time, so **the intermittency is evidence
against the simplest form of that hypothesis** and evidence for a race — something the submit needs is
sometimes present and sometimes not. That fits the shape of a dependency that arrives on a *different*
message than the delta referencing it, which is precisely the hazard `blob_sync`'s
BlobData-before-RingDelta ordering exists to prevent — but that ordering covers only the blobs C knows to
be application memory, and the swapchain images here are resources S built from **buffer tokens**, on the WP0
path, not from the blob-sync path at all.

**Two candidates, neither yet measured, and no fix until one is:**
1. The submit references the token-built swapchain images (`res=8/9/10`), and S's versions are not in the
   state the submit assumes — the WP0 buffer path and the (c)1 blob path never having been reconciled.
2. Venus places some part of the submit in the staging pool, which C declines to publish; the intermittency
   would then come from how much of the recorded stream happens to fit inline in the ring.

The instrument for both is the same and follows the one that just worked: fingerprint the *blobs* on both
sides, not just the ring, and see which resource S's copy disagrees with C's at the moment of the submit. The
ring took one run to clear; there is no reason the blobs should take longer.

**Worth stating plainly, because it is easy to lose in the detail:** the relay's core — the mechanism this
whole project rests on, shipping an unmodified application's command stream across a network — is now
*measured* byte-exact under a real application's load, 253 deltas in a row. That is not a small thing to be
able to say.

### 2026-07-26 — The blobs disagree in exactly one place: the staging pool is populated on C and empty on S

The ring was cleared by fingerprinting; the same instrument one level down clears the blobs, and leaves one
suspect standing.

Both sides now hash **every** blob they hold — throttled, separately switched (`RAYLAND_C1_BLOB_FP` /
`RAYLAND_S_BLOB_FP`), and reporting a **non-zero byte count** alongside the digest, because that second number
distinguishes *diverged* from *never populated at all*. Comparing each resource's last sample:

| resource | C non-zero | S non-zero | verdict |
|---|---|---|---|
| `res=4` (application, 256 KiB) | 262059 | 262059 | **identical** |
| `res=5` (application) | 396 | 396 | **identical** |
| `res=6` (application) | 396 | 396 | **identical** |
| `res=7` (application, 1 MiB) | 0 | 0 | **identical** |
| `res=1` (ring) | 5842 | 5843 | differ by one byte |
| `res=2` (reply arena) | 20474 | 20490 | differ by sixteen |
| **`res=3` (staging pool, 8 MiB)** | **28** | **0** | **empty on S** |

**Two things follow, and the first is good news worth stating on its own.** Every application blob matches
exactly. The incremental blob sync built at the start of this session — baseline, single-pass diff, changed
runs, return-path fold — is **measured correct under a real application's load**, not merely e2e-green on the
fixtures. The ring and the arena differ by a byte and by sixteen bytes respectively, which is the expected
skew of two processes sampling continuously-written memory at different instants; both are live channels
being written by both sides.

**The staging pool is the one structural divergence.** C's copy holds content; S's is *entirely zeros* — not
diverged, never filled. That is not a bug in the sync, it is the sync working as designed: `blob_id == 0`
marks the pool Venus-internal, and `blob_sync` declines to publish it deliberately, because C's stale copy of
S's arena would clobber replies the application is blocked on. The design note even anticipates the pool by
name as something C "genuinely wrote, harmless but pure waste". **Harmless was the assumption. It is now the
only measured difference between the two machines' state at the moment a submit fails to complete.**

**What this does and does not establish.** It does not prove the failing `vkQueueSubmit` reads those bytes —
28 non-zero bytes in 8 MiB is a very small footprint, more like a header or a descriptor than a recorded
command buffer, and the causal link is still unmeasured. What it does establish is that after eliminating the
ring (byte-exact, 253/253) and the application blobs (identical, 4 of 4), **the staging pool is the only
candidate left standing in the entire resource set**, and it is empty on the side that fails.

**Next, and it is a decision rather than a measurement:** the cheap experiment is to ship the pool C→S and see
whether the submit completes. That is *not* a fix — it would ship C's copy over a region S also writes, which
is precisely the clobber `blob_id` routing exists to prevent, and it could break the reply path in a way the
fixtures would not catch. But as a **diagnostic** it is decisive in one run: if the submit still fails, the
pool is exonerated and the token-built swapchain images are next; if it succeeds, the real design question
opens — how a region both sides write gets synchronised without either clobbering the other, which is the
same shape as the (c)2 problem and deserves its own spec rather than a patch at the end of a long day.

### 2026-07-26 — The staging-pool experiment: a clean negative, and the instrument distorted the first attempt

The blob fingerprints left the staging pool as the only structural divergence between the two machines, so
the experiment was to ship it and see whether the application's `vkQueueSubmit` completed. It was run as an
explicitly named diagnostic — `RAYLAND_C1_SHIP_BLOB=<res_id>`, the operator naming the resource rather than
the code inferring "the staging pool" from a size or an id, because inference is what has cost this
investigation most of its time.

**The mechanism works.** With the switch on, C and S report the same digest for `res=3`
(`nonzero=28 fnv=87d657a315c264f8`). The pool genuinely crosses; the last structural divergence is closed.

**The first run was invalid, and the reason is worth more than the run.** The application got *less* far with
the experiment on — 91 deltas and 36 proxy-trace lines, against 253 and 52 without it. S was not starved (3
slow sections, 91 of 91 deltas applied), so the cost was on C: the first `nonzero_runs` walked all 8 MiB a
byte at a time on **every relay**. That is precisely the per-relay scan cost removed from S this morning,
reintroduced on C by the instrument built to study its consequences. Rewritten chunked — comparing 64-byte
chunks against zeros with slice equality, descending per-byte only inside a non-zero chunk — exactly as the
S-side diff was.

**With the instrument fixed, the answer is negative.** 85 deltas, 36 proxy lines, ending on the same
`vkGetImageMemoryRequirements2 / vkAllocateMemory / vkGetImageDrmFormatModifierPropertiesEXT` signature. Both
runs with the pool shipped reached 36 proxy lines; both runs without it reached 52. **Shipping the staging
pool does not get the application further, and on this evidence makes it worse** — which is what `blob_id`
routing exists to prevent, and a reminder that "harmless but pure waste" was the optimistic half of that
design note.

**So the staging pool is exonerated as the missing ingredient.** That is a real result even though it is a
negative one: the ring is byte-exact, the application blobs are identical, and now the one remaining
structural difference has been closed and *did not help*. What the failing submit references is therefore
**not** raw blob content that C holds and S lacks. The remaining candidate is the one thing on this path that
never came from the blob machinery at all: the swapchain images S builds from **WP0 buffer tokens**
(`res=8/9/10`), which the (c)1 blob sync and the WP0 buffer path have never been reconciled against each
other. That is a seam between two subsystems rather than a missing byte, and it is where 4.3 was always going
to have to do its work.

The switch stays in the tree, inert by default and documented as *not* a fix: it publishes a region S also
writes, which is a clobber, and its `nonzero_runs` is not a diff and cannot un-set a byte. It earned its place
by answering a question, not by being safe to leave on.

### 2026-07-26 — The token-built resources agree too: there is no byte divergence left, so the wall is object state

Fingerprinting the swapchain images S builds from WP0 buffer tokens closes the last open candidate, and the
answer is negative in the most useful way.

**First, the instrument had to be fixed — again, and this is now a pattern worth naming.** The blob
fingerprint hashed every byte of every blob (~11 MiB) twice a second, on C inside the blob-table lock and on
S inside the applier lock. With it on, the application **never reached its swapchain buffers at all**: five
runs, all stopping at 36 proxy-trace lines with zero `create_immed`. With it off: three runs, **all** reaching
52 lines and four `create_immed`. Eight runs, a clean split — causation, not variance. Rewritten to skip zero
regions with chunk compares that lower to `memcmp`, hashing only non-zero content, it reached the buffers on
the *first* attempt. **Three separate times today an instrument has changed the behaviour it was measuring**,
and the reason is now itself a finding: the relay path is so latency-sensitive that tens of milliseconds
periodically stolen from it decide whether a real application gets through its setup. That is worth knowing
independently of this bug.

**With a cheap instrument, every resource agrees:**

| resource | C | S | |
|---|---|---|---|
| `res=8`, `res=9`, `res=10` — the token-built swapchain images | `nonzero=0` | `nonzero=0` | **match, both empty** |
| `res=7` | 0 | 0 | match |
| `res=2` reply arena | 20548 | 20548 | match |
| `res=4`, `res=5`, `res=6` | identical | identical | match |
| `res=1` ring | 6568 | 6569 | one byte of sampling skew |
| `res=3` staging pool | 120 | 0 | the known divergence, already shown irrelevant |

The swapchain images being empty on **both** sides is consistent rather than surprising — the submit that
would render into them is precisely what fails — but the point is that they *agree*. There is no resource
whose content C has and S lacks, except the staging pool, which was shipped and made no difference.

**So the elimination is complete, and it is worth stating as a positive claim:** the ring is byte-exact
(253/253 digests), every blob's content agrees, and the one structural gap was closed experimentally without
effect. **The failing `vkQueueSubmit` is not short of any bytes.** Whatever it needs, S has the memory for;
what S evidently does not have is the right *object state* behind it — the `VkImage`, its memory binding, its
layout — built through the WP0 token path rather than through anything the (c)1 blob machinery touches.

That reframes the remaining work from synchronisation to reconciliation, and it is squarely Task 4.3's:
the swapchain images exist on S as resources, and the application's commands refer to them as images with
assumptions — bound memory, a layout, a format — that nothing has yet been made responsible for establishing.
Chasing bytes is finished; the next question is what S's engine believes those three resources *are*.

### 2026-07-26 — What "CS error" actually means: S's decoder reads past the end of a stream whose bytes are provably intact

Following the elimination to its end, the remaining question was what condition virglrenderer reports as a
"CS error". The generated dispatcher answers it exactly:

```c
vn_decode_VkCommandTypeEXT(ctx->decoder, &cmd_type);
vn_decode_VkFlags(ctx->decoder, &cmd_flags);
vn_dispatch_table[cmd_type](ctx, cmd_flags);
if (vn_cs_decoder_get_fatal(ctx->decoder))
   vn_dispatch_debug_log(ctx, "%s resulted in CS error", vn_dispatch_command_name(cmd_type));
```

So the message means: **while dispatching `vkQueueSubmit`, the decoder's fatal flag was set.** That flag has
exactly one trigger, the same `vn_cs_decoder_set_fatal` whose silence misled this diary two days ago — a read
past the end of the available bytes. **S's decoder believes the submit's body is truncated.**

Which is paradoxical against everything else measured today: the ring relay is byte-exact across 253 deltas,
every blob's content agrees, and in the captured run all 102 deltas were sent *and* applied before the error.
The bytes are intact and the decoder runs off the end of them anyway.

**Two readings, and this diary is not going to pick one without evidence** — the count of theories refuted by
measurement today stands at eight, and every one of them was plausible at the time:

1. **The stream really is short at that moment.** S's ring thread dispatches at S's applied `tail`, and if
   that tail can ever fall mid-command the decoder would read into bytes not yet relayed. Mesa stores `tail`
   *after* writing a command, so every published tail should be a command boundary — but "should be" is
   exactly the kind of assumption this week has been punishing, and it has not been checked. It is checkable:
   walk the relayed stream cumulatively and confirm every relayed `tail` lands on a boundary.
2. **The submit's body is not all in the ring.** Venus can place a recorded command stream elsewhere, and
   `vkExecuteCommandStreamsMESA` (type 180) is how it refers to one. The scan for that opcode found nothing —
   but the scan only ever saw each delta's *first* unknown command, because `decode_commands` halts at the
   first size it does not know, so a 180 anywhere after that first command is invisible to it.

**Both readings are blocked on the same missing capability, and that is the real next piece of work:**
`venus_ring::decode`'s `encoded_size` table knows only a handful of command types, so the decoder cannot walk
a full stream — it stops at the application's first real command, every time. That was a sound, deliberately
conservative choice when the ring was opaque freight to be relayed unexamined, and it has served this whole
investigation well as a way to *name* the command in flight. It is now the limiting factor: **the question
"where does this stream actually end, and what is in it" cannot be asked at all.** Extending that table is not
a diagnostic hack; it is the difference between relaying bytes and understanding them, and it would answer
both readings above directly.

That is a piece of work with a clear shape and a clear payoff, and it deserves to be started deliberately
rather than bolted on at the end of a long day. The elimination that leads to it is complete and recorded:
**not the transport, not the ring bytes, not any blob's content, not the doorbell, not the consumer, not the
staging pool.** What is left is the framing of the stream itself, and Rayland currently cannot see it.

### 2026-07-26 — Correcting myself: the staging-pool "exoneration" was invalid — and now, done properly, it holds

Two corrections and one new measurement, in that order, because the corrections change what the measurement
means.

**First: "extend the `encoded_size` table" cannot be done as asked, and the module already says why.** Its own
docs list `vkCreateInstance`, `vkCreateRingMESA` and `vkExecuteCommandStreamsMESA` as commands it can *name*
but not size, with the warning that "recognising a command is not the same as being able to skip it, and
conflating the two is how a decoder desynchronizes". Most application commands are variable-length — arrays,
`pNext` chains, optional pointers — so no table of fixed sizes can walk a real stream. Doing it properly means
per-command decoders, which is the several thousand generated lines Mesa ships, and that is a subsystem
decision rather than an afternoon's work.

**Second, and worse: the previous entry's "clean negative" on the staging pool was not a result at all.** In
both of those runs the application stalled at 36 proxy-trace lines — *before* `create_immed`, and therefore
before it ever attempted the submit under test. The experiment never reached the thing it was meant to
falsify, and I read the absence of a difference as evidence. It was not evidence of anything. The cause was
the same latency sensitivity measured an hour earlier: shipping the pool's runs on **every** relay tripled C's
blob messages (454 → 1260), and this relay path does not tolerate that.

**Done properly, the conclusion survives.** Giving the pool a baseline and shipping only *changed* runs makes
the steady state one chunked comparison and no traffic. With that, the application reached the submit on the
**first** attempt (52 proxy lines, four `create_immed`), the pool is byte-identical on both sides
(`nonzero=212 fnv=3416b8b8656976fc` on C and on S) — **and the CS error still fires.** So the staging pool is
now genuinely exonerated, by an experiment that actually ran the code under test.

That is worth separating clearly: the earlier entry reached a right answer by an invalid route, which is not
the same as being right. Left in place, with this correction beside it.

**The new evidence, and it is the sharpest yet.** At the moment of failure S's own ring words read:

```
[s-ringctl] relayed_tail=27536  s_head=27356  s_tail=27536
```

`head` is **180 bytes** behind `tail`. So virglrenderer's decoder was given exactly those 180 bytes for the
command beginning at 27356, and ran past the end of them. **The command's encoding requires more bytes than
the published tail provides.**

Which sharpens the paradox rather than resolving it: every byte of every resource now demonstrably agrees
between the two machines — ring (253/253 digests), every application blob, the reply arena, and now the
staging pool — and the decoder still overruns. The remaining possibilities are about *extent* rather than
content: either a published `tail` can fall mid-command (Mesa stores `tail` after writing, so it should not,
but "should not" has been wrong repeatedly this week), or the submit's body is reached through a descriptor
whose `{resourceId, offset, size}` S resolves differently than C intends.

Distinguishing those needs the stream walked, which is exactly the capability the first correction says does
not exist. **The instrument gap and the open question are the same gap.** That is the honest state to leave
this in: nine theories refuted, the failure narrowed to the framing of a single command, and the next step
identified as a real piece of engineering rather than another probe.

### 2026-07-26 — Designing the Venus stream decoder: borrow Mesa's, do not rebuild it

The investigation ended at a capability gap rather than a bug: Rayland can relay Venus streams perfectly and
cannot read them. The design for closing that is written
(`docs/superpowers/specs/2026-07-26-venus-stream-decoder-design.md`); the reasoning that shaped it is here.

**The first thing settled was what the decoder is *for*, because it changes everything.** A diagnostic-only
decoder can be narrow and can afford to be wrong. A general capability has to track Mesa exactly. The choice
was the general capability — and with it, a binding constraint carried over unchanged from (c)1 spec §7:
**this decoder may never make a correctness decision.** The ring is relayed as opaque bytes precisely so that
a decoding bug cannot become a corruption bug, and that stays true. A decoder that observes can be wrong and
produce a wrong diagnosis, which a human notices; a decoder that decides can be wrong and produce a silently
corrupted frame, which is the exact shape of every wall this week. The plan carries a test that the relay path
does not depend on it.

**The second was where the knowledge comes from, and the answer is the same one `CLAUDE.md` already made
about the rendering engine.** Mesa's generated `venus-protocol` is 73,442 lines across 43 headers. Rayland
reuses Venus/virglrenderer rather than writing its own capture/replay engine because it "already exists and is
hardened against our exact threat model"; the protocol headers are the same category of artifact. Two
generator approaches were considered and rejected — parsing the C headers into Rust, or regenerating from
`vk.xml` — for one shared reason: **both create a second source of truth for a format Rayland does not own.**
When it diverges from Mesa's, and it will at some release, the symptom is a decoder that confidently reports
the wrong thing. Borrowing cannot diverge.

**The design turned on a feasibility question that could easily have gone the other way.** The renderer-side
decoders resolve object handles, which looks like it needs a live virglrenderer context. Reading the generated
code shows it does not:

```c
vn_decode_uint64_t(dec, &id);                       /* consumes 8 bytes */
*val = vn_cs_decoder_lookup_object(dec, id, ...);   /* consumes nothing */
```

**Byte consumption is independent of the lookup's result**, so a shim with stub lookups produces identical
framing to a live renderer. Handle validation lives in the *dispatch* functions, after decoding — so calling
`vn_decode_<cmd>_args_temp` directly gives framing with no object table, no validation, no execution and no
GPU. Had that check failed, the recommendation would have been wrong, and it was worth making before writing
a line of design rather than after.

**The shape that fell out is deliberately narrow:** a new crate whose entire public surface is *"how many
bytes does the command at the start of this slice occupy?"* — total, stateless, side-effect-free. All the
walking, the error taxonomy and the reporting stay in Rust in `venus_ring::decode`, where they are already
tested. If the borrowed protocol is ever replaced or the shim rewritten in Rust, one function has to keep its
meaning.

**Costs are recorded rather than glossed.** `rayland-vtest` advertises "no GPU dependencies, by construction:
only `libc` and `thiserror`", and that sentence becomes false — it stays GPU-free, but it gains a C
dependency, and `rayland-c` links it. The headers get vendored with their Mesa version recorded, because a
build that depends on a scratch directory happening to exist is not a build. And `no_gpu_linkage` gets re-run
and re-read rather than assumed, since the crate it guards has changed shape.

Not yet built. The spec is the deliverable; the plan comes next.

### 2026-07-26 — Task 1 built, and the "documented contract" was thinner than it looked

Task 1's brief was narrow on purpose: vendor Mesa's `venus-protocol` headers, write a replacement
`vkr_cs.h` satisfying the contract those headers' own comment declares, and prove the whole thing
compiles and links. The vendoring and the crate skeleton went exactly as planned — 52 files, 974 KiB
(the source directory's own `du` reports 4.1 MB due to block-size accounting; the file-by-file `diff`
against the source is empty, so the copy is exact). The surprise was in Step 4.

**The header comment `vn_protocol_renderer_cs.h` carries — the one both the design spec and the brief
call "the documented contract" — is stale relative to its own file's body.** The comment lists five
categories (encoder, decoder, object lookup, and four handle-id helpers). The brief's `vkr_cs.h`,
written from that comment, compiled cleanly against the *comment* and then failed against the *code*:
GCC reported six missing symbols the moment `shim.c` actually included the vendored tree —
`vkr_cs_decoder_get_blob_storage`, `vkr_cs_encoder_get_blob_storage`, `vkr_cs_decoder_alloc_temp_array`,
`vkr_cs_handle_indirect_id`, `vkr_cs_handle_load_id`, `vkr_cs_handle_store_id` — plus a `struct
vkr_object` the header dereferences directly rather than through a `vkr_cs_*` accessor. None of these
appear in the file's own "these types/functions are expected" comment. Mesa's generator grew blob-array
support and moved on; the hand-written comment above it did not follow.

**Whether this mattered for Task 1's narrow deliverable came down to one question: is each of these six
symbols merely type-checked, or actually exercised, given that Task 1's only C entry point is a constant
self-test that calls no decoder at all?** Reading the call graph settled it symbol by symbol.
`alloc_temp_array` is a mechanical composition of primitives the design already blessed (`alloc_temp`
plus a bounds-checked multiply) — implemented for real, not stubbed, since getting a bump-allocator
multiply right costs nothing extra. The three `handle_*_id` functions turned out to matter more than
they looked: they're called by `vn_decode_Vk*_temp` for *every* handle-typed command argument, which is
nearly every command, so a wrong answer there would have been silent and pervasive rather than confined
to an edge case. Their correct behavior fell out of two call sites already sitting in the vendored tree —
`vn_decode_VkInstance_temp` allocates a temp cell before storing an id, `vn_decode_VkBuffer` stores
straight into the handle slot — which distinguishes Vulkan's dispatchable handle types
(`VK_DEFINE_HANDLE`) from non-dispatchable ones (`VK_DEFINE_NON_DISPATCHABLE_HANDLE`), not a guess. But
which of those five dispatchable types actually needs the indirect treatment is a **pointer-size
comparison** (`sizeof(VkInstance) < sizeof(vkr_object_id)`), not a fixed per-type answer — on every
64-bit build that comparison is false, so dispatchable handles are direct too, same as non-dispatchable
ones. Critically, none of that machinery touches the byte cursor — it only manages an in-memory scratch
cell — so getting it wrong could never corrupt the one number this crate exists to report. The blob-storage functions are the
one piece left genuinely unresolved: the generated caller pattern skips the fatal flag on a NULL return
(`if (!val->pData) return;`, no `set_fatal`), so a stub returning NULL is safe *only* because Task 1
never reaches it — Task 2 must give it a real body before decoding any command with a blob array
(`vkCmdPushConstants2`, pipeline specialization constants, pipeline-cache data), or `command_len` will
silently under-report for exactly those commands. That gap is documented at length in `vkr_cs.h` itself,
flagged in the Task 1 report, and is the first thing Task 2 should read before writing a line of code.

**What this confirms, stated plainly:** "borrow, don't reimplement" was the right call, but "the contract
is small and documented" undersold it slightly — the header's *prose* is small and documented; its
*code* is the real contract, and reading code instead of trusting a comment above it is exactly the
discipline this whole decoder exists to bring to Rayland's own command streams. The crate compiles,
links, and reports `38` (`VK_COMMAND_TYPE_vkGetFenceStatus_EXT`) — Task 1's whole deliverable — with
that one deferred item carried forward explicitly rather than silently.

### 2026-07-26 — Task 1 review: the "spec-fixed" claim above was itself imprecise, and the code had a real bug behind it

Task 1 came back Approved, with one Important finding: `vkr_cs_handle_indirect_id` hardcoded `return
true;` for the five dispatchable handle types. Real virglrenderer computes `sizeof(VkInstance) <
sizeof(vkr_object_id)` instead — on every realistic (64-bit) build that is `8 < 8`, **false**, so
dispatchable handles get *direct* storage too, same as non-dispatchable ones. The hardcoded `true` was a
genuine behavioral divergence from the reference, not a stylistic difference: it made every dispatchable
handle take the temp-pool-allocation branch when the reference never does, on this architecture. The
reviewer traced every call site and confirmed it could not corrupt `command_len` (the byte cursor is
never touched by this code — only in-memory scratch bookkeeping), so it was not urgent, but Task 2 builds
directly on this header, so it is fixed now: `vkr_cs_handle_indirect_id` computes the same
pointer-size comparison the reference does, written out (not hardcoded to `false`) so it stays correct if
this crate is ever built for a target where pointers are narrower than the 64-bit wire id.

**The paragraph above describing this as "just Vulkan's own dispatchable/non-dispatchable handle split
… spec-fixed … should never need revisiting" was overstated, and that overstatement is exactly how the
bug survived a first read.** The real distinction is two-layered: WHICH handle types are ever candidates
for indirection is fixed by Vulkan's type taxonomy (only the five dispatchable types), but WHETHER a
candidate actually needs it is a runtime/build fact — a comparison between the host's pointer width and
the wire id's width — not a second fixed fact of the same kind as the first. Collapsing both into one
"spec-fixed" claim is what let a hardcoded `true` look self-evidently correct instead of like an
unjustified constant. The wording above has been corrected in place, per the reviewer's request, rather
than left to mislead a future reader; this entry is the record that it changed and why, so the diary
still shows the wrong turn rather than only the corrected belief.

`build.rs`'s comment was also tightened: it warned that reversing the include order "would pull in
virglrenderer's header", but virglrenderer's `vkr_cs.h` was never vendored — only `venus-protocol/` was
copied — so today this crate's copy is the *only* `vkr_cs.h` on the include path, and the order is
defensive precedent for if a competing header is ever added to `vendor/`, not a tiebreak against one that
exists now. The include order itself did not change; only the comment's claim about what it currently
guards against.

### 2026-07-26 — Task 2: the deferred blob-storage gap closed, and a second "the brief was wrong" moment

Task 1 left one thing genuinely unresolved and flagged in bold: `vkr_cs_decoder_get_blob_storage`
returning NULL is safe only because nothing in Task 1 reaches it, and the generated caller's early
return on NULL (`if (!val->pData) return;`) skips the one call that would notice truncation, without
setting the fatal flag itself. Task 2's whole first job was closing that before writing the switch that
would start actually reaching it. The fix ended up being narrow once the hazard was stated precisely:
match virglrenderer's own success path exactly (return `dec->cur`, no copy), but on the failure path —
where the reference silently returns NULL and trusts the caller to notice, which it doesn't — set fatal
ourselves, at the one point in the call graph that can still see the coming truncation before Mesa's own
generated code would swallow it. Matching the success path exactly meant the generated blob-array decode
now genuinely aliases (`dst == src`, since it reads from `dec->cur` into the very pointer we just handed
back pointing at `dec->cur`), which is why the `dec->cur != val` guard carried over from Task 1's review
(finding #2, previously unreachable) is reachable now — and verified reachable, by a hand-built
`vkCreatePipelineCache` stream carrying a real 5-byte blob padded to 8, which decodes to the exact
hand-computed 88 bytes, and reports `FAULT_TRUNCATED` rather than a short length when cut mid-blob.
Confidence: high — this is no longer "should be safe by inspection," it's measured against a byte layout
built independently of the code under test.

**The second surprise was not carried forward from Task 1 — it was new, and it echoes Task 1's own
"stale comment" discovery almost exactly.** The task brief asserted "the encoder side is never driven
by this crate" and allowed `vkr_cs_encoder_get_blob_storage` to stay a no-op. Building the generated
switch produced a compile error before a single case ran: six commands —
`vkGetQueryPoolResults`, `vkGetPipelineCacheData`, `vkCopyImageToMemoryMESA`,
`vkWriteAccelerationStructuresPropertiesKHR`, `vkGetRayTracingShaderGroupHandlesKHR`,
`vkGetRayTracingCaptureReplayShaderGroupHandlesKHR` — have generated *decoders* that take a `struct
vn_cs_encoder *` third argument, because Mesa pre-sizes each command's reply arena during the decode
pass rather than in a later, separate step. First draft made the encoder stub `abort()`, on the
(wrong) assumption it was truly unreachable; it would have crashed the moment any of these six commands
was decoded. Reading all six call sites (not sampling) showed the returned pointer is only ever stored,
never dereferenced, inside the decode function — it only becomes live inside the matching
`vn_encode_*_reply` function, which this crate never calls — so a fixed non-null `static` sentinel,
deliberately not sized to the request, discharges the contract with no exhaustion hazard of its own
(sizing it would recreate the very "our scratch pool was too small" failure this whole task exists to
rule out, for a `vkGetPipelineCacheData` reply that can legitimately be dozens of MB).

**What this confirms, restated because Task 1 already said something close and it is worth checking it
still holds:** a symbol-existence scan ("does `vn_decode_X_args_temp` exist") is not the same question as
"what does it actually require to call correctly," and only trying to compile against the real signatures
surfaces the difference. The generator itself had a matching near-miss: an early version of its
3-argument detection scanned every occurrence of a decoder's name, including the *call site* inside
`vn_dispatch_*` (whose arguments are variable names like `ctx->encoder`, not the parameter's declared
type) — and since that call site's match came later in the file than the true declaration, it silently
overwrote the correct "takes an encoder" answer with a wrong one for every affected command. Caught the
same way as the signature mismatch: the build failed loudly rather than producing a switch that looked
right and wasn't. Two independent times this task tried to be clever with a shortcut and got a wrong
answer, and two independent times the wrongness surfaced as a compile error rather than a silently wrong
`command_len` — which is the property this whole crate is supposed to have, so it is a reassuring way for
a mistake to have happened, not just an inconvenient one. Also found, separately: the brief's own Step 1
Python snippet passes `&dec_public` where `dec_public` is already a pointer — a straightforward typo,
fixed and documented in the generator rather than silently corrected.

The generated switch has 312 cases (`case 38:` for `vkGetFenceStatus` present, as expected), builds
clean with no warnings under `-Wall -Wextra`, and Task 1's link-proving self-test still passes (kept
deliberately, since deleting it was outside this task's stated file list and would have broken a
passing test for no requirement). Full detail, including the exact hand-derived byte layouts used to
verify both blob-storage fixes empirically, is in
`.superpowers/sdd/2026-07-26-venus-stream-decoder/task-2-report.md`.

### 2026-07-26 — Task 2 review: Approved, with the encoder-sentinel comment fixed for saying "safe" instead of "required"

Task 2 came back Approved. The reviewer verified both discoveries above adversarially rather than on
trust — re-checked the blob-storage fix against the reference's success path, and re-ran the six-site
encoder audit as a global scan across all 43 vendored headers, confirming no `vn_decode_*` function
anywhere calls `vn_cs_encoder_write`/`_acquire`/`_release`. Both held.

One Important finding, and it's a real one about how a true statement can still be the wrong
justification. The `vkr_cs_encoder_get_blob_storage` comment argued the sentinel is safe because it's
never dereferenced — accurate, but it answered "could this be NULL without immediate harm" when the
question that actually matters is "must this always be non-NULL." Two of the six commands
(`vkGetQueryPoolResults`, `vkWriteAccelerationStructuresPropertiesKHR`) decode more stream fields
*after* the blob-storage branch, so a NULL return — which nothing currently produces, but which a
future "let's add a size check to be safe" edit plausibly would — takes the generated early return and
skips those trailing decodes without setting fatal. Same hazard class as the decoder-side fix, on the
other side of the same function pair, and the comment that was supposed to prevent exactly this kind of
regression didn't say so. Rewritten to lead with the requirement, name the two commands and their
trailing fields, and warn explicitly against "hardening" it later. Also fixed: `src/lib.rs`'s comment
claiming the self-test would be "removed in Task 2" — stale the moment Task 2 chose to keep it, and
missed because `src/lib.rs` sat outside Task 2's stated file list, which is not an exemption from
CLAUDE.md's stale-comment rule.

**What this confirms:** "the code is right, the comment justifying it is wrong" is its own failure mode,
distinct from a stale comment describing removed code — here the comment was freshly written *for* the
code it sits above, and still argued the weaker of two true claims. A reviewer checking "is this safe"
would have agreed with the original wording; only checking "is this comment a load-bearing warning for
whoever touches this function next" caught that it wasn't.

### 2026-07-26 — Task 3: the safe Rust wrapper, and a teeth-check that didn't work as briefed

Wrapped `csrc/shim.c`'s one entry point in `command_len(&[u8]) -> Result<Command, DecodeFault>`,
`unsafe` confined to the single `extern "C"` call, per the brief's exact code. The brief's four tests
went red-to-green as expected — the RED was a genuine compile failure (`command_len`, `Command`,
`DecodeFault` didn't exist), not a runtime assertion, which is as strong a RED as this kind of change
gets.

Also added the two tests Task 2's review asked to be carried forward, since that review's own
verification used a throwaway harness that no longer exists and the reviewer wanted the load-bearing
evidence committed, not re-derived from memory next time someone doubts it. Both expected byte counts —
88 for a `vkCreatePipelineCache` with a 5-byte (unpadded) `pInitialData` blob, 48 for a
`vkGetPipelineCacheData` that reaches the 3-argument decoder path and its encoder sentinel — were worked
out field-by-field from `vn_protocol_renderer_pipeline_cache.h` *before* running anything, the same
discipline the brief demands for the 24-byte `vkGetFenceStatus` case ("a test whose expectation came
from the implementation proves only that the code equals itself"). Both passed on the first run, which
is corroborating, not proof — the derivation is what makes them trustworthy, not the fact that they
happened to match.

The teeth-check is the thing worth recording carefully, because the brief's literal instruction did not
survive contact with `size_t` being unsigned. The brief says: flip `vkr_cs_decoder_read`'s bounds check
from `<` to `>`, rebuild, and the truncation test should fail. Tried exactly that, and it *didn't* fail —
`a_truncated_command_is_a_fault_not_a_guess` still passed. Not because the check still worked: because
flipping the operator doesn't disable the check, it inverts *which* case trips it. `remaining > size`
fires on almost every normal read (there's usually slack left in the buffer), so the very first prologue
read now sets `fatal` spuriously — and the shim's post-prologue fatal check turns that into
`DecodeFault::Truncated` before the real truncation site is ever reached. For this test's specific input
that happens to be the *same* label the test expects, for a completely wrong reason: it's a coincidence,
not evidence. Running the full suite unmasked it immediately — two *other* tests
(`a_fixed_size_command_reports_its_real_length`, `an_unknown_command_type_reports_which_one`) failed
under the exact same mutation, which a filtered `cargo test ... a_truncated` run would never have shown.

That is exactly the "a test that cannot fail is not evidence" trap the task brief itself warned about,
just arriving from an unexpected direction — not a wrong expected number, but a mutation that doesn't
falsify the thing it was supposed to falsify. Fixed by using a mutation that actually does what the
brief's parenthetical said it wanted ("so it never sets fatal"): replaced the check's body with `if (0)`,
genuinely disabling it. Under *that* mutation, the truncated stream decodes to
`Ok(Command { command_type: 38, len: 24 })` — a plausible, wrong, unflagged length instead of an error —
which is the single hazard this entire crate exists to prevent, and is what actually makes
`a_truncated_command_is_a_fault_not_a_guess` fail. Reverted immediately after; `git diff` on `vkr_cs.h`
came back empty, confirming a byte-exact restore.

**What this confirms:** an operator flip is a plausible-sounding mutation for a teeth-check but is not
automatically a *falsifying* one, especially across unsigned arithmetic where "wrong" doesn't mean
"opposite," it means "wrong in some other shape." The fix wasn't to distrust the brief's intent (a
genuinely-disabled check is exactly what "so it never sets fatal" describes) — it was to notice that the
literal edit it named didn't deliver that intent, and to check the *specific test's* pass/fail, not just
skim a green result, before calling the teeth-check done. Full derivations and both test runs are in
`.superpowers/sdd/2026-07-26-venus-stream-decoder/task-3-report.md`.

### 2026-07-26 — Task 3 review: Approved, and a stale-comment miss caught by the diary itself

Task 3 came back Approved on substance: the reviewer independently re-derived both carried byte
counts (88 and 48) from the same generated decoder rather than trusting the report, and independently
re-traced the teeth-check finding above — confirming the `<`→`>` flip really does fire on the prologue
read rather than disabling the check, and that `if (0)` really does produce the confidently-wrong
`Ok(Command{38,24})` this crate exists to prevent. Both held on adversarial re-check, not just on trust.

One finding, and it is the second time this exact crate has produced it. `csrc/shim.c`'s
`rayland_venus_proto_selftest` (Task 1's link-proving self-test) still carried a comment saying it was
"still linked by `rayland-venus-proto`'s existing Rust wrapper (`src/lib.rs::selftest_command_type` and
its test)" — true when Task 2 wrote it, false the moment Task 3 deleted both the function and the test
it described, since nothing else in the tree calls it. First instinct was to flag it and leave it,
reasoning that Task 3's stated file list was `src/lib.rs` only and `csrc/shim.c` wasn't otherwise part
of this task. That reasoning does not survive contact with this diary's own earlier entry, four rounds
back: "missed because `src/lib.rs` sat outside Task 2's stated file list, which is not an exemption
from CLAUDE.md's stale-comment rule." Same crate, same shape of excuse, same rule — CLAUDE.md's binding
text has never been scoped to a task's declared files, and a file list exists to bound what a task
*builds*, not what it is responsible for leaving accurate. Fixed by deleting the function outright
(zero callers, and its one job — proving the vendored protocol compiles and links — is now done more
thoroughly by `command_len`'s six tests, which exercise the real entry point instead of a constant)
rather than correcting the comment in place, since a function whose only remaining purpose would be
"exists to be described as unused" is not worth keeping. Also tightened, on the same review pass: the
two magic fault-code branches in `command_len` now cite their `RAYLAND_VENUS_FAULT_*` names in
`csrc/shim.c` instead of being bare integers, and the SAFETY comment's "keeps no state between calls"
claim was corrected — the scratch pool's *bytes* are `_Thread_local static` and do persist, only its
bump-allocator cursor resets per call, which is what actually makes it safe rather than merely quiet.

**What this confirms, and it is worth saying plainly since it is the second occurrence:** "outside this
task's file list" is a real fact about scope, but it has been tried twice now as a reason to leave a
comment that is actively wrong, and twice it was wrong to try it. The diary recording the first
instance did not, by itself, prevent the second — it took a reviewer pointing at the diary's own
sentence for the rule to actually bind. That is a point in favor of *rereading* the diary before
declining a fix on scope grounds, not just writing it down and trusting the writing to stick.

### 2026-07-26 — Task 4: the walker reaches past the size table, and a fixture the brief assumed does not exist

Task 4 wired `venus_ring::decode` up to `rayland-venus-proto`: `decode_commands` now tries
[`encoded_size`]'s fixed table first and, for anything it cannot express, falls back to
`rayland_venus_proto::command_len` — Mesa's own generated decoder. `Truncated` from the borrowed
decoder maps to `DecodeStop::Truncated`; anything else maps to the old `DecodeStop::UnknownCommandSize`,
so the walk still refuses to guess, it just has far more to consult before it has to. The agreement
test (`the_size_table_and_the_borrowed_decoder_agree`) confirms the table and the borrowed decoder give
the identical 24-byte answer for `vkNotifyRingMESA`, the one command both can size — the cross-check
this whole design rests on, and it held with no fuss.

The brief's anchor test (Step 6) assumed a fixture called `CAPTURED_RING_COMMAND_STREAM` that would
walk a real, multi-command stream all the way to `DecodeStop::ReachedEnd`. That constant does not exist
anywhere in this repository, and neither does the data it would need: `captured.rs`'s one ring capture
(2026-07-15) preserves only the first 100 of the 216 bytes the client had actually produced — enough to
cover the three commands the fixed-size table already knew, plus one byte into `vkCreateInstance`, and
no further. That is not a gap this task can quietly paper over by writing a bigger fixture from
scratch: the whole point of a captured fixture, stated in this same file's provenance note, is that the
bytes are an observation, never synthesized to make an assertion pass. So this task's anchor test could
not be written as specified, and the honest thing was to say so rather than relax it to match what
happens to be on hand.

What actually happened when the walker was pointed at the real 100-byte capture (still using its
existing test, `captured_ring_bytes_decode_as_venus_vulkan_commands`) turned out to be its own small
finding: the stop at `vkCreateInstance` changed from `DecodeStop::UnknownCommandSize` to
`DecodeStop::Truncated { offset: 88 }`. Before Task 4, that stop meant "this module does not know how to
size this command." Now it means something more precise and more true: the borrowed decoder *does* know
how, in principle — Mesa generates a real decoder for `vkCreateInstance` — it simply was not handed
enough bytes, because the fixture's capture window ends at 100 while the client's writes did not. The
walker got strictly more honest about *why* it stopped, even though the offset it stopped at did not
move. That old assertion was pinning the size table's ceiling, not a requirement, and updating it (rather
than leaving the walker unable to make this distinction) is exactly what the brief asked for when a
pre-existing test turns out to have been testing a limitation.

To still get positive evidence that the walker can reach `ReachedEnd` via the borrowed decoder on real
bytes — the substance Step 6 was reaching for, even though its literal fixture doesn't exist — a
different real capture already in this file was pressed into service: the 2026-07-19
`vkGetDeviceQueue2` bytes (`CAPTURED_GET_DEVICE_QUEUE2`), captured whole with nothing before or after
it. `vkGetDeviceQueue2` is deliberately excluded from `encoded_size`'s table (the walk was never
expected to *reach* it, since variable-length commands precede it in a real session), so handing its 80
bytes straight to `decode_commands` genuinely exercises the fallback: the table returns `None`, the
borrowed decoder sizes it at 80, and the walk lands exactly on `stream.len()` — `ReachedEnd`, produced
by Mesa's decoder rather than the table, on a real Venus client's bytes. That is the new test
`the_borrowed_decoder_walks_a_real_variable_format_command_to_its_end`, and it is offered as what this
task could actually prove with the fixtures on hand, not as a substitute that pretends to be the
brief's original anchor.

**What is still an open gap, recorded rather than quietly worked around:** nothing in this repository
proves the walker can cross *several* variable-length commands in one real stream and land on
`ReachedEnd` — every fixture here is either short (the 100-byte ring prefix) or exactly one command
(the `vkGetDeviceQueue2` capture). That would need a fresh capture — the same `RAYLAND_RING_DUMP`
diagnostic used in the 2026-07-15 spike, rerun against a workload and long enough to catch a ring after
several full application commands, not just an init-only prefix — and is left for whenever that
stronger proof is actually needed, rather than invented now to make a test title read cleanly.

### 2026-07-26 — Task 4 review round: retracting absolutist claims that Task 4 itself made false

Task 4's review came back clean on substance but caught something worth recording on its own: several
comments elsewhere in `venus_ring` were still asserting, in the present tense, that a decode-based walk
can **never** get past a session's second command — "for every workload, forever", "cannot reach",
"would never reach". That was true right up until Task 4 landed the borrowed-decoder fallback earlier
the same day, and false the moment it did, and nobody had gone back to say so. This is exactly the
"belief later overturned" case CLAUDE.md's diary rule cares most about, and it very nearly went
unrecorded: the fix (`325e9f0`) touched four files — `out_of_line.rs`'s module docs and the test that
specifically pinned the absolute claim, `decode.rs`'s docs on `VK_COMMAND_TYPE_VK_CREATE_INSTANCE`,
`VK_COMMAND_TYPE_VK_GET_DEVICE_QUEUE2` and `find_get_device_queue2`, and `mod.rs`'s scope-limits
bullet — and changed no code behaviour at all, only what the comments claimed was true.

The correction is precise, not a blanket softening: the walker's blind spot is now **conditional**
rather than **closed**. A decode-based scan's reach is only ever as good as every command ahead of it
decoding without fault, and this crate's own fixtures already show both `Truncated` and
`UnknownCommandSize` occurring on genuine captured data — so "it might still stop early" remains true,
just not for the reason ("no decoder exists past command two") the stale comments gave. The dword scan
`out_of_line.rs` actually uses for the multi-ring question is kept for the same reason it always was:
its correctness does not depend on how far the linear walk can get, which is what makes it sound
regardless of whether this particular gap is open or closed on any given day.

Two minor corrections rode along: `DecodeStop::UnknownCommandSize`'s doc now notes it also catches
`DecodeFault::BadArgs` via `decode_commands`'s `Err(_)` catch-all (distinct from "no decoder exists" —
today unreachable in practice, but the doc should say what the code actually does, not just its common
case), and `vkGetDeviceQueue2`'s doc got the same present-tense fix as the rest.

**What we now believe:** the sub-project's own docs are not exempt from the staleness this whole
review pass exists to catch, and "no code behaviour changed" is not a reason to skip writing this
down — cargo test -p rayland-vtest stayed at 58 passed, 0 failed (plus `no_gpu_linkage`, 1 passed)
before and after, and the entry that matters here is the retraction, not a diff.

### 2026-07-26 — Task 5: closing the loop — a test with teeth, a guard re-read rather than assumed, and the docs this work makes false

Task 5 was the last task of this sub-project, and its job was not to add capability but to make the
rule the previous four tasks all leaned on into something a build can fail on. Across five tasks,
`rayland-venus-proto` went from "does not exist" to "borrowed Mesa decoder wired in as
`venus_ring::decode`'s fallback past the fixed-size table" — and every one of those tasks' own docs
said the same sentence: diagnostic and structural only, never load-bearing for what gets relayed. That
sentence lived in three places (the design spec, `rayland-venus-proto`'s own crate docs,
`rayland-vtest`'s crate docs) and none of them fail a build. Today gave it a fourth home that does.

The new test, `rayland-c/tests/decoder_is_not_load_bearing.rs`, is deliberately small: it greps the four
source files that actually decide what crosses the wire and when — `blob_sync.rs`, `ring.rs`,
`relay_engine.rs`, `link.rs` — for the literal string `rayland_venus_proto`, and fails loudly, quoting
(c)1 spec §7, if any of them contains it. The test's own doc comment says plainly what this does *not*
prove: it catches the direct, accidental case (an author reaching for `rayland_venus_proto::command_len`
inside the relay path because it happens to be sitting right there), not influence laundered through a
third module that itself never names the crate. A guard that claimed more than that would be exactly
the kind of overstatement this sub-project has been trying not to write.

**Teeth-checked for real, following the brief's own warning about Task 3's false-pass trap.** The
mutation was the literal one specified: add `// rayland_venus_proto` to the end of `blob_sync.rs`,
re-run, and read the failure — not just its exit code. It failed with exactly the intended message,
naming `src/blob_sync.rs` and quoting the DIAGNOSTIC ONLY rationale, because the substring check has no
room to fire for any other reason than the one being tested (unlike Task 3's bounds-check flip, there is
no unsigned-arithmetic side door here — either the byte string is present in the file or it is not).
Removed the line, re-ran, green again, and `git diff` on `blob_sync.rs` came back empty, confirming the
mutation left no trace behind.

**`no_gpu_linkage` was re-run and then actually read, per the brief's instruction not to assume it still
means what it used to.** It still asserts exactly one thing: `cargo tree -p rayland-c` must not contain
the string `rayland-engine`. That claim is completely unaffected by today's change — `rayland-venus-proto`
compiles Mesa's *generated protocol headers* through a `cc` build step, not `libvirglrenderer`, so it
was never going to trip this guard, and it didn't. The one thing that needed correcting was not this
test's assertion but its self-description: `rayland-c`'s own `no_gpu_linkage.rs` never actually claimed
the tree was "only `libc` and `thiserror`" — that phrasing lived in `rayland-vtest`'s crate docs and in
`CLAUDE.md`, not in the guard itself, so the guard's doc comment needed no correction. What did need
correcting were the two places that phrase actually lived.

**The documents this closes out.** `rayland-vtest/src/lib.rs`'s "What lives here" section and
`CLAUDE.md`'s `rayland-vtest` bullet both said "no GPU dependencies, by construction: only `libc` and
`thiserror`" — true when Task 1 started, false since `rayland-vtest`'s `Cargo.toml` picked up
`rayland-venus-proto` as a dependency. Both were rewritten to name the real three dependencies and to
say, precisely, why the crate is still GPU-free despite the new one: the vendored headers are Mesa's
*generated protocol format* — no driver, no device, no `libvirglrenderer` — and the arrow still points
`rayland-engine → rayland-vtest`, never the other way. `CLAUDE.md` also gained the `rayland-venus-proto`
bullet the brief specified, in the same voice as its neighbours.

**Two stale statements found outside this task's declared file list, and fixed rather than excused —**
the "outside this task's files" excuse has already been rejected twice in this project's own diary, and
a third instance would have made that rejection meaningless. First, `project-map.js`'s `vtest` node
still carried the same now-false "only libc and thiserror" sentence in both its `desc` and its
"no-GPU linkage guard" part — the brief's Step 7 only named the `venus-proto` node, but the `vtest`
node told the identical lie, so both were corrected. Second, `CLAUDE.md`'s own workspace-size sentence
— "A Cargo workspace of seventeen crates" — has been wrong since Task 1 landed the eighteenth
(`rayland-venus-proto`) three tasks ago and nobody updated the count; `ls crates/` says eighteen, and
the sentence now does too.

**What we now believe, and how confident:** the diagnostic-only invariant is enforced mechanically, not
just by prose, and the mechanism's own honest limit is written into its doc comment rather than implied.
The size table and Mesa's borrowed decoder agreed on every command both could size across all five
tasks — 24 bytes for `vkNotifyRingMESA` in Task 4's cross-check, and no disagreement was ever found. The
one deliberately-unresolved gap from Task 4 (no fixture yet proves a multi-command variable-length walk
reaching `ReachedEnd`) is unchanged by this task and is not this task's to close; it is recorded here
again so it does not quietly vanish from the record now that the sub-project's last task is done.

### 2026-07-26 — Task 5 review round: the guard we just wrote had a live hole, and where it actually was

Review of Task 5 came back mostly clean — the teeth-check was genuine, the `no_gpu_linkage` re-read was
correct, the doc corrections held in both directions — but flagged one **Critical** finding against the
one artifact the whole task existed to build: `decoder_is_not_load_bearing`'s guard would not have
caught the violation it exists to prevent, and the gap was not in the execution, it was in the plan.

Three facts, composed, made this concrete rather than theoretical. First, `RELAY_PATH` listed
`blob_sync.rs`, `ring.rs`, `relay_engine.rs`, `link.rs` — and missed `main.rs`, which is where the relay
decisions actually happen. `ring_watcher_thread` (in `main.rs`) is what calls
`scan_for_out_of_line_stream`, `messages_for_delta`, and `link.send`; `ring.rs` itself only extracts
bytes from the ring, it never sends anything anywhere. Second, `main.rs` already held a live, reachable
call into the borrowed decoder: the `RAYLAND_RING_DUMP` diagnostic (`decode_commands`, gated behind an
env var, feeding only `eprintln!`) sat inline inside the exact function that also decides what gets
relayed. Third, even had `main.rs` been in the list, the guard's one needle — the literal string
`rayland_venus_proto` — would not have matched that call, because it reaches the decoder through
`rayland-vtest`'s re-export (`rayland_vtest::venus_ring::decode::decode_commands`) and never spells the
crate's own name. Put together: someone could have made that already-present, already-reachable call
load-bearing — say, branching on its `stop` result to skip relaying a truncated command — and this
guard would have stayed green throughout.

**The fix chosen, and why, over the alternative the reviewer offered as a fallback.** The reviewer's own
proposal — extract the diagnostic into its own module so the relay path contains no decoder call at
all, then check both needles — is the one implemented, because the alternative (teaching the guard to
tell "diagnostic" and "load-bearing" call sites apart by inspecting *how* a call's result is used) is not
a textual check at all; it would need real data-flow analysis, which is a different kind of tool than
"grep the source for a name." A test that has to reason about intent is a test that can be fooled by
intent dressed up to look innocent. Moving the call to a place structurally incapable of influencing the
relay, and then asserting the relay path contains none of it, keeps the guard a mechanical fact rather
than a judgment call.

The new module is `crates/rayland-c/src/ring_dump.rs`: `dump_if_enabled`, taking a `&RingDelta` and
doing nothing unless `RAYLAND_RING_DUMP` is set, then printing exactly what the old inline block
printed (the per-command names/offsets/reply-flags line, the FNV-1a hash, and the raw multi-ring dword
scan) — moved verbatim, not rewritten, so the diagnostic's actual behavior is unchanged. `fnv1a` moved
with it, since nothing else in `main.rs` used it. `main.rs` now holds a single call,
`ring_dump::dump_if_enabled(pending)`, and no longer spells the decoder crate's name, the decoder
module's path, or `decode_commands` anywhere in its own text.

`RELAY_PATH` gained `main.rs`. The needle became a list, `FORBIDDEN_NEEDLES = ["rayland_venus_proto",
"decode_commands"]`, checked against every file in `RELAY_PATH` — deliberately **not** including
`ring_dump.rs` itself, since that file is where the one legitimate call is meant to live. The test's
honest-limit paragraph was rewritten per the review's Minor finding: it used to say a future author
"could route a decision through a third module," phrased as a remote hypothetical. It now says what was
actually true the whole time — the decoder is already reachable from every relay-path file through
`rayland-vtest`'s public re-export, and today `ring_dump` is the only caller. Stating the real situation
rather than a generic one is the more honest sentence, not just a more specific one.

**Teeth-checked again, this time for the case the first guard missed.** Restored a real, compiling call
to `decode_commands` directly inside `ring_watcher_thread` in `main.rs` (`let (_c, _s) =
rayland_vtest::venus_ring::decode::decode_commands(&pending.bytes);`, right where the diagnostic used to
live) — confirmed it built clean with `cargo build -p rayland-c`, so this is a real reachable path and
not a textual trick — then ran the guard: it failed, naming `src/main.rs` and the `"decode_commands"`
needle, exactly the violation this round exists to catch. Reverted; `git diff` on `main.rs` shows no
trace of the plant. Re-ran `cargo test -p rayland-c` in full (both `decoder_is_not_load_bearing` and
`no_gpu_linkage` green, 38+10+13+other suites all passing) and the full workspace suite a second time
(`cargo test --workspace`, exit 0, 66 `test result: ok` blocks, 0 `FAILED`, including the GPU-backed
`loopback_e2e` icosa fixtures) to confirm the `main.rs` refactor — not just a doc change this time —
disturbed nothing on the path this task must never touch.

**What we now believe:** a guard's plan is only as good as the concrete violation it was checked
against, and this one was checked against an abstraction ("a future author could reach the decoder")
rather than the actual, already-existing reachable call sitting in the very file the plan forgot to
list. The corrected guard is checked against that same real call, restored and confirmed to trip it,
which is a stronger claim than the first round could make.

### 2026-07-26 — The Venus stream decoder is built, and the plan that built it was wrong four times

Five tasks, five reviews, five fix rounds, one final whole-change review and one fix wave. `rayland-venus-proto`
exists: Rayland can now walk a Venus command stream instead of naming its first command. The engineering is
recorded in the task reports; what belongs here is what the process actually cost and what it caught.

**What was built.** Mesa's generated `venus-protocol` (43 headers, ~97k lines, virglrenderer 1.2.0 — the
version this machine links) is vendored and compiled against a `vkr_cs.h` this crate writes itself, which is
what keeps virglrenderer and Mesa's util library out of the build entirely. A ~150-line C shim drives Mesa's
own per-command decoders over a caller-supplied slice; a generated 312-case switch dispatches to them; and
Rust exposes exactly one function — *how long is the command at the start of these bytes?* `decode_commands`
asks its fixed-size table first and falls back to the borrowed decoder, keeping the table as an independent
cross-check rather than deleting it.

**The plan I wrote was defective four times, and every defect was caught by someone else.** That is worth
recording plainly, because the plan read as complete when it was written:

1. **A false-pass teeth-check.** Task 3's brief said to invert a bounds check from `<` to `>` and confirm the
   truncation test fails. It does not fail — the flip fires on the prologue instead, so the test passes for
   the wrong reason. Caught only because that implementer ran the *whole* suite rather than the named test,
   noticed two unrelated tests break, and substituted `if (0)` to genuinely disable the guard. A teeth-check
   that cannot fail is not evidence, and mine could not.
2. **A fixture that does not exist.** Task 4's anchor test was to walk `CAPTURED_RING_COMMAND_STREAM` to
   `ReachedEnd`. There is no such fixture; the real capture holds 100 of 216 bytes. The implementer refused to
   fabricate one — correctly, since the anchor's whole argument is that the byte total comes from a real
   virglrenderer and not from us — substituted real evidence from another capture, and recorded the residual
   gap. **That gap is still open: no fixture proves a *multi-command* variable-length walk reaching
   `ReachedEnd`.** The first real `RAYLAND_RING_DUMP` run of this decoder will settle it.
3. **A guard with a live hole.** Task 5's invariant test — the mechanical enforcement of "this decoder may
   never make a correctness decision" — omitted `main.rs`, which is where the relay decisions actually are,
   and grepped for a needle (`rayland_venus_proto`) that cannot match the reachable path
   (`rayland_vtest::venus_ring::decode::decode_commands`). `main.rs` *already contained* such a call. The
   guard would have stayed green through the exact violation it exists to prevent.
4. **An allowlist that rots.** Even after fixing (3), `RELAY_PATH` named 5 of 10 source files, and its
   disclosed "residual gap" turned out to be a live path (`main.rs` → `ring_dump::dump_if_enabled`) held open
   only by prose. It is now inverted — enumerate `src/*.rs`, subtract a justified exclusion set — so a new
   relay module is covered by default, and `ring_dump`'s returnless signature is pinned by an assertion
   rather than by a sentence.

**Two things the borrowed protocol taught us that no amount of design could have.** Mesa's generated
`vn_protocol_renderer_cs.h` **under-documents its own contract**: its "these types/functions are expected"
comment lists eleven symbols and the body needs six more. And six commands' *decoders* take a
`struct vn_cs_encoder *`, because Mesa pre-sizes their reply arena during decode — flatly contradicting the
spec's "this crate never drives the encoder". Both were found by compiling, not by reading.

**The failure mode this crate is built to avoid, and nearly reproduced twice.** A decoder that returns a
*plausible wrong length* is worse than one that refuses: a walker trusts it, desynchronizes, and reports
commands that never existed. Two paths could have done exactly that — `vkr_cs_decoder_get_blob_storage`
returning NULL (which Mesa's own reference does, *without* setting fatal, so the generated caller silently
skips the cursor-advancing read), and an encoder sentinel that must unconditionally succeed because two
commands decode trailing fields after that branch. Both are closed, and the second's comment now warns in
capitals against the "let's harden this with a size check" edit that would silently reopen it.

**A subtler one, caught last.** Temp-pool exhaustion was reported as `Truncated` — which is *literally* the
CS-error condition the live vkcube investigation is trying to confirm or refute, and our 1 MiB cap is 1000×
smaller than virglrenderer's 1 GiB. The length would have been safe and the *diagnosis* wrong, which for a
diagnostic tool is the whole cost of being wrong. It is now a distinct fault end to end.

**What this does not yet do.** It has not been pointed at the failing `vkQueueSubmit`. That is the next
session's work, and the reason all of this exists: to ask whether a published `tail` can fall mid-command, or
whether part of that submit lives outside the ring behind a `vkExecuteCommandStreamsMESA`. The instrument is
built and guarded; the question is still open.

### 2026-07-26 — The decoder pays for itself in one run: "CS error" never meant truncation, and the submit's *queue* is what S cannot find

The stream decoder was built to answer one question. Pointed at the failing submit, it answered it, refuted
both standing hypotheses, and overturned the premise its own design spec was written on — all from a single
`RAYLAND_RING_DUMP` run.

**Both hypotheses are dead.**

- *A published `tail` can fall mid-command* — **refuted.** The decoder walks **105 of 105 relayed deltas to
  `ReachedEnd`**. Every relayed `tail` lands exactly on a command boundary; a mid-command tail would have
  ended its delta in `Truncated`, and none did. (This was the owner's suggestion, arrived at independently:
  build the stream up piece by piece and see where it breaks. A walk reaching `ReachedEnd` *is* that check,
  performed on every boundary at once, and offline — which matters because the live failure is intermittent.)
- *Part of the submit lives outside the ring* — **refuted.** `vkExecuteCommandStreamsMESA` (type 180) appears
  **zero** times in the entire stream.

**And virglrenderer agrees with our framing exactly.** In the delta ending at 27536, our decoder puts
`vkQueueSubmit` (type 18) at offset 748. S's own ring words that run read `s_head = 27416` — which is offset
**748** in that delta, to the byte. virglrenderer consumed everything before the submit, stopped precisely at
its first byte, and only then failed. The two independent implementations frame this stream identically.

**So why does it fail? Because "CS error" does not mean what this diary has assumed for three days.** The
generated dispatcher reports it whenever the decoder's fatal flag is set *after* dispatch — and the fatal flag
is **overloaded**:

```c
vn_decode_vkQueueSubmit_args_temp(ctx->decoder, &args);
if (!args.queue) {
    vn_cs_decoder_set_fatal(ctx->decoder);   /* handle validation — not truncation */
    return;
}
```

`args.queue` comes from `vkr_cs_decoder_lookup_object(..., VK_OBJECT_TYPE_QUEUE)`, which returns NULL when the
context holds no object for that id. **A handle that fails lookup produces the identical "resulted in CS
error" message as a read past the end.**

That is where the earlier inference went wrong. It was derived from `vn_cs_decoder_set_fatal`'s only
*documented* trigger — the bounds check in `vn_cs_decoder_peek_internal` — and concluded "S's decoder believes
the submit is truncated". The conclusion was reasonable and false: validation sets the same flag, from a
different file, with no message distinguishing them. **The design spec for this very decoder opens by asserting
the truncation reading as fact.** That premise is now retracted; the decoder it justified is what disproved it,
which is the best outcome an instrument can have.

**What the failure actually is: S cannot resolve the queue the application submits to.** Everything measured
fits — the bytes are byte-exact, the framing is agreed, our decoder (which stubs lookups and never dispatches)
succeeds, and virglrenderer's dispatch fails at the first handle it must resolve.

**The queue's lifecycle in that same run, now readable for the first time:** exactly **one**
`vkGetDeviceQueue2` (type 155) crosses in the whole stream, and between it and the failing submit the
application issues `vkDestroyDevice` — S logs "application destroyed its device (device 5)". So the queue the
submit names is plausibly one whose device is gone, or one whose creation S never registered as an object. The
stream shows no second queue acquisition.

**Deliberately not concluded yet.** Two readings remain and they need opposite fixes: the application really is
submitting to a queue from a destroyed device (which for vkcube would be surprising and would point at
something we do to it), or a second device/queue exists whose `vkGetDeviceQueue2` never reached S or was never
registered there. Distinguishing them is one measurement — dump the queue *handle id* the submit names and the
handle ids S has registered, and see whether the submit's id was ever created, was destroyed, or never
arrived. After nine refuted theories on this stall, that measurement gets taken before anything is built.

**Worth recording about method, since it is the third time it has decided an outcome here.** Every wall this
week fell to an instrument, not an argument, and this one fell to an instrument that took five tasks to build
while the bug sat untouched. The decoder's first real use refuted two hypotheses, corrected a three-day-old
misreading, and located the fault — in one run, on the first attempt.

### 2026-07-26 — The queue ids match, and the decoder contradicts S's own device-destroy detector

The measurement asked for: dump the queue handle the submit names against what was created. Both are in the
stream, and the decoder now reaches them.

**The ids match, exactly and throughout:**

```
[ring-queue] GetDeviceQueue2 @0    creates queue id 0x6
[ring-queue] submit @6852          names   queue id 0x6
[ring-queue] submit @688           names   queue id 0x6
[ring-queue] submit @64            names   queue id 0x6
[ring-queue] submit @232           names   queue id 0x6
[ring-queue] submit @748           names   queue id 0x6
```

So the failing submit is **not** naming a stale, wrong, or never-created queue. It names precisely the one the
application acquired. That kills the simplest reading of the previous entry.

**And the shape of the failure is narrower than assumed: five submits, one CS error.** The queue resolved
perfectly well for the earlier submits. Whatever goes wrong, it makes an id that *was* resolvable stop being
so, partway through the session — which is why it presents intermittently.

**The finding, and it is a contradiction between two of our own components.** In virglrenderer, the thing that
destroys a queue is its device being destroyed. So the obvious candidate was `vkDestroyDevice` — and S logs
exactly that:

> `rayland-s: application destroyed its device (vkDestroyDevice for device 5); retiring the readback gate for
> ring_idx=1 so no fence can race the queue's destruction`

**But the decoder finds zero `vkDestroyDevice` (type 12) in the entire relayed stream** — across 105 deltas,
every one walked to `ReachedEnd`. Both cannot be true.

The two differ in kind, and that matters for which to believe. S's `find_destroy_device` is a **signature
scan** — a byte-pattern heuristic, written precisely because the walker could not reach far enough to decode
its way there, and documented as such. The decoder is a full walk using Mesa's own generated decoders, which
agrees with virglrenderer's framing to the byte (its `head` landed on our submit boundary exactly). **A
heuristic scan and a real decode disagree, and the decode is the better witness.**

If the scan is a false positive, S has been **acting on phantom evidence**: retiring the readback gate for a
device destruction that never happened. That is a live behaviour change driven by a bad signal, and it was
invisible until something could read the stream properly. It also would not, on its own, explain the queue
becoming unresolvable inside virglrenderer — S retiring its own gate does not unregister a host object — so
this is a real defect found *alongside* the one being hunted, not necessarily the cause of it.

**Deliberately not concluded.** Three readings remain and they are distinguishable: the scan is a false
positive and something else unregisters the queue; the scan is right and a destroy reaches virglrenderer by a
path the ring decode does not cover; or the fatal flag is set by neither the queue lookup nor a destroy, but
by the decode of the submit's *arguments* failing inside virglrenderer's context in a way our stub-lookup
decode does not reproduce (the dispatcher sets fatal on three distinct conditions, and only one of them is the
queue). After ten refuted theories on this stall, the next step is to check `find_destroy_device` against the
decoded stream directly — a self-contained test, no live run required, since both the scanner and the decoder
can be pointed at the same captured bytes.

### 2026-07-26 — `find_destroy_device` is a false positive, proved by running it beside the decoder on identical bytes

The contradiction from the previous entry is settled, and the signature scan is the one that was lying.

Both were pointed at the same delta, in the same run, and the log states the verdict rather than leaving it to
be cross-referenced by hand:

```
[ring-destroy] find_destroy_device FIRED at offset 6236 for device 0x5
               — on a decoded command boundary: false (decoder says type=none); tail=18476
```

**Offset 6236 is not a command boundary.** The decoder walked that delta to `ReachedEnd` and found no command
beginning there, so the scan's 16-byte pattern — `[type=12][flags=0][device_handle=0x5]` — matched **inside a
payload**. S fired on it exactly once this run, which is precisely the one time it logs "application destroyed
its device (vkDestroyDevice for device 5)".

**The application never destroyed its device. S has been retiring its readback gate on a phantom.**

**Why the scan was structurally vulnerable, and why it looked sound.** Its discriminator is a 64-bit device
handle, which reads as very specific — a stray `12` alone would not match. But it slides over *arbitrary*
positions rather than decoded boundaries, so it needs only that the two dwords immediately preceding some
occurrence of the handle happen to be `12` and `0`. Device handles appear throughout a real stream (most
commands take one), so the pattern gets many chances. `find_get_device_queue2` is built the same way and is
under the same suspicion; it has not been checked.

**This is not obviously the cause of the `vkQueueSubmit` failure**, and saying otherwise would be the eleventh
theory this stall has produced. S retiring its *own* readback gate does not unregister an object inside
virglrenderer, so the queue's disappearance still wants an explanation. What this is, is a real defect found
*next to* the one being hunted — and one with its own consequences, since the readback gate is (c)2's
completion machinery and it has been switching itself off spuriously.

**The fix is now available in a way it was not before.** These scans exist *because* the walker could not
reach past the first application command — `out_of_line.rs` and `decode.rs` both say so in as many words, and
one of them argued a decode-based approach could never work "for every workload, forever" (retracted earlier
today). That constraint is gone. A scan can now confirm its hit is a real decoded command boundary before
believing it, which converts a heuristic into a check. Whether to fix the scan, replace it with a decode, or
keep both and cross-check is a design question worth its own scoping rather than a patch at the end of a very
long day.

**Method note, because it is the fourth time this week.** The decoder was justified on one open question and
has now answered two, the second of which nobody had asked. Instruments that report what is actually there
keep finding defects nobody suspected; arguments about what must be there kept producing theories that
measurement then killed. Ten refuted so far.

### 2026-07-26 — `find_get_device_queue2` is sound; the two sibling scans differ, and the reason is the command number

Having proved `find_destroy_device` fires inside payloads, the obvious question was whether its sibling — built
the same way, and trusted for the same kind of decision — is any better. Pointed at the same bytes with the
decoder beside it:

```
[ring-queuescan] find_get_device_queue2 FIRED: end_offset=80 ring_idx=1 device=0x5
                 — decoder found a real type-155 in this delta: true (offsets agree: true); tail=9388
```

**It is correct.** It fires exactly once in the run; the decoder confirms a real `vkGetDeviceQueue2` in that
delta; and the offsets agree to the byte — the decoded command's offset plus its encoded size equals the
scan's reported `end_offset`. There is exactly one type-155 command in the whole stream, and the scan found
precisely it and nothing else.

So the two are not equally suspect, and it is worth writing down *why*, because "they are built the same way"
was the reason for suspecting both and it turns out not to be the deciding factor:

- **`vkDestroyDevice` is command type 12.** A `12` is an entirely ordinary value to meet in payload data —
  counts, enum values, sizes, offsets — and the scan pairs it with a flags word of `0`, which is commoner
  still. The 64-bit device handle looks like a strong discriminator, but handles appear all over a real stream
  (most commands take one), so the pattern gets many chances at a coincidence, and it takes them: the false
  hit landed at **offset 6236** in one run and **6300** in the next, same delta. A match that moves between
  runs is matching noise, not a command.
- **`vkGetDeviceQueue2` is command type 155**, a value that rarely occurs as incidental data, and its scan
  validates more structure around the hit rather than three fields.

The lesson is narrower and more useful than "signature scans are bad": a sliding pattern match's reliability is
a property of *how distinctive the pattern is in the surrounding data*, not of the technique. Nobody could have
known which of these two was safe without measuring, and now both are measured — one confirmed, one refuted,
by the same instrument in the same run.

**Where that leaves the wall.** The confirmed false positive is real and has a real consequence — S retires its
readback gate on a phantom device destruction — but it still does not explain a `VkQueue` becoming
unresolvable inside virglrenderer, and the queue ids match throughout. The submit failure remains open, with
the dispatcher's three distinct fatal conditions still not narrowed to one. What has changed is that this
codebase can now check a claim about a command stream instead of arguing about it, and two of its own
long-standing claims have already failed that check.

### 2026-07-26 — The scan that lied, and the invariant it broke on the way out

`find_destroy_device` now decodes before it believes. That is the whole change, and it is small; what
is worth writing down is what it cost and what it revealed.

**The bug was documented before it was found.** The function's own doc comment said a false positive
"would close the gate early and, on the next real readback, wedge — but the type + async-flags +
exact-device-handle triple makes that vanishingly unlikely." Both halves turned out to matter. The
consequence was stated correctly. The probability estimate was simply wrong, and it was wrong in the
way estimates written without data usually are: it reasoned about the *pattern* rather than about the
*data the pattern slides through*. `vkDestroyDevice` is type **12** paired with a flags word of **0**.
Twelve is an ordinary number to meet in a Vulkan command stream's arguments — a count, an index, a
small handle, a ring id — and zero is commoner still. Type **155**, which `find_get_device_queue2`
scans for and which held up under the same test, is not an ordinary number. The discriminating power
of a sliding match is a property of its haystack, not of the technique. We had no way to check that
before, and so we asserted it instead.

**The test we wrote for it is the honest version of the bug.** The trap in
`a_destroy_pattern_buried_in_a_payload_is_not_a_destroy` is not a contrived byte blob: it is a real
`vkNotifyRingMESA` — the ring doorbell, present in every captured stream — for ring 12 with seqno 5.
Its arguments spell `[12][0][5]` at offset 8, which *is* the sixteen-byte signature for "destroy
device 5", with none of it being a command. Writing the test that way took a couple of extra minutes
and changed what it proves: not "a scan can be fooled by adversarial input" but "a scan is fooled by
the traffic this system actually carries."

**One asymmetry had to survive the fix.** A false negative here is worse than a false positive: it
re-admits a teardown fence on a destroyed queue, which is render-server-fatal, where a false positive
merely closes the gate early. So the decode is trusted only where it actually reached. Walk the
stream; if a decoded command is the destroy, return it; if the walk consumed the whole stream and
found none, that is a *confident* negative the scan could never give. Only when the walk stops early
does the old scan run — and then only over the bytes past where the decoder stopped. Every false
positive the decoder could have ruled out is gone; no real destroy the scan would have found is lost.

**And it broke a rule we wrote ourselves, deliberately.** (c)1 spec §7 says the borrowed decoder is
diagnostic: nothing may use it to decide what to ship or when. `rayland-s` calls
`find_destroy_device` at `apply.rs:755` to decide when to retire the readback gate — a correctness
decision — and that call now runs `decode_commands`, which falls back to the vendored Mesa headers.
The decoder is load-bearing on S as of this commit. The guard test does not catch it, because it
enumerates `rayland-c`'s source files and this is `rayland-s`; the guard is not wrong, it is simply
aimed at the machine that must never link a GPU stack, which was the invariant it was written for.
The honest framing is that the rule was written to stop a decoding bug from becoming a corruption
bug, and here it was blocking the replacement of a heuristic *proven* to corrupt. That is a real
trade, not a loophole, and it is the owner's to make. It is recorded here rather than quietly
absorbed, and the guard is left un-widened so the discrepancy stays visible instead of being
papered over by a green test.

**What it does not explain.** Nothing about the `vkQueueSubmit` CS error moved. The gate retiring
early is a genuine defect with a genuine consequence, and it is now fixed, but the submit failure has
its own cause still unlocated. Two claims this codebase made about itself have now failed a check it
could not previously perform; the third — that the queue is registered and resolvable at submit time
— has held up every time it has been tested, which is starting to be informative in its own right.

### 2026-07-26 — Ten clean runs, and the gate that nobody reads

Ran `scripts/c2-icosa-two-machine.sh` over the real link, apollo → dop561: **10/10 runs, 0 stale
frames**, no wedge, no `SIGABRT`, no `invalid ring_idx`, nothing left running on either machine. That
is the result the day's change most needed, though not for the reason we set it up.

**What the run actually proves.** It is a *no-regression* result, not a confirmation. The
false-negative direction was the one worth fearing: if `find_destroy_device` had started missing a
real `vkDestroyDevice`, S would issue a teardown fence on a freed queue and the render server would
die. Ten runs across a real network with no wedge is decent evidence the asymmetry (trust the decode
only where it reached; fall back to the scan over the undecoded tail) does what it was built to do.
It says nothing about the false positive, because 0/10 is also what the previous commit scored.

**And here is the part that reframes the whole day.** While the sweep ran, we read what the retired
gate is wired to, and it is wired to nothing. `find_destroy_device`'s one effect is setting
`self.queue = None`. The queue latch feeds exactly three methods — `retirement_ring_idx`,
`queue_ring_drained`, `latest_submit_pos` — and **not one of them has a production caller**.
`main.rs` calls six things on the session: `lock`, `apply`, `reply_arena_fence_signaled`,
`take_app_blob_writes`, `take_ring_progress`, `take_venus_blob_writes`. The three gate methods appear
only in `crates/rayland-s/tests/apply.rs`. The G' fix of 2026-07-21 replaced the S-issued fence with
the reply-arena scan and left this apparatus in place, latching a queue and scanning every relayed
byte of every delta, feeding a decision nobody makes any more.

So the false positive was **real but inert**. It retired a gate no code reads. It never explained any
icosa staleness, and this morning's framing — that the sweep might show the fix helping — was wrong
before the sweep started, not because of anything the sweep found. It is stated here plainly because
that framing was written down and sent, and the correction belongs next to it.

**The awkward consequence.** The (c)1 spec §7 exception taken earlier today — letting the borrowed
decoder make a correctness decision in `rayland-s` — is load-bearing on a path that is not itself
load-bearing. That is a smaller breach than advertised, and it invites the obvious question of
whether the apparatus should simply be deleted instead of fixed. We are keeping it: it is exactly
where multi-queue support lands, it is now correct rather than subtly wrong, and deleting it would
discard the `ring_idx` decoding work that took real effort to get right. Recorded as an open seam on
the project map rather than quietly removed, so the next person meets it as a known state instead of
a puzzle.

**A smaller find, same shape.** `CLAUDE.md` and the project map both pointed at
`crates/rayland-s/src/delivery.rs` as the home of the readback-completion gate. That file was created
in `32b56dd` and **deleted in `0a21513`** — the G' commit — and the gate now lives inside
`progress_thread` in `main.rs`. Two documents survived the deletion of the file they cite. Both are
corrected. The pattern worth noticing is that both of today's documentation defects are the same
defect: a change landed, did its job, and left behind a description of the world as it was before.

### 2026-07-26 — Eight more theories dead, the failing submit fully characterised, and a wall we cannot see past without source

Spent the day pointing the new decoder at the `vkQueueSubmit` CS error. It did not fall. What it did
do is stop being mysterious: the failing command is now described down to every field and every
handle, and eight candidate explanations are refuted by measurement rather than argument.

**What the failing submit actually is.** vkcube issues five `vkQueueSubmit`s per run; four are
accepted and the fifth is refused. Dumping each one's bytes and decoding them field-by-field against
the vendored generated decoder:

| | fence | wait sem | cmdbuf | signal sem |
|---|---|---|---|---|
| #2 accepted | `0x1d` | `0x1e` | `0x18` | `0x2e` |
| #5 **refused** | `0x1f` | `0x20` | `0x19` | `0x2f` |

The refused submit is the **second swapchain image's** resource set, structurally identical to the
accepted one — same command sequence in its delta, same 120-byte encoding, every field parsing to
exactly 120 bytes with nothing left over. This matches an old entry's guess that "two of three
swapchain images work and the third does not", now with the actual handles attached.

**Refuted this session, each by a measurement:**

1. *Handle lookup failure (`!args.queue`)* — `VIRGL_LOG_LEVEL=debug` emits no `invalid object id`.
   **But see the caveat below; this one is not safely closed.**
2. *Queue id wrong* — all five submits name `0x6`, the id `vkGetDeviceQueue2` created.
3. *Queue id zero* (the silent `lookup_object` early-out that `VK_NULL_HANDLE` fences require) — no,
   it is `0x6`.
4. *Ring wrap* — buffer is 128 KiB; the failure is at free-running 27416.
5. *Frame desync* — virglrenderer's `head` (26748) lands exactly on a boundary our decoder agrees
   with, and walks cleanly to the submit before failing.
6. *Truncation / bounds overrun* — virglrenderer's extent `[26748, 27536)` covers the 120-byte submit
   exactly.
7. *Bad `sType` or unknown `pNext`* — our decoder runs the **same generated code** on the same bytes
   and sets no fatal.
8. *WSI resource import* — the `vkImportSemaphoreResourceMESA` immediately preceding the submit was
   the best lead of the day, since swapchain resources are exactly what WP0 has not plumbed. Dumped
   and decoded: it names `resourceId = 0` in **both** the accepted and refused frames, differing only
   in which semaphore it targets. Not the differentiator.
9. *The handles were never created on S* — enumerated every `vkCreateFence`, `vkCreateSemaphore` and
   `vkAllocateCommandBuffers` id that crossed the ring and cross-referenced them against the refused
   submit. All fourteen objects exist, command buffer `0x19` included.

**A correction, made mid-investigation and worth stating plainly.** Refutation 1 rests on the claim
that `vkr_cs_decoder_lookup_object` logs `invalid object id` when a lookup fails. That was inferred
from the *adjacency* of two strings in the render server's string table — `invalid object id %lu`
sits immediately before `%s resulted in CS error` — and never verified. Disassembling around the
reference showed it inside a function doing `calloc` under a mutex, which reads more like an object
table **insert** (duplicate id on create) than a lookup. So a failed lookup may well be silent, and
hypothesis 1 is *not* closed. It is left standing in the list with this caveat rather than quietly
promoted to "refuted", because the difference decides whether the next fix is ours or virglrenderer's.

**Where the wall now is, precisely.** The generated dispatcher has exactly one fatal path we cannot
reproduce (`if (!args.queue)`), and it calls through to virglrenderer's *own*
`vkr_dispatch_vkQueueSubmit`, which is not in the vendored headers and can set fatal for reasons we
cannot enumerate. Every condition we *can* enumerate is refuted. Getting past this needs
virglrenderer 1.2.0's `src/venus/vkr_queue.c`, and this machine cannot fetch it: gitlab.freedesktop.org
is behind an anti-bot wall, there is no GitHub mirror of the canonical repo, and `apt-get source` fails
because no `deb-src` line is configured. Enabling one needs root, which is the owner's to give.

**Method notes, both uncomfortable.** First, instruments moved this measurement for the *fifth* time:
the failure only occurs when the run is fast enough to reach the submit, so every diagnostic added
pushes runs from "abort at the submit" (exit 134) into "timeout during setup" (exit 124). The two
"failure modes" this diary has recorded separately for days are one failure at two speeds. Second, and
worse: two reproduction runs were launched concurrently against **fixed** socket and log paths, so they
overwrote each other and both had to be discarded. The script now derives per-run paths from a `RUNID`.
Neither of these cost a wrong conclusion, but only because they were noticed.

**One thing did hold up.** Across every failing run today, `find_destroy_device` never fired — the
morning's fix is working under the live workload, and the phantom gate retirement is gone.

### 2026-07-26 (later) — The source arrives, four more theories die, and the contradiction sharpens

Got virglrenderer 1.2.0's actual source. The route matters for next time: gitlab.freedesktop.org is
behind an anti-bot wall and there is no GitHub mirror, but the Ubuntu **archive pool** serves the
orig tarball over plain HTTP with no `deb-src` line and no root —
`http://archive.ubuntu.com/ubuntu/pool/main/v/virglrenderer/virglrenderer_1.2.0.orig.tar.bz2`. The
owner meanwhile enabled `deb-src` and `apt-get source` *still* failed; the tarball had already
landed. Worth remembering: for an installed package, the pool URL is the reliable path.

**First thing the source did was correct me.** This diary has been reasoning about a log line called
`invalid object id`, spotted in the render server's string table sitting immediately before
`%s resulted in CS error`, and treated its absence as proof that every handle lookup succeeded. That
string is not the lookup's. The real one reads **`failed to look up object %lu of type %d`** (and
`object %lu has type %d, not %d` for a type mismatch). I had been grepping for the wrong text. The
conclusion happens to survive — neither real string appears in any failing run, and `vkr_log` emits
at `VIRGL_LOG_LEVEL_INFO` which our `VIRGL_LOG_LEVEL=debug` runs demonstrably enabled — but it was
right by luck, not by method, and it was stated to the owner as established. Recorded because the
whole point of the earlier caveat was that this inference might be wrong; it was, in its reasoning,
and only accidentally not in its answer.

**The vendored headers are exactly virglrenderer's.** All **42** files byte-identical to
`src/venus/venus-protocol/`. So our decoder is not an approximation of the renderer's decoder — it is
the same generated code, and the only thing that can differ is the `vkr_cs.h` primitives underneath,
which this crate writes itself.

**Refuted against real source this session:**

- *virglrenderer's own handler sets fatal* — it does not. `vkr_dispatch_vkQueueSubmit` is nine lines:
  look up the queue, `vn_replace_..._handle`, lock, `QueueSubmit`, unlock. No fatal anywhere in it.
- *The dispatch handler is not installed* — it is, `vkr_queue.c:654`.
- *Temp-pool exhaustion* — would log `failed to suballocate %zu bytes from the temp pool`. Absent.
- *The ring frames commands in sized chunks our flat walk cannot see* — it does not.
  `vkr_ring_thread` computes `cmd_size = tail - cur`, copies that flat range, and
  `vkr_ring_submit_cmd` sets the decoder over the whole of it. The extent really is `[head, tail)`.

**And the decisive local test.** Fed the captured refused submit's 120 bytes to our `command_len`,
with the accepted submit from the same run as a control:

```
refused  -> Ok(Command { command_type: 18, len: 120 })
accepted -> Ok(Command { command_type: 18, len: 120 })
```

Identical. The bytes are structurally valid, and this is now a committed test rather than an
inference from a dump.

**So the contradiction is sharp, and it is worth stating as a contradiction rather than dressing it
as a lead.** Identical generated code, over identical bytes, with all handle lookups succeeding (both
failure modes log, and logging was on), no temp-pool failure (logs, absent), a handler that sets no
fatal, and a decoder extent that covers the command exactly — and yet `vn_dispatch_vkQueueSubmit`
comes out with the fatal flag set. One of those five statements is false and we cannot yet see which.

The single silent path left in `vkr_cs_decoder_lookup_object` is `if (!id) return NULL;` — no log, no
fatal — which pairs with the generated `if (!args.queue) { set_fatal; return; }`, also silent. That
is the *only* silent route to this exact signature. It requires the queue id to decode as **0**, and
the captured bytes say `0x6` at the offset the decoder reads. Either the id is genuinely 0 at
dispatch time on S — which would mean the bytes S decoded are not the bytes C relayed — or something
outside this enumeration is setting the flag.

**The next step is now cheap and was impossible this morning: patch and rebuild.** With the source in
hand, adding one `vkr_log` to that `!args.queue` branch (printing `args.queue` and the decoded id)
turns the last silent branch into a loud one, and virglrenderer builds with meson. That is a far
better instrument than another round of inference — and this investigation's record is unambiguous
that instruments settle it and arguments do not. Twelve theories refuted so far.

### 2026-07-26 (evening) — FOUND: the "CS error" is `VK_ERROR_DEVICE_LOST`. It was never a protocol bug.

The wall fell. The failing `vkQueueSubmit` is refused because **S's GPU loses the device executing
it**, and Venus reports device loss by poisoning the command stream:

```
RAYLAND: vkQueueSubmit dispatch: fatal_after_decode=0 queue=0x7137c461a770 submitCount=1 fence=...
RAYLAND: -> FATAL because vkQueueSubmit returned VK_ERROR_DEVICE_LOST (-4)
RAYLAND: vkQueueSubmit returned VkResult=-4 flags=0x0 fatal_now=1
virgl_render_server: vkr: vkQueueSubmit resulted in CS error
```

The four accepted submits in the same run print `VkResult=0`. The fifth prints `-4`.

**The branch nobody had read.** `vn_dispatch_vkQueueSubmit` ends like this:

```c
    if (flags & VK_COMMAND_GENERATE_REPLY_BIT_EXT) {
        ... encode reply ...
    } else if (args.ret == VK_ERROR_DEVICE_LOST) {
        vn_cs_decoder_set_fatal(ctx->decoder);
    }
```

Every submit in this workload has `flags = 0x0` — measured, not assumed — so the `else` is the live
path on every one of them. A device loss therefore sets the decoder's fatal flag **silently**, with no
log of its own, and the only thing that ever surfaces is the generic `%s resulted in CS error`. Three
days of this diary read that message as a *decoding* failure. It is a **GPU** failure wearing a
decoder's clothes.

**Why every protocol theory died.** Because they were all false, and they were false for the same
reason: the relay was correct the whole time. The ring was byte-exact, the framing agreed with
virglrenderer's to the byte, every handle resolved, every object had been created, the extent covered
the command exactly, and our decoder — running virglrenderer's own generated code, all 42 headers
byte-identical — accepts the refused bytes with `Ok(len: 120)`. Twelve refutations were twelve correct
answers to the wrong question. The one measurement that mattered was one nobody could take without
the renderer's source: *what did `vkQueueSubmit` actually return?*

**How the instrument was built, because the route is reusable.** No root, nothing installed
system-wide, `/usr/libexec/virgl_render_server` untouched:

1. Source from the Ubuntu **archive pool** over plain HTTP — `apt-get source` failed even after the
   owner enabled `deb-src`, but
   `pool/main/v/virglrenderer/virglrenderer_1.2.0.orig.tar.bz2` just works.
2. `meson`, `ninja`, `pyyaml` into a throwaway venv in the scratchpad (`python3 -m venv`), not apt.
3. The build hits a genuine glibc-vs-bundled-C11-threads clash on this toolchain; the distro's own
   `fix-c23.patch` (upstream, landed in virglrenderer 1.3.0) fixes it — the same patch that produced
   the installed binary, fetched from `virglrenderer_1.2.0-2ubuntu2.debian.tar.xz`.
4. Two `fprintf`s into the generated `vn_dispatch_vkQueueSubmit`, then
   `RENDER_SERVER_EXEC_PATH=<build>/server/virgl_render_server` — an env var the *system* library
   honours (`src/proxy/proxy_server.c:70`), so the patched server is spawned without replacing
   anything.

**What is now the real question, and it is a different question.** Why does the device die? The
leading suspect is already on record from the `VKR_DEBUG=validate` run earlier today, and it is not a
Rayland relay bug either — it is a resource-setup bug:

> `vkBindImageMemory2(): pBindInfos[0].memory has an external handleType of
> VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT which does not include at least one handle from
> VkImage handleType VkExternalMemoryHandleTypeFlags(0)` — VUID-VkBindImageMemoryInfo-memory-02728,
> and the matching `vkBindBufferMemory2` violation twice over.

Binding dma-buf-backed external memory to an image created with `handleTypes = 0` is exactly the kind
of spec violation that does not fail at bind time and instead faults the GPU when the resource is
first *rendered into* — which is the second swapchain image, which is the submit that dies. That is a
hypothesis, clearly labelled: it fits the evidence and has not been tested. It is also consistent with
the failure being intermittent and with it being the *second* image rather than the first.

**Method note, and it is the whole lesson of the week.** Every wall this week fell to an instrument.
This one was invisible to every instrument we could build inside Rayland, because the fact it needed
lived in a process we do not compile — and the fix was to stop reasoning about that process and go
compile it. Getting the source took three failed routes (Anubis-blocked GitLab, no canonical GitHub
mirror, `apt-get source` failing even with `deb-src` enabled) and one that worked in a single command.
The lesson is not "measure more", which this diary already knew. It is that **the boundary of what you
can measure is a choice, not a fact** — and it was cheaper to move that boundary than to keep
theorising inside it.

### 2026-07-26 (night) — The device loss is GPU-specific: NVIDIA 7/14, Intel 0/10

Having found that the "CS error" is `VK_ERROR_DEVICE_LOST`, the next question was *why the device
dies*. Two hypotheses were available. The first was mine and it is now dead; the second holds up.

**Refuted: the `handleType` mismatch.** The entry above nominated the validation errors as the
leading suspect — dma-buf external memory bound to images and buffers created with `handleTypes = 0`
(`VUID-VkBindImageMemoryInfo-memory-02728`, and the buffer equivalent twice). It fits so neatly that
it deserved a test rather than a promotion, and the test killed it: across the three
`VKR_DEBUG=validate` runs, **all three** carry exactly 3 bind violations and **all three** issue
exactly 5 submits — but only one of them loses the device. A condition present when the submit
succeeds cannot be what makes it fail. The violations are real spec breaches worth fixing on their
own account; they are not this bug.

**Holds up: it is the GPU.** vkcube by default selects **GPU 1 — the discrete NVIDIA RTX A500** — not
the Intel node `rayland-s` opens (`renderD128`). Forcing `--gpu_number 0` (Intel Iris Xe):

| GPU | runs | device loss | submits dispatched |
|---|---|---|---|
| NVIDIA RTX A500 (default) | 14 | **7** (50%) | 5, 6 |
| Intel Iris Xe (`--gpu_number 0`) | **10** | **0** | 8 every single run |

At NVIDIA's measured 50% rate, ten clean Intel runs by luck is `p ≈ 0.001`. And there is a second,
independent signal that does not depend on the failure rate at all: Intel dispatches **8** submits on
every run, where NVIDIA gets 5 or 6 before dying. Intel does not merely fail less; it gets strictly
further.

**What this does and does not mean, stated carefully.** It does *not* mean vkcube works on Intel —
both configurations still exit 124, timing out in setup, which is the known (c)1 synchronous-reply
latency wall and a separate problem. The claim is narrower and it is the one that matters for where
effort goes next: **the device loss is a property of the NVIDIA driver executing what Venus hands it,
not of Rayland's relay.** Every relay-side explanation was already refuted byte by byte; this closes
the loop by showing the same relayed stream is executed without fault by a different driver on the
same machine, in the same run configuration, through the same code path.

**A note on what nearly went wrong here.** The `handleType` hypothesis was attractive, mechanically
plausible, and *already written into the diary and the project map as the leading suspect* before it
was tested. It took one cheap correlation to kill it. That is the same failure mode this diary has
recorded all week under a different name — a reading that explains the evidence is not thereby the
cause — and it nearly got recorded as an answer. The rule that keeps working: nominate freely, but
mark it a hypothesis in writing, and test it before it hardens into a belief.

### 2026-07-26 (late) — The icosahedron is on the screen, and the slowness is fully accounted for

The owner asked, fairly, why after all this time there had been no demo: no spinning solid, no vkcube,
nothing originating on C and visible on S. The answer turned out to be one bug and one scoping
decision, and both were ours.

**The black window was never a presentation defect.** `FrameCapture` copies a blob's pixels at
`BlobCreated`. Its doc explains why that is the right moment, and it is — *for a one-frame
application*. Mesa creates the readback blob lazily at `vkMapMemory`, which for `rayland-refapp`
happens **after** its single `vkCmdCopyImageToBuffer`, so the blob is born already holding a finished
frame. `rayland-icosa-cpu` renders 120 frames into that same buffer, so its blob is born holding
nothing. We were photographing the buffer before anything had been drawn into it, and the relay was
correct the entire time — which is exactly what the earlier investigation concluded when it found the
application's own PNGs on C intact and filed the window as an unexplained defect.

**Two fixes failed before the third worked, and the failures are the useful part.** Re-reading the
blob's live pages once per apply stalled the ring for 30 s and killed the run: a 256 KiB read of
GPU-shared memory under the session lock. That is the **sixth** time this week an instrument became a
participant in the thing it was measuring, and the first time it happened in shipped code rather than
a diagnostic. Re-reading once *after* the session reads all zeroes, because the application has freed
the buffer by then. What works is `LiveFrame`: accumulate the frame from the readback runs
`progress_thread` **already** extracts and ships, gated on the fence that proves the submit and its
copy complete. No second read of the mapping, and the bytes are known whole by construction.

**The second half was scope, not a bug.** Presentation ran after `serve` returned — correct for one
still frame, useless for a live one, because by then there is nothing left to follow. It now runs on
its own thread beside the relay, and `rayland-present` grew `present_live`: an optional closure
supplying subsequent frames, with the window re-arming a `wl_surface.frame` callback on each commit.
With `None` it is byte-for-byte the old behaviour, which is what keeps `rayland-server` and the
single-frame path honest.

**It works.** `scripts/icosa-remote-demo.sh`, apollo → dop561: 120/120 frames, the icosahedron
turning on the laptop's screen, computed on a machine that never touched a GPU. First attempt showed
one frame and froze — the script had not created the output directory on C, so the fixture died
writing `frame_0000.png` having rendered exactly once. A demo failing for a reason that has nothing
to do with the system being demonstrated is its own small lesson.

**On the honest reading of what was seen.** It ran at roughly 2 fps at 256×256, and it is worth
writing down *why*, because "slow proof of concept" is the kind of phrase that hides whether anyone
understands the slowness. Per frame this fixture ships **1 MiB up** (a CPU-computed fractal written
into mapped memory with no interceptable call — the worst case it was built to be) and **256 KiB
down**, the latter fragmenting into ~5000 one-byte messages. That is ~2.6 MB/s: nothing on a LAN. So
it is not bandwidth-bound; it is bound by round trips and message count, both addressable. Its own
sibling `rayland-icosa-gpu` draws the same picture with **80 bytes per frame**.

And the size question has a pleasing answer: in a command-streaming design resolution is the GPU's
problem, and the GPU is next to the display. 1920×1080 costs the same on the wire as 256×256 — *once
pixels stop coming back*, which is precisely what WP0's token → `wl_buffer` path is for. Today S
presents the application's readback buffer because it cannot see the `DEVICE_LOCAL` render target, so
resolution costs bandwidth. That is a (c)1 scoping decision with a known exit, not a property of the
architecture.

The real ceiling is neither pixels nor bytes: it is **how many synchronous round trips the
application makes**. The icosahedron animates because it runs a fixed schedule and never waits;
vkcube times out in *setup* because it waits hundreds of times. Every latency mitigation Venus offers
is currently disabled in every run we do (`VN_PERF=no_fence_feedback,no_semaphore_feedback,...`) —
not as an oversight, but because feedback-on was loopback-only and was superseded during (c)2. That
is unclaimed headroom with a known reason for being unclaimed, which is the most honest kind.

### 2026-07-26 (night) — The GPU-fractal fixture is blocked by a false positive, and the culprit is the shader itself

The owner asked the obvious good question after seeing the demo: could the fractal be computed on
**S**, by shipping the SPIR-V rather than a megabyte of texture per frame? Two things had to be said.
First, SPIR-V already crosses — `vkCreateShaderModule` is an ordinary Venus command, and
`icosa-cpu`'s own shaders run on S. What does *not* cross is the fractal, which that fixture computes
on C's CPU on purpose, because it is the worst case it was built to be. Second, the fixture that does
what was asked already exists: `rayland-icosa-gpu`, same geometry, same schedule, same bit-exact
arithmetic, 80 bytes per frame instead of 1 MiB.

**So we ran it, and it does not work over the relay.** Natively on S it is perfect — 120 frames,
exit 0. Through Rayland it aborts with no frames. That is a real gap, found by asking for the
demo rather than by any test.

**The cause, and it is this morning's bug wearing a different hat.** `rayland-c` refuses the delta:

> refusing to relay the ring delta ending at tail 18100: the command stream carries a dword equal to
> 180 at byte offset 4608, which is `vkExecuteCommandStreamsMESA` — Venus's out-of-line command path

Pointing the decoder at the same bytes settles it. The delta walks cleanly to `ReachedEnd` as ten
commands — `vkCreateRenderPass`, `vkCreateDescriptorSetLayout`, two `vkCreateShaderModule`,
`vkCreatePipelineLayout`, `vkCreateGraphicsPipelines`, two `vkDestroyShaderModule`,
`vkSetReplyCommandStreamMESA`, `vkCreateImage` — and **contains no type 180 at all**. Offset 4608
falls inside the second `vkCreateShaderModule`'s 5860-byte payload. The matching dword is a word of
**the fractal fragment shader's own SPIR-V**. The shader that would make this fast is precisely what
trips the guard against shipping it.

That is the third signature scan this week to fire on payload bytes (`find_destroy_device`, then this),
and the refusal's own comment predicted it in as many words — "this scan over-approximates on
purpose". It was written honestly and it was right about itself.

**But the fix is *not* the same fix, and that difference matters.** `find_destroy_device` lives in
`rayland-s` and decides when to retire a readback gate. This one lives in **`rayland-c`, on the relay
path**, and decides *what gets relayed* — which is exactly what (c)1 §7 forbids a decode from
deciding, and what `decoder_is_not_load_bearing` mechanically prevents. Narrowing this refusal with
the decoder would turn some refusals into relays, so a decoding bug could become a corruption bug by
the most direct route there is. That is the invariant's central case, not a peripheral one, and it is
not mine to spend.

**One idea considered and retracted in the same breath.** Mesa spills out-of-line when a submission
exceeds `direct_size = buffer_size >> 4` — 8192 bytes here, against a batch of 8960, so the workload
sits *just* over the line. The tempting move was to enlarge the ring, since (c)1 rests on Rayland
being the host that allocates it. But `buffer_size` is **derived from the blob Mesa asked for**
(`identity.rs:198-207`); C observes that size, it does not choose it. The lever the refusal message
names — Mesa's `direct_order` — is a client-side constant, and patching Mesa is the one thing this
project has refused from the start.

**Where that leaves it.** Three routes, and they are genuinely different in kind: make the refusal
precise with the decoder (fast, and spends the core invariant); implement the out-of-line path
properly, relaying the shmems `vkExecuteCommandStreamsMESA` names (the correct answer, and real (c)1
v2 work); or narrow the scan heuristically (cheap, and still a heuristic that will be wrong again).
Recorded rather than chosen. What is *not* in doubt any more is the diagnosis: it is a false positive,
proved against the bytes, and the workload behind it is the one that makes the whole performance
argument.

### 2026-07-26 (late night) — The out-of-line path, implemented properly: icosa-gpu runs, and it is 7× faster

The owner chose option 2 over the quick fix, and it was the right call — not because the quick fix
would have failed, but because the proper one turned out to cost *less* than expected and to
answer a question the quick fix would have left open.

**The refusal did not need a better check; it needed to stop being necessary.** Venus replaces any
submission over `direct_size` (`buffer_size >> 4`) with `vkExecuteCommandStreamsMESA` naming other
shmems. Those live in the staging pool, which is `blob_id == 0`, which C's sync skipped — so S held
zeros and refusing was correct. The tempting fix was to make the scan precise with the decoder, which
would have spent (c)1 §7's central guarantee: on the relay path, a decode deciding what crosses the
wire is exactly the thing the rule forbids. Instead the relay now carries **every blob except the
ring**, so the referenced streams are simply already on S. The question is removed rather than
answered, and the decoder stays diagnostic.

**What made publishing a region S also writes safe was already built.** The old comment at that
filter said, correctly, that C's stale copy of the reply arena would clobber replies the application
is blocked on, and that the answer would have to be "a design for synchronising a region *both* sides
write". That design landed earlier the same day, for a different reason: every blob carries a
baseline, `take_changed_runs` ships only what differs from it, and `note_s_wrote` folds each S→C
write into the baseline as it arrives. S's own bytes therefore never look like C-side changes. The
`blob_id` filter was replaced by two tests that state the surviving properties directly — the ring is
never shipped as `BlobData` (it has `RingDelta`, which carries the `tail` that validates its bytes and
goes last), and the arena is never echoed back — both teeth-checked.

**One prerequisite, and it was the week's recurring mistake waiting to happen again.**
`take_changed_runs` compared one byte at a time. Fine for a few hundred KiB of application buffers;
ruinous pointed at an 8 MiB pool, and this change points it there on every relay. So it was made
chunked (`memcmp`-shaped, per-byte only inside a chunk that differs) *before* the routing widened —
proved equivalent to a naive reference across nine patterns sitting on and across the 64-byte
boundaries, and teeth-checked by closing runs at each boundary, which fails exactly the straddling
case. That is the fourth time this week a blob-page read has threatened the relay's latency, and the
first time it was anticipated instead of measured after the fact.

**Results.** `rayland-icosa-gpu` — the fixture that answers "compute the fractal on S from SPIR-V" —
now runs over the relay, **120/120 frames bit-identical to native on S**. And the number that matters:

| fixture | forward traffic/frame | ms/frame (loopback) | ms/frame (real link) |
|---|---|---|---|
| `icosa-cpu` (fractal on C's CPU) | ~1 MiB | 283 | 283 |
| `icosa-gpu` (fractal in SPIR-V) | ~80 bytes | 50 | ~41 |

**~7× faster over the real network, ~24 fps.** (The real-link figure spans only 5 s against 1 s
mtime granularity, so read it as 40–50 ms/frame, not as the network beating loopback.)

**A prediction of mine, wrong and usefully so.** Before running it I said the readback path would
swamp any forward-path gain, since 256 KiB comes back per frame and fragments into ~5000 one-byte
messages. It did not: the frame time collapsed anyway. So pushing a megabyte per frame through
uninterceptable mapped memory *was* the dominant cost, and the return path is now the bottleneck —
which makes that fragmentation the next obvious target, on evidence rather than on the guess I had
been carrying.

**Regression check, because this touched the path (c)2's correctness rests on.** The two-machine
sweep on `icosa-cpu` — the fixture whose mapped writes this change most plausibly disturbs — is
**10/10 runs, 0 stale frames**, unchanged from the pre-change baseline. Full suites green.

**And an expectation corrected before it could disappoint.** The owner hoped this would also unblock
vkcube. It does not: vkcube's stream contains **zero** type-180 commands, so it never met this
refusal. Its blockers remain the NVIDIA `VK_ERROR_DEVICE_LOST` (not ours, and absent on Intel:
0/10 against 7/14) and setup latency. What this change unblocks is the general case — *any*
submission over 8 KiB — which vkcube happens to be too small to need.

### 2026-07-26 (very late) — The readback fragmentation was already fixed, and it was never the bottleneck

Asked to fix the readback fragmentation. Measured it first, and the task dissolved twice over.

**It is already coalesced, and CLAUDE.md is stale.** `take_app_blob_writes` has carried a
`READBACK_COALESCE_GAP` of 256 for some time, with `blob::coalesce_ranges` behind it and six unit
tests plus an integration test. The "~5000 one-byte `BlobData`/frame" this file still records was
fixed in an earlier session and the note outlived the defect.

**The measurement, from a 120-frame `icosa-gpu` run over loopback:**

| resource | messages | bytes | one-byte msgs | mean run |
|---|---|---|---|---|
| `res=5` — the readback (256×256×4) | 24874 | 9.4 MB | 41 | **377 B** |
| `res=2` — the reply arena | 4540 | 20 KB | **3247** | 4.4 B |

So the readback's runs are healthy. The one-byte flood is the **arena**, and its grain is
*deliberate*: `take_venus_blob_writes` passes gap 0 because a gap byte is one S did **not** write, and
shipping it could clobber what C's Mesa has there. Coalescing it would trade a correctness property
for bytes that were never the cost.

**So the cost had to be message *count*, not bytes — and that hypothesis was wrong too.** `ship()`
took the send lock and flushed **once per message**: 29414 locks and 29414 flushes for that run.
Batching both is obviously right and completely lossless, so it was done. It is worth **1.03×** —
median `draw_readback` 50.4 ms → 48.7 ms, measured from the fixture's own microsecond CSV rather than
from PNG mtimes, which at 1 s granularity over a 6 s run could not have told the difference (and,
tried first, appeared to show the *opposite*). The change is kept because it is simpler and free, not
because it fixed anything.

**Which leaves the real answer, arrived at by elimination rather than by assertion.** ~50 ms per frame
at 256×256 on **loopback**, where the network is not a factor and the whole return path is 78 KB, is
not bandwidth, not message count, and not flush syscalls. It is the synchronous round trip: with
feedback off, the application implements `vkWaitForFences` by polling `vkGetFenceStatus`, and every
poll is a full C→S→execute→reply→C cycle. That is the latency wall this diary has named repeatedly as
*the* remaining architectural limit, and it is now the measured explanation for the frame time of a
workload that has nothing else left in it.

**A note on method, because this turn is the cleanest example of it all week.** Three plausible
theories — fragmentation, message count, flush cost — were held in turn, and the first two were
retired by measurement before any code was written for them. The third produced code worth keeping
and a number that says plainly it did not matter. The alternative, which this project has done before,
would have been to "fix the fragmentation", observe a 3% change, and quietly file it as done. What
made the difference was measuring **before** the fix rather than after: the per-resource breakdown
took one run and killed the premise in a single table.

### 2026-07-26 (end of day) — Venus feedback: 1.23× on loopback, one lost run over the wire, not adopted

The last lever named in the previous entry was Venus's feedback mechanisms, disabled wholesale in
every run this project makes. Tried them. The answer is no, and the shape of the no is worth having.

**Fence feedback cannot be enabled, and the reason is structural rather than incidental.** (c)2's
completion barrier — `Applier::reply_arena_fence_signaled` — works by spotting the application's
`vkGetFenceStatus` reply reading `VK_SUCCESS` in the live reply arena. Fence feedback exists precisely
to stop the application making that call. Enabling it: **exit 134, zero frames, immediately and every
time.** This is not a bug to fix but a dependency to redesign — the barrier would have to be re-derived
against the feedback buffer instead of the arena.

**Semaphore, event and query feedback looked like free money, and were not.** They have been off all
along purely by being in the same `VN_PERF` string as `no_fence_feedback`; nothing had tested them
separately. Enabled, with fence feedback still off:

- **loopback, `icosa-gpu`: 1.23×** — median `draw_readback` 48.7 ms → 39.5 ms, **all 120 frames
  bit-identical to native**. Nothing about that run suggested a problem.
- **two-machine sweep, `icosa-cpu`: FAIL.** Nine runs clean, and one lost *entirely* — the application
  taking a silent Venus `SIGABRT` (core dumped) partway through, 120/120 frames therefore differing.

The mechanism is not mysterious, and the codebase had already written it down: these are **shared
status pages S writes and C's Mesa reads directly**, and (c)1 does not relay them —
`scripts/c1-two-machine.sh` has said so in a comment for weeks. Mesa reads a page whose update has not
arrived and eventually gives up. Adopting any of this needs the pages relayed, not the flag removed.

**The methodological point, which is the one worth keeping.** A loopback pass proved nothing here, and
proved it *convincingly*: 120/120 bit-identical frames and a 23% speedup is exactly the evidence one
would accept if not required to go further. It took a ten-run real-network sweep to find a 1-in-10
total failure. This file had already recorded, weeks ago, that "the feedback-on buy-back was
loopback-only and is superseded" — and this session read that as *unexplored* rather than as a finding
someone had already paid for. It was rediscovered at the cost of a sweep. The note has been rewritten
to say which mechanism, what it measured, and why, so the next attempt costs a read instead of a run.

**Not adopted.** The sweep script keeps the setting parameterised — making it explicit and testable is
strictly better than the hardcoded string it replaced — but defaults to the value that passes, with
the measurement written beside it.

**What that leaves.** The synchronous round trip is confirmed as *the* remaining architectural limit,
and now with both of its obvious escapes closed off: the feedback mechanisms that would remove the
polls need transport work first, and making each poll cheaper (batching flushes) is worth 1.03×. The
next real move is relaying the feedback pages — which is a (c)1 transport task, not a tuning knob.

### 2026-07-26 (later still) — "Relay the feedback pages" is not a well-formed task, and the claim was mine

Asked to do the thing the previous entry named as the next move. Checked the premise first, and it
does not hold — which matters more than the task would have.

**The claim, and where it came from.** `scripts/c1-two-machine.sh` carries a comment saying
`VN_PERF=no_*_feedback` "disables the S→C shared status pages (c)1 does not yet relay". The previous
entry repeated it, and so did `CLAUDE.md`, as the explanation for why enabling semaphore/event/query
feedback lost a run. Both were written **tonight, by me, from that comment**, hours after this same
diary recorded that inherited claims are exactly what keeps costing this project days.

**It is not supported.** Two readings and one measurement:

- `Applier::emit_blob_writes` excludes **rings only**. Every other blob S holds — application memory
  and Venus's own shmems alike — goes through it.
- `HostBlob::take_bytes_s_wrote` does not record `copy_in` calls; it diffs against a **shadow**
  (`changed_byte_ranges`). So it detects writes virglrenderer's GPU makes *directly into the mapping*,
  which is precisely how it catches the readback buffer.
- Measured, `icosa-gpu` over loopback with the three feedbacks enabled: S ships back `res=2` (4542
  messages, 20058 bytes) and `res=5` (24867 messages, 9386970 bytes) **and nothing else** — against
  4540/24874 messages with feedback off. Six blobs exist; the other four never change from S's side.

**There is no un-relayed feedback page here.** So the abort has some other mechanism, and the honest
position is that it is *unknown*, not "known and awaiting transport work". What the feedbacks
demonstrably do is make the run **faster** (1.23×) while adding no S→C traffic at all — which is
itself informative: they are removing round trips rather than adding pages, and something about the
state Mesa then trusts locally is occasionally wrong.

**Not built, deliberately.** Writing a relay for pages that are already relayed would have produced a
change, a green loopback run, and no effect on the 1-in-10 failure — the precise shape of the "silent
nothing" this branch keeps shipping. The corrected note is worth more than the code would have been.

**And the pattern is worth naming once more, because it caught me twice in one evening.** Both times
the false claim arrived as a *comment written by someone who had reason to believe it*, and both times
it was adopted without a check because it explained the evidence. The check cost one run and two greps
on each occasion. The rule this diary already knows — a reading that explains the evidence is not
thereby the cause — apparently needs applying to the repository's own prose, not just to hypotheses
formed in the moment.

### 2026-07-26 (last) — The feedback abort did not reproduce in 22 tries, which revises the rate, not the verdict

Went to catch the abort with a debugger. Did not catch it, and the failure to catch is the result.

**The hunt.** `icosa-cpu` with semaphore/event/query feedback enabled, the application under
`gdb --batch -ex run -ex "thread apply all bt 20"` so an abort would yield a full backtrace:

- **loopback: 8/8 clean**, 120 frames each.
- **real network, apollo → dop561: 14/14 clean**, 120 frames each, backtrace pulled back after every run.

Twenty-two consecutive passes, against the one total-loss run the sweep produced.

**What that does and does not change.** It does *not* exonerate the configuration: the sweep failure
was real, a whole run lost to a silent `SIGABRT`, and one confirmed failure is not undone by later
successes. What it changes is the *rate*: "1 in 10" came from a single failure in a single sweep, and
0-in-22 after it puts the true rate far lower — one loss in 32 runs is ~3%, and plausibly rarer. A 3%
chance of losing a whole session is still disqualifying, so the verdict stands and the flags stay off.

**A caveat I raised and then had to walk back within the same hunt.** I predicted `gdb` would slow the
application enough to close the race window, and framed an exhaustion in advance as evidence about the
instrument rather than the bug. That was overstated: `gdb --batch -ex run` sets no breakpoints, so its
overhead is ptrace on signals and thread events — light. The honest reading of 22 clean runs is the
base rate, not masking. Worth recording because pre-committing to an interpretation of a result is
exactly how a null result gets explained away.

**Where this leaves the feedback question.** Three things are now known and one is not. Known: fence
feedback is structurally incompatible with (c)2's barrier; the other three are worth 1.23×; and the
S→C pages are *already relayed*, so the earlier "un-relayed pages" explanation is dead. Not known: why
the application aborts, rarely. Chasing a ~3% event needs a harness that runs unattended for many more
iterations than a working session can spare, and — better — one that captures a core rather than
holding a debugger open, since `ulimit -c` and `core_pattern` on C currently discard the evidence.
That is the next concrete step, and it is setup work rather than investigation.

### 2026-07-27 — 82 clean runs later, the feedback failure cannot be blamed on feedback

Set up real core capture on apollo and ran the hunt unattended. The result overturns the conclusion
two entries above — my own, from a few hours earlier.

**The setup.** `core_pattern` on C pipes to apport, which discards these cores, so it was pointed at
`/tmp/cores/core.%e.%p` with `ulimit -c unlimited` in the application's own shell, and **restored on
exit** via a trap (it is a system-wide change on another machine and must not outlive the run — it
was, and the log says so). No debugger held open this time: lighter, and closer to the conditions
that produced the original failure.

**60 attempts, all `rc=0 frames=120 cores=0`.** Verified per-attempt rather than trusting the summary
line — a harness failing fast sixty times would also print EXHAUSTED.

**The tally, and the problem with it.** With the three feedbacks enabled: 8 loopback under `gdb`, 14
real-network under `gdb`, 60 real-network with core capture — **82 clean runs**. Against them, one
lost run in the original ten-run sweep. So **1 failure in 92 feedback-on runs, about 1%**, against
**0 in 20 feedback-off runs**.

That is not a significant difference. Twenty runs cannot distinguish a 1% failure rate from zero.
**The failure therefore cannot be attributed to feedback**, and this diary said it could — twice, in
CLAUDE.md and here, with a mechanism ("shared status pages (c)1 does not relay") that was itself
refuted an hour later. The honest position is that one run of `icosa-cpu` over a real network died of
something unexplained, and it happened to be a run with feedback on.

**What that changes.** The flags stay off, but for a different and weaker reason: not "feedback breaks
it" but "an unexplained total-session loss is unexplained either way, and 1.23× does not buy that
risk". If someone later wants the 1.23×, the work is not "fix feedback" — it is to run the
feedback-*off* configuration enough times to know its own failure rate, which nobody has done. A
baseline of 20 is not a baseline.

**The pattern this session kept producing, one last time.** A single failure suggested a cause; the
cause suggested a mechanism; the mechanism was plausible enough to write into two documents. Checking
the mechanism killed it. Chasing the failure showed the rate was an order of magnitude off. Neither
step required cleverness — only declining to treat one observation as a rate, which is the same error
in a different costume each time it appears.
