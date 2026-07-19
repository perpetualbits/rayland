//! **Real bytes, captured from a live Mesa Venus client**, and the tests that decode them.
//!
//! # Provenance — read this before touching a single value below
//! These dwords were observed on **2026-07-15** by `mmap`ing the shared pages behind the first blob
//! resource a live **Mesa 26.0.3** Venus ICD created, while it drove an init-only Vulkan workload
//! (instance → device → command pool → image → memory → bind) against `libvirglrenderer` 1.2.0 on
//! an Intel Iris Xe (RPL-P) via `/dev/dri/renderD128`. The snapshot is the sampler's *first-data*
//! capture, taken ~2 ms into the session — early enough that the ring had not yet been drained or
//! overwritten. It was **byte-identical across two independent runs** of the same workload.
//!
//! They are transcribed verbatim from the capture's hex output. They are **not** synthesized, not
//! regenerated, and — critically — **not constructed from this module's own decoder**. That last
//! point is the whole value of the fixture: if these bytes had been produced by encoding what the
//! decoder expects, the tests below would prove nothing but that the decoder agrees with itself.
//! Because they are a memory image of what Mesa actually wrote, the decoder's independently-derived
//! command sizes agreeing with the host's `head` counter is *evidence*.
//!
//! **Do not "fix" a value here to make a test pass.** These bytes are an observation. If the
//! decoder disagrees with them, the decoder is wrong.
//!
//! # Why this fixture exists at all
//! The capture originally lived only in a scratch directory that `git clean -fdx` deletes. This
//! module is the finding's durable form: it needs no GPU, no Mesa, no virglrenderer and no network
//! to run, so CI re-proves on every commit that the Venus command ring carries a legible Vulkan
//! command stream.
//!
//! # What is preserved, and what is not
//! Only the **first 292 bytes** of the 131268-byte ring: the 192-byte control area plus the first
//! 100 bytes of the command buffer. That is what the diagnostic printed and therefore all that
//! exists. It is enough to cover the three commands the host had consumed, and one byte past them.
//! The client had by then produced 216 bytes (`tail`), so the fixture holds *less* than the client
//! wrote — which is why the decoder stops where it does and why nothing here exercises a ring wrap.

use super::decode::{
    DecodeStop, GetDeviceQueue2, VK_COMMAND_TYPE_VK_CREATE_INSTANCE,
    VK_COMMAND_TYPE_VK_ENUMERATE_INSTANCE_VERSION, VK_COMMAND_TYPE_VK_SET_REPLY_COMMAND_STREAM_MESA,
    decode_commands, decode_reply_command_stream, encoded_size, find_get_device_queue2,
};
use super::{RING_BUFFER_OFFSET, RING_HEAD_OFFSET, RING_TAIL_OFFSET};

/// The captured ring prefix, as the little-endian `u32` values the client's CPU wrote.
///
/// Stored as dwords rather than bytes because that is the unit the ring is written and observed in
/// (the diagnostic reads it with `read_volatile::<u32>` and prints `{:08x}`), so a reviewer can
/// diff this table against the capture's hex output line for line.
///
/// The layout of the table below mirrors the capture's own 8-dwords-per-line hex rows. Byte offsets
/// are in the row comments. The regions:
/// - `0x00..0xc0` — the ring's 192-byte control area.
/// - `0xc0..0x124` — the first 100 bytes of the command buffer.
#[rustfmt::skip]
const CAPTURED_RING_PREFIX: [u32; 73] = [
    // --- control area, verbatim from the capture's "suspected control area" dump ---------------
    // 0x000000: `head` = 0x58 = 88 — the host had consumed 88 bytes at this instant. Everything
    // else in this 64-byte slot is padding to keep `head` off `tail`'s cache line.
    0x00000058, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    // 0x000020: still `head`'s 64-byte slot.
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    // 0x000040: `tail` = 0xd8 = 216 — the client had produced 216 bytes. It runs ahead of `head`,
    // as a producer must.
    0x000000d8, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    // 0x000060: still `tail`'s 64-byte slot.
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    // 0x000080: `status` = 0 — no bits set, so the host's ring thread was **actively polling** at
    // this instant. (Mesa's bitmask: bit 0 = IDLE. Over the full run `status` was observed to go
    // 0 -> 1 -> 0, i.e. polling, then parked once nothing arrived for 1 ms, then cleared.)
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    // 0x0000a0: still `status`'s 64-byte slot; the control area ends at 0xc0.
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    // --- command buffer ------------------------------------------------------------------------
    // 0x0000c0: command 1 begins: 0xb2 = 178 = vkSetReplyCommandStreamMESA, flags 0, present
    // marker, then resourceId=2, offset=0 (u64), size=0x14=20 (u64, spanning into the next row).
    0x000000b2, 0x00000000, 0x00000001, 0x00000000, 0x00000002, 0x00000000, 0x00000000, 0x00000014,
    // 0x0000e0: command 1's `size` high dword, then command 2: 0x89 = 137 =
    // vkEnumerateInstanceVersion, flags 1 (wants a reply), present marker. Then command 3: another
    // 0xb2 = 178 at 0xf4.
    0x00000000, 0x00000089, 0x00000001, 0x00000001, 0x00000000, 0x000000b2, 0x00000000, 0x00000001,
    // 0x000100: command 3's present-marker high dword.
    //
    // PROVENANCE NOTE: this single dword is the one value not inside the capture's verbatim
    // "first 256 bytes" hex block, which ends at 0x100. It is zero on two independent grounds:
    // the capture's hand-decode records `0x100 = 00000000`, and the diagnostic's own
    // "first non-zero dword beyond the control area" search reported byte 0x104 — which it could
    // only do if 0x100 were zero. It is transcribed here, not assumed.
    0x00000000,
    // 0x000104: the capture's second hex region, printed from the first non-zero dword past the
    // control area. Command 3's body: resourceId=2, offset=0x14=20 (u64) — chaining off command
    // 1's offset 0 + size 20 — and size=0x18=24 (u64). Then at 0x118 command 4 begins: cmd_type 0
    // = vkCreateInstance, flags 1. That is where the decoder stops.
    0x00000002, 0x00000014, 0x00000000, 0x00000018, 0x00000000, 0x00000000, 0x00000001, 0x00000001,
];

/// The captured prefix as a byte image of the original x86-64 shared mapping.
///
/// # Inputs / outputs
/// Takes nothing; returns 292 bytes — [`CAPTURED_RING_PREFIX`] flattened little-endian.
///
/// # Why little-endian rather than native
/// The dwords were captured on x86-64, so the bytes Mesa actually placed in memory are their
/// little-endian encoding. Emitting them with `to_le_bytes` (and decoding them with
/// `from_le_bytes`) reconstructs that exact image on **any** host, so this test means the same
/// thing on a big-endian machine instead of quietly passing for the wrong reason. Rayland
/// explicitly targets heterogeneous architectures, so this is not a hypothetical.
fn captured_ring_bytes() -> Vec<u8> {
    CAPTURED_RING_PREFIX
        .iter()
        .flat_map(|dword| dword.to_le_bytes())
        .collect()
}

/// Read one of the ring's control words out of the captured image.
///
/// # Inputs / outputs
/// - `ring`: the byte image from [`captured_ring_bytes`].
/// - `offset`: one of [`RING_HEAD_OFFSET`], [`RING_TAIL_OFFSET`] or [`super::RING_STATUS_OFFSET`].
/// - Returns the control word's value.
///
/// # Failure modes
/// Panics if the image is too short — acceptable and desirable in a test: a fixture that cannot
/// hold its own control area is broken, and failing loudly beats returning a plausible zero.
fn control_word(ring: &[u8], offset: usize) -> u32 {
    let field: [u8; 4] = ring[offset..offset + 4]
        .try_into()
        .expect("the captured image covers the whole 192-byte control area");
    u32::from_le_bytes(field)
}

/// The ring's control words are where the layout says they are, and say what the capture saw.
///
/// This is the layout assertion: it reads [`RING_HEAD_OFFSET`], [`RING_TAIL_OFFSET`] and
/// [`super::RING_STATUS_OFFSET`] out of real captured memory and checks the values are the ones
/// observed. If someone edits a layout constant, this fails — because the constants and the bytes
/// are independent facts that happen to agree, which is exactly the property worth guarding.
#[test]
fn captured_control_words_sit_at_the_declared_offsets() {
    let ring = captured_ring_bytes();

    // `head`: the host had consumed 88 bytes. The decode test below shows 88 is a command boundary.
    assert_eq!(control_word(&ring, RING_HEAD_OFFSET), 88);
    // `tail`: the client had produced 216 bytes — ahead of `head`, as a producer must be. A `head`
    // that ever passed `tail` would mean the host consumed bytes that were never written.
    assert_eq!(control_word(&ring, RING_TAIL_OFFSET), 216);
    assert!(
        control_word(&ring, RING_HEAD_OFFSET) < control_word(&ring, RING_TAIL_OFFSET),
        "head must trail tail: the host cannot consume what the client has not produced"
    );
    // `status` = 0: no bits set (Mesa's bit 0 = IDLE), so the host's ring thread was actively
    // polling when this snapshot was taken — which is exactly why no doorbell was needed here.
    // Mesa only sends one once it sees the IDLE bit, i.e. once the host has stopped looking.
    assert_eq!(control_word(&ring, super::RING_STATUS_OFFSET), 0);

    // Neither counter had wrapped: both are far below the buffer size, which is precisely why this
    // fixture cannot speak to wrap behaviour (see the parent module's scope limits).
    assert!(
        (control_word(&ring, RING_TAIL_OFFSET) as usize) < super::RING_BUFFER_SIZE,
        "the captured ring never wrapped; nothing here tests modulo indexing"
    );
}

/// **The headline test: real Venus Vulkan commands decode out of the captured ring bytes.**
///
/// This is the durable form of the sub-project's central finding — that Mesa's Venus ICD ships its
/// Vulkan commands through shared memory, in the same `vn_cs_encoder` language the vtest socket's
/// inline path uses. It runs anywhere: no GPU, no Mesa, no virglrenderer, no network.
///
/// What makes it evidence rather than circular: the command **sizes** (36/16/36) come from
/// [`encoded_size`], summed from Mesa's generated `vn_sizeof_*` field by field; the **bytes** come
/// from a live client. The two were derived independently and agree.
#[test]
fn captured_ring_bytes_decode_as_venus_vulkan_commands() {
    let ring = captured_ring_bytes();
    // Slice the command buffer out of the ring image. Linear, not modulo: the captured ring never
    // wrapped, and `decode_commands` has no wrap handling by design.
    let stream = &ring[RING_BUFFER_OFFSET..];
    assert_eq!(stream.len(), 100, "the capture preserved 100 buffer bytes");

    let (commands, stop) = decode_commands(stream);

    // Exactly the three commands the host had already consumed. Nothing was invented, and the walk
    // did not run past what it can justify.
    assert_eq!(commands.len(), 3, "three whole commands were decoded");

    // Command 1 — the reply channel is set up before anything that needs an answer is issued.
    assert_eq!(commands[0].offset, 0);
    assert_eq!(
        commands[0].command_type,
        VK_COMMAND_TYPE_VK_SET_REPLY_COMMAND_STREAM_MESA
    );
    assert_eq!(
        commands[0].command_flags, 0,
        "no reply wanted for a setup command"
    );
    assert_eq!(
        commands[0].encoded_size, 36,
        "Mesa's vn_sizeof: 4+4+8+4+8+8"
    );

    // Command 2 — the first real Vulkan call. Its flags ask for a reply, which is why command 1
    // had to come first.
    assert_eq!(
        commands[1].offset, 36,
        "starts exactly where command 1 ends"
    );
    assert_eq!(
        commands[1].command_type,
        VK_COMMAND_TYPE_VK_ENUMERATE_INSTANCE_VERSION
    );
    assert_eq!(commands[1].command_flags, 1, "bit 0: generate a reply");
    assert_eq!(commands[1].encoded_size, 16, "Mesa's vn_sizeof: 4+4+8");

    // Command 3 — a second reply-channel setup, ahead of the next answering call.
    assert_eq!(commands[2].offset, 52, "36 + 16");
    assert_eq!(
        commands[2].command_type,
        VK_COMMAND_TYPE_VK_SET_REPLY_COMMAND_STREAM_MESA
    );
    assert_eq!(commands[2].command_flags, 0);
    assert_eq!(commands[2].encoded_size, 36);

    // The walk stops at command 4 — `vkCreateInstance`, whose encoding is variable-length because
    // it carries application strings and an extension list. Stopping is the correct answer: see
    // `decode_commands`'s pitfalls on why guessing a size would desynchronize everything after it.
    assert_eq!(
        stop,
        DecodeStop::UnknownCommandSize {
            offset: 88,
            command_type: VK_COMMAND_TYPE_VK_CREATE_INSTANCE,
        },
        "the fourth command is vkCreateInstance, which this decoder cannot size"
    );
    // And that stop is a real limit of our knowledge, not of the data: the client had produced 216
    // bytes, so vkCreateInstance's body genuinely exists — beyond both our size table and the
    // 100 bytes the capture preserved.
    assert_eq!(encoded_size(VK_COMMAND_TYPE_VK_CREATE_INSTANCE), None);
}

/// **The consumption arithmetic closes: `head` = 88 = 36 + 16 + 36.**
///
/// The single most convincing line of the whole capture, and the reason it deserves its own test.
///
/// `head` is written by the *host* (virglrenderer's `vkr_ring` thread) and says how many bytes it
/// consumed. The sizes 36/16/36 are ours, summed from Mesa's `vn_sizeof_*` headers. Two entirely
/// separate parties — a C ring thread on one side, a Rust size table derived from generated
/// headers on the other — independently arrive at 88. There is no way for that to be a coincidence
/// or an artifact of how the fixture was built: had the decoder mis-sized even one command, the sum
/// would miss `head` and this would fail.
///
/// It also proves `head` lands on a **command boundary**, which is what a consumer's cursor into a
/// well-framed command stream must always do.
#[test]
fn head_equals_the_summed_sizes_of_the_consumed_commands() {
    let ring = captured_ring_bytes();
    let stream = &ring[RING_BUFFER_OFFSET..];

    // What the host said it consumed — captured from its own control word.
    let head = control_word(&ring, RING_HEAD_OFFSET) as usize;

    // What we say those bytes contain, sized from Mesa's headers with no reference to `head`.
    let (commands, _) = decode_commands(stream);
    let decoded_bytes: usize = commands.iter().map(|c| c.encoded_size).sum();

    assert_eq!(decoded_bytes, 88, "36 + 16 + 36");
    assert_eq!(
        head, decoded_bytes,
        "the host's own byte counter lands exactly on our third command's boundary"
    );
    // Stated the long way round, because this identity is the finding.
    assert_eq!(head, 36 + 16 + 36);
}

/// The reply-stream offsets chain, and name the second blob as the reply arena.
///
/// A second independent cross-check on the decode, using a relationship *between* two commands
/// rather than any single value: each `vkSetReplyCommandStreamMESA` points at where the next reply
/// goes, and the second one starts exactly where the first one's reservation ended
/// (`offset=0, size=20` then `offset=20, size=24`). A decoder that had lost the stream frame, or
/// that had the descriptor's field layout wrong, could not produce a closing chain.
///
/// Both name `resource_id = 2` — the client's second blob, the 1 MiB one. That is how the reply
/// arena was identified, and the capture confirmed it from the other side: that blob's contents at
/// offset 0 held `vkEnumerateInstanceVersion`'s answer, and at offset 20 — exactly where the second
/// reservation pointed — `vkCreateInstance`'s.
#[test]
fn reply_command_streams_chain_and_name_the_reply_arena() {
    let ring = captured_ring_bytes();
    let stream = &ring[RING_BUFFER_OFFSET..];
    let (commands, _) = decode_commands(stream);

    let first = decode_reply_command_stream(stream, &commands[0])
        .expect("command 1 is a reply-stream command with a non-NULL descriptor");
    let second = decode_reply_command_stream(stream, &commands[2])
        .expect("command 3 is a reply-stream command with a non-NULL descriptor");

    // Both point at blob 2: the reply arena, distinct from blob 1 (this ring).
    assert_eq!(first.resource_id, 2, "the 1 MiB blob is the reply arena");
    assert_eq!(second.resource_id, 2);

    // The first reservation: 20 bytes at the arena's start, for vkEnumerateInstanceVersion's reply.
    assert_eq!(first.offset, 0);
    assert_eq!(first.size, 20);

    // The second begins exactly where the first ended — the chain that has to close.
    assert_eq!(second.offset, 20);
    assert_eq!(second.size, 24);
    assert_eq!(
        second.offset,
        first.offset + first.size,
        "consecutive reply reservations must tile the arena without gap or overlap"
    );

    // The command between them is the one whose reply the first reservation was for: the client
    // sets up the channel, then issues the call. Ordering is the mechanism, not a coincidence.
    assert_eq!(
        commands[1].command_type,
        VK_COMMAND_TYPE_VK_ENUMERATE_INSTANCE_VERSION
    );
    assert_eq!(
        commands[1].command_flags, 1,
        "it wanted the reply that reservation holds"
    );

    // A non-reply-stream command must not be decoded as one; the guard exists so a caller cannot
    // read a neighbouring command's bytes as descriptor fields.
    assert_eq!(decode_reply_command_stream(stream, &commands[1]), None);
}

/// Truncating the captured stream inside a command's *body* — after a complete, readable
/// prologue — is reported, not silently mis-decoded.
///
/// Built from the real fixture rather than synthetic bytes so it exercises the same path the live
/// window does: the capture itself preserved 100 of the client's 216 produced bytes, so decoding a
/// window that cuts through a command is the *normal* case here, and it must never manufacture a
/// command from bytes that are not all present. This specific cut point exercises the "the command
/// is sizeable but its bytes are not all here" branch (`decode.rs`'s `offset + encoded_size >
/// stream.len()` check) — the one that only ever fires once a command's size is already known from
/// a whole prologue, as distinct from the prologue-truncation test below.
#[test]
fn a_stream_cut_mid_command_body_stops_as_truncated() {
    let ring = captured_ring_bytes();
    // Command 2 starts at offset 36. Take its full 8-byte `[type][flags]` prologue (so its
    // `encoded_size` — 16 bytes — is known) plus 2 of its 8 remaining body bytes, so the prologue
    // read succeeds but the subsequent bounds check on the whole command does not.
    let stream = &ring[RING_BUFFER_OFFSET..RING_BUFFER_OFFSET + 46];

    let (commands, stop) = decode_commands(stream);

    // Command 1 is whole and is still reported: a partial tail does not discard good commands.
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].encoded_size, 36);
    // Command 2 is not: 16 bytes were needed and only 10 (8-byte prologue + 2 body bytes) remain.
    assert_eq!(stop, DecodeStop::Truncated { offset: 36 });
}

/// Truncating the captured stream *before* a command's prologue is even fully readable is reported
/// the same way — `DecodeStop::Truncated` at the command's offset — but through a different guard
/// in the decoder: the `[type][flags]` read itself comes up short, before any command size is ever
/// known. Kept as its own test (rather than folded into the body-truncation test above) so this
/// earlier guard has direct coverage independent of the later bounds check.
#[test]
fn a_stream_cut_before_command_prologue_stops_as_truncated() {
    let ring = captured_ring_bytes();
    // Cut two bytes into command 2's 8-byte prologue, so not even `[type][flags]` can be read.
    let stream = &ring[RING_BUFFER_OFFSET..RING_BUFFER_OFFSET + 38];

    let (commands, stop) = decode_commands(stream);

    // Command 1 is whole and is still reported: a partial tail does not discard good commands.
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].encoded_size, 36);
    // Command 2's prologue itself is incomplete: only 2 of its 8 bytes are present.
    assert_eq!(stop, DecodeStop::Truncated { offset: 36 });
}

/// A stream cut exactly on a command boundary consumes every byte and reports a clean end.
///
/// The counterpart to the truncation test, and the outcome that matters for a live reader: slicing
/// the captured buffer at the host's own `head` accounts for every byte with whole commands and no
/// remainder — the signature of a stream frame that was never lost.
#[test]
fn a_stream_cut_at_head_consumes_every_byte() {
    let ring = captured_ring_bytes();
    let head = control_word(&ring, RING_HEAD_OFFSET) as usize;
    // Exactly the bytes the host consumed — a boundary it chose, not one we picked.
    let stream = &ring[RING_BUFFER_OFFSET..RING_BUFFER_OFFSET + head];

    let (commands, stop) = decode_commands(stream);

    assert_eq!(commands.len(), 3);
    assert_eq!(stop, DecodeStop::ReachedEnd, "no bytes left over at head");
}

/// **Real `vkGetDeviceQueue2` bytes, captured from a live Mesa Venus client on 2026-07-19.**
///
/// # Provenance — read this before touching a single value below
/// These 20 little-endian dwords are the 80-byte `vkGetDeviceQueue2` command as it appeared in a
/// live icosa-fixture Venus session's command ring on **2026-07-19**, dumped by a throwaway spike
/// (a signature scan added to `rayland-s`'s `RingDelta` handler, then reverted) that printed the
/// command's raw bytes the first time it saw it. They are a **memory image of what Mesa actually
/// wrote**, transcribed verbatim from that hex output — **not** synthesized, and **not** produced by
/// any encoder in this repository. That independence is the whole value: [`find_get_device_queue2`]
/// deriving `ring_idx = 1` from bytes Mesa wrote is *evidence* the decode is right; had the bytes been
/// built from what the decoder expects, the test below would prove only that it agrees with itself.
///
/// **Do not "fix" a value here to make a test pass.** These bytes are an observation. If the decoder
/// disagrees with them, the decoder is wrong.
///
/// The full field-by-field derivation (offset table, the four signature magic words, and the
/// `ring_idx = 1` finding) is in `docs/design/2026-07-19-c2-ringidx-decode.md` §1 and
/// `.superpowers/sdd/ringidx-decode-findings.md`. Byte offsets in the row comments below.
#[rustfmt::skip]
const CAPTURED_GET_DEVICE_QUEUE2: [u32; 20] = [
    // off 0x00: VkCommandTypeEXT = 155 (vkGetDeviceQueue2_EXT) | off 0x04: VkCommandFlagsEXT = 0 (async)
    // off 0x08: VkDevice handle = 3 (u64, lo/hi)
    0x0000_009b, 0x0000_0000, 0x0000_0003, 0x0000_0000,
    // off 0x10: pQueueInfo presence marker = 1 (u64) | off 0x18: VkDeviceQueueInfo2.sType = 1000145003
    0x0000_0001, 0x0000_0000, 0x3b9d_006b, /* off 0x1c: pNext[0] marker = 1 (u64) */ 0x0000_0001,
    // off 0x20: pNext marker hi | off 0x24: VkDeviceQueueTimelineInfoMESA.sType = 1000384005
    // off 0x28: inner pNext marker = 0 (NULL, u64)
    0x0000_0000, 0x3ba0_a605, 0x0000_0000, 0x0000_0000,
    // off 0x30: ringIdx = 1  | off 0x34: flags = 0 | off 0x38: queueFamilyIndex = 0 | off 0x3c: queueIndex = 0
    0x0000_0001, 0x0000_0000, 0x0000_0000, 0x0000_0000,
    // off 0x40: pQueue marker = 1 (u64) | off 0x48: pQueue VkQueue handle = 5 (u64, out-param)
    0x0000_0001, 0x0000_0000, 0x0000_0005, 0x0000_0000,
];

/// The captured `vkGetDeviceQueue2` as a byte image of the original x86-64 ring memory. See
/// [`captured_ring_bytes`] for why little-endian rather than native.
fn captured_gdq2_bytes() -> Vec<u8> {
    CAPTURED_GET_DEVICE_QUEUE2
        .iter()
        .flat_map(|dword| dword.to_le_bytes())
        .collect()
}

/// **The headline test: the app's real `ring_idx` decodes out of real captured bytes.**
///
/// [`find_get_device_queue2`] reads `ring_idx = 1` from 80 bytes Mesa actually wrote — the value the
/// (c)2 completion fence must use. It is exactly the value the earlier `finish-report` tried and saw
/// rejected as fatal; the difference is not the value but *when* the fence is issued (after the queue
/// is registered), which is the caller's concern, not this decoder's.
#[test]
fn get_device_queue2_ring_idx_decodes_from_real_bytes() {
    let bytes = captured_gdq2_bytes();
    assert_eq!(bytes.len(), 80, "the command is fixed at 80 bytes");

    let found = find_get_device_queue2(&bytes).expect("the captured command must be recognized");
    assert_eq!(
        found,
        GetDeviceQueue2 { ring_idx: 1, end_offset: 80 },
        "real Mesa bytes yield ring_idx=1, and the command ends at byte 80"
    );
}

/// The command is found at its true offset when embedded in a larger stream, the way it really sits
/// in the ring (preceded and followed by other commands' bytes).
///
/// The prefix and suffix are arbitrary filler chosen so as not to contain the signature — the point
/// is that the scan locates the command at a non-zero, 4-aligned offset and reports `end_offset`
/// relative to the stream start, which is what lets the caller compare it against the ring's `head`.
#[test]
fn get_device_queue2_is_found_at_its_offset_within_a_stream() {
    let cmd = captured_gdq2_bytes();
    // 40 bytes of filler ahead of it (4-aligned, so the command still starts on a boundary), and
    // some trailing bytes after. None of the filler is the signature.
    let prefix = vec![0xAAu8; 40];
    let suffix = vec![0x55u8; 24];
    let mut stream = prefix.clone();
    stream.extend_from_slice(&cmd);
    stream.extend_from_slice(&suffix);

    let found = find_get_device_queue2(&stream).expect("the embedded command must be found");
    assert_eq!(found.ring_idx, 1);
    assert_eq!(
        found.end_offset,
        prefix.len() + cmd.len(),
        "end_offset is measured from the start of the stream, past the prefix"
    );
}

/// A stream that does not contain the command yields `None` — never a false positive.
///
/// Uses the captured ring prefix (real Venus commands: reply-stream setup, enumerate-version, …) —
/// bytes that are genuine command data but are *not* a `vkGetDeviceQueue2`. A scanner that matched
/// here would be reading argument bytes as a command header.
#[test]
fn a_stream_without_the_command_yields_none() {
    let ring = captured_ring_bytes();
    let stream = &ring[RING_BUFFER_OFFSET..];
    assert_eq!(
        find_get_device_queue2(stream),
        None,
        "the captured prefix has no vkGetDeviceQueue2 and must not spuriously match"
    );
}

/// A command truncated below its full 80 bytes yields `None`, never a partial read.
///
/// The delta carrying `vkGetDeviceQueue2` might not have fully arrived yet; a caller must not act on
/// half a command. Every prefix length short of 80 must refuse, including one that holds all four
/// signature words but not the `ring_idx` that follows them — a scanner that returned before checking
/// it had the whole command could read past its slice.
#[test]
fn a_truncated_command_yields_none() {
    let full = captured_gdq2_bytes();
    // Every length from 0 up to (but not including) the full 80 bytes must decline.
    for len in 0..full.len() {
        assert_eq!(
            find_get_device_queue2(&full[..len]),
            None,
            "a {len}-byte prefix is not a whole command and must not decode"
        );
    }
    // The whole 80 bytes, by contrast, does decode — the boundary is exactly at the command's size.
    assert!(find_get_device_queue2(&full).is_some());
}

/// Corrupting the decisive MESA sType magic word makes the scan miss — proving the match rests on
/// the signature, not on the command type alone.
///
/// `vkGetDeviceQueue2`'s type (155) could in principle collide with argument bytes of some other
/// command; the two 32-bit `VkStructureType` constants are what make a match unambiguous. This test
/// flips the `VkDeviceQueueTimelineInfoMESA.sType` (at offset 36) to a wrong value and confirms the
/// command is no longer recognized, so a real collision on the type word alone cannot yield a bogus
/// `ring_idx`.
#[test]
fn a_wrong_timeline_stype_defeats_the_match() {
    let mut bytes = captured_gdq2_bytes();
    // Offset 36..40 is the MESA timeline sType. Overwrite it with a value that is not the constant.
    bytes[36..40].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
    assert_eq!(
        find_get_device_queue2(&bytes),
        None,
        "with the MESA sType corrupted the signature no longer matches"
    );
}
