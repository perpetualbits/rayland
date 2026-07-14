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
