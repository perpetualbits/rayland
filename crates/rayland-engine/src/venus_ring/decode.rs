//! A decoder for the Venus command stream that lives in the ring's buffer.
//!
//! # The encoding, in one paragraph
//! Mesa's `vn_cs_encoder` writes each command as a **`[VkCommandTypeEXT][VkCommandFlagsEXT]`
//! prologue** — two little-endian `u32`s — followed by the command's arguments, packed with no
//! padding and no alignment beyond the natural size of each field. A pointer argument is encoded as
//! an 8-byte **presence marker** (non-zero = the pointer was non-NULL) followed, if present, by the
//! pointee's contents inline. An out-parameter (something the *reply* fills in) contributes only
//! its presence marker: the client is telling the host "I passed you a buffer for this", not
//! sending a value.
//!
//! # The pitfall that shapes this entire module: commands are not self-delimiting
//! **There is no length field.** Nothing in a command says how long it is. The only way to find
//! where command N+1 starts is to already know how long command N is, which requires knowing the
//! *structure* of command N's arguments — that is, having Mesa's generated `vn_sizeof_*` for that
//! specific command. Venus has roughly a thousand of them.
//!
//! This has a sharp consequence for any decoder, and it is why [`encoded_size`] returns an
//! `Option`: a decoder that *guesses* a size does not merely mis-read one command, it loses the
//! stream frame and every subsequent command is confident nonsense at a wrong offset. So this
//! decoder knows a handful of sizes exactly, from Mesa's own headers, and **stops** the instant it
//! meets a command it cannot size. Stopping early is a correct, honest answer. Guessing is not.
//!
//! # Scope
//! See the parent module's scope limits. In short: no ring-wrap handling (the input here is a
//! **linear** slice, deliberately), no `vkExecuteCommandStreamsMESA` out-of-line streams, and a
//! size table covering three command types. This decoder exists to prove that the ring's bytes are
//! the Venus command language — a question it answers conclusively — not to consume a real
//! workload.

/// `VK_COMMAND_TYPE_vkCreateInstance_EXT`. Variable-size (it carries application name strings and
/// an extension list), so [`encoded_size`] cannot size it and the decoder stops here.
pub const VK_COMMAND_TYPE_VK_CREATE_INSTANCE: u32 = 0;

/// `VK_COMMAND_TYPE_vkEnumerateInstanceVersion_EXT`.
pub const VK_COMMAND_TYPE_VK_ENUMERATE_INSTANCE_VERSION: u32 = 137;

/// `VK_COMMAND_TYPE_vkSetReplyCommandStreamMESA_EXT`.
///
/// Venus's reply channel is not implicit: before a command that expects an answer, the client
/// issues this to say *"put your replies in resource R at offset O, and I have reserved S bytes"*.
/// Consecutive `SetReplyCommandStream`s chain — the next one's `offset` is the previous one's
/// `offset + size` — which makes them an unusually good decoder self-check (see
/// [`decode_reply_command_stream`]).
pub const VK_COMMAND_TYPE_VK_SET_REPLY_COMMAND_STREAM_MESA: u32 = 178;

/// `VK_COMMAND_TYPE_vkExecuteCommandStreamsMESA_EXT` — the **out-of-line** command path.
///
/// Declared here for recognition value only: [`encoded_size`] deliberately does not size it. When
/// Mesa has more command bytes than it wants to inline (roughly 8 KiB), it does not put them in the
/// ring; it puts a *descriptor* in the ring pointing at other shared memory. This constant exists
/// so that a future reader who hits a stop at type 180 immediately knows they have found the second
/// data path rather than a corrupt stream. This module has never seen it — see the parent module's
/// scope limits.
pub const VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA: u32 = 180;

/// `VK_COMMAND_TYPE_vkCreateRingMESA_EXT` — the command that declares a ring's layout in-band.
///
/// Variable-size (it carries a `VkRingCreateInfoMESA` with a `pNext` chain), so it is not sized
/// here. This is the command that arrives *inline on the vtest socket* and tells the host where the
/// ring's `head`, `tail`, `status` and buffer live inside the blob.
pub const VK_COMMAND_TYPE_VK_CREATE_RING_MESA: u32 = 188;

/// `VK_COMMAND_TYPE_vkNotifyRingMESA_EXT` — the doorbell.
///
/// Arrives inline on the vtest socket, not in the ring, but it is the same command language, and
/// its size is fixed and confirmed by the capture. Its `seqno` argument is the ring's `tail`: the
/// doorbell literally says *"my tail is now X, come look"*.
pub const VK_COMMAND_TYPE_VK_NOTIFY_RING_MESA: u32 = 190;

/// The size in bytes of a command's `[type][flags]` prologue: two little-endian `u32`s.
const COMMAND_HEADER_BYTES: usize = 8;

/// The size in bytes of an encoded pointer-presence marker. Eight, matching a 64-bit pointer on the
/// capture host, and non-zero means "the pointer was not NULL".
///
/// Domain pitfall: the *value* of a present marker is not stable, and must never be compared
/// against one. The capture shows the client writing `0x0000_0000_0000_0001` while the renderer
/// writes `0x1_0000_0000` in the reply direction — consistent with encoders that only ever test the
/// marker for non-NULL-ness. [`decode_reply_command_stream`] therefore tests `!= 0`, nothing more.
const POINTER_MARKER_BYTES: usize = 8;

/// One decoded command from the ring's buffer.
///
/// Deliberately shallow: it records *where* a command is, *what* it is, and *how long* it is —
/// which is exactly what was needed to prove the ring carries the Venus command language, and is
/// exactly what can be established without Mesa's full generated decoder. Arguments are not
/// decoded here; [`decode_reply_command_stream`] does that for the one command type whose payload
/// carries independently checkable evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingCommand {
    /// Byte offset of this command's first byte, relative to the **start of the slice passed to
    /// [`decode_commands`]** — which callers normally slice from [`super::RING_BUFFER_OFFSET`],
    /// making this directly comparable against the ring's [`super::RING_HEAD_OFFSET`] /
    /// [`super::RING_TAIL_OFFSET`] byte counters.
    pub offset: usize,
    /// The `VkCommandTypeEXT` — e.g. [`VK_COMMAND_TYPE_VK_SET_REPLY_COMMAND_STREAM_MESA`].
    pub command_type: u32,
    /// The `VkCommandFlagsEXT`. Bit 0 set means the client wants a reply written back.
    pub command_flags: u32,
    /// Total encoded length in bytes, prologue included — the stride to the next command.
    pub encoded_size: usize,
}

/// Why [`decode_commands`] stopped walking.
///
/// Every variant is a normal outcome, not a failure: this decoder's honest answer to most real
/// streams is "I got this far". Modelling that as an error type would misrepresent it — nothing has
/// gone *wrong* when a decoder with three known command sizes meets a fourth command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeStop {
    /// The walk consumed the slice exactly, landing on its final byte with no remainder. For a
    /// slice cut at the ring's `tail`, this is the ideal outcome: every produced byte was accounted
    /// for by a whole command, which is strong evidence the frame was never lost.
    ReachedEnd,
    /// A command's type is not in [`encoded_size`]'s table, so its length — and therefore where the
    /// next command begins — is unknowable. The walk stops rather than guess. On the captured
    /// stream this fires at [`VK_COMMAND_TYPE_VK_CREATE_INSTANCE`].
    UnknownCommandSize {
        /// Offset of the command that could not be sized.
        offset: usize,
        /// Its `VkCommandTypeEXT`, so the caller can say *which* command it was.
        command_type: u32,
    },
    /// The slice ends part-way through a command: fewer bytes remain than the command's prologue or
    /// its declared size needs.
    ///
    /// This is expected and benign when decoding a *window* of a ring (the capture preserved only
    /// the first 100 bytes of a 216-byte production). It is a genuine red flag when decoding a
    /// slice cut exactly at `tail`, where it would mean the client published a partial command —
    /// or, far more likely, that the reader raced the writer and read a torn `tail`.
    Truncated {
        /// Offset at which the incomplete command begins.
        offset: usize,
    },
}

/// The encoded size, in bytes, of a Venus command — or `None` if this module cannot size it.
///
/// # Where these numbers come from
/// Each is Mesa's own `vn_sizeof_<command>` for the observed call shape, summed field by field from
/// the generated protocol headers — **not** measured from the capture. That independence is the
/// point: the fixture test compares these sizes against a captured `head` byte counter that the
/// client's host wrote, and the two agreeing is evidence. Had these sizes been derived from the
/// capture, the test would be a tautology proving only that the capture equals itself.
///
/// | command | fields | bytes |
/// |---|---|---|
/// | [`VK_COMMAND_TYPE_VK_SET_REPLY_COMMAND_STREAM_MESA`] | type 4 + flags 4 + marker 8 + `resourceId` 4 + `offset` 8 + `size` 8 | **36** |
/// | [`VK_COMMAND_TYPE_VK_ENUMERATE_INSTANCE_VERSION`] | type 4 + flags 4 + marker 8 | **16** |
/// | [`VK_COMMAND_TYPE_VK_NOTIFY_RING_MESA`] | type 4 + flags 4 + `ring` 8 + `seqno` 4 + `flags` 4 | **24** |
///
/// # Inputs / outputs
/// - `command_type`: a `VkCommandTypeEXT` as read from a command's prologue.
/// - Returns `Some(bytes)` for a command whose encoding is **fixed** and known, else `None`.
///
/// # Failure modes / pitfalls
/// `None` is not an error; see [`DecodeStop::UnknownCommandSize`]. Critically, `None` is also the
/// right answer for a command this module *names* but cannot size:
/// [`VK_COMMAND_TYPE_VK_CREATE_INSTANCE`], [`VK_COMMAND_TYPE_VK_CREATE_RING_MESA`] and
/// [`VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA`] are all variable-length. Recognising a
/// command is not the same as being able to skip it, and conflating the two is how a decoder
/// desynchronizes.
pub fn encoded_size(command_type: u32) -> Option<usize> {
    match command_type {
        // A `VkCommandStreamDescriptionMESA` passed by pointer: presence marker, then the struct's
        // `resourceId` (u32), `offset` (VkDeviceSize/u64) and `size` (VkDeviceSize/u64) packed with
        // no padding — note `offset` lands at a 4-byte-aligned address, which is exactly the sort of
        // thing a reader who assumes C struct alignment gets wrong.
        VK_COMMAND_TYPE_VK_SET_REPLY_COMMAND_STREAM_MESA => {
            Some(COMMAND_HEADER_BYTES + POINTER_MARKER_BYTES + 4 + 8 + 8)
        }
        // `pApiVersion` is a pure out-parameter: only its presence marker goes on the wire, because
        // the value travels back in the reply, not out in the command.
        VK_COMMAND_TYPE_VK_ENUMERATE_INSTANCE_VERSION => {
            Some(COMMAND_HEADER_BYTES + POINTER_MARKER_BYTES)
        }
        // The doorbell: a `ring` id (u64, by value — not a pointer, so no marker), then `seqno` and
        // `flags` as u32s.
        VK_COMMAND_TYPE_VK_NOTIFY_RING_MESA => Some(COMMAND_HEADER_BYTES + 8 + 4 + 4),
        // Everything else, including the command types this module names but cannot size. See the
        // doc comment: naming is not sizing.
        _ => None,
    }
}

/// Read a little-endian `u32` at `offset`, or `None` if fewer than 4 bytes remain.
///
/// Little-endian regardless of the host: the capture was taken on x86-64, and these bytes are a
/// memory image of that host's ring. Decoding them natively would make this decoder — and the
/// fixture test — silently wrong on a big-endian machine, which is not hypothetical for a project
/// whose stated goal includes running the client on other architectures.
fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let field = bytes.get(offset..offset + 4)?;
    // `try_into` cannot fail: the slice was just bounds-checked to be exactly 4 bytes long.
    Some(u32::from_le_bytes(field.try_into().ok()?))
}

/// Read a little-endian `u64` at `offset`, or `None` if fewer than 8 bytes remain. See
/// [`read_u32_le`] for why the endianness is pinned rather than native.
fn read_u64_le(bytes: &[u8], offset: usize) -> Option<u64> {
    let field = bytes.get(offset..offset + 8)?;
    // `try_into` cannot fail: the slice was just bounds-checked to be exactly 8 bytes long.
    Some(u64::from_le_bytes(field.try_into().ok()?))
}

/// Walk a Venus command stream, returning every command it could size and why it stopped.
///
/// # Inputs / outputs
/// - `stream`: the bytes to walk, as a **linear** slice. Callers reading a live ring should pass
///   `&blob[RING_BUFFER_OFFSET..][..tail]` — see the pitfalls below before doing so.
/// - Returns the commands decoded, in stream order, plus the [`DecodeStop`] that ended the walk.
///   The command list is a prefix of the truth: everything in it is decoded correctly, and there
///   may be more after the stop point that this module cannot reach.
///
/// # Failure modes
/// Cannot fail and cannot panic: every read is bounds-checked, and an unsizeable or incomplete
/// command ends the walk with the corresponding [`DecodeStop`]. An empty slice yields no commands
/// and [`DecodeStop::ReachedEnd`].
///
/// # Pitfalls
/// - **No wrap handling.** `stream` is linear. A real ring is circular and Mesa indexes it modulo
///   [`super::RING_BUFFER_SIZE`], so once `tail` exceeds the buffer size a naive
///   `&blob[RING_BUFFER_OFFSET..][..tail]` slice is both out of bounds and semantically wrong. The
///   captured ring never wrapped, and this function has never been run against one that did.
/// - **Racing the writer.** A ring is being written *by another process* as you read it. Any
///   snapshot may be torn. Decoding past a torn `tail` is how you turn a race into a plausible-
///   looking command that never existed.
/// - **The stop point is not the end of the stream.** [`DecodeStop::UnknownCommandSize`] means this
///   module ran out of knowledge, not that the client ran out of commands.
pub fn decode_commands(stream: &[u8]) -> (Vec<RingCommand>, DecodeStop) {
    let mut commands = Vec::new();
    // Byte offset of the next command to decode; advanced by each command's own encoded size,
    // because nothing in the stream tells us where the next one starts.
    let mut offset = 0usize;

    loop {
        // Landing exactly on the end means every byte was consumed by a whole command — the
        // outcome that says the frame was never lost.
        if offset == stream.len() {
            return (commands, DecodeStop::ReachedEnd);
        }
        // Not enough left for even a `[type][flags]` prologue: the slice cuts through a command.
        let (Some(command_type), Some(command_flags)) =
            (read_u32_le(stream, offset), read_u32_le(stream, offset + 4))
        else {
            return (commands, DecodeStop::Truncated { offset });
        };
        // The whole reason this decoder is conservative: without a known size we cannot find the
        // next command, and a guess would desynchronize every command after it.
        let Some(encoded_size) = encoded_size(command_type) else {
            return (
                commands,
                DecodeStop::UnknownCommandSize {
                    offset,
                    command_type,
                },
            );
        };
        // The command is sizeable but its bytes are not all here — normal when decoding a captured
        // window, suspicious when decoding up to a live `tail`.
        if offset + encoded_size > stream.len() {
            return (commands, DecodeStop::Truncated { offset });
        }
        commands.push(RingCommand {
            offset,
            command_type,
            command_flags,
            encoded_size,
        });
        // Advance by this command's own length: the only available stride.
        offset += encoded_size;
    }
}

/// The decoded payload of a [`VK_COMMAND_TYPE_VK_SET_REPLY_COMMAND_STREAM_MESA`] command: where the
/// client has told the host to write its replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyCommandStream {
    /// The resource id of the reply arena. In the capture this is the client's *second* blob — the
    /// 1 MiB one — which is how that blob's purpose was identified.
    pub resource_id: u32,
    /// Byte offset within the arena at which the host should write the next reply.
    pub offset: u64,
    /// Number of bytes the client has reserved there for it.
    pub size: u64,
}

/// Decode the arguments of a `vkSetReplyCommandStreamMESA` command.
///
/// # Why this one command's payload is worth decoding
/// It is the best available self-check on the whole decoder. Consecutive reply streams **chain**:
/// each one's `offset` equals the previous one's `offset + size`. That chain is a relationship
/// between fields the decoder read at two different offsets, using sizes derived from Mesa's
/// headers rather than from the data. If the walk had lost the frame, or the field layout were
/// wrong, the chain would not close — but it does, in the captured stream (`offset=0, size=20`
/// then `offset=20, size=24`), and the arena's contents at exactly those offsets hold exactly the
/// matching replies.
///
/// # Inputs / outputs
/// - `stream`: the same slice that was passed to [`decode_commands`].
/// - `command`: a [`RingCommand`] produced from that slice.
/// - Returns `Some(ReplyCommandStream)` on success; `None` if `command` is not a reply-stream
///   command, if its bytes are not all within `stream`, or if the `pStream` pointer was encoded as
///   NULL (a present marker is required — a NULL descriptor carries no fields to read).
///
/// # Failure modes
/// Cannot panic; every read is bounds-checked. Passing a `command` that came from a *different*
/// slice is a caller error that this cannot detect and that would decode neighbouring bytes as
/// fields.
pub fn decode_reply_command_stream(
    stream: &[u8],
    command: &RingCommand,
) -> Option<ReplyCommandStream> {
    // Only this command type has these fields; refuse anything else rather than read garbage.
    if command.command_type != VK_COMMAND_TYPE_VK_SET_REPLY_COMMAND_STREAM_MESA {
        return None;
    }
    // The `pStream` presence marker sits immediately after the prologue. Test only for non-zero:
    // the marker's value is not a stable `1` (see `POINTER_MARKER_BYTES`).
    let marker = read_u64_le(stream, command.offset + COMMAND_HEADER_BYTES)?;
    if marker == 0 {
        return None;
    }
    // The `VkCommandStreamDescriptionMESA` body follows the marker, packed with no padding.
    let body = command.offset + COMMAND_HEADER_BYTES + POINTER_MARKER_BYTES;
    Some(ReplyCommandStream {
        resource_id: read_u32_le(stream, body)?,
        // `offset` and `size` are `VkDeviceSize` (u64) and sit at 4-byte-aligned — not
        // 8-byte-aligned — addresses, because the encoder packs rather than aligns.
        offset: read_u64_le(stream, body + 4)?,
        size: read_u64_le(stream, body + 12)?,
    })
}
