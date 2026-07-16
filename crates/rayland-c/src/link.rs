//! [`QuicLink`]: the [`RelayLink`] that actually crosses a network.
//!
//! # What this module is, and what it deliberately is not
//! It is the seam [`RelayLink`] was designed for, finally filled with a real transport. Everything
//! before (c)1 Task 6 spoke to a mock: `relay_engine`'s tests answer from a scripted queue, and the
//! daemon's earlier TCP placeholder was never run against a live S. This is the first thing in the
//! sub-project that puts a genuine network between C and S.
//!
//! It is **not** new QUIC code. `rayland-transport` (SP2) already owns the QUIC endpoint, the TLS
//! configuration, and the confined tokio runtime that drives them, and exposes blocking
//! `Read`/`Write` adapters so synchronous code can use them unchanged. This module is the thin
//! adapter between those adapters and `rayland-relay`'s framing — which is all it should be, and is
//! why it is short.
//!
//! # Why the two halves are separate types
//! [`RelayLink`] declares `send` and `recv` on one trait, but `rayland-c` never uses them on one
//! object: `main.rs`'s reader thread owns receiving and nothing else may do it, while the vtest
//! thread and the ring watcher share sending behind a mutex. That split is not a style choice — the
//! daemon's module docs name the deadlock a single-owner design causes (while the vtest thread is
//! blocked reading Mesa's socket, nobody drains the link, so S's replies sit unread while the
//! application spins on a `head` only those replies can advance).
//!
//! So [`QuicSendLink`] and [`QuicRecvLink`] each implement the half they own and **refuse the
//! other in type**, rather than implementing both and trusting a comment. See their `send`/`recv`
//! doc comments for why refusing is safer than a plausible-looking implementation.
//!
//! # Why QUIC rather than TCP (ring-findings §7)
//! The findings are emphatic that **latency, not bandwidth, is what will hurt Rayland**: the reply
//! arena was ~12x the command traffic, and its replies are round trips the application blocks on.
//! Head-of-line blocking on a single TCP stream is exactly the wrong property for that. (c)1 v1
//! still puts everything on **one** QUIC stream, so it does not yet *collect* on that — a single
//! stream has the same head-of-line behaviour TCP does. What it buys now is the endpoint, the
//! handshake and the congestion control being in place, so that splitting the reply path onto its
//! own stream is a later change to this file rather than a transport project. That is a real,
//! unclaimed limitation and it is stated here rather than in a report nobody reads.

// The relay message set and its framing.
use rayland_relay::{C2S, S2C, read_msg, write_msg};
// The transport halves SP2 exposes, and the error type the engine seam speaks.
use rayland_transport::{QuicRecv, QuicSend};
use rayland_vtest::EngineError;

// `write_msg` hands bytes to the stream; flushing is what makes them leave.
use std::io::Write;
use std::net::SocketAddr;

/// Connect to S over QUIC and return the two halves of the link.
///
/// # Why this returns halves rather than one object
/// See the module docs: `rayland-c` runs a dedicated reader thread, and nothing else may receive.
/// Returning the halves separately makes that arrangement the only one the types permit, rather
/// than a rule a future edit could quietly break.
///
/// # Inputs / outputs
/// - `s_addr`: S's address. QUIC is UDP, so this is a UDP endpoint even though the surrounding code
///   speaks of a "connection" — there is a real handshake, it is just not TCP's.
/// - Returns the send half (for the vtest thread and the ring watcher, behind a mutex) and the
///   receive half (for the reader thread, exclusively).
///
/// # Failure modes
/// Returns [`EngineError::RelayLinkFailed`] if the endpoint cannot be bound or the handshake fails
/// — most often because S is not running, or is not reachable at `s_addr`. The error names the
/// address, because "connection refused" with no address in it is the least useful message a
/// two-machine bring-up can produce.
pub fn connect(s_addr: SocketAddr) -> Result<(QuicSendLink, QuicRecvLink), EngineError> {
    // SP2 owns the endpoint, the TLS config and the runtime; this is the whole of (c)1's QUIC code.
    let stream = rayland_transport::connect(s_addr).map_err(|e| EngineError::RelayLinkFailed {
        detail: format!("connecting to S at {s_addr} over QUIC: {e:#}"),
    })?;
    // Two threads, two halves, no lock between the reader and the writers.
    let (send, recv) = stream.split();
    Ok((QuicSendLink { send }, QuicRecvLink { recv }))
}

/// The sending half of the link to S.
///
/// Shared between the vtest thread (via `ChannelLink`) and the ring watcher, behind a mutex that
/// `main.rs` owns. The mutex is what keeps a watcher's blob-then-delta batch atomic against an
/// interleaved blob creation from the vtest thread — see the watcher's send loop.
pub struct QuicSendLink {
    /// SP2's blocking write adapter over the QUIC send half.
    send: QuicSend,
}

/// The receiving half of the link to S.
///
/// Owned exclusively by `main.rs`'s reader thread. Nothing else may receive; see the module docs.
pub struct QuicRecvLink {
    /// SP2's blocking read adapter over the QUIC receive half.
    recv: QuicRecv,
}

impl crate::relay_engine::RelayLink for QuicSendLink {
    /// Frame and write one message to S, then flush it.
    ///
    /// The flush is not politeness. `write_msg` hands bytes to the adapter, but a request that has
    /// not left C is a request S never sees — and the caller is often blocked waiting for its
    /// answer, so the application stalls on a reply that was never asked for. (SP2's `QuicStream`
    /// flush is a no-op today because it does not buffer above quinn; it is called anyway, because
    /// this code's correctness must not rest on a detail of the transport's current internals.)
    fn send(&mut self, m: &C2S) -> Result<(), EngineError> {
        // `write_msg` reports the framed size — body plus the 4-byte prefix — because it is the only
        // place that knows it without serializing a possibly-megabyte message twice (Task 9).
        let framed = write_msg(&mut self.send, m).map_err(|e| EngineError::RelayLinkFailed {
            detail: format!("writing {m:?} to S failed: {e}"),
        })?;
        // Classify and count *after* a successful write: a message that failed to go out is not
        // traffic, and counting it would inflate the byte totals with bytes the network never saw.
        crate::metrics::metrics().record_send(m, framed);
        self.send.flush().map_err(|e| EngineError::RelayLinkFailed {
            detail: format!("flushing the link to S failed: {e}"),
        })
    }

    /// Refused in type: this half cannot receive, and the reader thread must be the only thing that
    /// does.
    ///
    /// # Why this is a refusal and not an implementation
    /// A plausible implementation is impossible here — this half holds no receive stream — but the
    /// deeper reason is that it *should* be impossible. If anything other than the reader thread
    /// could receive, S's replies would land in the wrong caller and the session would desynchronize
    /// **silently**: the next request would be answered by this one's reply, and every request after
    /// that by the previous one's, forever. That is the failure mode `RelayLink`'s own contract
    /// warns about, and making it unrepresentable is stronger than documenting it.
    fn recv(&mut self) -> Result<S2C, EngineError> {
        Err(EngineError::RelayLinkFailed {
            detail: "the send half of the link to S cannot receive; only the reader thread may \
                     receive, or S's replies desynchronize (see rayland-c's module docs)"
                .into(),
        })
    }
}

impl crate::relay_engine::RelayLink for QuicRecvLink {
    /// Refused in type: this half cannot send. It exists so the reader thread can block in `recv`
    /// while other threads send, which is exactly what it must not be able to interfere with.
    fn send(&mut self, _m: &C2S) -> Result<(), EngineError> {
        Err(EngineError::RelayLinkFailed {
            detail: "the receive half of the link to S cannot send".into(),
        })
    }

    /// Block until S says something.
    ///
    /// # Failure modes
    /// [`EngineError::RelayLinkFailed`] if the link failed or S closed it. A closed link is an error
    /// rather than an end-of-stream, deliberately: every caller downstream of this is waiting for a
    /// specific answer, and "no answer is coming" is a failure for all of them. SP2's adapter maps a
    /// lost connection to EOF (`Ok(0)`), which `read_msg` then surfaces as a short read — so a
    /// vanished S arrives here as an I/O error, which is what it is.
    fn recv(&mut self) -> Result<S2C, EngineError> {
        // The framed size comes back with the message for the same reason it does on the send side:
        // only the framing layer knows what actually crossed (Task 9).
        let (m, framed) = read_msg(&mut self.recv).map_err(|e| EngineError::RelayLinkFailed {
            detail: format!("reading from S failed: {e}"),
        })?;
        // Every S->C message passes through this one function — the reader thread owns `recv`
        // exclusively — so counting here cannot miss a message, and that exclusivity is what makes
        // the return-path total (ring-findings §7's ~12x prediction) trustworthy rather than a
        // sample.
        crate::metrics::metrics().record_recv(&m, framed);
        Ok(m)
    }
}
