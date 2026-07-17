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

## Repository status and layout

A Cargo workspace of seventeen crates. Each declares its own license per the policy below
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
  construction:** only `libc` and `thiserror`. Rayland's **C** side speaks this protocol
  but must never link a GPU stack (C is the weak, possibly headless, possibly RISC-V
  machine), so `tests/no_gpu_linkage.rs` asserts `rayland-engine` is absent from this
  crate's dependency tree. **The dependency arrow points `rayland-engine` →
  `rayland-vtest`, and must never be reversed.** LGPL.
- **`crates/rayland-relay`** — the **(c)1 relay wire protocol**: the `C2S`/`S2C` messages that
  cross the network between C and S (ring deltas, blob syncs, replies) and their `postcard`
  framing. Pure data — no GPU, no sockets, no async runtime — because both `rayland-c` and the
  future `rayland-s` depend on it and C must never link a GPU stack. LGPL.
- **`crates/rayland-c`** — **C's daemon ((c)1).** A local vtest server that a stock, unmodified
  Mesa Venus ICD connects to: it hands the application plain local memfds for its ring and blobs,
  **watches the ring** (where 100% of the application's Vulkan commands actually live), and relays
  the bytes to S. The insight it rests on is that the vtest protocol's "host" is whoever allocates
  the ring, and Rayland can be that host — so no Mesa fork and no patch is needed. Its
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
  produces no blob at all). LGPL.
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
  depend on `rayland-present` (the fixtures may not), and it **has** a wall-clock frame loop (the
  fixtures forbid one, since it would destroy their bit-identical native-vs-remoted comparison). See
  its crate docs for the full contrast, cross-referencing `docs/icosa-fixtures.md` and the design
  spec's §2. GPL, `publish = false`.

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

- **C0 — Venus First Light** *(in progress; substance complete)*: a real, unmodified
  Vulkan app, captured by Mesa's Venus ICD, replayed on S's real GPU through our
  virglrenderer-embedding host — PNG bit-identical to native. Same machine, local socket,
  offscreen. See [`docs/c0-venus-first-light.md`](docs/c0-venus-first-light.md).
- **(c)1 — the network.** **Rescoped by C0's findings.** *Not* "swap the socket for QUIC":
  C0 proved the vtest socket carries **0% of the application's commands** — they cross via
  **shared memory** whose fd is passed over `SCM_RIGHTS`, and **neither a shared page nor an
  fd survives a network**. (c)1 is a protocol design task. It also owes SP1 host-side pixels
  for on-screen presentation. **Required reading:**
  [`docs/design/2026-07-15-venus-ring-findings.md`](docs/design/2026-07-15-venus-ring-findings.md).
- **(c)2 — mapped-memory coherence:** the `vkMapMemory` problem (apps write vertices and
  textures straight into mapped memory with **no API call to intercept**). The icosahedron
  fixtures (`rayland-icosa-cpu`/`rayland-icosa-gpu`) were built to make this bite and, run
  through C0's path, did not — see [`docs/icosa-fixtures.md`](docs/icosa-fixtures.md) for
  why not and where the real failure is still waiting.
- **(c)3 — content-addressed assets.**
- **(c)4 — real/complex applications; GL via Zink.**

## License

Rayland is an application: **GPL-3.0-or-later**. Library crates that emerge from the
project may be **LGPL-3.0-or-later**; each crate declares its own license in its manifest.
