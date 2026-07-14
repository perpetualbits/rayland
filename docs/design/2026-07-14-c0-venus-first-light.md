# C0 — Venus First Light (the real-engine pivot)

**Date:** 2026-07-14
**Status:** Sub-project design spec (awaiting owner review)
**Parent design:** [`2026-07-13-native-remote-wayland-gpu.md`](2026-07-13-native-remote-wayland-gpu.md)
**Predecessors:** SP0–SP3 (all complete, merged). This begins **arc (c)** — replacing the
hand-emitted toy protocol with a real capture/replay engine so *unmodified* applications run.
**Grounded in:** the (c)0 feasibility spike (2026-07-14), findings recorded in project memory.

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

## 2. Scope — what C0 is, and is not

C0 **is**: a new **`rayland-engine`** crate that FFI-binds `libvirglrenderer` behind a clean Rust
trait; a host that speaks the **vtest wire protocol** Mesa's Venus ICD emits, drives a
virglrenderer **venus** context on the real GPU, extracts the rendered image, and writes a PNG;
and a small unmodified reference Vulkan app used as the captured workload. Local Unix socket, one
machine, one app, offscreen (no window), verified by pixels.

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

- **C side is unmodified Mesa + configuration.** No Rayland code runs on C in C0. The app loads
  Mesa's Venus ICD (`VK_ICD_FILENAMES=…/virtio_icd.json`) and is pointed at the host's socket
  (the env the spike used: `VTEST_SOCKET_NAME`). Venus does the capture and serialization for us —
  this *is* the "reuse, don't reinvent" of locked-decision 1a on the capture side.
- **S side is the new `rayland-engine` host.** It (a) accepts the vtest socket, (b) parses the
  vtest wire protocol Mesa's Venus client speaks, (c) feeds the venus command stream to
  `libvirglrenderer` via FFI, which replays it on S's real GPU, and (d) extracts the resulting
  image and writes a PNG. This host is the robust, owned replacement for the flaky
  `virgl_test_server`.

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

## 7. Output extraction

virglrenderer replays into a host GPU resource. C0 extracts that resource's pixels by **readback
into CPU memory** (the SP0 path) and writes a PNG — the simplest, machine-verifiable output, and
one that does not depend on the compositor. (Later, (c)1 exports the resource as a **dmabuf** and
presents it via the SP3 window — the pieces are already built; C0 deliberately stops at the PNG so
the engine is proven in isolation.)

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
