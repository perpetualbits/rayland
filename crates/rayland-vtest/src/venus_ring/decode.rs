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

/// `VK_COMMAND_TYPE_vkGetDeviceQueue2_EXT` — the command that carries the application's per-queue
/// timeline **`ring_idx`**, the one value S needs to fence on real GPU completion (see
/// [`find_get_device_queue2`]).
///
/// Unlike the commands above, this one is **fixed-size** (80 bytes, no strings/arrays/variable pNext
/// for Mesa's queue-init shape), so it *could* be sized — but it is deliberately kept out of
/// [`encoded_size`] and the linear walk, because the walk cannot *reach* it: variable-size commands
/// (`vkCreateInstance`, `vkCreateDevice`, …) precede it and correctly stop the walk long before. It is
/// instead found by a self-verifying **signature scan** ([`find_get_device_queue2`]).
pub const VK_COMMAND_TYPE_VK_GET_DEVICE_QUEUE2: u32 = 155;

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

/// The total encoded size of a `vkGetDeviceQueue2` command, in bytes. Fixed: Mesa's queue-init shape
/// has no strings, arrays, or variable pNext (exactly one `VkDeviceQueueTimelineInfoMESA`, inner
/// pNext NULL), so the whole command is always this long. Source: `vn_sizeof_vkGetDeviceQueue2`
/// summed field by field — see `docs/design/2026-07-19-c2-ringidx-decode.md` §1.
const GET_DEVICE_QUEUE2_SIZE: usize = 80;

/// Byte offset of the outer `VkDeviceQueueInfo2.sType` within an encoded `vkGetDeviceQueue2`. Its
/// value is a fixed structure-type constant used as a signature magic word (see
/// [`find_get_device_queue2`]).
const QUEUE_INFO2_STYPE_OFFSET: usize = 24;

/// Byte offset of the `VkDeviceQueueTimelineInfoMESA.sType` — the second, MESA-specific magic word.
const TIMELINE_INFO_STYPE_OFFSET: usize = 36;

/// Byte offset of the `ringIdx` `u32` — the value this whole decode exists to read.
const RING_IDX_OFFSET: usize = 48;

/// Byte offset of the `VkDevice` handle (a `u64` object id) in commands whose first argument is the
/// device — both `vkGetDeviceQueue2` and `vkDestroyDevice` encode it here, right after the 8-byte
/// prologue. Used to tie a `vkDestroyDevice` to the *same* device whose queue was latched (see
/// [`find_destroy_device`]).
const DEVICE_HANDLE_OFFSET: usize = 8;

/// Byte offset of the `pQueue` out-parameter's `VkQueue` handle within an encoded `vkGetDeviceQueue2`
/// (after the pQueue presence marker at +64). Venus object ids are client-assigned, so this is the
/// handle the application's own `vkQueueSubmit` will name — [`find_queue_submit`] matches on it to
/// recognise *this* queue's submits.
const GET_DEVICE_QUEUE2_QUEUE_HANDLE_OFFSET: usize = 72;

/// `VK_COMMAND_TYPE_vkDestroyDevice_EXT` — the command that frees the device and, with it, the
/// per-queue timeline registered at `vkGetDeviceQueue2`. Once the host dispatches this, a fence on
/// that queue's `ring_idx` is render-server-fatal; [`find_destroy_device`] lets S close its readback
/// gate *before* that happens. Fixed 24-byte encoding for the no-allocator case, but this decoder
/// only reads its first 16 bytes (type, flags, device), which are fixed regardless of `pAllocator`.
pub const VK_COMMAND_TYPE_VK_DESTROY_DEVICE: u32 = 12;

/// `VkStructureType VK_STRUCTURE_TYPE_DEVICE_QUEUE_INFO_2`, as written on the wire (a raw
/// little-endian `int32`; Venus does not remap sTypes). A core Vulkan constant.
const STYPE_DEVICE_QUEUE_INFO_2: u32 = 1000145003;

/// `VkStructureType VK_STRUCTURE_TYPE_DEVICE_QUEUE_TIMELINE_INFO_MESA`, as written on the wire. A
/// Venus/MESA extension constant; its presence in the pNext chain is what makes a match unambiguous.
const STYPE_DEVICE_QUEUE_TIMELINE_INFO_MESA: u32 = 1000384005;

/// The decoded result of [`find_get_device_queue2`]: the application's per-queue `ring_idx`, and
/// where in the stream the command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GetDeviceQueue2 {
    /// The `VkDeviceQueueTimelineInfoMESA.ringIdx` Mesa assigned this queue — an integer ≥ 1 that is
    /// the app's real per-queue timeline index. Fencing on it (once the queue is registered on the
    /// host) is a genuine GPU-completion barrier; fencing on `0` is not, and fencing on a wrong value
    /// is render-server-fatal. See `docs/design/2026-07-19-c2-ringidx-decode.md`.
    pub ring_idx: u32,
    /// Byte offset of the **first byte past** this command, relative to the start of `stream`. When
    /// `stream` is the ring's linear command buffer, this equals the command's free-running ring
    /// position **because `vkGetDeviceQueue2` is decoded during device init, before the ring first
    /// wraps** — so a caller can gate on `head >= end_offset` (both free-running) to know the host's
    /// ring thread has dispatched this command, and that stays true for the rest of the run even after
    /// the ring wraps.
    pub end_offset: usize,
    /// The `VkDevice` handle (Venus object id) this queue belongs to. Kept so the caller can later
    /// recognise the matching [`find_destroy_device`] — i.e. close the gate when *this* device is
    /// destroyed, not some other.
    pub device_handle: u64,
    /// The `VkQueue` handle (Venus object id) this call returned — the queue the application submits
    /// its rendering to. Kept so [`find_queue_submit`] can recognise *this* queue's `vkQueueSubmit`.
    pub queue_handle: u64,
}

/// Find the application's `vkGetDeviceQueue2` in a Venus command stream and read its `ring_idx`.
///
/// # Why this is a signature scan and not part of the linear walk
/// [`decode_commands`] cannot reach this command: it walks from the stream's start and stops at the
/// first command it cannot size, and the app's init emits several variable-size commands
/// (`vkCreateInstance`, `vkCreateDevice`, …) *before* `vkGetDeviceQueue2`. So this instead scans for
/// the command's fixed 80-byte signature directly. That is not the "guess a size and desynchronize"
/// failure [`decode_commands`] refuses — it matches on **four independent, self-verifying constants**:
/// the command type (155), the async command flags (0), and two 32-bit `VkStructureType` magic words
/// ([`STYPE_DEVICE_QUEUE_INFO_2`], [`STYPE_DEVICE_QUEUE_TIMELINE_INFO_MESA`]). A coincidental match of
/// all four in unrelated argument bytes is astronomically unlikely.
///
/// # Inputs / outputs
/// - `stream`: a Venus command stream — normally the ring's **linear** command buffer
///   `&blob[RING_BUFFER_OFFSET..][..applied_tail]`. The app's queue is obtained during device init at a
///   tiny `tail`, far below the buffer size, so it is decoded before the ring first wraps (see the
///   design doc — the ring *does* wrap later in the run, but not this early).
/// - Returns `Some(GetDeviceQueue2)` for the **first** match, or `None` if the command is not present
///   (or not yet fully in `stream`). For (c)1's single-queue configuration the first match is the
///   app's one queue; multiple queues are out of scope (design doc §6).
///
/// # Failure modes / pitfalls
/// - Cannot panic: every read is bounds-checked, and a candidate too close to the end to hold all 80
///   bytes is skipped rather than read out of bounds.
/// - Scans on a **4-byte stride**: Venus encodes every command 4-byte-aligned (`vn_encode` asserts
///   `size % 4 == 0`) and the ring buffer starts 4-aligned, so a command can only begin at a
///   multiple-of-4 offset. Scanning every byte would be slower and could only ever match the same
///   aligned offsets.
/// - `None` is not "the app has no queue"; it can equally mean "the delta carrying it has not arrived
///   yet". A caller latching the result must keep calling until it returns `Some`.
pub fn find_get_device_queue2(stream: &[u8]) -> Option<GetDeviceQueue2> {
    // Step by 4: every Venus command begins on a 4-byte boundary (see the doc comment). The last
    // possible start is `len - 80`; `step_by` naturally stops before running past it.
    let mut offset = 0usize;
    while offset + GET_DEVICE_QUEUE2_SIZE <= stream.len() {
        // The four magic words. `read_u32_le` is bounds-checked, but the loop condition already
        // guarantees all of `[offset, offset + 80)` is in range, so none of these can be `None`.
        let is_match = read_u32_le(stream, offset) == Some(VK_COMMAND_TYPE_VK_GET_DEVICE_QUEUE2)
            // Async command flags: `vkGetDeviceQueue2` is emitted `vn_async_*`, so flags == 0.
            && read_u32_le(stream, offset + 4) == Some(0)
            // The outer struct's sType — a core Vulkan constant, written raw little-endian.
            && read_u32_le(stream, offset + QUEUE_INFO2_STYPE_OFFSET) == Some(STYPE_DEVICE_QUEUE_INFO_2)
            // The MESA timeline struct's sType — the decisive, extension-specific magic word.
            && read_u32_le(stream, offset + TIMELINE_INFO_STYPE_OFFSET)
                == Some(STYPE_DEVICE_QUEUE_TIMELINE_INFO_MESA);
        if is_match {
            // All four constants agreed; `ring_idx` at +48 is the value we came for, and the
            // `VkDevice` handle at +8 identifies the device so its later destroy can be recognised.
            let ring_idx = read_u32_le(stream, offset + RING_IDX_OFFSET)?;
            let device_handle = read_u64_le(stream, offset + DEVICE_HANDLE_OFFSET)?;
            let queue_handle =
                read_u64_le(stream, offset + GET_DEVICE_QUEUE2_QUEUE_HANDLE_OFFSET)?;
            return Some(GetDeviceQueue2 {
                ring_idx,
                end_offset: offset + GET_DEVICE_QUEUE2_SIZE,
                device_handle,
                queue_handle,
            });
        }
        offset += 4;
    }
    None
}

/// `VK_COMMAND_TYPE_vkQueueSubmit_EXT` and `_vkQueueSubmit2_EXT` — the commands that submit rendering
/// (and, for a readback frame, the copy-to-buffer) to the application's queue. Both encode an
/// identical fixed prefix; [`find_queue_submit`] matches either. Source: `vn_protocol_driver_defines.h`.
pub const VK_COMMAND_TYPE_VK_QUEUE_SUBMIT: u32 = 18;
/// See [`VK_COMMAND_TYPE_VK_QUEUE_SUBMIT`].
pub const VK_COMMAND_TYPE_VK_QUEUE_SUBMIT2: u32 = 206;

/// Byte offset of `submitCount` (`u32`) within an encoded `vkQueueSubmit`/`vkQueueSubmit2`.
const QUEUE_SUBMIT_COUNT_OFFSET: usize = 16;
/// Byte offset of the `pSubmits` array-size marker (`u64`, equal to `submitCount` when non-NULL).
const QUEUE_SUBMIT_ARRAY_MARKER_OFFSET: usize = 20;
/// Bytes of a `vkQueueSubmit` prefix this decoder reads (through the array-size marker). The rest of
/// the command (the per-batch submit infos and the trailing `VkFence`) is variable and not read.
const QUEUE_SUBMIT_PREFIX_SPAN: usize = QUEUE_SUBMIT_ARRAY_MARKER_OFFSET + 8;

/// Find the **latest** `vkQueueSubmit`/`vkQueueSubmit2` for a specific queue in a Venus command stream.
///
/// # Why S needs this — firing the readback fence exactly once the submit is in the ring
/// The completion fence must be issued *after* the application's own `vkQueueSubmit` has crossed the
/// ring, or S's fence (via the render-server context-op path, which can overtake the ring thread)
/// enqueues its empty submit ahead of the application's and ships a torn readback. Watching the ring
/// merely *drain* is not enough: a synchronous frame's commands arrive in several deltas, so the ring
/// is transiently drained between them — before the submit delta arrives. Knowing the position of the
/// latest submit lets S wait until a submit **newer than the last delivered one** is present (and then
/// dispatched, which a drained ring proves) — a structural signal, not a timing guess. Returning the
/// *latest* match (not the first) is what makes "newer than last delivered" a simple offset compare.
///
/// # The signature
/// Matches `u32@X ∈ {18, 206}` (submit / submit2 — identical prefix), `u32@X+4 == 0` (async flags),
/// `u64@X+8 == queue_handle` (this queue), `u32@X+16 == submitCount ≥ 1`, and `u64@X+20 ==
/// submitCount` (the array-size marker equals the count). The queue handle plus that count/marker
/// self-consistency makes a coincidental match in unrelated argument bytes vanishingly unlikely — which
/// matters, because a *false* match that is newer than the real submit would fire the fence early and
/// tear the frame. Source offsets: `docs/design/2026-07-19-c2-ringidx-decode.md` and Mesa
/// `vn_protocol_driver_queue.h`.
///
/// # Inputs / outputs
/// - `stream`: a Venus command stream (normally the ring's linear buffer `&blob[RING_BUFFER_OFFSET..]`).
/// - `queue_handle`: the `VkQueue` id from the latched [`GetDeviceQueue2::queue_handle`].
/// - Returns `Some(offset)` of the **last** matching submit's first byte (relative to `stream`), or
///   `None` if no submit for this queue is present. Cannot panic (bounds-checked); 4-byte stride.
pub fn find_queue_submit(stream: &[u8], queue_handle: u64) -> Option<usize> {
    let mut latest = None;
    let mut offset = 0usize;
    while offset + QUEUE_SUBMIT_PREFIX_SPAN <= stream.len() {
        let ty = read_u32_le(stream, offset);
        let submit_count = read_u32_le(stream, offset + QUEUE_SUBMIT_COUNT_OFFSET);
        let is_match = (ty == Some(VK_COMMAND_TYPE_VK_QUEUE_SUBMIT)
            || ty == Some(VK_COMMAND_TYPE_VK_QUEUE_SUBMIT2))
            // Async flags — the guest emits submits fire-and-forget (flags 0) by default.
            && read_u32_le(stream, offset + 4) == Some(0)
            // This queue, not another — the decisive discriminator.
            && read_u64_le(stream, offset + DEVICE_HANDLE_OFFSET) == Some(queue_handle)
            // A real batch (>= 1) whose array-size marker agrees with the count: a strong internal
            // consistency check that stray argument bytes will not satisfy.
            && submit_count.is_some_and(|c| c >= 1)
            && read_u64_le(stream, offset + QUEUE_SUBMIT_ARRAY_MARKER_OFFSET)
                == submit_count.map(u64::from);
        if is_match {
            // Keep scanning: we want the *latest* submit, so remember this and look for a later one.
            latest = Some(offset);
        }
        offset += 4;
    }
    latest
}

/// Find a `vkDestroyDevice` for a specific device in a Venus command stream.
///
/// # Why S needs this — closing the readback gate before the queue disappears
/// Once the host dispatches `vkDestroyDevice`, the per-queue timeline registered at
/// `vkGetDeviceQueue2` is gone, and a fence on that `ring_idx` becomes render-server-fatal (it kills
/// the context → the application `SIGABRT`s). S must therefore stop issuing readback fences the moment
/// this command appears. Because the application is synchronous, this command only arrives during
/// teardown, strictly after the last frame has been delivered — so detecting it here (in the message
/// thread, as the delta is applied, *before* the doorbell that lets the host dispatch it) closes the
/// gate with no fence ever racing the queue's destruction. See
/// `docs/design/2026-07-19-c2-ringidx-decode.md` §7.
///
/// # The signature
/// Matches `u32@X == 12` (`vkDestroyDevice_EXT`), `u32@X+4 == 0` (async flags), and
/// `u64@X+8 == device_handle` — the **same device** whose queue was latched. Requiring the specific
/// device handle (rather than the type alone) is what keeps this from firing on unrelated argument
/// bytes that merely start with a `12`. The command's `pAllocator` tail is not read, so a match holds
/// whether or not the application passed an allocator.
///
/// # Inputs / outputs
/// - `stream`: a Venus command stream (normally the ring's linear buffer `&blob[RING_BUFFER_OFFSET..]`);
///   only *presence* is used (there is one destroy per session), so a wrapped buffer is fine.
/// - `device_handle`: the `VkDevice` id from the latched [`GetDeviceQueue2::device_handle`].
/// - Returns `Some(offset)` of the first match (the command's first byte, relative to `stream`), or
///   `None` if this device's `vkDestroyDevice` is not present.
///
/// # Failure modes / pitfalls
/// Cannot panic: bounds-checked, and a candidate too close to the end to hold the 16 read bytes is
/// skipped. Scans on a 4-byte stride, like [`find_get_device_queue2`], for the same alignment reason.
/// A false *positive* (matching non-destroy bytes) would close the gate early and, on the next real
/// readback, wedge — but the type + async-flags + exact-device-handle triple makes that vanishingly
/// unlikely, and the caller's registration deadline turns even that into a loud session end rather
/// than a silent hang. A false *negative* (missing a real destroy) is the dangerous direction — it
/// re-admits the fatal teardown fence — so the signature is kept minimal and exact rather than
/// over-constrained.
pub fn find_destroy_device(stream: &[u8], device_handle: u64) -> Option<usize> {
    // The bytes this reads span `[offset, offset + 16)` — type (4) + flags (4) + device (8).
    const READ_SPAN: usize = DEVICE_HANDLE_OFFSET + 8;
    let mut offset = 0usize;
    while offset + READ_SPAN <= stream.len() {
        let is_match = read_u32_le(stream, offset) == Some(VK_COMMAND_TYPE_VK_DESTROY_DEVICE)
            // Async command flags: `vkDestroyDevice` is void, emitted `vn_async_*`, so flags == 0.
            && read_u32_le(stream, offset + 4) == Some(0)
            // The decisive discriminator: this must be *this* device's destroy, not a stray `12`.
            && read_u64_le(stream, offset + DEVICE_HANDLE_OFFSET) == Some(device_handle);
        if is_match {
            return Some(offset);
        }
        offset += 4;
    }
    None
}
