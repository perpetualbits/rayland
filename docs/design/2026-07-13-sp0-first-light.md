# SP0 — First Light

**Date:** 2026-07-13
**Status:** Sub-project design spec (awaiting owner review)
**Parent design:** [`2026-07-13-native-remote-wayland-gpu.md`](2026-07-13-native-remote-wayland-gpu.md)

---

## 1. Purpose and the single success criterion

SP0 is the **walking skeleton**: the narrowest possible slice that proves Rayland's
central claim — *that rendering commands produced on one machine can be replayed on
another machine's real GPU to produce correct pixels.* Everything hard is deliberately
cut so this one loop can be built, seen to work, and trusted.

**Success criterion (measurable):** a program on the **C** ("client") side emits a
command stream describing a single coloured triangle; that stream travels over a plain
TCP socket to the **S** ("server") side; **S replays it on a real Vulkan GPU** into an
off-screen image; and the resulting image, written to a PNG, shows the triangle on the
expected background. An automated test asserts the pixel colours directly (centre pixel ≈
triangle colour, corners ≈ clear colour), so success is verified by machine, not by eye.

## 2. Scope — what SP0 is, and (importantly) is not

SP0 **is**: emit → serialize → TCP → deserialize → replay-on-real-GPU → read-back →
PNG, for one hardcoded triangle whose vertex data is uploaded *from C* (so the data path,
not just the command path, is exercised).

SP0 is deliberately **NOT** (each deferred to a named later sub-project):

- **No real Vulkan interception.** SP0's C side does *not* capture the Vulkan calls of an
  unmodified application. It **hand-emits** a tiny Rayland command stream directly. Real
  interception (a Vulkan *layer*, later an ICD) is a large problem of its own and is
  deferred to **SP1/SP2**. *(This is a refinement of the earlier "lean toward a layer"
  note — hand-emitting is simpler and truer to "cut every corner"; see §10.)*
- **No Venus/virglrenderer FFI yet.** SP0's replayer is a small, purpose-built Rust
  Vulkan host using [`ash`](https://crates.io/crates/ash). The mature C engine is adopted
  in **SP1/SP2** once the pipe exists; SP0 only shapes the seam.
- **No QUIC** — plain blocking TCP on `localhost` (**SP2** brings QUIC and two machines).
- **No Wayland, no window** — off-screen rendering to a PNG (**SP1** puts it on screen).
- **No security, auth, or sandboxing** (**SP4**).
- **No mapped-memory coherence protocol, no asset caching** (**SP3**). SP0's single
  vertex upload is a one-shot copy, not the general mechanism.
- **No cross-architecture, no concurrency** — one client, one connection, same machine.

## 3. Architecture: two roles

```
   C side (client)                         S side (server)
 ┌───────────────────┐                   ┌────────────────────────────────────┐
 │ rayland-client    │   TCP/localhost   │ rayland-server                     │
 │  build triangle   │ ────────────────► │  read stream                       │
 │  command stream   │   (rayland-wire   │  replay via ash on real Vulkan GPU │
 │  send bytes       │    message bytes) │  off-screen image → read back      │
 └───────────────────┘                   │  encode PNG                        │
                                         └────────────────────────────────────┘
```

Both roles share one library, **`rayland-wire`**, which defines the SP0 command messages
and how they serialize to and from bytes. Neither role knows anything about the other's
internals — they agree only on the wire types.

## 4. The SP0 wire protocol

A minimal, explicit set of messages — just enough for one triangle. Serialized with
[`postcard`](https://crates.io/crates/postcard) over `serde` (pure Rust, compact,
deterministic, and works unchanged on a future RISC-V C side). Each message is
length-prefixed so the reader can frame them on a stream.

| Message | Fields | Meaning |
|---------|--------|---------|
| `Hello` | protocol version | handshake; server rejects a version it doesn't speak |
| `BeginFrame` | width, height, clear colour (RGBA) | allocate an off-screen target of this size, cleared to this colour |
| `UploadVertices` | `Vec<Vertex>` (position `[f32;2]`, colour `[f32;3]`) | the triangle's three vertices — this is the *data* crossing the wire |
| `DrawTriangles` | vertex count | draw the uploaded vertices as a triangle list |
| `EndFrame` | (none) | finish the frame; server reads back the image and writes the PNG |

This format is a **throwaway** specific to SP0 (§10). It is *not* Vulkan's wire format;
its only job is to prove the loop. SP1/SP2 replace it with the real engine's protocol.

## 5. Workspace layout

Convert the repository from a single crate into a Cargo **workspace** (the published
`rayland` name-holder crate stays as a member):

```
Cargo.toml                 # [workspace] manifest
crates/
  rayland/                 # the published placeholder/facade crate (unchanged for now)
  rayland-wire/            # lib: SP0 command messages + (de)serialization  ← unit-tested
  rayland-client/          # bin (C side): builds and sends the triangle stream
  rayland-server/          # bin (S side): receives, replays on the GPU, writes PNG
shaders/
  triangle.vert / .frag    # GLSL source (human-readable)
  triangle.vert.spv / .frag.spv   # precompiled SPIR-V, embedded via include_bytes!
```

Precompiled SPIR-V is committed and embedded so the build needs no shader-compiler
toolchain; the exact `glslangValidator` command that produced the `.spv` is recorded in a
comment in each shader and in `shaders/README.md`, so the binaries are reproducible and
auditable.

## 6. S-side replay, in detail

The server performs **head-less** (no swapchain, no surface, no window) Vulkan rendering:

1. Create a Vulkan instance and pick a physical device with a graphics queue. No
   presentation extensions are requested — this is pure off-screen compute-and-draw.
2. On `BeginFrame`: create a colour-attachment `VkImage` of the requested size, a render
   pass that clears to the requested colour, a framebuffer, and a graphics pipeline using
   the embedded SPIR-V shaders.
3. On `UploadVertices`: copy the received vertices into a `HOST_VISIBLE` vertex buffer.
   *(In SP0 this is a plain one-shot copy; it is the humble ancestor of SP3's
   mapped-memory coherence protocol, and the spec names it as such so we remember the
   lineage.)*
4. On `DrawTriangles`: record and submit a command buffer that binds the pipeline and
   vertex buffer and issues one draw.
5. On `EndFrame`: copy the rendered image into a `HOST_VISIBLE` buffer, map it, read the
   pixels, and encode a PNG with the [`image`](https://crates.io/crates/image) crate.

**Pitfall to respect (documented in the code):** Vulkan image data read back from a
`HOST_VISIBLE` buffer is laid out row by row but may be padded to an alignment; the
read-back must honour the buffer's real row stride, not assume `width * 4`. Getting this
wrong yields a subtly sheared image — a classic first-timer trap.

## 7. Testing strategy (test-first)

- **`rayland-wire` unit tests:** every message type round-trips — serialize, then
  deserialize, and assert the result equals the original. Pure and fast; written first.
- **End-to-end integration test:** start the server on a background thread bound to an
  ephemeral port, run the client against it, and assert on the **pixel buffer** (before
  PNG encoding) — centre pixel within tolerance of the triangle colour, all four corners
  within tolerance of the clear colour. Deterministic; no human inspection.
- **CI without a GPU:** CI runs the same test against **Mesa `lavapipe`** (a CPU software
  Vulkan implementation), so the full pipeline is exercised on hardware-less runners.
  Locally it runs against the real GPU. *(This mirrors how `rt` uses software GL for a
  low-power path — same idea, Vulkan side.)*

## 8. Error handling and dependencies

- **Libraries** (`rayland-wire`) use a precise error enum via
  [`thiserror`](https://crates.io/crates/thiserror); **binaries** use
  [`anyhow`](https://crates.io/crates/anyhow) for readable top-level error reports. No
  `unwrap()` on anything that can fail at runtime; every fallible step explains what it
  was trying to do when it failed.
- Dependencies are kept minimal and conventional: `ash` (Vulkan), `serde` + `postcard`
  (wire), `image` (PNG), `thiserror`/`anyhow` (errors). No large frameworks.

## 9. Definition of done

- `cargo test` passes locally (real GPU) and in CI (lavapipe), including the E2E pixel
  assertions.
- `cargo clippy` is clean; `cargo fmt` applied.
- Every function has a doc-block; every non-trivial line has a value-adding comment; code
  and comments agree (per `CLAUDE.md`).
- Running `rayland-server` then `rayland-client` by hand produces a PNG of the triangle.
- A short `docs/` note shows the PNG and the exact commands to reproduce it.

## 10. Refinements to confirm at review

Two choices deviate slightly from earlier off-hand leanings; flag now so they're
deliberate:

1. **Hand-emit instead of a Vulkan layer for SP0.** Simpler, fewer moving parts, truer to
   "cut every corner." Real interception moves to SP1/SP2. *(If you'd rather SP0 already
   intercept a real Vulkan app via a layer, say so — it enlarges SP0 noticeably.)*
2. **A throwaway SP0 wire format (postcard), not Venus's protocol.** Fastest path to first
   pixel; the mature engine's real protocol arrives with the engine itself in SP1/SP2.
   *(If you'd rather adopt the real protocol from day one, SP0 grows into SP2's territory.)*

Everything else follows the parent design and `CLAUDE.md`.
