# Rayland — working conventions

This file governs how code and documentation are written in this repository. It is
binding: follow it exactly. If a change makes any statement here false, update this file
in the same change.

## What Rayland is

Rayland provides **native remote GPU rendering for Wayland**: an application runs on one
machine but is rendered and displayed on another machine — the one with the capable GPU
and the monitor the user is looking at — by sending a **command stream** across the
network rather than a **pixel stream**.

The full architecture is in [`docs/design/2026-07-13-native-remote-wayland-gpu.md`](docs/design/2026-07-13-native-remote-wayland-gpu.md).
Read it before making non-trivial changes; it explains why Wayland deliberately made
remoteness hard and which ecosystem pieces must grow.

## The S / C vocabulary (do not get this backwards)

Rayland uses X11-era terms, which are the *reverse* of cloud usage:

- **S ("server" side)** — where the **user sits**: keyboard, mouse, **display, GPU**,
  the Wayland compositor, working drivers. The strong machine.
- **C ("client" side)** — where the **application executable runs**. May be weak,
  a different CPU architecture (e.g. RISC-V), or headless. No good display path.

The app on **C** emits rendering commands; **S's GPU** does the drawing and shows the
result on **S's** display. Primary mode ships **commands, not pixels**. A video-encode
fallback exists but is not the goal (in the target setup, C is the wrong place to encode).

## Locked decisions

- **Language: Rust for all code Rayland writes.** The Vulkan command
  serialization/replay engine is *reused* from the virtual-machine world
  (Venus / virglrenderer) via Rust FFI, behind a clean Rust trait boundary, rather than
  reinvented — it already exists and is hardened against our exact threat model (an
  untrusted party driving the host GPU). "All Rust" therefore means: our code is 100%
  Rust; the borrowed engine is an external dependency like any linked C library. The
  trait boundary must stay clean enough that the engine could later be Rustified or
  swapped without touching the rest.

## How the human works (read this before touching git)

The repository owner develops with **Claude Code in a shell on their Linux laptop — that
laptop is the primary copy.** GitHub is the **remote/backup/publishing** point, not the
working copy. Claude.ai is used for **ideation and for crafting the prompts** that laptop
Claude Code sessions then execute. Any session that is *not* running on the laptop (e.g.
Claude Code on the web, in a cloud container) is the exception, and must behave as a
guest:

- **Never commit to or push `main` from a non-laptop session.** Push finished work to a
  clearly-named side branch and leave merging to the human on the laptop.
- Treat a cloud clone as disposable: anything worth keeping is pushed to its side branch
  before the session ends, and nothing assumes it will still exist tomorrow.
- If work in a non-laptop session could collide with uncommitted work on the laptop, the
  laptop wins — leave a branch to reconcile rather than racing the primary.

## Code conventions

Write code as if a human reviewer — possibly one not deeply versed in
Wayland/Vulkan/GPU-remoting — must painstakingly verify **every line** for correctness.

- **A doc-comment block (`///` or `//!`) on every function**, describing in detail what
  it does, its inputs and outputs, its failure modes, and any domain pitfalls. Same for
  every type, trait, and module.
- **An intent comment on every non-trivial line.** The comment explains the *why* or the
  *domain meaning* ("advance the timeline semaphore so the compositor may composite this
  frame") — **never** a restatement of the syntax ("increment i"). Comments must **add
  value**, not noise. Genuinely trivial lines (a bare `}`, an obvious `use`) get no
  comment.
- **Code and comments must always agree.** A stale or contradicting comment is a bug and
  is fixed in the same edit as the code it describes.
- **Super-clear, super-clean code.** Prefer small, focused functions and files that a
  reader can hold in their head at once. If a file grows large or a function does several
  things, that is a signal to split it.
- Prefer explicit over clever. Name things for what they mean in the problem domain.

## Documentation conventions

- Documentation is **top-notch and readable for people not already familiar** with the
  problem space (this explicitly includes the repository owner).
- Explain the **pitfalls** of the domain, not just the happy path.
- **Never omit information for the sake of brevity.** Be clear *and* complete; if a
  concept needs 300 words to be understood correctly, use them.

## The development diary (keep it, every working turn)

[`docs/DIARY.md`](docs/DIARY.md) is the **story** of Rayland — the reasoning as it actually
unfolded, including the wrong turns. It is deliberately **not** a commit log (git has that) and
**not** a status report (the design docs and this file carry the current truth). Its purpose is
twofold and both halves matter: it is a **map for whoever tries the idea again** if Rayland fails
(a dead end that is *understood* is worth more than a green test whose reason nobody wrote down),
and it is **trust-material** for software written by an AI under human supervision — trust that
cannot be asserted, only earned by showing the work honestly, mistakes included.

The binding rule: **on every working turn, add an entry** to the `## Entries` section of
`docs/DIARY.md`, dated, in the project's own voice. An entry records the *thinking* — what we were
unsure of, what we tried, what surprised us, what we now believe and how confident we are — not the
diff. A turn that is purely conversational (a question answered, nothing built or decided) needs no
entry; a turn that plans, builds, debugs, decides, or learns something does. **Tell it straight:**
record uncertainty while it is still uncertain, and when a belief is later overturned, leave the
entry in and give the overturning its own entry — never quietly edit the history. The diary is
allowed to be wrong in places; it may never be dishonest about it. Read `docs/DIARY.md`'s own
preface and "How this diary continues" for the full spirit before writing in it.

## The project map (keep it current, every turn)

The repository root holds an **interactive project map**: [`project-map.js`](project-map.js) (the data —
`window.PROJECT_MAP`: nodes, layers, statuses, dependency edges, and the roadmap) and
[`project-map.html`](project-map.html) (a project-agnostic renderer that reads that data and draws a
layered dependency graph, opened directly from disk via `file://`). It exists so a human — the owner
included — can see at a glance what is shipped, what is in flight, what is an open seam, and what is
planned, and drill into any crate or capability for its sub-parts, files, and specs.

**The binding rule, analogous to the diary's:** on **every working turn**, check the map against the
work you just did, and if anything it depicts has changed — a node's status, a new sub-part landing, a
new crate, a dependency, a roadmap item advancing — **update `project-map.js` in the same turn** and bump
`project.updated` to that day's date. Never invent status: derive it from this file's roadmap, the SDD
ledger, the diary, and what actually exists in the tree. The renderer is project-agnostic and normally
needs no edits; the *data* is what tracks reality. A turn that changed no status leaves the map alone (do
not churn the `updated` date for nothing) — but you must have *looked*. Every new session learns of the
map's existence and this rule by reading this file.

## The project overview (the handover surface)

[`docs/OVERVIEW.md`](docs/OVERVIEW.md) is a **single self-contained account of the whole project**,
written for a reader doing planning and design **without the repository open**. It exists because of
how this project is now directed: **planning and design happen in Claude.ai**, and sessions in this
repository receive prompts and report back. Claude.ai cannot see the tree; it sees this document and
a source snapshot (`~/bin/snapshot.sh`, which packs any repo's source and docs — run it from the repo
root).

It is not a duplicate of this file. `CLAUDE.md` is binding conventions; `docs/DIARY.md` is 4,000+
lines of chronology; `OVERVIEW.md` is the *shape* — mechanism, status, measured numbers, what is
open, and, most importantly, **the discoveries that a plan must not re-propose**, stated as
discoveries rather than left implicit in the code. Several of this project's central facts overturned
earlier beliefs; a planner without them will confidently re-specify a dead end.

**The binding rule:** when a turn changes something `OVERVIEW.md` asserts — a phase completing, a
measured number, a belief overturned, a new open question — **update it in the same turn** and bump
its "Last brought current" line. It carries the same honesty obligation as the diary: a retired
design stays visible as retired, rather than vanishing.

## Repository status and layout

A Cargo workspace of eighteen crates. Each declares its own license per the policy below
(library → LGPL, application/binary → GPL); all are `v0.0.x` and pre-stable.

- **`crates/rayland`** — the published placeholder that reserves the crates.io name; the
  future facade. GPL.
- **`crates/rayland-wire`** — the SP0-era hand-rolled command messages and their framing
  (`postcard`). LGPL.
- **`crates/rayland-client`** — SP0-era C side: hand-builds the triangle command stream
  and sends it. GPL.
- **`crates/rayland-server`** — SP0-era S side: replays that stream on a real GPU and
  presents it (PNG / `wl_shm` window / zero-copy dmabuf). GPL.
- **`crates/rayland-transport`** — QUIC transport: synchronous stream adapters over a
  `quinn` connection (SP2). LGPL.
- **`crates/rayland-vtest`** — the **vtest** wire protocol Mesa's Venus ICD speaks, the
  `RenderEngine` / `VtestTransport` traits, `EngineError`, and `venus_ring/` — the
  repository's knowledge of Mesa's command ring. **Has no GPU dependencies, by
  construction.** It links `libc`, `thiserror`, and `rayland-venus-proto` — the last of
  which compiles Mesa's *generated protocol headers*, which are format definitions with no
  driver, no device and no `libvirglrenderer` behind them. Rayland's **C** side speaks this
  protocol but must never link a GPU stack (C is the weak, possibly headless, possibly
  RISC-V machine), and `rayland-c`'s `tests/no_gpu_linkage.rs` asserts `rayland-engine` is
  absent from its dependency tree. **The dependency arrow points `rayland-engine` →
  `rayland-vtest`, and must never be reversed.** LGPL, `publish = false` (its `rayland-venus-proto`
  path dependency is itself unpublished; see that crate's `Cargo.toml` for the reasoning).
- **`crates/rayland-venus-proto`** — **framing for Venus command streams**: how long is the command
  at the start of these bytes? It vendors Mesa's *generated* `venus-protocol` headers and compiles
  them against a replacement `vkr_cs.h` this crate writes itself, so the borrowed decoders run with
  no virglrenderer and no Mesa util library behind them. Byte consumption does not depend on object
  lookups, so stub lookups give framing identical to a live renderer's. **It may never decide what gets relayed, when, or
  which blobs a delta reads** ((c)1 spec §7: the ring is relayed as opaque bytes precisely so a
  decoding bug cannot become a corruption bug), enforced for the relay path by
  `rayland-c/tests/decoder_is_not_load_bearing.rs`. That constraint was once broader — "never a
  correctness decision" — and **one deliberate exception was taken on 2026-07-26, off the relay
  path**: `find_destroy_device` now validates a signature match against a decoded command boundary,
  and `rayland-s` uses its answer to retire the readback gate. The bare scan was measured
  false-positiving on payload bytes and retiring that gate on a device destruction that never
  happened. The `rayland-c` guard is deliberately **not** widened to cover `rayland-s`, so the
  discrepancy stays visible instead of being papered over; see `docs/DIARY.md`, 2026-07-26. LGPL, `publish = false`.
- **`crates/rayland-relay`** — the **(c)1 relay wire protocol**: the `C2S`/`S2C` messages that
  cross the network between C and S (ring deltas, blob syncs, replies) and their `postcard`
  framing. Pure data — no GPU, no sockets, no async runtime — because both `rayland-c` and the
  future `rayland-s` depend on it and C must never link a GPU stack. It also carries one diagnostic
  module, `trace` (the (c)1 Task 9 stage tracer): env-gated stderr timestamps on a shared
  `CLOCK_MONOTONIC`, used by **both** daemons to record the return path's stages against one clock —
  none of a GPU stack, network I/O, or a socket, so the purity above holds. LGPL.
- **`crates/rayland-c`** — **C's daemon ((c)1).** A local vtest server that a stock, unmodified
  Mesa Venus ICD connects to: it hands the application plain local memfds for its ring and blobs,
  **watches the ring** (where 100% of the application's Vulkan commands actually live), and relays
  the bytes to S. The insight it rests on is that the vtest protocol's "host" is whoever allocates
  the ring, and Rayland can be that host — so no Mesa fork and no patch is needed.
  **Since 2026-07-26 it also relays Venus's out-of-line command streams.** Venus replaces any submission
  over `direct_size` (`buffer_size >> 4`, so 8 KiB for the 128 KiB ring) with `vkExecuteCommandStreamsMESA`
  naming *other* shmems, which live in the staging pool (`blob_id == 0`). The C→S sync used to skip those,
  so S held zeros and `rayland-c` refused the delta — correct, but it made every submission over 8 KiB
  unrelayable, which is most real workloads. It now synchronises **every blob except the ring** (the ring
  keeps `RingDelta`, which carries the `tail` that validates its bytes and is sent last). Publishing a
  region S *also* writes is safe because of the **baseline**, not the `blob_id`: `note_s_wrote` folds each
  S→C write into it, so S's replies are never echoed back. The alternative — making the old refusal precise
  with the decoder — was rejected because this is the relay path and (c)1 §7 forbids a decode from deciding
  what crosses the wire; carrying the stream removes the question instead of answering it. Its
  `tests/no_gpu_linkage.rs` guards the **binary**, which covers `rayland-vtest`, `rayland-relay`
  and everything they pull in transitively. GPL, `publish = false`.
- **`crates/rayland-s`** — **S's daemon ((c)1).** The other end of `rayland-c`: it applies the
  relayed messages to a real `libvirglrenderer`. The thing to know about it is that it does **not**
  "receive commands and execute them" — a relayed ring delta is *written into the ring blob's
  memory*, because that is where virglrenderer's own ring thread polls for it
  (`vkr_ring.c:33-58` points the ring at the blob's pages; `vkr_ring.c:262-266` loops on them).
  `RenderEngine::submit` is used only for the inline vtest path, which carries the
  `vkCreateRingMESA` that creates the ring and essentially nothing else. Unlike `rayland-c`, this
  crate **may** depend on `rayland-engine`: it is the GPU machine. GPL, `publish = false`.
- **`crates/rayland-present`** — **on-screen presentation ((c)1 Task 7), extracted from
  `rayland-server`'s `window.rs`/`dmabuf.rs`.** Takes finished pixels and shows them in a real
  `xdg_toplevel` window, via `wl_shm` or zero-copy `zwp_linux_dmabuf_v1`. Shared by both the SP-era
  `rayland-server` and `rayland-s`, so it lives in its own crate rather than being duplicated.
  **Note (c)1 uses only the `wl_shm` path** and is deliberately *not* zero-copy: S presents the
  application's readback blob, because it cannot see the app's `DEVICE_LOCAL` render target (that
  produces no blob at all). **Since 2026-07-26 it can also follow a live render:** `present_live()` takes
  an optional boxed `'static` closure supplying subsequent frames, and the window re-arms a
  `wl_surface.frame` callback on every commit. With `None` it is `present()` exactly — no callbacks
  requested, no behaviour change for `rayland-server`. Pacing is the **compositor's**, not the relay's:
  the window shows whichever frame S last completed, so a slower remote render repeats frames. That keeps
  presentation from ever blocking the relay, at the cost of not being frame-accurate. LGPL.
- **`crates/rayland-engine`** — **the real engine (arc (c)).** FFI-embeds
  `libvirglrenderer` behind `rayland-vtest`'s `RenderEngine` trait, driving a Venus
  context on S's GPU. Since (c)1 Task 1 this crate is *only* the GPU: the `ffi`
  declarations and the `VirglEngine` that drives them. It re-exports `rayland-vtest`'s
  types, so its public paths are unchanged. LGPL.
- **`crates/rayland-refapp`** — C0's captured workload: an **ordinary** offscreen Vulkan
  triangle program with **zero `rayland-*` dependencies** and no knowledge of remoting.
  Its value is that it is boring and typical; keep it that way. GPL, `publish = false`.
- **`crates/rayland-icosa-core`** — shared foundations for the icosahedron fixtures: the geometry,
  the frame-indexed animation schedule, the Mandelbrot math, and the bit-exact `log2`/`sin`/`cos`
  those rest on. **No dependencies at all, and never touches a GPU** — its correctness is
  arithmetic. Its reason for existing is that the two fixtures must be identical in everything but
  the property under study, and two copies of this code would drift. LGPL, `publish = false`.
- **`crates/rayland-icosa-vk`** — the Vulkan scaffolding both icosahedron fixtures share: bring-up,
  the depth-tested render pass and pipeline, the targets, the persistent host mapping, and the
  readback. It exists so the two fixtures **cannot** drift in the parts that must be identical for
  their comparison to mean anything — the same argument `rayland-icosa-core` rests on, applied to
  the render loop. Knows nothing about remoting. LGPL, `publish = false`.
- **`crates/rayland-icosa-cpu`** — fixture A: an ordinary offscreen Vulkan program drawing a
  spinning icosahedron textured with a fractal it computes on **its own CPU** and writes into
  persistently-mapped `HOST_COHERENT` memory every frame — with no flush, and so no call on the wire
  saying a megabyte changed. That is both what an ordinary Vulkan program does and exactly the case
  with nothing to intercept, which is the problem this fixture states in executable form. Depends
  only on the two icosa libraries and knows nothing about remoting. GPL, `publish = false`.
- **`crates/rayland-icosa-gpu`** — fixture B: the same spinning icosahedron, same geometry, same
  schedule, same fractal arithmetic, and — via `rayland-icosa-vk` — literally the same render loop.
  Only the fractal moves: it is evaluated in a fragment shader, so 80 bytes per frame cross
  mapped memory instead of a megabyte. It is the **volume control** for `rayland-icosa-cpu`, not an
  alternative to it: it still writes its uniforms through a persistent mapping with no interceptable
  call, so the pair isolates how cost scales with mapped-write volume, not the presence of mapped
  writes. GPL, `publish = false`.
- **`crates/rayland-icosa-window`** — **a demo, not a fixture, and must never be mistaken for one.**
  Opens a live Wayland window and shows the icosa solid actually spinning, for a human to look at —
  no PNGs, no CSV, nothing reproducible, and therefore unusable by (c)1's netem sweep. Because it is
  not evidence about anything, it is exempt from every rule the fixtures are bound by: it **may**
  depend on `rayland-*` crates (the fixtures may not) and it **has** a redraw loop paced by the
  compositor (the fixtures forbid any such loop, since it would destroy their bit-identical
  native-vs-remoted comparison). In practice it speaks Wayland directly, via `smithay-client-toolkit`
  — not through `rayland-present`, whose one-static-frame-per-call shape (built for `rayland-s`) does
  not fit an animated demo; it owns one persistent `xdg_toplevel` for its whole run and redraws it on
  every `wl_surface::frame` callback, rather than opening a new window per animation frame. See its
  crate docs for the full contrast and that history, cross-referencing `docs/icosa-fixtures.md` and
  the design spec's §2. GPL, `publish = false`.

The work is decomposed into sub-projects, each getting its own design spec →
implementation plan → build cycle, sequenced as a "walking skeleton" (get something
rendering end-to-end first, then harden).

**Arc (s) — SP0–SP3 built Rayland's own hand-rolled `postcard` protocol end to end. All
complete and merged.** Their code is untouched and their tests still pass; it coexists
with arc (c) until arc (c) fully supersedes it.

- **SP0 — First light** *(complete)*: trivial Vulkan triangle on C → serialized commands
  over plain TCP/localhost → replay on S's real GPU → write a PNG. Proves the core loop.
- **SP1 — Onto the screen** *(complete)*: replace PNG-dump with a live Wayland window on S.
- **SP2 — Real transport** *(complete)*: TCP → QUIC.
- **SP3 — Zero-copy presentation** *(complete)*: dmabuf export to the compositor, with a
  `wl_shm` fallback.
- **SP4 — Adaptive L3 + session/security:** RTT-adaptive policy, SSH-bootstrap, sandboxing.
- **SP5 — Proxy completeness:** full Sommelier/waypipe-grade Wayland coverage.
- **Audio:** a later, separate track (transport reservations already made in the design).

**Arc (c) — the real-engine pivot: replace that hand-rolled protocol with the reused
Venus/virglrenderer capture/replay engine, so *unmodified* applications run.**

- **C0 — Venus First Light** *(complete)*: a real, unmodified
  Vulkan app, captured by Mesa's Venus ICD, replayed on S's real GPU through our
  virglrenderer-embedding host — PNG bit-identical to native. Same machine, local socket,
  offscreen. See [`docs/c0-venus-first-light.md`](docs/c0-venus-first-light.md).
- **(c)1 — the network.** **Rescoped by C0's findings.** *Not* "swap the socket for QUIC":
  C0 proved the vtest socket carries **0% of the application's commands** — they cross via
  **shared memory** whose fd is passed over `SCM_RIGHTS`, and **neither a shared page nor an
  fd survives a network**. (c)1 is a protocol design task. It also owes SP1 host-side pixels
  for on-screen presentation. **Required reading:**
  [`docs/design/2026-07-15-venus-ring-findings.md`](docs/design/2026-07-15-venus-ring-findings.md).
  **Delivered:** the **forward path** (unmodified app commands C→S, executed on S's GPU,
  bit-identical on trivial workloads) and **presentation** both work. **Handed to (c)2:** the
  **readback return path** — see the next bullet. Task 9 measured it (message-rate-bound, mapped
  memory shipped 5.2×/frame) and found it silently delivers stale/torn frames; five fixes across
  Task 9 failed, and the final one proved *why* — see
  [`docs/c1-the-network.md`](docs/c1-the-network.md) §3.1 and
  [`docs/design/2026-07-17-fence-feedback-walking-skeleton.md`](docs/design/2026-07-17-fence-feedback-walking-skeleton.md)
  §11.
- **(c)2 — mapped-memory coherence:** the `vkMapMemory` problem (apps write vertices and
  textures straight into mapped memory with **no API call to intercept**). The icosahedron
  fixtures (`rayland-icosa-cpu`/`rayland-icosa-gpu`) were built to make this bite and, run
  through C0's path, did not — see [`docs/icosa-fixtures.md`](docs/icosa-fixtures.md) for
  why not and where the real failure is still waiting. **Now also owns the readback return
  path handed over by (c)1**: an application that maps a **GPU-written** buffer and reads it
  back cannot be served by S *passively observing and diffing* that memory — S is a foreign
  reader with no fence→coherency relationship, and every patch of the observe-and-diff path hit
  a different wall (proof:
  [`docs/design/2026-07-17-fence-feedback-walking-skeleton.md`](docs/design/2026-07-17-fence-feedback-walking-skeleton.md)
  §9–§11). Two candidate fixes were then **investigated and both retired**:
  (a) the fenced engine-side read (`virgl_renderer_transfer_read_iov`) is a **hardcoded stub** for the
  Venus/render-server path in virglrenderer 1.2.0 **and 1.3.0**, with no engine-level coherence API at
  all; (b) `DMA_BUF_IOCTL_SYNC` on the readback dma-buf is a **measured no-op** (byte-identical to the
  raw read in 6561/6561 samples) — the memory is already CPU-coherent, so **the tearing is not a
  cache-coherence problem**. The robust conclusion: correctness needs the host GPU work **retired
  through an engine call** (a context fence/poll — no lock-free substitute exists), and *that* call
  takes Rayland's single global engine lock, which is what contends with the message-thread doorbell
  (ring-stall `SIGABRT` / timeout, Phase 1). That fence-vs-doorbell contention was the architecture
  problem, and it is **solved**: the **engine actor** (`crates/rayland-engine/src/actor.rs`, committed
  and smoke-tested) makes one thread own virglrenderer while an `EngineClient` implements `RenderEngine`
  by messaging it, so the fence and the doorbell cooperate on one thread instead of deadlocking — **the
  refapp e2e passes through the actor with no wedge.** The actor is now **wired in**, and the true `T4`
  barrier is **delivered**: the daemon issues `virgl_renderer_context_create_fence` on the application's
  **real per-queue `ring_idx`** — decoded from its `vkGetDeviceQueue2` on the ring — which does a
  genuine `vkQueueSubmit`+`vkWaitForFences` on the app's own queue (`ring_idx = 0`, the old hardcode,
  fenced no GPU work). The fence is gated to fire only when it is a safe, real barrier: not before the
  queue is registered on the host (a premature fence is render-server-fatal), not after the app's
  `vkDestroyDevice` frees it (a late fence is fatal too), and not before the app's own `vkQueueSubmit`
  has crossed the ring and been dispatched (an early fence overtakes it and ships a torn readback) —
  with submit positions tracked **free-running** so the gate survives the ring wrapping mid-run. The
  `icosa_cpu` fixture now renders **bit-identical across the (c)1 loopback relay** (`loopback_e2e
  icosa_cpu_renders`, 0/120 frames differing across consecutive runs). This proves the **readback
  return path**, not the mapped-memory forward path: on loopback the fixture's uninterceptable mapped
  writes still reach S, so that break (a true network, where they cannot) is not yet exercised. Full
  trail:
  [`docs/design/2026-07-19-c2-ringidx-decode.md`](docs/design/2026-07-19-c2-ringidx-decode.md),
  [`docs/design/2026-07-18-c2-engine-actor.md`](docs/design/2026-07-18-c2-engine-actor.md) §8–§9, and
  [`docs/design/2026-07-18-c2-readback-reachability.md`](docs/design/2026-07-18-c2-readback-reachability.md).
  A **two-machine (real-network) run** confirmed the readback barrier holds off loopback: `rayland-refapp`
  is bit-identical apollo→dop561 (and presents on dop561's screen), and `rayland-icosa-cpu` delivers
  faithfully whatever S rendered, with no wedge/`SIGABRT`/`invalid ring_idx`.
  **The open (c)2 problem is now *located precisely*, and it is the opposite of what a first spike
  suggested:** over the true network ~2/120 icosa frames come back as the *whole previous frame*, but the
  cause is a **readback-completion lag on S**, not a forward mapped-blob relay race — the forward relay is
  ordered and verified fresh. A per-delivery correlation on S (env-gated `RAYLAND_C1_FPLOG`, throwaway
  instrumentation) fingerprinted both the delivered readback image **and** an independent forward-input
  witness: the resident per-frame **uniform** the draw reads directly. Across two independent stale runs,
  every stale frame showed `uniform = N (fresh), delivered image = N−1 (stale)`, and frame N's image was
  **never delivered at all** — S already held frame N's forward inputs when it shipped frame N−1's pixels.
  The earlier spike, which dumped *only* S's readback, misread "S delivered N−1 repeatedly" as "S rendered
  against stale forward inputs (forward lag)"; the uniform witness shows S's forward inputs were correct
  and the **readback delivery** lagged. The defect is in S's own (c)2 completion barrier interacting with
  the fixture's **two submits per frame**: the trigger ships `res6` (the readback blob) without
  guaranteeing its *content* corresponds to the newest submitted draw, so under real-network timing it
  ships the previous frame's pixels for the current frame's submits and drops the current frame's readback.
  Loopback hides it (0/120). See
  [`docs/design/2026-07-19-c2-true-remote-mapped-sync.md`](docs/design/2026-07-19-c2-true-remote-mapped-sync.md).
  **Landed (2026-07-19), and it sharply reduces but does not eliminate the defect:** the `rayland-s`
  **readback-completion gate** (then in `crates/rayland-s/src/delivery.rs`, wired into
  `progress_thread` — the G' fix below removed that module, and the surviving return-path logic lives
  directly in `progress_thread` in `crates/rayland-s/src/main.rs` plus
  `Applier::reply_arena_fence_signaled` in `crates/rayland-s/src/apply.rs`)
  completes a delivery only once `take_app_blob_writes` shows the readback blob actually advanced past
  the last delivered frame (or a 250 ms identical-frame bound expires), so a two-submits-per-frame app's
  copy submit can no longer ship the previous frame's pixels. Over the real network
  (`scripts/c2-icosa-two-machine.sh`) this took 11 runs from *most runs losing 1–4 frames* to **10/11
  runs fully clean**. Design + plan:
  [`docs/design/2026-07-19-c2-readback-completion-gate.md`](docs/design/2026-07-19-c2-readback-completion-gate.md).
  **That ~1/11 `N == N−1` residual is now FIXED — 0 stale across 20 real-network runs** (2026-07-21).
  The fix (`docs/design/2026-07-21-c2-getfencestatus-completion.md`) is the **G'** approach, reached
  after three recorded dead ends: the empty-submit context fence retires before the readback DMA
  (`T2 < T4`, pervasive — `docs/design/2026-07-20-c2-fence-empty-submit-finding.md`); a "wait-drain"
  design rested on a false premise (with feedback off Mesa does **not** send `vkWaitForFences`, it
  **polls `vkGetFenceStatus`** — the spike gate caught it before it was built); and a fingerprint-gated
  res6-first ordering ("G-lite") killed the `N−1` staleness but tore, having no completion barrier. **The
  signal that worked:** with feedback off the application releases itself by polling `vkGetFenceStatus`
  until the reply reads `VK_SUCCESS`; virglrenderer writes that reply into the reply arena as `[38][0]`,
  which means the app's submit *and its readback copy* are complete on S's GPU. `Applier::reply_arena_fence_signaled`
  scans the **live** arena for it (the shipped diff fragments the reply into per-changed-byte runs, so the
  contiguous pattern is invisible there); it is safe against a lingering prior success because the app
  polls `VK_NOT_READY` (`[38][1]`) *during* a copy's DMA, so a live `[38][0]` means a fence just signalled.
  `progress_thread` then ships the readback (gated on `take_app_blob_writes` non-empty — a draw's fresh,
  complete `res6`) **before** the reply arena and the head-advance that release the app. No S-issued fence,
  no timing heuristic; the progress thread no longer touches the engine. **Scope:** feedback-OFF only (the
  only config that renders over a real network; the feedback-on "buy-back" was loopback-only and is
  superseded — the loopback icosa e2e now runs feedback-off to guard the shipping path). The readback's
  fragmentation into ~5000 one-byte `BlobData`/frame is since **fixed by gap-threshold coalescing**
  (readback path only, gap ≤ 256 — safe there because `res6` is S-written and C-read-only, so re-shipped
  gap bytes are idempotent): ~5000 → ~180 messages/frame, still bit-identical, still 0 stale
  ([`docs/design/2026-07-21-c2-readback-coalescing.md`](docs/design/2026-07-21-c2-readback-coalescing.md)).
  Wall-clock did **not** move, which located the return path's real bound: per-frame **round-trip
  latency** (the app's `vkGetFenceStatus` polling), not one-directional readback volume. **Still open:**
  that round-trip count (adaptive polling / reply batching, when latency matters), and multi-queue
  support.
  **Feedback, per mechanism, measured 2026-07-26/27 (this supersedes "feedback-OFF only" as a bare assertion):**
  2026-07-26 established *why*, per mechanism, by measurement. **`no_fence_feedback` is load-bearing:** the
  barrier above works by spotting the app's `vkGetFenceStatus` reply reading `VK_SUCCESS`, and fence feedback
  removes that poll — enabling it gives exit 134 and zero frames, immediately and every time. **Semaphore, event and query
  feedback are worth 1.23× and their one observed failure is UNATTRIBUTED:** enabling them measured **1.23×**
  on `icosa-gpu` over loopback (median `draw_readback` 48.7 ms → 39.5 ms, all 120 frames bit-identical), and a
  two-machine sweep lost one run of ten to a silent Venus `SIGABRT`. That looked decisive and is not. Hunted
  since: **82 further clean runs with the flags on** — 8 loopback and 14 real-network under `gdb`, then 60
  real-network unattended with genuine core capture armed on C (`core_pattern` pointed at a file, `ulimit -c
  unlimited`), all 120 frames, no core produced. So **1 failure in 92 feedback-on runs (~1%) against 0 in 20
  feedback-off runs** — which is *not* a significant difference, and the failure cannot now be pinned on
  feedback at all. It may have been unrelated. The flags remain off because an unexplained total-session loss
  is unexplained either way, but the reason is "we do not know what that was", not "feedback breaks it". **The mechanism is NOT known**, and one plausible-sounding
  explanation was checked and refuted the same evening: "(c)1 does not relay the feedback pages" (a comment in
  `scripts/c1-two-machine.sh`) is not supported. `emit_blob_writes` excludes only **rings**, and
  `take_bytes_s_wrote` detects change by diffing a shadow — so it catches writes virglrenderer's GPU makes
  directly, not just relayed `copy_in`s. Measured with the three feedbacks on: S ships back `res=2` and `res=5`
  and nothing else, with traffic within 0.1% of the feedback-off run. There is **no un-relayed feedback page**
  in this workload. Whatever makes the app abort, it is not that. **A loopback pass proves nothing about
  feedback** — 120/120 bit-identical frames and 1.23× preceded the failure. **Still open:** multi-queue support, and the synchronous round trip itself, which is now
  the *measured* explanation for frame time (see below), not a suspicion.
**Where (c)1/(c)2 stand after 2026-07-26, measured.** An unmodified application on C, rendered by S's
GPU and **displayed live on S's screen**, works end to end: `scripts/icosa-remote-demo.sh` runs
`rayland-icosa-gpu` or `-cpu` on apollo and animates it in a window on dop561. Three findings decide where
effort goes next:

- **The `vkQueueSubmit` "CS error" was never a Rayland bug.** It is `VK_ERROR_DEVICE_LOST` (`VkResult=-4`)
  from the real submit on S's GPU; Venus reports device loss by setting the decoder's fatal flag through a
  branch that runs only when `flags == 0x0`, so it surfaced as the generic `%s resulted in CS error` with no
  log of its own. Proved by building a patched `virgl_render_server` (source from the Ubuntu archive pool
  over plain HTTP — no root, no `deb-src`; `meson`/`ninja`/`pyyaml` in a venv; the distro's own
  `fix-c23.patch`) and spawning it via `RENDER_SERVER_EXEC_PATH`, which the *system* library honours. **It is
  GPU-specific:** NVIDIA RTX A500 7/14 runs lost, Intel Iris Xe **0/10** and reaching strictly further
  (8 submits vs 5–6). vkcube defaults to the discrete GPU; `--gpu_number 0` avoids it.
- **Frame time is the synchronous round trip, and that is now measured rather than suspected.** On loopback,
  `icosa-gpu` costs ~50 ms/frame with a 78 KB return path — not bandwidth, not message count, not flush
  syscalls (batching `ship()`'s per-message lock and flush is worth **1.03×**). With feedback off the app
  implements `vkWaitForFences` by polling `vkGetFenceStatus`, and every poll is a full C→S→execute→reply→C
  cycle. **The readback is not fragmented** — `res=5` averages 377-byte runs; the one-byte flood is the reply
  arena, whose gap-0 grain is a deliberate correctness property (a gap byte is one S did not write).
- **Where the fixtures put the cost.** `icosa-cpu` pushes ~1 MiB/frame of CPU-computed fractal through
  uninterceptable mapped memory; `icosa-gpu` does the same picture in ~80 bytes by evaluating it in a
  fragment shader. Measured over the real network: **283 ms/frame against ~41 ms**, i.e. the mapped-write
  volume *was* the dominant cost. In a command-streaming design resolution is separately cheap — it is the
  GPU's problem and the GPU is next to the display — but costs bandwidth *today* only because S presents the
  application's readback buffer, which is what WP0's token → `wl_buffer` path exists to end.

- **WP0 — Wayland proxy first light** *(IN PROGRESS — the active front)*: today S presents the
  application's **readback buffer** — pixels the GPU wrote, copied back to memory, shipped, and
  re-uploaded as `wl_shm`. That is a bandwidth tax and the reason resolution costs anything at all.
  WP0 ends it by proxying the application's **own Wayland protocol**: the app connects to a proxy on
  C rather than to a compositor, its `wl_surface`/`xdg_toplevel` requests are relayed to S's real
  compositor, and the one thing that cannot cross a network — the swapchain `wl_buffer`'s fd — is
  replaced by a **`BufferToken`** naming the S-side resource the command relay already rendered. **No
  pixels cross the network** — a claim that was *false in practice* until 2026-08-29, when it was finally
  measured: the (c)2 return path was shipping every presented frame back to C at ~877 KB/frame, because it
  cannot tell a swapchain image from a readback. Presented resources are now excluded and S→C fell 571×, to
  ~1.9 KB/frame. The display was always zero-copy, so the waste had no symptom and every test passed. Presentation is **zero-copy dma-buf**: Venus requests the swapchain
  image as `HOST3D` with `VkExportMemoryAllocateInfo{DMA_BUF}` unconditionally, so the S-side
  resource really does export a compositor-importable dma-buf (measured: `fd_type=1`). Note Task 4.0
  first concluded the opposite and was **overturned by Task 4.0-bis**; both are left in the plan.
  **State (2026-08-29): 4.3 and 4.5 DONE — an unmodified vkcube on C spins in its own window on S's screen, confirmed by a human watching it, with pixels no longer crossing the wire.** 4.1 (C-side wiring) and 4.2 (S router, replay, object-id map) done; **4.4 (the event
  return path) genuinely works** — vkcube receives both `configure` events through the tunnel and
  acks them; **4.3 is the open piece** — C's half is complete, S part 1 (retaining each blob's
  exported dma-buf descriptor, which `mem->exported` permits exactly once, at creation) is landed,
  and **S part 2 (token → `wl_buffer`) is specified but deliberately not written**, because it
  cannot be verified without a compositor and a GPU and "code that compiles while exercising no test
  would look done". Two decisions it needs first: **`stride` must go on `BufferToken`** (deriving
  `width × bpp` garbles pixels rather than failing cleanly), and **S must *synthesize* the
  `params.add`** rather than replay it, since C drops the fd by design — a request S originates, a
  first for the replay module. Resolve and clone the fd **under the applier lock and release it
  before any `send_request`**, or the relay's mutex ends up behind a compositor round trip. Then
  4.5: vkcube's cube on S's screen. See
  [`docs/design/2026-07-22-wp0-wayland-proxy-first-light.md`](docs/design/2026-07-22-wp0-wayland-proxy-first-light.md)
  and its `-plan` companion.
- **(c)3 — content-addressed assets.**
- **(c)4 — real/complex applications; GL via Zink.**

## License

Rayland is an application: **GPL-3.0-or-later**. Library crates that emerge from the
project may be **LGPL-3.0-or-later**; each crate declares its own license in its manifest.
