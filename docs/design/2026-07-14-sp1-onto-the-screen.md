# SP1 — Onto the Screen

**Date:** 2026-07-14
**Status:** Sub-project design spec (awaiting owner review)
**Parent design:** [`2026-07-13-native-remote-wayland-gpu.md`](2026-07-13-native-remote-wayland-gpu.md)
**Predecessor:** [`2026-07-13-sp0-first-light.md`](2026-07-13-sp0-first-light.md) (complete, merged)

---

## 1. Purpose and the single success criterion

SP0 proved that a command stream produced on **C** ("client") replays correctly on **S**
("server")'s real GPU, and dumped the result to a PNG. SP1 takes the exact same rendered
frame and — instead of writing a file — **shows it in a live Wayland window on S**, with a
real event loop.

SP1's **one new hard thing** is *presentation*: opening a real `xdg_toplevel` window on the
compositor the user is sitting in front of, driving a Wayland event loop, and tearing the
window down cleanly. Everything about GPU rendering is inherited verbatim from SP0 — SP1
adds no new rendering, no new wire messages, and no new transport.

**Success criterion (measurable + observable):**

1. *Machine-verified:* a new unit test proves the pixel-format conversion that feeds the
   Wayland buffer is byte-correct (the only genuinely new logic), and **all SP0 tests
   remain green** on both a real GPU and Mesa lavapipe.
2. *Human-observed (documented manual smoke check):* running the server then the client
   opens a window on S showing a **red triangle on a blue background**; **closing the
   window makes the client exit**, and **killing the client (Ctrl-C) closes the window**.

## 2. Scope — what SP1 is, and is not

SP1 **is**: take the `RenderedFrame` that `handle_connection` already returns, copy its
pixels into a shared-memory (`wl_shm`) buffer, attach that buffer to an `xdg_toplevel`
surface, and run a Wayland event loop that keeps the window alive until either the window is
closed or the client disconnects.

SP1 is deliberately **NOT** (each deferred to a named later sub-project, unchanged from the
roadmap):

- **No QUIC, no two machines** — still plain blocking TCP on `localhost` (**SP2**).
- **No dmabuf / zero-copy GPU buffer sharing** — SP1 uses a CPU round-trip
  (GPU → readback → `wl_shm`). Zero-copy dmabuf/`linux-drm-syncobj` is **SP3**.
- **No real Vulkan interception** — the client still hand-emits the SP0 command stream
  (**SP1/SP2** in the interception track, out of scope here).
- **No input forwarding** — SP1 is output only. Routing S-side keyboard/mouse back to the
  app on C is its own later track.
- **No animation / multi-frame stream** — one frame per run. The event loop and client are
  *structured* so a frame stream is a small later addition (see §6), but SP1 does not build
  it.
- **No resize re-render** — the window is fixed at the frame's dimensions; honoring a
  compositor-suggested size by re-rendering is deferred (it is the multi-frame concern).

## 3. Architecture: the change is entirely S-side

```
   C side (client)                         S side (server)
 ┌───────────────────┐                   ┌─────────────────────────────────────────────┐
 │ rayland-client    │   TCP/localhost   │ rayland-server                              │
 │  send triangle    │ ────────────────► │  handle_connection → RenderedFrame (RGBA8)  │
 │  stream, then     │                   │  pack_xrgb8888 → wl_shm buffer              │
 │  WAIT on socket   │ ◄──── EOF ──────  │  attach + commit → xdg_toplevel window      │
 │  (liveness)       │   (on window      │  calloop loop watches { Wayland fd, TCP fd }│
 └───────────────────┘    close)         │  either closes → tear down both             │
                                         └─────────────────────────────────────────────┘
```

`rayland-wire`, `rayland-client`'s `send_triangle`, and `rayland-server`'s `render.rs` /
`handle_connection` are **unchanged**. SP1 adds one S-side module and rewires the two
binaries' `main`.

## 4. Components

### 4.1 New module: `crates/rayland-server/src/window.rs`

A [`smithay-client-toolkit`](https://crates.io/crates/smithay-client-toolkit) (SCTK)
application that:

1. Connects to the compositor named by `WAYLAND_DISPLAY`.
2. Binds the registry globals it needs: `wl_compositor`, `wl_shm`, and `xdg_wm_base`
   (the stable window-management protocol). A missing global is a clear, fatal error.
3. Creates a `wl_surface` + `xdg_surface` + `xdg_toplevel`, sets a title
   (`"Rayland — SP1"`) and app-id (`"nl.rayland.sp1"`), and hints a fixed size by setting
   the toplevel's min and max size equal to the frame's dimensions.
4. Allocates a shared-memory buffer via SCTK's `SlotPool`, fills it with `pack_xrgb8888`,
   marks the surface's opaque region (a compositor performance hint, valid because the
   frame is fully opaque), attaches the buffer, and commits.
5. Runs a [`calloop`](https://crates.io/crates/calloop) event loop with **two** sources
   (see §6) until a stop signal from either.

### 4.2 New pure function: `pack_xrgb8888(frame: &RenderedFrame, dst: &mut [u8])`

Lives in `window.rs`; depends on nothing from Wayland (so it is trivially unit-testable and
compiles even where the Wayland stack is not exercised). It copies the frame's
tightly-packed **RGBA8** pixels into `dst` in `wl_shm` `Xrgb8888` layout. See §5 for the
byte-order pitfall this function exists to handle.

### 4.3 `crates/rayland-server/src/main.rs`

Changes from "render → write PNG" to "render → present in a window." Retains a
`--png <path>` option: when given, it writes the PNG and exits (the SP0 behaviour, kept so
that headless machines and the SP0 reproduce-it flow still work); otherwise it opens the
window. Argument handling stays small and explicit.

### 4.4 `crates/rayland-client/src/main.rs`

Sends the triangle exactly as today via the unchanged `send_triangle`, then **blocks
reading the socket until EOF** rather than returning immediately. This keeps the connection
alive as a liveness channel so the window stays open; when the server closes the socket (on
window close) the read returns EOF and the client exits. `Ctrl-C` kills the client, which
the server observes as a disconnect. `send_triangle` in `lib.rs` is untouched.

## 5. The pixel-format pitfall (why `pack_xrgb8888` exists)

`RenderedFrame::pixels` is tightly-packed **RGBA8**: byte order in memory is `R, G, B, A`,
matching SP0's `R8G8B8A8_UNORM` render target (which SP1 does **not** change, so SP0's
channel-index assertions stay valid).

`wl_shm`'s `Xrgb8888` format is a 32-bit value **`0x00RRGGBB` interpreted little-endian**,
so its bytes in memory are `B, G, R, X` — the reverse channel order. `pack_xrgb8888`
therefore performs an explicit per-pixel swizzle:

```
dst[4*i + 0] = pixels[4*i + 2]   // B
dst[4*i + 1] = pixels[4*i + 1]   // G
dst[4*i + 2] = pixels[4*i + 0]   // R
dst[4*i + 3] = 0                 // X (unused; opaque window)
```

Getting this wrong renders the red triangle **blue** on screen — a defect the manual smoke
check catches immediately, and the unit test in §8 locks down. The function honours the
buffer's real row stride (the `wl_shm` pool row stride is `width * 4`, which is already the
tight stride for a 32-bit format, so no extra padding arises here — but the code computes
against the buffer's stride explicitly rather than assuming, mirroring SP0's readback
discipline). `Xrgb8888` (rather than `Argb8888`) is chosen because the window is opaque;
the alpha byte is ignored and the surface carries an opaque region.

## 6. The dual-fd event loop (the "close on either" mechanism)

A single `calloop` loop registers **two** event sources:

- **The Wayland connection**, fed into calloop by
  [`calloop-wayland-source`](https://crates.io/crates/calloop-wayland-source)'s
  `WaylandSource`. This delivers `xdg_toplevel` `configure` (acknowledged) and `close`
  events, and dispatches the SCTK state.
- **The TCP socket**, registered as a `calloop::generic::Generic` source over the stream's
  file descriptor. A readable socket that yields **zero bytes** means the client has
  disconnected.

After `handle_connection` returns the frame, the socket carries no more protocol data; it is
kept open **purely as a liveness channel**. The teardown is symmetric:

- **Window closed** (`xdg_toplevel` close, e.g. the user clicks the close button) → signal
  the loop to stop → the server drops the socket → the client's blocked read returns EOF →
  the client exits.
- **Client disconnected** (client process killed / `Ctrl-C`) → the socket becomes readable
  and reads zero bytes → signal the loop to stop → the window is destroyed.

Either trigger cleanly ends both sides. This two-fd multiplex is the same shape a real
Wayland proxy needs, so establishing it now is deliberate structure, not incidental.

**Extensibility (Q2 "structured for later"):** because presentation is already driven by an
event loop with a live socket source, a future multi-frame stream (SP2+) adds a "frame
arrived" branch on the socket source that re-fills and re-commits the buffer — without
reshaping the loop. SP1 does not build this; it only avoids precluding it.

## 7. Workspace and dependency changes

No new crates. `crates/rayland-server` gains the `window` module and these
`[workspace.dependencies]`, added to `rayland-server`'s manifest:

- `smithay-client-toolkit` (SCTK) — pulls `wayland-client`, `wayland-protocols`, `calloop`.
- `calloop-wayland-source` — bridges Wayland events into the calloop loop.

The Wayland stack is configured to use its **dlopen backend** (loading `libwayland-client`
at runtime rather than linking it), so the crate builds in CI with **no `libwayland`
package installed** and the window is simply never opened there (see §8). `image` stays for
the `--png` fallback. `rayland-server` remains a binary crate → **GPL-3.0-or-later**;
`rayland-wire` and `rayland-client` are unchanged.

## 8. Testing strategy (light CI, per the SP1 decision)

- **All SP0 tests unchanged and green** — render pixel assertions, `handle_connection`
  state machine, and the end-to-end TCP render — on both a real GPU and lavapipe. SP1
  touches none of that code.
- **New unit test for `pack_xrgb8888`** — the only new logic. Feed a small `RenderedFrame`
  with known RGBA bytes (a couple of distinct pixels) and assert the destination buffer is
  byte-exact `Xrgb8888`: red↔blue swapped, `X` byte zeroed, stride honoured. Pure and fast;
  no Wayland connection.
- **CI stays exactly as light as SP0's** — same workflow, no compositor, no display. Thanks
  to the dlopen backend, `cargo build` / `clippy` / `test` succeed without installing
  `libwayland`; the `pack_xrgb8888` test runs, and the window code is compiled and
  clippy-checked but never executed. If a future toolchain quirk requires a build-time
  `libwayland` header, adding `libwayland-dev` to the CI install step is the escape hatch,
  documented in the CI file.
- **Manual smoke check (documented in `docs/sp1-onto-the-screen.md`):**
  1. `cargo run -p rayland-server` (opens, waits for a connection).
  2. `cargo run -p rayland-client` → a window appears on S with a red triangle on blue.
  3. Close the window → the client process exits.
  4. Re-run, then `Ctrl-C` the client → the window closes.
  Plus the headless escape hatch: `cargo run -p rayland-server -- --png out.png` reproduces
  the SP0 PNG.

## 9. Error handling

- **Binary** (`rayland-server`, `rayland-client`) uses `anyhow` with contextual messages.
  No `unwrap`/`expect` on runtime-fallible paths (per `CLAUDE.md`).
- Presentation failures give clear, actionable errors: `WAYLAND_DISPLAY` unset or the
  compositor unreachable; a required global (`wl_compositor`, `wl_shm`, `xdg_wm_base`)
  absent; shm buffer allocation failure; calloop dispatch errors. On a machine with no
  compositor, the window path fails with a readable message and the `--png` fallback
  remains available.

## 10. Definition of done

- `cargo test` passes locally (real GPU) and in CI (lavapipe), including the new
  `pack_xrgb8888` test and all inherited SP0 tests.
- `cargo clippy --workspace -- -D warnings` clean; `cargo fmt` applied.
- Every function has a doc-block; every non-trivial line has a value-adding comment; code
  and comments agree (per `CLAUDE.md`).
- Running `rayland-server` then `rayland-client` opens a window showing the triangle;
  closing the window exits the client; killing the client closes the window.
- `docs/sp1-onto-the-screen.md` documents the run-it-and-see steps and the `--png` fallback.

## 11. Refinements to confirm at review

1. **`wl_shm` + CPU round-trip for SP1, dmabuf deferred to SP3.** SP1 knowingly renders on
   the GPU, reads back to CPU, and hands CPU pixels to the compositor (which re-uploads).
   Wasteful in production, trivial for one triangle, and it keeps SP1's new surface area to
   "a window and an event loop." Zero-copy is SP3, as already planned.
2. **`--png` fallback retained in `main.rs`.** Keeps headless machines and the SP0
   reproduce-it flow working; near-zero cost.
3. **Fixed-size window; resize deferred.** The toplevel hints min = max = the frame size;
   honoring a different compositor-suggested size would require re-rendering, which is the
   multi-frame concern held for later.

Everything else follows the parent design and `CLAUDE.md`.
