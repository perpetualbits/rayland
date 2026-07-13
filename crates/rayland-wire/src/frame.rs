//! Framing: turning [`Message`] values into bytes on a stream and back.
//!
//! A byte stream (like TCP) has no message boundaries of its own, so we prefix each
//! serialized message with its length as a little-endian `u32`. The reader first reads
//! the 4-byte length, then reads exactly that many bytes, then decodes them. Fixing the
//! byte order as little-endian keeps the framing identical across CPU architectures —
//! which matters because the client may one day be big- or little-endian and a different
//! architecture from the server.

// Read/Write are the standard streaming I/O traits; std::io::Error is their error type.
use std::io::{Read, Write};

// The message type being framed.
use crate::Message;

/// The largest length prefix `read_message` will accept before allocating a body buffer.
///
/// The length prefix on the wire is an untrusted `u32` supplied by whatever is on the
/// other end of the stream. Without a bound, a corrupt or malicious peer could send a
/// length near `u32::MAX` and force `read_message` to attempt a multi-gigabyte
/// allocation; in Rust, allocation failure aborts the process rather than returning an
/// error, so an unbounded length prefix is a denial-of-service / crash vector. 64 MiB is
/// enormously generous for SP0, whose messages are a handful of tiny fixed-size structs
/// and small vertex lists — the bound exists purely to keep a bad C→S length prefix from
/// aborting S, not because SP0 traffic is expected to approach it.
pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Everything that can go wrong while framing or deframing a message.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// The underlying stream failed (connection closed, disk full, etc.).
    #[error("stream I/O failed while framing a message")]
    Io(#[from] std::io::Error),
    /// The bytes could not be (de)serialized as a valid message.
    #[error("message (de)serialization failed")]
    Codec(#[from] postcard::Error),
    /// The length prefix exceeded [`MAX_FRAME_BYTES`]; the frame is refused before allocating.
    #[error("frame length {len} exceeds the maximum of {} bytes", MAX_FRAME_BYTES)]
    FrameTooLarge { len: u32 },
}

/// Serialize `msg` and write it to `w` as a length-prefixed frame.
///
/// The frame is a little-endian `u32` byte count followed by that many bytes of
/// postcard-encoded message. Returns an error if serialization or the write fails.
pub fn write_message<W: Write>(w: &mut W, msg: &Message) -> Result<(), WireError> {
    // Encode the message into an owned byte vector.
    let bytes = postcard::to_stdvec(msg)?;
    // The length prefix must fit in a u32; SP0 messages are tiny, so this always holds,
    // but we convert explicitly rather than silently truncate.
    let len = u32::try_from(bytes.len()).map_err(|_| {
        // Map an implausibly huge message to an I/O error rather than panicking.
        WireError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "message too large to frame",
        ))
    })?;
    // Write the 4-byte little-endian length prefix.
    w.write_all(&len.to_le_bytes())?;
    // Write the message body.
    w.write_all(&bytes)?;
    // Everything written successfully.
    Ok(())
}

/// Read one length-prefixed frame from `r` and decode it into a [`Message`].
///
/// Reads the 4-byte little-endian length, then exactly that many body bytes, then
/// decodes them. Returns an error if the stream ends early or the bytes are not a valid
/// message.
///
/// **Pitfall:** the length prefix is untrusted input — it comes straight off the wire
/// before anything about the sender is verified. Naively trusting it (e.g.
/// `vec![0u8; len]` with `len` taken directly from the prefix) lets a corrupt or
/// malicious peer request an allocation of up to ~4 GiB (`u32::MAX`); since Rust aborts
/// the process on allocation failure rather than returning an error, that is a
/// denial-of-service / crash vector, not merely a slow path. To close it, `len` is
/// checked against [`MAX_FRAME_BYTES`] and rejected with [`WireError::FrameTooLarge`]
/// *before* the body buffer is allocated.
pub fn read_message<R: Read>(r: &mut R) -> Result<Message, WireError> {
    // Read the 4-byte length prefix; a short read here means the stream ended.
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    // Interpret the prefix as a little-endian u32; kept as u32 (not widened yet) so it
    // can be compared directly against MAX_FRAME_BYTES before it drives any allocation.
    let len = u32::from_le_bytes(len_bytes);
    // Refuse implausibly large frames up front: an untrusted peer must not be able to
    // force a huge allocation (which would abort the process on failure) via this value.
    if len > MAX_FRAME_BYTES {
        return Err(WireError::FrameTooLarge { len });
    }
    // Allocate a buffer of exactly that size (now bounded) and fill it from the stream.
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    // Decode the body bytes into a Message.
    let message = postcard::from_bytes(&body)?;
    // Hand back the decoded message.
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, PROTOCOL_VERSION, Vertex};

    #[test]
    fn messages_round_trip_through_a_buffer() {
        // A representative sequence of messages, exactly what the client sends in SP0.
        let sent = vec![
            Message::Hello {
                version: PROTOCOL_VERSION,
            },
            Message::BeginFrame {
                width: 4,
                height: 4,
                clear_color: [0.0, 0.0, 1.0, 1.0],
            },
            Message::UploadVertices {
                vertices: vec![Vertex {
                    position: [0.0, -0.5],
                    color: [1.0, 0.0, 0.0],
                }],
            },
            Message::DrawTriangles { vertex_count: 3 },
            Message::EndFrame,
        ];

        // Write every message into an in-memory byte buffer (stands in for a TCP stream).
        let mut buffer: Vec<u8> = Vec::new();
        for message in &sent {
            write_message(&mut buffer, message).expect("writing to a Vec cannot fail");
        }

        // Read them back out of the buffer in order and collect them.
        let mut cursor = std::io::Cursor::new(buffer);
        let mut received = Vec::new();
        for _ in 0..sent.len() {
            received.push(read_message(&mut cursor).expect("each framed message must decode"));
        }

        // The sequence read back must equal the sequence written.
        assert_eq!(received, sent);
    }

    #[test]
    fn truncated_stream_is_an_error() {
        // Write one valid message so we have a real, well-formed length-prefixed frame.
        let message = Message::EndFrame;
        let mut buffer: Vec<u8> = Vec::new();
        write_message(&mut buffer, &message).expect("writing to a Vec cannot fail");

        // Cut the body short by a few bytes while keeping the full 4-byte length prefix
        // intact, so read_message believes more body bytes are coming than are present.
        let truncated_len = buffer.len().saturating_sub(3);
        let truncated = &buffer[..truncated_len];
        let mut cursor = std::io::Cursor::new(truncated);

        // The short body read must surface as a WireError::Io, not a panic or silent
        // wrong result.
        let result = read_message(&mut cursor);
        assert!(
            matches!(result, Err(WireError::Io(_))),
            "expected WireError::Io from a truncated stream, got {result:?}"
        );
    }

    #[test]
    fn oversized_length_is_rejected() {
        // Craft a frame whose length prefix claims a ~4 GiB body (u32::MAX), far beyond
        // MAX_FRAME_BYTES. No body bytes are needed: read_message must reject the frame
        // from the length prefix alone, before it ever tries to allocate or read a body.
        let bytes = u32::MAX.to_le_bytes();
        let mut cursor = std::io::Cursor::new(bytes);

        let result = read_message(&mut cursor);
        assert!(
            matches!(result, Err(WireError::FrameTooLarge { .. })),
            "expected WireError::FrameTooLarge from an oversized length prefix, got {result:?}"
        );
    }
}
