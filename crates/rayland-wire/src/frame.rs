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

/// Everything that can go wrong while framing or deframing a message.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// The underlying stream failed (connection closed, disk full, etc.).
    #[error("stream I/O failed while framing a message")]
    Io(#[from] std::io::Error),
    /// The bytes could not be (de)serialized as a valid message.
    #[error("message (de)serialization failed")]
    Codec(#[from] postcard::Error),
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
pub fn read_message<R: Read>(r: &mut R) -> Result<Message, WireError> {
    // Read the 4-byte length prefix; a short read here means the stream ended.
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    // Interpret the prefix as a little-endian u32, then widen to usize for allocation.
    let len = u32::from_le_bytes(len_bytes) as usize;
    // Allocate a buffer of exactly that size and fill it from the stream.
    let mut body = vec![0u8; len];
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
}
