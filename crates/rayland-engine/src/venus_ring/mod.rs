//! Mesa's Venus **command ring**: its shared-memory layout, a decoder for the command stream it
//! carries, and a live diagnostic that watches one being written.
//!
//! # The finding this module exists to record
//! When a real Mesa Venus client (the ICD, `libvulkan_virtio.so`) talks to us over the vtest wire
//! protocol, **it does not send its Vulkan commands on the socket.** The socket carries only ring
//! *management*: one `vkCreateRingMESA` to declare the ring, then a series of `vkNotifyRingMESA`
//! doorbells that say no more than *"my tail has moved, come look"*. Every application Vulkan
//! command — `vkCreateInstance`, `vkEnumerateInstanceVersion`, and so on — is written by the client
//! **directly into shared memory**: a blob resource that *we* allocate and whose descriptor *we*
//! export to it.
//!
//! That memory is a Mesa `vn_ring`. Its bytes are the same `vn_cs_encoder` command language the
//! socket's inline `VCMD_SUBMIT_CMD2` path uses — the same `[VkCommandTypeEXT][VkCommandFlagsEXT]`
//! prologue, the same `vn_sizeof_*` encodings. This matters far beyond a plumbing detail: it means
//! remote Vulkan over Venus is a **transport problem, not an opacity problem**. The command stream
//! is legible to us today, with no VM, no virtgpu, and no reverse-engineering of an unknown format
//! — because we hold the descriptor for the memory it lives in and the layout is declared to us
//! in-band (see [`layout`](#the-ring-layout) below).
//!
//! The consequence for Rayland's architecture is direct: [`crate::RenderEngine::submit`] — the path
//! the vtest socket feeds — never sees a single application Vulkan command. It sees the ring's
//! *address* and then a series of pokes. A network transport (SP2's QUIC work) cannot simply
//! forward the socket; it must carry the ring's bytes and synthesize the ring's *handshake* on both
//! ends, because the client polls `head` while the host writes it. That is bidirectional state, not
//! one-way streaming.
//!
//! The full evidence — three independent live captures, the byte-level confirmations, and the
//! reasoning — is in the sub-project's spike report. The load-bearing bytes from that capture are
//! preserved in this repository as a test fixture (`captured.rs`) so the finding survives without
//! a GPU, a Mesa install, or the scratch directory the capture was made in.
//!
//! # The ring layout
//! The client's first blob is **131268** bytes, which decomposes exactly:
//!
//! ```text
//!   192 (control words) + 131072 (128 KiB command buffer) + 4 (extra) = 131268
//! ```
//!
//! | offset  | field    | written by                         |
//! |---------|----------|------------------------------------|
//! | `0x00`  | `head`   | the host — bytes *consumed*        |
//! | `0x40`  | `tail`   | the client — bytes *produced*      |
//! | `0x80`  | `status` | the host (a bitmask; bit 0 = IDLE) |
//! | `0xc0`  | buffer   | the client — the command stream    |
//! | `0x200c0` | extra  | (vestigial; nothing observed here) |
//!
//! Each control word gets its own 64-byte slot because Mesa declares them `alignas(64)`: `head`,
//! `tail` and `status` are written by *different* threads on *different* sides of the shared
//! mapping, and packing them into one cache line would make every doorbell a false-sharing storm.
//! The 64-byte stride is a performance decision of Mesa's, not a Rayland one, but a reader that
//! assumes three adjacent dwords will read garbage.
//!
//! # Pitfall: do not hardcode these offsets in a real reader
//! The constants in this module ([`RING_HEAD_OFFSET`] and friends) are the values **one observed
//! client declared**, and they are here so the fixture test can assert against something. They are
//! *not* a specification. Mesa transmits every one of them in-band, in the `vkCreateRingMESA`
//! command's `VkRingCreateInfoMESA`, precisely so the host does not have to know them a priori. A
//! production ring reader must parse them from that message. Hardcoding them buys nothing and
//! breaks silently the day Mesa changes a stride or a client picks a different buffer size.
//!
//! # Pitfall: `head` and `tail` are byte counters, not indices
//! In the observed capture they are plain monotonically-increasing byte counts that had not yet
//! wrapped (4024 bytes used of a 131072-byte buffer). Mesa indexes the buffer *modulo* its size, so
//! a long-running ring will have `tail` far larger than the buffer and a command may straddle the
//! wrap point. See the scope limits below: none of that is exercised here.
//!
//! # Scope limits — what this code has never seen (read this before trusting it)
//! The decoder in [`decode`] was written to answer one question ("are these real Vulkan commands?")
//! against roughly 4 KB of an **init-only** workload. It answered it. It is not a general Venus
//! command-stream reader, and the following are entirely untested:
//!
//! - **Ring wrap.** The observed ring used 4024 of 131072 bytes, so `head` and `tail` never wrapped
//!   and never approached the buffer's end. [`decode::decode_commands`] therefore takes a **linear**
//!   slice and has no modulo arithmetic at all. A command that straddles the wrap point would be
//!   decoded as garbage. What a client does when the ring is *full* (block? notify and spin?) is
//!   equally unknown.
//! - **The out-of-line command path.** `vkExecuteCommandStreamsMESA` (180) never appeared: every
//!   command in the capture was inlined directly into the ring. Mesa switches to out-of-line
//!   streams — a *pointer* in the ring to command bytes living in some other shmem — for large
//!   payloads (commands above roughly 8 KiB). Any real rendering workload is expected to use this
//!   path, and this module cannot follow it. Note also that only a window of the ring was
//!   inspected, so "180 was never seen" means "not in the window", not "not in the ring".
//! - **Anything past `vkCreateInstance`.** [`decode::encoded_size`] knows the size of exactly three
//!   command types, one of which is the doorbell that never even appears in a ring. It stops cleanly
//!   at the first command it cannot size rather than guessing — which, on the captured stream,
//!   happens at command four. That is a feature (a wrong size would desynchronize the walk and
//!   produce confident nonsense forever after), but it means the decoder covers a rounding error of
//!   Venus's ~1000-command surface.
//! - **Real GPU work.** No fences, no timelines, no draws.
//!
//! In short: this module proves a mechanism. It does not implement a client.

// The command-stream decoder and its command-size table: the durable, testable half of the finding.
pub mod decode;
// The live diagnostic that watches a real client write a real ring. Inert unless its env var is set.
pub mod dump;

// The verbatim bytes captured from a live Mesa 26.0.3 Venus client, and the tests that decode them.
// Test-only: this is a fixture, not a runtime table, and it must never be mistaken for one.
#[cfg(test)]
mod captured;

/// Byte offset of the ring's `head` control word — how many bytes the **host** has consumed.
///
/// See the module docs' pitfall: this is the value one observed client declared in-band, not a
/// constant a real reader may assume.
pub const RING_HEAD_OFFSET: usize = 0x00;

/// Byte offset of the ring's `tail` control word — how many bytes the **client** has produced.
///
/// This is the ring's write frontier, and the strongest single piece of evidence in the capture:
/// `RING_BUFFER_OFFSET + tail` predicted, exactly, where the next observed write landed, three
/// times in a row with no fitting. It is also what a `vkNotifyRingMESA` doorbell's `seqno` carries.
pub const RING_TAIL_OFFSET: usize = 0x40;

/// Byte offset of the ring's `status` word — a host-written **bitmask**, not a boolean.
///
/// From Mesa's generated headers (`vn_protocol_renderer_defines.h`, and identically in the
/// driver-side copy): `IDLE = 0x1`, `FATAL = 0x2`, `ALIVE = 0x4`.
///
/// # Domain pitfall: the polarity is the opposite of what `status == 1` suggests
/// **Bit 0 set means the host's ring thread is IDLE (parked); `status == 0` means it is actively
/// polling.** "1" reads like "busy" and means the reverse. This module's docs originally recorded
/// the polarity inverted; the raw captured values were never affected (and no code reads this word
/// — the decoder uses observed *offsets*), but the mechanism below only makes sense the right way
/// round.
///
/// # Domain pitfall: this word is why doorbell *counts* measure nothing
/// Mesa sends a `vkNotifyRingMESA` only when it observes the **IDLE bit set** — i.e. only when the
/// host has already given up polling — and then at most once per millisecond. So byte-identical
/// ring traffic produced **one** doorbell in one capture run and **four** in another. Any metric
/// built on counting socket messages is measuring the scheduler, not the workload.
pub const RING_STATUS_OFFSET: usize = 0x80;

/// Byte offset at which the ring's command buffer begins — the end of the 192-byte control area.
pub const RING_BUFFER_OFFSET: usize = 0xc0;

/// Size of the observed ring's command buffer: 128 KiB.
///
/// `head` and `tail` are taken modulo this value by Mesa. Nothing in this module does that
/// arithmetic, because nothing in the capture ever wrapped — see the module's scope limits.
pub const RING_BUFFER_SIZE: usize = 131072;

/// Byte offset of the `extra` region, immediately after the command buffer.
pub const RING_EXTRA_OFFSET: usize = RING_BUFFER_OFFSET + RING_BUFFER_SIZE;

/// Size of the `extra` region. Four bytes, and nothing was ever observed to write them; it appears
/// vestigial in this Mesa version. Recorded because it is part of the size arithmetic, not because
/// it is known to be useful.
pub const RING_EXTRA_SIZE: usize = 4;

/// Total size of the observed ring's shared-memory blob, in bytes.
///
/// This number is what made the ring findable in the first place. A blob of 131268 bytes is a
/// 128 KiB power-of-two buffer plus a 196-byte remainder, and a non-power-of-two remainder next to
/// a power-of-two buffer is what a *header* looks like. The `vkCreateRingMESA` command later
/// confirmed the decomposition exactly, and this module's `shmem_size_decomposes` test keeps that
/// arithmetic honest.
pub const RING_SHMEM_SIZE: usize = RING_EXTRA_OFFSET + RING_EXTRA_SIZE;

#[cfg(test)]
mod tests {
    use super::*;

    /// The size arithmetic that identified the blob as a ring must close exactly.
    ///
    /// This is not arithmetic for its own sake: the capture's `vkCreateRingMESA` declared the shmem
    /// size as 131268 *and* declared the five offsets independently, and the fact that the offsets
    /// tile the size with no gap and no overlap is what rules out "these are coincidental numbers".
    /// If a future edit adjusts one constant without the others, this fails rather than silently
    /// producing a layout that describes no real ring.
    #[test]
    fn shmem_size_decomposes() {
        // The control area is exactly three 64-byte-aligned words, ending where the buffer begins.
        assert_eq!(RING_BUFFER_OFFSET, 192, "control area is 192 bytes");
        // Each control word sits in its own 64-byte slot (Mesa's `alignas(64)`, to avoid the
        // false sharing that would otherwise couple the host's writes to the client's).
        assert_eq!(RING_TAIL_OFFSET - RING_HEAD_OFFSET, 64);
        assert_eq!(RING_STATUS_OFFSET - RING_TAIL_OFFSET, 64);
        assert_eq!(RING_BUFFER_OFFSET - RING_STATUS_OFFSET, 64);
        // The buffer is a power of two, which is what lets Mesa index it with a cheap mask.
        assert!(RING_BUFFER_SIZE.is_power_of_two());
        // The whole blob is control + buffer + extra, with nothing left over. This is the exact
        // decomposition `vkCreateRingMESA` declared for the 131268-byte blob the client asked for.
        assert_eq!(192 + RING_BUFFER_SIZE + RING_EXTRA_SIZE, RING_SHMEM_SIZE);
        assert_eq!(RING_SHMEM_SIZE, 131268, "the observed blob's exact size");
    }
}
