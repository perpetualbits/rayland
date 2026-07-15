//! Noticing Venus's **out-of-line command path** in a ring delta — without decoding the ring.
//!
//! # The problem this module exists for
//! Ring-findings §5.3 established (c)1's single biggest known scaling limit. If one submission's
//! encoded length exceeds `direct_size` = `buffer_size >> direct_order` = `131072 >> 4` = **8192
//! bytes**, Mesa does not put the commands in the ring. It puts a small
//! `vkExecuteCommandStreamsMESA` (`VkCommandTypeEXT` **180**) there instead, which *points at other
//! shmems by `res_id`* (`vn_ring.c:494-528`):
//!
//! ```c
//! descs[desc_count++] = (VkCommandStreamDescriptionMESA){
//!    .resourceId = buf->shmem->res_id,
//!    .offset     = buf->offset,
//!    .size       = buf->committed_size,
//! };
//! ```
//!
//! S's virglrenderer resolves that `res_id` to a host pointer (`vkr_cs.c:75-94`) and runs its
//! dispatch loop over *that* memory. (c)1 v1 does not relay those shmems, so S would resolve the id
//! to a blob it holds and execute **whatever that blob happens to contain** — zeros — as Vulkan
//! commands. That is not a crash. It is arbitrary GPU misbehaviour, arbitrarily far from its cause,
//! and it is exactly the failure the design spec's §5.1 demands never be silent.
//!
//! v1 legitimately need not *implement* the path: C0 Task 4b proved the reference application never
//! triggers it (its largest single input is a 1008-byte SPIR-V module, nowhere near 8192), and
//! measured opcode 180 occurring **zero** times across every sample of all six blobs. But "our one
//! app never does this" is not "we may mishandle it quietly".
//!
//! # Why this is a dword scan and **not** a decode — the design decision this module encodes
//! The obvious implementation is to walk the command stream with [`super::decode::decode_commands`]
//! and look for type 180. **That does not work, and believing it does is worse than having no check
//! at all.**
//!
//! Venus commands are **not self-delimiting**: nothing in a command says how long it is, so finding
//! command N+1 means already knowing command N's exact encoded size (see [`super::decode`]'s module
//! docs). [`super::decode::encoded_size`] knows three command types out of Venus's ~1000, and stops
//! honestly at the first one it cannot size. And Mesa's own source pins where that stop lands: the
//! first command in the instance ring is `vkEnumerateInstanceVersion`
//! (`vn_instance_init_renderer_versions`, `vn_instance.c:92-93`), and the **second** is
//! `vkCreateInstance` (`vn_instance.c:362-363`) — which is variable-length and unsizeable. So a
//! decode-based scan inspects roughly the first two commands a session ever produces and is blind to
//! every byte after them, including every 180 that could ever appear.
//!
//! Such a scan would report "no out-of-line streams" for **every** workload, forever. It would pass
//! its own tests. It would be silence wearing a check's clothes, which is precisely what §5.1
//! forbids.
//!
//! So this module does something structurally different, and the difference is the whole point:
//!
//! - **It has no frame.** It never tracks where a command starts or how long one is, so there is no
//!   stride to get wrong and nothing to desynchronize. A decoder that loses the frame produces
//!   confident nonsense forever after; this cannot lose a frame because it never has one.
//! - **It is therefore sound for refusal.** A `vkExecuteCommandStreamsMESA` command's first dword
//!   *is literally the value 180*, at a 4-byte-aligned offset (see the alignment argument below).
//!   So "no dword equals 180" is a **trustworthy** answer: this scan cannot miss an out-of-line
//!   stream.
//! - **It is deliberately imprecise in the safe direction.** It over-approximates: any dword that
//!   happens to equal 180 — a count, a size, a byte pattern inside an inlined SPIR-V module — is
//!   reported. That is a false positive, and a false positive here is a **loud, typed, actionable
//!   refusal**, not corruption.
//!
//! This is the asymmetry that makes the check legitimate under the spec's §7 rule that v1 relays
//! the ring as opaque bytes rather than decoding it: *"decoding the ring to make a correctness
//! decision means a decoding bug becomes a corruption bug"*. Scanning to **refuse** is not that
//! trade. A bug here fails closed — the session stops with a message naming a byte offset. A bug in
//! a decoder that *acts* fails open, and writes wrong bytes to a real GPU. The bytes themselves are
//! still relayed verbatim and still parsed by nobody on this side; this is a predicate over them,
//! not a reading of them.
//!
//! The technique is not novel here — it is exactly what C0 Task 4b used to establish the finding in
//! the first place (ring-findings §5.3), scanning every dword of every blob:
//!
//! ```text
//! res1 (  131268 B, 8 samples): dwords ever == 180 -> 0     (the command ring)
//! ```
//!
//! # Why the false-positive risk is bounded, but not nil
//! A false positive breaks a working application, so the precision genuinely matters, and the risk
//! is real rather than theoretical: SPIR-V is dword-granular and is inlined directly into the ring
//! (Mesa hands the shader module's words straight through), so **a shader with 180 or more result
//! ids contains the dword `180` routinely** — result ids are allocated densely from 1 upward, so any
//! shader past that size is all but guaranteed to carry it somewhere in its module, as an id operand
//! rather than a command header. `direct_size` is 8192 bytes = 2048 SPIR-V words for the 128 KiB
//! instance ring, so such a shader fits comfortably under the threshold and would use the *inline*
//! ring path this scan inspects — meaning this is a session v1 could otherwise have served, and the
//! scan would refuse it anyway. That is a real cost, not a hypothetical one.
//!
//! What keeps the cost *bounded* rather than open-ended is what v1 already cannot serve regardless:
//!
//! - **On the reference application it cannot fire.** C0 Task 4b's result is conclusive rather than
//!   a sampling artefact: `tail` is monotonic and never wrapped, so the final sample's ring buffer
//!   contained *every byte the client ever wrote*, and 180 was not among them.
//! - **On an application whose single submission genuinely exceeds `direct_size`, the scan's
//!   imprecision is moot.** Any workload with large descriptor writes or a long command buffer
//!   crosses 8192 routinely and genuinely uses the out-of-line path; such a session is unserviceable
//!   by v1 whether or not the dword that tripped the scan was a real command header.
//!
//! The SPIR-V case above is the gap between those two: an application that fits under `direct_size`
//! byte-for-byte, and that v1's transport could otherwise carry, but whose shader happens to contain
//! the bit pattern 180 and gets refused for it. No better check exists to close that gap — a
//! decode-based scan is blind past ring command #2 (see above) — so the false positive is accepted
//! as the price of a check that cannot miss a real out-of-line stream. **The honest statement, which
//! callers must not overstate: a refusal from this module means "a dword equal to 180 is present",
//! not "an out-of-line stream is definitely present".** [`OutOfLineStream`]'s message says so,
//! because a human debugging a refusal needs to know which claim is being made.
//!
//! # The alignment argument, which the soundness rests on
//! Scanning only 4-byte-aligned dwords is what makes this cheap, and it is sound because the Venus
//! command stream is dword-granular end to end:
//!
//! - virglrenderer measures command buffers in **`ndw`** — a count of dwords — and rejects anything
//!   that is not a whole number of them (this crate's [`crate::EngineError::UnalignedCommand`]
//!   exists for exactly that reason).
//! - Every `vn_sizeof_*` sums fields that are `u32`s and `u64`s, so every command's encoded size is
//!   a multiple of 4 (see [`super::decode::encoded_size`]'s table: 36, 16, 24).
//! - Mesa's producer starts the ring's `tail` counter at 0 and advances it by those sizes
//!   (`ring->cur += size`, `vn_ring_write_buffer`, `vn_ring.c:127-142`), so every command begins at
//!   a stream offset that is a multiple of 4.
//! - A ring delta is the half-open range `[previous_tail, tail)`, and both endpoints are such
//!   offsets — so **byte 0 of a delta is dword-aligned with respect to the stream**, and a scan from
//!   index 0 lands on exactly the offsets a command header could occupy.
//! - Mesa publishes `tail` only *after* writing a submission's bytes (`vn_ring_submit_internal`,
//!   `vn_ring.c:440`), so a delta cut at a published `tail` contains whole commands. A 180 header
//!   cannot straddle a delta boundary and be missed by both halves.

// The command type this module looks for. Imported rather than restated so there is exactly one
// copy of the number, alongside the rest of the repository's ring knowledge.
use super::decode::VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA;

/// A dword equal to [`VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA`] was found in a command
/// stream (c)1 v1 was about to relay.
///
/// # What this does and does not claim
/// It claims that a dword at `offset` holds the value 180. It does **not** claim that a
/// `vkExecuteCommandStreamsMESA` command definitely begins there — see the module docs: this scan is
/// deliberately a sound over-approximation, so it cannot miss a real out-of-line stream but can
/// report an argument dword that merely happens to equal 180. The distinction is in the error's
/// message because a human acting on this refusal needs it.
///
/// This is a typed value rather than a `String` for the reason every other refusal in this
/// repository is: the tests must be able to assert *which* refusal happened rather than grep prose,
/// and `offset` is the one piece of information that turns "(c)1 refused" into something a person
/// can go and look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "the command stream carries a dword equal to {VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA} \
     at byte offset {offset}, which is vkExecuteCommandStreamsMESA — Venus's out-of-line command \
     path, which (c)1 v1 does not relay. Mesa produces this when a single submission exceeds \
     direct_size (buffer_size >> 4 = 8192 bytes for the 128 KiB instance ring), and the real \
     commands then live in *other* shmems that this version never ships; S would execute whatever \
     its copy of those blobs happens to contain. Refusing here rather than relaying a stream that \
     would misbehave on S's GPU with no trace of the cause. NOTE: this scan over-approximates on \
     purpose — it cannot miss a real out-of-line stream, but an argument dword that merely happens \
     to equal {VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA} triggers it too. Either way this \
     workload is outside (c)1 v1's scope. The lever, when the time comes, is Mesa's client-side \
     direct_order constant (vn_instance.c:152): 0 makes direct_size == buffer_size."
)]
pub struct OutOfLineStream {
    /// Byte offset of the offending dword, relative to the start of the scanned slice.
    ///
    /// Relative to the *slice*, not to the ring's byte stream: the caller knows where the slice came
    /// from and this module does not. A caller reporting this to a human should say which delta it
    /// was, since "offset 40" alone is not locatable.
    pub offset: usize,
}

/// Scan a Venus command stream for any dword equal to
/// [`VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA`], refusing the stream if one is present.
///
/// **Read the module docs before changing this.** This is deliberately not a decode, and the reasons
/// are load-bearing: a decode-based check would inspect the first two commands of a session and be
/// blind to every 180 that could ever occur, while reporting success.
///
/// # Inputs / outputs
/// - `stream`: the command bytes about to be relayed — normally a ring delta's payload. Treated as
///   opaque; nothing here interprets structure.
/// - Returns `Ok(())` if no dword equals 180, which is a **trustworthy** negative: a real
///   `vkExecuteCommandStreamsMESA` header is itself such a dword and cannot hide from this. Returns
///   [`OutOfLineStream`] naming the **first** offending offset otherwise — the first, because it is
///   the one nearest the cause and because a caller that is about to refuse the whole stream has no
///   use for the rest.
///
/// # Failure modes
/// Cannot panic. A slice whose length is not a multiple of 4 has its trailing 1–3 bytes ignored,
/// which is sound: a dword cannot fit there, so no command header can begin there. Such a remainder
/// cannot occur in a real ring delta anyway (every Venus command's encoded size is a multiple of 4 —
/// see the module docs' alignment argument), so this is tolerance for a malformed input rather than
/// a case that carries meaning.
///
/// # Pitfalls
/// - **A refusal is not proof.** See [`OutOfLineStream`]: this over-approximates by design.
/// - **The offsets are the slice's, not the ring's.** Only the caller knows the delta's base.
/// - **This says nothing about the inline vtest path.** Commands arriving in `VCMD_SUBMIT_CMD2` are
///   ring *management* — `vkCreateRingMESA` and `vkNotifyRingMESA`, 140–236 bytes across an entire
///   Vulkan initialization (ring-findings §2) — and Mesa constructs the out-of-line wrapper only
///   inside `vn_ring_submission_get_cs` (`vn_ring.c:484-529`), i.e. only on the ring path. Scanning
///   the inline path would cost a little and prove nothing.
pub fn scan_for_out_of_line_stream(stream: &[u8]) -> Result<(), OutOfLineStream> {
    // 4-byte-aligned dwords only. The module docs' alignment argument is what makes this complete
    // rather than a sampling of the stream: every command header sits at a multiple of 4 from the
    // delta's start, so every position a 180 could occupy is visited exactly once.
    for (index, dword) in stream.chunks_exact(4).enumerate() {
        // Little-endian to match the ring's memory image, exactly as `decode::read_u32_le` pins it:
        // decoding natively would make this silently wrong on a big-endian C, and CLAUDE.md names
        // other architectures as explicit targets for the machine the application runs on.
        // `try_into` cannot fail — `chunks_exact(4)` yields exactly 4 bytes.
        let value = u32::from_le_bytes(dword.try_into().expect("chunks_exact(4) yields 4 bytes"));
        if value == VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA {
            return Err(OutOfLineStream {
                // `index` counts dwords; the caller wants a byte offset it can go and look at.
                offset: index * 4,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // The command types the reference application's ring actually carries, so the "clean stream"
    // tests are built from real values rather than invented ones.
    use super::super::decode::{
        VK_COMMAND_TYPE_VK_CREATE_INSTANCE, VK_COMMAND_TYPE_VK_ENUMERATE_INSTANCE_VERSION,
        VK_COMMAND_TYPE_VK_SET_REPLY_COMMAND_STREAM_MESA,
    };

    /// Build a little-endian dword stream from `u32`s, the way Mesa's encoder lays the ring out.
    fn stream_of(dwords: &[u32]) -> Vec<u8> {
        dwords.iter().flat_map(|d| d.to_le_bytes()).collect()
    }

    /// The reference application's real ring prologue must pass. This is the workload (c)1 v1
    /// exists to serve, and a check that refused it would break the only thing that works.
    ///
    /// The command types are the real ones and in the real order Mesa's source dictates:
    /// `vkSetReplyCommandStreamMESA` (178), then `vkEnumerateInstanceVersion` (137) — the first
    /// command the instance ring ever carries (`vn_instance.c:92-93`) — then `vkCreateInstance` (0)
    /// (`vn_instance.c:362-363`).
    #[test]
    fn the_reference_applications_ring_prologue_is_not_refused() {
        let stream = stream_of(&[
            VK_COMMAND_TYPE_VK_SET_REPLY_COMMAND_STREAM_MESA,
            0,
            VK_COMMAND_TYPE_VK_ENUMERATE_INSTANCE_VERSION,
            0,
            VK_COMMAND_TYPE_VK_CREATE_INSTANCE,
            0,
        ]);

        assert_eq!(
            scan_for_out_of_line_stream(&stream),
            Ok(()),
            "C0 Task 4b measured zero dwords equal to 180 across every sample of the refapp's \
             whole ring; refusing this workload would break the only application (c)1 v1 serves"
        );
    }

    /// An empty delta is clean. The watcher never produces one, but a scan that panicked or refused
    /// on it would turn a non-event into a session-ending error.
    #[test]
    fn an_empty_stream_is_clean() {
        assert_eq!(scan_for_out_of_line_stream(&[]), Ok(()));
    }

    /// **The load-bearing test: a `vkExecuteCommandStreamsMESA` header must be refused.**
    ///
    /// This is the whole point of the module. Mesa emits exactly this — a small type-180 wrapper
    /// carrying `VkCommandStreamDescriptionMESA`s that point at other shmems — in place of the real
    /// commands whenever one submission exceeds `direct_size` (8192 bytes). (c)1 v1 does not relay
    /// those shmems, so relaying this wrapper would have S execute the contents of a blob C never
    /// shipped.
    ///
    /// The payload mirrors the real encoding (`vn_ring.c:494-528`): `[type][flags][streamCount]`
    /// then a descriptor's `resourceId`, which is `4` here — the reference session's 8 MiB staging
    /// pool, a plausible target.
    #[test]
    fn an_out_of_line_command_stream_is_refused_naming_its_offset() {
        let stream = stream_of(&[
            VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA,
            0, // cmd_flags: always 0 for the wrapper — it takes no reply.
            1, // streamCount.
            4, // the first descriptor's resourceId.
        ]);

        assert_eq!(
            scan_for_out_of_line_stream(&stream),
            Err(OutOfLineStream { offset: 0 }),
            "a type-180 command header must be refused, naming where it was found"
        );
    }

    /// A 180 that appears **after** commands the decoder cannot size must still be found.
    ///
    /// **This is the test that pins why this module is not a decoder**, and it is the one that would
    /// have caught the mistake the brief for this task specified. `vkCreateInstance` (type 0) is
    /// variable-length, so `decode::encoded_size` returns `None` for it and `decode::decode_commands`
    /// stops there — and Mesa's source puts it at ring command **two** (`vn_instance.c:362-363`). A
    /// decode-based scan would therefore stop before this 180 and report the stream clean, for every
    /// workload, forever. The dword scan has no frame to lose and finds it.
    #[test]
    fn a_180_after_an_unsizeable_command_is_still_found() {
        let stream = stream_of(&[
            // The command that stops the decoder dead, at the position Mesa really puts it.
            VK_COMMAND_TYPE_VK_CREATE_INSTANCE,
            0,
            // Stand-in for vkCreateInstance's variable-length payload — application name strings and
            // an extension list — which is exactly what makes it unsizeable.
            0xdead_beef,
            0xfeed_face,
            // And the out-of-line wrapper, downstream of the decoder's stop point.
            VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA,
            0,
        ]);

        assert_eq!(
            scan_for_out_of_line_stream(&stream),
            Err(OutOfLineStream { offset: 16 }),
            "a decoder stops at vkCreateInstance (ring command #2) and would never reach this 180; \
             the scan must, or the check is silence wearing a check's clothes"
        );
    }

    /// The **first** offending dword is the one reported: it is nearest the cause, and a caller
    /// about to refuse the whole stream has no use for the rest.
    #[test]
    fn the_first_offending_dword_is_the_one_reported() {
        let stream = stream_of(&[
            VK_COMMAND_TYPE_VK_ENUMERATE_INSTANCE_VERSION,
            0,
            VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA,
            VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA,
        ]);

        assert_eq!(
            scan_for_out_of_line_stream(&stream),
            Err(OutOfLineStream { offset: 8 })
        );
    }

    /// The scan is little-endian, pinned rather than native.
    ///
    /// Decoding these bytes natively would make the check silently wrong on a big-endian machine —
    /// and CLAUDE.md names RISC-V as an explicit target for **C**, which is the machine that runs
    /// this scan. `0x0000_00b4` little-endian is `b4 00 00 00`; the byte-reversed form must not be
    /// mistaken for a command header.
    #[test]
    fn the_scan_reads_dwords_little_endian() {
        // The real thing: 180 as Mesa's x86-64 encoder writes it.
        assert_eq!(
            scan_for_out_of_line_stream(&[0xb4, 0x00, 0x00, 0x00]),
            Err(OutOfLineStream { offset: 0 })
        );
        // The same bytes big-endian, i.e. 0xb400_0000 — a different value, and not a 180.
        assert_eq!(
            scan_for_out_of_line_stream(&[0x00, 0x00, 0x00, 0xb4]),
            Ok(())
        );
    }

    /// A trailing 1–3 bytes are ignored rather than panicked over. A dword cannot fit there, so no
    /// command header can begin there — and a real delta never has such a remainder anyway, because
    /// every Venus command's encoded size is a multiple of 4.
    #[test]
    fn a_trailing_partial_dword_is_ignored_rather_than_panicked_over() {
        let mut stream = stream_of(&[VK_COMMAND_TYPE_VK_ENUMERATE_INSTANCE_VERSION, 0]);
        stream.extend_from_slice(&[0xb4, 0x00, 0x00]);

        assert_eq!(
            scan_for_out_of_line_stream(&stream),
            Ok(()),
            "three bytes cannot hold a command header, and refusing a malformed tail would turn a \
             non-event into a session-ending error"
        );
    }

    /// **The honest limit, made a test so nobody mistakes the scan for a decoder.**
    ///
    /// The scan over-approximates: an argument dword that merely happens to equal 180 is refused
    /// too. This is not a bug to be fixed by making the scan smarter — a smarter scan is a decoder,
    /// and a decoder cannot see past ring command #2. It is recorded here so that a future reader
    /// who hits a puzzling refusal knows this behaviour is intended and knows what the refusal does
    /// and does not claim.
    #[test]
    fn an_argument_dword_that_happens_to_equal_180_is_also_refused() {
        // `vkEnumerateInstanceVersion` with a pointer-presence marker whose low dword is 180. No
        // out-of-line stream is present; the scan refuses anyway, by design.
        let stream = stream_of(&[
            VK_COMMAND_TYPE_VK_ENUMERATE_INSTANCE_VERSION,
            0,
            VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA,
            0,
        ]);

        assert_eq!(
            scan_for_out_of_line_stream(&stream),
            Err(OutOfLineStream { offset: 8 }),
            "the scan is a sound over-approximation: it cannot miss a real out-of-line stream, and \
             the price is that it cannot tell one from a coincidence. A false positive is a loud, \
             typed refusal; a false negative would be silent GPU corruption. The trade is deliberate"
        );
    }
}
