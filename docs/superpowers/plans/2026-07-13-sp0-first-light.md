# SP0 — First Light Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove Rayland's core claim end-to-end — rendering commands emitted on the C ("client") side replay on the S ("server") side's real GPU to produce correct pixels — by drawing one triangle over TCP/localhost and verifying the pixels automatically.

**Architecture:** Three crates in a Cargo workspace. `rayland-wire` (library) defines a tiny command protocol and length-prefixed framing. `rayland-client` (binary + library) hand-builds a triangle command stream and sends it over TCP. `rayland-server` (binary + library) receives the stream and replays it with real Vulkan (via `ash`) into an off-screen image, then reads the pixels back tightly-packed and writes a PNG. Everything hard (real Vulkan interception, QUIC, Wayland, security, the Venus engine) is deferred to later sub-projects per the SP0 spec.

**Tech Stack:** Rust (edition 2024), `ash` (Vulkan), `serde` + `postcard` (wire), `image` (PNG), `thiserror` (library errors), `anyhow` (binary errors). CI renders on Mesa `lavapipe` (CPU software Vulkan).

## Global Constraints

Copied verbatim from the SP0 spec and `CLAUDE.md`; every task implicitly includes these.

- **Language:** Rust only, `edition = "2024"`, `rust-version = "1.85"`.
- **Licenses (per author policy):** library crates `LGPL-3.0-or-later`; binary crates `GPL-3.0-or-later`. So `rayland-wire` = LGPL-3.0-or-later; `rayland-client` and `rayland-server` = GPL-3.0-or-later.
- **Comments:** a doc-comment block (`///`/`//!`) on every function, type, and module; a *value-adding* intent comment on every non-trivial line (never a syntax restatement); genuinely trivial lines (bare `}`, obvious `use`) get none; code and comments must always agree.
- **Errors:** libraries use a precise `thiserror` enum; binaries use `anyhow`. No `unwrap()`/`expect()` on anything that can fail at runtime.
- **Wire format:** `postcard` over `serde`, each message length-prefixed with a little-endian `u32`.
- **Testing:** deterministic pixel assertions (no human inspection); the full pipeline must run GPU-less in CI via `lavapipe`.
- **Quality gate:** `cargo fmt`, `cargo clippy` clean, all tests pass, before a task is done.
- **Reference:** the SP0 spec is `docs/design/2026-07-13-sp0-first-light.md`; the parent design is `docs/design/2026-07-13-native-remote-wayland-gpu.md`.

---

### Task 1: Workspace restructure + `rayland-wire` message types

**Files:**
- Create: `Cargo.toml` (workspace root — replaces the current single-crate root manifest)
- Move: `Cargo.toml` → `crates/rayland/Cargo.toml`, `src/lib.rs` → `crates/rayland/src/lib.rs`
- Create: `crates/rayland/README.md` (short crate readme; the rich repo README stays at root)
- Create: `crates/rayland-wire/Cargo.toml`
- Create: `crates/rayland-wire/src/lib.rs`
- Create: `crates/rayland-wire/src/message.rs`
- Test: `crates/rayland-wire/src/message.rs` (unit tests in a `#[cfg(test)]` module)

**Interfaces:**
- Produces:
  - `rayland_wire::PROTOCOL_VERSION: u32`
  - `rayland_wire::Vertex { position: [f32; 2], color: [f32; 3] }` — derives `Debug, Clone, Copy, PartialEq, Serialize, Deserialize`
  - `rayland_wire::Message` enum with variants `Hello { version: u32 }`, `BeginFrame { width: u32, height: u32, clear_color: [f32; 4] }`, `UploadVertices { vertices: Vec<Vertex> }`, `DrawTriangles { vertex_count: u32 }`, `EndFrame` — derives `Debug, Clone, PartialEq, Serialize, Deserialize`

- [ ] **Step 1: Create the workspace root manifest**

Replace the root `Cargo.toml` (currently the placeholder package manifest) with a virtual workspace manifest:

```toml
# Rayland workspace root.
#
# This is a *virtual* manifest: it has no [package] of its own, only a list of
# member crates. The published name-holder crate now lives at crates/rayland.
[workspace]
resolver = "3"                       # resolver 3 is the default for edition 2024; stated explicitly for clarity
members = [
    "crates/rayland",                # the published placeholder / future facade
    "crates/rayland-wire",           # shared wire protocol (this task)
    # rayland-client and rayland-server are added by their own tasks
]

# Dependency versions shared across member crates, so every crate agrees on one version.
[workspace.dependencies]
serde = { version = "1", features = ["derive"] }   # (de)serialization derives for wire types
postcard = { version = "1", features = ["use-std"] }  # compact, deterministic, pure-Rust wire format
thiserror = "2"                      # precise error enums for library crates
anyhow = "1"                         # ergonomic top-level errors for binary crates
ash = "0.38"                         # thin Vulkan bindings (used by the server)
image = "0.25"                       # PNG encoding (used by the server)
```

- [ ] **Step 2: Move the placeholder crate under `crates/rayland`**

```bash
mkdir -p crates/rayland/src
git mv src/lib.rs crates/rayland/src/lib.rs
# The old root Cargo.toml was overwritten in Step 1; recreate the crate's own manifest:
```

Create `crates/rayland/Cargo.toml` (the package manifest that was formerly at the root, now pointing at the workspace):

```toml
# The marquee `rayland` crate. Still a placeholder that holds the crates.io name;
# it will become the workspace facade once real functionality exists.
[package]
name = "rayland"
version = "0.0.1"
edition = "2024"
rust-version = "1.85"
description = "Native remote GPU rendering for Wayland: run an app on one machine, render it on another machine's GPU and display, over a command stream rather than a pixel stream. Early work in progress."
license = "GPL-3.0-or-later"          # the project is an application
repository = "https://github.com/perpetualbits/rayland"
readme = "README.md"
keywords = ["wayland", "vulkan", "gpu", "remote-desktop", "rendering"]
categories = ["rendering"]

[dependencies]
```

Create `crates/rayland/README.md`:

```markdown
# rayland

Placeholder for the Rayland project (native remote GPU rendering for Wayland).
See the repository root README and `docs/design/` for the full picture.
```

- [ ] **Step 3: Write the failing test for the wire message types**

Create `crates/rayland-wire/src/message.rs` with only the test module first (the types it references don't exist yet, so it won't compile — that is the "red"):

```rust
#[cfg(test)]
mod tests {
    // Bring the message types into scope for the tests.
    use super::*;

    // A round-trip helper: serialize a message with postcard, deserialize it, and
    // return the result, so each test can assert "what went in comes back out".
    fn round_trip(message: &Message) -> Message {
        // Serialize to a byte vector using postcard's std-backed helper.
        let bytes = postcard::to_stdvec(message).expect("serialization must succeed for a valid message");
        // Deserialize those same bytes back into a Message.
        postcard::from_bytes(&bytes).expect("deserialization must succeed for bytes we just produced")
    }

    #[test]
    fn hello_round_trips() {
        // A Hello carrying the current protocol version must survive a round trip unchanged.
        let original = Message::Hello { version: PROTOCOL_VERSION };
        assert_eq!(round_trip(&original), original);
    }

    #[test]
    fn upload_vertices_round_trips() {
        // Three coloured vertices (the triangle) must survive a round trip unchanged.
        let original = Message::UploadVertices {
            vertices: vec![
                Vertex { position: [0.0, -0.5], color: [1.0, 0.0, 0.0] },
                Vertex { position: [0.5, 0.5], color: [1.0, 0.0, 0.0] },
                Vertex { position: [-0.5, 0.5], color: [1.0, 0.0, 0.0] },
            ],
        };
        assert_eq!(round_trip(&original), original);
    }
}
```

- [ ] **Step 4: Create the wire crate manifest and lib entry**

Create `crates/rayland-wire/Cargo.toml`:

```toml
# Shared wire protocol for Rayland. A LIBRARY, so per policy it is LGPL-3.0-or-later.
[package]
name = "rayland-wire"
version = "0.0.1"
edition = "2024"
rust-version = "1.85"
description = "Rayland's on-the-wire command messages and framing (SP0)."
license = "LGPL-3.0-or-later"
repository = "https://github.com/perpetualbits/rayland"

[dependencies]
serde = { workspace = true }          # derive Serialize/Deserialize on the message types
postcard = { workspace = true }       # the concrete serialization format
thiserror = { workspace = true }      # used by the framing module (Task 2)
```

Create `crates/rayland-wire/src/lib.rs`:

```rust
//! Rayland's on-the-wire protocol (SP0).
//!
//! This crate defines the small set of command messages that the C ("client") side
//! sends to the S ("server") side, and how they are framed on a byte stream. In SP0
//! this protocol is a deliberate throwaway — just enough to draw one triangle — and it
//! is *not* Vulkan's own wire format. Later sub-projects replace it with the real
//! command-remoting engine's protocol.

// The message types (this task).
mod message;
// Re-export them at the crate root so callers write `rayland_wire::Message`.
pub use message::{Message, Vertex, PROTOCOL_VERSION};
```

- [ ] **Step 5: Add the message types above the test module**

Prepend to `crates/rayland-wire/src/message.rs` (before the `#[cfg(test)]` module):

```rust
//! The SP0 command messages and the geometry they carry.

// serde's derive macros generate the (de)serialization code for our types.
use serde::{Deserialize, Serialize};

/// The protocol version the server and client must agree on.
///
/// SP0 is pre-1.0 throwaway protocol, so this is simply `0`. The server rejects any
/// `Hello` whose version it does not recognise, which is how future incompatible
/// changes will be caught instead of silently misinterpreting bytes.
pub const PROTOCOL_VERSION: u32 = 0;

/// One vertex of the triangle: a 2-D position and an RGB colour.
///
/// Positions are in Vulkan normalised device coordinates (each axis roughly -1.0..=1.0).
/// This is the *data* that crosses the wire in SP0 — the humble ancestor of the
/// mapped-memory and asset-residence machinery that arrives in SP3.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Vertex {
    /// Position in normalised device coordinates: `[x, y]`.
    pub position: [f32; 2],
    /// Linear RGB colour in `0.0..=1.0`: `[r, g, b]`.
    pub color: [f32; 3],
}

/// A single command from client to server.
///
/// The client sends these in order — `Hello`, `BeginFrame`, `UploadVertices`,
/// `DrawTriangles`, `EndFrame` — and the server replays them against a real GPU. Each
/// variant is documented with the server-side effect it triggers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// Handshake. The server checks `version` against [`PROTOCOL_VERSION`] and refuses
    /// to proceed if they differ.
    Hello {
        /// The protocol version the client speaks.
        version: u32,
    },
    /// Begin a frame: allocate an off-screen render target of `width`×`height` pixels
    /// and clear it to `clear_color` (RGBA, each channel `0.0..=1.0`).
    BeginFrame {
        /// Target width in pixels.
        width: u32,
        /// Target height in pixels.
        height: u32,
        /// Background colour the target is cleared to before drawing.
        clear_color: [f32; 4],
    },
    /// Upload the triangle's vertices into a GPU vertex buffer on the server.
    UploadVertices {
        /// The vertices, in draw order.
        vertices: Vec<Vertex>,
    },
    /// Draw `vertex_count` vertices as a triangle list using the uploaded vertices.
    DrawTriangles {
        /// How many vertices to draw (3 for one triangle).
        vertex_count: u32,
    },
    /// End the frame: the server reads the rendered image back and writes the PNG.
    EndFrame,
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p rayland-wire`
Expected: `hello_round_trips` and `upload_vertices_round_trips` PASS.

- [ ] **Step 7: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p rayland-wire -- -D warnings
git add -A
git commit -m "SP0 Task 1: workspace + rayland-wire message types with round-trip tests"
```

---

### Task 2: `rayland-wire` length-prefixed framing

**Files:**
- Create: `crates/rayland-wire/src/frame.rs`
- Modify: `crates/rayland-wire/src/lib.rs` (add `mod frame;` and re-exports)
- Test: `crates/rayland-wire/src/frame.rs` (`#[cfg(test)]` module)

**Interfaces:**
- Consumes: `Message` (Task 1).
- Produces:
  - `rayland_wire::WireError` (a `thiserror` enum wrapping I/O and postcard errors)
  - `rayland_wire::write_message<W: std::io::Write>(w: &mut W, msg: &Message) -> Result<(), WireError>`
  - `rayland_wire::read_message<R: std::io::Read>(r: &mut R) -> Result<Message, WireError>`

- [ ] **Step 1: Write the failing framing test**

Create `crates/rayland-wire/src/frame.rs` with the test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, Vertex, PROTOCOL_VERSION};

    #[test]
    fn messages_round_trip_through_a_buffer() {
        // A representative sequence of messages, exactly what the client sends in SP0.
        let sent = vec![
            Message::Hello { version: PROTOCOL_VERSION },
            Message::BeginFrame { width: 4, height: 4, clear_color: [0.0, 0.0, 1.0, 1.0] },
            Message::UploadVertices {
                vertices: vec![Vertex { position: [0.0, -0.5], color: [1.0, 0.0, 0.0] }],
            },
            Message::DrawTriangles { vertex_count: 3 },
            Message::EndFrame,
        ];

        // Write every message into an in-memory byte buffer (stands in for a TCP stream).
        let mut buffer: Vec<u8> = Vec::new();
        for message in &sent {
            write_message(&mut buffer, message).expect("writing to a Vec cannot fail");
        }

        // Read them back out of the buffer in order and collect them.
        let mut cursor = std::io::Cursor::new(buffer);
        let mut received = Vec::new();
        for _ in 0..sent.len() {
            received.push(read_message(&mut cursor).expect("each framed message must decode"));
        }

        // The sequence read back must equal the sequence written.
        assert_eq!(received, sent);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rayland-wire messages_round_trip_through_a_buffer`
Expected: FAIL — `write_message`/`read_message`/`WireError` are not defined.

- [ ] **Step 3: Implement the framing above the test module**

Prepend to `crates/rayland-wire/src/frame.rs`:

```rust
//! Framing: turning [`Message`] values into bytes on a stream and back.
//!
//! A byte stream (like TCP) has no message boundaries of its own, so we prefix each
//! serialized message with its length as a little-endian `u32`. The reader first reads
//! the 4-byte length, then reads exactly that many bytes, then decodes them. Fixing the
//! byte order as little-endian keeps the framing identical across CPU architectures —
//! which matters because the client may one day be big- or little-endian and a different
//! architecture from the server.

// Read/Write are the standard streaming I/O traits; std::io::Error is their error type.
use std::io::{Read, Write};

// The message type being framed.
use crate::Message;

/// Everything that can go wrong while framing or deframing a message.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// The underlying stream failed (connection closed, disk full, etc.).
    #[error("stream I/O failed while framing a message")]
    Io(#[from] std::io::Error),
    /// The bytes could not be (de)serialized as a valid message.
    #[error("message (de)serialization failed")]
    Codec(#[from] postcard::Error),
}

/// Serialize `msg` and write it to `w` as a length-prefixed frame.
///
/// The frame is a little-endian `u32` byte count followed by that many bytes of
/// postcard-encoded message. Returns an error if serialization or the write fails.
pub fn write_message<W: Write>(w: &mut W, msg: &Message) -> Result<(), WireError> {
    // Encode the message into an owned byte vector.
    let bytes = postcard::to_stdvec(msg)?;
    // The length prefix must fit in a u32; SP0 messages are tiny, so this always holds,
    // but we convert explicitly rather than silently truncate.
    let len = u32::try_from(bytes.len()).map_err(|_| {
        // Map an implausibly huge message to an I/O error rather than panicking.
        WireError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "message too large to frame",
        ))
    })?;
    // Write the 4-byte little-endian length prefix.
    w.write_all(&len.to_le_bytes())?;
    // Write the message body.
    w.write_all(&bytes)?;
    // Everything written successfully.
    Ok(())
}

/// Read one length-prefixed frame from `r` and decode it into a [`Message`].
///
/// Reads the 4-byte little-endian length, then exactly that many body bytes, then
/// decodes them. Returns an error if the stream ends early or the bytes are not a valid
/// message.
pub fn read_message<R: Read>(r: &mut R) -> Result<Message, WireError> {
    // Read the 4-byte length prefix; a short read here means the stream ended.
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    // Interpret the prefix as a little-endian u32, then widen to usize for allocation.
    let len = u32::from_le_bytes(len_bytes) as usize;
    // Allocate a buffer of exactly that size and fill it from the stream.
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    // Decode the body bytes into a Message.
    let message = postcard::from_bytes(&body)?;
    // Hand back the decoded message.
    Ok(message)
}
```

- [ ] **Step 4: Wire the module into the crate root**

Modify `crates/rayland-wire/src/lib.rs` — add below the existing `mod message;` line:

```rust
// Length-prefixed framing over byte streams (Task 2).
mod frame;
// Re-export the framing API at the crate root.
pub use frame::{read_message, write_message, WireError};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rayland-wire`
Expected: all three tests (two from Task 1, one from Task 2) PASS.

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p rayland-wire -- -D warnings
git add -A
git commit -m "SP0 Task 2: length-prefixed message framing with round-trip test"
```

---

### Task 3: Triangle shaders (GLSL source + precompiled SPIR-V)

**Files:**
- Create: `shaders/triangle.vert`
- Create: `shaders/triangle.frag`
- Create: `shaders/triangle.vert.spv` (compiled, committed binary)
- Create: `shaders/triangle.frag.spv` (compiled, committed binary)
- Create: `shaders/README.md` (records the exact compile command)

**Interfaces:**
- Produces: two SPIR-V files embedded by the server in Task 4 via `include_bytes!`.

- [ ] **Step 1: Write the vertex shader**

Create `shaders/triangle.vert`:

```glsl
#version 450
// One vertex's inputs, matching the Vertex layout uploaded over the wire.
layout(location = 0) in vec2 inPosition;   // normalised-device-coordinate position
layout(location = 1) in vec3 inColor;      // linear RGB colour
// Colour passed through to the fragment shader, interpolated across the triangle.
layout(location = 0) out vec3 fragColor;
void main() {
    // Place the vertex; z = 0, w = 1 for a simple 2-D triangle.
    gl_Position = vec4(inPosition, 0.0, 1.0);
    // Forward the colour unchanged.
    fragColor = inColor;
}
```

- [ ] **Step 2: Write the fragment shader**

Create `shaders/triangle.frag`:

```glsl
#version 450
// Interpolated colour from the vertex shader.
layout(location = 0) in vec3 fragColor;
// The pixel colour written to the render target.
layout(location = 0) out vec4 outColor;
void main() {
    // Opaque colour (alpha = 1).
    outColor = vec4(fragColor, 1.0);
}
```

- [ ] **Step 3: Compile to SPIR-V and record the command**

Run (requires `glslangValidator` from the Vulkan SDK / `glslang-tools` package):

```bash
glslangValidator -V shaders/triangle.vert -o shaders/triangle.vert.spv
glslangValidator -V shaders/triangle.frag -o shaders/triangle.frag.spv
```

Create `shaders/README.md`:

```markdown
# Shaders

`triangle.vert` / `triangle.frag` are the GLSL sources. The committed `.spv` files are
their SPIR-V compilations, embedded into `rayland-server` at build time so the build
needs no shader compiler.

Regenerate after editing the GLSL with:

    glslangValidator -V shaders/triangle.vert -o shaders/triangle.vert.spv
    glslangValidator -V shaders/triangle.frag -o shaders/triangle.frag.spv
```

- [ ] **Step 4: Verify the SPIR-V is valid (magic number)**

The first four bytes of a SPIR-V module are the magic number `0x07230203`. Verify:

```bash
python3 -c "import struct,sys; d=open('shaders/triangle.vert.spv','rb').read(); print(hex(struct.unpack('<I', d[:4])[0]))"
```

Expected output: `0x7230203`

- [ ] **Step 5: Commit**

```bash
git add shaders/
git commit -m "SP0 Task 3: triangle GLSL shaders and precompiled SPIR-V"
```

---

### Task 4: `rayland-server` off-screen Vulkan renderer

**Files:**
- Create: `crates/rayland-server/Cargo.toml`
- Create: `crates/rayland-server/src/lib.rs`
- Create: `crates/rayland-server/src/render.rs`
- Modify: `Cargo.toml` (add `crates/rayland-server` to `members`)
- Test: `crates/rayland-server/tests/render.rs`

**Interfaces:**
- Consumes: `rayland_wire::Vertex` (Task 1).
- Produces:
  - `rayland_server::render::FrameRequest { width: u32, height: u32, clear_color: [f32; 4], vertices: Vec<Vertex> }`
  - `rayland_server::render::RenderedFrame { width: u32, height: u32, pixels: Vec<u8> }` — `pixels` is tightly-packed RGBA8, `width * height * 4` bytes
  - `rayland_server::render::render_triangle(request: &FrameRequest) -> anyhow::Result<RenderedFrame>`

- [ ] **Step 1: Write the failing pixel-assertion test**

Create `crates/rayland-server/tests/render.rs`:

```rust
//! Integration test for the off-screen renderer.
//!
//! Renders one triangle into a 64×64 image and asserts on the pixels directly, so the
//! GPU path is verified by machine. Runs on a real GPU locally and on Mesa lavapipe in CI.

// The renderer under test.
use rayland_server::render::{render_triangle, FrameRequest};
// The vertex type carried by a request.
use rayland_wire::Vertex;

/// Fetch the RGBA of the pixel at (x, y) from a tightly-packed RGBA8 buffer.
fn pixel_at(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    // Each pixel is 4 bytes; compute the byte offset of (x, y).
    let index = ((y * width + x) * 4) as usize;
    // Copy the four channels out.
    [pixels[index], pixels[index + 1], pixels[index + 2], pixels[index + 3]]
}

/// Assert two channel values are within a tolerance (software and hardware rasterisers
/// differ by a few least-significant bits at edges).
fn close(actual: u8, expected: u8) -> bool {
    // Absolute difference within 8/255 is "the same colour" for our purposes.
    (actual as i16 - expected as i16).abs() <= 8
}

#[test]
fn triangle_center_is_red_and_corners_are_blue() {
    // A centred red triangle that covers the middle but not the corners of the image.
    let request = FrameRequest {
        width: 64,
        height: 64,
        clear_color: [0.0, 0.0, 1.0, 1.0], // blue background
        vertices: vec![
            Vertex { position: [0.0, -0.5], color: [1.0, 0.0, 0.0] },
            Vertex { position: [0.5, 0.5], color: [1.0, 0.0, 0.0] },
            Vertex { position: [-0.5, 0.5], color: [1.0, 0.0, 0.0] },
        ],
    };

    // Render on the real (or software) GPU.
    let frame = render_triangle(&request).expect("rendering the triangle must succeed");

    // The output must be a tightly-packed 64×64 RGBA8 buffer.
    assert_eq!(frame.pixels.len(), (64 * 64 * 4) as usize);

    // The centre pixel is inside the triangle → red.
    let center = pixel_at(&frame.pixels, 64, 32, 32);
    assert!(close(center[0], 255) && close(center[1], 0) && close(center[2], 0),
            "centre should be red, was {center:?}");

    // All four corners are outside the triangle → blue (the clear colour).
    for (x, y) in [(0, 0), (63, 0), (0, 63), (63, 63)] {
        let corner = pixel_at(&frame.pixels, 64, x, y);
        assert!(close(corner[0], 0) && close(corner[1], 0) && close(corner[2], 255),
                "corner ({x},{y}) should be blue, was {corner:?}");
    }
}
```

- [ ] **Step 2: Create the server crate manifest and lib entry**

Create `crates/rayland-server/Cargo.toml`:

```toml
# The S ("server") side: receives commands and replays them on a real GPU. A BINARY
# crate (it has a main), so per policy it is GPL-3.0-or-later.
[package]
name = "rayland-server"
version = "0.0.1"
edition = "2024"
rust-version = "1.85"
description = "Rayland server: replays a command stream on a real GPU and writes the result (SP0)."
license = "GPL-3.0-or-later"
repository = "https://github.com/perpetualbits/rayland"

[dependencies]
rayland-wire = { path = "../rayland-wire" }   # the shared command/protocol types
ash = { workspace = true }                    # Vulkan bindings for the off-screen renderer
image = { workspace = true }                  # PNG encoding for the binary
anyhow = { workspace = true }                 # top-level error handling

[dev-dependencies]
rayland-wire = { path = "../rayland-wire" }   # tests construct Vertex/FrameRequest
```

Add `"crates/rayland-server"` to the `members` array in the root `Cargo.toml`.

Create `crates/rayland-server/src/lib.rs`:

```rust
//! The Rayland server (library half).
//!
//! This crate's job in SP0 is to take a stream of [`rayland_wire::Message`] commands and
//! replay them on a real GPU, producing pixels. The GPU work lives in [`render`]; the
//! stream-handling that drives it is added in Task 6. Keeping this logic in a library
//! (rather than only in `main.rs`) is what lets the end-to-end test in Task 7 exercise it.

// The off-screen Vulkan renderer (this task).
pub mod render;
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p rayland-server --test render`
Expected: FAIL — `rayland_server::render` does not exist yet.

- [ ] **Step 4: Implement the off-screen renderer**

Create `crates/rayland-server/src/render.rs`. This is the heart of SP0: a self-contained, head-less (no window, no swapchain) Vulkan pipeline that draws the triangle and reads it back.

```rust
//! Off-screen Vulkan rendering of a single triangle.
//!
//! "Head-less" means there is no window, no swapchain, and no surface — we render into an
//! ordinary GPU image and copy the result back to CPU memory. This is exactly what the S
//! side must do when the real display is handled separately (by the compositor, in later
//! sub-projects). In SP0 the caller just gets the pixels back.
//!
//! ## Why copy-to-buffer instead of mapping the image directly
//! A GPU image in `OPTIMAL` tiling has a driver-private memory layout you cannot read
//! meaningfully on the CPU. A `LINEAR` image *can* be mapped, but each row is padded to a
//! driver-chosen `rowPitch`, and assuming `width * 4` there produces a subtly sheared
//! image — a classic first-timer trap. We sidestep both by rendering to an `OPTIMAL`
//! image and then using `vkCmdCopyImageToBuffer`, which packs the pixels tightly
//! (`bufferRowLength = 0`) into a host-visible buffer we can read directly.

// The Vulkan API surface and its core handle/struct types.
use ash::vk;
// The vertex type as it arrives over the wire.
use rayland_wire::Vertex;

/// Everything needed to render one frame.
pub struct FrameRequest {
    /// Target width in pixels.
    pub width: u32,
    /// Target height in pixels.
    pub height: u32,
    /// Background colour (RGBA, `0.0..=1.0`) the image is cleared to.
    pub clear_color: [f32; 4],
    /// The triangle's vertices, in draw order.
    pub vertices: Vec<Vertex>,
}

/// The rendered result: a tightly-packed RGBA8 image.
pub struct RenderedFrame {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes of RGBA8, row-major, no padding.
    pub pixels: Vec<u8>,
}

// The compiled shaders, embedded so the build needs no shader compiler (see Task 3).
// SPIR-V is a stream of 32-bit words, so we align the bytes to 4 for `read_spv`.
const VERT_SPV: &[u8] = include_bytes!("../../../shaders/triangle.vert.spv");
const FRAG_SPV: &[u8] = include_bytes!("../../../shaders/triangle.frag.spv");

/// Render `request`'s triangle off-screen and return the pixels.
///
/// Creates a throwaway Vulkan instance, device, image, pipeline, and vertex buffer;
/// records and submits one draw; copies the image into a host-visible buffer packed
/// tightly; and returns the RGBA8 bytes. All Vulkan objects are created and destroyed
/// within this call — SP0 renders one frame per process, so there is no state to keep.
///
/// # Errors
/// Returns an error if no Vulkan device is available or any Vulkan call fails.
pub fn render_triangle(request: &FrameRequest) -> anyhow::Result<RenderedFrame> {
    // SAFETY: every ash call below is an FFI call into the Vulkan driver. They are unsafe
    // because Vulkan trusts us to pass valid handles and sizes; we uphold that by
    // constructing each argument immediately before use and destroying handles in reverse
    // order at the end. The whole body is one unsafe block for readability.
    unsafe { render_triangle_inner(request) }
}

/// The `unsafe` body of [`render_triangle`], separated so the public function stays safe
/// to call and the safety reasoning lives in one place.
unsafe fn render_triangle_inner(request: &FrameRequest) -> anyhow::Result<RenderedFrame> {
    // Load the Vulkan loader from the system (libvulkan.so / lavapipe in CI).
    let entry = ash::Entry::load()?;

    // Describe our application; Vulkan uses this only for driver diagnostics.
    let app_info = vk::ApplicationInfo::default()
        .api_version(vk::make_api_version(0, 1, 0, 0)); // request Vulkan 1.0 — all we need

    // Create the instance with no extensions (off-screen needs none).
    let instance_info = vk::InstanceCreateInfo::default().application_info(&app_info);
    let instance = entry.create_instance(&instance_info, None)?;

    // Pick the first physical device that has a graphics-capable queue family.
    let physical_devices = instance.enumerate_physical_devices()?;
    let (physical_device, queue_family_index) = physical_devices
        .iter()
        .find_map(|&pd| {
            // Inspect each queue family for graphics support.
            instance
                .get_physical_device_queue_family_properties(pd)
                .iter()
                .enumerate()
                .find(|(_, props)| props.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .map(|(index, _)| (pd, index as u32))
        })
        .ok_or_else(|| anyhow::anyhow!("no Vulkan device with a graphics queue was found"))?;

    // Create a logical device with one graphics queue.
    let queue_priorities = [1.0f32]; // single queue, priority is irrelevant but required
    let queue_info = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(queue_family_index)
        .queue_priorities(&queue_priorities);
    let queue_infos = [queue_info];
    let device_info = vk::DeviceCreateInfo::default().queue_create_infos(&queue_infos);
    let device = instance.create_device(physical_device, &device_info, None)?;
    // Retrieve the queue we will submit work to.
    let queue = device.get_device_queue(queue_family_index, 0);

    // Query memory properties once; used to choose memory types for the image and buffers.
    let mem_props = instance.get_physical_device_memory_properties(physical_device);

    // --- Off-screen colour image (OPTIMAL tiling, used as attachment + transfer source) ---
    let format = vk::Format::R8G8B8A8_UNORM; // 8 bits per channel, matches our RGBA8 output
    let extent = vk::Extent3D { width: request.width, height: request.height, depth: 1 };
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(extent)
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = device.create_image(&image_info, None)?;
    // Allocate and bind DEVICE_LOCAL memory for the image.
    let image_mem_req = device.get_image_memory_requirements(image);
    let image_mem = allocate(&device, &mem_props, image_mem_req,
        vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
    device.bind_image_memory(image, image_mem, 0)?;

    // An image view over the whole image, needed by the framebuffer.
    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1,
        });
    let image_view = device.create_image_view(&view_info, None)?;

    // --- Render pass: clear the colour attachment, store it, leave it as a transfer src ---
    let color_attachment = vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)      // clear to clear_color at the start
        .store_op(vk::AttachmentStoreOp::STORE)    // keep the drawn pixels
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL); // ready for the readback copy
    let color_ref = vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let color_refs = [color_ref];
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_refs);
    let attachments = [color_attachment];
    let subpasses = [subpass];
    let render_pass_info = vk::RenderPassCreateInfo::default()
        .attachments(&attachments)
        .subpasses(&subpasses);
    let render_pass = device.create_render_pass(&render_pass_info, None)?;

    // Framebuffer binding the image view to the render pass.
    let fb_attachments = [image_view];
    let framebuffer_info = vk::FramebufferCreateInfo::default()
        .render_pass(render_pass)
        .attachments(&fb_attachments)
        .width(request.width)
        .height(request.height)
        .layers(1);
    let framebuffer = device.create_framebuffer(&framebuffer_info, None)?;

    // --- Shader modules from the embedded SPIR-V ---
    let vert_module = create_shader_module(&device, VERT_SPV)?;
    let frag_module = create_shader_module(&device, FRAG_SPV)?;
    // The entry point name every shader uses, as a NUL-terminated C string.
    let entry_name = std::ffi::CString::new("main").expect("literal has no NUL bytes");
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert_module)
            .name(&entry_name),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag_module)
            .name(&entry_name),
    ];

    // --- Vertex input: one binding (our Vertex), two attributes (position, colour) ---
    let binding = vk::VertexInputBindingDescription::default()
        .binding(0)
        .stride(std::mem::size_of::<Vertex>() as u32) // 5 f32s = 20 bytes
        .input_rate(vk::VertexInputRate::VERTEX);
    let attributes = [
        vk::VertexInputAttributeDescription::default()
            .location(0).binding(0)
            .format(vk::Format::R32G32_SFLOAT).offset(0),          // position at byte 0
        vk::VertexInputAttributeDescription::default()
            .location(1).binding(0)
            .format(vk::Format::R32G32B32_SFLOAT).offset(8),       // colour at byte 8
    ];
    let bindings = [binding];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attributes);

    // Draw the vertices as a list of triangles.
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);

    // A viewport and scissor covering the whole image.
    let viewport = vk::Viewport {
        x: 0.0, y: 0.0,
        width: request.width as f32, height: request.height as f32,
        min_depth: 0.0, max_depth: 1.0,
    };
    let scissor = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D { width: request.width, height: request.height },
    };
    let viewports = [viewport];
    let scissors = [scissor];
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewports(&viewports)
        .scissors(&scissors);

    // Standard rasterisation: fill triangles, no culling (so winding order can't hide it).
    let rasterizer = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    // No multisampling.
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    // Write all colour channels, no blending.
    let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)
        .blend_enable(false);
    let blend_attachments = [blend_attachment];
    let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
        .attachments(&blend_attachments);

    // An empty pipeline layout (no descriptors or push constants in SP0).
    let layout = device.create_pipeline_layout(
        &vk::PipelineLayoutCreateInfo::default(), None)?;

    // Assemble the graphics pipeline.
    let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterizer)
        .multisample_state(&multisample)
        .color_blend_state(&color_blend)
        .layout(layout)
        .render_pass(render_pass)
        .subpass(0);
    let pipeline = device
        .create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
        .map_err(|(_, e)| e)?[0]; // create_graphics_pipelines returns (pipelines, error)

    // --- Vertex buffer (host-visible so we can copy the vertices straight in) ---
    let vertex_bytes = request.vertices.len() * std::mem::size_of::<Vertex>();
    let vbuf_info = vk::BufferCreateInfo::default()
        .size(vertex_bytes as u64)
        .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let vertex_buffer = device.create_buffer(&vbuf_info, None)?;
    let vbuf_req = device.get_buffer_memory_requirements(vertex_buffer);
    let vbuf_mem = allocate(&device, &mem_props, vbuf_req,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)?;
    device.bind_buffer_memory(vertex_buffer, vbuf_mem, 0)?;
    // Map the buffer and copy the vertex data in.
    let ptr = device.map_memory(vbuf_mem, 0, vertex_bytes as u64, vk::MemoryMapFlags::empty())?;
    std::ptr::copy_nonoverlapping(
        request.vertices.as_ptr() as *const u8, ptr as *mut u8, vertex_bytes);
    device.unmap_memory(vbuf_mem);

    // --- Readback buffer (host-visible, holds the tightly-packed image after the copy) ---
    let readback_size = (request.width * request.height * 4) as u64;
    let rbuf_info = vk::BufferCreateInfo::default()
        .size(readback_size)
        .usage(vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let readback_buffer = device.create_buffer(&rbuf_info, None)?;
    let rbuf_req = device.get_buffer_memory_requirements(readback_buffer);
    let rbuf_mem = allocate(&device, &mem_props, rbuf_req,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)?;
    device.bind_buffer_memory(readback_buffer, rbuf_mem, 0)?;

    // --- Command buffer: draw, then copy image → readback buffer ---
    let pool = device.create_command_pool(
        &vk::CommandPoolCreateInfo::default().queue_family_index(queue_family_index), None)?;
    let cmd = device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1))?[0];
    device.begin_command_buffer(cmd,
        &vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;

    // Begin the render pass, clearing to the requested colour.
    let clear = vk::ClearValue { color: vk::ClearColorValue { float32: request.clear_color } };
    let clears = [clear];
    let rp_begin = vk::RenderPassBeginInfo::default()
        .render_pass(render_pass)
        .framebuffer(framebuffer)
        .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D { width: request.width, height: request.height } })
        .clear_values(&clears);
    device.cmd_begin_render_pass(cmd, &rp_begin, vk::SubpassContents::INLINE);
    device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
    device.cmd_bind_vertex_buffers(cmd, 0, &[vertex_buffer], &[0]);
    // Draw the triangle: vertex_count vertices, 1 instance.
    device.cmd_draw(cmd, request.vertices.len() as u32, 1, 0, 0);
    device.cmd_end_render_pass(cmd);

    // Copy the rendered image (now TRANSFER_SRC_OPTIMAL) into the readback buffer,
    // tightly packed: buffer_row_length = 0 means "use the image width", no padding.
    let copy = vk::BufferImageCopy::default()
        .buffer_offset(0)
        .buffer_row_length(0)
        .buffer_image_height(0)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0, base_array_layer: 0, layer_count: 1 })
        .image_extent(extent);
    device.cmd_copy_image_to_buffer(cmd, image,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL, readback_buffer, &[copy]);
    device.end_command_buffer(cmd)?;

    // Submit and wait for completion using a fence.
    let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
    let cmds = [cmd];
    let submit = vk::SubmitInfo::default().command_buffers(&cmds);
    device.queue_submit(queue, &[submit], fence)?;
    // Wait up to ~10 seconds for the GPU to finish.
    device.wait_for_fences(&[fence], true, 10_000_000_000)?;

    // --- Read the pixels out of the readback buffer ---
    let mapped = device.map_memory(rbuf_mem, 0, readback_size, vk::MemoryMapFlags::empty())?;
    // Copy the bytes into an owned Vec so we can free the GPU memory before returning.
    let mut pixels = vec![0u8; readback_size as usize];
    std::ptr::copy_nonoverlapping(mapped as *const u8, pixels.as_mut_ptr(), readback_size as usize);
    device.unmap_memory(rbuf_mem);

    // --- Tear down (reverse creation order). SP0 renders once per process, so a leak on
    // an earlier `?` is harmless; explicit destruction here keeps the happy path clean. ---
    device.destroy_fence(fence, None);
    device.destroy_command_pool(pool, None);
    device.destroy_buffer(readback_buffer, None);
    device.free_memory(rbuf_mem, None);
    device.destroy_buffer(vertex_buffer, None);
    device.free_memory(vbuf_mem, None);
    device.destroy_pipeline(pipeline, None);
    device.destroy_pipeline_layout(layout, None);
    device.destroy_shader_module(vert_module, None);
    device.destroy_shader_module(frag_module, None);
    device.destroy_framebuffer(framebuffer, None);
    device.destroy_render_pass(render_pass, None);
    device.destroy_image_view(image_view, None);
    device.destroy_image(image, None);
    device.free_memory(image_mem, None);
    device.destroy_device(None);
    instance.destroy_instance(None);

    // Hand back the pixels.
    Ok(RenderedFrame { width: request.width, height: request.height, pixels })
}

/// Choose a memory type index satisfying `requirements` and `wanted` property flags, then
/// allocate that much device memory.
///
/// Vulkan exposes several memory types with different properties (device-local,
/// host-visible, …); a buffer/image's `memory_type_bits` says which are legal for it, and
/// we pick the first legal type that also has every flag in `wanted`.
///
/// # Errors
/// Returns an error if no memory type matches or the allocation fails.
unsafe fn allocate(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    requirements: vk::MemoryRequirements,
    wanted: vk::MemoryPropertyFlags,
) -> anyhow::Result<vk::DeviceMemory> {
    // Scan the memory types for one allowed by the resource and carrying all wanted flags.
    let type_index = (0..mem_props.memory_type_count)
        .find(|&i| {
            // Bit i set in memory_type_bits means "type i is allowed for this resource".
            let allowed = requirements.memory_type_bits & (1 << i) != 0;
            // The type must also expose every property flag we asked for.
            let has_flags = mem_props.memory_types[i as usize].property_flags.contains(wanted);
            allowed && has_flags
        })
        .ok_or_else(|| anyhow::anyhow!("no suitable Vulkan memory type for {wanted:?}"))?;
    // Allocate exactly the required size of the chosen type.
    let info = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(type_index);
    Ok(device.allocate_memory(&info, None)?)
}

/// Create a Vulkan shader module from SPIR-V bytes.
///
/// SPIR-V is a sequence of 32-bit words; `ash::util::read_spv` converts the byte slice
/// into the `u32` slice Vulkan expects and validates the length and magic number.
///
/// # Errors
/// Returns an error if the bytes are not valid SPIR-V or module creation fails.
unsafe fn create_shader_module(device: &ash::Device, spv: &[u8]) -> anyhow::Result<vk::ShaderModule> {
    // Wrap the bytes in a Cursor so read_spv can consume them.
    let mut cursor = std::io::Cursor::new(spv);
    // Decode the byte stream into 32-bit SPIR-V words.
    let words = ash::util::read_spv(&mut cursor)?;
    // Build the module from the words.
    let info = vk::ShaderModuleCreateInfo::default().code(&words);
    Ok(device.create_shader_module(&info, None)?)
}
```

- [ ] **Step 5: Run the test to verify it passes (needs a Vulkan device)**

Run: `cargo test -p rayland-server --test render`
Expected: `triangle_center_is_red_and_corners_are_blue` PASS. If no local GPU, install lavapipe (see Task 8) and it will still pass on CPU.

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p rayland-server -- -D warnings
git add -A
git commit -m "SP0 Task 4: off-screen Vulkan triangle renderer with pixel-assertion test"
```

---

### Task 5: `rayland-client` — build and send the triangle stream

**Files:**
- Create: `crates/rayland-client/Cargo.toml`
- Create: `crates/rayland-client/src/lib.rs`
- Create: `crates/rayland-client/src/main.rs`
- Modify: `Cargo.toml` (add `crates/rayland-client` to `members`)
- Test: `crates/rayland-client/src/lib.rs` (`#[cfg(test)]` module)

**Interfaces:**
- Consumes: `rayland_wire::{Message, Vertex, write_message, PROTOCOL_VERSION}`.
- Produces:
  - `rayland_client::send_triangle<W: std::io::Write>(w: &mut W, width: u32, height: u32, clear_color: [f32; 4]) -> Result<(), rayland_wire::WireError>` — writes the full `Hello…EndFrame` sequence for a fixed centred red triangle.

- [ ] **Step 1: Write the failing test**

Create `crates/rayland-client/src/lib.rs` with the test first, referencing the not-yet-written function:

```rust
//! The Rayland client (library half): builds and sends a triangle command stream.

#[cfg(test)]
mod tests {
    use super::*;
    use rayland_wire::{read_message, Message, PROTOCOL_VERSION};

    #[test]
    fn send_triangle_emits_the_expected_sequence() {
        // Send into an in-memory buffer instead of a socket.
        let mut buffer: Vec<u8> = Vec::new();
        send_triangle(&mut buffer, 64, 64, [0.0, 0.0, 1.0, 1.0])
            .expect("writing to a Vec cannot fail");

        // Read the framed messages back out.
        let mut cursor = std::io::Cursor::new(buffer);
        let mut messages = Vec::new();
        while let Ok(m) = read_message(&mut cursor) {
            messages.push(m);
        }

        // The sequence must be exactly Hello, BeginFrame, UploadVertices(3), Draw(3), End.
        assert_eq!(messages.len(), 5, "expected five messages, got {}", messages.len());
        assert_eq!(messages[0], Message::Hello { version: PROTOCOL_VERSION });
        assert!(matches!(messages[1], Message::BeginFrame { width: 64, height: 64, .. }));
        assert!(matches!(&messages[2], Message::UploadVertices { vertices } if vertices.len() == 3));
        assert_eq!(messages[3], Message::DrawTriangles { vertex_count: 3 });
        assert_eq!(messages[4], Message::EndFrame);
    }
}
```

- [ ] **Step 2: Create the client crate manifest**

Create `crates/rayland-client/Cargo.toml`:

```toml
# The C ("client") side: builds a command stream and sends it. A BINARY crate, so per
# policy GPL-3.0-or-later.
[package]
name = "rayland-client"
version = "0.0.1"
edition = "2024"
rust-version = "1.85"
description = "Rayland client: emits a triangle command stream to a Rayland server (SP0)."
license = "GPL-3.0-or-later"
repository = "https://github.com/perpetualbits/rayland"

[[bin]]
name = "rayland-client"
path = "src/main.rs"

[lib]
name = "rayland_client"
path = "src/lib.rs"

[dependencies]
rayland-wire = { path = "../rayland-wire" }   # message types + framing
anyhow = { workspace = true }                 # top-level errors in main
```

Add `"crates/rayland-client"` to the `members` array in the root `Cargo.toml`.

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p rayland-client`
Expected: FAIL — `send_triangle` is not defined.

- [ ] **Step 4: Implement `send_triangle` above the test module**

Prepend to `crates/rayland-client/src/lib.rs` (before the `#[cfg(test)]` module):

```rust
// The message types, framing writer, and version constant.
use rayland_wire::{write_message, Message, Vertex, WireError, PROTOCOL_VERSION};
// Write is the trait for anything we can send bytes to (a Vec in tests, a TcpStream in main).
use std::io::Write;

/// Build the SP0 triangle command stream and write it to `w`.
///
/// Emits the fixed sequence the server expects — `Hello`, `BeginFrame`, `UploadVertices`,
/// `DrawTriangles`, `EndFrame` — for a single centred red triangle on the given clear
/// colour. The triangle geometry is hardcoded in SP0; later sub-projects derive it from a
/// real application instead.
///
/// # Errors
/// Returns a [`WireError`] if any message fails to serialize or the write fails.
pub fn send_triangle<W: Write>(
    w: &mut W,
    width: u32,
    height: u32,
    clear_color: [f32; 4],
) -> Result<(), WireError> {
    // Handshake first so the server can reject a mismatched protocol version.
    write_message(w, &Message::Hello { version: PROTOCOL_VERSION })?;
    // Ask for an off-screen target of the requested size and background colour.
    write_message(w, &Message::BeginFrame { width, height, clear_color })?;
    // The three vertices of a centred triangle, all red; it covers the image centre but
    // not the corners, which is what the server-side pixel test relies on.
    let vertices = vec![
        Vertex { position: [0.0, -0.5], color: [1.0, 0.0, 0.0] },
        Vertex { position: [0.5, 0.5], color: [1.0, 0.0, 0.0] },
        Vertex { position: [-0.5, 0.5], color: [1.0, 0.0, 0.0] },
    ];
    // Upload the geometry.
    write_message(w, &Message::UploadVertices { vertices })?;
    // Draw the three vertices as one triangle.
    write_message(w, &Message::DrawTriangles { vertex_count: 3 })?;
    // End the frame, prompting the server to read back and save the image.
    write_message(w, &Message::EndFrame)?;
    // All messages sent.
    Ok(())
}
```

- [ ] **Step 5: Implement the binary**

Create `crates/rayland-client/src/main.rs`:

```rust
//! Rayland client binary: connect to a server over TCP and send the triangle stream.

// The library function that does the actual work.
use rayland_client::send_triangle;
// TcpStream is our byte sink; it implements Write.
use std::net::TcpStream;

/// Connect to the server address given as the first CLI argument (default
/// `127.0.0.1:9000`) and send one triangle at 256×256 on a blue background.
///
/// # Errors
/// Returns an error if the connection or any send fails.
fn main() -> anyhow::Result<()> {
    // Read the server address from argv, or fall back to the localhost default.
    let address = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:9000".to_string());
    // Open the TCP connection to the server.
    let mut stream = TcpStream::connect(&address)?;
    // Send the triangle command stream.
    send_triangle(&mut stream, 256, 256, [0.0, 0.0, 1.0, 1.0])?;
    // Tell the user where the result will appear (the server writes the PNG).
    println!("sent triangle to {address}; the server writes the PNG");
    // Success.
    Ok(())
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p rayland-client`
Expected: `send_triangle_emits_the_expected_sequence` PASS.

- [ ] **Step 7: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p rayland-client -- -D warnings
git add -A
git commit -m "SP0 Task 5: rayland-client builds and sends the triangle command stream"
```

---

### Task 6: `rayland-server` — consume the stream and write the PNG

**Files:**
- Modify: `crates/rayland-server/src/lib.rs` (add `handle_connection` + `RenderedFrame` re-use)
- Create: `crates/rayland-server/src/main.rs`
- Modify: `crates/rayland-server/Cargo.toml` (declare `[lib]` and `[[bin]]`)
- Test: `crates/rayland-server/tests/handle.rs`

**Interfaces:**
- Consumes: `rayland_wire::{Message, read_message, PROTOCOL_VERSION}`; `render::{FrameRequest, RenderedFrame, render_triangle}` (Task 4).
- Produces:
  - `rayland_server::handle_connection<R: std::io::Read>(reader: &mut R) -> anyhow::Result<render::RenderedFrame>` — reads a full command stream and returns the rendered pixels.

- [ ] **Step 1: Write the failing test**

Create `crates/rayland-server/tests/handle.rs`:

```rust
//! Test that handle_connection turns a command stream into correct pixels, without any
//! sockets — the stream is an in-memory buffer built with the client's own function.

// The function under test.
use rayland_server::handle_connection;
// Reuse the client library to build a real command stream.
use rayland_client::send_triangle;

#[test]
fn handle_connection_renders_the_triangle() {
    // Build the exact byte stream a client would send.
    let mut stream: Vec<u8> = Vec::new();
    send_triangle(&mut stream, 64, 64, [0.0, 0.0, 1.0, 1.0]).expect("client build cannot fail");

    // Feed it to the server's connection handler.
    let mut cursor = std::io::Cursor::new(stream);
    let frame = handle_connection(&mut cursor).expect("handling the stream must render a frame");

    // The centre must be red (inside the triangle).
    let center = {
        let i = ((32 * 64 + 32) * 4) as usize;
        [frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2]]
    };
    assert!((center[0] as i16 - 255).abs() <= 8 && center[1] <= 8 && center[2] <= 8,
            "centre should be red, was {center:?}");
}
```

Add `rayland-client = { path = "../rayland-client" }` to `[dev-dependencies]` in `crates/rayland-server/Cargo.toml`, and declare the crate as both a lib and a bin:

```toml
[lib]
name = "rayland_server"
path = "src/lib.rs"

[[bin]]
name = "rayland-server"
path = "src/main.rs"
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rayland-server --test handle`
Expected: FAIL — `handle_connection` is not defined.

- [ ] **Step 3: Implement `handle_connection`**

Append to `crates/rayland-server/src/lib.rs`:

```rust
// The wire messages and framed reader.
use rayland_wire::{read_message, Message, PROTOCOL_VERSION};
// The renderer and its request/result types.
use render::{render_triangle, FrameRequest, RenderedFrame};

/// Read a full SP0 command stream from `reader`, replay it, and return the rendered frame.
///
/// Processes messages in order: verifies the `Hello` version, accumulates the frame
/// parameters and vertices, and renders when `EndFrame` arrives. Any message arriving out
/// of the expected order, or a version mismatch, is an error.
///
/// # Errors
/// Returns an error on a protocol-version mismatch, a malformed/out-of-order stream, an
/// early end of stream, or a rendering failure.
pub fn handle_connection<R: std::io::Read>(reader: &mut R) -> anyhow::Result<RenderedFrame> {
    // Frame parameters, filled in by BeginFrame.
    let mut width = 0u32;
    let mut height = 0u32;
    let mut clear_color = [0.0f32; 4];
    // The vertices, filled in by UploadVertices.
    let mut vertices = Vec::new();

    // Read and dispatch messages until EndFrame returns the rendered result.
    loop {
        // Read the next framed message; a stream that ends before EndFrame is an error.
        let message = read_message(reader)?;
        match message {
            Message::Hello { version } => {
                // Refuse to proceed if the client speaks a different protocol version.
                anyhow::ensure!(
                    version == PROTOCOL_VERSION,
                    "protocol version mismatch: client {version}, server {PROTOCOL_VERSION}"
                );
            }
            Message::BeginFrame { width: w, height: h, clear_color: c } => {
                // Record the target size and background colour.
                width = w;
                height = h;
                clear_color = c;
            }
            Message::UploadVertices { vertices: v } => {
                // Store the geometry for the draw.
                vertices = v;
            }
            Message::DrawTriangles { vertex_count } => {
                // SP0 draws exactly the uploaded vertices; guard the invariant.
                anyhow::ensure!(
                    vertex_count as usize == vertices.len(),
                    "DrawTriangles count {vertex_count} != uploaded vertex count {}",
                    vertices.len()
                );
            }
            Message::EndFrame => {
                // Everything is gathered; render and return.
                let request = FrameRequest { width, height, clear_color, vertices };
                return render_triangle(&request);
            }
        }
    }
}
```

- [ ] **Step 4: Implement the binary**

Create `crates/rayland-server/src/main.rs`:

```rust
//! Rayland server binary: accept one TCP connection, render it, and write a PNG.

// The connection handler from the library.
use rayland_server::handle_connection;
// TcpListener accepts incoming connections.
use std::net::TcpListener;

/// Listen on the address given as the first CLI argument (default `127.0.0.1:9000`),
/// handle exactly one connection, and write the rendered image to the path given as the
/// second argument (default `out.png`).
///
/// # Errors
/// Returns an error if binding, accepting, rendering, or writing the PNG fails.
fn main() -> anyhow::Result<()> {
    // Resolve the listen address and output path from argv, with localhost defaults.
    let address = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:9000".to_string());
    let out_path = std::env::args().nth(2).unwrap_or_else(|| "out.png".to_string());

    // Bind and announce readiness.
    let listener = TcpListener::bind(&address)?;
    println!("rayland-server listening on {address}");

    // Accept exactly one connection (SP0 handles a single client then exits).
    let (mut stream, peer) = listener.accept()?;
    println!("connection from {peer}");

    // Replay the stream on the GPU.
    let frame = handle_connection(&mut stream)?;

    // Encode the tightly-packed RGBA8 pixels as a PNG and save them.
    image::save_buffer(
        &out_path,
        &frame.pixels,
        frame.width,
        frame.height,
        image::ColorType::Rgba8,
    )?;
    println!("wrote {out_path} ({}x{})", frame.width, frame.height);

    // Success.
    Ok(())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rayland-server`
Expected: both `render` and `handle` integration tests PASS.

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p rayland-server -- -D warnings
git add -A
git commit -m "SP0 Task 6: server consumes the command stream and writes a PNG"
```

---

### Task 7: End-to-end test over a real TCP socket

**Files:**
- Create: `crates/rayland-server/tests/e2e.rs`

**Interfaces:**
- Consumes: `rayland_client::send_triangle`, `rayland_server::handle_connection`.

- [ ] **Step 1: Write the end-to-end test**

Create `crates/rayland-server/tests/e2e.rs`:

```rust
//! End-to-end: a real client on one thread sends over a real TCP socket to a server on
//! another thread, which renders and returns pixels. This is SP0's headline proof.

// Client and server library entry points.
use rayland_client::send_triangle;
use rayland_server::handle_connection;
// Networking and threading.
use std::net::{TcpListener, TcpStream};

#[test]
fn client_to_server_over_tcp_renders_the_triangle() {
    // Bind to an ephemeral port (":0" lets the OS choose a free one), avoiding clashes.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind must succeed");
    // Learn the actual address chosen so the client can connect to it.
    let address = listener.local_addr().expect("listener has an address");

    // Server thread: accept one connection, render it, hand the frame back to the test.
    let server = std::thread::spawn(move || {
        let (mut stream, _peer) = listener.accept().expect("accept must succeed");
        handle_connection(&mut stream).expect("server must render the frame")
    });

    // Client side (on the test thread): connect and send the triangle.
    let mut stream = TcpStream::connect(address).expect("client connects");
    send_triangle(&mut stream, 64, 64, [0.0, 0.0, 1.0, 1.0]).expect("client sends");
    // Close the write half so the server's read loop sees the stream end cleanly.
    stream.shutdown(std::net::Shutdown::Write).expect("shutdown write");

    // Collect the rendered frame from the server thread.
    let frame = server.join().expect("server thread must not panic");

    // Verify the triangle: centre red, one corner blue.
    let center_i = ((32 * 64 + 32) * 4) as usize;
    assert!((frame.pixels[center_i] as i16 - 255).abs() <= 8, "centre red channel");
    let corner_i = 0usize; // pixel (0,0)
    assert!(frame.pixels[corner_i + 2] >= 247, "corner blue channel");
}
```

- [ ] **Step 2: Run the end-to-end test**

Run: `cargo test -p rayland-server --test e2e`
Expected: `client_to_server_over_tcp_renders_the_triangle` PASS.

- [ ] **Step 3: Run the whole workspace test suite**

Run: `cargo test`
Expected: every test in every crate PASSES.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "SP0 Task 7: end-to-end client->TCP->server render test"
```

---

### Task 8: CI (lavapipe) and a reproduce-it doc

**Files:**
- Create: `.github/workflows/ci.yml`
- Create: `docs/sp0-first-light.md`

**Interfaces:** none (project infrastructure and documentation).

- [ ] **Step 1: Write the CI workflow**

Create `.github/workflows/ci.yml`:

```yaml
# Continuous integration for Rayland.
#
# Runners have no GPU, so we install Mesa's lavapipe — a CPU software Vulkan driver — and
# point Vulkan's loader at it. The full render pipeline (including the pixel-assertion
# tests) then runs on the CPU, exactly as it would on a real GPU.
name: CI
on:
  push:
    branches: [main]
  pull_request:
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Install Vulkan loader + lavapipe (software Vulkan)
        run: |
          sudo apt-get update
          sudo apt-get install -y mesa-vulkan-drivers libvulkan1 vulkan-tools glslang-tools
      - name: Point the Vulkan loader at lavapipe
        run: echo "VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json" >> "$GITHUB_ENV"
      - name: Format check
        run: cargo fmt --check
      - name: Clippy
        run: cargo clippy --workspace -- -D warnings
      - name: Test
        run: cargo test --workspace
```

- [ ] **Step 2: Write the reproduce-it doc**

Create `docs/sp0-first-light.md`:

```markdown
# SP0 — First Light (how to run it)

SP0 draws one triangle: the client emits rendering commands, the server replays them on a
real GPU, and writes the result as a PNG.

## Run it by hand

In one terminal, start the server (it waits for one connection, then writes `out.png`):

    cargo run -p rayland-server            # listens on 127.0.0.1:9000, writes out.png

In another terminal, run the client:

    cargo run -p rayland-client            # connects to 127.0.0.1:9000

Open `out.png`: a red triangle on a blue background, rendered on the server's GPU from the
client's command stream.

## Run the tests

    cargo test                             # includes the deterministic pixel assertions

On a machine without a GPU, install Mesa lavapipe (CPU software Vulkan) and the tests still
pass — this is what CI does:

    sudo apt-get install -y mesa-vulkan-drivers vulkan-tools
    export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json
    cargo test
```

- [ ] **Step 3: Verify CI config is well-formed and tests pass locally**

Run: `cargo test --workspace && cargo fmt --check && cargo clippy --workspace -- -D warnings`
Expected: all green.

- [ ] **Step 4: Commit and push**

```bash
git add -A
git commit -m "SP0 Task 8: CI on lavapipe and a reproduce-it doc"
git push origin main
```

---

## Self-Review

**1. Spec coverage** — every SP0 spec section maps to a task:
- Success criterion (emit→TCP→replay→PNG, machine-verified pixels) → Tasks 5, 6, 7.
- Wire protocol (§4, postcard, length-prefixed) → Tasks 1, 2.
- Workspace layout (§5) → Task 1 (root), Tasks 4/5/6 (crates), Task 3 (shaders).
- S-side replay detail (§6, off-screen, copy-to-buffer tight packing) → Task 4.
- Vertex-upload data path (§6) → Task 4 (vertex buffer), Task 5 (UploadVertices).
- Row-stride pitfall (§6) → Task 4 renderer doc + `cmd_copy_image_to_buffer` with `buffer_row_length(0)`.
- Testing strategy (§7, unit round-trips, deterministic pixels, lavapipe CI) → Tasks 1, 2, 4, 7, 8.
- Error handling (§8, thiserror lib / anyhow bin, no unwrap on fallible paths) → Tasks 2 (WireError), 4/6 (anyhow).
- License policy → Task 1 (wire = LGPL), Tasks 5/6 (bins = GPL).
- Definition of done (§9) → fmt/clippy/test steps in every task; docs in Task 8.

**2. Placeholder scan** — no "TBD/TODO/handle edge cases"; every code step carries complete code; every command has an expected result.

**3. Type consistency** — `Message`, `Vertex`, `write_message`/`read_message`, `WireError`, `FrameRequest`, `RenderedFrame`, `render_triangle`, `send_triangle`, `handle_connection` are named identically wherever they are produced and consumed across tasks. `RenderedFrame.pixels` is tightly-packed RGBA8 in Task 4 and read as such in Tasks 6/7.

One known approximation: the `ash` 0.38 calls in Task 4 are written to be substantially correct, but exact builder-method names/lifetimes may need minor adjustment against the installed `ash` version — the failing-then-passing test cycle in Task 4 Steps 3/5 is what catches and fixes any drift.
