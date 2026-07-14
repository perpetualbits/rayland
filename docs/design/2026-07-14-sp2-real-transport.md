# SP2 — Real Transport

**Date:** 2026-07-14
**Status:** Sub-project design spec (awaiting owner review)
**Parent design:** [`2026-07-13-native-remote-wayland-gpu.md`](2026-07-13-native-remote-wayland-gpu.md)
**Predecessors:** [`2026-07-13-sp0-first-light.md`](2026-07-13-sp0-first-light.md), [`2026-07-14-sp1-onto-the-screen.md`](2026-07-14-sp1-onto-the-screen.md) (both complete, merged)

---

## 1. Purpose and the single success criterion

SP0 proved a command stream replays on S's GPU; SP1 put the result in a live Wayland window
on S. Both ran over a plain TCP socket on `localhost`. SP2 replaces that transport with
**QUIC over a real network**, so the client (**C**) can run on a **different machine and a
different CPU architecture** from the server (**S**).

SP2's **one new hard thing** is *real, encrypted, cross-machine transport*. Rendering (SP0)
and on-screen presentation (SP1) are reused unchanged. The headline demonstration: a triangle
**emitted on the Milk-V Mars rv64 single-board computer** appears in a window on the laptop's
GPU, across the LAN — the weak, foreign-architecture C driving the strong S that the whole
project exists to enable.

**Success criterion (measurable + observable):**

1. *Machine-verified:* an automated **QUIC loopback end-to-end test** — a server and a client
   as two tasks on `127.0.0.1`, talking QUIC — asserts the returned frame's pixels (centre
   red, corners blue), and **all SP0/SP1 tests remain green**.
2. *Human-observed (documented manual milestones):*
   - (i) On one machine, the server shows the triangle in a window, fed over QUIC from a
     local client.
   - (ii) **Cross-machine:** with the server on the laptop (S) and the client run on
     **`milkv.localdomain` (rv64)** and separately on **`apollo.localdomain`**, the triangle
     appears in the window on S — proving the transport works across machines and
     architectures.

## 2. Scope — what SP2 is, and is not

SP2 **is**: swap the TCP socket for a single bidirectional **QUIC** stream carrying the
existing length-prefixed `postcard` command messages, encrypted with TLS, over a real
network, with the client buildable and runnable on the rv64 SBC.

SP2 is deliberately **NOT** (each deferred to a named later sub-project):

- **No multi-stream sibling protocol** — one bidirectional QUIC stream, the same
  `Hello…EndFrame` framing as SP0/SP1. The control/command/memory/asset/media split is
  **SP3**.
- **No zero-copy dmabuf** — presentation still goes GPU → readback → `wl_shm` as in SP1.
  Zero-copy is **SP3**.
- **No real authentication** — the TLS channel is **encrypted but unauthenticated** (the
  client accepts any certificate). SSH-bootstrap ("mosh for GPU"), certificate trust, and
  sandboxing are **SP4**.
- **No adaptive/congestion policy** — QUIC's built-in congestion control is used as-is; the
  RTT-adaptive LAN↔WAN degradation policy is **SP4**.
- **No cross-*compilation* toolchain work** — the client is built **natively on each target**
  (it has no GPU dependency), not cross-compiled from the laptop.
- **No audio** — later track.

## 3. Architecture: async confined to one new crate

The core insight is to **quarantine async**. `quinn` (QUIC) is async and pulls in a `tokio`
runtime; everything else in Rayland is synchronous (`std::io`) and, on S, driven by a
`calloop` event loop (SP1). SP2 introduces a new library crate whose entire job is to hide
QUIC's async behind **synchronous** stream adapters, so no existing sync code is rewritten.

```
   C side (client)                         S side (server)
 ┌────────────────────┐   QUIC/UDP over    ┌───────────────────────────────────────────────┐
 │ rayland-client     │   a real network   │ rayland-server                                │
 │  connect() ──────► │ ═════════════════► │  accept() → sync Read of command stream       │
 │  sync Read+Write   │  (TLS-encrypted,   │  handle_connection → RenderedFrame (SP0)      │
 │  send_triangle     │   one bidi stream) │  run_window (SP1) shows it on S's GPU/display │
 │  wait_until_closed  │ ◄──── close ────  │  liveness: tokio task pings the calloop loop  │
 └────────────────────┘                    │           on QUIC close; loop-drop closes conn │
                                           └───────────────────────────────────────────────┘
        ↑ tokio runtime (background threads) drives QUIC on BOTH sides; the main
          thread stays synchronous and, on S, runs the calloop window loop.
```

### 3.1 New crate: `crates/rayland-transport` (library, LGPL-3.0-or-later)

Owns every async/QUIC/TLS dependency so nothing leaks into the other crates. Dependencies:
`quinn` (QUIC), `rustls` with the **`rustls-rustcrypto`** pure-Rust crypto provider, `rcgen`
(self-signed cert), and `tokio` (runtime, confined here). It does **not** depend on
`rayland-wire`: the transport carries opaque bytes, and the message framing stays in the
callers (`rayland-client`/`rayland-server`), so the transport and the protocol evolve
independently. Public surface (names indicative; finalized in the plan):

- **Client:** `connect(server_addr) -> anyhow::Result<QuicStream>`, where `QuicStream`
  implements **both `std::io::Read` and `std::io::Write`** over a single bidirectional QUIC
  stream. `send_triangle(&mut stream, …)` and `wait_until_closed(&mut stream)` work over it
  **unchanged**.
- **Server:** `listen(bind_addr) -> anyhow::Result<QuicListener>` and
  `QuicListener::accept() -> anyhow::Result<(QuicRecv, Liveness)>`, where `QuicRecv`
  implements **`std::io::Read`** (the incoming command stream, fed to the existing
  `handle_connection`) and `Liveness` is the transport-agnostic disconnect/close handle
  described in §3.3.
- A **`dangerous_insecure`** module: the rustls "accept any server certificate" verifier used
  by the client. Its module- and type-level doc-comments state bluntly that it disables
  authentication, is for SP2's skeleton only, must never ship, and is replaced by SP4. The
  name is deliberately alarming so it cannot be used by accident.

### 3.2 The synchronous bridge

`quinn`'s `SendStream`/`RecvStream` are async. The adapters wrap them with a handle to the
confined `tokio` runtime and call `runtime_handle.block_on(...)` inside each `read`/`write`.

- The **runtime runs on its own (background) threads**; the **sync consumer runs on a
  different, non-runtime thread** (the process main thread). `block_on` on a runtime `Handle`
  from a non-runtime thread blocks only the caller while the runtime's workers keep driving
  the UDP/QUIC IO — no deadlock. (Calling `block_on` from *within* a runtime worker thread
  would panic; the design keeps the sync consumer off those threads, and this constraint is
  documented at the bridge.)
- `RecvStream::read` returning "stream finished" maps to `Ok(0)` from `std::io::Read::read`,
  i.e. **EOF**. This is what makes `handle_connection`'s read loop terminate on `EndFrame`
  and `wait_until_closed` return when the peer closes — identical semantics to TCP.
- Write errors and stream resets map to `std::io::Error` so the sync callers' existing error
  handling applies.

### 3.3 Liveness and the calloop seam (preserving SP1's "close on either")

SP1's `run_window` watched the raw TCP file descriptor for EOF. QUIC has no single readable
fd (it is multiplexed over one UDP socket driven by tokio), so liveness is re-plumbed through
a **transport-agnostic signal**:

- A background tokio task awaits the QUIC **connection's `closed()`** future. When the client
  disconnects, it fires a **`calloop` channel/ping** that the window loop already watches →
  the loop sets its exit flag → the window closes. (This replaces SP1's `Generic<TcpStream>`
  fd source.)
- The window loop holds a **close-guard**; when the loop ends (window closed by the user),
  dropping the guard **closes the QUIC connection**, so the client's `QuicStream` read hits
  EOF and the client exits.

Both directions of SP1's "close on either" are preserved. `run_window` is **generalized**
from taking a concrete `TcpStream` to taking a `RenderedFrame` plus a small `Liveness`
abstraction (a calloop event source that fires on remote disconnect + a close action on
drop). This is a modest, behavior-preserving refactor of SP1's window module; the `--png`
path is unaffected.

### 3.4 Where the tokio runtime lives

One `tokio` runtime is created per process (client and server each). On the server it is
started before `accept()`, kept alive for the process lifetime, and its worker threads drive
QUIC while the main thread runs `handle_connection` (via the blocking bridge) and then the
calloop window loop. On the client it drives QUIC while the main thread runs `send_triangle`
and `wait_until_closed` (via the bridge). The runtime is an implementation detail of
`rayland-transport`; the binaries do not touch `tokio` directly.

## 4. Client (C side): cross-architecture by construction

The client performs **no GPU work** — it only builds and sends the command stream — so it has
**no `ash`/Vulkan dependency** and nothing that resists a foreign architecture. In SP2 it
gains only a dependency on `rayland-transport`. It is built **natively on each target
machine** (`ssh milkv.localdomain` / `ssh apollo.localdomain`, then `cargo build -p
rayland-client`), not cross-compiled — which avoids cross-toolchain setup entirely. The
**pure-Rust `rustls-rustcrypto` provider** is the enabler: it has no C/assembly, so it
compiles on rv64 (and any other target) without a system crypto library or cross toolchain.

## 5. Security posture (minimal; SP4 owns real security)

At startup the server generates an **ephemeral self-signed certificate** with `rcgen`. The
client uses the `dangerous_insecure` verifier to accept it without checking. The QUIC channel
is therefore **encrypted but not authenticated** — it resists passive eavesdropping but not
an active man-in-the-middle. This is the honest minimum for a transport skeleton and requires
**nothing to be provisioned on `milkv`/`apollo`**, keeping the cross-machine test frictionless.
SP4 replaces this with SSH-bootstrapped trust and real certificate verification. The
insecurity is contained in the loudly-named module of §3.1 so it can never be mistaken for
production behavior.

## 6. De-risking spike (the FIRST task in the plan)

`quinn` historically obtained its QUIC packet-protection keys from `ring`/`aws-lc-rs`; whether
the pure-Rust `rustls-rustcrypto` provider can drive quinn's QUIC crypto is the single biggest
unknown. So the plan's **first task is a minimal loopback spike**: stand up a `quinn` server
and client on `127.0.0.1` using `rustls` + `rustls-rustcrypto` + an `rcgen` self-signed cert
and the `dangerous_insecure` verifier, open one bidirectional stream, send a few bytes, and
assert they arrive. 

- **If it works:** the pure-Rust provider is confirmed and the rest of SP2 builds on it.
- **If it does not:** fall back to building **`ring` or `aws-lc-rs` natively on the SBC**
  (both support `riscv64gc-unknown-linux-gnu`), keeping pure-Rust as the goal and native SBC
  build as the escape hatch. The spike's outcome is recorded before further work proceeds.

This spike gates everything after it; it is cheap and removes the project's largest risk up
front.

## 7. Testing strategy

- **New: QUIC loopback end-to-end test** (headless, deterministic, CI-friendly). A server and
  a client run as two tasks/threads on an ephemeral `127.0.0.1` UDP port over QUIC; the client
  sends the triangle; the test asserts the server-side `RenderedFrame` pixels (centre red,
  corners blue), exactly like SP0's TCP e2e but over the real transport. `rustls-rustcrypto`
  is pure Rust, so this builds and runs on CI with **no system crypto libraries**.
- **Kept unchanged and green:** all SP0/SP1 unit tests (`pack_xrgb8888`, `wait_until_closed`,
  `handle_connection`, wire round-trips, the render pixel test) on both a real GPU and
  lavapipe. The existing TCP e2e test stays as a pure sync-path check (it exercises
  `handle_connection` over a socket and does not depend on QUIC).
- **Manual milestones (§1.2):** the on-screen local QUIC run and the cross-machine
  `milkv`/`apollo` runs, documented step-by-step in `docs/sp2-real-transport.md` including how
  to build and run the client on each target and how to point it at the server's LAN address.

### 7.1 CI note (carrying SP1's lesson)

The SP1 CI break came from a dependency's **default features** pulling a build-time system
library (`xkbcommon` via pkg-config). SP2 must apply the same discipline to its new deps:
choose `rustls`'s crypto provider explicitly (**`rustls-rustcrypto`**, pure Rust) and set
`default-features = false` where a default would pull `aws-lc-rs`/`ring` (which need a C
toolchain). Verify with `cargo tree` that no C-crypto crate is in the graph. CI stays light —
no system crypto or Wayland libraries required to build and run the automated tests.

## 8. Error handling and dependencies

- **Libraries** (`rayland-transport`, `rayland-wire`) use `thiserror`; **binaries**
  (`rayland-server`, `rayland-client`) use `anyhow` with contextual messages. No
  `unwrap()`/`expect()` on runtime-fallible paths (asserts guarding documented caller-bug
  invariants are allowed; `expect` in tests is allowed).
- New dependencies are confined to `rayland-transport`: `quinn`, `rustls` +
  `rustls-rustcrypto`, `rcgen`, `tokio`. The other crates gain at most a dependency on
  `rayland-transport`. Licenses: `rayland-transport` is a library → **LGPL-3.0-or-later**;
  binaries stay **GPL-3.0-or-later**; `rayland-wire` stays **LGPL-3.0-or-later**.

## 9. Definition of done

- The spike (§6) has run and its outcome (pure-Rust provider works, or the fallback taken) is
  recorded.
- `cargo test` passes locally (real GPU) and in CI (lavapipe), including the QUIC loopback
  e2e and all inherited SP0/SP1 tests.
- `cargo clippy --workspace -- -D warnings` clean; `cargo fmt` applied.
- Every function has a doc-block; every non-trivial line has a value-adding comment; code and
  comments agree; the `dangerous_insecure` module is loudly documented.
- The server binary listens on QUIC and shows the streamed triangle in a window; the client
  binary connects over QUIC. Closing the window exits the client; killing the client closes
  the window (SP1 semantics preserved over QUIC).
- **Cross-machine milestone:** the client, built and run natively on `milkv.localdomain`
  (rv64) and on `apollo.localdomain`, drives the window on the laptop over the LAN.
- `docs/sp2-real-transport.md` documents the local and cross-machine run steps.

## 10. Refinements to confirm at review

1. **Async confined to `rayland-transport` via a `block_on` sync bridge.** Keeps SP0/SP1's
   sync code and the calloop window unchanged; the alternative (full async end-to-end) was a
   large rewrite for little SP2 benefit.
2. **`rustls-rustcrypto` (pure Rust), spike-gated, with native-SBC `ring`/`aws-lc-rs` as the
   fallback.** Prioritizes the rv64 client build; the spike removes the integration risk up
   front.
3. **Single bidirectional QUIC stream, same `postcard` framing.** The multi-stream sibling
   protocol is SP3's charter, kept out of SP2.
4. **Encrypted-but-unauthenticated TLS with a loudly-named skip-verify.** Real authentication
   is SP4; the container keeps the insecurity from ever being mistaken for production.

Everything else follows the parent design and `CLAUDE.md`.
