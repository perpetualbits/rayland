//! Rayland's on-the-wire protocol (SP0).
//!
//! This crate defines the small set of command messages that the C ("client") side
//! sends to the S ("server") side, and how they are framed on a byte stream. In SP0
//! this protocol is a deliberate throwaway — just enough to draw one triangle — and it
//! is *not* Vulkan's own wire format. Later sub-projects replace it with the real
//! command-remoting engine's protocol.

// The message types (this task).
mod message;
// Re-export them at the crate root so callers write `rayland_wire::Message`.
pub use message::{Message, PROTOCOL_VERSION, Vertex};
