//! The Rayland (c)1 relay wire protocol.
//!
//! This crate defines the messages that cross the network between `rayland-c` (the
//! weak, possibly-headless machine where the *application* runs) and `rayland-s` (the
//! machine with the real GPU and the display), and the length-prefixed framing used to
//! put them on a byte stream. It is pure data: no GPU code, no sockets, no async
//! runtime. That is deliberate — `rayland-c` must never need to link a GPU stack, and a
//! pure-data crate is fully unit-testable without any hardware or process I/O.
//!
//! # Why the message set looks the way it does
//!
//! It would be natural to assume Venus's Vulkan command stream travels as discrete,
//! interceptable API calls. It does not. Sub-project C0's investigation
//! (`docs/design/2026-07-15-venus-ring-findings.md`) found that Mesa's Venus ICD writes
//! the application's entire Vulkan command stream into a **shared-memory ring** that it
//! and the host driver `mmap` in common; the Unix-domain vtest socket carries only ring
//! *management* (create-ring, and an occasional doorbell). On a single machine that
//! shared page is free. Across a network there is no such thing as a page shared between
//! two machines, so (c)1 exists to carry, as explicit messages, everything that a shared
//! mapping used to make invisible:
//!
//! - [`C2S::RingDelta`] carries the new ring bytes themselves — **this is the payload the
//!   whole project exists to move**, since it *is* the serialized Vulkan command stream.
//! - [`C2S::SubmitCmd`] carries the bytes that arrive on the vtest socket itself, in a
//!   `VCMD_SUBMIT_CMD2`. It is tiny — ring-findings §2 measured 140–236 bytes for an
//!   entire Vulkan initialization — and it is **indispensable**, because the socket's one
//!   real command is `vkCreateRingMESA`: the message that makes S create the ring in the
//!   first place. An S that handles `RingDelta` but not `SubmitCmd` has no ring to deliver
//!   anything to, and nothing the application draws is ever executed.
//! - [`S2C::BlobData`] carries the reply arena the application blocks on, and the
//!   readback buffer the GPU renders into — without it the application never learns the
//!   answer to a synchronous call, or sees its own pixels.
//! - [`C2S::NotifyRing`] would carry the doorbell — but **nothing constructs it today**,
//!   and an implementor of S must not wait for one. `rayland-c`'s relay engine forwards
//!   *everything* off the vtest socket as [`C2S::SubmitCmd`], and Mesa's `vkNotifyRingMESA`
//!   arrives on that socket like any other command, so the doorbell reaches S **inside**
//!   `SubmitCmd`, already in the Venus command language S's context decoder expects. The
//!   variant is kept because the doorbell may yet deserve to be hoisted out of the command
//!   stream — recognising it costs a decode S would otherwise not do — and removing it now
//!   would only have to be undone. See its own doc comment for why counting doorbells is
//!   meaningless regardless of how they arrive.
//!
//! # Framing
//!
//! Messages are framed exactly as `rayland-wire` frames its SP0 messages: a
//! little-endian `u32` byte count, followed by that many bytes of `postcard`-encoded
//! message. The length prefix is untrusted input — `rayland-s` reads it directly off a
//! network socket before anything about the sender is verified — so [`read_msg`] checks
//! it against [`MAX_FRAME_BYTES`] and refuses oversized frames *before* allocating a
//! buffer to hold the body. See [`mod@frame`] for the full rationale.

// The C2S / S2C message enums (this module).
mod message;
// Re-export the message types at the crate root so callers write `rayland_relay::C2S`.
pub use message::{BlobRun, C2S, S2C};

// Length-prefixed framing over byte streams, generic over the message type being framed.
mod frame;
// Re-export the framing API at the crate root.
pub use frame::{MAX_FRAME_BYTES, RelayError, read_msg, write_msg};
