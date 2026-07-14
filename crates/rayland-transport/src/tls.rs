//! TLS configuration for the SP2 QUIC transport: an ephemeral self-signed server certificate
//! and a deliberately-insecure client that accepts any certificate.
//!
//! SP2's security posture is **encrypted but not authenticated** (see the design spec §5):
//! the QUIC channel is protected against passive eavesdropping, but the client does not verify
//! the server's identity. Real authentication (SSH-bootstrap, certificate trust) is SP4. The
//! insecurity is confined to [`dangerous_insecure`] so it cannot be used by accident.
//!
//! **Crypto provider note (SP2 Task 1 decision gate):** the pure-Rust `rustls-rustcrypto`
//! provider (0.0.2-alpha) was tried first, per the SP2 plan's de-risking spike. It builds and
//! the rustls configs construct successfully, but `quinn::crypto::rustls::QuicServerConfig`
//! panics at runtime with "no initial cipher suite found" — the provider does not (yet) expose
//! a cipher suite quinn recognizes as usable for QUIC's Initial packet protection. This module
//! therefore uses `rustls::crypto::ring::default_provider()` (the `ring` fallback the plan
//! anticipated). `ring` is C/assembly, not pure Rust — revisit this when `rustls-rustcrypto`
//! matures past alpha, since rv64 portability was the original motivation for trying it first.

// The ALPN protocol identifier both ends must agree on; a mismatch fails the handshake.
const ALPN: &[u8] = b"rayland-sp2";

/// Build the shared QUIC transport parameters: a short idle timeout so a peer that vanishes
/// (e.g. the client is Ctrl-C'd — UDP has no connection-close signal) is detected within a
/// few seconds, plus a keep-alive interval so a live-but-idle connection is NOT dropped by
/// that timeout. This keeps SP1's "close on either side" teardown prompt over QUIC.
///
/// Without this, quinn's default 30s idle timeout means a killed client's window lingers on
/// the server for up to 30 seconds (a window-close, by contrast, is already prompt: it drives
/// an explicit `conn.close()` in [`super::sync_stream::Liveness::drop`], which the peer sees
/// immediately as `ConnectionLost`, not via the idle timer). Applied to BOTH
/// [`server_quic_config`] and [`client_quic_config`] — quinn negotiates the *lower* of the two
/// peers' advertised idle timeouts, so both ends must set it for the short value to take
/// effect regardless of which side goes quiet.
fn transport_config() -> std::sync::Arc<quinn::TransportConfig> {
    // A fresh transport config with quinn's defaults, then our two overrides.
    let mut tc = quinn::TransportConfig::default();
    // Detect a silently-gone peer within ~5s (default is 30s). `IdleTimeout` wraps a QUIC
    // varint of milliseconds; converting from a hardcoded 5s `Duration` cannot fail in
    // practice (it is far below the varint's range limit), so `.expect` documents that as an
    // assert on our own constant rather than a runtime possibility.
    tc.max_idle_timeout(Some(
        std::time::Duration::from_secs(5)
            .try_into()
            .expect("5s is a valid idle timeout"),
    ));
    // Send keep-alive pings well inside the idle timeout so an idle-but-alive link (e.g. the
    // window is open but nothing is being drawn) survives instead of being timed out.
    tc.keep_alive_interval(Some(std::time::Duration::from_millis(1500)));
    std::sync::Arc::new(tc)
}

/// Build the QUIC server configuration: a fresh self-signed certificate and a rustls server
/// config wrapped for QUIC.
///
/// The certificate is generated per process (`rcgen`) and never persisted — it exists only to
/// satisfy TLS's requirement that the server present a certificate; it is not a trust anchor.
///
/// # Errors
/// Returns an error if certificate generation, the rustls config, or the QUIC wrapping fails.
pub fn server_quic_config() -> anyhow::Result<quinn::ServerConfig> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::sync::Arc;

    // Generate a self-signed certificate for "localhost" (the name is not checked by the
    // client in SP2, but a valid cert must still be presented).
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    // Extract the DER-encoded certificate and its PKCS#8 private key.
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());

    // Build the rustls server config with the `ring` provider, explicitly (not process-default).
    // See the module doc comment: `rustls-rustcrypto` cannot yet drive quinn's QUIC crypto.
    let mut server_crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![cert_der], PrivateKeyDer::Pkcs8(key_der))?;
    // Advertise our ALPN so the client's matching ALPN completes the negotiation.
    server_crypto.alpn_protocols = vec![ALPN.to_vec()];

    // Wrap the rustls config for QUIC; this fails if the provider lacks a QUIC cipher suite.
    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?;
    // The final quinn server configuration.
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
    // Short idle timeout + keep-alive so a Ctrl-C'd client is noticed within seconds, not
    // quinn's default 30s (see `transport_config`'s doc comment).
    server_config.transport_config(transport_config());
    Ok(server_config)
}

/// Build the QUIC client configuration: the insecure verifier plus our ALPN.
///
/// # Errors
/// Returns an error if the rustls config or the QUIC wrapping fails.
pub fn client_quic_config() -> anyhow::Result<quinn::ClientConfig> {
    use std::sync::Arc;

    // Build the rustls client config with the `ring` provider and the accept-anything
    // verifier from `dangerous_insecure`. See the module doc comment for why `ring` is used
    // instead of the pure-Rust `rustls-rustcrypto` provider.
    let mut client_crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
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
    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_crypto));
    // Same short idle timeout + keep-alive as the server; quinn negotiates the lower of the
    // two peers' values, so both sides must set it (see `transport_config`'s doc comment).
    client_config.transport_config(transport_config());
    Ok(client_config)
}

/// The deliberately-insecure certificate verifier. **DO NOT SHIP.**
///
/// Every method here accepts whatever the server presents without checking identity. This
/// disables TLS authentication entirely — it protects only confidentiality, not against a
/// man-in-the-middle. It exists solely so SP2 can prove the transport without the SP4
/// certificate-trust machinery, and MUST be replaced before Rayland is used for anything real.
pub mod dangerous_insecure {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};
    use std::sync::Arc;

    /// A `ServerCertVerifier` that accepts ANY certificate. Insecure by design; see the module
    /// docs. It still delegates *signature* verification to a real crypto provider so the TLS
    /// handshake's math is valid — only the *identity* check is skipped.
    #[derive(Debug)]
    pub struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

    impl SkipServerVerification {
        /// Construct the verifier, wrapping the same provider used for signature checks.
        pub fn new() -> Arc<Self> {
            // Wrap the same provider the configs use (`ring`; see the module doc comment),
            // for signature-scheme support.
            Arc::new(Self(Arc::new(rustls::crypto::ring::default_provider())))
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
