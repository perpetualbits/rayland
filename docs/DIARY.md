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
