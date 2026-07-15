# C0 — Venus First Light (the real-engine pivot)

**Date:** 2026-07-14
**Status:** Sub-project design spec (awaiting owner review)
**Parent design:** [`2026-07-13-native-remote-wayland-gpu.md`](2026-07-13-native-remote-wayland-gpu.md)
**Predecessors:** SP0–SP3 (all complete, merged). This begins **arc (c)** — replacing the
hand-emitted toy protocol with a real capture/replay engine so *unmodified* applications run.
**Grounded in:** the (c)0 feasibility spike (2026-07-14), findings recorded in project memory.

---

## Amendments — where reality overruled this plan (2026-07-15)

> **This block was added after C0 was built. The sections below it are the plan as written on
> 2026-07-14 and are deliberately left intact**, because a reader must be able to see *what was
> planned*, *what actually happened*, and *why they differ*. Each superseded claim is annotated in
> place with a pointer back here. Nothing below has been silently rewritten.
>
> The full evidence for every correction is in
> [`2026-07-15-venus-ring-findings.md`](2026-07-15-venus-ring-findings.md); how to run what was
> actually built is in [`../c0-venus-first-light.md`](../c0-venus-first-light.md).

### Amendment 1 — the **app** writes the PNG, not the host (supersedes §1, §2, §3, §7)

**The plan says C0's host "extracts the rendered image, and writes a PNG". That did not happen, and
the evidence says it could not.** Owner decision, taken during Task 4b.

Why the planned host-side extraction was not merely inconvenient but impossible:

- The app's `VkImage` is created by **Venus commands inside the shared-memory ring**. It never
  enters our engine's resource table, so `RenderEngine::read_back` could not see it in principle.
- Task 4b confirmed it independently from the other direction: a **`DEVICE_LOCAL` image produces no
  blob at all.** The host-side readback the plan describes had **nothing to read**.
- The blobs a live client actually creates are its command ring and its staging/reply pools — not
  rendered images (Task 4a). Blob resources also have no queryable pixel format (Task 3), so "read
  back the last blob" is not a shortcut to a frame either.

**What was built instead:** the reference app performs its own `vkMapMemory` readback and writes its
own PNG — which is what any ordinary offscreen Vulkan program does, and therefore *strengthens* the
"unmodified real app" claim rather than weakening it. That readback creates its own shared blob
(`res=6`, 16384 B = 64×64×4, caught holding the clear colour): **the pixel return path.**

**Consequence — do not mistake this for dead code.** `RenderEngine::read_back` and `EngineFrame`
(Task 3) are **off C0's critical path but are not wasted**: SP1 and (c)1 must put a frame **on a
screen**, which requires **host-side pixels**. C0's app-side readback deliberately sidesteps that
question rather than answering it. The host-side frame-extraction spike (Task 4c) was **deferred by
the owner** and remains an **open question that (c)1/SP1 must close**. Lead on record: `blob_id != 0`
cleanly discriminates the app's own memory from Venus's internal plumbing.

### Amendment 2 — the transport seam is not a seam (supersedes §6)

**§6's claim that "(c)1 only swaps the stream source" is disproved.** C0 discovered that the vtest
socket carries **0% of the application's Vulkan commands** — only ring management (one
`vkCreateRingMESA`, then `vkNotifyRingMESA` doorbells). **100% of the commands are written directly
into shared memory**, which the host allocates and the client `mmap`s after we pass it a file
descriptor over `SCM_RIGHTS`.

**Neither the shared page nor the fd survives a network.** QUIC has no fd-passing, and two machines
cannot share a page. **There is no socket carrying commands to swap.** (c)1 is a **protocol design
task**, not a transport substitution, and it is far more entangled with (c)2's coherence work than
this spec's clean decomposition assumed.

The good news, proven twice independently: the ring's bytes are the **same legible Venus command
language** as the inline path, so this is a plumbing problem, not an encoding problem. See the
findings document — it is required reading before (c)1 is designed.

C0 encoded this at the type level rather than leaving a note: `serve_vtest` is generic over a
`VtestTransport` trait whose `send_fd` is a **required** method, so a future QUIC transport
confronts the gap **at compile time**.

### Amendment 3 — what C0 proved, precisely

C0 proved the command stream **replays faithfully on one machine, over shared memory** (PNG
bit-identical to native, 0/16384 bytes differing, 15/15 runs), and that `libvirglrenderer` is
**reliable** where `virgl_test_server` was flaky — which was §5's gate and the real risk.

**It proved nothing about remoting.** The proof works *because* client and host share memory. It
also never triggered the out-of-line command path (>8192 B per submission — C0's largest input is a
1008-byte SPIR-V), never wrapped the ring (7.58% peak), and never exercised the 5 s fence timeout
(it lives on `read_back`, which C0's path does not reach by design).

---

## 1. Purpose and the single success criterion

Every slice so far has **hand-emitted a fixed triangle** through a throwaway postcard protocol.
SP0's own spec said that protocol would be replaced "with the real engine" — and it is time.
Arc (c) makes an **unmodified Vulkan application** run: its GPU commands are captured on **C**,
replayed on **S**'s real GPU, with no application changes.

The feasibility spike proved the mechanism is real: Mesa's **Venus** Vulkan driver (ICD) can run
**without a virtual machine**, serializing an app's Vulkan commands over a socket to a host that
drives **virglrenderer** on the real GPU. It also proved the *debug* host (`virgl_test_server`)
is flaky, and clarified the production architecture: **S must embed `libvirglrenderer` directly
via FFI** (locked-decision 1a), not shell out to a test binary.

C0 is the **walking skeleton of the real engine**: the narrowest slice that proves a real,
unmodified Vulkan app's commands, captured by Venus, replay correctly on S's GPU **through a
Rayland-owned host that embeds virglrenderer** — machine-verified by pixels, same machine, local
socket. It is the (c)-arc analogue of SP0.

**Success criterion (measurable):** a small, **unmodified** headless Vulkan program (a reference
renderer that draws a known triangle to an offscreen image — a normal Vulkan app that knows
nothing of Rayland) runs on C with Mesa's Venus ICD pointed at a **Rayland engine host** on S;
the host, embedding `libvirglrenderer`, replays the stream on S's real GPU; the rendered image is
read back and written to a PNG; an automated test asserts the pixels (centre triangle colour,
corners background). Same machine, local Unix socket. **Reliability is part of the criterion:**
the host must initialize a Venus context and replay **repeatedly** without the flakiness the
spike hit with `virgl_test_server`.

> **Amended 2026-07-15 (Amendment 1):** "the rendered image is read back and written to a PNG"
> describes the **app's** readback, not the host's. The host-side extraction this sentence implies
> was found impossible and was superseded. The pixel assertion and the reliability criterion both
> stand and were met.

## 2. Scope — what C0 is, and is not

C0 **is**: a new **`rayland-engine`** crate that FFI-binds `libvirglrenderer` behind a clean Rust
trait; a host that speaks the **vtest wire protocol** Mesa's Venus ICD emits, drives a
virglrenderer **venus** context on the real GPU, extracts the rendered image, and writes a PNG;
and a small unmodified reference Vulkan app used as the captured workload. Local Unix socket, one
machine, one app, offscreen (no window), verified by pixels.

> **Amended 2026-07-15 (Amendment 1):** "**extracts the rendered image, and writes a PNG**" is
> **false** — the host does neither, and the evidence says it could not. The **app** does its own
> `vkMapMemory` readback and writes the PNG. Everything else in this paragraph was built as
> written.

C0 is deliberately **NOT** (each deferred to a named later slice of arc (c) or beyond):

- **No QUIC / no network / no second machine** — the transport is the local vtest Unix socket;
  swapping it for Rayland's QUIC (SP2) is **(c)1**.
- **No on-screen presentation** — the result is read back to a PNG, exactly like SP0. Wiring the
  engine's output into the SP3 dmabuf window is **(c)1**.
- **No mapped-memory coherence protocol of our own** — C0 leans on whatever Venus/virglrenderer's
  vtest path already does for the simple readback case. The general coherence work is **(c)2**.
- **No content-addressed assets** — **(c)3**.
- **No complex/real application, no GL(→Zink)** — C0's workload is a *small* headless Vulkan
  reference app for verifiability. `solarsim` and GL apps are **(c)4**.
- **No reimplementation of Vulkan** — we reuse Mesa's Venus (capture) and virglrenderer (replay);
  C0 writes only the *host glue* around them (per locked-decision 1a).
- **No production security/sandboxing** — the engine executes an untrusted command stream on the
  host GPU; C0 inherits virglrenderer's own hardening but adds no new sandboxing (SP4 track).

## 3. Architecture

```
      C side (unmodified app)                    S side (the GPU machine)
 ┌───────────────────────────────┐        ┌──────────────────────────────────────────────┐
 │ reference Vulkan app          │        │ rayland-engine host (NEW)                     │
 │  (normal Vulkan; knows        │  vtest │  ├ vtest-protocol server (accepts the socket) │
 │   nothing of Rayland)         │ ─────► │  ├ VirglEngine: FFI → libvirglrenderer        │
 │ Mesa VENUS ICD                │  Unix  │  │   drives a *venus* context on the real GPU │
 │  VK_ICD_FILENAMES=virtio_icd  │ socket │  ├ replay on S's real Vulkan GPU (ANV)         │
 │  → serialize Vulkan → socket  │        │  └ extract rendered image → readback → PNG    │
 └───────────────────────────────┘        └──────────────────────────────────────────────┘
      no GPU needed on C                        the RenderEngine trait boundary (1a) lives here
```

> **Amended 2026-07-15 (Amendments 1 and 2).** This diagram is wrong in two ways, both of which C0
> discovered rather than assumed:
> 1. The host's last box (`extract rendered image → readback → PNG`) **does not exist**. The app
>    writes the PNG.
> 2. **The `vtest` arrow is not the data path.** It carries ring *management* only — one
>    `vkCreateRingMESA`, then doorbells. **100% of the application's Vulkan commands travel through
>    a shared-memory ring** that the host allocates and the client `mmap`s via an fd we pass back
>    over `SCM_RIGHTS`. The arrow that matters is missing from this picture entirely. See
>    [the ring findings](2026-07-15-venus-ring-findings.md) and the corrected diagram in
>    [`../c0-venus-first-light.md`](../c0-venus-first-light.md).

- **C side is unmodified Mesa + configuration.** No Rayland code runs on C in C0. The app loads
  Mesa's Venus ICD (`VK_ICD_FILENAMES=…/virtio_icd.json`) and is pointed at the host's socket
  (the env the spike used: `VTEST_SOCKET_NAME`). Venus does the capture and serialization for us —
  this *is* the "reuse, don't reinvent" of locked-decision 1a on the capture side.
- **S side is the new `rayland-engine` host.** It (a) accepts the vtest socket, (b) parses the
  vtest wire protocol Mesa's Venus client speaks, (c) feeds the venus command stream to
  `libvirglrenderer` via FFI, which replays it on S's real GPU, and (d) extracts the resulting
  image and writes a PNG. This host is the robust, owned replacement for the flaky
  `virgl_test_server`.
  > **Amended 2026-07-15 (Amendment 1):** step **(d) did not happen** and could not — the app does
  > its own readback. Steps (a)–(c) were built as written, and the "robust owned replacement" claim
  > was borne out: §5's reliability gate passed.

## 4. The FFI boundary (locked-decision 1a made concrete)

Decision 1a requires the borrowed engine sit **behind a clean Rust trait** so it "could later be
Rustified or swapped without touching the rest." C0 realizes that:

- A **`RenderEngine` trait** in `rayland-engine` expresses what Rayland needs of a replay engine:
  create a context, feed it a command-stream chunk, and produce a rendered image (as a readback
  buffer in C0; as a dmabuf later). The trait names domain concepts, not virglrenderer specifics.
- A **`VirglEngine` implementation** FFI-links `libvirglrenderer` (the `virgl_renderer_*` C API)
  and implements the trait. All `unsafe`/C-FFI is confined here.
- The vtest-protocol server talks to the `RenderEngine` trait, never to virglrenderer directly —
  so a future pure-Rust or gfxstream engine is a drop-in.

`libvirglrenderer.so.1` is present at runtime; C0 adds a build dependency on the dev headers
(`libvirglrenderer-dev`) and generates or hand-writes the FFI bindings for the small surface it
needs.

## 5. The reliability de-risk (the spike's warning)

The spike enumerated a Venus device once, then `virgl_test_server` flaked (repeated
`vkEnumeratePhysicalDevices → INITIALIZATION_FAILED`, no orphan processes, native Vulkan healthy —
a DRM/EGL context artifact of repeated venus init/teardown in the *test harness*). The open risk
is whether that flakiness lives in **`libvirglrenderer` itself** or only in the throwaway harness.

Therefore **the first task of the C0 plan is a focused FFI reliability spike**: bring up a
virglrenderer venus context via FFI and initialize/replay/teardown it **repeatedly (e.g. 20×)**
in one process and across processes, proving it is *reliable*. If the library itself is flaky, C0
must solve that (correct init/teardown ordering, context lifecycle, EGL/DRM handling) before any
higher-level work — and if it proves unfixable, that is the trigger to reconsider the engine
(GFXReconstruct / crosvm-gpu), surfaced to the owner. This gates the slice, exactly as SP2's
crypto spike and SP3's dmabuf spike gated theirs.

## 6. The transport seam (what (c)1 replaces)

Mesa's Venus ICD, in its no-VM mode, speaks the **vtest** wire protocol over a Unix socket. C0's
host implements enough of that protocol to drive a venus context. That socket **is the C→S seam**:
(c)1 replaces the local Unix socket with Rayland's **QUIC** transport (SP2's `rayland-transport`),
so the app runs on a different machine. C0 keeps the boundary clean (the vtest-protocol server
reads from an abstract byte stream) so (c)1 only swaps the stream source.

> **Amended 2026-07-15 — this entire section's premise is DISPROVED (Amendment 2).**
>
> **The socket is not the C→S seam, and "(c)1 only swaps the stream source" is false.** The socket
> carries **0% of the application's Vulkan commands**. All of them are written straight into shared
> memory — pages the host allocates and the client `mmap`s via a file descriptor passed over
> `SCM_RIGHTS` — with **no protocol message when the app draws**.
>
> **Neither a shared page nor an fd crosses a network.** So there is nothing here to "swap":
> (c)1 must *design a protocol that ships the ring's bytes and synthesizes the ring's handshake on
> both ends* (the client polls `head` while the host writes it — bidirectional state, not one-way
> streaming). This also makes (c)1 far more entangled with (c)2's coherence work than this spec's
> decomposition assumed.
>
> This section is the single largest thing C0 got wrong, and finding it out was C0's most valuable
> output. **[The ring findings](2026-07-15-venus-ring-findings.md) supersede this section and are
> required reading before (c)1 is designed.**

## 7. Output extraction

virglrenderer replays into a host GPU resource. C0 extracts that resource's pixels by **readback
into CPU memory** (the SP0 path) and writes a PNG — the simplest, machine-verifiable output, and
one that does not depend on the compositor. (Later, (c)1 exports the resource as a **dmabuf** and
presents it via the SP3 window — the pieces are already built; C0 deliberately stops at the PNG so
the engine is proven in isolation.)

> **Amended 2026-07-15 — this section describes something that was not built (Amendment 1).**
>
> **"virglrenderer replays into a host GPU resource" that C0 can read is false.** There is no such
> resource. The app's `VkImage` is created by Venus commands *inside the ring* and never enters our
> resource table; a `DEVICE_LOCAL` image produces **no blob at all**; and blob resources have no
> queryable pixel format. The planned readback had **nothing to read**.
>
> **What was built:** the app performs its own `vkMapMemory` readback and writes its own PNG — what
> any ordinary offscreen Vulkan program does, which *strengthens* the "unmodified real app" claim.
>
> **What this section still gets right, and what it now costs:** the parenthetical is the live
> problem. **(c)1/SP1 must put a frame on a screen, and that genuinely does require host-side
> pixels** — which C0's app-side readback sidesteps rather than answers. `RenderEngine::read_back`
> and `EngineFrame` are therefore **off C0's critical path but are not dead code**; they are
> waiting for the answer. The bounded host-side frame-extraction spike (Task 4c) was **deferred by
> the owner** and is an **open question (c)1/SP1 must close**. It will have to work through the
> ring's object graph, not the resource table; `blob_id != 0` cleanly discriminates the app's own
> memory from Venus's plumbing and is the most promising lead.

## 8. Testing strategy

- **New reliability test (the spike, gated):** repeatedly init/replay/teardown a virglrenderer
  venus context via FFI; assert no failure over N iterations. Skips cleanly where virglrenderer /
  a usable render node is unavailable (so CI without a GPU stays green).
- **New end-to-end test (GPU-gated):** launch the `rayland-engine` host on a temporary socket, run
  the reference Vulkan app against it via Mesa's Venus ICD, and assert the resulting PNG's pixels
  (centre triangle colour, corners background). Skips cleanly when Venus/virglrenderer/GPU are
  absent (CI/lavapipe). The reference app's *native* correctness is separately unit-checked so a
  failure localizes to the engine path.
- **Unchanged & green:** all SP0–SP3 tests (the hand-emitted path stays until (c) fully supersedes
  it). C0 adds a crate; it does not modify the existing render/transport/window code.
- **CI note:** `libvirglrenderer` + Mesa Venus are Linux GPU/DRM features; the C0 GPU tests skip
  on the runner (no render node / no venus), so CI stays light — as the dmabuf tests already do.

## 9. Error handling and dependencies

- **`rayland-engine`** is a library → `LGPL-3.0-or-later`; uses `thiserror` for its error type;
  the host binary uses `anyhow`. No `unwrap`/`expect` on runtime-fallible non-test paths; every
  FFI call's failure is checked and mapped to a typed error (a C engine returning an error code
  must never become a silent success).
- New deps: a build/link dependency on **`libvirglrenderer`** (system lib + dev headers) and its
  FFI bindings (hand-written for the small surface, or `bindgen`); the reference app uses `ash`
  (already in-tree). The vtest protocol is implemented in Rust (no new crate). Presentation
  (`image` for PNG) is already in-tree.

## 10. Definition of done

- The FFI reliability spike passes (repeated venus context init/replay/teardown is reliable), and
  its outcome is recorded.
- `cargo test` passes locally (real GPU, including the reference-app → engine → PNG test) and in
  CI (GPU-dependent tests skip cleanly), plus all inherited SP0–SP3 tests.
- `cargo clippy --workspace -- -D warnings` clean; `cargo fmt` applied.
- Every function has a doc-block; every non-trivial line a value-adding comment; the FFI boundary's
  safety reasoning is documented.
- Running the reference app against the `rayland-engine` host produces a PNG whose pixels match the
  app's native output — proving a real, unmodified Vulkan app replayed on S's GPU through our host.
- A short `docs/` note shows the PNG and the exact commands.

## 11. Refinements to confirm at review (the genuinely open C0 decisions)

1. **The captured workload is a *small* headless Vulkan reference app, not `vkcube`/`solarsim`.**
   `vkcube` needs a swapchain/WSI (Venus WSI over vtest is unproven and adds a variable); `solarsim`
   is far too much for a skeleton. A tiny offscreen Vulkan renderer is *unmodified* from Rayland's
   view (Venus captures it transparently) yet fully verifiable by pixels. Real/complex apps and the
   WSI question are (c)1+/(c)4. *(If you'd rather C0 already target `vkcube`, it grows to include
   Venus WSI — say so.)*
2. **We implement the vtest protocol ourselves (in the host), driving `libvirglrenderer` via FFI —
   rather than forking `virgl_test_server` or embedding crosvm.** This is the "clean owned host"
   the spike's flakiness pointed to, and the minimal reuse-not-reinvent path. *(If the vtest
   protocol surface proves larger than a skeleton warrants, the fallback is to vendor/adapt
   `virgl_test_server`'s core — noted as a plan risk.)*
3. **C0 stops at a PNG (readback), not the screen.** Isolates "does the engine replay correctly"
   from presentation; (c)1 adds the dmabuf/window (already built). *(If you'd rather see it on
   screen immediately, that folds (c)1's presentation into C0.)*
4. **Same machine, local socket.** The network is (c)1. Keeping C0 local isolates the engine from
   the transport, exactly as SP0 was localhost before SP2 brought QUIC.

## 12. Assumption to verify first (the C0 spike)

Beyond the reliability spike (§5), the plan's early work pins the **exact `libvirglrenderer` venus
API** (context create, the vtest/venus command-submit entry points, resource creation, and the
readback path) and the **exact vtest wire framing** Mesa's Venus ICD emits — by reading the
installed Mesa/virglrenderer sources and driving the FFI against the real library. These are
uncertain today (dev headers not yet installed, API unread), so — as with SP2's quinn and SP3's
dmabuf — the plan front-loads a spike that discovers and pins them before the higher-level host is
built. If that spike shows the vtest/venus library surface is impractical to drive directly, that
is the moment to reconsider (fork `virgl_test_server`, or a different engine), surfaced to the
owner.

Everything else follows the parent design, locked-decision 1a, and `CLAUDE.md`.
