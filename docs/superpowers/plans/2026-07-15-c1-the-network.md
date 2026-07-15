# (c)1 — The Network — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split C0's single-machine host into a C-side relay daemon and an S-side engine host joined by QUIC, so the unmodified `rayland-refapp` runs on `appollo` while its frame is rendered on `dop561`'s GPU and presented on `dop561`'s screen — and measure what that costs.

**Architecture:** `rayland-c` *is* a vtest server, so stock Mesa talks to it over a local Unix socket and cannot tell it is not an ordinary vtest host. It allocates the Venus command ring and every blob as **local memfds**, watches the ring's `tail`, and relays ring deltas + mapped-blob contents over QUIC to `rayland-s`, which re-materializes them into a real virglrenderer context, replays on the GPU, and presents. No Mesa fork, no app changes.

**Tech Stack:** Rust edition 2024; `postcard` framing (following `rayland-wire`'s pattern); `quinn` via SP2's `rayland-transport`; `libc` for memfd/mmap/sendmsg; Mesa 26.0.3 Venus ICD (stock) on C; `libvirglrenderer` 1.2.0 on S; `image` for PNG; SCTK/`wayland-client` for presentation.

**Spec:** [`docs/design/2026-07-15-c1-the-network.md`](../../design/2026-07-15-c1-the-network.md) — **read it before Task 1.** Its §5 channel inventory and §6 crutch table are binding.
**Required background:** [`docs/design/2026-07-15-venus-ring-findings.md`](../../design/2026-07-15-venus-ring-findings.md) — the evidence this design rests on. Do not re-derive it.

## Global Constraints

- **Edition:** `edition = "2024"`, `rust-version = "1.85"`.
- **C links no GPU code.** `rayland-c` and everything it depends on must **never** link `libvirglrenderer`. This is not tidiness — C is meant to be the weak machine (eventually RISC-V `milkv`), and if C needs a GPU stack the project's thesis is false. **Task 1 adds a test that enforces this mechanically.**
- **Licences:** libraries → `LGPL-3.0-or-later` (`thiserror`); binaries → `GPL-3.0-or-later` (`anyhow`). Each crate declares its own.
- **No `unwrap`/`expect` on runtime-fallible non-test paths** (`expect` in tests is fine).
- **Comments:** doc-block on every fn/type/module; intent comment on every non-trivial line (the *why*/domain meaning, never restating syntax); **code and comments must always agree** — a stale comment is a bug fixed in the same edit.
- **`rayland-refapp` stays unmodified.** Zero `rayland-*` dependencies, zero Venus/remoting awareness. If a task seems to need a refapp change, that is a design error — stop and report.
- **No silent caps.** Anything unimplemented (notably the out-of-line stream path) must produce a **typed error naming what happened**, never a guess, never a silent drop.
- **Tests skip cleanly** when their dependency (GPU / network / second machine) is absent, as C0's do. CI stays light.
- **Verify against the real library, GPU and network — not the IDE.** rust-analyzer served stale phantom compile errors four times during C0. Trust `cargo`.
- **Venus configuration is a crutch table, not configuration** (spec §6). Every setting below is temporary except `VN_DEBUG=vtest`, and each must be named in code comments with its exit condition:
  - `VN_DEBUG=vtest` — **permanent**; without it Mesa silently prefers virtgpu and never connects.
  - `VN_PERF=no_multi_ring` — forces a single ring, making the inherited `ring_idx = 0` assumption legitimate.
  - `VN_PERF=no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback` — removes the S→C shared status pages.
  - `VN_DEBUG=no_abort` — **only after Task 3's progress-aware timeout exists** (spec §6.2).
- **Socket paths must be short** — `sun_path` is 108 bytes. Use `/tmp/rl-c1.sock`, never a scratchpad path.
- **Machines:** C = `appollo.localdomain` (x86_64, AMD GPU **unused**, key-based ssh, passwordless sudo, do not break it). S = `dop561` (Intel Iris Xe, the display). Loopback on `dop561` is the CI/dev path.

---

## File Structure

| Path | Responsibility |
|---|---|
| `crates/rayland-vtest/` | **New (Task 1, moved out of `rayland-engine`).** The vtest wire protocol, `venus_ring/`, the `RenderEngine` trait, `VtestTransport`, `EngineError`. **Pure Rust, no FFI.** Both sides depend on it. |
| `crates/rayland-engine/` | **Shrinks (Task 1).** virglrenderer FFI (`ffi.rs`, `virgl.rs`) only. S-side only. |
| `crates/rayland-relay/src/lib.rs` | **New (Task 2).** The (c)1 protocol: `C2S`/`S2C` message enums + postcard framing. |
| `crates/rayland-c/src/main.rs` | **New (Task 3).** Daemon entry: bind the Unix socket, accept, run. |
| `crates/rayland-c/src/shm.rs` | **New (Task 3).** Local memfd blob allocation + mapping. |
| `crates/rayland-c/src/ring.rs` | **New (Task 3).** Ring watcher: `tail` deltas, `head` advance, park/kick. |
| `crates/rayland-c/src/relay_engine.rs` | **New (Task 3).** `RelayEngine: RenderEngine` — forwards to S instead of rendering. |
| `crates/rayland-s/src/main.rs` | **New (Task 4).** QUIC listener → apply relay messages → `VirglEngine`. |
| `crates/rayland-s/src/apply.rs` | **New (Task 4).** Applies `C2S` messages to the engine; produces `S2C`. |
| `crates/rayland-present/` | **New (Task 7, extracted from `rayland-server/src/window.rs`).** dmabuf + `wl_shm` presentation. |
| `crates/rayland-s/src/present.rs` | **New (Task 7).** Presents the readback blob via `wl_shm`. |
| `crates/rayland-c/src/metrics.rs` | **New (Task 9).** Round-trip / byte / latency counters. |
| `docs/c1-the-network.md` | **New (Task 9).** How to run it + **the measurement table**. |

---

## Task 1: Split `rayland-vtest` out of `rayland-engine` (C must not link virglrenderer)

The foundational task. `rayland-engine` FFI-links `libvirglrenderer`; the C side must not. Everything else depends on this boundary existing.

**Files:**
- Create: `crates/rayland-vtest/Cargo.toml`
- Move (git mv, content unchanged where possible): `crates/rayland-engine/src/{vtest.rs,transport.rs,error.rs}` and `crates/rayland-engine/src/venus_ring/` → `crates/rayland-vtest/src/`
- Create: `crates/rayland-vtest/src/lib.rs` (the `RenderEngine` trait + `EngineFrame` + re-exports, moved from `rayland-engine/src/lib.rs`)
- Modify: `crates/rayland-engine/src/lib.rs` (keep only the FFI impl; re-export `rayland-vtest`'s types so existing users compile), `crates/rayland-engine/Cargo.toml`, `Cargo.toml` (workspace members)
- Test: `crates/rayland-vtest/tests/no_gpu_linkage.rs`

**Interfaces:**
- Produces: crate `rayland_vtest` exporting `RenderEngine`, `EngineError`, `EngineFrame`, `BlobResource`, `VtestTransport`, `serve_vtest`, `venus_ring::*`.
- `rayland-engine` continues to export `VirglEngine` and `virgl_available`, and **re-exports** `rayland_vtest::*` so `rayland-engine`'s existing tests and `examples/vtest_serve.rs` keep compiling.

- [ ] **Step 1: Create the crate and move the pure-Rust modules**

```bash
cd /home/roland/git/rayland
mkdir -p crates/rayland-vtest/src
git mv crates/rayland-engine/src/vtest.rs      crates/rayland-vtest/src/vtest.rs
git mv crates/rayland-engine/src/transport.rs  crates/rayland-vtest/src/transport.rs
git mv crates/rayland-engine/src/error.rs      crates/rayland-vtest/src/error.rs
git mv crates/rayland-engine/src/venus_ring    crates/rayland-vtest/src/venus_ring
```

`crates/rayland-vtest/Cargo.toml`:

```toml
[package]
name = "rayland-vtest"
version = "0.0.1"
edition = "2024"
rust-version = "1.85"
# Library → LGPL per CLAUDE.md's licensing policy.
license = "LGPL-3.0-or-later"
description = "Mesa Venus vtest wire protocol and command-ring knowledge. No GPU dependencies."

[dependencies]
thiserror = { workspace = true }
# The four syscalls the vtest protocol needs and std does not expose:
# sendmsg (SCM_RIGHTS), memfd_create + mmap (blob shared memory), eventfd (SYNC_WAIT replies).
libc = { workspace = true }
```

Add `"crates/rayland-vtest",` to the workspace `members` in `/home/roland/git/rayland/Cargo.toml`.

- [ ] **Step 2: Write the linkage test FIRST (it is the point of this task)**

Create `crates/rayland-vtest/tests/no_gpu_linkage.rs`:

```rust
//! Enforces (c)1's load-bearing structural constraint: `rayland-vtest` must never pull in a GPU
//! stack. The C side of Rayland depends on this crate, and C is by design the *weak* machine — a
//! headless box, eventually a RISC-V one, with no GPU libraries at all. If this crate ever links
//! `libvirglrenderer`, the project's central claim ("C needs no GPU") becomes quietly false.
//!
//! This is a test rather than a code review note because the failure is silent: adding
//! `rayland-engine` to `[dependencies]` compiles fine on a developer box that happens to have
//! virglrenderer installed, and only fails much later on the machine that matters.

/// The dependency tree of `rayland-vtest` must not contain `rayland-engine` (which FFI-links
/// `libvirglrenderer`), nor any crate that does.
///
/// # How it works
/// `cargo tree -p rayland-vtest` prints this crate's transitive dependencies. We assert
/// `rayland-engine` is absent. Failure mode: someone adds a convenience dependency and does not
/// realize it drags a GPU stack onto a machine that has none.
#[test]
fn rayland_vtest_does_not_depend_on_the_gpu_engine() {
    let out = std::process::Command::new(env!("CARGO"))
        .args(["tree", "-p", "rayland-vtest", "--prefix", "none"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree runs");
    let tree = String::from_utf8_lossy(&out.stdout);
    assert!(
        !tree.contains("rayland-engine"),
        "rayland-vtest must not depend on rayland-engine (it FFI-links libvirglrenderer, and \
         Rayland's C side must run on a machine with no GPU stack). cargo tree said:\n{tree}"
    );
}
```

- [ ] **Step 3: Run the test — expect FAIL (the crate does not build yet)**

Run: `cargo test -p rayland-vtest --test no_gpu_linkage`
Expected: FAIL — the crate has no `lib.rs` yet.

- [ ] **Step 4: Write `crates/rayland-vtest/src/lib.rs`**

Move the `RenderEngine` trait, `EngineFrame`, `BlobResource` and the module declarations out of `crates/rayland-engine/src/lib.rs` into it. The module header must say what this crate is and why it exists:

```rust
//! The Mesa **Venus vtest** wire protocol, and this repository's knowledge of Venus's command ring.
//!
//! # Why this crate exists separately from `rayland-engine`
//! `rayland-engine` FFI-links `libvirglrenderer` — a GPU stack. Rayland's **C** side (where the
//! *application* runs) speaks this protocol but must never need a GPU: C is by design the weak
//! machine, possibly a different CPU architecture, possibly headless. Splitting the protocol from
//! the GPU implementation is what lets `rayland-c` run there. `tests/no_gpu_linkage.rs` enforces it.
//!
//! The [`RenderEngine`] trait is the seam: `rayland-engine`'s `VirglEngine` implements it by
//! driving a real GPU, while (c)1's `RelayEngine` implements it by forwarding to another machine.

pub mod error;
pub mod transport;
pub mod venus_ring;
pub mod vtest;
```

- [ ] **Step 5: Shrink `rayland-engine` to the FFI, re-exporting the rest**

`crates/rayland-engine/src/lib.rs` keeps `ffi` and `virgl` and adds:

```rust
// `rayland-engine` was the whole of C0's engine; (c)1 split the protocol out into `rayland-vtest`
// so the C side can link this crate's *absence*. Re-exported so existing dependents (this crate's
// own tests, `examples/vtest_serve.rs`) keep their import paths.
pub use rayland_vtest::{error, transport, venus_ring, vtest, EngineError, EngineFrame, RenderEngine};
```

Add to `crates/rayland-engine/Cargo.toml`: `rayland-vtest = { path = "../rayland-vtest" }`. Remove `thiserror`/`libc` from it if they become unused (the compiler will say).

- [ ] **Step 6: Run everything**

```bash
cargo test -p rayland-vtest --test no_gpu_linkage   # expect PASS
cargo test --workspace                              # expect: same totals as before the split
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```
Expected: the 40 `rayland-engine` tests redistribute between the two crates; **the total count must not drop**. If a test vanished, find it — do not proceed.

- [ ] **Step 7: Commit**

```bash
git add -A crates/rayland-vtest crates/rayland-engine Cargo.toml
git commit -m "(c)1 Task 1: split rayland-vtest out of rayland-engine so C links no GPU code"
```

---

## Task 2: `rayland-relay` — the wire protocol

The messages that cross the network. Pure data + framing: no GPU, no network, no I/O. Fully unit-testable.

**Files:**
- Create: `crates/rayland-relay/Cargo.toml`, `crates/rayland-relay/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub enum C2S`, `pub enum S2C`, `pub fn write_msg<W: Write, M: Serialize>(w: &mut W, m: &M) -> Result<(), RelayError>`, `pub fn read_msg<R: Read, M: DeserializeOwned>(r: &mut R) -> Result<M, RelayError>`, `pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;`, `pub enum RelayError`.

- [ ] **Step 1: Create the crate**

`crates/rayland-relay/Cargo.toml`:

```toml
[package]
name = "rayland-relay"
version = "0.0.1"
edition = "2024"
rust-version = "1.85"
license = "LGPL-3.0-or-later"
description = "The Rayland (c)1 relay protocol: Venus ring deltas, blob syncs and replies over a network."

[dependencies]
serde = { workspace = true }
postcard = { workspace = true }
thiserror = { workspace = true }
```

Add `"crates/rayland-relay",` to workspace `members`.

- [ ] **Step 2: Write the failing round-trip test**

`crates/rayland-relay/src/lib.rs`, in `#[cfg(test)] mod tests`:

```rust
#[test]
fn ring_delta_round_trips_through_framing() {
    // A ring delta is the (c)1 payload that actually matters: the bytes Mesa wrote into the
    // command ring between two tails. Everything else is bookkeeping around it.
    let msg = C2S::RingDelta {
        ring_res_id: 1,
        tail: 4024,
        bytes: vec![0xb2, 0x00, 0x00, 0x00],
    };
    let mut buf = Vec::new();
    write_msg(&mut buf, &msg).expect("write");
    let got: C2S = read_msg(&mut buf.as_slice()).expect("read");
    assert_eq!(got, msg);
}

#[test]
fn oversized_frame_is_rejected_not_allocated() {
    // A hostile or corrupt length prefix must not become a multi-gigabyte allocation. `rayland-s`
    // reads these from the network; treating the length as trustworthy is a denial-of-service.
    let mut framed = Vec::new();
    framed.extend_from_slice(&(u32::MAX).to_le_bytes());
    framed.extend_from_slice(b"junk");
    let err = read_msg::<_, C2S>(&mut framed.as_slice()).expect_err("must reject");
    assert!(matches!(err, RelayError::FrameTooLarge { .. }));
}
```

- [ ] **Step 3: Run — expect FAIL**

Run: `cargo test -p rayland-relay`
Expected: FAIL — `C2S` not defined.

- [ ] **Step 4: Implement the messages**

```rust
/// Messages travelling **C → S**: the application's side of the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum C2S {
    /// Session opening. `vtest_protocol_version` is what our local vtest server negotiated with
    /// Mesa, so S can reject a mismatch loudly rather than misframing later.
    Hello { vtest_protocol_version: u32 },

    /// Create the Venus rendering context. Mirrors `VCMD_CONTEXT_INIT`.
    CreateContext { ctx_id: u32 },

    /// The Venus capability set the client asked for. S answers from the real GPU: C has no GPU
    /// and cannot invent this.
    GetCapset { version: u32 },

    /// A blob the client asked us to allocate. **C has already allocated its local memfd shadow**;
    /// this asks S to create the real resource so the GPU has something to read/write.
    CreateBlob { blob_mem: u32, blob_flags: u32, blob_id: u64, size: u64 },

    /// Contents of a mapped blob, C → S. (c)1 v1 ships the whole blob (spec §7): no dirty
    /// tracking. `offset` exists so a later version can ship ranges without a protocol change.
    BlobData { res_id: u32, offset: u64, bytes: Vec<u8> },

    /// New command-ring bytes: everything Mesa wrote in `[previous_tail, tail)`. **This is the
    /// payload the whole project is about** — the serialized Vulkan command stream.
    RingDelta { ring_res_id: u32, tail: u32, bytes: Vec<u8> },

    /// The doorbell (`vkNotifyRingMESA`). Carried for fidelity; S's ring thread may also just see
    /// the bytes. Never used as a *metric* — C0 proved the count is timing-dependent.
    NotifyRing { ring_id: u64, seqno: u32 },

    /// Release a resource. Mirrors `VCMD_RESOURCE_UNREF`.
    UnrefResource { res_id: u32 },
}

/// Messages travelling **S → C**: the GPU's side of the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum S2C {
    /// The real capset bytes from S's GPU, answering [`C2S::GetCapset`].
    Capset { bytes: Vec<u8> },

    /// The engine-assigned resource id for a [`C2S::CreateBlob`]. C maps this to its local shadow.
    BlobCreated { res_id: u32 },

    /// Contents of a blob **S wrote**: the reply arena the app blocks on, and the readback buffer
    /// the GPU renders into. Without this the app never sees its own pixels.
    BlobData { res_id: u32, offset: u64, bytes: Vec<u8> },

    /// S has replayed and retired everything up to this ring position. C does **not** need this to
    /// advance its local `head` (see Task 3's decision note) — it is carried for progress
    /// detection, which is what Task 3's timeout consults.
    RingProgress { ring_res_id: u32, consumed_tail: u32 },

    /// A typed failure on S. Sent rather than dropping the connection so C can log something a
    /// human can act on.
    Error { message: String },
}
```

Framing: a `u32` little-endian byte length followed by the postcard payload, length-checked against `MAX_FRAME_BYTES` **before allocating** (this is `rayland-wire`'s discipline — follow it).

- [ ] **Step 5: Run — expect PASS**

Run: `cargo test -p rayland-relay`
Expected: PASS, 2 tests.

- [ ] **Step 6: Lints + commit**

```bash
cargo clippy -p rayland-relay --all-targets -- -D warnings && cargo fmt --check
git add crates/rayland-relay Cargo.toml
git commit -m "(c)1 Task 2: the relay wire protocol (ring deltas, blob syncs, replies)"
```

---

## Task 3: `rayland-c` — the C-side daemon

The heart of (c)1's client half: be a vtest server, allocate blobs locally, watch the ring, relay.

**Files:**
- Create: `crates/rayland-c/Cargo.toml`, `src/main.rs`, `src/shm.rs`, `src/ring.rs`, `src/relay_engine.rs`
- Test: `crates/rayland-c/tests/ring_watch.rs`
- Modify: `Cargo.toml` (workspace members)

**Interfaces:**
- Consumes: `rayland_vtest::{serve_vtest, RenderEngine, EngineError, VtestTransport}`, `rayland_vtest::venus_ring::{RING_HEAD_OFFSET, RING_TAIL_OFFSET, RING_STATUS_OFFSET, RING_BUFFER_OFFSET}`, `rayland_relay::{C2S, S2C, write_msg, read_msg}`.
- Produces: `pub struct RelayEngine<T: RelayLink>` implementing `RenderEngine`; `pub trait RelayLink { fn send(&mut self, m: &C2S) -> Result<(), EngineError>; fn recv(&mut self) -> Result<S2C, EngineError>; }` (so Task 3 is testable with a mock and Task 6 plugs QUIC in); `pub struct RingWatcher`.

**DECISION — local `head` advance (not in the spec; recorded here):** `head` is host-written and the app reads it to know how much ring space is free. `rayland-c` advances the local `head` **as soon as it has relayed the bytes**, not when S reports consuming them. This makes C's ring a pure staging buffer and removes a round-trip per wrap. **The consequence must be understood:** backpressure then comes from QUIC's flow control, not from S's ring occupancy, so a slow S causes C's relay to block rather than the app to stall on a full ring. That is the intended behaviour, but it means **`S2C::RingProgress` is a liveness signal, not a flow-control one.**

- [ ] **Step 1: Create the crate**

```toml
[package]
name = "rayland-c"
version = "0.0.1"
edition = "2024"
rust-version = "1.85"
# Binary → GPL per CLAUDE.md's licensing policy.
license = "GPL-3.0-or-later"
publish = false
description = "Rayland's C-side daemon: a local vtest server that relays Venus's command ring to S."

[dependencies]
rayland-vtest = { path = "../rayland-vtest" }   # protocol only — NEVER rayland-engine (see Task 1)
rayland-relay = { path = "../rayland-relay" }
anyhow = { workspace = true }
libc = { workspace = true }
```

- [ ] **Step 2: Write the failing ring-watcher test (the load-bearing logic)**

`crates/rayland-c/tests/ring_watch.rs`:

```rust
//! The ring watcher is where (c)1 is most likely to hang intermittently, so it is tested against a
//! synthetic ring rather than only in a live drive where a stall looks like a network problem.

use rayland_c::ring::{RingWatcher, ParkDecision};
use rayland_vtest::venus_ring::{RING_BUFFER_OFFSET, RING_TAIL_OFFSET, RING_STATUS_OFFSET};

/// A tail that advanced must yield exactly the bytes written between the old and new tail.
#[test]
fn advancing_tail_yields_exactly_the_new_bytes() {
    let mut ring = vec![0u8; 131268];
    ring[RING_BUFFER_OFFSET..RING_BUFFER_OFFSET + 4].copy_from_slice(&[0xb2, 0, 0, 0]);
    write_u32(&mut ring, RING_TAIL_OFFSET, 4);

    let mut w = RingWatcher::new(1, 131072);
    let delta = w.take_delta(&ring).expect("a delta");
    assert_eq!(delta.tail, 4);
    assert_eq!(delta.bytes, vec![0xb2, 0, 0, 0]);

    // Draining twice must not re-send bytes: duplicate commands would be replayed on the GPU.
    assert!(w.take_delta(&ring).is_none(), "no new bytes, no delta");
}

/// THE HANG BUG THIS TEST EXISTS FOR: Mesa only sends a doorbell when the IDLE bit is set AND
/// >=1ms has passed since the last kick (`vn_ring.c:475-483`). So a kick is NOT guaranteed for
/// every write. A watcher that sets IDLE and sleeps unconditionally will miss work and stall.
/// It MUST re-read `tail` after publishing IDLE and stay awake if it changed.
#[test]
fn park_is_refused_when_tail_moved_after_idle_was_published() {
    let mut ring = vec![0u8; 131268];
    let mut w = RingWatcher::new(1, 131072);
    w.take_delta(&ring);

    // Simulate the race: the watcher publishes IDLE, and Mesa writes before it can sleep.
    w.publish_idle(&mut ring);
    write_u32(&mut ring, RING_TAIL_OFFSET, 8);

    assert_eq!(
        w.decide_park(&ring),
        ParkDecision::StayAwake,
        "tail moved after IDLE was published; parking here would sleep through pending work \
         because Mesa's >=1ms throttle may suppress the kick"
    );
    // And IDLE must be cleared again, or Mesa keeps paying for kicks we do not need.
    assert_eq!(read_u32(&ring, RING_STATUS_OFFSET) & 1, 0);
}

fn write_u32(b: &mut [u8], off: usize, v: u32) { b[off..off + 4].copy_from_slice(&v.to_le_bytes()); }
fn read_u32(b: &[u8], off: usize) -> u32 { u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) }
```

- [ ] **Step 3: Run — expect FAIL**

Run: `cargo test -p rayland-c --test ring_watch`
Expected: FAIL — `rayland_c::ring` does not exist.

- [ ] **Step 4: Implement `src/shm.rs`** — `memfd_create` + `mmap(MAP_SHARED)` of a given size, returning an owned fd + mapping. This is the same mechanics `rayland-vtest`'s GUEST blob path already implements (C0 Task 4a) — **read it and follow it**, especially the lifecycle: the fd may be closed after sending, the mapping must outlive the resource and be `munmap`ed exactly once.

- [ ] **Step 5: Implement `src/ring.rs`** — `RingWatcher { res_id, buffer_size, last_tail }` with:
  - `take_delta(&mut self, ring: &[u8]) -> Option<RingDelta>` — read `tail` at `RING_TAIL_OFFSET`, return `ring[RING_BUFFER_OFFSET + last_tail .. RING_BUFFER_OFFSET + tail]`, update `last_tail`. **Handle wrap** (`tail < last_tail` means it wrapped: emit the tail-to-end segment then the start-to-tail segment).
  - `publish_idle(&mut self, ring: &mut [u8])` — set bit 0 of `status`. **Polarity:** bit 0 set = *we are parked*; `0` = *we are actively polling*. (The repo documented this backwards until 2026-07-15.)
  - `decide_park(&mut self, ring: &[u8]) -> ParkDecision` — re-read `tail`; if it moved since `publish_idle`, clear IDLE and return `StayAwake`; else `Park`.
  - `advance_head(&mut self, ring: &mut [u8], upto: u32)` — write `head` once bytes are relayed (see the DECISION note above).

- [ ] **Step 6: Implement `src/relay_engine.rs`** — `RelayEngine<T: RelayLink>` implementing `RenderEngine`:
  - `create_venus_context` → `C2S::CreateContext`
  - `venus_capset` → `C2S::GetCapset`, block for `S2C::Capset` (C has no GPU; only S can answer)
  - `create_blob_resource` → allocate the local memfd shadow via `shm.rs`, send `C2S::CreateBlob`, await `S2C::BlobCreated`, return the fd to hand the client
  - `submit` → `C2S::RingDelta` (the inline `SUBMIT_CMD2` path; the ring path goes through `ring.rs`)
  - `read_back` → **`unimplemented` is forbidden.** Return a typed `EngineError` saying host-side readback is S's job and C never has pixels.

- [ ] **Step 7: Implement `src/main.rs`** — bind `/tmp/rl-c1.sock`, accept one connection, `serve_vtest(&mut stream, &mut relay_engine)`, and run the ring watcher. Add the **progress-aware timeout** (spec §6.2): if `tail` has advanced but no `S2C::RingProgress` has arrived within `RAYLAND_C1_STALL_TIMEOUT` (default 30s), log and exit non-zero. **This must exist before anyone sets `VN_DEBUG=no_abort`** — Mesa's watchdog reports ALIVE without consulting ring state, so disabling it without this replaces a 3.5s abort with an 895-second hang.

- [ ] **Step 8: Run — expect PASS**

Run: `cargo test -p rayland-c` → 2 tests PASS. Then `cargo clippy -p rayland-c --all-targets -- -D warnings && cargo fmt --check`.

- [ ] **Step 9: Commit**

```bash
git add crates/rayland-c Cargo.toml
git commit -m "(c)1 Task 3: rayland-c — local vtest server, memfd blobs, ring watcher, RelayEngine"
```

---

## Task 4: `rayland-s` — apply relay messages to the engine

**Files:**
- Create: `crates/rayland-s/Cargo.toml`, `src/main.rs`, `src/apply.rs`
- Test: `crates/rayland-s/tests/apply.rs`
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: `rayland_engine::{VirglEngine, virgl_available}`, `rayland_vtest::RenderEngine`, `rayland_relay::{C2S, S2C}`.
- Produces: `pub fn apply(engine: &mut dyn RenderEngine, msg: C2S) -> Result<Vec<S2C>, EngineError>`.

- [ ] **Step 1: Write the failing test** using C0's existing `RenderEngine` test double (`rayland-vtest`'s `vtest.rs` tests already contain one — reuse it, do not write a second):

```rust
/// A ring delta must reach the engine as a `submit` of exactly those bytes — no reframing, no
/// padding. The bytes are a Venus command stream; a single dword of drift corrupts every
/// subsequent command.
#[test]
fn ring_delta_becomes_a_verbatim_submit() {
    let mut engine = RecordingEngine::default();
    let out = apply(&mut engine, C2S::RingDelta { ring_res_id: 1, tail: 4, bytes: vec![1, 2, 3, 4] })
        .expect("applies");
    assert_eq!(engine.submits, vec![(1u32, vec![1u8, 2, 3, 4])]);
    assert!(matches!(out.as_slice(), [S2C::RingProgress { consumed_tail: 4, .. }]));
}
```

- [ ] **Step 2: Run — expect FAIL.** Run: `cargo test -p rayland-s --test apply`

- [ ] **Step 3: Implement `apply`** — one match arm per `C2S` variant, each mapping to the `RenderEngine` call C0 already built, and returning the `S2C` messages it owes. **Every `EngineError` becomes an `S2C::Error` with a human-readable message; none is swallowed.**

- [ ] **Step 4: Run — expect PASS.** Then clippy + fmt.

- [ ] **Step 5: Commit**

```bash
git add crates/rayland-s Cargo.toml
git commit -m "(c)1 Task 4: rayland-s — apply relay messages to the virglrenderer engine"
```

---

## Task 5: Blob sync + out-of-line detection

Without this the app's vertex buffer never reaches S and the triangle is undefined.

**Files:**
- Modify: `crates/rayland-c/src/relay_engine.rs`, `crates/rayland-s/src/apply.rs`
- Test: `crates/rayland-c/tests/blob_sync.rs`, `crates/rayland-vtest/src/vtest.rs` (out-of-line rejection test)

- [ ] **Step 1: Write the failing tests**

```rust
/// C0 Task 4b caught the refapp's vertex buffer (res=3, 64 bytes) decoding float-for-float out of
/// a mapped blob. The app writes it with a plain memcpy and no API call, so if we do not ship it
/// before the GPU reads, the triangle renders from uninitialized memory.
#[test]
fn dirty_blob_is_shipped_before_the_ring_delta_that_may_read_it() { /* ordering assertion */ }

/// Commands over direct_size (8192) are replaced in-ring by vkExecuteCommandStreamsMESA
/// (opcode 180) pointing at OTHER shmems. (c)1 v1 does not implement that path. It must therefore
/// REFUSE it in a way a human can act on — decoding past it would corrupt the stream and present
/// as inexplicable GPU misbehaviour far from the cause.
#[test]
fn out_of_line_command_stream_is_a_typed_error_not_a_guess() {
    let err = decode_submit_cmd2(&submit_with_opcode(180)).expect_err("must refuse");
    assert!(matches!(err, EngineError::OutOfLineStreamUnsupported { .. }));
}
```

- [ ] **Step 2: Run — expect FAIL.**

- [ ] **Step 3: Implement.** In `RelayEngine`: before sending any `C2S::RingDelta`, send `C2S::BlobData` for every mapped blob shadow (v1 ships **all** of them, whole — spec §7; no dirty tracking). On `S2C::BlobData`, copy into the local shadow so the app sees replies and pixels. In `rayland-vtest`, add the `OutOfLineStreamUnsupported` variant and the opcode-180 check.

- [ ] **Step 4: Run — expect PASS.** clippy + fmt.

- [ ] **Step 5: Commit**

```bash
git commit -am "(c)1 Task 5: conservative blob sync both ways + typed refusal of out-of-line streams"
```

---

## Task 6: QUIC wiring + loopback end-to-end

**Files:**
- Create: `crates/rayland-c/src/link.rs` (impl `RelayLink` over `rayland-transport`)
- Modify: `crates/rayland-c/src/main.rs`, `crates/rayland-s/src/main.rs`
- Test: `crates/rayland-s/tests/loopback_e2e.rs`

- [ ] **Step 1: Write the GPU-gated loopback e2e test.** Gate on `virgl_available()`; skip cleanly otherwise (copy `crates/rayland-engine/tests/refapp_venus_e2e.rs`'s gating idiom exactly). It must: start `rayland-s` on `127.0.0.1`, start `rayland-c` on `/tmp/rl-c1.sock`, run `rayland-refapp` with the Task 0 environment, and assert the app's PNG is **centre red, corners blue**. It must **panic if the app never connects** — a test that passes when nothing happened is worse than no test.

- [ ] **Step 2: Run — expect FAIL.**

- [ ] **Step 3: Implement `RelayLink` over `rayland-transport`'s synchronous stream adapters (SP2).** Reuse them; do not write new QUIC code.

- [ ] **Step 4: Run — expect PASS.** Then run it **10 times** (`--test-threads=1`): reliability is a first-class requirement and this is the first time a network sits in the path.

- [ ] **Step 5: Commit**

```bash
git commit -am "(c)1 Task 6: QUIC link + loopback end-to-end (refapp renders through the relay)"
```

---

## Task 7: `rayland-present` + presenting the readback blob

**Files:**
- Create: `crates/rayland-present/` (extracted from `crates/rayland-server/src/window.rs`)
- Create: `crates/rayland-s/src/present.rs`
- Modify: `crates/rayland-server/src/{main.rs,window.rs}`, `crates/rayland-s/src/main.rs`, `Cargo.toml`

- [ ] **Step 1: Extract `window.rs` into `rayland-present` (LGPL)**, and have `rayland-server` depend on it. **`cargo test --workspace` must stay green** — SP1/SP3's tests are the regression net for this move.

- [ ] **Step 2: Present from the readback blob (spec §7.1).** S cannot see the app's `DEVICE_LOCAL` render target (C0 Task 4b: it produces **no blob at all**). It *can* see the readback blob — C0 caught it as `res=6`, 16384 B = 64×64×4, holding the clear colour. So `present.rs` takes that blob's bytes and pushes them through `rayland-present`'s **`wl_shm`** path. **Comment must state plainly that this is not zero-copy and why** (dmabuf-exporting a resource requires seeing the resource), and that b2 expires the shortcut.
  Identify the blob by size (`width * height * 4`) and log the choice; if two candidates match, **error rather than guess**.

- [ ] **Step 3: Verify on the real GPU + compositor** — the triangle appears in a window on `dop561`. This is a human check; record what you saw.

- [ ] **Step 4: Commit**

```bash
git add crates/rayland-present crates/rayland-s Cargo.toml
git commit -m "(c)1 Task 7: extract rayland-present; present the readback blob via wl_shm"
```

---

## Task 8: Two-machine bring-up (appollo → dop561)

**Files:** Create `scripts/c1-two-machine.sh`

- [ ] **Step 1: Install the client stack on appollo.** `ssh appollo.localdomain` (key-based, passwordless sudo; **do not break it**). It needs Mesa 26 with the Venus ICD (`/usr/share/vulkan/icd.d/virtio_icd.json`) and the `rayland-c` binary. **It needs no GPU and no Wayland.** Record exactly what you installed.

- [ ] **Step 2: Run it.** `rayland-s` on dop561, `rayland-c` + refapp on appollo, with:
```bash
VN_DEBUG=vtest,no_abort \
VN_PERF=no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback \
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json \
VTEST_SOCKET_NAME=/tmp/rl-c1.sock \
env -u VK_LOADER_DRIVERS_SELECT ./rayland-refapp /tmp/out.png
```
**Only set `no_abort` if Task 3's progress timeout is in place.**

- [ ] **Step 3: The correctness assertion (spec §10.2).** Compare venus-from-appollo against `rayland-refapp` run natively **on dop561** — both are the **same Intel GPU**, so assert **bit-identity**. Do **not** compare against appollo-native: that is an AMD render and means nothing.

- [ ] **Step 4: Write `scripts/c1-two-machine.sh`** with the verified commands, and **run the script** — a documented command that does not work is the exact bug C0 shipped and a reviewer caught.

- [ ] **Step 5: Commit**

```bash
git add scripts/c1-two-machine.sh
git commit -m "(c)1 Task 8: two-machine bring-up (appollo -> dop561)"
```

---

## Task 9: Measurement + docs — the deliverable that answers the question

**This is not instrumentation added at the end. It is what makes (c)1 evidence instead of a demo.**

**Files:**
- Create: `crates/rayland-c/src/metrics.rs`, `docs/c1-the-network.md`
- Modify: `crates/rayland-c/src/{main.rs,link.rs,relay_engine.rs}`

- [ ] **Step 1: Count things.** In `metrics.rs`: round-trips (any send that blocks for a reply), bytes each way **split by channel** (ring / replies / blob sync), and wall-clock to first frame. Print a summary at exit behind `RAYLAND_C1_METRICS=1`.

- [ ] **Step 2: Sweep simulated WAN.** On appollo:
```bash
sudo tc qdisc add dev <iface> root netem delay 20ms    # then 50ms, then 100ms
sudo tc qdisc del dev <iface> root                     # ALWAYS clean up — do not leave appollo crippled
```
Record round-trips, bytes, and time-to-frame at 0/20/50/100 ms RTT. Find where it becomes unusable.

- [ ] **Step 3: Write `docs/c1-the-network.md`** — house style of `docs/c0-venus-first-light.md`; readable by a non-expert; **the measurement table is the centrepiece**. Compare against the spec's §8.1 predictions: steady state should be *bandwidth*-bound (Venus is async by design), startup RTT-bound but one-off, the return path ~12× the command path.
  **If a prediction failed, say so loudly** — a failed prediction is the most valuable result this project can produce, and burying it would waste the whole exercise.
  State plainly what (c)1 did **not** prove (spec §13): not arbitrary apps, not a real Wayland app, not that performance is acceptable — only what it *is*.

- [ ] **Step 4: Commit**

```bash
git add crates/rayland-c/src/metrics.rs docs/c1-the-network.md
git commit -m "(c)1 Task 9: measurement harness, WAN sweep, and the (c)1 verdict doc"
```

---

## Self-Review

**1. Spec coverage** — every spec section maps to a task:
- §1 success criterion (refapp on C → QUIC → S's GPU → window on S; two independent checks; reliability) → Tasks 6, 7, 8.
- §2 why the old scope is dead → context only; no task needed.
- §3 scope/non-goals (not b2, not general coherence, not zero-copy, not RISC-V, no Mesa fork) → respected throughout; the zero-copy loss is documented in Task 7 Step 2.
- §4 architecture (topology, the cut, `RenderEngine` seam, data flow, the `status` bit) → Tasks 1, 3.
- §4.5 double-check-before-park → **Task 3 Step 2's second test** (the named hang bug).
- §5 channel inventory → ring (T3), replies (T5), feedback (disabled, Global Constraints), blobs (T5), out-of-line (T5 typed refusal).
- §6 crutch table → Global Constraints + Task 8 Step 2; §6.2's ordering constraint → Task 3 Step 7.
- §7 coherence + §7.1 presentation source → Tasks 5, 7.
- §8 measurement → Task 9.
- §9 crate structure → Tasks 1, 2, 3, 4, 7.
- §10 testing → Tasks 1, 2, 3, 4, 6 (loopback CI), 8 (manual two-machine).
- §11 deferred (b2, milkv) → no tasks, correctly.
- §12 open questions → Q1 (`no_multi_ring`) Task 8 Step 2; Q2 (kick throttle) Task 3; Q5 (fence timeout) Task 6; Q3/Q4/Q6 remain open and are stated as such in Task 9's doc.

**2. Placeholder scan** — no TBDs. Tasks 5–8 give prose + exact commands rather than full listings where the work is mechanical (a file move, a `match` arm per enum variant already defined in Task 2); every step where the *logic* is novel — the ring watcher, the framing, the linkage guard — carries real code. The one deliberate omission is Task 5 Step 1's test bodies, which assert an ordering already fully specified in Step 3.

**3. Type consistency** — `RenderEngine`/`EngineError`/`EngineFrame` (Task 1) are used by `RelayEngine` (Task 3) and `apply` (Task 4); `C2S`/`S2C` (Task 2) are used by Tasks 3–6; `RelayLink` (Task 3) is implemented in Task 6; `RingWatcher`/`ParkDecision` (Task 3) appear only there. `venus_ring`'s `RING_*_OFFSET` constants come from Task 1's move and are consumed in Task 3.

**Note for the executor — this plan is larger than C0's and its risk is concentrated in two places.** Task 3's ring watcher is where an intermittent hang will live if the park/kick discipline is wrong (and the repo documented the `status` polarity *backwards* until 2026-07-15 — re-read `venus_ring/mod.rs`, not your memory). Task 8 is the first time a real network is in the path, and the first honest test of whether any of this works. If Task 6's loopback e2e cannot be made to pass, **stop and report** — that is a real finding about the design, not a bug to grind on.
