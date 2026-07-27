//! **Framing for Venus command streams** — how long is the command at the start of these bytes?
//!
//! # Why this crate exists
//! Rayland relays Venus command streams byte-exactly and, until this crate, could not read them:
//! `rayland_vtest::venus_ring::decode::encoded_size` can express only *fixed* encodings, so a walk
//! halts at the application's first real command. Most Vulkan commands are variable-length — arrays,
//! `pNext` chains, optional pointers — so no table of sizes can ever walk a real stream.
//!
//! # Why it borrows rather than reimplements
//! Mesa already generates a complete, exact decoder for its own protocol. Reimplementing it would
//! create a second source of truth for a format Rayland does not own, and when the two diverge the
//! symptom is a decoder confidently reporting the wrong thing. This mirrors `CLAUDE.md`'s locked
//! decision to reuse the Venus/virglrenderer engine rather than write one.
//!
//! # THE BINDING CONSTRAINT
//! **This crate may never decide what to relay, when to relay it, or which blobs a delta reads.**
//! (c)1 spec §7 relays the ring as opaque bytes precisely so that a decoding bug cannot become a
//! corruption bug, and that reasoning is untouched here. `rayland-c`'s
//! `tests/decoder_is_not_load_bearing.rs` pins it mechanically for the relay path.
//!
//! **The constraint was once broader — "diagnostic and structural only, never a correctness
//! decision" — and one deliberate exception was taken on 2026-07-26**, off the relay path:
//! `rayland_vtest::venus_ring::decode::find_destroy_device` now decodes before believing a
//! signature match, and `rayland-s` uses its answer to decide when to retire the readback gate.
//! The bare scan it replaced was measured false-positiving on ordinary payload bytes, retiring
//! that gate on a device destruction that never happened; the rule existed to stop a decode bug
//! becoming a corruption bug, and there it was protecting a heuristic proven to corrupt. Recorded
//! as a trade, not a loophole — see `docs/DIARY.md`, 2026-07-26.
//!
//! See `docs/superpowers/specs/2026-07-26-venus-stream-decoder-design.md`.

unsafe extern "C" {
    /// The shim's one entry point. See `csrc/shim.c` for the contract; the wrapper below is the only
    /// caller and is what makes that contract safe to rely on.
    fn rayland_venus_command_len(
        bytes: *const u8,
        len: usize,
        out_cmd_type: *mut u32,
        out_len: *mut usize,
    ) -> core::ffi::c_int;
}

/// One decoded command's framing: what it is, and how long it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    /// The `VkCommandTypeEXT` read from the command's prologue.
    pub command_type: u32,
    /// The command's total length in bytes, prologue included — the stride to the next command.
    pub len: usize,
}

/// Why a command could not be framed.
///
/// Every variant is a refusal to guess. A walker that receives one of these must stop, because the
/// position of every following command is unknown — which is precisely the desynchronisation the old
/// size table's conservatism was protecting against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeFault {
    /// The stream ended inside the command. **This is the same condition virglrenderer reports as a
    /// "CS error"** — its decoder and this one share the mechanism, so the two agree by construction.
    Truncated,
    /// This Mesa generates no decoder for that command type, so its length is unknowable here.
    UnknownCommand {
        /// The type read from the prologue, so a caller can name what stopped it.
        command_type: u32,
    },
    /// The command's bytes were all present; the shim's 1 MiB scratch pool (`csrc/shim.c`'s `temp`)
    /// was too small to hold what Mesa's generated decoder needed to allocate while decoding it.
    ///
    /// # Do not confuse this with [`Truncated`](DecodeFault::Truncated)
    /// The two look similar from the caller's side — both stop the walk — but they mean opposite
    /// things about the *stream*. `Truncated` means the bytes genuinely ran out: the command is
    /// incomplete, and a caller walking a live ring should treat that as a real red flag (see
    /// `venus_ring::decode::DecodeStop::Truncated`'s own docs). `TempPoolExhausted` means the bytes
    /// were fine — the command could have been decoded in full — but this crate's own scratch arena
    /// was the limiting factor, not the wire. virglrenderer itself caps the equivalent pool at 1 GiB
    /// (`VKR_CS_DECODER_TEMP_POOL_MAX_SIZE`), a thousand times this shim's 1 MiB, so a command the
    /// real renderer decodes without complaint could legitimately hit this here. Reporting it as
    /// `Truncated` would tell a caller "the stream is broken" about a stream that is not.
    TempPoolExhausted,
    /// The shim rejected its arguments. Unreachable from this wrapper, which always passes a valid
    /// slice and valid out-params; present because the C contract admits it.
    BadArgs,
}

/// How many bytes does the command at the start of `stream` occupy, and which command is it?
///
/// # Inputs / outputs
/// - `stream`: bytes positioned at a command's `[type][flags]` prologue. Not consumed; nothing is
///   retained. An empty slice is [`DecodeFault::Truncated`].
/// - Returns the command's type and length, or a typed fault.
///
/// # Failure modes
/// See [`DecodeFault`]. Cannot panic: every fault the borrowed decoders can raise becomes a variant.
///
/// # Pitfall: this reports framing, not meaning
/// The command's arguments are decoded and then discarded. Object handles are read but never
/// resolved — the shim's lookup returns null, which cannot change how many bytes were consumed. Do
/// not read anything into the fact that a command decoded successfully: it means the bytes were
/// well-formed, not that the command would execute.
///
/// # Pitfall: this must never decide what gets relayed
/// (c)1 spec §7 relays the ring as opaque bytes so that a decoding bug cannot become a corruption
/// bug. This function is for reporting, measuring and testing. The crate docs record the single
/// deliberate exception taken off the relay path (`find_destroy_device`, 2026-07-26) — read it
/// before adding a second one, because the argument that justified it was specific.
pub fn command_len(stream: &[u8]) -> Result<Command, DecodeFault> {
    let mut command_type: u32 = 0;
    let mut len: usize = 0;
    // SAFETY: `stream` is a valid slice of `stream.len()` readable bytes; the two out-params are
    // valid, writable locals. The shim reads only within the slice and writes only through the
    // out-params. Nothing it allocates outlives the call, and nothing it returns depends on a
    // prior call: the per-command scratch pool (`csrc/shim.c`'s `temp`) is `_Thread_local static`
    // storage whose *bytes* persist between calls, but its `temp_used` bump-allocator cursor is
    // reset to zero at the top of every call, so no call can ever observe a previous call's
    // allocations — the persistence is an implementation detail of the arena, not shared state.
    //
    // AUDITED, NOT ASSUMED — "writes only through the out-params" rests on one further fact that is
    // not visible from this file: `vkr_cs_decoder_get_blob_storage` (csrc/vkr_cs.h) casts away
    // `const` and hands the generated decoder a pointer straight into `stream`'s own bytes — a
    // *writable* alias into what Rust sees as an immutable `&[u8]`. That is safe only because, as of
    // virglrenderer 1.2.0, every one of the six generated call sites that receive this pointer
    // immediately follows the pattern `if (!p) return; vn_decode_blob_array(dec, p, size)`, and
    // `vn_decode_blob_array` special-cases `dst == src` into a no-op (see `vkr_cs_decoder_read`'s
    // aliasing guard) — so nothing ever actually writes through it today. This is a property of the
    // *vendored headers*, not of this crate's own code, and it has no test that would catch a future
    // Mesa version violating it: **re-audit this on every `vendor/venus-protocol` bump** (see
    // `vendor/MESA_VERSION`'s update checklist). If a future generated decoder ever writes through
    // this pointer, this SAFETY comment becomes false and this function silently mutates a Rust
    // caller's immutable borrow — and on the `RAYLAND_RING_DUMP` path those same bytes are relayed
    // moments later, which is exactly the diagnostic-becomes-corruption failure this crate exists to
    // prevent. See the matching note on `vkr_cs_decoder_get_blob_storage` in csrc/vkr_cs.h.
    let rc = unsafe {
        rayland_venus_command_len(stream.as_ptr(), stream.len(), &mut command_type, &mut len)
    };
    match rc {
        0 => Ok(Command { command_type, len }),
        // `RAYLAND_VENUS_FAULT_TRUNCATED` in `csrc/shim.c`: the stream ended inside the command.
        1 => Err(DecodeFault::Truncated),
        // `RAYLAND_VENUS_FAULT_UNKNOWN_COMMAND` in `csrc/shim.c`: no generated decoder for this type.
        2 => Err(DecodeFault::UnknownCommand { command_type }),
        // `RAYLAND_VENUS_FAULT_TEMP_POOL_EXHAUSTED` in `csrc/shim.c`: the command's bytes were all
        // present, but this crate's own 1 MiB scratch pool was too small — a fault about this
        // crate's arena, not about the stream. See `DecodeFault::TempPoolExhausted`'s doc comment
        // for why this must never collapse into `Truncated`.
        4 => Err(DecodeFault::TempPoolExhausted),
        // Any other code is a shim contract violation (`RAYLAND_VENUS_FAULT_BAD_ARGS`, or anything
        // undocumented). Reported as `BadArgs` rather than panicking: a diagnostic that aborts the
        // process is worse than one that says it does not know.
        _ => Err(DecodeFault::BadArgs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `vkGetFenceStatus` is `[type=38][flags][device u64][fence u64]` — 24 bytes — and it is the
    /// command vkcube polls while waiting for the submit that motivated this crate. Decoding it
    /// proves the borrowed decoders run and that framing comes out of them.
    #[test]
    fn a_fixed_size_command_reports_its_real_length() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&38u32.to_le_bytes()); // command type
        stream.extend_from_slice(&0u32.to_le_bytes()); // flags
        stream.extend_from_slice(&0x1111_1111_1111_1111u64.to_le_bytes()); // device handle
        stream.extend_from_slice(&0x2222_2222_2222_2222u64.to_le_bytes()); // fence handle
        let cmd = command_len(&stream).expect("a decodable command");
        assert_eq!(cmd.command_type, 38);
        assert_eq!(cmd.len, 24);
    }

    /// A stream cut inside a command must be a typed fault, never a panic and never a plausible
    /// wrong length — a wrong length is how a walker desynchronizes and invents commands.
    #[test]
    fn a_truncated_command_is_a_fault_not_a_guess() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&38u32.to_le_bytes());
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.extend_from_slice(&0u64.to_le_bytes()); // only one of the two handles
        assert_eq!(command_len(&stream), Err(DecodeFault::Truncated));
    }

    /// Too short even for the prologue. The prologue is where every walk begins, so this is the
    /// boundary a caller hits at the end of every stream.
    #[test]
    fn a_slice_too_short_for_the_prologue_is_truncated() {
        assert_eq!(command_len(&[0u8; 3]), Err(DecodeFault::Truncated));
    }

    /// A command type Mesa generates no decoder for is reported *with its type*, so a caller can say
    /// which command stopped it — the single most useful fact this crate produces when it fails.
    #[test]
    fn an_unknown_command_type_reports_which_one() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&9_999u32.to_le_bytes()); // past the end of the enum
        stream.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            command_len(&stream),
            Err(DecodeFault::UnknownCommand { command_type: 9_999 })
        );
    }

    /// `vkCreatePipelineCache` with a 5-byte `pInitialData` blob — not a multiple of 4 — carried over
    /// from Task 2's review: its reviewer asked that this stream be committed as the load-bearing
    /// evidence for `vkr_cs_decoder_get_blob_storage`'s NULL-return fix (see `csrc/vkr_cs.h`), whose
    /// hazard was exactly a silent under-report when a blob array's storage request is refused.
    ///
    /// Layout, derived field-by-field from `vn_decode_vkCreatePipelineCache_args_temp` and
    /// `vn_decode_VkPipelineCacheCreateInfo_{temp,self_temp}` in
    /// `vendor/venus-protocol/vn_protocol_renderer_pipeline_cache.h`, plus the 8-byte prologue every
    /// command shares (`vn_decode_VkCommandTypeEXT` + `vn_decode_VkFlags`, both `vn_decode_uint32_t`
    /// underneath — see `vn_protocol_renderer_types.h`):
    ///   - `[type=61][flags]`                    — 8 bytes: the prologue (61 = `vkCreatePipelineCache`,
    ///     `vn_protocol_renderer_defines.h`).
    ///   - `[device u64]`                        — 8 bytes: `vn_decode_VkDevice_lookup` reads one u64
    ///     id regardless of whether the lookup resolves (it never does in this crate).
    ///   - `[pCreateInfo-present u64]`            — 8 bytes: `vn_decode_simple_pointer` is
    ///     `vn_decode_array_size_unchecked`, i.e. one u64; must be non-zero or the decoder takes the
    ///     `else` branch and sets fatal instead of decoding a struct.
    ///   - `[sType i32=17]`                       — 4 bytes: `vn_decode_VkStructureType` is
    ///     `vn_decode_int32_t`; 17 is `VK_STRUCTURE_TYPE_PIPELINE_CACHE_CREATE_INFO` (`vulkan_core.h`).
    ///     A mismatch does not change the byte count (the decode keeps going regardless) but does set
    ///     fatal, which would turn this into an unwanted `Truncated` — so it must be the real value.
    ///   - `[pNext-present u64=0]`                — 8 bytes: another `vn_decode_simple_pointer`; must be
    ///     zero, since a non-zero value takes the "no known/supported struct" branch and sets fatal.
    ///   - `[flags u32]`                          — 4 bytes: `VkPipelineCacheCreateFlags`, decoded as
    ///     plain `VkFlags`.
    ///   - `[initialDataSize u64=5]`              — 8 bytes: `vn_decode_size_t`, the real (unpadded)
    ///     blob length.
    ///   - `[array_size u64=5]`                   — 8 bytes: `vn_decode_array_size` re-reads the count
    ///     and requires it to equal `initialDataSize`, or it sets fatal and zeroes the count.
    ///   - `[blob, padded to 4 bytes]`            — 8 bytes: `vn_decode_blob_array` advances by
    ///     `(size + 3) & ~3` = `(5 + 3) & ~3` = 8, even though only 5 bytes are real data — this is the
    ///     padding the test name calls out, and the reason the expected total is not simply
    ///     "prologue + fields + 5".
    ///   - `[pAllocator-present u64=0]`           — 8 bytes: `vn_decode_simple_pointer`; must be zero,
    ///     since Venus's host allocator is never the client's — a non-zero value sets fatal.
    ///   - `[pPipelineCache-present u64=1]`       — 8 bytes: `vn_decode_simple_pointer`; must be
    ///     non-zero, or the `else` branch sets fatal instead of decoding the output handle.
    ///   - `[pPipelineCache u64]`                 — 8 bytes: `vn_decode_VkPipelineCache`, one more id.
    ///
    /// Total: 8 + 8 + 8 + 4 + 8 + 4 + 8 + 8 + 8 + 8 + 8 + 8 = **88 bytes**.
    #[test]
    fn a_blob_array_command_accounts_for_its_padding() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&61u32.to_le_bytes()); // command type: vkCreatePipelineCache
        stream.extend_from_slice(&0u32.to_le_bytes()); // flags
        stream.extend_from_slice(&0x3333_3333_3333_3333u64.to_le_bytes()); // device handle
        stream.extend_from_slice(&1u64.to_le_bytes()); // pCreateInfo is present
        stream.extend_from_slice(&17i32.to_le_bytes()); // sType = VK_STRUCTURE_TYPE_PIPELINE_CACHE_CREATE_INFO
        stream.extend_from_slice(&0u64.to_le_bytes()); // pNext is absent
        stream.extend_from_slice(&0u32.to_le_bytes()); // VkPipelineCacheCreateFlags
        stream.extend_from_slice(&5u64.to_le_bytes()); // initialDataSize: 5 bytes, not a multiple of 4
        stream.extend_from_slice(&5u64.to_le_bytes()); // array_size, must match initialDataSize
        stream.extend_from_slice(&[0xAAu8, 0xBB, 0xCC, 0xDD, 0xEE, 0, 0, 0]); // 5 real bytes + 3 padding
        stream.extend_from_slice(&0u64.to_le_bytes()); // pAllocator is absent
        stream.extend_from_slice(&1u64.to_le_bytes()); // pPipelineCache (the output handle) is present
        stream.extend_from_slice(&0x4444_4444_4444_4444u64.to_le_bytes()); // the output handle itself
        let cmd = command_len(&stream).expect("a decodable command");
        assert_eq!(cmd.command_type, 61);
        assert_eq!(cmd.len, 88);
    }

    /// `vkGetPipelineCacheData` is one of the six commands whose generated `_args_temp` decoder also
    /// takes a `struct vn_cs_encoder *` (see the long comment on `vkr_cs_encoder_get_blob_storage` in
    /// `csrc/vkr_cs.h`), carried over from Task 2's review as the load-bearing evidence for that
    /// sentinel: this stream is built so the decoder actually calls
    /// `vn_cs_encoder_get_blob_storage`/`vkr_cs_encoder_get_blob_storage` rather than skipping it, which
    /// is the only way to prove the sentinel (a non-null, never-dereferenced pointer) is what lets this
    /// command decode instead of aborting.
    ///
    /// Layout, derived field-by-field from `vn_decode_vkGetPipelineCacheData_args_temp` in
    /// `vendor/venus-protocol/vn_protocol_renderer_pipeline_cache.h`:
    ///   - `[type=63][flags]`                — 8 bytes: the prologue (63 =
    ///     `vkGetPipelineCacheData`, `vn_protocol_renderer_defines.h`).
    ///   - `[device u64]`                    — 8 bytes: `vn_decode_VkDevice_lookup`.
    ///   - `[pipelineCache u64]`             — 8 bytes: `vn_decode_VkPipelineCache_lookup`.
    ///   - `[pDataSize-present u64=1]`       — 8 bytes: `vn_decode_simple_pointer`; must be non-zero,
    ///     or the `else` branch sets fatal instead of decoding `*pDataSize`.
    ///   - `[pDataSize u64=4]`               — 8 bytes: `vn_decode_size_t`, the cache-data byte count
    ///     the (simulated) client claims.
    ///   - `[pData array_size u64=4]`        — 8 bytes: `vn_peek_array_size` (a peek, not consumed by
    ///     itself) followed by `vn_decode_array_size(dec, *pDataSize)`, which re-reads the same 8 bytes
    ///     and requires them to equal `*pDataSize` — hence 4 again, not a fresh value. This is the
    ///     branch that reaches `vn_cs_encoder_get_blob_storage`: the sentinel it returns is always
    ///     non-null, so the `if (!args->pData) return;` guard never fires and the (empty, in this
    ///     decode direction) blob-array bookkeeping completes normally.
    ///
    /// Note the asymmetry with the previous test: `vn_decode_vkGetPipelineCacheData_args_temp` never
    /// calls `vn_decode_blob_array` — `pData` is an *out* parameter the host fills on the reply path,
    /// so no blob bytes are read here at all, only the size fields. Total:
    /// 8 + 8 + 8 + 8 + 8 + 8 = **48 bytes**.
    #[test]
    fn a_command_taking_the_three_argument_decoder_path_decodes() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&63u32.to_le_bytes()); // command type: vkGetPipelineCacheData
        stream.extend_from_slice(&0u32.to_le_bytes()); // flags
        stream.extend_from_slice(&0x5555_5555_5555_5555u64.to_le_bytes()); // device handle
        stream.extend_from_slice(&0x6666_6666_6666_6666u64.to_le_bytes()); // pipelineCache handle
        stream.extend_from_slice(&1u64.to_le_bytes()); // pDataSize is present
        stream.extend_from_slice(&4u64.to_le_bytes()); // *pDataSize == 4
        stream.extend_from_slice(&4u64.to_le_bytes()); // pData's array_size, must match *pDataSize
        let cmd = command_len(&stream).expect("a decodable command");
        assert_eq!(cmd.command_type, 63);
        assert_eq!(cmd.len, 48);
    }

    /// `vkResetFences` with a `fenceCount` chosen to overrun the shim's 1 MiB temp pool, without
    /// needing anywhere near 1 MiB of actual stream bytes.
    ///
    /// # Why this stream is cheap to construct
    /// `vn_decode_vkResetFences_args_temp` (`vendor/venus-protocol/vn_protocol_renderer_fence.h`)
    /// allocates `sizeof(VkFence) * fenceCount` bytes from the temp pool *before* it reads a single
    /// handle out of the stream — so exhaustion is visible the moment the array-size field is
    /// decoded, and the function returns immediately on the `if (!args->pFences) return;` guard
    /// without ever touching `fenceCount * 8` bytes of handle data that this test never provides.
    /// `sizeof(VkFence)` is 8 on every 64-bit build (`VK_DEFINE_NON_DISPATCHABLE_HANDLE`), so
    /// `200_000 * 8 = 1_600_000` bytes requested against a `1 << 20 = 1_048_576`-byte pool overruns
    /// it by a wide, comfortable margin — no edge-case rounding to get right.
    ///
    /// Layout: `[type=37][flags]` (prologue, 8) + `[device u64]` (8) + `[fenceCount u32=200_000]` (4)
    /// + `[array_size u64=200_000]` (8, must equal `fenceCount`) = 28 bytes total. The command type
    /// 37 is `VK_COMMAND_TYPE_vkResetFences_EXT`
    /// (`vendor/venus-protocol/vn_protocol_renderer_defines.h`).
    #[test]
    fn a_command_whose_array_overruns_the_temp_pool_is_a_distinct_fault() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&37u32.to_le_bytes()); // command type: vkResetFences
        stream.extend_from_slice(&0u32.to_le_bytes()); // flags
        stream.extend_from_slice(&0x7777_7777_7777_7777u64.to_le_bytes()); // device handle
        stream.extend_from_slice(&200_000u32.to_le_bytes()); // fenceCount
        stream.extend_from_slice(&200_000u64.to_le_bytes()); // array_size, must match fenceCount
        assert_eq!(command_len(&stream), Err(DecodeFault::TempPoolExhausted));
    }
}
