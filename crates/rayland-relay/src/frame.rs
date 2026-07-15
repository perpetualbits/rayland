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
/// 8 MiB is generous for a single (c)1 frame: the largest individual blob observed by
/// C0's captures (`docs/design/2026-07-15-venus-ring-findings.md` §6) was exactly 8 MiB
/// (`8388608` bytes, the command-buffer staging pool), and the command ring itself is
/// 128 KiB. A frame carrying the full contents of that 8 MiB blob plus its message
/// envelope will sit right at this boundary; if (c)1 later needs to move blobs larger
/// than 8 MiB whole, this constant — or the "ship the whole blob" strategy in
/// [`crate::C2S::BlobData`]'s doc comment — will need to be revisited together.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

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
pub fn write_msg<W: Write, M: Serialize>(w: &mut W, m: &M) -> Result<(), RelayError> {
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
    Ok(())
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
pub fn read_msg<R: Read, M: DeserializeOwned>(r: &mut R) -> Result<M, RelayError> {
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
    Ok(message)
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

        let result: Result<C2S, RelayError> = read_msg(&mut cursor);
        assert!(
            matches!(result, Err(RelayError::Io(_))),
            "expected RelayError::Io from a truncated stream, got {result:?}"
        );
    }
}
