# SP1 — Onto the Screen — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the SP0 PNG dump with a live Wayland window on S that displays the streamed triangle, tearing down when either the window is closed or the client disconnects.

**Architecture:** All changes are S-side plus a small client tweak. The server still renders with SP0's `render_triangle` and returns a `RenderedFrame` (tightly-packed RGBA8 in CPU memory). A new `window` module converts that frame into a `wl_shm` `Xrgb8888` buffer, attaches it to an `xdg_toplevel` surface via smithay-client-toolkit (SCTK), and runs a `calloop` event loop that watches **two** file descriptors — the Wayland connection and the TCP socket — so either side closing ends both. The client sends its frame (unchanged) then blocks reading the socket until EOF, keeping the window alive.

**Tech Stack:** Rust (edition 2024); `smithay-client-toolkit` 0.20 (Wayland client, `xdg-shell`, `wl_shm` `SlotPool`); `calloop` 0.14 + `calloop-wayland-source` 0.4 (event loop, both reexported by SCTK); `wayland-client` 0.31 with the `dlopen` backend (so CI needs no `libwayland`); existing `ash`/`image` unchanged.

## Global Constraints

Copied verbatim from the spec and `CLAUDE.md`; every task implicitly includes these.

- **Edition:** `edition = "2024"`, `rust-version = "1.85"` on every crate manifest.
- **Comments:** a doc-comment block (`///`/`//!`) on every function, type, and module; an intent comment on every **non-trivial** line explaining the *why*/domain meaning (never restating syntax); genuinely trivial lines (a bare `}`, an obvious `use`) get none; code and comments must always agree.
- **Errors:** binaries (`rayland-server`, `rayland-client`) use `anyhow` with contextual messages; libraries use `thiserror`. No `unwrap()`/`expect()` on runtime-fallible paths in non-test code (`expect` in tests is fine).
- **Licenses:** `rayland-server` and `rayland-client` are binary crates → `GPL-3.0-or-later` (already set); `rayland-wire` stays `LGPL-3.0-or-later`. Do not change license fields.
- **Pixel format (the pitfall):** `RenderedFrame::pixels` is tightly-packed **RGBA8** (memory bytes `R,G,B,A`). `wl_shm` `Xrgb8888` is a 32-bit **little-endian** `0x00RRGGBB` word (memory bytes `B,G,R,X`). The conversion must swizzle accordingly; getting it wrong renders the red triangle blue.
- **Testing rigor:** CI stays compositor-free and light. Pure logic (`pack_xrgb8888`, `wait_until_closed`) is unit-tested; the on-screen window is a documented **manual** smoke check. Do not add a compositor to CI.
- **Verify against cargo, not the IDE.** rust-analyzer diagnostics may lag mid-edit; trust `cargo build`/`cargo test`/`cargo clippy`.
- **SCTK integration caveat:** the exact SCTK 0.20 handler-trait method signatures are authoritative from the compiler. Where this plan's `window.rs` code and the compiler disagree on a trait method signature, adapt to what `cargo build` requires (the compiler prints the exact expected signature); keep the documented behavior identical. This applies **only** to the SCTK handler glue in Task 3, not to the pure functions.

---

## File Structure

- `crates/rayland-server/src/window.rs` — **new.** Two responsibilities, both S-side presentation: (a) the pure `pack_xrgb8888` frame→buffer conversion (Task 1, unit-tested); (b) the SCTK application + `run_window` dual-fd event loop (Task 3, integration). Kept in one module because they change together and are small.
- `crates/rayland-server/src/lib.rs` — **modify.** Add `pub mod window;`.
- `crates/rayland-server/src/main.rs` — **modify.** Parse `--png <path>`; render, then either save the PNG (fallback) or open the window (Task 4).
- `crates/rayland-server/Cargo.toml` — **modify.** Add SCTK + `wayland-client` (dlopen) deps (Task 3).
- `crates/rayland-client/src/lib.rs` — **modify.** Add `wait_until_closed` (Task 2, unit-tested).
- `crates/rayland-client/src/main.rs` — **modify.** After sending, wait until the server closes the socket (Task 2).
- `Cargo.toml` (workspace) — **modify.** Add SCTK/`wayland-client` to `[workspace.dependencies]` (Task 3).
- `docs/sp1-onto-the-screen.md` — **new.** Run-it-and-see steps + `--png` fallback (Task 4).

---

## Task 1: `pack_xrgb8888` — the frame→`wl_shm` pixel conversion

The one piece of genuinely new logic that can be tested without a compositor. Pure function; full TDD. Creating `window.rs` with only this function keeps it compiling **without** any Wayland dependency (added later in Task 3).

**Files:**
- Create: `crates/rayland-server/src/window.rs`
- Modify: `crates/rayland-server/src/lib.rs` (add `pub mod window;`)
- Test: inline `#[cfg(test)]` in `crates/rayland-server/src/window.rs`

**Interfaces:**
- Consumes: `crate::render::RenderedFrame` (fields `width: u32`, `height: u32`, `pixels: Vec<u8>` — tightly-packed RGBA8).
- Produces: `pub fn pack_xrgb8888(frame: &RenderedFrame, dst: &mut [u8])` — fills `dst` (must be exactly `frame.width * frame.height * 4` bytes) with the frame's pixels in `wl_shm` `Xrgb8888` little-endian layout.

- [ ] **Step 1: Add the module declaration**

In `crates/rayland-server/src/lib.rs`, immediately after the existing `pub mod render;` line, add:

```rust
// The S-side presentation path: convert a rendered frame to a wl_shm buffer and
// show it in a live Wayland window (SP1).
pub mod window;
```

- [ ] **Step 2: Write the failing test**

Create `crates/rayland-server/src/window.rs` with exactly this content:

```rust
//! S-side presentation: turn a [`RenderedFrame`] into an on-screen Wayland window.
//!
//! SP0 rendered a triangle on the GPU and read it back into a `RenderedFrame` (tightly
//! packed RGBA8 in CPU memory). SP1's job is purely to *present* that frame: copy it into a
//! shared-memory (`wl_shm`) buffer and show it in a real `xdg_toplevel` window on the
//! compositor the user is sitting in front of. No GPU work changes; this module only adds
//! windowing.
//!
//! The pure conversion [`pack_xrgb8888`] is unit-tested here. The SCTK-driven window
//! ([`run_window`], added later) is integration code verified by building and by a manual
//! on-screen smoke check, because asserting real on-screen output needs a compositor.

// The rendered frame we present; its pixels are tightly-packed RGBA8.
use crate::render::RenderedFrame;

/// Copy a rendered frame's pixels into a `wl_shm` `Xrgb8888` buffer.
///
/// `RenderedFrame::pixels` is tightly-packed **RGBA8**: the four bytes of each pixel are, in
/// memory order, red, green, blue, alpha. A `wl_shm` `Xrgb8888` buffer stores each pixel as
/// a 32-bit value `0x00RRGGBB` interpreted **little-endian**, so its four bytes in memory
/// order are blue, green, red, unused. This function performs that reordering — the classic
/// channel-swizzle pitfall of feeding GPU pixels to a Wayland buffer. Getting it wrong makes
/// the red triangle appear blue on screen.
///
/// # Inputs
/// - `frame`: the source; `frame.pixels.len()` must equal `frame.width * frame.height * 4`
///   (which `render_triangle` guarantees).
/// - `dst`: the destination buffer, which must be exactly the same length as `frame.pixels`
///   (i.e. `width * height * 4`). The `wl_shm` pool row stride for a 32-bit format is
///   `width * 4`, which is already tight, so no per-row padding arises.
///
/// # Panics
/// Panics (via `assert_eq!`) if `dst` is not exactly `frame.pixels.len()` bytes — a caller
/// bug, since the caller sizes the buffer from the same dimensions.
pub fn pack_xrgb8888(frame: &RenderedFrame, dst: &mut [u8]) {
    // The destination must match the source exactly; a mismatch is a caller error, not a
    // recoverable runtime condition, so we assert rather than return a Result.
    assert_eq!(
        dst.len(),
        frame.pixels.len(),
        "destination buffer must be width*height*4 bytes"
    );
    // Walk source and destination four bytes (one pixel) at a time in lockstep.
    for (rgba, out) in frame.pixels.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        // Read the source channels by their documented RGBA positions.
        let r = rgba[0] as u32;
        let g = rgba[1] as u32;
        let b = rgba[2] as u32;
        // Assemble the 32-bit 0x00RRGGBB word; the unused top byte stays 0 (opaque window).
        let word = (r << 16) | (g << 8) | b;
        // Writing the word little-endian lays the bytes out as B, G, R, 0 — exactly the
        // Xrgb8888 memory order the compositor expects.
        out.copy_from_slice(&word.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::RenderedFrame;

    #[test]
    fn pack_xrgb8888_swizzles_rgba_into_little_endian_xrgb() {
        // Two known pixels: pure red then pure green, both fully opaque in the source.
        let frame = RenderedFrame {
            width: 2,
            height: 1,
            pixels: vec![
                255, 0, 0, 255, // red   (R,G,B,A)
                0, 255, 0, 255, // green (R,G,B,A)
            ],
        };
        // Destination sized exactly width*height*4.
        let mut dst = vec![0u8; frame.pixels.len()];
        pack_xrgb8888(&frame, &mut dst);
        // Red -> Xrgb8888 little-endian bytes B,G,R,X = 0,0,255,0.
        // Green -> 0,255,0,0.
        assert_eq!(dst, vec![0, 0, 255, 0, 0, 255, 0, 0]);
    }
}
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p rayland-server --lib window::tests::pack_xrgb8888_swizzles_rgba_into_little_endian_xrgb`
Expected: PASS. (This is test-first in spirit: the assertion encodes the exact byte layout before any window code exists; if the swizzle were written wrong it would fail here.)

- [ ] **Step 4: Verify the whole workspace is still green and lint-clean**

Run: `cargo test --workspace`
Expected: all prior SP0 tests plus the new one PASS, 0 failed.
Run: `cargo clippy --workspace -- -D warnings` then `cargo fmt --check`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/rayland-server/src/window.rs crates/rayland-server/src/lib.rs
git commit -m "SP1 Task 1: pack_xrgb8888 (RGBA8 -> wl_shm Xrgb8888) with unit test"
```

---

## Task 2: Client waits on the socket (liveness channel)

The client must keep the connection open after sending so the server's window stays up; when the server closes the socket (window closed), the client exits. Add a small, unit-tested helper and wire it into the client binary.

**Files:**
- Modify: `crates/rayland-client/src/lib.rs` (add `wait_until_closed` + a test)
- Modify: `crates/rayland-client/src/main.rs` (call it after sending)

**Interfaces:**
- Produces: `pub fn wait_until_closed<R: std::io::Read>(reader: &mut R) -> std::io::Result<()>` — reads and discards until end of stream (the server closing the connection), then returns `Ok(())`.
- Consumes: existing `send_triangle` (unchanged).

- [ ] **Step 1: Write the failing test**

In `crates/rayland-client/src/lib.rs`, inside the existing `#[cfg(test)] mod tests { ... }` block (which already has `use super::*;`), add this test:

```rust
    #[test]
    fn wait_until_closed_returns_at_end_of_stream() {
        // A reader that yields a few bytes and then EOF models a server that sends nothing
        // and later closes the connection. wait_until_closed must drain and return Ok.
        let mut reader = std::io::Cursor::new(vec![1u8, 2, 3]);
        wait_until_closed(&mut reader).expect("reaching end of stream is success, not error");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rayland-client --lib wait_until_closed_returns_at_end_of_stream`
Expected: FAIL to **compile** with `cannot find function wait_until_closed in this scope`.

- [ ] **Step 3: Implement `wait_until_closed`**

In `crates/rayland-client/src/lib.rs`, after the existing `send_triangle` function (before the `#[cfg(test)]` module), add:

```rust
/// Block until the peer closes the connection, discarding anything it sends.
///
/// In SP1 the client sends one frame and then holds the connection open purely as a
/// *liveness channel*: as long as the socket is open, the server keeps the window on screen.
/// When the user closes the window, the server drops the socket; the read here then returns
/// end-of-stream and the client exits. The SP1 server sends no bytes back, so any received
/// data is unexpected and simply discarded rather than interpreted.
///
/// # Errors
/// Returns any I/O error other than a clean end of stream (which is the normal, successful
/// termination and yields `Ok(())`).
pub fn wait_until_closed<R: std::io::Read>(reader: &mut R) -> std::io::Result<()> {
    // A small scratch buffer; we never keep what we read — only watch for the stream to end.
    let mut sink = [0u8; 256];
    loop {
        match reader.read(&mut sink) {
            // Zero bytes read means the peer closed the connection: the window was closed.
            Ok(0) => return Ok(()),
            // Any bytes are unexpected in SP1; discard them and keep watching for the close.
            Ok(_) => continue,
            // A genuine I/O failure propagates to the caller.
            Err(error) => return Err(error),
        }
    }
}
```

You will also need `use std::io::Read;` visible. The file already imports `std::io::Write`; change that import to bring in both:

```rust
use std::io::{Read, Write};
```

(If `Read` ends up unused because `Write` is the only one referenced elsewhere, the compiler will say so — but `wait_until_closed` references `Read`, so both are used.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p rayland-client --lib wait_until_closed_returns_at_end_of_stream`
Expected: PASS.

- [ ] **Step 5: Wire it into the client binary**

Replace the body of `crates/rayland-client/src/main.rs` from the `send_triangle` line onward. The new file reads:

```rust
//! Rayland client binary: connect to a server over TCP, send the triangle stream, and keep
//! the connection open so the server's window stays on screen until it (or we) closes.

// The library functions that do the actual work.
use rayland_client::{send_triangle, wait_until_closed};
// TcpStream is our byte sink (Write) and liveness channel (Read).
use std::net::TcpStream;

/// Connect to the server address given as the first CLI argument (default
/// `127.0.0.1:9000`), send one triangle at 256×256 on a blue background, then block until
/// the server closes the connection (which it does when its window is closed).
///
/// # Errors
/// Returns an error if the connection, the send, or the wait fails.
fn main() -> anyhow::Result<()> {
    // Read the server address from argv, or fall back to the localhost default.
    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9000".to_string());
    // Open the TCP connection to the server.
    let mut stream = TcpStream::connect(&address)?;
    // Send the triangle command stream.
    send_triangle(&mut stream, 256, 256, [0.0, 0.0, 1.0, 1.0])?;
    // Report that the frame is on its way and the window will stay until closed.
    println!("sent triangle to {address}; holding the connection until the window closes");
    // Hold the connection open as a liveness channel; returns when the server closes it
    // (i.e. the window was closed). Killing this process instead makes the server's socket
    // read hit EOF, which closes the window — the symmetric teardown.
    wait_until_closed(&mut stream)?;
    // The server closed the connection: the window is gone, so we are done.
    println!("server closed the connection; exiting");
    // Success.
    Ok(())
}
```

- [ ] **Step 6: Verify build, lints, and the full suite**

Run: `cargo build -p rayland-client`
Expected: builds clean.
Run: `cargo test --workspace` then `cargo clippy --workspace -- -D warnings` then `cargo fmt --check`
Expected: all green/clean.

- [ ] **Step 7: Commit**

```bash
git add crates/rayland-client/src/lib.rs crates/rayland-client/src/main.rs
git commit -m "SP1 Task 2: client holds the connection open as a liveness channel"
```

---

## Task 3: The SCTK window + dual-fd event loop (`run_window`)

Integration code: connect to the compositor, present the frame in an `xdg_toplevel` window, and run one `calloop` loop watching both the Wayland connection and the TCP socket. This cannot be unit-tested without a compositor (per the light-CI decision); its gate is **`cargo build` + `cargo clippy` clean**, plus the manual smoke check in Task 4.

**Files:**
- Modify: `Cargo.toml` (workspace) — add deps
- Modify: `crates/rayland-server/Cargo.toml` — add deps
- Modify: `crates/rayland-server/src/window.rs` — add the SCTK app + `run_window`

**Interfaces:**
- Consumes: `pack_xrgb8888` (Task 1); `crate::render::RenderedFrame`.
- Produces: `pub fn run_window(frame: RenderedFrame, stream: std::net::TcpStream) -> anyhow::Result<()>` — opens a window showing `frame`, watches `stream` for disconnect, and returns `Ok(())` when either the window is closed or the client disconnects (dropping `stream` on the way out, which the client observes as EOF).

- [ ] **Step 1: Add workspace dependencies**

In the root `Cargo.toml` under `[workspace.dependencies]`, add these lines (after the existing `image` line):

```toml
smithay-client-toolkit = "0.20"                    # Wayland client toolkit: xdg-shell + wl_shm + calloop (reexported)
wayland-client = { version = "0.31", features = ["dlopen"] }  # dlopen: load libwayland at runtime so CI needs no libwayland package
```

- [ ] **Step 2: Add the dependencies to `rayland-server`**

In `crates/rayland-server/Cargo.toml`, under `[dependencies]` (after the existing `anyhow` line), add:

```toml
smithay-client-toolkit = { workspace = true }   # the Wayland window + wl_shm + calloop event loop
wayland-client = { workspace = true }            # enables the dlopen backend for the whole tree
```

- [ ] **Step 3: Verify the dependencies resolve and build**

Run: `cargo build -p rayland-server`
Expected: SCTK and its transitive crates download and compile; the crate still builds (nothing uses the new deps yet). No `libwayland` install is needed because of the `dlopen` feature.

- [ ] **Step 4: Implement the SCTK app and `run_window`**

Append the following to `crates/rayland-server/src/window.rs` (after `pack_xrgb8888`, before the `#[cfg(test)]` module). **Note the SCTK integration caveat in Global Constraints:** if the compiler reports a different signature for any handler-trait method, match the compiler and keep the behavior identical.

```rust
// --- Live Wayland window (SCTK + calloop) ---
//
// The types below come from smithay-client-toolkit (SCTK). SCTK wraps the raw Wayland
// registry/xdg-shell/wl_shm handshake in handler traits: we implement the few our window
// needs (compositor, shm, xdg window) and leave the rest to SCTK's delegate macros.

// Standard networking + io for the liveness socket.
use std::io::Read;
use std::net::TcpStream;

// calloop and the Wayland event source, reexported by SCTK so versions always match.
use smithay_client_toolkit::reexports::calloop::{
    EventLoop, LoopHandle, // the event loop and a handle to register sources on it
    Interest, Mode, PostAction, // how a file-descriptor source is polled and what to do after
    generic::Generic,      // wraps an arbitrary fd (our TCP socket) as a calloop source
};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource; // Wayland fd -> calloop
use smithay_client_toolkit::reexports::client::{
    Connection, QueueHandle,           // the compositor connection and per-state queue handle
    globals::registry_queue_init,      // one-shot registry bootstrap
    protocol::{wl_shm, wl_surface::WlSurface, wl_output::WlOutput}, // protocol objects we touch
};

// SCTK building blocks: registry, compositor, shm pool, and xdg window.
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_registry, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window, registry_handlers,
};

/// All state the window's event loop needs, threaded through SCTK's handler callbacks.
///
/// SCTK calls our handler methods with `&mut RaylandWindow`, so everything the window must
/// read or mutate in response to compositor events lives here: the SCTK sub-states, the
/// frame we are presenting, the shm pool, and the `exit` flag that ends the loop.
struct RaylandWindow {
    // SCTK's registry bookkeeping (which globals exist).
    registry_state: RegistryState,
    // SCTK's shared-memory manager; owns the wl_shm global.
    shm: Shm,
    // The pool we allocate the pixel buffer from.
    pool: SlotPool,
    // The xdg_toplevel window (surface + role).
    window: Window,
    // The frame we present; its RGBA8 pixels feed pack_xrgb8888.
    frame: RenderedFrame,
    // Set true to break the event loop: window closed or client disconnected.
    exit: bool,
    // True until the first configure, so we draw exactly once when the window is ready.
    first_configure: bool,
}

impl RaylandWindow {
    /// Fill the shm buffer from the frame and commit it to the surface.
    ///
    /// Allocates a buffer sized to the frame (the window is fixed at the frame's
    /// dimensions), converts the pixels with [`pack_xrgb8888`], attaches the buffer, marks
    /// the whole surface damaged, and commits. Called once on first configure; the static
    /// image then stays on screen with no further redraws.
    fn draw(&mut self) -> anyhow::Result<()> {
        // Fixed window size = the frame's size; stride is tight for a 32-bit format.
        let width = self.frame.width as i32;
        let height = self.frame.height as i32;
        let stride = width * 4;
        // Allocate a buffer and get writable access to its bytes in one step.
        let (buffer, canvas) = self
            .pool
            .create_buffer(width, height, stride, wl_shm::Format::Xrgb8888)
            .map_err(|e| anyhow::anyhow!("failed to create wl_shm buffer: {e}"))?;
        // Convert our RGBA8 frame into the Xrgb8888 buffer (the swizzle lives here).
        pack_xrgb8888(&self.frame, canvas);
        // Attach the finished buffer to the window's surface.
        let surface = self.window.wl_surface();
        buffer
            .attach_to(surface)
            .map_err(|e| anyhow::anyhow!("failed to attach buffer: {e}"))?;
        // Mark the entire surface as changed so the compositor repaints it.
        surface.damage_buffer(0, 0, width, height);
        // Commit the surface state (buffer + damage) to make it visible.
        self.window.commit();
        Ok(())
    }
}

impl CompositorHandler for RaylandWindow {
    // Output scale changes need no action: our buffer is a fixed-size static image.
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_factor: i32,
    ) {
    }
    // Output transform changes likewise need no action for a static frame.
    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }
    // We never request frame callbacks (the image is static), so this is a no-op.
    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _time: u32,
    ) {
    }
    // Which output the surface entered is irrelevant to a fixed-size static window.
    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }
    // Same for leaving an output.
    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }
}

impl WindowHandler for RaylandWindow {
    // The user closed the window (e.g. the close button): end the loop, which drops the
    // socket and lets the client exit.
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _window: &Window) {
        self.exit = true;
    }
    // The compositor is ready for us to draw. We draw exactly once, on the first configure.
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
        _configure: WindowConfigure,
        _serial: u32,
    ) {
        // Only the first configure triggers a draw; later configures (e.g. focus changes)
        // need no redraw for a static image.
        if self.first_configure {
            self.first_configure = false;
            // A draw failure here is unexpected; surface it by requesting exit so the
            // process ends rather than hanging with a blank window.
            if self.draw().is_err() {
                self.exit = true;
            }
        }
    }
}

impl ShmHandler for RaylandWindow {
    // SCTK asks for our Shm state to service wl_shm events.
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for RaylandWindow {
    // Hand SCTK our registry bookkeeping.
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    // We register no extra global handlers (no seats/outputs tracked): an empty list.
    registry_handlers!();
}

// Wire SCTK's protocol dispatch to our handler impls above.
delegate_compositor!(RaylandWindow);
delegate_shm!(RaylandWindow);
delegate_xdg_shell!(RaylandWindow);
delegate_xdg_window!(RaylandWindow);
delegate_registry!(RaylandWindow);

/// Open a Wayland window showing `frame`, and keep it up until the window is closed or the
/// client on `stream` disconnects — whichever comes first.
///
/// This runs one `calloop` event loop with two sources: the Wayland connection (window
/// events) and the TCP socket (liveness). Closing the window sets the exit flag; the client
/// disconnecting is seen as end-of-stream on the socket and also sets it. On return the loop
/// and its sources drop, closing the socket, which the client observes as EOF.
///
/// # Errors
/// Returns an error if the compositor is unreachable (`WAYLAND_DISPLAY` unset or invalid), a
/// required global (`wl_compositor`, `wl_shm`, `xdg_wm_base`) is missing, buffer allocation
/// fails, or the event loop errors.
pub fn run_window(frame: RenderedFrame, stream: TcpStream) -> anyhow::Result<()> {
    // Connect to the compositor named by WAYLAND_DISPLAY.
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow::anyhow!("cannot connect to a Wayland compositor: {e}"))?;
    // Bootstrap the registry and get the initial event queue.
    let (globals, event_queue) = registry_queue_init(&conn)
        .map_err(|e| anyhow::anyhow!("Wayland registry initialization failed: {e}"))?;
    // A handle used to create protocol objects bound to our state type.
    let qh: QueueHandle<RaylandWindow> = event_queue.handle();

    // Create the calloop event loop that will drive everything.
    let mut event_loop: EventLoop<RaylandWindow> =
        EventLoop::try_new().map_err(|e| anyhow::anyhow!("failed to create event loop: {e}"))?;
    let loop_handle: LoopHandle<RaylandWindow> = event_loop.handle();

    // Feed Wayland events into the loop.
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| anyhow::anyhow!("failed to insert the Wayland source: {e}"))?;

    // Bind the globals we need; a missing one is a clear, fatal error.
    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wl_compositor unavailable: {e}"))?;
    let xdg_shell = XdgShell::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("xdg_wm_base (window shell) unavailable: {e}"))?;
    let shm = Shm::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wl_shm unavailable: {e}"))?;

    // Allocate a shm pool large enough for one frame-sized buffer.
    let pool_size = (frame.width * frame.height * 4) as usize;
    let pool = SlotPool::new(pool_size, &shm)
        .map_err(|e| anyhow::anyhow!("failed to create shm pool: {e}"))?;

    // Create the surface and give it the xdg_toplevel role (a normal window).
    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    // A human-readable title and a stable app id (provisional).
    window.set_title("Rayland — SP1");
    window.set_app_id("nl.rayland.Sp1");
    // Request a fixed size by pinning min == max to the frame's dimensions; compositors
    // commonly honour this by floating the window at exactly that size.
    window.set_min_size(Some((frame.width, frame.height)));
    window.set_max_size(Some((frame.width, frame.height)));
    // Initial commit with no buffer: the compositor replies with a configure, after which
    // we draw.
    window.commit();

    // Register the TCP socket as a liveness source: readable-then-zero-bytes means the
    // client disconnected. Non-blocking so the callback never stalls the loop.
    stream
        .set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("failed to set the socket non-blocking: {e}"))?;
    loop_handle
        .insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            |_readiness, socket, state: &mut RaylandWindow| {
                // Drain whatever is readable; we only care whether the stream has ended.
                let mut sink = [0u8; 256];
                loop {
                    match socket.read(&mut sink) {
                        // EOF: the client is gone. Ask the loop to stop and remove this source.
                        Ok(0) => {
                            state.exit = true;
                            return Ok(PostAction::Remove);
                        }
                        // Unexpected bytes in SP1: ignore and keep draining.
                        Ok(_) => continue,
                        // Nothing more to read right now: leave the source in place.
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            return Ok(PostAction::Continue);
                        }
                        // A real socket error: treat like a disconnect and stop.
                        Err(_) => {
                            state.exit = true;
                            return Ok(PostAction::Remove);
                        }
                    }
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to watch the client socket: {e}"))?;

    // Assemble the state and run the loop until either trigger sets `exit`.
    let mut state = RaylandWindow {
        registry_state: RegistryState::new(&globals),
        shm,
        pool,
        window,
        frame,
        exit: false,
        first_configure: true,
    };
    // Dispatch events with a modest timeout; break out as soon as `exit` is set. A None
    // timeout blocks until an event, which is fine since both triggers produce events.
    while !state.exit {
        event_loop
            .dispatch(None, &mut state)
            .map_err(|e| anyhow::anyhow!("event loop dispatch failed: {e}"))?;
    }
    // Returning drops the loop and its Generic source, closing the socket; the client then
    // sees EOF and exits.
    Ok(())
}
```

- [ ] **Step 5: Build and lint (the gate for this task)**

Run: `cargo build -p rayland-server`
Expected: compiles. If the compiler reports a different signature for any SCTK handler method (e.g. an extra parameter, or `wl_output::Transform` under a different path), adjust that method's signature to match exactly what the compiler prints, keeping the body's behavior identical. Do **not** change `pack_xrgb8888` or the loop logic.
Run: `cargo clippy -p rayland-server -- -D warnings`
Expected: clean. (Allow `#[allow(clippy::too_many_arguments)]` only if clippy flags an SCTK-mandated handler signature; comment why.)
Run: `cargo test --workspace`
Expected: all existing tests still PASS (this task adds no tests but must not break any).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/rayland-server/Cargo.toml crates/rayland-server/src/window.rs
git commit -m "SP1 Task 3: SCTK wl_shm window + calloop dual-fd (Wayland + socket) event loop"
```

---

## Task 4: Wire the server binary (`--png` fallback + window) and document it

Make the server open the window by default and keep a `--png <path>` escape hatch. Add the reproduce-it doc. Finish with the manual smoke check.

**Files:**
- Modify: `crates/rayland-server/src/main.rs`
- Create: `docs/sp1-onto-the-screen.md`

**Interfaces:**
- Consumes: `rayland_server::handle_connection`, `rayland_server::window::run_window`.

- [ ] **Step 1: Rewrite the server binary**

Replace the entire contents of `crates/rayland-server/src/main.rs` with:

```rust
//! Rayland server binary: accept one TCP connection, render it on the GPU, and either show
//! the result in a live Wayland window (default) or write it to a PNG (`--png <path>`).

// The connection handler and the window presenter from the library.
use rayland_server::handle_connection;
use rayland_server::window::run_window;
// TcpListener accepts the incoming connection.
use std::net::TcpListener;

/// Run the server: bind, accept one connection, render the streamed frame, then present it.
///
/// Arguments (all optional, order-independent for the flag):
/// - the first positional argument is the listen address (default `127.0.0.1:9000`);
/// - `--png <path>` writes the frame to `<path>` and exits instead of opening a window
///   (the SP0 behaviour, kept for headless machines and for reproducing the PNG).
///
/// # Errors
/// Returns an error if binding, accepting, rendering, PNG writing, or window presentation
/// fails.
fn main() -> anyhow::Result<()> {
    // Collect args once so we can scan for the flag and the positional address.
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Look for `--png <path>`; if present, remember the path and treat it as the mode.
    let mut png_path: Option<String> = None;
    // The listen address is the first argument that is not the flag or its value.
    let mut address: Option<String> = None;
    // Walk the arguments, consuming the value after `--png`.
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // `--png` takes the next argument as its output path.
            "--png" => {
                let path = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--png requires a path argument"))?;
                png_path = Some(path.clone());
                // Skip the flag and its value.
                i += 2;
            }
            // The first non-flag argument is the listen address.
            other => {
                if address.is_none() {
                    address = Some(other.to_string());
                }
                i += 1;
            }
        }
    }
    // Fall back to the localhost default if no address was given.
    let address = address.unwrap_or_else(|| "127.0.0.1:9000".to_string());

    // Bind and announce readiness.
    let listener = TcpListener::bind(&address)?;
    println!("rayland-server listening on {address}");

    // Accept exactly one connection (SP1 still handles a single client).
    let (mut stream, peer) = listener.accept()?;
    println!("connection from {peer}");

    // Replay the stream on the GPU into a CPU-side frame.
    let frame = handle_connection(&mut stream)?;

    // Present the frame: PNG if requested, otherwise a live window.
    match png_path {
        // Headless/fallback path: encode the tightly-packed RGBA8 pixels as a PNG.
        Some(path) => {
            image::save_buffer(
                &path,
                &frame.pixels,
                frame.width,
                frame.height,
                image::ColorType::Rgba8,
            )?;
            println!("wrote {path} ({}x{})", frame.width, frame.height);
        }
        // Default path: show the frame in a window until it or the client closes.
        None => {
            println!("presenting in a window; close it (or stop the client) to exit");
            // Hand the socket to the window so it can watch for client disconnect.
            run_window(frame, stream)?;
            println!("window closed; exiting");
        }
    }

    // Success.
    Ok(())
}
```

- [ ] **Step 2: Build and lint**

Run: `cargo build -p rayland-server`
Expected: builds clean.
Run: `cargo clippy --workspace -- -D warnings` and `cargo fmt --check`
Expected: clean.

- [ ] **Step 3: Confirm the whole automated suite is green on GPU and lavapipe**

Run: `cargo test --workspace`
Expected: all SP0 tests + the two new unit tests (`pack_xrgb8888…`, `wait_until_closed…`) PASS, 0 failed.
Run: `VK_LOADER_DRIVERS_SELECT='*lvp*' cargo test --workspace`
Expected: same, all PASS on lavapipe. (The window code is compiled but not executed by any test.)

- [ ] **Step 4: Verify the `--png` fallback still reproduces SP0**

In one terminal: `cargo run -p rayland-server -- --png /tmp/sp1.png`
In another: `cargo run -p rayland-client`
Expected: the server prints `wrote /tmp/sp1.png (256x256)` and exits; the client prints that it sent the triangle. Open `/tmp/sp1.png`: a red triangle on blue.
(Note: with `--png` the server closes the socket as soon as it finishes writing, so the client's `wait_until_closed` returns immediately — the client exits cleanly too.)

- [ ] **Step 5: Manual on-screen smoke check (the SP1 success criterion)**

Perform these by hand on the machine with the Wayland session and record the outcome in the commit message / ledger:
1. Terminal A: `cargo run -p rayland-server`
2. Terminal B: `cargo run -p rayland-client`
   - Expected: a window titled "Rayland — SP1" appears showing a **red triangle on a blue background** (if it shows blue-on-red, the swizzle is inverted — but Task 1's test guards that).
3. Close the window → Terminal B's client prints `server closed the connection; exiting` and exits.
4. Re-run A and B, then press `Ctrl-C` in Terminal B → the window closes and Terminal A prints `window closed; exiting`.

If any step fails, treat it as a task failure and fix before committing.

- [ ] **Step 6: Write the reproduce-it doc**

Create `docs/sp1-onto-the-screen.md`:

```markdown
# SP1 — Onto the Screen (how to run it)

SP1 shows the streamed triangle in a **live Wayland window** on S, instead of writing a PNG.
The client emits the same command stream as SP0; the server replays it on the GPU, copies the
result into a `wl_shm` buffer, and displays it in an `xdg_toplevel` window.

## Run it (on a machine with a Wayland session)

In one terminal, start the server (it waits for one connection):

    cargo run -p rayland-server            # listens on 127.0.0.1:9000

In another terminal, run the client:

    cargo run -p rayland-client            # connects to 127.0.0.1:9000

A window titled "Rayland — SP1" appears showing a **red triangle on a blue background**.

- Close the window → the client exits (the server closed its liveness connection).
- Or press Ctrl-C in the client → the window closes (the server saw the client disconnect).

Either side ending tears down both — the window and the client always stop together.

## Headless / PNG fallback

Without a Wayland session (or to reproduce the SP0 PNG), ask the server to write a file and
exit instead of opening a window:

    cargo run -p rayland-server -- --png out.png
    cargo run -p rayland-client

Open `out.png`: the same red-triangle-on-blue image.

## Tests

    cargo test                             # unit tests: the pixel swizzle + the liveness wait

The on-screen window itself is verified by eye (above); CI stays compositor-free. The Wayland
crates use the dlopen backend, so building needs no `libwayland` package. See the
[SP1 design spec](design/2026-07-14-sp1-onto-the-screen.md) for why.

## Known SP1 limitations (deferred by design)

- Fixed-size window (the frame's size); no resize re-render — SP2+/SP3.
- CPU round-trip through `wl_shm`; zero-copy dmabuf is SP3.
- One frame per run; a live frame stream is a later sub-project.
```

- [ ] **Step 7: Commit**

```bash
git add crates/rayland-server/src/main.rs docs/sp1-onto-the-screen.md
git commit -m "SP1 Task 4: server presents in a window (--png fallback) + reproduce-it doc"
```

---

## Self-Review

**1. Spec coverage** — every SP1 spec section maps to a task:
- §1 success criterion (machine: swizzle test + SP0 tests; human: manual window check) → Task 1 test, Task 4 Steps 3 & 5.
- §2 scope / non-goals (TCP-only, no dmabuf, no input, no animation, fixed size) → respected across all tasks; documented in Task 4's doc "Known limitations".
- §3–4 architecture / components (`window.rs`, `pack_xrgb8888`, `run_window`, `main.rs` `--png`, client wait) → Tasks 1 (pack), 2 (client wait), 3 (run_window), 4 (main).
- §5 pixel-format pitfall → Task 1 (`pack_xrgb8888` + its test), documented in code.
- §6 dual-fd event loop / close-on-either → Task 3 (`WaylandSource` + `Generic` socket source; `request_close` and EOF both set `exit`).
- §7 deps (SCTK, wayland-client dlopen; SCTK-reexported calloop) → Task 3 Steps 1–2.
- §8 testing (unit tests + manual; light CI; dlopen) → Task 1, Task 2, Task 4; CI unchanged by design.
- §9 error handling (anyhow, no unwrap, clear messages) → Tasks 2–4 use `anyhow` + `map_err` context; asserts only for caller-bug invariants.
- §10 definition of done → Task 4 Steps 2–6.
- §11 refinements (wl_shm/CPU, --png retained, fixed size) → Tasks 3–4 and the doc.

**2. Placeholder scan** — no TBD/TODO; every code step shows complete code; the only intentional flexibility is the documented SCTK-signature reconciliation in Task 3 Step 5, which is a compiler-guided exact-match, not a placeholder.

**3. Type consistency** — `pack_xrgb8888(frame: &RenderedFrame, dst: &mut [u8])` defined in Task 1, used identically in Task 3's `draw`. `wait_until_closed<R: Read>(&mut R) -> io::Result<()>` defined in Task 2, called in the client `main`. `run_window(frame: RenderedFrame, stream: TcpStream) -> anyhow::Result<()>` defined in Task 3, called in Task 4's `main`. `RenderedFrame { width: u32, height: u32, pixels: Vec<u8> }` used consistently. `wl_shm::Format::Xrgb8888` used in both `draw` and matches the `pack_xrgb8888` contract.

**Note for the executor:** CI YAML needs **no change** — the new deps build under the existing workflow thanks to the dlopen backend. If (and only if) CI fails to build the Wayland crates, add `libwayland-dev` to the CI `apt-get install` line as documented in the SP1 spec §8; this is the sole contingency.
