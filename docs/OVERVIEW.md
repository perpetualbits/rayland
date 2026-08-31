# Rayland — project overview

**Purpose of this document.** A single, self-contained orientation to the whole project, written
for a reader who is doing **planning and design** for Rayland without necessarily having the
repository open. It is the companion to a source snapshot (`snapshot.sh`, see §10): the snapshot
carries the code, this document carries the meaning.

It is deliberately complete rather than short. Rayland's hard parts are hard in ways that are not
visible from the code — several of the project's central facts were discovered only by measurement,
and at least three plausible-sounding designs were built and then disproved. A plan made without
those facts will re-propose a dead end. They are all recorded here, with pointers to the evidence.

**Last brought current:** 2026-08-31, against branch `main`.

---

## 1. The working arrangement

From 2026-08-29 onward the project runs in two roles:

- **Claude.ai — planning and design.** Discussion, architecture, specification, and the writing of
  prompts. Reads this document and the snapshot; produces prompts and design decisions.
- **Claude Code on the laptop — execution and reporting.** Receives those prompts, does the work in
  the repository against real hardware, and reports back.

This document and the snapshot are the channel from the repository to the planning side. The report
back is the channel in the other direction.

Two consequences worth stating, because they shape what a plan may assume:

1. **The laptop is the primary copy.** GitHub is backup/publishing. A plan should never assume work
   exists anywhere but the laptop unless it was pushed.
2. **Anything end-to-end needs two physical machines** (§9). A plan whose verification step requires
   the GPU or the display cannot be checked on a travel machine, and should say so explicitly so
   the work can be sequenced around it.

---

## 2. What Rayland is

**Native remote GPU rendering for Wayland.** An application runs on one machine but is rendered and
displayed on another — the one with the capable GPU and the monitor the user is actually looking at
— by sending a **command stream** across the network rather than a **pixel stream**.

It is the modern heir to X11's network-transparent graphics, rebuilt for Vulkan. The name nods to
Sun Ray (thin client, compute elsewhere, display here) and rhymes with Wayland.

A video-encode fallback for already-rendered pixels is architecturally reserved but is explicitly
**not the goal**: in the target setup the weak machine is exactly the wrong place to run an
expensive encoder.

### 2.1 The S / C vocabulary — do not get this backwards

Rayland uses X11-era terms, which are the **reverse** of cloud usage. Every document, comment, and
identifier in the project depends on this:

| Term | Meaning | Has |
|---|---|---|
| **S** — the "server" side | Where the **user sits** | Keyboard, mouse, **display, GPU**, the Wayland compositor, working drivers. The strong machine. |
| **C** — the "client" side | Where the **application executable runs** | Possibly weak, possibly a different CPU architecture (RISC-V), possibly headless. No good display path. |

The app on **C** emits rendering commands; **S's GPU** does the drawing and shows the result on
**S's** display.

A hard structural rule follows from this and is enforced by tests: **C must never link a GPU
stack.** `crates/rayland-c/tests/no_gpu_linkage.rs` asserts `rayland-engine` is absent from C's
dependency tree. The dependency arrow points `rayland-engine → rayland-vtest` and **must never be
reversed**.

### 2.2 Why this is hard

Wayland deliberately assumes the application and the compositor share memory and a GPU: the app
renders into a GPU buffer and passes a **file-descriptor handle** over a local socket. You cannot
send a file descriptor across a network. Remoteness is therefore not a missing feature — it is an
**excluded assumption**.

### 2.3 Why it is not hopeless

The hardest component — serializing a Vulkan command stream and replaying it on a remote GPU —
already exists and is hardened in the virtual-machine world (Venus, virglrenderer, gfxstream; the
whole stack ships in ChromeOS Crostini), and it is hardened against **exactly Rayland's threat
model**: an untrusted party driving the host GPU. Rayland's job is largely to swap that stack's
transport from "shared memory inside one computer" to "a real network", plus the genuinely new
pieces a network needs.

**Locked decision: Rust for all code Rayland writes.** The Vulkan serialization/replay engine is
*reused* via FFI behind a clean Rust trait boundary rather than reinvented. "All Rust" means our
code is 100% Rust; the borrowed engine is an external linked dependency. The trait boundary must
stay clean enough that the engine could later be Rustified or swapped.

---

## 3. How it actually works

This section is the part most worth reading before designing anything, because the mechanism is
counter-intuitive and was itself a discovery.

### 3.1 The founding discovery: the socket carries nothing

The first working prototype (arc (s), SP0–SP3) used Rayland's own hand-rolled `postcard` protocol
and worked end to end. The pivot to the real engine (arc (c)) then produced the finding that
reshaped the project:

> **The vtest socket carries 0% of the application's Vulkan commands.**

Mesa's Venus ICD does not send commands over its socket. It writes them into a **shared-memory
ring** whose file descriptor was passed over `SCM_RIGHTS`; the socket carries only a **doorbell**
(`SUBMIT_CMD2`) saying "the ring has advanced". Neither a shared page nor a file descriptor
survives a network.

So "(c)1 — the network" was never "swap the socket for QUIC". It was a protocol design problem.
Evidence: `docs/design/2026-07-15-venus-ring-findings.md`.

### 3.2 The insight that makes it work without patching Mesa

The vtest protocol's "host" is **whoever allocates the ring**. Rayland can be that party.

`rayland-c` is a **local vtest server** that a stock, unmodified Mesa Venus ICD connects to. It
hands the application plain local memfds for its ring and its blobs. The application then writes
its commands into memory that *Rayland owns*. No Mesa fork, no patch, no `LD_PRELOAD` of the
driver — the app and its driver are entirely unmodified.

### 3.3 The forward path, concretely

```
  C (apollo)                                  network                S (dop561)
  ┌────────────────────────────┐                                     ┌────────────────────────────┐
  │ unmodified Vulkan app      │                                     │ rayland-s                  │
  │   ↓ (stock Mesa Venus ICD) │                                     │   ↓ writes delta INTO its   │
  │ writes cmds into ring memfd│                                     │   own mirror of the ring's  │
  │   ↓ doorbell over socket   │      C2S::RingDelta (postcard)      │   memory                    │
  │ rayland-c watches ring tail│ ──────────── QUIC ────────────────► │   ↓                        │
  │   relays the raw BYTES     │      C2S::BlobData (all blobs       │ libvirglrenderer's own ring │
  │                            │              except the ring)       │ thread polls that memory    │
  │                            │                                     │   ↓                        │
  │                            │ ◄─────────── QUIC ───────────────── │ real GPU draws              │
  │ app's readback blob filled │      S2C::BlobData (readback,       │   ↓                        │
  │ app's fence poll answered  │      reply arena), progress         │ rayland-present → window    │
  └────────────────────────────┘                                     └────────────────────────────┘
```

The crucial subtlety, and the thing most easily got wrong when reasoning about `rayland-s`:

> **`rayland-s` does not "receive commands and execute them."** A relayed ring delta is **written
> into the ring blob's memory**, because that is where virglrenderer's own ring thread polls for it
> (`vkr_ring.c:33-58` points the ring at the blob's pages; `vkr_ring.c:262-266` loops on them).
> `RenderEngine::submit` is used only for the *inline* vtest path, which carries the
> `vkCreateRingMESA` that creates the ring and essentially nothing else.

**Blob synchronisation.** Since 2026-07-26, C synchronises **every blob except the ring**. This was
forced by Venus's **out-of-line command streams**: any submission larger than `direct_size`
(`buffer_size >> 4`, so 8 KiB for the 128 KiB ring) is replaced by `vkExecuteCommandStreamsMESA`
naming *other* shmems. Skipping those made every submission over 8 KiB unrelayable — i.e. most real
workloads. The ring keeps its own `RingDelta` message (which carries the `tail` that validates its
bytes, and is sent last). Publishing a region S also writes is safe because of the **baseline**,
not the blob id: `note_s_wrote` folds each S→C write into the baseline, so S's replies are never
echoed back.

**A standing design constraint on this path** ((c)1 spec §7): the ring is relayed as **opaque
bytes** precisely so that a decoding bug cannot become a corruption bug. `rayland-venus-proto` may
answer "how long is this command?" for framing, but it **may never decide what gets relayed, when,
or which blobs a delta reads**. Enforced by `crates/rayland-c/tests/decoder_is_not_load_bearing.rs`.
One deliberate exception exists, **off the relay path**, and is documented at four sites: on
2026-07-26 `find_destroy_device` was allowed to validate a signature against a decoded command
boundary, because the bare byte scan was false-positiving on payload bytes. The guard test is
deliberately *not* widened to cover `rayland-s`, so the discrepancy stays visible rather than
papered over.

### 3.4 The return path, and why it was the hardest problem

An application that maps a **GPU-written** buffer and reads it back cannot be served by S passively
observing and diffing that memory. S is a foreign reader with no fence→coherency relationship with
the GPU. Every patch of the observe-and-diff approach hit a different wall. Two candidate fixes were
investigated and **both retired by measurement**:

- The fenced engine-side read (`virgl_renderer_transfer_read_iov`) is a **hardcoded stub** for the
  Venus/render-server path in virglrenderer 1.2.0 **and** 1.3.0 — there is no engine-level
  coherence API at all.
- `DMA_BUF_IOCTL_SYNC` on the readback dma-buf is a **measured no-op**: byte-identical to the raw
  read in 6561/6561 samples. The memory is already CPU-coherent, so **the tearing was never a
  cache-coherence problem.**

The conclusion was that correctness needs the host GPU work retired through an **engine call** — and
that call takes Rayland's single global engine lock, which is exactly what contends with the
message-thread doorbell (ring-stall `SIGABRT`). That architectural deadlock is **solved by the
engine actor** (`crates/rayland-engine/src/actor.rs`): one thread owns virglrenderer, and an
`EngineClient` implements `RenderEngine` by messaging it, so the fence and the doorbell cooperate on
one thread instead of deadlocking.

**The barrier that finally worked ("G'", 2026-07-21), after three recorded dead ends:**

With fence feedback off, the application releases itself by polling `vkGetFenceStatus` until the
reply reads `VK_SUCCESS`. virglrenderer writes that reply into the reply arena as `[38][0]`, which
means the app's submit **and its readback copy** are complete on S's GPU.
`Applier::reply_arena_fence_signaled` scans the **live** arena for that pattern (the shipped diff
fragments the reply into per-changed-byte runs, so the contiguous pattern is invisible there). It is
safe against a lingering prior success because the app polls `VK_NOT_READY` (`[38][1]`) *during* a
copy's DMA, so a live `[38][0]` means a fence just signalled. `progress_thread` then ships the
readback **before** the reply arena and the head-advance that release the app. No S-issued fence, no
timing heuristic; the progress thread no longer touches the engine.

The three dead ends, all left in the record deliberately:
1. An empty-submit context fence **retires before the readback DMA** (`T2 < T4`, pervasive).
2. A "wait-drain" design rested on a false premise — with feedback off Mesa does **not** send
   `vkWaitForFences`, it polls `vkGetFenceStatus`. Caught by a spike before it was built.
3. A fingerprint-gated res6-first ordering ("G-lite") killed the staleness but **tore**, having no
   completion barrier.

**Result: 0 stale frames across 20 real-network runs.**

### 3.5 Presentation

`rayland-present` shows finished pixels in a real `xdg_toplevel` window, via `wl_shm` or zero-copy
`zwp_linux_dmabuf_v1`. The (c)1/(c)2 path uses **only `wl_shm` and is deliberately not zero-copy**:
S presents the application's **readback blob**, because it cannot see the app's `DEVICE_LOCAL`
render target (that produces no blob at all). Ending that is precisely what WP0 exists for.

`present_live()` follows a live render by re-arming a `wl_surface.frame` callback on every commit.
**Pacing is the compositor's, not the relay's** — the window shows whichever frame S last completed,
so a slower remote render repeats frames. That keeps presentation from ever blocking the relay, at
the cost of not being frame-accurate.

---

## 4. Repository layout

A Cargo workspace of **eighteen crates**, ~46k lines of Rust and ~14.5k lines of Markdown docs. All
crates are `v0.0.x` and pre-stable. Each declares its own license: **library → LGPL-3.0-or-later,
application/binary → GPL-3.0-or-later.**

### 4.1 The live path — arc (c)

| Crate | Side | Role |
|---|---|---|
| `rayland-c` | C | **C's daemon.** Local vtest server for a stock Venus ICD; hands out memfds, watches the ring, relays bytes. Also hosts WP0's Wayland proxy. ~6.9k lines. GPL, unpublished. |
| `rayland-s` | S | **S's daemon.** Applies relayed messages to a real `libvirglrenderer`; owns the return-path barrier and presentation. ~6.0k lines. GPL, unpublished. |
| `rayland-relay` | both | The **(c)1 relay wire protocol**: `C2S`/`S2C` messages and `postcard` framing. Pure data — no GPU, no sockets, no async — because C must never link a GPU stack. Also carries the `trace` stage tracer. LGPL. |
| `rayland-vtest` | both | The **vtest wire protocol** Mesa's Venus ICD speaks, the `RenderEngine`/`VtestTransport` traits, `EngineError`, and `venus_ring/`. **No GPU dependencies by construction.** LGPL, unpublished. |
| `rayland-venus-proto` | both | **Framing only**: how long is the command at the start of these bytes? Vendors Mesa's *generated* `venus-protocol` headers compiled against a replacement `vkr_cs.h` this crate writes itself, so the borrowed decoders run with no virglrenderer and no Mesa util library. LGPL, unpublished. |
| `rayland-engine` | S | **The real engine.** FFI-embeds `libvirglrenderer` behind `RenderEngine`, driving a Venus context on S's GPU. Contains the **engine actor**. LGPL. |
| `rayland-present` | S | On-screen presentation: finished pixels in a real `xdg_toplevel`, `wl_shm` or zero-copy dmabuf. Shared by `rayland-server` and `rayland-s`. LGPL. |

### 4.2 Applications and fixtures — they know nothing about remoting

| Crate | What it is |
|---|---|
| `rayland-refapp` | C0's captured workload: an **ordinary** offscreen Vulkan triangle with **zero `rayland-*` dependencies**. Its value is that it is boring and typical; keep it that way. |
| `rayland-icosa-core` | Shared foundations for the icosa fixtures: geometry, frame-indexed animation schedule, Mandelbrot math, and bit-exact `log2`/`sin`/`cos`. **No dependencies at all**; its correctness is arithmetic. |
| `rayland-icosa-vk` | The Vulkan scaffolding both fixtures share, so they **cannot drift** in the parts that must be identical for the comparison to mean anything. |
| `rayland-icosa-cpu` | **Fixture A.** Spinning icosahedron textured with a fractal computed on **its own CPU** and written into persistently-mapped `HOST_COHERENT` memory every frame — no flush, so **no call on the wire** saying a megabyte changed. That is the (c)2 problem stated in executable form. |
| `rayland-icosa-gpu` | **Fixture B.** Same picture, same schedule, same render loop; only the fractal moves — evaluated in a fragment shader, so ~80 bytes/frame cross mapped memory instead of ~1 MiB. It is the **volume control** for fixture A, not an alternative to it. |
| `rayland-icosa-window` | **A demo, not a fixture, and must never be mistaken for one.** Live window, human-watchable, nothing reproducible. Exempt from the fixture rules: it *may* depend on `rayland-*` crates and it *has* a compositor-paced redraw loop (which would destroy a fixture's bit-identical comparison). |

**The fixture discipline matters to any plan that touches them:** the two fixtures must be identical
in everything but the property under study, and they must have **no redraw loop**, or their
native-vs-remoted comparison stops being bit-exact and stops being evidence.

### 4.3 Arc (s) — the superseded hand-rolled arc, still passing

`rayland-wire` (messages + framing), `rayland-client` (C side), `rayland-server` (S side),
`rayland-transport` (QUIC stream adapters over `quinn`), and `rayland` (the published crates.io
name-holder and future facade). SP0–SP3 built Rayland's own protocol end to end; all complete and
merged. **The code is untouched and its tests still pass**, coexisting with arc (c) until arc (c)
fully supersedes it.

---

## 5. Where the project stands

### 5.1 Roadmap

| Phase | Status |
|---|---|
| SP0 · First light — triangle, TCP, replay on a real GPU, PNG | **done** |
| SP1 · Onto the screen — live Wayland window on S | **done** |
| SP2 · Real transport — TCP → QUIC | **done** |
| SP3 · Zero-copy presentation — dmabuf export, `wl_shm` fallback | **done** |
| C0 · Venus first light — unmodified app via Mesa's Venus ICD, PNG bit-identical to native | **done** |
| (c)1 · The network | **done** |
| (c)2 · Mapped memory and the readback return path | **done** |
| **WP0 · Wayland proxy first light** | **ACTIVE** — 4.3 and 4.5 done; commit gating and hardening remain |
| (c)3 · Content-addressed assets | planned |
| (c)4 · Real/complex applications; GL via Zink | planned |
| SP4 · Adaptive L3, session/security (SSH bootstrap, sandboxing) | planned |
| SP5 · Proxy completeness (Sommelier/waypipe-grade Wayland coverage) | planned |
| Audio | planned, separate track (transport reservations already made) |

### 5.2 The headline: the thesis is proven

**An unmodified Vulkan application runs on C, is rendered by S's GPU, and animates live in a window
on S's screen.** `scripts/icosa-remote-demo.sh` does it, apollo → dop561, commands over the wire,
not pixels. That was the whole bet.

### 5.3 What is measured, not merely believed

The project has an unusually strong measurement discipline; these are the numbers a plan should
reason against rather than re-deriving.

| Quantity | Value |
|---|---|
| Shipping-config failure rate, real network | **0 failures in 480 runs** → <0.62% at 95% (rule of three) |
| Stale frames after the G' barrier | **0 across 20 real-network runs** |
| `icosa-gpu` frame time, real network | **~41 ms/frame** |
| `icosa-cpu` frame time, real network | **283 ms/frame** — mapped-write volume *was* the dominant cost |
| `icosa-gpu` loopback | ~50 ms/frame with a **78 KB** return path |
| Readback message count after gap-threshold coalescing | ~5000 → **~180 messages/frame**, still bit-identical, still 0 stale |
| Batching `ship()`'s per-message lock and flush | **1.03×** — i.e. not the bottleneck |
| Venus semaphore/event/query feedback, loopback | **1.23×** (median `draw_readback` 48.7 ms → 39.5 ms, 120/120 bit-identical) |
| Feedback-arm failures | **1 in 92** runs, vs 0 in 20 without — *not* a significant difference |
| `VK_ERROR_DEVICE_LOST` on `vkQueueSubmit` | NVIDIA RTX A500 **7/14** runs lost; Intel Iris Xe **0/10** |
| Teardown `SIGABRT` (libepoxy, from `virgl_renderer_cleanup`) | was ~21%, **fixed** |
| **WP0 return traffic, presented-buffer exclusion off / on** | **307,776 B → 219 B per frame: 1,406×** (5 frame-matched runs a side, A/B inside one binary) |
| WP0 forward traffic, same A/B | 3,723 B → 3,594 B per frame — **1.04×, i.e. unchanged** |
| **WP0 end-to-end failure rate** | **59/60 runs clean**; the one failure is a teardown artefact of the definition, so **0 genuine defects in 60** |
| WP0 frame rate, 20 s runs against headless weston | 261–489 attaches (median 438) = **13–24 fps** |
| **WP0 frame time, attributed (loopback)** | **65.8 ms** = 0.5 GPU + ~24.9 compositor pacing + **~40.4 Rayland** |
| Native ceiling, same compositor and app | **25.4 ms/frame (39.4 fps)** — of which 24.9 ms is pacing, so the GPU work is **0.49 ms** |
| **Synchronous round trips per WP0 frame** | **≈4.4** (S→C replies), ≈6.4 counting control — an *n × RTT* floor on any link |
| **Round trip decomposed (loopback, 2026-08-31)** | C flush → **S read 3.9 ms**; S read → S reply 3.2 ms; S reply → C read 0.67 ms |
| `reply_arena_fence_signaled` lock-held, before / after | p50 **1048 µs → 131 µs**; p99 134 ms → 8.4 ms; worst 537 ms → 33.5 ms |
| C→S messages per frame, before / after coalescing | **90.3 → 14.8 (6.1×)**; time in `send()` 1,644 → 303 ms (5.4×); bytes +6.8% |
| Median frame gap across **both** of those fixes | **61 → 61 ms — unchanged** (see the finding below) |

**Frame time is the synchronous round trip.** With feedback off the app implements `vkWaitForFences`
by polling `vkGetFenceStatus`, and *every poll is a full C→S→execute→reply→C cycle*. It is not
bandwidth, not message count, not flush syscalls. The readback is **not** fragmented (`res=5`
averages 377-byte runs); the one-byte flood is the **reply arena**, whose gap-0 grain is a
deliberate correctness property — a gap byte is one S did not write.

**And as of 2026-08-31 there is a sharper statement available, which a plan must not contradict: frame
time is not bound by CPU work on either side.** Two independent mechanism fixes landed the same day —
S's largest lock-holder (`reply_arena_fence_signaled`, a word-by-word walk of a 1 MiB reply arena on
every ring-progress event, replaced by a `memchr` byte search: **8× at the median, 16× at p99 and at
the worst case**) and C's forward message flood (5,409 of 5,495 `BlobData` per run carrying 1–3 bytes,
gap-coalesced: **6.1× fewer messages, 5.4× less time in `send()`**). Each collapsed its mechanism by
5–8×. **Neither moved the median frame gap measurably.** The chain is *serialized latency*, so what is
left in it is the **number** of round trips per frame and the fixed cost each pays.

The `PROGRESS_POLL` (200 µs, `rayland-s`) is the standing suspect for that fixed cost. Note carefully
that the experiment which appears to refute it — 200 µs → 20 µs measured *worse*, 11.2 fps against
12.5–14.9 — **was run when every poll dragged a ~2 ms arena scan behind it**. That scan is now 131 µs.
The refutation was of a different system and the experiment needs re-running; do not treat it as
settled either way.

**The untested prediction.** All of the above is loopback on a fast laptop, where saving C CPU buys
nothing because C has CPU to spare. The forward-coalescing change predicts a real gain on a **weak C**
— the riscv64 milkv board that manages 5 fps — and that has not been measured.

### 5.4 Two findings that overturned earlier beliefs — do not re-propose the retired versions

1. **The three-day `vkQueueSubmit` "CS error" was never a Rayland bug.** It is `VK_ERROR_DEVICE_LOST`
   (`VkResult=-4`) from the real submit on S's GPU. Venus reports device loss through a branch that
   runs only when `flags == 0x0`, so it surfaced as a generic `%s resulted in CS error` with no log
   of its own. Proved by building a patched `virgl_render_server` and spawning it via
   `RENDER_SERVER_EXEC_PATH`, which the *system* library honours. **It is GPU-specific**, and vkcube
   defaults to the discrete GPU — hence the `--gpu_number 0` gotcha.

2. **Venus fence feedback is load-bearing and must stay off; the other three feedbacks are
   unattributed.** `no_fence_feedback` is required: the G' barrier works by spotting the app's
   `vkGetFenceStatus` reply, and fence feedback removes that poll — enabling it gives exit 134 and
   zero frames, immediately and every time. But semaphore/event/query feedback is a different
   question: worth 1.23×, and its one observed failure was hunted through **82 further clean runs**
   (including 60 unattended with genuine core capture armed, no core produced). 1/92 vs 0/20 is not
   significant, and the failure **cannot now be pinned on feedback at all**. The flags remain off
   because an unexplained total-session loss is unexplained either way — but the reason is "we do
   not know what that was", not "feedback breaks it". A plausible-sounding explanation ("(c)1 does
   not relay the feedback pages") was checked and **refuted**: `emit_blob_writes` excludes only
   rings, and `take_bytes_s_wrote` detects change by diffing a shadow, so it catches writes the GPU
   makes directly. Measured: S ships back `res=2` and `res=5` and nothing else, traffic within 0.1%.
   **There is no un-relayed feedback page in this workload.**

---

## 6. What is open

Ranked as the project itself ranks them.

### 6.1 WP0 · Wayland proxy first light — the active front

**The point of WP0:** today S presents the application's *readback buffer* — pixels the GPU wrote,
copied back to memory, shipped, and re-uploaded as `wl_shm`. That is a bandwidth tax and it is why
resolution costs anything at all. WP0 replaces it by **proxying the application's real Wayland
protocol**: the app connects to a proxy on C rather than to a compositor, its
`wl_surface`/`xdg_toplevel` requests are relayed to S's real compositor, and the one thing that
cannot cross a network — the swapchain `wl_buffer`'s file descriptor — is replaced by a
**`BufferToken`** naming the S-side resource the command relay has already rendered. **No pixels
cross the network.**

**Task state:**

| Task | State |
|---|---|
| 4.0 spike — can S export a compositor-importable dma-buf? | **done**, and its first answer was wrong (see below) |
| 4.1 — C-side wiring (link-backed sink, inode→res_id resolver, proxy as a 4th thread) | done |
| 4.2 — S router, persistent Wayland client, object-id map, session replay | done |
| **4.3 — token → `wl_buffer`** | **DONE and verified over the real network, 2026-08-29** — S builds real `wl_buffer`s from relayed tokens (4 of 4 swapchain images, no protocol error) |
| 4.4 — event return path (eventfd wakeup, `send_event`, `S2C::WaylandEvent`, `configure`) | **genuinely working** — measured: vkcube receives both `configure`s through the tunnel and **acks** them |
| 4.5 — end-to-end: vkcube's spinning cube on S's screen | **REACHED 2026-08-29**, confirmed by a human watching the screen — and with **pixels no longer crossing the network** (S→C fell 571×). See §6.1.2 |

**4.3 part 2 is the immediate next piece of work.** Its shape turned out smaller and different from
the plan's decomposition: **C's half was already complete** (the `params.add` handler resolves the
passed memfd's inode to an S-side resource id, `create_immed` assembles the full `BufferToken`, and
an unresolved fd is deliberately *not* forwarded rather than guessed at — the doc comments calling
this "the next sub-step" are stale). **4.3 is S-side only.** Part 1 is landed: S retains the dma-buf
descriptor virglrenderer exports per blob, exposed as `Applier::exported_fd() -> BorrowedFd`, a
borrow and never ownership, because `mem->exported` permits exactly one export per resource and it
already happened at creation.

Part 2 is unwritten. `crates/rayland-s/src/wayland_client.rs:592` still logs *"buffer-token request
(obj N opcode M) deferred to 4.3; skipped"* and drops the whole request.

**The sequence as built, with two corrections to what this document previously said.** Both are
recorded rather than silently rewritten, because the planning side reasoned from the old version.

> **Correction 1 — it is three requests, not two.** The earlier text had S synthesizing only the
> `add` and letting "the existing path replay `create_immed`". There is no such path: C's proxy
> intercepts `zwp_linux_dmabuf_v1.create_params` and **does not forward it**, so when a
> `create_immed` arrives S has no `zwp_linux_buffer_params_v1` object at all, and the app-side params
> id is not in the id map. The request would be refused at the sender lookup before anything looked
> at the token. S must originate the whole sequence.
>
> **Correction 2 — the `wl_buffer` child is declared at the *params object's* version, not v1.** A
> Wayland object inherits the version of the object that created it, and `wayland-backend` enforces
> it: `send_request` **panics** unless `child_spec`'s version equals the sender's. The first
> two-machine run of this code died on exactly that (`expected version 3 but got 1`), taking the
> daemon's main thread with it. `wl_buffer` has only ever *had* version 1, which makes this
> genuinely counter-intuitive; the version is a statement about lineage, not capability.

1. Resolve `token.resource_id` to an **owned duplicate** of S's exported dma-buf, through the
   `ExportedFdSource` trait. The trait exists so the lock rule is structural: the applier guard
   cannot escape `dup_exported_fd`, so no caller can hold it across a compositor round trip.
2. **`create_params` (opcode 1)** on the bound `zwp_linux_dmabuf_v1`, child
   `zwp_linux_buffer_params_v1` at the bound dmabuf version. Map the app's params id to the result.
3. **`add` (opcode 1)** with `[Fd, Uint(0) plane_idx, Uint(offset), Uint(stride), Uint(mod_hi),
   Uint(mod_lo)]` — offset and stride from the token, never derived.
4. **`create_immed` (opcode 3)** with `[NewId(null), Int(width), Int(height), Uint(format),
   Uint(0) flags]`, child `wl_buffer` **at the params object's version**. Map the app's buffer id to
   the result; the app's own `attach`/`commit` then replay through the path that already works.
5. **The commit still wants gating on the frame's completion** — the (c)2 G' signal. Deliberately not
   built with the token path: shipping both together would make any failure ambiguous between them.

All three sends are wrapped in `catch_unwind`, matching the generic replay path, because
`send_request` panics rather than erring on a protocol violation and a refused buffer must cost the
frame, not the session.

**Three details a plan must decide or respect, none of which the written plan mentioned — all three
are now settled and built:**

1. **S must *synthesize* the `add`, not translate it.** C intercepts `add` and drops the fd by design
   — that *is* buffer-by-token — so S's params object has **no planes** when `create_immed` arrives.
   This is a request S **originates** rather than replays: a first for the replay module.
2. **The plane layout travels on the token. DECIDED and landed, 2026-08-29.** `BufferToken` now
   carries `stride` **and** `offset`, both taken verbatim from the app's `params.add`. Deriving
   `width × bpp` was the assumption the plan flags as **garbling pixels rather than failing
   cleanly**, and assuming `offset = 0` is the same class of assumption at the same cost of one
   `u32`. C knows both values because Mesa passes them to `add` before the proxy drops the fd, and
   they originate in the image layout Venus queried on **S's own GPU** — so the token carries S's
   own layout round-tripped through the application, not a guess made on the machine with no GPU.
   Shipped with it: C now **refuses** multi-plane buffers rather than approximating them. A second
   `add`, or one whose `plane_idx` is not `0`, poisons the params object so `create_immed` forwards
   nothing — the app keeps a locally valid `wl_buffer`, S is never told to present it, and the
   broken assumption appears in the log where it broke. `plane_idx` is deliberately *not* a carried
   field: a token describes exactly one plane by construction.
3. **Lock discipline.** Resolve and clone the fd **under the applier lock, and release it before any
   `send_request`.** A Wayland call made under that lock puts the relay's mutex behind a compositor
   round trip.

**The plumbing question is settled.** `WaylandReplay` reaches `Applier`'s descriptors through the
`ExportedFdSource` trait, implemented by a newtype over the `Arc<Mutex<Applier>>` in `main.rs`. This
was chosen over handing the replay the `Arc` directly so the lock discipline is enforced by the type
rather than by a comment.

**What the real-network run showed, 2026-08-29 (`scripts/wp0-vkcube-two-machine.sh`, one run).**
vkcube on apollo, its window replayed onto dop561's compositor:

- **4 of 4 swapchain images became real `wl_buffer`s** from relayed tokens — 500×500, XRGB8888,
  LINEAR — with **no protocol error**. Because `create_immed` is the *immediate* variant, which
  raises a fatal protocol error on a bad buffer, S's compositor accepting it silently is positive
  evidence that the dma-buf imported correctly.
- The app then **attached, damaged, requested a frame callback, and committed** — the full present
  sequence, replayed with no S-side error. The proxy trace ran to 64 lines, against the 36 recorded
  on 2026-07-25 when the app died after binding dmabuf six times.
- **Then it stalls — and as of 2026-08-29 the cause is known exactly**, found by instrumenting both
  halves of the event return path rather than by reasoning about them. See the next subsection.

Note the measured values in this configuration were `offset 0, stride 2000 = width × 4` — i.e. here
the derivation would have *happened* to be right. That is exactly why the token carries them: the
fixture proves the path, the configuration does not prove the assumption.

### 6.1.1 The frame-callback stall — located, 2026-08-29, and NOT yet fixed

**The picture arrives; the animation does not.** vkcube's window appears on dop561's screen with the
cube correctly rendered by S's GPU (`docs/data/2026-08-29-wp0-event-witness/cube-on-dop561.png`), and
then never updates: the same 450×450 interior was **pixel-identical, 0 of 202 500 differing, across
two captures 17 s apart**. Two runs, both identical in behaviour.

**Why:** the application never receives its second `wl_surface.frame` callback, and it will not draw
again until it does.

**Where it is lost — a recycled-id race in C's proxy object map, not anywhere in S:**

1. The app creates frame callback #1 with app id 24; C registers it.
2. S's compositor fires `done`; S emits it; C delivers it. **A `wl_callback.done` is a *destructor*
   event** — delivering it destroys the object.
3. The app immediately creates frame callback #2, and libwayland **reuses the id it just freed**: also
   24. C registers it.
4. **Only now** does the backend's `destroyed()` for callback #1 run — and `ProxyState::objects` is
   keyed by bare `protocol_id`, so `objects.remove(&24)` removes **callback #2's** entry.
5. Callback #2's `done` arrives and has nothing to deliver to:
   `drop:unknown-object app_obj=24`. The app waits forever.

S is entirely correct throughout: it emitted both `done`s, and mapped both callbacks consistently
(`map s_obj=13 app_obj=24` twice — both ends recycle the same ids in step).

**FIXED 2026-08-29.** `destroyed()` now removes an entry only when it still holds the object being
destroyed — `ObjectId`'s equality includes a per-client serial, so two objects that shared a slot at
different times compare unequal. `PendingParams` got the same treatment via an `owner: ObjectId`
field, since its values are not `ObjectId`s; that half has no observed symptom yet, but the witness
log proves id reuse crosses interfaces, so a late params destroy wiping a new params object's `add`
state is the same bug in different clothes. Guarded by `wayland_proxy_recycled_id.rs`, which drives
thirty callback cycles and fails **10/10** against remove-by-number.

**Correction to the previous account:** this document and the 2026-08-29 report called `rayland-s`'s
`IdMaps` an "accidentally safe latent twin". That was **wrong**, and acting on it would have imported
the bug into S. `IdMaps::insert` writes both directions at object *creation*, so a recycled id is
refreshed by its new owner; nothing removes by number, which is exactly why nothing can go wrong. The
no-op `destroyed()` there is **load-bearing**. Nor does never removing leak — growth is bounded by the
app's peak live-object count, because ids are recycled.

**Verified, and it did not make the cube spin.** Zero `drop:unknown-object` in every run; in the run
where callbacks flowed, S emitted 9 `wl_callback.done` and C delivered 9, and the app went from 1
attach to 9 attaches / 9 frame requests / 10 commits. But three captures six seconds apart still
differ by **0 pixels of 202,500** — because the app had done all nine attaches within ~18 s and
every photograph came after it stopped. See §6.1.2.

### 6.1.2 WP0 reaches end-to-end — and the pixel stream that was hiding behind it

**Reached 2026-08-29, confirmed by a human watching the screen.** An unmodified `vkcube` runs on
apollo, is rendered by dop561's GPU, and **spins in its own window on dop561's screen**. Two defects
stood between the recycled-id fix and that result, and the second is the more important.

**1. The vanishing window — a cached handle to a destroyed object.** `WaylandReplay` recorded the
S-side `ObjectId` of the `zwp_linux_dmabuf_v1` global at the moment the *application* bound it. The
application binds that global repeatedly while probing formats — **twelve times** in one measured run
— and **destroys each one**. The cached id therefore named a dead object as soon as the app moved on:
every later `create_params` failed with `Invalid ObjectId`, so no `wl_buffer` existed, so the app's
`attach` failed too — and **a `wl_surface` with no valid buffer is unmapped by definition**, so the
compositor removed the window from the screen while the application carried on unaware.

S now binds **its own** dmabuf global, once, and never destroys it: it needs *a* factory, not the
application's factory. This is §6.4's identifier hazard in its second form — *a handle you cached is
not a handle you still have* — and it was introduced in `rayland-s` on the same day its twin was fixed
in `rayland-c`.

**2. S was shipping every rendered frame back to C.** Measured with C's own per-channel counters, same
workload before and after, ~60 s per run:

| | Before | After |
|---|---:|---:|
| C→S total (commands) | 804,814 B | 1,626,138 B |
| **S→C total** | **105,254,034 B** | **184,311 B** |
| **S→C per frame** | **~877 KB** | **~1.9 KB** |

A 500×500×4 frame is 1,000,000 bytes. That is a **frame-sized payload per frame, crossing the network
to a machine with no display**, where nothing consumed it — in the project whose entire thesis is that
pixels do not cross the network.

**The mechanism.** The (c)2 return path ships back whatever S's GPU wrote into any blob. That is
correct for a **readback** — an application that maps a GPU-written buffer and reads it — and it has no
way to distinguish that from a **swapchain image**, which the application only ever *shows*. Only the
WP0 token path knows which is which. So it now says: building a `wl_buffer` from a resource marks that
resource **presented**, and presented resources are excluded from the return path exactly as rings
already are. **571× less return traffic.**

The exclusion is narrow by construction — an offscreen fixture never populates the set, so the (c)2
readback path is untouched and its GPU loopback e2e still passes. **Known limit, written into the code
rather than left implicit:** an application that both *presents* a buffer and *reads it back* would now
be denied the readback. None is known here, since a presented swapchain image is `DEVICE_LOCAL` and
never mapped.

**Why it survived so long, which is the part worth learning from.** The display was *already* correct:
S's compositor imports S's own dma-buf, and no pixel is needed to make the window appear. The waste had
**no symptom**. Every test passed. The demo looked exactly like the thesis working. And
`scripts/wp0-vkcube-two-machine.sh`'s own header asserted "**No pixels cross the network**" — a claim
that was false on every run. It was found only because the repository owner, watching the demo, asked
whether pixels were crossing the wire, and the answer was one measurement away.

**Not claimed:** any frame rate, any failure rate, or that presentation is correctly paced or
tear-free. The commit gate remains untouched and this is a handful of runs.

### 6.1.4 The latency half, measured — 2026-08-30

The bandwidth half of the thesis is settled (~3.6 KB/frame out, 219 B/frame back). The latency half
had never been examined. Full data: `docs/data/2026-08-30-wp0-frame-time/`. **All loopback.**

**The number that transfers to a real network: ≈4.4 synchronous round trips per frame.** That is an
*n × RTT* floor no bandwidth saving can remove — 2.2 ms on this LAN (invisible), 22 ms across a city
(+33%), **132 ms to another country (3× worse), 352 ms transatlantic (fatal)**. It is a *good* number:
small enough that LAN and metro links work, and the right thing to attack if a WAN is ever the goal.

**The budget, per frame:** 0.49 ms GPU + ~24.9 ms compositor pacing + **~40.4 ms Rayland** = 65.8 ms.
So **38% of a WP0 frame is pacing that a native client pays too.**

**The ceiling is 39.4 fps, not 60**, and it is entirely weston's pacing — the GPU work is 0.49 ms.
Any comparison of Rayland against 60 fps would have been wrong.

**Ruled out:** GPU render time (0.7% of the frame), the network (loopback throughout), bandwidth, and
polling granularity alone (500 µs/200 µs sleeps cannot make 4–8 ms intervals). **Still live:** the
forward blob-sync volume — 72.5 messages/frame, and milkv's ~3.7×-slower core produced almost exactly
3.7× fewer frames, which is the signature of per-message cost.

**Not fully decomposed:** the ~40 ms is located, not itemised — the application's submission and the
`wl_buffer` commit have no trace stations, so two segments of the path are uninstrumented.

**On `vkgears`, corrected same-day:** it segfaults against **seatless** compositors because
mesa-demos 9.0.0 dereferences the `wl_seat` global unconditionally — an upstream bug, and the headless
weston used here advertises no seat. Natively against COSMIC it runs at **61 fps**. Its failure
*through Rayland* is a Rayland defect; see §6.1.5. Every figure above is `vkcube`'s.

### 6.1.5 The dropped `wl_keyboard.keymap` CRASHES applications — 2026-08-30

Recorded since the event-witness session as a capability gap (*"no relayed application will have a
keyboard"*). **It is not a gap, it is a crash**, and it is ours:

1. C's proxy advertises `wl_seat` unconditionally (`wayland_proxy.rs:998`).
2. S relays `wl_seat.capabilities`, so the application creates a `wl_keyboard`.
3. S **drops `wl_keyboard.keymap`** (it carries an fd — correct in isolation) …
4. … and **keeps relaying that keyboard's other events**. The application dereferences an xkb state
   that was never created: `SIGSEGV in xkb_state_update_mask`, backtrace confirmed.

**Dropping an event whose dependants are still delivered is the bug**, not the drop itself.

It also retires the earlier "vkgears works against headless weston, dies against COSMIC" table: a
seatless compositor never sends a keymap, so nothing depends on the missing one. That table was
measuring whether a seat existed to expose our own gap.

**Cheap mitigations** (unapplied, a design decision): suppress the keyboard bit in relayed
`capabilities`, or stop advertising `wl_seat`. **The real fix** is substituting the keymap's *content*
as the buffer path substitutes a token — it is a bounded string, unlike a swapchain.
Evidence: `docs/data/2026-08-30-wp0-frame-time/keymap-drop-crashes-applications.md`.

### 6.1.3 WP0 measured, and two defects a second application found — 2026-08-30

**The numbers now come from repetition rather than a pair of runs** (`scripts/wp0-soak.sh`, data in
`docs/data/2026-08-30-wp0-rate-and-traffic/`).

- **Rate: 59 of 60 runs clean.** The single failure is an artefact of the failure *definition*: two
  events dropped during the application's final teardown, for objects it had legitimately destroyed,
  immediately before "session ended cleanly". So **0 genuine defects in 60 runs** — but the definition
  needs a teardown guard before that number means what it appears to.
- **Throughput:** 261–489 attaches per 20 s run (median 438) = 13–24 fps, no liveness failure.
- **Traffic, A/B'd inside one binary:** S→C **307,776 → 219 B/frame (1,406×)**; C→S **3,723 → 3,594
  B/frame (1.04×)**. The unexplained C→S rise in the 2026-08-29 report **was not an effect** — it came
  from comparing a 120-frame run against a 96-frame one. Both of that day's traffic figures were
  uncontrolled comparisons.
- **The recycled-id fix is load-bearing, not an edge case:** one 20-second run declined **470** stale
  destroys.

**The soak must run against headless weston, not the desktop, and this is not incidental.** A
compositor emits frame callbacks only for surfaces it composites, so a desktop soak scores every
blank, lock and workspace switch as a liveness failure. Three things are required and each was learned
by getting it wrong: the **GL renderer** (pixman cannot import a dma-buf), **`__EGL_VENDOR_LIBRARY_FILENAMES`
pinned to Mesa** (weston's EGL otherwise composites on the NVIDIA card while frames render on Intel),
and **`--idle-time=0`** (weston stops compositing after 300 s, which looks exactly like the application
stalling — the second time in two days a compositor declining to draw was mistaken for a Rayland bug).

**Both defects below were FIXED on 2026-08-30, and `vkgears` now runs end to end ON LOOPBACK** — 345
attaches, 345 frame callbacks delivered, 10–13 fps, zero panics. A second independent application
through WP0.

> **Qualifier attached 2026-08-30, and it should travel with this claim wherever it is repeated.**
> That run was **loopback**, and §7's own rule is that loopback proves little about the forward
> mapped-memory break or about feedback. The two-machine confirmation of `vkgears` is **owed and not
> done** — and note it may not be obtainable as stated, since `vkgears` has since been found to
> segfault natively against headless weston and on milkv under lavapipe. `vkcube` *has* been run
> apollo→dop561 and milkv→dop561 with a window on screen.
The guarded soak that followed was 25/25 clean (loopback). The original findings are kept below because
the *shapes* are what matter:

1. **FIXED — a bind capped on S was not propagated to its children.** `handle_bind` correctly caps a bind at the
   version S advertises, but objects created from that global still carry the version **C** stamped —
   the application's. A Wayland child inherits its parent's version, so the first `get_xdg_surface`
   panics with `expected version 5 but got 6`. **This is the third instance of the version-inheritance
   rule** (§6.4), and the first where S's own capping creates the mismatch. vkcube never exposed it
   because nothing was ever capped for it.
2. **FIXED (behaviour, not survivability) — `catch_unwind` around `send_request` cannot save the
   session, and no longer claims to.** It catches the panic and logs
   "request dropped, session continues" — then the process **segfaults**, because the panic occurred
   with the `maps` mutex held, poisoning it, and the next
   `.expect("the WP0 id maps lock is never poisoned")` finds it poisoned. The comment is false and the
   reassuring log line is worse than silence.

**`rayland-icosa-window` cannot run over WP0, correctly.** It presents via `wl_shm`, which the proxy
does not advertise, and refuses cleanly. `wl_shm.create_pool` passes a file descriptor — which cannot
cross a network — and its contents are pixels, ~1 MB/frame, exactly what the presented-buffer
exclusion removed. It is a `wl_shm` client; WP0 is a dmabuf mechanism.

## 6.2 The cheapest queued experiment — needs both machines

```
TRIES=400 VN_PERF_SETTING=no_multi_ring,no_fence_feedback scripts/soak-failure-rate.sh
```

The semaphore/event/query feedback arm: **worth 1.23×**, currently held back by exactly one
unexplained failure (1/92) against a shipping arm clean through 480 runs. One night of soak settles
it either way. This is the best ratio of information to effort currently on the board.

### 6.3 Longer-term open questions

- **The synchronous round trip itself.** Now the *measured* explanation for frame time, and since
  2026-08-31 it is specifically the round-trip **count** and each trip's fixed cost, not CPU work on
  either side — see §5.3. Candidate directions: the 200 µs `PROGRESS_POLL` (re-test it; its earlier
  refutation predates the arena-scan fix), adaptive polling, reply batching. Matters most when
  latency is high.
- **A latent coalescing hazard on S's return path**, found by the safety check the C→S coalescing
  change required and deliberately *not* fixed there. `Applier::take_app_blob_writes` coalesces at
  gap 256 on the argument that `res6` is "S-written and C-read-only" — but its filter selects every
  non-ring, non-Venus-internal, non-presented blob. A blob **both** sides write would receive S's
  stale copy of the application's own bytes at a 256-byte grain: the "false sharing at S's page
  grain" hazard that byte-granular diffing was introduced to kill, reintroduced smaller. It does not
  fire on today's workloads.
- **Multi-queue support.** The return-path barrier decodes the application's real per-queue
  `ring_idx` from its `vkGetDeviceQueue2`; a genuinely multi-queue application is unexplored.
- **The mapped-memory forward break is still not exercised.** This is subtle and important: the
  `icosa_cpu` fixture renders bit-identical across the loopback relay, but **on loopback the
  fixture's uninterceptable mapped writes still reach S**. What (c)2 proved is the **readback return
  path**. The forward case — a true network, where those writes *cannot* reach S — is the fixture's
  original purpose and is **still waiting**. `docs/icosa-fixtures.md` explains why it did not bite.
- **(c)3 content-addressed assets**, **(c)4 real applications and GL via Zink**, then the SP4/SP5
  hardening tracks and audio.

---

## 6.4 An epistemic hazard this project has now hit twice

**A test written from the same belief as the code it tests can only confirm that belief.** It feels
like verification and is not; it is the belief being restated in a second file.

The worked example is WP0 4.3's `create_immed` child version. The plan said the `wl_buffer` child is
version 1. The code said version 1. **The unit test asserted version 1 and passed.** All three were
wrong in the same way, because all three came from the same source — and `wl_buffer` really does only
have a version 1, so the belief was *locally* true and merely irrelevant. Only S's real compositor
knew that a Wayland child inherits its **parent's** version, and it said so by panicking.

This is not an argument against unit tests; the same session's mutation-checked tests caught three
real regressions. It is an argument about what a test can and cannot witness:

- A test can witness **internal consistency** — that the code does what its author meant.
- A test cannot witness **a fact about the world** the author did not know.

**A second worked example, from the session that fixed the recycled-id race.** The first regression
test written for it **passed against the buggy code**, because it did a round-trip between the two
callbacks — which let the destruction land before the new object was registered, quietly stepping
around the race. It tested what its author imagined the sequence to be. Even after it was made to
reproduce, it failed against the bug only **2 runs in 10**: a single sample of a race is a coin flip,
and a test that catches a defect a fifth of the time will sit green in CI with the defect present.
Driving thirty cycles, as a real application does, took it to **10/10 failing against the bug and
10/10 passing with the fix**. Both rounds were found by mutation, not by inspection.

The countermeasures this project already uses, and should keep using:

- **Mutation-check every new assertion.** Break the implementation the test exists to forbid and
  confirm *that* test fails and the others do not. It catches a test that asserts nothing — though,
  note carefully, it would **not** have caught the version bug, because the mutation would have been
  derived from the same wrong belief.
- **Prefer a witness to an argument.** When a component's behaviour is in question, instrument it and
  read what it says. The three-day `vkQueueSubmit` wall, the stale-frame misdiagnosis, and this
  session's frame-callback stall were all settled by an instrument after theories had failed.
- **CLOSED BY CONSTRUCTION, 2026-08-30 — the version-inheritance rule.** A Wayland child inherits the
  version of the object that created it, and `wayland-backend` enforces it by panicking. This bit three
  times (`create_immed`'s `wl_buffer` child; the params object; `get_xdg_surface`, found by `vkgears`),
  the last because S *caps* a bind at what its compositor advertises and the children then carried the
  application's higher version. It is no longer a hazard to remember: `IdMaps` records every object's
  version, seeded with the capped value at bind and propagated to each child, and `child_spec` is built
  from **the sender's** version — the wire's is logged and decides nothing. The invariant is *every
  object's version equals the capped version of the global it descends from*. Kept in this list as a
  worked example of the shape: three instances of one rule, each fixed as an instance, until the fourth
  forced fixing the rule.
- **`catch_unwind` around a dependency's panicking API is not a recovery mechanism** unless that
  dependency's locks survive the panic. `Backend::send_request` panics on a protocol violation **while
  holding wayland-backend's own `ConnectionState::protocol` mutex**, and `lock_protocol()` is a bare
  `.lock().unwrap()` — so the panic poisons it and the *next backend call of any kind* aborts the
  process. The wrapper duly caught the first panic and logged "session continues"; the process then
  segfaulted one call later. **It converted an immediate, legible crash into a reassuring line followed
  by a crash**, which is strictly worse than no wrapper. Ask, before wrapping: *does this API panic with
  a lock held, and is that lock mine?* Rayland's own map lock is fine — but only because it is never
  held across a call that can panic, which is a property that has to be maintained, and is now written
  where it can be checked.
- **A Wayland protocol id is a slot number, not an object identity** — the third hazard of this
  family, and the one that cost a frozen window. An id is unique only among objects alive at one
  instant; the moment an object dies, the next one gets its number. Any map keyed by such an id must
  remove **by identity**, never by number, because cleanup runs late and the slot may already have a
  new, live owner. The general form: *an identifier that is reused is not an identity, and code that
  treats it as one is correct only until something dies.*
- **Wording that says "absent" can hide something that crashes.** `wl_keyboard.keymap` was recorded
  for weeks as a capability gap — *"no relayed application will have a keyboard"* — which reads as
  something merely missing, a feature nobody has got to yet. It was in fact segfaulting every
  application that used a seat (§6.1.5). Nothing about the sentence was false; it was the *register*
  that misled, and it survived several sessions because "will not have a keyboard" sounds like a
  limitation rather than a defect. When recording a gap, say what happens to a program that hits it,
  not what it lacks.
- **A claim in a comment is not a measurement.** `scripts/wp0-vkcube-two-machine.sh`'s header asserted
  "No pixels cross the network" while the running system shipped ~877 KB per frame across it, and
  `CLAUDE.md` repeated the claim. Nothing was lying; the sentence described the *design*, and no test
  distinguished design from behaviour. Where a document states a quantity — no pixels, zero copies,
  bounded memory — something must **measure** it, or it is a wish with good grammar.
- **Treat "no errors in the log" as evidence only about paths that can log.** §6.1.1's stall sat
  behind four bare `return`s that reported nothing; the previous report's "no S-side replay errors"
  was true and meant nothing.

## 7. Rules that bind any plan

These are not style preferences; they are enforced or load-bearing.

**Structural:**
- C links no GPU stack. Ever. Test-enforced.
- `rayland-engine → rayland-vtest`, never the reverse.
- The ring crosses the wire as **opaque bytes**; the decoder is not load-bearing on the relay path.
- The fixtures stay boring: no `rayland-*` dependencies, no redraw loop, and the pair differs only
  in the property under study.

**Process (from `CLAUDE.md`, binding on every working turn):**
- **The diary.** `docs/DIARY.md` gets an entry every working turn — the *thinking*, not the diff,
  including wrong turns. When a belief is overturned, the old entry **stays** and the overturning
  gets its own entry. Its stated purpose is twofold: a map for whoever tries the idea again if
  Rayland fails, and trust-material for software written by an AI under human supervision. ~4200
  lines today, and it is the single best source on why anything is the way it is.
- **The project map.** `project-map.js` (data) + `project-map.html` (renderer, opened via `file://`)
  must be checked every turn and updated in the same turn if anything it depicts changed.
- **Code conventions.** A doc-comment on *every* function, type, trait, and module. An intent
  comment on every non-trivial line explaining the **why** or the domain meaning, never restating
  syntax. Code and comments must always agree — a stale comment is a bug fixed in the same edit.
- **Documentation conventions.** Written for a reader **not** already familiar with the problem
  space (explicitly including the repository owner). Explain the pitfalls, not just the happy path.
  **Never omit information for the sake of brevity.**

**Git discipline:** the laptop is primary; never commit or push to `main` from a non-laptop session
— push to a clearly-named side branch and leave merging to the human.

---

## 8. Standing gotchas

Each of these cost real time to discover.

- **Run vkcube with `--gpu_number 0`.** It defaults to the discrete NVIDIA GPU and provokes
  `VK_ERROR_DEVICE_LOST`.
- **Never set `VN_DEBUG=no_abort`.** Mesa's stall abort *is* the stall detector.
- **Prefix cargo with `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target` on dop561.**
- **A loopback pass proves nothing about feedback**, and little about the mapped-memory forward
  path. 120/120 bit-identical frames and 1.23× immediately preceded the one feedback failure.
- **`README.md` is stale.** It still says "no working software yet". `CLAUDE.md` and the diary carry
  the current truth.

---

## 9. The physical setup and how to run things

**Two machines, and they are not interchangeable:**

| Host | Role | Has |
|---|---|---|
| **apollo** | **C** — runs the application | 16 cores, no display path needed |
| **dop561** | **S** — GPU and display | NVIDIA RTX A500 + Intel Iris Xe, the monitor, the compositor |

`dogstar` is a travel laptop with neither.

**What runs anywhere** (pure crates, no GPU, no network):
```sh
cargo test -p rayland-vtest -p rayland-relay -p rayland-venus-proto     # 83 tests
```

**What needs both machines:** every end-to-end, presentation, or performance claim.

**The scripts** (`scripts/`), each with a full explanatory header:

| Script | What it does |
|---|---|
| `icosa-remote-demo.sh` | **The demo.** Icosahedron computed on apollo, rendered by dop561's GPU, live in a window on dop561's screen. |
| `c1-two-machine.sh` | (c)1 Task 8 two-machine bring-up: an unmodified Vulkan app renders across a network. |
| `c2-icosa-two-machine.sh` | (c)2's readback-completion gate, proven over a real network. |
| `soak-failure-rate.sh` | Measures a configuration's failure rate over many runs. Deliberately does not stop at the first failure — a rate needs its whole denominator. |
| `c1-sweep.sh` | (c)1 Task 9 measurement sweep: what remoting costs as latency rises (netem). |
| `c1-trace-analyze.py` | Joins the stage trace from one loopback reproducer run. |
| `tools/parse_vkqueuesubmit.py` | Field-by-field decode of a captured submit — the instrument that cracked the device-lost mystery. |

**The patched-`virgl_render_server` recipe** (the instrument that named `VK_ERROR_DEVICE_LOST`) is
written out step by step in the 2026-07-26 diary entry: source from the Ubuntu archive pool over
plain HTTP (no root, no `deb-src`), `meson`/`ninja`/`pyyaml` in a venv, the distro's own
`fix-c23.patch`, then `RENDER_SERVER_EXEC_PATH`. **No root needed.**

---

## 10. Snapshots

`~/bin/snapshot.sh` packs the source-y parts of whatever git repository the current directory is in
— source, docs, configuration; no build trees, no `.git`, no caches. Repo-agnostic: run it from
`~/git/rayland` and you get `/tmp/rayland.tar.gz`, run it from `~/git/eno` and you get
`/tmp/eno.tar.gz`.

```sh
cd ~/git/rayland && snapshot.sh                    # → /tmp/rayland.tar.gz  (~1.8M, 259 files)
snapshot.sh /some/where/else.tar.gz                # explicit output path

# Optional: drop a large machine-generated tree that is technically source but is noise.
SNAPSHOT_EXCLUDE='^crates/rayland-venus-proto/vendor/' snapshot.sh   # ~1.4M, 205 files
```

The list comes from `git ls-files`, so **uncommitted new files are not included** — commit or
`git add` before snapshotting.

---

## 11. Where to look for what

Ordered by how often a design discussion will want them.

| Question | Document |
|---|---|
| Conventions, current status, the crate-by-crate account | **`CLAUDE.md`** — dense and current; the single most information-rich file |
| Why is anything the way it is? What was tried and failed? | **`docs/DIARY.md`** — ~4200 lines, chronological; the tail entry of 2026-07-27 is the pick-up map |
| What is shipped / in flight / an open seam, visually | **`project-map.html`** (opened from disk; reads `project-map.js`) |
| The full architecture, and what exists vs. must be invented | `docs/design/2026-07-13-native-remote-wayland-gpu.md` |
| **Why the socket carries nothing** — required reading for (c)1 | `docs/design/2026-07-15-venus-ring-findings.md` |
| The (c)1 network arc | `docs/c1-the-network.md` |
| Why observe-and-diff cannot work (the walls, in order) | `docs/design/2026-07-17-fence-feedback-walking-skeleton.md` §9–§11 |
| The engine actor | `docs/design/2026-07-18-c2-engine-actor.md` §8–§9 |
| The barrier that works (G') | `docs/design/2026-07-21-c2-getfencestatus-completion.md` |
| WP0's spec and plan, including both 4.0 outcomes | `docs/design/2026-07-22-wp0-wayland-proxy-first-light{,-plan}.md` |
| The mapped-memory problem stated in executable form | `docs/icosa-fixtures.md` |
| C0's Venus first light | `docs/c0-venus-first-light.md` |
| The superseded hand-rolled arc | `docs/sp0-first-light.md` … `docs/sp3-zero-copy-presentation.md` |

There are 31 design documents in `docs/design/`, named by date and subject; the dated filenames make
the chronology of the investigation readable directly from the directory listing.
