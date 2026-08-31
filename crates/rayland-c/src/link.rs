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

/// Whether the C-side link trace is on, read from `RAYLAND_C1_LINK_LOG`.
///
/// # Why this exists, and why its markers match S's
/// A synchronous round trip — the thing that sets both the frame time and the length of a swapchain
/// rebuild — crosses four points: C writes, S reads, S writes, C reads. S has stamped its two since
/// 2026-08-31 (`[s-link]`); these are the other two. With both on the same `CLOCK_MONOTONIC` the trip
/// decomposes into *send + flush*, *wire to S*, *S processing*, and *wire back*, which is the only way
/// to say where its ~2 ms actually sits rather than guessing. Measured need: a swapchain rebuild does
/// roughly 477 of these and takes ~970 ms, which is the stall the owner sees on a focus change.
fn link_log_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RAYLAND_C1_LINK_LOG").is_some())
}

/// Emit one C-side link event, timestamped on the clock S also uses.
///
/// Markers mirror S's so the two logs read as one stream: `s>` before a write, `s<` after the flush
/// that makes it leave, `r<` when a reply has been read. Inert unless [`link_log_enabled`].
fn clink(marker: &str, what: &str) {
    if !link_log_enabled() {
        return;
    }
    eprintln!(
        "[c-link] t_ns={} {marker} {what}",
        rayland_relay::trace::monotonic_ns()
    );
}

/// A short, payload-free description of a `C2S`, for the link trace.
///
/// # Why it names a resource but never its contents
/// Attributing a round trip needs to know *which* stream a message belongs to — the ring, the reply
/// arena, or a staging blob all cross this link and only one of them releases the application — so the
/// resource id and the byte count are carried. The bytes themselves never are: this is a latency
/// instrument, a megabyte in a log line would destroy the timing it measures, and the payload is the
/// application's own pixels.
fn c2s_kind(m: &C2S) -> String {
    match m {
        C2S::CreateContext { .. } => "CreateContext".to_string(),
        C2S::CreateBlob { blob_id, size, .. } => format!("CreateBlob blob={blob_id} size={size}"),
        C2S::BlobData { res_id, bytes, .. } => format!("BlobData res={res_id} n={}", bytes.len()),
        C2S::RingDelta {
            ring_res_id,
            tail,
            bytes,
        } => format!("RingDelta res={ring_res_id} tail={tail} n={}", bytes.len()),
        C2S::SubmitCmd { .. } => "SubmitCmd".to_string(),
        C2S::NotifyRing { ring_id, seqno } => format!("NotifyRing ring={ring_id} seq={seqno}"),
        C2S::UnrefResource { .. } => "UnrefResource".to_string(),
        C2S::WaylandRequest { .. } => "WaylandRequest".to_string(),
        C2S::WaylandBind { .. } => "WaylandBind".to_string(),
        _ => "other".to_string(),
    }
}

/// The same for an `S2C`, and for the same reasons.
fn s2c_kind(m: &S2C) -> String {
    match m {
        S2C::BlobCreated { res_id, .. } => format!("BlobCreated res={res_id}"),
        S2C::BlobData { res_id, bytes, .. } => format!("BlobData res={res_id} n={}", bytes.len()),
        S2C::RingProgress { consumed_tail, .. } => format!("RingProgress tail={consumed_tail}"),
        S2C::Error { .. } => "Error".to_string(),
        S2C::WaylandEvent { .. } => "WaylandEvent".to_string(),
        _ => "other".to_string(),
    }
}

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
        // Stamped before the write and again after the flush, so the trace separates "serialising and
        // handing to quinn" from "actually on the wire" — a distinction that matters because a request
        // that has not been flushed is one S has not seen, and the caller is usually blocked on it.
        clink("s>", &c2s_kind(m));
        // The cost clock starts *after* the trace line, so an enabled trace inflates nothing but
        // itself. This is the third instrument in this project to be caught measuring its own
        // logging, and the fix each time was to move the boundary rather than to trust it.
        let sending = std::time::Instant::now();
        let framed = write_msg(&mut self.send, m).map_err(|e| EngineError::RelayLinkFailed {
            detail: format!("writing {m:?} to S failed: {e}"),
        })?;
        // Classify and count *after* a successful write: a message that failed to go out is not
        // traffic, and counting it would inflate the byte totals with bytes the network never saw.
        crate::metrics::metrics().record_send(m, framed);
        let flushed = self.send.flush().map_err(|e| EngineError::RelayLinkFailed {
            detail: format!("flushing the link to S failed: {e}"),
        });
        // Recorded even when the write failed above would have returned early — it cannot reach here
        // in that case — so every duration folded in is one of a message that actually went out.
        crate::metrics::metrics().record_send_cost(sending.elapsed());
        clink("s<", &c2s_kind(m));
        flushed
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
        clink("r<", &s2c_kind(&m));
        Ok(m)
    }
}
