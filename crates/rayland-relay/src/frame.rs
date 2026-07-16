//! Framing: turning a message into bytes on a stream and back.
//!
//! This mirrors `rayland-wire`'s framing exactly (see `crates/rayland-wire/src/frame.rs`)
//! because it solves the identical problem: a byte stream (a QUIC stream, a TCP socket,
//! an in-memory buffer in a test) has no message boundaries of its own, so every message
//! is prefixed with its own length. The reader first reads a fixed-size length, then
//! reads exactly that many body bytes, then decodes them.
//!
//! Unlike `rayland-wire`, whose `write_message`/`read_message` are hardcoded to its one
//! `Message` enum, [`write_msg`]/[`read_msg`] here are generic over the message type. This
//! crate has *two* message enums travelling in opposite directions ([`crate::C2S`] and
//! [`crate::S2C`]), and `rayland-c`/`rayland-s` each only ever need to frame one direction
//! at a time — genericity avoids duplicating the same four lines of framing logic twice.

// Read/Write are the standard streaming I/O traits; std::io::Error is their error type.
use std::io::{Read, Write};

// Serialize is needed to encode an outgoing message; DeserializeOwned (rather than plain
// Deserialize<'de>) is needed because the decoded value must outlive the borrowed byte
// buffer it was decoded from — the caller gets back an owned `M`, not one borrowing `r`.
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The largest length prefix [`read_msg`] will accept before allocating a body buffer.
///
/// As in `rayland-wire`, the length prefix on the wire is an untrusted `u32` supplied by
/// whatever is on the other end of the stream — for this crate, that is `rayland-s`
/// reading whatever `rayland-c` (or an attacker on the network path) sent it. Without a
/// bound, a corrupt or malicious peer could claim a length near `u32::MAX` and force
/// `read_msg` to attempt a multi-gigabyte allocation; Rust aborts the whole process on
/// allocation failure rather than returning a recoverable error, so an unbounded length
/// prefix is a denial-of-service, not merely a slow path.
///
/// # Why 32 MiB, and the trap this number exists to avoid
/// The cap must sit **strictly above the largest _legitimate_ payload plus its envelope**, or it
/// stops being a hostility guard and becomes a bug that rejects real traffic. Two real ceilings
/// bound a (c)1 frame, and the value must clear **both**:
///
/// 1. **8 MiB — the command-buffer staging pool.** C0 measured this blob (`res=4`) at *exactly*
///    `8388608` bytes (`docs/design/2026-07-15-venus-ring-findings.md` §6), and (c)1 v1 ships
///    mapped blobs **whole**, deliberately (the spec's §7 rules out dirty tracking as premature
///    cleverness). A [`crate::C2S::BlobData`] carrying it adds a discriminant, `res_id`, `offset`
///    and a length prefix on top.
/// 2. **16 MiB — virglrenderer's maximum ring size** (`VKR_RING_BUFFER_MAX_SIZE`, `vkr_ring.h:20`).
///    A [`crate::C2S::RingDelta`] is bounded by the ring it came from. Mesa's `buf_size` is a
///    client-side constant currently set to 128 KiB (`vn_instance.c:149`), so today's deltas are
///    tiny — but that constant is exactly the sort of thing a later slice raises for throughput,
///    and the host would accept it.
///
/// **This constant was 8 MiB and that was a latent bug**, caught in review before it could bite:
/// it equalled ceiling (1) *exactly*, so the envelope alone would have pushed the one blob C0 had
/// already captured past the cap — blob sync would have failed on day one, presenting as a framing
/// bug rather than a sizing one. 16 MiB would merely relocate the same mistake onto ceiling (2).
/// 32 MiB clears both with margin while staying a bounded, survivable allocation.
///
/// If a future slice needs to move something larger than this whole — (c)4's real applications will
/// push hundreds of megabytes of texture — the answer is **not** to keep raising this number. It is
/// to chunk, which [`crate::C2S::BlobData`]'s `offset` field already exists to express.
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Everything that can go wrong while framing or deframing a message.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    /// The underlying stream failed (connection reset, disk full, EOF mid-frame, etc.).
    #[error("stream I/O failed while framing a relay message")]
    Io(#[from] std::io::Error),
    /// The bytes could not be (de)serialized as a valid message.
    #[error("relay message (de)serialization failed")]
    Codec(#[from] postcard::Error),
    /// The length prefix exceeded [`MAX_FRAME_BYTES`]; the frame is refused before
    /// allocating a buffer to hold it.
    #[error("frame length {len} exceeds the maximum of {} bytes", MAX_FRAME_BYTES)]
    FrameTooLarge {
        /// The rejected length, exactly as read off the wire.
        len: u32,
    },
}

/// Serialize `msg` and write it to `w` as a length-prefixed frame.
///
/// The frame is a little-endian `u32` byte count followed by that many bytes of
/// `postcard`-encoded message. Returns an error if serialization or the underlying write
/// fails. Generic over `M` so the same function frames both [`crate::C2S`] (written by
/// `rayland-c`) and [`crate::S2C`] (written by `rayland-s`).
///
/// # Returns the number of framed bytes written — body **plus** the 4-byte prefix
/// That count exists for (c)1 Task 9's measurement (`rayland-c`'s `metrics` module), and it is
/// returned from here rather than recomputed by the caller for one blunt reason: **this is the
/// only place the true size is known without paying for it twice.** A caller wanting the size
/// would have to serialize the message a second time — and blob-sync messages carry a megabyte of
/// mapped memory, so a second `postcard::to_stdvec` would allocate and copy 1 MiB per frame purely
/// to count it, corrupting the very timings being measured.
///
/// Callers that do not care may ignore it; `write_msg(w, m)?;` in statement position discards it
/// exactly as it discarded the previous `()`.
pub fn write_msg<W: Write, M: Serialize>(w: &mut W, m: &M) -> Result<usize, RelayError> {
    // Encode the message into an owned byte vector using postcard's compact, deterministic format.
    let bytes = postcard::to_stdvec(m)?;
    // The length prefix must fit in a u32; converted explicitly (never silently truncated) so an
    // implausibly huge message is reported as an error instead of corrupting the frame.
    let len = u32::try_from(bytes.len()).map_err(|_| {
        RelayError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "relay message too large to frame",
        ))
    })?;
    // Write the 4-byte little-endian length prefix ahead of the body, so the reader knows exactly
    // how many bytes to read before attempting to decode.
    w.write_all(&len.to_le_bytes())?;
    // Write the message body itself.
    w.write_all(&bytes)?;
    // Report what actually crossed: the body plus the 4-byte length prefix. Counting only the body
    // would under-report every message by 4 bytes, which on a chatty channel is a real distortion of
    // a measurement whose whole purpose is to be trusted.
    Ok(bytes.len() + 4)
}

/// Read one length-prefixed frame from `r` and decode it into an `M`.
///
/// Reads the 4-byte little-endian length, checks it against [`MAX_FRAME_BYTES`], then
/// reads exactly that many body bytes and decodes them. Returns an error if the stream
/// ends early, the length is implausible, or the bytes are not a valid `M`.
///
/// **Pitfall — the length prefix is untrusted input.** It arrives straight off the wire
/// before anything about the sender has been verified: for `rayland-s`, that is whatever
/// `rayland-c` — or an attacker sitting on the network path — chose to send. Trusting it
/// naively (e.g. `vec![0u8; len]` with `len` taken directly from the prefix) would let a
/// corrupt or malicious peer request an allocation of up to ~4 GiB (`u32::MAX`); since
/// Rust aborts the process on allocation failure rather than returning an error, that is
/// a denial-of-service / crash vector, not merely a slow path. To close it, `len` is
/// checked against [`MAX_FRAME_BYTES`] and rejected with [`RelayError::FrameTooLarge`]
/// **before** the body buffer is allocated.
///
/// # Returns the decoded message **and** the number of framed bytes read
/// The byte count is the body plus the 4-byte prefix, and it is returned for the same reason
/// [`write_msg`] returns its count: (c)1 Task 9 measures the return path, ring-findings §7 predicts
/// that path is ~12x the command path, and only this function knows the true framed size without
/// re-encoding a message that may hold a megabyte of blob data.
///
/// The tuple is deliberately not an out-parameter or a separate `read_msg_counted`: a second entry
/// point would let a future caller read frames that the measurement never sees, and a metric with a
/// silent blind spot is worse than no metric, because it still looks like a total.
pub fn read_msg<R: Read, M: DeserializeOwned>(r: &mut R) -> Result<(M, usize), RelayError> {
    // Read the 4-byte length prefix; a short read here means the stream ended (or never had a
    // full frame to begin with).
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    // Interpret the prefix as a little-endian u32, matching write_msg's encoding.
    let len = u32::from_le_bytes(len_bytes);
    // Refuse implausibly large frames *before* any allocation sized by `len` happens: an
    // untrusted peer must never be able to force a huge allocation via this value alone.
    if len as usize > MAX_FRAME_BYTES {
        return Err(RelayError::FrameTooLarge { len });
    }
    // Allocate a buffer of exactly the now-bounded size and fill it from the stream.
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    // Decode the body bytes into the caller's requested message type.
    let message = postcard::from_bytes(&body)?;
    // Report the framed size alongside the message: `len` body bytes plus the 4-byte prefix that
    // was consumed before them.
    Ok((message, len as usize + 4))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::C2S;

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
        // read_msg reports the framed size alongside the message; assert it matches what
        // write_msg said it wrote, so the two halves of the byte accounting Task 9 rests on are
        // pinned against each other rather than merely believed.
        let written = write_msg(&mut Vec::new(), &msg).expect("write");
        let (got, framed): (C2S, usize) = read_msg(&mut buf.as_slice()).expect("read");
        assert_eq!(got, msg);
        assert_eq!(
            framed, written,
            "read and write must agree on the framed size"
        );
        assert_eq!(framed, buf.len(), "the framed size must be the whole frame");
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

    #[test]
    fn truncated_stream_is_an_error() {
        // Write one valid message so we have a real, well-formed length-prefixed frame,
        // matching rayland-wire's equivalent test for the same failure mode.
        let message = C2S::UnrefResource { res_id: 7 };
        let mut buffer: Vec<u8> = Vec::new();
        write_msg(&mut buffer, &message).expect("writing to a Vec cannot fail");

        // Cut the body short while keeping the full 4-byte length prefix intact, so read_msg
        // believes more body bytes are coming than are actually present.
        let truncated_len = buffer.len().saturating_sub(2);
        let truncated = &buffer[..truncated_len];
        let mut cursor = std::io::Cursor::new(truncated);

        let result: Result<(C2S, usize), RelayError> = read_msg(&mut cursor);
        assert!(
            matches!(result, Err(RelayError::Io(_))),
            "expected RelayError::Io from a truncated stream, got {result:?}"
        );
    }
}
