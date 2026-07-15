//! Rayland transport (SP2): synchronous stream adapters over a QUIC connection.
//!
//! QUIC (via [`quinn`]) is asynchronous and needs a [`tokio`] runtime; the rest of Rayland is
//! synchronous. This crate quarantines all of that: it owns the runtime and the QUIC/TLS
//! stack and exposes blocking `Read`/`Write` adapters ([`sync_stream`]) so the existing
//! synchronous client/server code (`send_triangle`/`wait_until_closed`/`handle_connection`)
//! runs unchanged over QUIC. Task 1 established the crate and the TLS configuration ([`tls`])
//! and proved a QUIC handshake + byte round-trip on localhost (see the `spike_tests` module
//! below). Task 2 (this file's [`connect`]/[`listen`] plus [`sync_stream`]) adds the public
//! synchronous API: [`connect`] for the client, [`listen`]/[`QuicListener::accept`] for the
//! server, and the [`Liveness`] handle the server's window loop watches for client disconnect.

// TLS configuration and the loud insecure verifier.
pub mod tls;

// The synchronous stream adapters.
pub mod sync_stream;

// Public re-exports so callers write `rayland_transport::QuicStream`, etc.
pub use sync_stream::{Liveness, QuicListener, QuicRecv, QuicSend, QuicStream};

// Standard networking types.
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::runtime::Runtime;

/// Build the confined multi-thread tokio runtime that drives QUIC.
///
/// A multi-thread runtime is required (not `current_thread`): the synchronous adapters call
/// `block_on` from the caller's own thread while expecting the runtime's *worker* threads to
/// keep driving the QUIC connection's packet I/O concurrently. A `current_thread` runtime has
/// no separate worker thread, so a `block_on` call from outside it would never see progress on
/// tasks spawned onto it (e.g. the `Liveness` close-watcher).
///
/// # Errors
/// Returns an error if the runtime cannot be created (e.g. thread spawning fails).
fn build_runtime() -> anyhow::Result<Arc<Runtime>> {
    // A multi-thread runtime so its workers drive QUIC while the caller blocks on `block_on`.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    Ok(Arc::new(rt))
}

/// Connect to a Rayland server over QUIC and return a synchronous bidirectional stream.
///
/// Binds an ephemeral local UDP port, connects to `server_addr` using the insecure client TLS
/// config (see [`tls`]), and opens the single bidirectional command stream the rest of Rayland
/// treats as its transport. The returned [`QuicStream`] owns a dedicated tokio runtime that
/// keeps driving the connection for as long as the stream is alive.
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

/// Bind a QUIC server endpoint on `bind_addr`.
///
/// Use `bind_addr`'s port `0` to let the OS choose an ephemeral port, then read the actual
/// bound address back with [`QuicListener::local_addr`].
///
/// # Errors
/// Returns an error if the runtime, TLS config, or endpoint binding fails.
pub fn listen(bind_addr: SocketAddr) -> anyhow::Result<QuicListener> {
    // Own a runtime for the server.
    let rt = build_runtime()?;
    // Bind the server endpoint inside the runtime.
    let endpoint = rt.block_on(async {
        anyhow::Ok(quinn::Endpoint::server(
            tls::server_quic_config()?,
            bind_addr,
        )?)
    })?;
    Ok(QuicListener::new(rt, endpoint))
}

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
            endpoint_client
                .set_default_client_config(tls::client_quic_config().expect("client config"));
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
