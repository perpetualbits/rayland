# (c)1 — The Network (rescoped after C0)

**Date:** 2026-07-15
**Status:** Sub-project design spec (awaiting owner review)
**Parent design:** [`2026-07-13-native-remote-wayland-gpu.md`](2026-07-13-native-remote-wayland-gpu.md)
**Predecessor:** C0 — Venus First Light (substance complete)
**Required reading:** [`2026-07-15-venus-ring-findings.md`](2026-07-15-venus-ring-findings.md) — this spec is
built directly on it and does not repeat its evidence.

---

## 1. Purpose and the single success criterion

C0 proved that a real, unmodified Vulkan application's commands, captured by Mesa's Venus ICD,
replay correctly on a real GPU through a Rayland-owned host — with a PNG **bit-identical** to the
same app run natively. But C0 ran **on one machine, over shared memory**. It proved the command
stream is faithful. It proved *nothing whatsoever* about remoting.

(c)1 is where the network arrives, and therefore where the project's central claim is first
actually tested: **that an application's rendering can cross a network as *language* — a stream of
commands — rather than as pixels.**

**Success criterion (measurable):** the `rayland-refapp` — unmodified, unaware of Rayland — runs on
**C** (`appollo.localdomain`, x86_64). Its Vulkan commands cross a real network over QUIC to **S**
(`dop561`), where they are replayed on S's real Intel GPU and the resulting frame is **presented in
a window on S's display**. Correctness is asserted twice, by two independent paths:

1. the app's own readback PNG, written on C, and
2. the frame the host presents on S,

and the venus-rendered output is **bit-identical** to `rayland-refapp` run natively **on S**
(both are the same Intel GPU, so bit-identity is a legitimate assertion — see §10.2).

**Reliability is part of the criterion**, as it was in C0: repeated runs, no orphaned processes,
no wedged sessions.

**And a second, equal deliverable: a measurement table (§8).** Without it, (c)1 is a demo. With it,
(c)1 is evidence about whether remote Wayland is feasible — which is the actual question.

---

## 2. Why the original (c)1 scope is dead

The parent design and C0's spec both describe (c)1 as, in essence, *"swap the local vtest socket
for Rayland's QUIC transport, and wire the output into SP3's dmabuf window."* C0's spec §6 called
the vtest socket "the transport seam" and said "(c)1 only swaps the stream source."

**Every part of that is false, and C0 disproved it with live evidence:**

- **The vtest socket carries 0% of the application's commands.** It carries `vkCreateRingMESA` and
  `vkNotifyRingMESA` doorbells — ring *management*. Nothing else. There is no stream on the socket
  to swap.
- **The commands travel through a shared-memory ring**, whose fd is passed over `SCM_RIGHTS`.
  **Neither a shared page nor a file descriptor survives a network.**
- Therefore (c)1 is **a protocol design task**, not a transport substitution.

This is not a setback; it is the finding C0 existed to produce. But it means (c)1 must be designed,
not merely scheduled.

---

## 3. Scope — what (c)1 is, and is not

(c)1 **is**: a split of the C0 host into a **C-side relay daemon** and an **S-side engine host**,
joined by QUIC; a Rayland relay protocol carrying the ring, the replies, and mapped-memory
contents; on-screen presentation on S; and a measurement of what all of that costs.

(c)1 is deliberately **NOT**:

- **Not b2 — the app does not present.** The refapp stays headless/offscreen; **our host** puts the
  frame on screen. A real Wayland app opening its *own* window requires proxying the Wayland
  protocol (the SP5 axis) *and* Venus's WSI/swapchain path, which normally reaches a host
  compositor via virtio-gpu and is an open question outside a VM. That is **b2, the next slice**.
  See §11.1.
- **Not general `vkMapMemory` coherence.** (c)1 implements the narrow, conservative sync the refapp
  actually needs (§7). The general problem is **(c)2**.
- **Not zero-copy presentation — (c)1 does not inherit SP3's headline property.** The host cannot
  see the app's `DEVICE_LOCAL` render target, so it presents from the app's readback blob via
  `wl_shm`, with a GPU→CPU round trip on S. This is a deliberate b1 shortcut with a known expiry
  date; see §7.1. SP3's dmabuf path is not wasted, merely not reachable yet.
- **Not optimized.** (c)1 is deliberately slow in measurable ways (§6, §8). Buying the performance
  back is (c)2's job, informed by (c)1's numbers.
- **Not content-addressed assets** — **(c)3**.
- **Not RISC-V.** `milkv.localdomain` is the eventual dream C (see §11.2), but one unknown at a
  time: x86→x86 first.
- **Not a Mesa fork.** See §4.2.
- **No production security/sandboxing.** The engine still executes an untrusted stream; C0's
  posture is unchanged.

---

## 4. Architecture

### 4.1 Topology

- **C = `appollo.localdomain`** — x86_64, has an AMD GPU that is **entirely unused**. Runs the
  refapp and the `rayland-c` daemon. **Needs no GPU and no Wayland**: Venus is a *serializing*
  driver; it never touches local hardware. This is the thesis in physical form.
- **S = `dop561`** — Intel Iris Xe, the display the user is looking at, the Wayland compositor.
  Runs `rayland-s`.

### 4.2 The cut: why stock Mesa, and no fork

The application runs **stock Mesa** with `VN_DEBUG=vtest`, pointed at a **local Unix socket**.
`rayland-c` is the vtest server on the other end of that socket.

This is the key structural insight, and it inverts what looked like C0's worst finding. C0 found
that the ring is a **HOST3D blob — the *host* allocates it and the client maps host memory**, which
seemed strictly worse for a network than the alternative. But *we are the host*. From Venus's point
of view, `rayland-c` is an ordinary vtest host that merely happens to be on the same machine. It
allocates the ring and every blob as **plain local memfds** and passes the fds; the app maps them
and cannot tell the difference.

So: **stock Mesa, stock application, no fork, no patch.** The "no seam in Mesa" problem
(`bo_flush` is a nop in both backends; the ring is a `vn_renderer_shmem` with only create/destroy)
is sidestepped entirely, because we are not trying to hook Mesa — we are standing where Mesa
already expects a host to stand. vtest is a supported interface.

### 4.3 The `RenderEngine` trait pays off

`serve_vtest` drives a `RenderEngine` **trait**, not a concrete engine. On C, we implement a
`RelayEngine` that **forwards to S** instead of rendering. CLAUDE.md's locked decision — *"the trait
boundary must stay clean enough that the engine could later be Rustified or swapped without
touching the rest"* — pays off here, for a reason nobody anticipated when it was written: the thing
we swap in is not another renderer, it is **a network**.

### 4.4 Data flow

```
  C = appollo (no GPU, no Wayland)          |          S = dop561 (Intel GPU, display)
                                            |
  rayland-refapp (unmodified)               |
    │  Vulkan calls                         |
    ▼                                       |
  stock Mesa Venus ICD                      |
    │  writes commands into the ring        |
    │  + doorbells over the vtest socket    |
    ▼                                       |
  ┌──────────────────────────┐              |          ┌───────────────────────────┐
  │ rayland-c                │              |          │ rayland-s                 │
  │  • vtest server          │              |          │  • re-materializes ring   │
  │  • allocates ring/blobs  │   QUIC       |          │    + blobs into a real    │
  │    as LOCAL memfds       │◄────────────────────────►│    virglrenderer context │
  │  • watches ring `tail`   │  relay proto |          │  • replays on the GPU     │
  │  • RelayEngine           │              |          │  • presents (dmabuf)      │
  └──────────────────────────┘              |          └───────────────────────────┘
                                            |                      │
                                            |                      ▼
                                            |          Wayland compositor → screen
```

**Two rings exist**: one on C that the app writes, one on S that virglrenderer reads. (c)1 relays
the delta between them. Likewise every blob has a local shadow on C and its real counterpart on S.

### 4.5 Waking up: the ring `status` bit

C0's findings doc records the corrected polarity: **bit 0 of `status` set means the host's ring
thread is IDLE (parked); `status == 0` means it is actively polling.** Mesa sends
`vkNotifyRingMESA` **only when the IDLE bit is set** *and* ≥1ms has passed since the last kick
(`vn_ring.c:475-483`).

This is a real seam, and it means `rayland-c` **is not forced to spin**: it sets IDLE before
sleeping and Mesa will kick it over the socket; it clears IDLE while actively draining and Mesa
skips the kick.

**Pitfall — this must be got right or (c)1 will hang intermittently:** because of the ≥1ms throttle,
a kick is **not guaranteed** for every write. `rayland-c` must therefore use the standard
double-check-before-park discipline: set IDLE, **re-read `tail`**, and only sleep if it is still
unchanged. A naive "set IDLE then sleep" will miss work and stall. This is exactly the class of bug
the inverted-polarity documentation would have caused, and it is called out here because the
polarity error was live in this repository until 2026-07-15.

---

## 5. The channel inventory — everything that must cross

C0 proved the ring is not the only shared page. This is the complete list of what a network must
carry, and (c)1's strategy for each:

| # | Channel | Direction | What it is | (c)1 v1 strategy |
|---|---|---|---|---|
| 1 | **Command ring** | C→S | The 128 KiB buffer; the real payload | Relay `tail` deltas |
| 2 | **Reply arena** | S→C | Where the host writes command replies; the app **blocks** on these | Relay; each is a round-trip |
| 3 | **Feedback slots** | S→C | Fence/semaphore/event/query status, written by the host so the client can poll without a round-trip (`vn_feedback.h`) | **Disable** (§6) |
| 4 | **Mapped blobs** | C→S and S→C | The app's `vkMapMemory` memory: vertices out, pixels back | Conservative full sync (§7) |
| 5 | **Out-of-line streams** | C→S | Submissions >8192 B are replaced in-ring by `vkExecuteCommandStreamsMESA` (opcode 180) pointing at *other* shmems | **Detect and fail loudly** (§5.1) |

### 5.1 The out-of-line path: not implemented, but never silent

C0 Task 4b established that the refapp never triggers this: `vn_ring_submission_can_direct` fires
only when a **single submission** exceeds `direct_size` = `131072 >> 4` = **8192 bytes**, and the
refapp's largest input is a 1008-byte SPIR-V. So (c)1 v1 may legitimately not implement it.

**But it must notice.** If opcode 180 ever appears, (c)1 must return a typed error naming exactly
what happened — never decode past it, never guess. Silently mishandling it would corrupt the stream
in a way that presents as inexplicable GPU misbehaviour hours away from the cause.

**This is the single biggest known scaling limit of (c)1**, and it must be documented as such: *"the
ring is the whole stream" is true for the refapp and will break on the first real application.* The
lever, when the time comes, is `direct_order` — a client-side constant (`vn_instance.c:152`) where
`0` makes `direct_size == buffer_size`.

---

## 6. Venus configuration — crutches, declared as crutches

(c)1 v1 turns Venus's shared-memory optimizations **off** so the stream becomes self-contained. This
makes it slower, on purpose, in exchange for being correct and measurable.

**Every setting below is a crutch with an exit condition. None of them is "how Rayland works."**
The failure mode this table exists to prevent is these quietly becoming permanent.

| Setting | Why | Exit condition |
|---|---|---|
| `VN_DEBUG=vtest` | Required, or Mesa silently prefers virtgpu and never connects | **Permanent** — not a crutch |
| `VN_PERF=no_multi_ring` | Forces a single ring, making the `ring_idx = 0` assumption in the fence path **legitimate** rather than lucky (it has been latent since C0 Task 3) | Remove when the relay handles multiple rings |
| `VN_PERF=no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback` | Removes the S→C shared status pages (channel 3), making the stream self-contained | **First thing to buy back** — see §6.1 |
| `VN_DEBUG=no_abort` | Stops Mesa's 3.5s watchdog killing a legitimately slow networked run (`vn_common.c:278` gates the abort on this flag) | Remove once (c)1 has its own progress-aware timeout — see §6.2 |

### 6.1 Why disabling feedback is temporary, not fundamental

The feedback slots exist so the client can check "is this fence signalled?" by reading a shared
page instead of asking the host. **Shared memory is a *pull* mechanism; a network is a *push*
one.** We cannot ship a page on every read — but we do not need to. The slots are small and few, and
S knows when it writes them. So the optimization is restored by having **S push the slot contents
on change**, and the round-trip disappears. That is (c)2 work; the point here is that the slowness
is a property of v1, not of the architecture.

### 6.2 `no_abort` is the dangerous one

C0 found that Mesa's watchdog **reports liveness it never checks**:
`vkr_context_ring_monitor_thread` sets ALIVE unconditionally, from a thread that never consults
ring state (`vkr_context.c:536-539`). It proves the host process is scheduled — not that the ring
progresses. A transport that forwards it faithfully while the ring is stalled converts a 3.5-second
abort into an **895-second hang**.

Setting `no_abort` means a genuine deadlock presents as a **silent hang** instead of a fast abort.
So the ordering is mandatory: **(c)1 implements its own progress-aware timeout — one that actually
consults ring `head`/`tail` movement — *before* Mesa's is switched off.** Otherwise we trade a fast
wrong answer for a slow one, which is worse.

---

## 7. Coherence: the narrow slice (c)1 must solve

There is no version of (c)1 that avoids `vkMapMemory`. Even the most minimal path forces it: C0
Task 4b caught the refapp's vertex buffer (`res=3`, 64 bytes) decoding float-for-float out of a
mapped blob, and its readback buffer (`res=6`, 16384 B = 64×64×4) holding the blue clear colour.
The app writes vertices into mapped memory **with no API call to intercept**, because it believes
that memory is coherent.

**(c)1 v1 strategy: conservative full sync, triggered by ring relay.** Ship the full contents of
every mapped blob in the direction it is needed — C→S before the GPU reads, S→C after it writes. No
dirty tracking, no cleverness. For a 64-byte vertex buffer and a 16 KiB readback this is trivially
cheap; for a real application it would not be, which is precisely why the measurement (§8) matters.

**On the trigger — an ambiguity worth killing now.** "At every submission boundary" is a phrase that
sounds precise and is not: **`vkQueueSubmit` is invisible to us.** It is encoded *inside the ring*,
and v1 relays the ring as opaque bytes without parsing them. The only boundary v1 can actually
observe is **its own relay event** — i.e. "we are about to ship ring bytes to S". So:

- **C→S:** before shipping a ring delta, ship every mapped blob whose shadow is dirty.
- **S→C:** ~~after S reports the replayed batch retired, ship back every mapped blob the GPU may have
  written.~~ **RETRACTED 2026-07-15 — see §7.2. The S→C half was both incomplete (it never carried
  the reply arena at all) and unsound (it is a last-writer-wins race). The corrected rule: S ships
  back exactly the pages S actually wrote.**

This is deliberately over-eager: it syncs blobs that may not have changed, and it syncs on relays
that contain no submit at all. That is the intended cost. **The precision upgrade is already in
hand** — `venus_ring/decode.rs` can decode the ring, so a later version can find the actual submits
and sync exactly what they touch. v1 does not do this, because decoding the ring to make a
correctness decision means a decoding bug becomes a corruption bug, and we would rather pay bytes
than debug that.

**Why this is honest rather than lazy:** it is *correct* for any app, and its cost is *visible*.
The alternative — guessing at an optimization before we have numbers — is how projects acquire
subtle corruption bugs.

### 7.2 The S→C rule, corrected: ship what S wrote, not what S owns

**Added 2026-07-15, after Task 5.** The original S→C rule above was wrong twice over, and both
faults were mine rather than an implementer's.

**It never carried the reply arena.** §5's channel 2 lists the arena as S→C traffic, but no task
owned it, so nothing shipped it. The symptom is not the hang one might expect: `head` *does* advance
from `S2C::RingProgress`, so `vn_ring_wait_seqno` returns and the application is **released onto an
arena that is still zeros**. `vn_instance_init_renderer_versions` reads `instance_version = 0`,
fails the `VN_MIN_RENDERER_VERSION` check, and `vkCreateInstance` fails. **Silent garbage, not a
stall** — worth knowing before debugging it.

**And "every blob the GPU *may* have written" is a last-writer-wins race.** S ships back app blobs
its GPU never touched — vertex and uniform buffers, the common case. Concretely: the app memcpys
frame N+1's vertices into `res=3`; S's poll fires on head movement from frame N and ships S's
**stale** `res=3`; C's reader overwrites the app's fresh vertices; C then relays the stale bytes
back. Invisible in the refapp, which writes its vertices exactly once — which is precisely why every
test passed.

**A rejected fix, recorded because it is the attractive one.** Identify the arena by decoding
`vkSetReplyCommandStreamMESA` (opcode 178) out of the ring. It is **silently unsound**: 178 is
emitted before *every* reply-bearing command (`vn_ring_submit_command` → `vn_ring_set_reply_shmem_locked`,
`vn_ring.c:711-715`), so all but the first sit behind the decoder's stop point at the unsizeable
`vkCreateInstance`; and when the 1 MiB reply pool fills, `vn_renderer_shmem_pool_grow_locked`
(`vn_renderer_util.c:70-96`) mints a **new `res_id`**. C0 measured 48820 bytes of reply traffic, so
the refapp never grows the pool — this would have passed every test we have and corrupted the first
longer session, with S shipping a dead arena while the app read a live one. **Not a decoding bug: a
correct decode of an incomplete picture, which is worse, because there is no bug to find.**

**THE RULE: S ships back exactly the bytes S wrote.** Stop asking *"whose memory is this?"* and ask
*"did I write it?"* — on one machine every byte S writes is instantly visible to C, so ownership
predicates are a *guess* at that relationship while observed writes **are** it. Mechanically: S
snapshots each blob after applying an inbound `C2S::BlobData` (so C's own writes never count as S's),
diffs **byte-granular** at retirement, and ships the changed runs via `BlobData`'s existing `offset`
field. Rings are excluded by `res_id` — S already holds them, and `RingDelta`/`RingProgress` own
those bytes. That exclusion is **structural, not heuristic**.

> **Amended 2026-07-15, during Task 5b: this said *page*-granular, and that was an unexamined habit
> of mine rather than a decision.** Dirty-*page* tracking is the usual idiom because page tables are
> the usual mechanism — but S is not using page tables, it is using `memcmp`, **and a `memcmp` is
> byte-granular for free.**
>
> Page granularity leaves a live hole. If S writes one region of a 4096-byte page while the
> application writes another region of *the same page* — entirely legal, and requiring no Vulkan
> synchronization between them — then S's run carries its **stale** copy of the app's bytes and
> clobbers the app's fresh ones. `VkDeviceMemory` is page-aligned and applications suballocate, so
> this is realistic rather than theoretical. **It is the same shape as the whole-blob race this very
> section exists to remove: invisible in the refapp, live for the first real application.**
>
> The compare cost is identical and the shipped volume is smaller, so byte granularity is strictly
> better and the earlier wording bought nothing. Task 5b implemented the page-granular rule as
> specified and flagged the hole rather than deviating from a binding spec unasked — the right call,
> which is why this is an amendment and not a bug report.

This is the same epistemological move as §5.1's out-of-line dword scan: **a predicate over bytes,
not a reading of them.** §7's "no decoding the ring to make a correctness decision" survives intact.

What falls out, with no knowledge of what any blob *is*: the arena ships (blocker gone); the staging
pool never does (no wiped recording); app buffers never do (race gone); the readback ships, because
the GPU genuinely wrote it. It is immune to the reply pool growing, to the shmem cache recycling ids,
and to Venus adding a fourth internal shmem tomorrow. The cost — roughly 8 MiB of **byte** compares
per retirement, worst case (the same volume the retracted page-granular rule would have compared,
now compared one byte at a time rather than one page at a time — see the amendment below) — is
exactly the kind of honest, measurable slowness §6 and §8 ask v1 for.

### 7.1 Where the presented pixels come from — and why it is not zero-copy

§1 promises a frame on S's screen, so this must be settled rather than left open.

**The host cannot see the app's render target.** C0 Task 4b established that the refapp's
`DEVICE_LOCAL` `VkImage` produces **no blob at all** — it is created by Venus commands *inside the
ring* and never appears in our engine's resource table. There is nothing there for the host to
present.

**But the host can see the readback blob.** The refapp does `vkCmdCopyImageToBuffer` into
`HOST_VISIBLE` memory, and 4b caught exactly that buffer: `res=6`, 16384 B = 64×64×4, holding the
blue clear colour. Its *real* memory lives on S, written by S's GPU. So `rayland-s` presents
**from the readback blob** — the same bytes the app will later read on C.

**Two consequences, stated plainly rather than buried:**

1. **This is not zero-copy, and (c)1 therefore does not inherit SP3's headline property.** The
   pixels take a GPU→CPU round trip on S and reach the compositor through `rayland-present`'s
   **`wl_shm` path**, not its dmabuf path. SP3's dmabuf work is not wasted — it is what a real
   presentation path will use — but (c)1 cannot use it, because dmabuf-exporting a resource
   requires seeing the resource.
2. **It borrows the app's readback, which b2 will not have.** A swapchain image has no
   `vkCmdCopyImageToBuffer` and no host-visible buffer to borrow. So this is a legitimate b1
   shortcut with a **known expiry date**: b2 forces the zero-copy question (§12.6), and that is the
   right time to spend C0 Task 4c's deferred spike.

**The removal paths for the coherence strategy (both (c)2's business, recorded so v1 does not
foreclose them):**

1. **Non-coherent memory + real flush hooks** (Approach B). If Venus advertised memory *without*
   `HOST_COHERENT`, the app would be **obliged by the Vulkan spec** to call
   `vkFlushMappedMemoryRanges` — which lands on `bo_flush` and tells us exactly what changed. This
   is unproven ground: both existing backends nop their flush because both assume coherence. It
   likely requires the Mesa work (c)1 avoids, and it risks apps that demand coherent memory.
2. **Soft-dirty page tracking** via `/proc/<pid>/pagemap` + `clear_refs`. Works **cross-process**,
   needs no Mesa change and no app change — a genuine middle path that keeps the stock-Mesa
   property.

---

## 8. Measurement — the deliverable that makes this a verdict

**This section is a first-class requirement, not instrumentation added at the end.** The owner's
question is *"is remote Wayland feasible?"* A demo cannot answer it; a table can.

(c)1 must produce, in `docs/c1-the-network.md`:

- **Round-trip count** — at startup, and per frame. This is the number that decides WAN viability.
- **Bytes each way, split by channel** (ring / replies / blob sync). This tells us what to compress
  and what to cache, and it is cheap to measure once the split exists.
- **Frame latency** — native on S, vs local socket, vs LAN.
- **Breaking point under simulated WAN** — inject 20/50/100 ms RTT with `tc netem` on appollo and
  find where it becomes unusable. This is the cheapest possible way to answer "would this work over
  the internet" without an internet.

### 8.1 What we already expect, and why the architecture is not intrinsically slow

Recorded here so (c)1's results can be compared against a **prediction**, which is the difference
between measuring and rationalizing:

- **Steady state should be bandwidth-bound, not RTT-bound.** Venus is asynchronous by design — the
  ring exists precisely so `vkCmdDraw` does not wait. This is the opposite of X11's
  round-trip-per-operation, and it is the strongest structural reason to believe this can work.
- **Bandwidth should be small.** C0 measured **~4 KiB of ring traffic for a complete Vulkan
  initialization**. For scale, 1080p60 video is 5–20 Mbit/s. Language is much cheaper than pixels —
  the project's founding intuition, now with a number attached.
- **Startup is RTT-bound but one-off.** `vkEnumeratePhysicalDevices` and friends are synchronous by
  API contract: dozens × RTT. ~10 ms on a LAN; ~1 s on a bad WAN. Survivable.
- **The genuine limit is per-frame synchronization** — fence waits now, swapchain acquire/present
  in b2. One round-trip per frame against a 16.7 ms budget at 60fps. A LAN RTT (~0.2 ms) is
  nothing; a 20 ms WAN RTT costs a frame. **This limit binds every remoting scheme, video streaming
  included. It is physics, not architecture.**
- **The known asymmetry:** C0 measured the reply arena at 48820 bytes against 4024 bytes of ring
  traffic — the **return** path was ~12× the command path. If that ratio holds, the return path is
  where the bytes are.

**A prediction that fails is a finding.** If steady state turns out RTT-bound, that is far more
important than any demo, and it must be reported loudly rather than tuned around.

---

## 9. Crate structure

Working the design through surfaced a constraint that is easy to miss and fatal to the thesis:
**`rayland-engine` FFI-links `libvirglrenderer`, and the C side must never need it.** C is meant to
be the *weak* machine — eventually a RISC-V box with no GPU stack at all. If the C daemon links
virglrenderer, the claim "C needs no GPU" quietly becomes false.

| Crate | Change | Licence |
|---|---|---|
| `rayland-vtest` | **New — split out of `rayland-engine`**: the vtest protocol, `venus_ring/`, and the `RenderEngine` trait. Pure Rust, **no FFI**. | LGPL |
| `rayland-engine` | Keeps the virglrenderer FFI impl only. **S-side only.** | LGPL |
| `rayland-present` | **New — extracted from `rayland-server`'s `window.rs`**: dmabuf + `wl_shm` presentation. Both the SP-era server and `rayland-s` need it; duplicating it would rot. | LGPL |
| `rayland-relay` | **New**: the (c)1 protocol — ring deltas, blob syncs, replies — with postcard framing following `rayland-wire`'s pattern. Shared by both sides. | LGPL |
| `rayland-c` | **New binary**: the C-side daemon. Depends on `rayland-vtest` + `rayland-relay` + `rayland-transport`. **Zero GPU dependencies.** | GPL |
| `rayland-s` | **New binary**: the S-side host. Depends on `rayland-engine` + `rayland-relay` + `rayland-transport` + `rayland-present`. | GPL |
| `rayland-transport` | Reused unchanged (SP2's QUIC). | LGPL |
| `rayland-refapp` | Reused **unmodified** — that is the point. | GPL |

This takes the workspace to 10 crates. That is flagged deliberately: **each split is forced by
either the licensing policy (library → LGPL, binary → GPL) or the C-has-no-GPU constraint — not by
neatness.** The lighter alternative to splitting `rayland-vtest` out is a Cargo feature flag on
`rayland-engine`, but a real RISC-V C box cannot easily build virglrenderer at all, so the split
earns itself.

**Deliberately untouched:** `rayland-wire` and `rayland-client` are SP0-era and superseded by
Venus, but pruning them is separate work that does not serve (c)1.

---

## 10. Testing

### 10.1 What runs where

- **The two-machine run cannot live in CI.** It is a scripted manual verification with documented,
  *verified* commands (C0 shipped a documented command that did not work; a reviewer caught it —
  do not repeat that).
- **CI gets the loopback path**: `rayland-c` and `rayland-s` on one box, QUIC over `127.0.0.1`. A
  real network stack with zero latency — catches protocol bugs, not timing ones. Honest about
  which.
- **No GPU, no network needed** for: ring-delta computation, blob dirty-detection, relay framing.
  These are ordinary unit tests and must exist.
- Every GPU/network test **skips cleanly** when its dependency is absent, as C0's do.

### 10.2 The correctness assertion

Keep `rayland-refapp` **exactly as it is**. Unmodified is the entire point, and it already performs
its own `vkMapMemory` readback — which forces both directions of blob sync (§7) and yields **two
independent verifications**: the app's own PNG on C, and the frame the host presents on S. Different
paths, both must be right.

**Bit-identity is a legitimate assertion here, but only against the right baseline.** venus-from-C
renders on S's **Intel** GPU, so it must be compared against `rayland-refapp` run natively **on S**
(also Intel) — C0 established that same-GPU/same-stack replay is bit-identical (0/16384 bytes).
Comparing against appollo-native would be an **AMD** render and is meaningless. Assert bit-identity
against the S-native baseline; **report** rather than assert any divergence, so a legitimate change
surfaces to a human instead of turning CI red.

---

## 11. Deferred, and why

### 11.1 b2 — the app presents its own window

The owner's sequencing is **b1, then b2 if proven**. b2 — a real Wayland app creating its own
`wl_surface` and swapchain — needs two further things: the Wayland protocol proxy (the SP5 axis),
and Venus's WSI/swapchain path working **outside a VM**, where presentation normally reaches a host
compositor through virtio-gpu. That second one is a genuine unknown and deserves its own spike.

The Wayland-proxy half is **not** the novel risk: waypipe and Sommelier already demonstrate it. The
novel risk is the GPU command stream, which is what b1 targets. b1 therefore furthers the
feasibility argument most per unit of risk.

### 11.2 milkv — the RISC-V C

`milkv.localdomain` (4-core rv64imafdc / sv39 / SiFive u74-mc, 8 GB) is the *dream* demonstration:
a weak RISC-V box running the application while an Intel GPU on another machine does the drawing —
exactly the "C may be weak, or a different CPU architecture" case the parent design names.

**Its Xorg-vs-Wayland situation is irrelevant to the C role**, and this is worth stating plainly
because it looked like a blocker: **C needs no compositor and no GPU.** Venus never touches local
hardware, the refapp is headless, and even a future Wayland app on C gets its socket from our proxy
rather than a local compositor. The only real question for milkv is whether **Mesa 26 with the
Venus ICD builds for riscv64** there — a build problem, not a distro problem.

Deferred purely to keep one unknown at a time: x86→x86 must work first.

---

## 12. Open questions (recorded, not smoothed over)

1. **Does `VN_PERF=no_multi_ring` actually force a single ring?** Inferred from the name; **not
   verified**. If it does not, the `ring_idx = 0` assumption inherited from C0 Task 3 remains
   latent and (c)1 must handle multiple rings.
2. **The ≥1ms kick throttle** (§4.5) — the double-check-before-park discipline is the standard
   answer, but the exact interaction with Mesa's throttle should be verified empirically, not
   assumed.
3. **Does disabling all four feedback types actually work?** Each `VN_PERF` switch exists, but the
   combination is untested, and Venus may have paths that assume feedback is present.
4. **What is the 1 MiB blob? Resolved, by source, during (c)1 Task 5's review.** C0 identified it
   by *behaviour* as the reply arena (`vkSetReplyCommandStreamMESA` names `resourceId=2`), but left
   open which of Venus's independent 1 MiB allocations that is. Tracing the emitter settles it:
   `vn_instance.c:328-332` shows `instance->cs_shmem_pool` is sized `8u << 20` (8 MiB) and
   `instance->reply_shmem_pool` is sized `1u << 20` (1 MiB) — the only 1 MiB pool the instance owns.
   The only place a `vkSetReplyCommandStreamMESA` is ever emitted is
   `vn_ring_set_reply_shmem_locked` (`vn_ring.c:672-687`), and its `shmem` argument always comes from
   `vn_ring_submit_command`'s call to `vn_instance_reply_shmem_alloc` (`vn_ring.c:701`), which is a
   thin wrapper (`vn_instance.h:92-98`) over `vn_renderer_shmem_pool_alloc(..., &instance->
   reply_shmem_pool, ...)`. So C0's captured `resourceId=2` **is** `instance->reply_shmem_pool`, by
   construction of the code rather than by having observed it behave one way. The third 1 MiB
   candidate, `ring->upload` (`vn_ring.c:322`), is excluded on the same evidence C0 already had: the
   refapp emits zero opcode-180s (ring-findings §5.3), and `ring->upload` backs exactly that
   out-of-line path (`vn_ring.c:605`), so it is never allocated in the captured session. This entry
   is kept, rather than deleted now that it is answered, because how it was resolved — reasoning from
   the source rather than from another capture — is itself worth having on record.
5. **Does the fence path's 5s `FENCE_WAIT_TIMEOUT` survive a real networked workload?** It has
   never been exercised — C0's path never reached it.
6. **Residual on presentation (the decision itself is made — see §7.1):** the host presents from
   the app's readback blob. What remains open is whether a **zero-copy** path to the app's actual
   `DEVICE_LOCAL` render target is reachable at all — the question C0 Task 4c was to spike and the
   owner deferred. Not needed for b1; needed before presentation is efficient, and near-certainly
   needed for b2, where a swapchain image has no readback buffer to borrow.

---

## 13. What (c)1 will and will not prove

**Will prove:** that a Venus command stream can cross a real network and still produce a correct
frame on a remote GPU; what that costs in round-trips, bytes and latency; and where it breaks as
RTT grows.

**Will not prove:** that arbitrary applications work (the out-of-line path at 8192 bytes, §5.1, and
general coherence, §7, both remain open); that a real Wayland application can be remoted (b2, §11.1);
or that the performance is acceptable — only what the performance *is*.

**The honest framing:** C0 proved the stream is *language*. (c)1 proves whether that language
*travels*. Neither settles whether Wayland is remotable — but together they replace opinion with
measurement, which is the only way that question was ever going to be answered.
