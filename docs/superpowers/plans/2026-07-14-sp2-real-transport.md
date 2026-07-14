# SP2 — Real Transport — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the localhost TCP socket with QUIC over a real network, in a new `rayland-transport` crate that hides QUIC's async behind synchronous stream adapters, so the client can run on a different machine and architecture (rv64) while SP0/SP1's sync rendering and calloop window are reused unchanged.

**Architecture:** A new library crate `rayland-transport` owns all async/QUIC/TLS (`quinn`, `rustls` + a pure-Rust crypto provider, `rcgen`, a confined `tokio` runtime) and exposes **synchronous** `Read`/`Write` stream adapters plus a `Liveness` handle. The client connects and reuses `send_triangle`/`wait_until_closed` unchanged; the server accepts, feeds the sync stream to `handle_connection`, and presents via a generalized `run_window`. A single bidirectional QUIC stream carries the same `postcard` framing. TLS is encrypted-but-unauthenticated (ephemeral self-signed cert + a loudly-named skip-verify).

**Tech Stack:** Rust edition 2024; `quinn` 0.11, `rustls` 0.23 with `rustls-rustcrypto` (pure-Rust crypto; `ring` fallback), `rcgen` 0.14, `tokio` 1; existing `ash`/`image`/`smithay-client-toolkit` unchanged.

## Global Constraints

Copied verbatim from the spec and `CLAUDE.md`; every task implicitly includes these.

- **Edition:** `edition = "2024"`, `rust-version = "1.85"` on every crate manifest.
- **Comments:** doc-comment block (`///`/`//!`) on every function/type/module; intent comment on every **non-trivial** line explaining the *why*, never restating syntax; trivial lines get none; code and comments must always agree.
- **Errors:** libraries (`rayland-transport`, `rayland-wire`) use `thiserror`; binaries use `anyhow` with contextual messages. No `unwrap()`/`expect()` on runtime-fallible paths in non-test code (`expect` in tests fine; asserts guarding documented caller-bug invariants fine).
- **Licenses:** `rayland-transport` is a library → `LGPL-3.0-or-later`; `rayland-server`/`rayland-client` binaries → `GPL-3.0-or-later`; `rayland-wire` → `LGPL-3.0-or-later`.
- **ALPN:** both endpoints use the exact ALPN protocol id `b"rayland-sp2"` (must match, or the handshake fails).
- **Crypto provider:** primary is the **pure-Rust `rustls-rustcrypto`** provider (so the rv64 client builds with no C/asm). If Task 1's spike shows quinn's QUIC crypto cannot use it, fall back to `rustls`'s `ring` provider and **record the decision in the ledger**; later tasks use whichever Task 1 selected. Build every `rustls` config with `builder_with_provider(...)` (explicit provider — never rely on a process-default).
- **The skip-verify is loud:** the client's certificate verifier lives in a module named `dangerous_insecure`, whose module- and type-level docs state bluntly that it disables authentication, is SP2-skeleton-only, must never ship, and is replaced by SP4.
- **CI discipline (SP1 lesson):** audit new deps' default features for build-time system-lib pulls. `rustls-rustcrypto` is pure Rust; ensure `rustls` does not pull `aws-lc-rs`/`ring` C crypto unless the fallback is taken. Verify with `cargo tree`.
- **Verify against cargo, not the IDE.** rust-analyzer diagnostics can lag mid-edit; trust `cargo build`/`test`/`clippy`.
- **This host filters software Vulkan globally.** If a GPU-backed test can't find a device, re-run that command prefixed with `VK_LOADER_DRIVERS_SELECT='*lvp*'`.

---

## File Structure

- `crates/rayland-transport/` — **new library crate.** `Cargo.toml`, `src/lib.rs`, `src/tls.rs` (cert gen + configs + `dangerous_insecure`), `src/sync_stream.rs` (the `block_on` Read/Write adapters + `Liveness`), `src/lib.rs` (public `connect`/`listen`/`accept`, the confined runtime).
- `Cargo.toml` (workspace) — **modify.** Add the member + `[workspace.dependencies]` entries.
- `crates/rayland-server/src/window.rs` — **modify.** Generalize `run_window` from `TcpStream` to a generic disconnect source.
- `crates/rayland-server/src/main.rs` — **modify.** Listen/accept over QUIC; feed `handle_connection`; present via `run_window`; keep `--png`.
- `crates/rayland-server/Cargo.toml` — **modify.** Add `rayland-transport` dep + dev-deps for the e2e test.
- `crates/rayland-client/src/main.rs` — **modify.** Connect over QUIC; `send_triangle` + `wait_until_closed` unchanged.
- `crates/rayland-client/Cargo.toml` — **modify.** Add `rayland-transport` dep.
- `crates/rayland-server/tests/quic_e2e.rs` — **new.** QUIC loopback pixel-assertion test.
- `docs/sp2-real-transport.md` — **new.** Local + cross-machine run steps.

---

## Task 1: `rayland-transport` foundation + QUIC loopback spike (DE-RISK FIRST)

This is both the de-risking spike (§6 of the spec) and the transport foundation. It stands up a quinn server + client on localhost using the pure-Rust crypto provider and proves a bidirectional byte round-trip. **Its outcome selects the crypto provider for all later tasks.**

**Files:**
- Create: `crates/rayland-transport/Cargo.toml`, `crates/rayland-transport/src/lib.rs`, `crates/rayland-transport/src/tls.rs`
- Modify: `Cargo.toml` (workspace)

**Interfaces:**
- Produces (used by Task 2): `tls::server_quic_config() -> anyhow::Result<quinn::ServerConfig>` and `tls::client_quic_config() -> anyhow::Result<quinn::ClientConfig>` (both using the selected provider, ALPN `b"rayland-sp2"`, the self-signed cert / skip-verify).

- [ ] **Step 1: Add the crate to the workspace**

In the root `Cargo.toml`, add `"crates/rayland-transport",` to `members`, and under `[workspace.dependencies]` add:

```toml
quinn = "0.11"                                          # QUIC transport (async)
rustls = { version = "0.23", default-features = false } # TLS; provider chosen explicitly (no default C-crypto)
rustls-rustcrypto = "0.0.2-alpha"                       # pure-Rust rustls CryptoProvider (rv64-friendly)
rcgen = "0.14"                                          # self-signed certificate generation
tokio = { version = "1", features = ["rt-multi-thread", "sync", "time", "macros"] } # confined async runtime
```

- [ ] **Step 2: Create the crate manifest**

`crates/rayland-transport/Cargo.toml`:

```toml
# The Rayland transport: hides QUIC's async behind synchronous stream adapters. A LIBRARY
# crate, so per policy it is LGPL-3.0-or-later.
[package]
name = "rayland-transport"                                                     # workspace/crates.io package name
version = "0.0.1"                                                              # pre-release; SP2 walking skeleton
edition = "2024"                                                              # Rust 2024 edition, per repo convention
rust-version = "1.85"                                                         # minimum toolchain for edition 2024
description = "Rayland transport: synchronous stream adapters over a QUIC connection (SP2)." # crates.io/tooling purpose
license = "LGPL-3.0-or-later"                                                 # library crate → LGPL per repo policy
repository = "https://github.com/perpetualbits/rayland"                       # upstream source location

[dependencies]
quinn = { workspace = true }              # QUIC endpoints, connections, streams
rustls = { workspace = true }             # TLS configuration underlying QUIC
rustls-rustcrypto = { workspace = true }  # pure-Rust crypto provider (rv64-friendly)
rcgen = { workspace = true }              # ephemeral self-signed certificate
tokio = { workspace = true }              # the confined async runtime that drives QUIC
thiserror = { workspace = true }          # precise library error type
anyhow = { workspace = true }             # contextual errors for setup paths returned to binaries
```

- [ ] **Step 3: Write the TLS/config module with the loud skip-verify**

Create `crates/rayland-transport/src/tls.rs`. This holds cert generation, both quinn configs, and the `dangerous_insecure` verifier. **Provider note:** the code below uses `rustls_rustcrypto::provider()`. If Step 6 shows quinn cannot use it, change the two `provider()` calls (and the verifier's provider) to `rustls::crypto::ring::default_provider()` and add `ring` to `rustls`'s features — see Step 6.

```rust
//! TLS configuration for the SP2 QUIC transport: an ephemeral self-signed server certificate
//! and a deliberately-insecure client that accepts any certificate.
//!
//! SP2's security posture is **encrypted but not authenticated** (see the design spec §5):
//! the QUIC channel is protected against passive eavesdropping, but the client does not verify
//! the server's identity. Real authentication (SSH-bootstrap, certificate trust) is SP4. The
//! insecurity is confined to [`dangerous_insecure`] so it cannot be used by accident.

// The ALPN protocol identifier both ends must agree on; a mismatch fails the handshake.
const ALPN: &[u8] = b"rayland-sp2";

/// Build the QUIC server configuration: a fresh self-signed certificate and a rustls server
/// config wrapped for QUIC.
///
/// The certificate is generated per process (`rcgen`) and never persisted — it exists only to
/// satisfy TLS's requirement that the server present a certificate; it is not a trust anchor.
///
/// # Errors
/// Returns an error if certificate generation, the rustls config, or the QUIC wrapping fails.
pub fn server_quic_config() -> anyhow::Result<quinn::ServerConfig> {
    use std::sync::Arc;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    // Generate a self-signed certificate for "localhost" (the name is not checked by the
    // client in SP2, but a valid cert must still be presented).
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    // Extract the DER-encoded certificate and its PKCS#8 private key.
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());

    // Build the rustls server config with the pure-Rust provider (explicit, not process-default).
    let mut server_crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls_rustcrypto::provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![cert_der], PrivateKeyDer::Pkcs8(key_der))?;
    // Advertise our ALPN so the client's matching ALPN completes the negotiation.
    server_crypto.alpn_protocols = vec![ALPN.to_vec()];

    // Wrap the rustls config for QUIC; this fails if the provider lacks a QUIC cipher suite.
    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?;
    // The final quinn server configuration.
    Ok(quinn::ServerConfig::with_crypto(Arc::new(quic_crypto)))
}

/// Build the QUIC client configuration: the insecure verifier plus our ALPN.
///
/// # Errors
/// Returns an error if the rustls config or the QUIC wrapping fails.
pub fn client_quic_config() -> anyhow::Result<quinn::ClientConfig> {
    use std::sync::Arc;

    // Build the rustls client config with the pure-Rust provider and the accept-anything
    // verifier from `dangerous_insecure`.
    let mut client_crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls_rustcrypto::provider(),
    ))
    .with_safe_default_protocol_versions()?
    .dangerous()
    .with_custom_certificate_verifier(dangerous_insecure::SkipServerVerification::new())
    .with_no_client_auth();
    // Match the server's ALPN.
    client_crypto.alpn_protocols = vec![ALPN.to_vec()];

    // Wrap for QUIC (same QUIC-suite requirement as the server side).
    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)?;
    // The final quinn client configuration.
    Ok(quinn::ClientConfig::new(Arc::new(quic_crypto)))
}

/// The deliberately-insecure certificate verifier. **DO NOT SHIP.**
///
/// Every method here accepts whatever the server presents without checking identity. This
/// disables TLS authentication entirely — it protects only confidentiality, not against a
/// man-in-the-middle. It exists solely so SP2 can prove the transport without the SP4
/// certificate-trust machinery, and MUST be replaced before Rayland is used for anything real.
pub mod dangerous_insecure {
    use std::sync::Arc;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};

    /// A `ServerCertVerifier` that accepts ANY certificate. Insecure by design; see the module
    /// docs. It still delegates *signature* verification to a real crypto provider so the TLS
    /// handshake's math is valid — only the *identity* check is skipped.
    #[derive(Debug)]
    pub struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

    impl SkipServerVerification {
        /// Construct the verifier, wrapping the pure-Rust provider used for signature checks.
        pub fn new() -> Arc<Self> {
            // Wrap the same provider the configs use, for signature-scheme support.
            Arc::new(Self(Arc::new(rustls_rustcrypto::provider())))
        }
    }

    impl ServerCertVerifier for SkipServerVerification {
        /// Accept the server certificate unconditionally (identity check skipped).
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            // No identity verification: assert the cert is "verified" regardless.
            Ok(ServerCertVerified::assertion())
        }

        /// Verify a TLS 1.2 handshake signature using the real provider (math still checked).
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        /// Verify a TLS 1.3 handshake signature using the real provider (math still checked).
        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        /// Report the signature schemes the underlying provider supports.
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
}
```

- [ ] **Step 4: Write the crate root with a confined runtime and the spike test**

Create `crates/rayland-transport/src/lib.rs`:

```rust
//! Rayland transport (SP2): synchronous stream adapters over a QUIC connection.
//!
//! QUIC (via [`quinn`]) is asynchronous and needs a [`tokio`] runtime; the rest of Rayland is
//! synchronous. This crate quarantines all of that: it owns the runtime and the QUIC/TLS
//! stack and will expose blocking `Read`/`Write` adapters (Task 2) so the existing synchronous
//! client/server code is reused unchanged. This file (Task 1) establishes the crate, the TLS
//! configuration ([`tls`]), and proves a QUIC handshake + byte round-trip on localhost.

// TLS configuration and the loud insecure verifier.
pub mod tls;

#[cfg(test)]
mod spike_tests {
    use super::tls;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    // The QUIC loopback spike: stand up a server and client on localhost with the pure-Rust
    // provider, open one bidirectional stream, and round-trip bytes. If this fails to compile
    // or the handshake/`try_from` fails, the pure-Rust provider cannot drive quinn — take the
    // `ring` fallback (see the plan's Task 1, Step 6) and re-run.
    #[test]
    fn quic_loopback_round_trips_bytes() {
        // A dedicated multi-thread runtime drives QUIC for the duration of the test.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime builds");

        rt.block_on(async {
            // Bind the server to an ephemeral localhost UDP port.
            let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let endpoint_server = quinn::Endpoint::server(
                tls::server_quic_config().expect("server config"),
                server_addr,
            )
            .expect("server endpoint");
            // Learn the actual bound port so the client can reach it.
            let addr = endpoint_server.local_addr().expect("server local addr");

            // Server task: accept one connection, accept one bi stream, echo a reply.
            let server = tokio::spawn(async move {
                let incoming = endpoint_server.accept().await.expect("incoming");
                let conn = incoming.await.expect("connection");
                let (mut send, mut recv) = conn.accept_bi().await.expect("accept bi");
                // Read the client's greeting (bounded read).
                let got = recv.read_to_end(64).await.expect("read greeting");
                assert_eq!(got, b"ping");
                // Reply, then finish the send side.
                send.write_all(b"pong").await.expect("write reply");
                send.finish().expect("finish");
                // Keep the connection alive briefly so the client can read the reply.
                conn.closed().await;
            });

            // Client: connect, open a bi stream, send "ping", read "pong".
            let mut endpoint_client =
                quinn::Endpoint::client(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                    .expect("client endpoint");
            endpoint_client.set_default_client_config(tls::client_quic_config().expect("client config"));
            let conn = endpoint_client
                .connect(addr, "localhost")
                .expect("connect starts")
                .await
                .expect("connected");
            let (mut send, mut recv) = conn.open_bi().await.expect("open bi");
            send.write_all(b"ping").await.expect("write ping");
            send.finish().expect("finish");
            let reply = recv.read_to_end(64).await.expect("read reply");
            assert_eq!(reply, b"pong");

            // Close the connection so the server task ends, then join it.
            conn.close(0u32.into(), b"done");
            server.await.expect("server task");
        });
    }
}
```

- [ ] **Step 5: Run the spike**

Run: `cargo test -p rayland-transport --lib spike_tests::quic_loopback_round_trips_bytes -- --nocapture`
Expected (success): PASS — the pure-Rust provider drives a full QUIC handshake and byte round-trip.

- [ ] **Step 6: Decision gate — record the provider outcome**

- **If Step 5 PASSED:** the pure-Rust provider works. Note in your report: "provider = rustls-rustcrypto (pure Rust)". Proceed.
- **If Step 5 FAILED** (compile error referencing missing QUIC suites, or a runtime panic from `QuicServerConfig::try_from`/`QuicClientConfig::try_from` about unsupported cipher suites): take the fallback. Change `rustls` in the workspace `Cargo.toml` to `rustls = { version = "0.23", features = ["ring"] }`, and in `tls.rs` replace the three `rustls_rustcrypto::provider()` calls with `rustls::crypto::ring::default_provider()`. Re-run Step 5; it must now PASS. Note in your report: "provider = ring (pure-Rust provider incompatible with quinn's QUIC crypto)". This is an expected, planned outcome — not a failure of the task.

If neither provider yields a passing spike, STOP and report BLOCKED with the exact errors — do not proceed to Task 2.

- [ ] **Step 7: Lints and commit**

Run: `cargo clippy -p rayland-transport -- -D warnings` and `cargo fmt`. Both clean.
Run: `cargo test --workspace` — the new spike test passes and no existing test regressed. (Use the `VK_LOADER_DRIVERS_SELECT='*lvp*'` prefix if a GPU test can't find a device.)

```bash
git add crates/rayland-transport Cargo.toml
git commit -m "SP2 Task 1: rayland-transport foundation + QUIC loopback spike (provider: <rustcrypto|ring>)"
```

---

## Task 2: Synchronous stream adapters + `connect`/`listen`/`accept`

Wrap the async QUIC streams in blocking `Read`/`Write` adapters backed by the confined runtime, and expose the public sync API. This is what lets `send_triangle`/`wait_until_closed`/`handle_connection` run unchanged over QUIC.

**Files:**
- Create: `crates/rayland-transport/src/sync_stream.rs`
- Modify: `crates/rayland-transport/src/lib.rs` (add the runtime + public API)

**Interfaces:**
- Consumes: `tls::server_quic_config`, `tls::client_quic_config` (Task 1).
- Produces (used by Tasks 3/4):
  - `connect(server_addr: SocketAddr) -> anyhow::Result<QuicStream>` — client side; `QuicStream: std::io::Read + std::io::Write` over one bidirectional stream.
  - `listen(bind_addr: SocketAddr) -> anyhow::Result<QuicListener>`; `QuicListener::accept(&self) -> anyhow::Result<(QuicRecv, Liveness)>` — server side; `QuicRecv: std::io::Read`; `Liveness: std::os::fd::AsFd` with `impl std::io::Read for &Liveness` and a `Drop` that closes the connection.

- [ ] **Step 1: Write the sync adapters**

Create `crates/rayland-transport/src/sync_stream.rs`:

```rust
//! Blocking `Read`/`Write` adapters over asynchronous `quinn` streams.
//!
//! Each adapter holds a handle to the confined tokio runtime (which drives QUIC on its own
//! threads) and calls `block_on` per operation from the caller's synchronous thread. Because
//! the caller runs on a NON-runtime thread while the runtime's workers keep driving the
//! connection, `block_on` blocks only the caller — no deadlock. Calling these from inside a
//! runtime worker thread would panic; callers must stay on their own thread.

// Standard IO traits we implement.
use std::io::{Read, Write};
// The pipe read-end that signals disconnect, and fd borrowing for calloop.
use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
// The confined runtime shared by all adapters.
use tokio::runtime::Runtime;

/// A synchronous, bidirectional stream over one QUIC bi-stream (the client's view).
pub struct QuicStream {
    // Keeps the runtime alive for the stream's lifetime; drives the async reads/writes.
    rt: Arc<Runtime>,
    // The QUIC send half.
    send: quinn::SendStream,
    // The QUIC receive half.
    recv: quinn::RecvStream,
}

impl QuicStream {
    /// Wrap a QUIC bi-stream pair with the runtime handle.
    pub(crate) fn new(rt: Arc<Runtime>, send: quinn::SendStream, recv: quinn::RecvStream) -> Self {
        Self { rt, send, recv }
    }
}

impl Read for QuicStream {
    /// Block until some bytes arrive; a finished stream reads as end-of-file (`Ok(0)`).
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Drive the async read to completion on the runtime.
        match self.rt.block_on(self.recv.read(buf)) {
            // Some bytes were read.
            Ok(Some(n)) => Ok(n),
            // The stream finished cleanly: report EOF so sync readers terminate.
            Ok(None) => Ok(0),
            // A read error (reset, connection lost) becomes an io::Error.
            Err(e) => Err(std::io::Error::other(e)),
        }
    }
}

impl Write for QuicStream {
    /// Block until the bytes are accepted by the QUIC stream.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // `write` returns how many bytes were accepted.
        self.rt
            .block_on(self.send.write(buf))
            .map_err(std::io::Error::other)
    }
    /// QUIC streams are not user-buffered here, so flush is a no-op success.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The server's synchronous view of the incoming command stream (receive half only).
pub struct QuicRecv {
    // Keeps the runtime alive and drives the reads.
    rt: Arc<Runtime>,
    // The QUIC receive half of the client's bi-stream.
    recv: quinn::RecvStream,
}

impl QuicRecv {
    /// Wrap a QUIC receive stream with the runtime handle.
    pub(crate) fn new(rt: Arc<Runtime>, recv: quinn::RecvStream) -> Self {
        Self { rt, recv }
    }
}

impl Read for QuicRecv {
    /// Same semantics as [`QuicStream::read`]: finished stream → EOF.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.rt.block_on(self.recv.read(buf)) {
            Ok(Some(n)) => Ok(n),
            Ok(None) => Ok(0),
            Err(e) => Err(std::io::Error::other(e)),
        }
    }
}

/// A liveness handle for the server's window loop.
///
/// It bundles two things the SP1 window loop needs, now transport-agnostic:
/// - an fd ([`AsFd`]) that reaches **end-of-file when the client disconnects**, so the calloop
///   loop can watch it exactly as SP1 watched the TCP socket fd (a background task closes the
///   pipe's write end when the QUIC connection closes);
/// - a `Drop` that **closes the QUIC connection**, so when the window is closed and the loop
///   drops this handle, the client's stream reaches EOF and the client exits.
///
/// This preserves SP1's "close on either" teardown over QUIC.
pub struct Liveness {
    // Keeps the runtime alive so the background close-watcher task keeps running.
    _rt: Arc<Runtime>,
    // The QUIC connection; closed on drop (window-close → client sees EOF).
    conn: quinn::Connection,
    // The read end of a pipe; reaches EOF when the watcher closes the write end on disconnect.
    disconnect: File,
}

impl Liveness {
    /// Build a liveness handle: spawn a task that closes `pipe_write` when `conn` closes, and
    /// keep the pipe read end for the caller to watch. `disconnect` must be non-blocking.
    pub(crate) fn new(
        rt: Arc<Runtime>,
        conn: quinn::Connection,
        disconnect: File,
        pipe_write: File,
    ) -> Self {
        // Watch the connection for close and signal the pipe by dropping its write end.
        let watch_conn = conn.clone();
        rt.spawn(async move {
            // Resolves when the peer (client) disconnects or the connection otherwise closes.
            watch_conn.closed().await;
            // Dropping the write end makes the read end report EOF to the window loop.
            drop(pipe_write);
        });
        Self { _rt: rt, conn, disconnect }
    }
}

impl AsFd for Liveness {
    /// Expose the disconnect pipe's read end so the window loop can poll it.
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.disconnect.as_fd()
    }
}

impl Read for &Liveness {
    /// Read from the disconnect pipe (used by the window loop's calloop callback). Returns
    /// `Ok(0)` (EOF) once the client has disconnected.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // `&File` implements Read; delegate to the pipe read end.
        (&self.disconnect).read(buf)
    }
}

impl Drop for Liveness {
    /// Close the QUIC connection when the window loop drops us (window closed → client EOF).
    fn drop(&mut self) {
        // Application close code 0 with a short reason; the client observes this as EOF.
        self.conn.close(0u32.into(), b"window closed");
    }
}
```

- [ ] **Step 2: Add the public `connect`/`listen`/`accept` API and the runtime**

In `crates/rayland-transport/src/lib.rs`, add below `pub mod tls;`:

```rust
// The synchronous stream adapters.
pub mod sync_stream;

// Public re-exports so callers write `rayland_transport::QuicStream`, etc.
pub use sync_stream::{Liveness, QuicListener, QuicRecv, QuicStream};

// Standard networking and fd types.
use std::net::SocketAddr;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::sync::Arc;
use tokio::runtime::Runtime;

/// Build the confined multi-thread tokio runtime that drives QUIC.
///
/// # Errors
/// Returns an error if the runtime cannot be created.
fn build_runtime() -> anyhow::Result<Arc<Runtime>> {
    // A multi-thread runtime so its workers drive QUIC while the caller blocks on `block_on`.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    Ok(Arc::new(rt))
}

/// Connect to a Rayland server over QUIC and return a synchronous bidirectional stream.
///
/// # Errors
/// Returns an error if the runtime, endpoint, TLS config, or connection fails.
pub fn connect(server_addr: SocketAddr) -> anyhow::Result<QuicStream> {
    // Own a runtime for this connection.
    let rt = build_runtime()?;
    // Everything QUIC happens inside the runtime.
    let (send, recv) = rt.block_on(async {
        // Bind an ephemeral local UDP port for the client endpoint.
        let mut endpoint = quinn::Endpoint::client(SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            0,
        ))?;
        // Install our insecure client config.
        endpoint.set_default_client_config(tls::client_quic_config()?);
        // Connect; the server name is unchecked by our verifier but must be syntactically valid.
        let conn = endpoint.connect(server_addr, "localhost")?.await?;
        // Open the single bidirectional command stream.
        let bi = conn.open_bi().await?;
        anyhow::Ok(bi)
    })?;
    // Wrap the async streams in the sync adapter.
    Ok(QuicStream::new(rt, send, recv))
}
```

Add to `sync_stream.rs` the `QuicListener` (kept here so all quinn wrapping lives together):

```rust
/// A QUIC listener that accepts one client connection at a time.
pub struct QuicListener {
    // The runtime driving the endpoint; shared into accepted connections' adapters.
    rt: Arc<Runtime>,
    // The bound QUIC server endpoint.
    endpoint: quinn::Endpoint,
}

impl QuicListener {
    /// Wrap a bound server endpoint.
    pub(crate) fn new(rt: Arc<Runtime>, endpoint: quinn::Endpoint) -> Self {
        Self { rt, endpoint }
    }

    /// Accept one connection and its bidirectional command stream; return a synchronous reader
    /// for the stream and a [`Liveness`] handle for the window loop.
    ///
    /// # Errors
    /// Returns an error if accepting the connection or the bi-stream fails, or if the disconnect
    /// pipe cannot be created.
    pub fn accept(&self) -> anyhow::Result<(QuicRecv, Liveness)> {
        // Accept the connection and its first bi-stream on the runtime.
        let (conn, recv) = self.rt.block_on(async {
            // Wait for an incoming connection and finish its handshake.
            let incoming = self
                .endpoint
                .accept()
                .await
                .ok_or_else(|| anyhow::anyhow!("endpoint closed before a connection arrived"))?;
            let conn = incoming.await?;
            // Accept the client's single command bi-stream; we only read from it.
            let (_send, recv) = conn.accept_bi().await?;
            anyhow::Ok((conn, recv))
        })?;

        // Create a non-blocking pipe: the read end is watched by the window loop; the write end
        // is closed by Liveness's background task when the connection closes.
        let (read_fd, write_fd) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK)?;
        // Convert the OwnedFds into std Files for convenient Read + AsFd.
        let disconnect = File::from(read_fd);
        let pipe_write = File::from(write_fd);

        // Build the reader and liveness handle sharing this listener's runtime.
        let quic_recv = QuicRecv::new(self.rt.clone(), recv);
        let liveness = Liveness::new(self.rt.clone(), conn, disconnect, pipe_write);
        Ok((quic_recv, liveness))
    }
}
```

Add `listen` to `lib.rs`:

```rust
/// Bind a QUIC server endpoint on `bind_addr`.
///
/// # Errors
/// Returns an error if the runtime, TLS config, or endpoint binding fails.
pub fn listen(bind_addr: SocketAddr) -> anyhow::Result<QuicListener> {
    // Own a runtime for the server.
    let rt = build_runtime()?;
    // Bind the server endpoint inside the runtime.
    let endpoint = rt.block_on(async {
        anyhow::Ok(quinn::Endpoint::server(tls::server_quic_config()?, bind_addr)?)
    })?;
    Ok(QuicListener::new(rt, endpoint))
}
```

Add `rustix` to `rayland-transport`'s deps (`Cargo.toml`): `rustix = { version = "1", features = ["pipe"] }` — pure Rust bindings, already in the tree via SCTK; used only to make a non-blocking pipe. Remove the unused `FromRawFd`/`IntoRawFd` import if the compiler flags it (the `File::from(OwnedFd)` conversions above need only `std::fs::File`).

- [ ] **Step 3: Sync round-trip + EOF test**

Add to `crates/rayland-transport/src/sync_stream.rs` an integration-style test module (or a `tests/` file) that uses the **public sync API** to prove a round-trip and that closing the server side yields EOF on the client:

```rust
#[cfg(test)]
mod sync_tests {
    use crate::{connect, listen};
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[test]
    fn sync_api_round_trips_and_signals_disconnect() {
        // Bind the server on an ephemeral localhost port.
        let listener = listen(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .expect("listen");
        // Discover the bound address for the client.
        let addr = listener_local_addr(&listener);

        // Server thread: accept, read the client's bytes to EOF, then drop liveness (closing conn).
        let server = std::thread::spawn(move || {
            let (mut recv, liveness) = listener.accept().expect("accept");
            // Read everything the client sends until it half-closes its send side.
            let mut got = Vec::new();
            recv.read_to_end(&mut got).expect("read to end");
            // Dropping `liveness` here closes the connection (window-close analogue).
            drop(liveness);
            got
        });

        // Client: connect, send bytes, finish the send side so the server sees EOF.
        let mut stream = connect(addr).expect("connect");
        stream.write_all(b"hello quic").expect("write");
        // Signal end of the client's send stream so the server's read_to_end returns.
        finish_send(&mut stream);

        // The server received exactly what we sent.
        let got = server.join().expect("server thread");
        assert_eq!(got, b"hello quic");

        // After the server dropped liveness, the client's read reaches EOF.
        let mut tail = Vec::new();
        stream.read_to_end(&mut tail).expect("client reads to EOF after server close");
    }
}
```

The two helpers (`listener_local_addr`, `finish_send`) require exposing the bound address and a way to finish the client's send half. Add to the public API as needed:
- `QuicListener::local_addr(&self) -> std::io::Result<SocketAddr>` delegating to `self.endpoint.local_addr()`.
- `QuicStream::finish(&mut self) -> std::io::Result<()>` calling `self.rt.block_on(async { self.send.finish() })` (quinn's `finish` is sync-returning but marks the stream finished; wrap for symmetry) — document that finishing the send half is how the client signals "no more commands", used by `wait_until_closed`'s peer.

(Adjust the test to call `listener.local_addr()` and `stream.finish()` directly instead of the placeholder helpers.)

- [ ] **Step 4: Run tests, lints, commit**

Run: `cargo test -p rayland-transport` — the spike test and the new sync test pass.
Run: `cargo clippy -p rayland-transport -- -D warnings`, `cargo fmt --check` — clean.
Run: `cargo tree -p rayland-transport | grep -iE "aws-lc|ring"` — confirm no C-crypto crate unless the Task 1 fallback was taken.

```bash
git add crates/rayland-transport
git commit -m "SP2 Task 2: synchronous QUIC stream adapters + connect/listen/accept + Liveness"
```

---

## Task 3: Generalize `run_window` to a transport-agnostic disconnect source

SP1's `run_window` took a concrete `TcpStream`. Generalize it so it accepts anything providing a watchable fd that reaches EOF on disconnect — both `TcpStream` (still used until Task 4) and the QUIC `Liveness`. This is a small, behavior-preserving refactor.

**Files:**
- Modify: `crates/rayland-server/src/window.rs`

**Interfaces:**
- Produces: `pub fn run_window<S>(frame: RenderedFrame, disconnect: S) -> anyhow::Result<()> where S: std::os::fd::AsFd, for<'a> &'a S: std::io::Read` — draws `frame` in a window and runs the calloop loop, exiting when the window is closed OR `disconnect` reaches EOF; on return it **drops `disconnect` by value** (for the QUIC `Liveness`, that Drop closes the connection). The `for<'a> &'a S: Read` bound (rather than `S: Read`) is required because calloop's `Generic` source callback only exposes the fd object through a shared reference.

- [ ] **Step 1: Change the signature and the fd source**

In `crates/rayland-server/src/window.rs`, locate the current `pub fn run_window(frame: RenderedFrame, stream: TcpStream) -> anyhow::Result<()>`. Change it to be generic. The two substantive edits:

1. Signature and doc:

```rust
/// Open a Wayland window showing `frame`, and keep it up until the window is closed or the
/// remote peer disconnects — whichever comes first.
///
/// `disconnect` is any source that (a) provides a file descriptor via [`AsFd`] and (b) reaches
/// end-of-file when the peer disconnects. SP1 passed a `TcpStream`; SP2 passes a QUIC
/// `Liveness`. The source MUST already be non-blocking. On return, `disconnect` is dropped,
/// which — for the QUIC `Liveness` — closes the connection so the client also exits.
///
/// # Errors
/// Returns an error if the compositor is unreachable, a required global is missing, buffer
/// allocation fails, or the event loop errors.
pub fn run_window<S>(frame: RenderedFrame, disconnect: S) -> anyhow::Result<()>
where
    S: std::os::fd::AsFd,
    for<'a> &'a S: std::io::Read,
{
```

The bound is `for<'a> &'a S: Read` (not `S: Read`) because calloop's `Generic` source hands the fd object back to the callback only through a shared reference (`&NoIoDrop<S>`, which derefs to `&S`); reading therefore goes through `&S`. Both `TcpStream` and the QUIC `Liveness` implement `Read for &Self`.

2. The calloop source registration currently does `stream.set_nonblocking(true)?` and moves the `TcpStream` into `Generic::new(stream, ...)`, reading via `&TcpStream` in the callback. Replace with the generic source. Remove the `set_nonblocking` call (the source is required non-blocking by contract — `TcpStream` callers set it before calling; `Liveness`'s pipe is created non-blocking). Register `Generic::new(disconnect, Interest::READ, Mode::Level)` and, in the callback, read through `&S`:

```rust
|_readiness, source, state: &mut RaylandWindow| {
    // `source` is &mut NoIoDrop<S>; deref to &S and read through it (both TcpStream and
    // Liveness implement `Read for &Self`). EOF (Ok(0)) means the peer disconnected.
    let mut reader: &S = source;
    let mut sink = [0u8; 256];
    loop {
        match reader.read(&mut sink) {
            Ok(0) => {
                state.exit = true;
                return Ok(PostAction::Remove);
            }
            Ok(_) => continue,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(PostAction::Continue);
            }
            Err(_) => {
                state.exit = true;
                return Ok(PostAction::Remove);
            }
        }
    }
}
```

Note: the caller in `main.rs` still passes a `TcpStream` at this point; `TcpStream` satisfies `AsFd` and `for<'a> &'a TcpStream: Read`, and it must be set non-blocking by `main` before the call. Add `stream.set_nonblocking(true)?;` in `main.rs` right before the existing `run_window(frame, stream)` call so behavior is unchanged. (Task 4 replaces this path with QUIC.)

- [ ] **Step 2: Build and verify nothing regressed**

Run: `cargo build -p rayland-server` — compiles (the `for<'a> &'a S: Read` bound is satisfied by `TcpStream`).
Run: `cargo clippy --workspace -- -D warnings`, `cargo fmt --check` — clean.
Run: `cargo test --workspace` — all SP0/SP1 tests still pass (this refactor changes no behavior; `run_window` is not exercised by tests, and `main` still uses TCP). Use the `VK_LOADER_DRIVERS_SELECT='*lvp*'` prefix if needed.

- [ ] **Step 3: Commit**

```bash
git add crates/rayland-server/src/window.rs crates/rayland-server/src/main.rs
git commit -m "SP2 Task 3: generalize run_window to any AsFd + Read disconnect source"
```

---

## Task 4: Swap the binaries to QUIC + loopback e2e + docs

Replace TCP with QUIC in both binaries, add the QUIC pixel-assertion e2e test, and document local + cross-machine runs.

**Files:**
- Modify: `crates/rayland-server/src/main.rs`, `crates/rayland-server/Cargo.toml`
- Modify: `crates/rayland-client/src/main.rs`, `crates/rayland-client/Cargo.toml`
- Create: `crates/rayland-server/tests/quic_e2e.rs`
- Create: `docs/sp2-real-transport.md`

**Interfaces:**
- Consumes: `rayland_transport::{connect, listen}`, `QuicListener::accept`, `handle_connection`, `run_window`, `send_triangle`, `wait_until_closed`.

- [ ] **Step 1: Client binary over QUIC**

Add to `crates/rayland-client/Cargo.toml` under `[dependencies]`: `rayland-transport = { path = "../rayland-transport" }`.

Rewrite `crates/rayland-client/src/main.rs` to connect over QUIC (default `127.0.0.1:9000` as a UDP address), sending and waiting exactly as before:

```rust
//! Rayland client binary: connect to a server over QUIC, send the triangle stream, and hold
//! the connection open so the server's window stays on screen until it (or we) closes.

// The library functions that build and drain the command stream.
use rayland_client::{send_triangle, wait_until_closed};
// The QUIC transport connect entry point.
use rayland_transport::connect;

/// Connect to the server address given as the first CLI argument (default `127.0.0.1:9000`),
/// send one triangle at 256×256 on a blue background, then block until the server closes the
/// connection (which it does when its window is closed).
///
/// # Errors
/// Returns an error if the address is invalid, or the connection, send, or wait fails.
fn main() -> anyhow::Result<()> {
    // Resolve and parse the server address (a UDP socket address for QUIC).
    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9000".to_string());
    let server_addr: std::net::SocketAddr = address
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid server address {address:?}: {e}"))?;

    // Open the QUIC connection (returns a synchronous Read+Write stream).
    let mut stream = connect(server_addr)?;
    // Send the triangle command stream, exactly as over TCP.
    send_triangle(&mut stream, 256, 256, [0.0, 0.0, 1.0, 1.0])?;
    // Report and hold the connection open as a liveness channel.
    println!("sent triangle to {address} over QUIC; holding the connection until the window closes");
    // Returns when the server closes the connection (its window was closed).
    wait_until_closed(&mut stream)?;
    // The server closed the connection: we are done.
    println!("server closed the connection; exiting");
    Ok(())
}
```

Note: `connect` returns `QuicStream`, which is `Read + Write`; `send_triangle<W: Write>` and `wait_until_closed<R: Read>` accept it unchanged. The client sends its `EndFrame` message but does **not** call `finish()` — it keeps the send side open so the connection stays alive for liveness, matching SP1.

- [ ] **Step 2: Server binary over QUIC**

Add to `crates/rayland-server/Cargo.toml` under `[dependencies]`: `rayland-transport = { path = "../rayland-transport" }`. Under `[dev-dependencies]` add (for the e2e test): `rayland-transport = { path = "../rayland-transport" }` is already a normal dep, so tests see it; keep the existing `rayland-client` dev-dep.

Rewrite `crates/rayland-server/src/main.rs` to listen over QUIC. The `--png` flag and address parsing stay; the transport changes from `TcpListener` to `rayland_transport::listen`:

```rust
//! Rayland server binary: accept one QUIC connection, render it on the GPU, and either show
//! the result in a live Wayland window (default) or write it to a PNG (`--png <path>`).

// The connection handler and the window presenter from the library.
use rayland_server::handle_connection;
use rayland_server::window::run_window;
// The QUIC transport listener.
use rayland_transport::listen;

/// Run the server: bind a QUIC endpoint, accept one connection, render the streamed frame,
/// then present it (window by default, or `--png <path>` to write a PNG and exit).
///
/// The first positional argument is the listen address (default `127.0.0.1:9000`).
///
/// # Errors
/// Returns an error if the address is invalid, or binding, accepting, rendering, PNG writing,
/// or window presentation fails.
fn main() -> anyhow::Result<()> {
    // Collect args, scanning for `--png <path>` and the positional address (same as SP1).
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut png_path: Option<String> = None;
    let mut address: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--png" => {
                let path = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--png requires a path argument"))?;
                png_path = Some(path.clone());
                i += 2;
            }
            other => {
                if address.is_none() {
                    address = Some(other.to_string());
                }
                i += 1;
            }
        }
    }
    let address = address.unwrap_or_else(|| "127.0.0.1:9000".to_string());
    // Parse the UDP socket address QUIC binds to.
    let bind_addr: std::net::SocketAddr = address
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address {address:?}: {e}"))?;

    // Bind the QUIC endpoint and announce readiness.
    let listener = listen(bind_addr)?;
    println!("rayland-server listening on {address} (QUIC)");

    // Accept exactly one connection; get the sync command reader and the liveness handle.
    let (mut recv, liveness) = listener.accept()?;
    println!("connection accepted");

    // Replay the stream on the GPU into a CPU-side frame.
    let frame = handle_connection(&mut recv)?;

    // Present: PNG fallback, or a live window watching `liveness` for client disconnect.
    match png_path {
        Some(path) => {
            // Headless path: encode the RGBA8 pixels as a PNG. Dropping `liveness` closes the
            // connection so the client exits.
            image::save_buffer(&path, &frame.pixels, frame.width, frame.height, image::ColorType::Rgba8)?;
            println!("wrote {path} ({}x{})", frame.width, frame.height);
            drop(liveness);
        }
        None => {
            // Default path: show the frame until the window or the client closes. `liveness`
            // is moved in BY VALUE; when the window loop ends, run_window drops it, which
            // closes the QUIC connection so the client also exits.
            println!("presenting in a window; close it (or stop the client) to exit");
            run_window(frame, liveness)?;
            println!("window closed; exiting");
        }
    }
    Ok(())
}
```

Note: `run_window` takes its source **by value** (`S = Liveness`). Its bound is `S: AsFd, for<'a> &'a S: Read`; with `S = Liveness`, the required `&Liveness: Read` is the impl added in Task 2. Passing `&liveness` would instead demand `&&Liveness: Read` (not implemented), so pass `liveness` by value. The `None` and `Some(path)` branches are mutually exclusive, so each consumes `liveness` exactly once (the PNG branch `drop(liveness)`, the window branch moves it into `run_window`).

- [ ] **Step 3: QUIC loopback e2e pixel test**

Create `crates/rayland-server/tests/quic_e2e.rs`:

```rust
//! End-to-end over a real QUIC connection on localhost: a client sends the triangle command
//! stream, the server accepts it over QUIC, replays it on the GPU, and we assert the pixels.
//! This is SP2's headline proof that the transport swap is correct.

// The client's command-stream builder and the server's stream handler.
use rayland_client::send_triangle;
use rayland_server::handle_connection;
// The QUIC transport.
use rayland_transport::{connect, listen};
// Networking types.
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[test]
fn client_to_server_over_quic_renders_the_triangle() {
    // Bind the server on an ephemeral localhost UDP port.
    let listener = listen(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .expect("listen must succeed");
    // Discover the bound address for the client to connect to.
    let addr = listener.local_addr().expect("listener has an address");

    // Server thread: accept one QUIC connection and render the streamed frame.
    let server = std::thread::spawn(move || {
        let (mut recv, _liveness) = listener.accept().expect("accept must succeed");
        handle_connection(&mut recv).expect("server must render the frame")
    });

    // Client: connect over QUIC and send the triangle.
    let mut stream = connect(addr).expect("client connects");
    send_triangle(&mut stream, 64, 64, [0.0, 0.0, 1.0, 1.0]).expect("client sends");

    // Recover the rendered frame from the server thread.
    let frame = server.join().expect("server thread must not panic");

    // Centre must be red (inside the triangle).
    let center_i = ((32 * 64 + 32) * 4) as usize;
    assert!((frame.pixels[center_i] as i16 - 255).abs() <= 8, "centre red channel");
    // Top-left corner must be blue (clear colour shows through).
    assert!(frame.pixels[2] >= 247, "corner blue channel");
}
```

- [ ] **Step 4: Build, test (GPU + lavapipe), lint**

Run: `cargo build --workspace`.
Run: `cargo test -p rayland-server --test quic_e2e` — PASS on the real GPU.
Run: `VK_LOADER_DRIVERS_SELECT='*lvp*' cargo test -p rayland-server --test quic_e2e` — PASS on lavapipe.
Run: `cargo test --workspace` — all tests pass (SP0/SP1 + wire + the new QUIC e2e). The existing TCP `e2e.rs` still passes (it tests `handle_connection` over TCP directly, independent of QUIC).
Run: `cargo clippy --workspace -- -D warnings`, `cargo fmt --check` — clean.

- [ ] **Step 5: Reproduce-it + cross-machine doc**

Create `docs/sp2-real-transport.md`:

```markdown
# SP2 — Real Transport (how to run it)

SP2 carries the triangle command stream over **QUIC** instead of TCP, so the client can run on
a different machine and CPU architecture from the server. The server still renders on its GPU
and shows the result in a window (SP1).

## Local run (one machine)

Terminal A — the server (binds a QUIC/UDP port, waits for one connection):

    cargo run -p rayland-server            # listens on 127.0.0.1:9000 (QUIC)

Terminal B — the client:

    cargo run -p rayland-client            # connects to 127.0.0.1:9000 over QUIC

A window shows a red triangle on blue. Close it → the client exits; Ctrl-C the client → the
window closes (SP1 teardown, now over QUIC).

Headless / PNG fallback:

    cargo run -p rayland-server -- --png out.png
    cargo run -p rayland-client

## Cross-machine run (the SP2 milestone)

The server (S) runs on the machine with the GPU and display (the laptop, `dop561`); the client
(C) runs on another machine over the LAN. The client does no GPU work, so it builds on a weak
or foreign-architecture host.

1. On S, start the server bound to the LAN interface (or `0.0.0.0`) so C can reach it:

       cargo run -p rayland-server -- 0.0.0.0:9000

   Note S's LAN IP (e.g. `192.168.x.y`). QUIC is UDP — ensure UDP/9000 is allowed.

2. On C, build and run the client **natively** (no cross-compilation needed):

   - rv64 SBC:  `ssh milkv.localdomain`, then in a checkout of this repo:

         cargo build -p rayland-client
         cargo run -p rayland-client -- 192.168.x.y:9000

   - apollo:    `ssh apollo.localdomain -i ~/.ssh/keys.d/stationoost/id_ed25519`, then:

         cargo run -p rayland-client -- 192.168.x.y:9000

3. The triangle — emitted on C — appears in the window on S, rendered by S's GPU. This is the
   remote-app-on-your-screen milestone: a program on the rv64 board drawing on the laptop.

## Tests

    cargo test                             # unit + the QUIC loopback e2e (asserts pixels)

The transport uses a pure-Rust TLS crypto provider, so it builds on rv64 and on CI with no
system crypto libraries. The channel is **encrypted but not authenticated** in SP2 (a loudly-
named skip-verify); real authentication is SP4. See the
[SP2 design spec](design/2026-07-14-sp2-real-transport.md).

## Known SP2 limitations (deferred by design)

- One bidirectional stream; the multi-stream sibling protocol is SP3.
- Encrypted but unauthenticated (skip-verify); SSH-bootstrap + real trust is SP4.
- CPU round-trip through `wl_shm`; zero-copy dmabuf is SP3.
```

- [ ] **Step 6: Commit**

```bash
git add crates/rayland-server crates/rayland-client docs/sp2-real-transport.md
git commit -m "SP2 Task 4: QUIC transport in both binaries + loopback e2e + reproduce-it doc"
```

---

## Self-Review

**1. Spec coverage** — every SP2 spec section maps to a task:
- §1 success criterion (QUIC loopback pixel test; cross-machine manual) → Task 4 Step 3 + Step 5.
- §2 scope / non-goals (single stream, no dmabuf/auth/adaptive, native build) → respected throughout; documented in Task 4's doc.
- §3 architecture (rayland-transport, sync bridge, tokio confinement) → Tasks 1–2.
- §3.3 liveness / calloop seam → Task 2 (`Liveness`) + Task 3 (generalized `run_window`).
- §4 client cross-arch (no GPU dep) → Task 4 Step 1 (client only gains `rayland-transport`).
- §5 security (ephemeral self-signed + loud skip-verify) → Task 1 (`tls.rs` / `dangerous_insecure`).
- §6 spike-first → Task 1 (the loopback spike + decision gate).
- §7 testing (QUIC e2e, kept SP0/SP1 tests, manual milestones) → Tasks 1–4.
- §7.1 CI discipline (pure-Rust crypto, cargo-tree check) → Task 1 constraints + Task 2 Step 4.
- §8 error handling / deps / licenses → all tasks (thiserror/anyhow; LGPL transport).
- §9 definition of done → Task 4 Steps 4–6.
- §10 refinements → Tasks 1–4 and the doc.

**2. Placeholder scan** — code is complete per step. The one deliberate flexibility is Task 1's provider decision gate (rustcrypto vs ring) — a real spike outcome, recorded in the ledger, not a placeholder. Task 2 Step 3's two test helpers are explicitly resolved to real API calls (`local_addr`/`finish`).

**3. Type consistency** — `connect() -> QuicStream` (Read+Write), used by client `main` and the e2e test; `listen() -> QuicListener`, `accept() -> (QuicRecv, Liveness)`, used by server `main` and the e2e test; `QuicRecv: Read` feeds `handle_connection<R: Read>`; `Liveness: AsFd` + `impl Read for &Liveness` satisfies `run_window<S: AsFd, for<'a> &'a S: Read>`; `tls::{server_quic_config, client_quic_config}` produced in Task 1, consumed in Task 2. ALPN `b"rayland-sp2"` on both ends.

**Note for the executor:** CI YAML needs no change — the transport crypto is pure Rust (or, if the fallback is taken, `ring`, which builds on the runner). If Task 1 takes the `ring` fallback, confirm CI still builds (ring needs a C compiler, which `ubuntu-latest` has) and note it. Run `cargo tree | grep -iE 'aws-lc|ring'` after Task 1 to know which path you're on.
