//! The **vtest wire-protocol server**: parse what Mesa's Venus ICD emits over a byte stream and
//! drive a [`RenderEngine`] (Task 1) with it.
//!
//! # What vtest is, and why Rayland speaks it
//! Mesa's **Venus** Vulkan ICD serializes an application's Vulkan calls into a command stream. In
//! a virtual machine it hands that stream to the host over virtio-gpu; but Venus also has a
//! *no-VM* backend, `vn_renderer_vtest`, that talks a small socket protocol called **vtest**
//! (normally to virglrenderer's `virgl_test_server`). Rayland's C side runs exactly this vtest
//! backend, so the S side must *be* the vtest server: read the vtest messages, and replay their
//! Venus command buffers on the real GPU through [`RenderEngine`]. This module is that server's
//! parser and dispatcher.
//!
//! # The wire framing (pinned from Mesa 26.0.3 + virglrenderer, and verified by byte capture)
//! Every message is a fixed **2-dword header** followed by a payload:
//!
//! ```text
//!   dword 0: length   (VTEST_CMD_LEN)  -- size of the payload that follows
//!   dword 1: cmd_id   (VTEST_CMD_ID)   -- which VCMD_* command this is
//!   ... payload ...
//! ```
//!
//! Both header dwords, and every payload dword, are little-endian `u32`s. (vtest is natively
//! *host*-endian because it was designed for same-host VMs; all of Rayland's realistic C/S targets
//! — x86_64, aarch64, riscv64 — are little-endian, and a live capture of Mesa's ICD on x86_64
//! confirmed LE, so we fix LE and treat cross-endian as an explicit future concern.)
//!
//! **Length-unit pitfall.** The `length` field is a *dword* count for every command **except**
//! `VCMD_CREATE_RENDERER`, whose length is a *byte* count (the length of the renderer-name string,
//! `strlen(name)+1`, not padded to a dword). The capture showed `len=11` for the 11-byte name
//! `"vulkaninfo\0"`. We special-case create-renderer accordingly.
//!
//! # The handshake (exact order Mesa's `vtest_init` performs)
//! 1. `VCMD_CREATE_RENDERER` — name string; **no response**.
//! 2. `VCMD_PING_PROTOCOL_VERSION` (len 0) immediately followed by a dummy
//!    `VCMD_RESOURCE_BUSY_WAIT` (handle 0). If the server answers the ping, ping is supported.
//! 3. `VCMD_PROTOCOL_VERSION` — client sends its version (4); server replies with the negotiated
//!    `min(client, server)`. Mesa requires the negotiated version be **≥ 3** or it aborts init
//!    ("vtest protocol version too old" — the failure the feasibility spike saw).
//! 4. `VCMD_GET_PARAM(MAX_TIMELINE_COUNT)` — must come back valid and non-zero or Mesa aborts
//!    ("no timeline support").
//! 5. `VCMD_GET_CAPSET(venus)` — must come back valid with the real Venus capset or Mesa aborts
//!    ("no venus capset"). Answered from the engine via [`RenderEngine::venus_capset`].
//! 6. `VCMD_CONTEXT_INIT(venus)` — **no response**; this is where we create the GPU context.
//!
//! After the handshake the app runs: `VCMD_RESOURCE_CREATE_BLOB` (memory), `VCMD_SYNC_*` (timeline
//! semaphores), and `VCMD_SUBMIT_CMD2` (the Venus command buffers) — the last routed to
//! [`RenderEngine::submit`].
//!
//! # Deliberate scope of this task (C0 Task 2)
//! This is the parser + dispatcher skeleton. The handshake, `VCMD_CONTEXT_INIT` →
//! `create_venus_context`, and `VCMD_SUBMIT_CMD2` → `submit` are wired for real. Resource and sync
//! commands are handled with in-band bookkeeping stubs (they parse correctly and get an in-band
//! reply), because two things they need are out of scope here and belong to later tasks:
//! - **fd passing.** `VCMD_RESOURCE_CREATE_BLOB` and `VCMD_SYNC_WAIT` reply with a *file
//!   descriptor* over a `SCM_RIGHTS` control message. A generic `Read + Write` stream (the point
//!   of the generic bound — SP2's QUIC transport has no fds) cannot carry one, so we write only
//!   the in-band part of those replies and defer the fd side channel to Task 4's Unix-socket wiring
//!   (and, ultimately, to Rayland's own sibling-protocol memory sharing, which replaces fd passing
//!   for cross-machine operation).
//! - **pixel readback.** Getting the rendered image back out of the resource is Task 3.
//!
//! Unimplemented opcodes are reported as a typed error — never silently dropped.

// Streaming I/O over whatever byte transport carries the vtest protocol (Unix socket now, QUIC
// stream later — the whole reason `serve_vtest` is generic).
use std::io::{Read, Write};

// The trait we drive and the crate error type every failure maps into.
use crate::{EngineError, RenderEngine};

// ----------------------------------------------------------------------------------------------
// Pinned protocol constants (transcribed from virglrenderer's `vtest/vtest_protocol.h`).
// ----------------------------------------------------------------------------------------------

/// The vtest message header is two dwords: `[length][cmd_id]` (`VTEST_HDR_SIZE == 2`).
const VTEST_HEADER_DWORDS: usize = 2;

/// The highest vtest protocol version this server implements (`VTEST_PROTOCOL_VERSION`). We
/// advertise 4 and negotiate `min(client, 4)`; Mesa's Venus backend requires the result be ≥ 3.
const VTEST_PROTOCOL_VERSION: u32 = 4;

/// Timeline count reported for `VCMD_PARAM_MAX_TIMELINE_COUNT`. Must be non-zero or Mesa's Venus
/// backend aborts init with "no timeline support". virglrenderer's server uses 64; we match it.
const VTEST_MAX_TIMELINE_COUNT: u32 = 64;

/// The single GPU context id this server maps its one client connection onto. vtest is one context
/// per connection; `VCMD_CONTEXT_INIT` creates it and every `VCMD_SUBMIT_CMD2` targets it.
const VTEST_CONTEXT_ID: u32 = 1;

// vtest command ids (`VCMD_*`) — only those Mesa's Venus ICD actually emits, plus the handshake.
const VCMD_RESOURCE_UNREF: u32 = 3;
const VCMD_SUBMIT_CMD: u32 = 6; // legacy virgl submit (not used by Venus); rejected explicitly.
const VCMD_RESOURCE_BUSY_WAIT: u32 = 7;
const VCMD_CREATE_RENDERER: u32 = 8;
const VCMD_PING_PROTOCOL_VERSION: u32 = 10;
const VCMD_PROTOCOL_VERSION: u32 = 11;
const VCMD_GET_PARAM: u32 = 15;
const VCMD_GET_CAPSET: u32 = 16;
const VCMD_CONTEXT_INIT: u32 = 17;
const VCMD_RESOURCE_CREATE_BLOB: u32 = 18;
const VCMD_SYNC_CREATE: u32 = 19;
const VCMD_SYNC_UNREF: u32 = 20;
const VCMD_SYNC_READ: u32 = 21;
const VCMD_SYNC_WRITE: u32 = 22;
const VCMD_SYNC_WAIT: u32 = 23;
const VCMD_SUBMIT_CMD2: u32 = 24;

/// `VCMD_PARAM_MAX_TIMELINE_COUNT` — the one `VCMD_GET_PARAM` parameter Mesa's Venus backend asks
/// for (and requires non-zero).
const VCMD_PARAM_MAX_TIMELINE_COUNT: u32 = 1;

/// The Venus capset id on the wire (`VIRTGPU_DRM_CAPSET_VENUS`), which Mesa sends in
/// `VCMD_GET_CAPSET` / `VCMD_CONTEXT_INIT`. Same value as the engine's Venus capset (4).
const VENUS_CAPSET_ID: u32 = 4;

/// Upper bound on a single message's declared payload, checked *before* any buffer is allocated.
///
/// The length prefix is untrusted input off the wire. Without a bound, a corrupt or malicious peer
/// could claim a payload near `u32::MAX` dwords and force a multi-gigabyte allocation; in Rust an
/// allocation failure aborts the process, so an unbounded prefix is a crash/DoS vector, not merely
/// a slow path. 64 MiB is enormously generous for a Venus command batch while still bounding the
/// worst case. Mirrors `rayland-wire`'s `MAX_FRAME_BYTES`.
pub const MAX_VTEST_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;

// ----------------------------------------------------------------------------------------------
// Decoded command + outcome types.
// ----------------------------------------------------------------------------------------------

/// One decoded vtest message — the typed result of parsing a header + payload off the stream.
///
/// Only the commands Mesa's Venus ICD actually sends have variants here; any other opcode is
/// rejected by [`read_command`] rather than being represented. Keeping decoding separate from
/// dispatch is what lets the framing be unit-tested with no engine and no GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VtestCommand {
    /// `VCMD_CREATE_RENDERER`: opening message carrying the client's process name. No reply.
    CreateRenderer {
        /// The raw name bytes (includes the trailing NUL the client sends).
        name: Vec<u8>,
    },
    /// `VCMD_PING_PROTOCOL_VERSION`: probe for whether the server supports version negotiation.
    PingProtocolVersion,
    /// `VCMD_RESOURCE_BUSY_WAIT`: during the handshake it is a dummy (handle 0) used to detect ping
    /// support; later it polls a resource's GPU busy state.
    ResourceBusyWait {
        /// Resource handle to test (0 for the handshake dummy).
        handle: u32,
        /// Flags (`VCMD_BUSY_WAIT_FLAG_WAIT` to block until idle).
        flags: u32,
    },
    /// `VCMD_PROTOCOL_VERSION`: the client's proposed protocol version, to be negotiated down.
    ProtocolVersion {
        /// The version the client proposes (Mesa sends 4).
        version: u32,
    },
    /// `VCMD_GET_PARAM`: query a scalar renderer parameter (Venus asks for max timeline count).
    GetParam {
        /// Which parameter (`VCMD_PARAM_*`).
        param: u32,
    },
    /// `VCMD_GET_CAPSET`: request a capability-set blob (Venus requests capset 4).
    GetCapset {
        /// Capset id requested.
        id: u32,
        /// Capset version requested.
        version: u32,
    },
    /// `VCMD_CONTEXT_INIT`: bind this connection to a capset — for us, create the Venus context.
    ContextInit {
        /// The capset id to initialize the context with (4 = Venus).
        capset_id: u32,
    },
    /// `VCMD_RESOURCE_CREATE_BLOB`: allocate a host/guest-shared memory blob (device memory, the
    /// command ring, staging buffers). Its reply carries a res_id **and an fd** (fd deferred here).
    ResourceCreateBlob {
        /// Blob type (`VCMD_BLOB_TYPE_*`).
        blob_type: u32,
        /// Blob flags (`VCMD_BLOB_FLAG_*`: mappable / shareable / cross-device).
        flags: u32,
        /// Requested size in bytes (assembled from the lo/hi dword pair).
        size: u64,
        /// Client-chosen blob id (assembled from the lo/hi dword pair; 0 is a valid id for Venus).
        blob_id: u64,
    },
    /// `VCMD_RESOURCE_UNREF`: drop a resource created earlier. No reply.
    ResourceUnref {
        /// The resource handle to release.
        res_handle: u32,
    },
    /// `VCMD_SYNC_CREATE`: create a timeline sync object with an initial value. Reply: its id.
    SyncCreate {
        /// Initial timeline value (assembled from the lo/hi dword pair).
        initial_value: u64,
    },
    /// `VCMD_SYNC_UNREF`: drop a sync object. No reply.
    SyncUnref {
        /// The sync id to release.
        sync_id: u32,
    },
    /// `VCMD_SYNC_READ`: read a sync object's current timeline value. Reply: the value.
    SyncRead {
        /// The sync id to read.
        sync_id: u32,
    },
    /// `VCMD_SYNC_WRITE`: set a sync object's timeline value. No reply.
    SyncWrite {
        /// The sync id to write.
        sync_id: u32,
        /// The value to store (assembled from the lo/hi dword pair).
        value: u64,
    },
    /// `VCMD_SYNC_WAIT`: wait for one or more syncs to reach given values. Its reply is a *pollable
    /// fd* over `SCM_RIGHTS` (deferred here — see the module scope note).
    SyncWait {
        /// Wait flags (`VCMD_SYNC_WAIT_FLAG_ANY`).
        flags: u32,
        /// Poll timeout in milliseconds (`u32::MAX` for "infinite").
        timeout: u32,
        /// The `(sync_id, value)` pairs to wait on.
        waits: Vec<(u32, u64)>,
    },
    /// `VCMD_SUBMIT_CMD2`: submit one or more batches of Venus command buffers. The load-bearing
    /// command — each batch's command dwords are replayed on the GPU via [`RenderEngine::submit`].
    SubmitCmd2 {
        /// The batches, each with its extracted command dwords and sync signals.
        batches: Vec<SubmitBatch>,
    },
}

/// One batch inside a `VCMD_SUBMIT_CMD2` message, with its command stream and sync signals already
/// sliced out of the wire body (offsets validated during decode, so dispatch can trust them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitBatch {
    /// Batch flags (`VCMD_SUBMIT_CMD2_FLAG_*`, e.g. `RING_IDX`).
    pub flags: u32,
    /// The Venus command-buffer dwords for this batch — what gets replayed on the GPU.
    pub cmd: Vec<u32>,
    /// The `(sync_id, value)` timeline signals this batch raises when it completes.
    pub syncs: Vec<(u32, u64)>,
    /// The timeline ring index this batch targets (`ring_idx`; meaningful with `FLAG_RING_IDX`).
    pub ring_idx: u32,
}

/// What a completed `serve_vtest` session reports back to its caller.
///
/// Its reason for being is `rendered_resource_id`: the resource that Task 3 will read the rendered
/// pixels out of. In the vtest/Venus data path the rendered image lives in a blob resource, so we
/// report the most recently created blob as the best-effort readback candidate (Task 3 refines
/// *which* blob once it understands the Venus object graph). The counters are for diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VtestOutcome {
    /// The resource id Task 3 should read pixels from, if any resource was created.
    pub rendered_resource_id: Option<u32>,
    /// The GPU context id this session created (if `VCMD_CONTEXT_INIT` was reached).
    pub context_id: Option<u32>,
    /// How many `VCMD_SUBMIT_CMD2` batches were replayed (diagnostic).
    pub submitted_batches: u64,
}

// ----------------------------------------------------------------------------------------------
// Framing: reading one message off the stream (length-checked), and writing a reply.
// ----------------------------------------------------------------------------------------------

/// Read exactly `buf.len()` bytes, distinguishing a *clean* end-of-stream from a truncated one.
///
/// Returns `Ok(true)` if the buffer was filled, `Ok(false)` if the stream ended cleanly *before a
/// single byte was read* (the connection closed at a message boundary — a normal end of session),
/// and an [`EngineError::VtestIo`] if it ended partway through (a truncated message) or the read
/// otherwise failed.
fn read_full_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<bool, EngineError> {
    // How many bytes we still need to fill `buf`.
    let mut filled = 0;
    while filled < buf.len() {
        // Read into the unfilled tail.
        let n = r.read(&mut buf[filled..])?;
        if n == 0 {
            // Zero bytes means the peer closed. If we hadn't read anything, this is a clean end of
            // session; if we were mid-message, the message is truncated — an error.
            if filled == 0 {
                return Ok(false);
            }
            return Err(EngineError::VtestIo(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "vtest stream ended in the middle of a message",
            )));
        }
        filled += n;
    }
    Ok(true)
}

/// Read exactly `buf.len()` bytes, treating *any* short read (including a clean close) as an error.
/// Used for payload bytes, where the header already promised more data is coming.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), EngineError> {
    // `read_exact` maps a premature close to `UnexpectedEof`, which our `#[from]` turns into
    // `VtestIo` — exactly the "truncated payload" signal we want.
    r.read_exact(buf)?;
    Ok(())
}

/// Read a payload of `count` little-endian dwords, after bounding `count` against
/// [`MAX_VTEST_PAYLOAD_BYTES`] so an untrusted length prefix cannot drive a huge allocation.
fn read_payload_dwords<R: Read>(r: &mut R, count: u32) -> Result<Vec<u32>, EngineError> {
    // Bound the declared size in *bytes* before allocating anything.
    let byte_len = count as u64 * 4;
    if byte_len > MAX_VTEST_PAYLOAD_BYTES {
        return Err(EngineError::VtestFrameTooLarge { len: byte_len });
    }
    // Now the size is bounded, allocate and fill the dword buffer.
    let mut raw = vec![0u8; byte_len as usize];
    read_full(r, &mut raw)?;
    // Decode each 4-byte group as a little-endian dword (see the module's endianness note).
    let words = raw
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Ok(words)
}

/// Read a payload of `count` raw bytes (used only for `VCMD_CREATE_RENDERER`, whose length is a
/// byte count, not a dword count), bounded against [`MAX_VTEST_PAYLOAD_BYTES`].
fn read_payload_bytes<R: Read>(r: &mut R, count: u32) -> Result<Vec<u8>, EngineError> {
    // Bound before allocating.
    if count as u64 > MAX_VTEST_PAYLOAD_BYTES {
        return Err(EngineError::VtestFrameTooLarge { len: count as u64 });
    }
    let mut raw = vec![0u8; count as usize];
    read_full(r, &mut raw)?;
    Ok(raw)
}

/// Small helper: build a vtest protocol error with a formatted message (never a silent drop).
fn protocol_err(detail: impl Into<String>) -> EngineError {
    EngineError::VtestProtocol {
        detail: detail.into(),
    }
}

/// Require that a fixed-size command's payload is exactly `expected` dwords long, so a handler that
/// indexes fixed fields cannot read past a short payload.
fn expect_len(cmd: &str, got: usize, expected: usize) -> Result<(), EngineError> {
    if got != expected {
        return Err(protocol_err(format!(
            "{cmd}: expected {expected}-dword payload, got {got}"
        )));
    }
    Ok(())
}

/// Read and decode exactly one vtest message from `r`.
///
/// Returns `Ok(Some(cmd))` for a decoded command, `Ok(None)` on a clean end of session (the peer
/// closed at a message boundary), or an [`EngineError`] for I/O failure, an over-long length
/// prefix, or a malformed / unsupported message. This is the single point where untrusted wire
/// bytes become typed commands, so all bounds and opcode checks live here.
pub fn read_command<R: Read>(r: &mut R) -> Result<Option<VtestCommand>, EngineError> {
    // Read the 2-dword header, allowing a clean EOF here (end of session) but not mid-header.
    let mut header = [0u8; VTEST_HEADER_DWORDS * 4];
    if !read_full_or_eof(r, &mut header)? {
        return Ok(None);
    }
    // dword 0 = payload length, dword 1 = command id (both little-endian).
    let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let cmd_id = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);

    // `VCMD_CREATE_RENDERER` is the one command whose length is a *byte* count (the name string),
    // not a dword count — decode it before the generic dword path.
    if cmd_id == VCMD_CREATE_RENDERER {
        let name = read_payload_bytes(r, length)?;
        return Ok(Some(VtestCommand::CreateRenderer { name }));
    }

    // Every other command's length is a dword count; read the whole declared payload up front so
    // the stream stays in sync even if we only need a prefix of it.
    let p = read_payload_dwords(r, length)?;

    // Decode by command id. Fixed-size commands assert their exact length; variable-size ones
    // (sync-wait, submit-cmd2) validate their internal offsets. An unknown id is a hard error.
    let cmd = match cmd_id {
        VCMD_PING_PROTOCOL_VERSION => {
            // Length 0 per the protocol; anything else is malformed.
            expect_len("PING_PROTOCOL_VERSION", p.len(), 0)?;
            VtestCommand::PingProtocolVersion
        }
        VCMD_RESOURCE_BUSY_WAIT => {
            // `[handle][flags]` (`VCMD_BUSY_WAIT_SIZE == 2`).
            expect_len("RESOURCE_BUSY_WAIT", p.len(), 2)?;
            VtestCommand::ResourceBusyWait {
                handle: p[0],
                flags: p[1],
            }
        }
        VCMD_PROTOCOL_VERSION => {
            // `[version]` (`VCMD_PROTOCOL_VERSION_SIZE == 1`).
            expect_len("PROTOCOL_VERSION", p.len(), 1)?;
            VtestCommand::ProtocolVersion { version: p[0] }
        }
        VCMD_GET_PARAM => {
            // `[param]` (`VCMD_GET_PARAM_SIZE == 1`).
            expect_len("GET_PARAM", p.len(), 1)?;
            VtestCommand::GetParam { param: p[0] }
        }
        VCMD_GET_CAPSET => {
            // `[id][version]` (`VCMD_GET_CAPSET_SIZE == 2`).
            expect_len("GET_CAPSET", p.len(), 2)?;
            VtestCommand::GetCapset {
                id: p[0],
                version: p[1],
            }
        }
        VCMD_CONTEXT_INIT => {
            // `[capset_id]` (`VCMD_CONTEXT_INIT_SIZE == 1`).
            expect_len("CONTEXT_INIT", p.len(), 1)?;
            VtestCommand::ContextInit { capset_id: p[0] }
        }
        VCMD_RESOURCE_CREATE_BLOB => {
            // `[type][flags][size_lo][size_hi][id_lo][id_hi]` (`VCMD_RES_CREATE_BLOB_SIZE == 6`).
            expect_len("RESOURCE_CREATE_BLOB", p.len(), 6)?;
            VtestCommand::ResourceCreateBlob {
                blob_type: p[0],
                flags: p[1],
                size: join_u64(p[2], p[3]),
                blob_id: join_u64(p[4], p[5]),
            }
        }
        VCMD_RESOURCE_UNREF => {
            // `[res_handle]` (`VCMD_RES_UNREF_SIZE == 1`).
            expect_len("RESOURCE_UNREF", p.len(), 1)?;
            VtestCommand::ResourceUnref { res_handle: p[0] }
        }
        VCMD_SYNC_CREATE => {
            // `[value_lo][value_hi]` (`VCMD_SYNC_CREATE_SIZE == 2`).
            expect_len("SYNC_CREATE", p.len(), 2)?;
            VtestCommand::SyncCreate {
                initial_value: join_u64(p[0], p[1]),
            }
        }
        VCMD_SYNC_UNREF => {
            // `[sync_id]` (`VCMD_SYNC_UNREF_SIZE == 1`).
            expect_len("SYNC_UNREF", p.len(), 1)?;
            VtestCommand::SyncUnref { sync_id: p[0] }
        }
        VCMD_SYNC_READ => {
            // `[sync_id]` (`VCMD_SYNC_READ_SIZE == 1`).
            expect_len("SYNC_READ", p.len(), 1)?;
            VtestCommand::SyncRead { sync_id: p[0] }
        }
        VCMD_SYNC_WRITE => {
            // `[sync_id][value_lo][value_hi]` (`VCMD_SYNC_WRITE_SIZE == 3`).
            expect_len("SYNC_WRITE", p.len(), 3)?;
            VtestCommand::SyncWrite {
                sync_id: p[0],
                value: join_u64(p[1], p[2]),
            }
        }
        VCMD_SYNC_WAIT => decode_sync_wait(&p)?,
        VCMD_SUBMIT_CMD2 => decode_submit_cmd2(&p)?,
        VCMD_SUBMIT_CMD => {
            // Legacy virgl (non-Venus) submit path. Venus never uses it; reject clearly rather than
            // pretend to handle it.
            return Err(protocol_err(
                "VCMD_SUBMIT_CMD (legacy virgl submit) is not supported; Venus uses SUBMIT_CMD2",
            ));
        }
        other => {
            return Err(protocol_err(format!(
                "unsupported or unknown vtest command id {other} (length {length} dwords)"
            )));
        }
    };
    Ok(Some(cmd))
}

/// Combine a little-endian `(lo, hi)` dword pair into a `u64`, as the protocol splits 64-bit
/// sizes/ids/values across two dwords.
fn join_u64(lo: u32, hi: u32) -> u64 {
    (lo as u64) | ((hi as u64) << 32)
}

/// Decode a `VCMD_SYNC_WAIT` payload: `[flags][timeout]` then `count` triples of
/// `[sync_id][value_lo][value_hi]` (`VCMD_SYNC_WAIT_SIZE(count) == 2 + 3*count`). The count is
/// inferred from the payload length so a mismatched/odd length is rejected.
fn decode_sync_wait(p: &[u32]) -> Result<VtestCommand, EngineError> {
    // Must contain at least the flags+timeout pair.
    if p.len() < 2 {
        return Err(protocol_err(format!(
            "SYNC_WAIT: payload {} dwords is shorter than the 2-dword header",
            p.len()
        )));
    }
    // The remaining dwords must be a whole number of 3-dword `(id, lo, hi)` triples.
    let rest = p.len() - 2;
    if rest % 3 != 0 {
        return Err(protocol_err(format!(
            "SYNC_WAIT: {rest} dwords after the header is not a multiple of 3 (one per wait)"
        )));
    }
    // Pull the fixed header fields.
    let flags = p[0];
    let timeout = p[1];
    // Assemble each wait's `(sync_id, value)`.
    let waits = p[2..]
        .chunks_exact(3)
        .map(|t| (t[0], join_u64(t[1], t[2])))
        .collect();
    Ok(VtestCommand::SyncWait {
        flags,
        timeout,
        waits,
    })
}

/// Decode a `VCMD_SUBMIT_CMD2` payload into batches, slicing each batch's command dwords and sync
/// signals out of the flat body and validating every offset against the body length.
///
/// Wire layout (all dword offsets/counts): `[batch_count]` then `batch_count` batch headers of 8
/// dwords each — `[flags][cmd_offset][cmd_size][sync_offset][sync_count][ring_idx][num_in][num_out]`
/// — followed by the command streams and sync arrays the offsets point into. Offsets are dword
/// indices *from the start of the whole payload* (matching the C server's `submit_cmd2_buf[...]`).
fn decode_submit_cmd2(p: &[u32]) -> Result<VtestCommand, EngineError> {
    // The first dword is the batch count (`VCMD_SUBMIT_CMD2_BATCH_COUNT == 0`).
    if p.is_empty() {
        return Err(protocol_err("SUBMIT_CMD2: empty payload (no batch count)"));
    }
    let batch_count = p[0] as usize;
    // Bound the batch headers: 1 count dword + 8 dwords per batch must fit in the payload. This
    // both rejects a lie and prevents the indexing below from panicking. (Multiplication is on
    // usize; a malicious `batch_count` near `u32::MAX` cannot overflow usize on 64-bit and would
    // fail this check anyway since the payload is bounded to 64 MiB.)
    let headers_end = 1 + batch_count * 8;
    if headers_end > p.len() {
        return Err(protocol_err(format!(
            "SUBMIT_CMD2: {batch_count} batch headers do not fit in a {}-dword payload",
            p.len()
        )));
    }

    // Extract and validate each batch.
    let mut batches = Vec::with_capacity(batch_count);
    for i in 0..batch_count {
        // Base index of this batch's 8-dword header.
        let base = 1 + i * 8;
        let flags = p[base];
        let cmd_offset = p[base + 1] as usize;
        let cmd_size = p[base + 2] as usize;
        let sync_offset = p[base + 3] as usize;
        let sync_count = p[base + 4] as usize;
        let ring_idx = p[base + 5];
        // (num_in_syncobj / num_out_syncobj at +6/+7 are protocol-v4 syncobj passing — out of
        // scope here; we do not read them, but they are part of the 8-dword header we bounded.)

        // The command slice `[cmd_offset, cmd_offset+cmd_size)` must lie inside the payload.
        let cmd_end = cmd_offset
            .checked_add(cmd_size)
            .ok_or_else(|| protocol_err("SUBMIT_CMD2: cmd offset+size overflow"))?;
        if cmd_end > p.len() {
            return Err(protocol_err(format!(
                "SUBMIT_CMD2 batch {i}: cmd range {cmd_offset}..{cmd_end} exceeds payload {}",
                p.len()
            )));
        }
        // The sync array is `sync_count` triples starting at `sync_offset`; bound it too.
        let sync_end = sync_offset
            .checked_add(
                sync_count
                    .checked_mul(3)
                    .ok_or_else(|| protocol_err("SUBMIT_CMD2: sync_count*3 overflow"))?,
            )
            .ok_or_else(|| protocol_err("SUBMIT_CMD2: sync offset+len overflow"))?;
        if sync_end > p.len() {
            return Err(protocol_err(format!(
                "SUBMIT_CMD2 batch {i}: sync range {sync_offset}..{sync_end} exceeds payload {}",
                p.len()
            )));
        }

        // Copy out the command dwords for this batch (what we replay on the GPU).
        let cmd = p[cmd_offset..cmd_end].to_vec();
        // Assemble each sync signal `(sync_id, value)` from its 3-dword triple.
        let syncs = p[sync_offset..sync_end]
            .chunks_exact(3)
            .map(|t| (t[0], join_u64(t[1], t[2])))
            .collect();

        batches.push(SubmitBatch {
            flags,
            cmd,
            syncs,
            ring_idx,
        });
    }
    Ok(VtestCommand::SubmitCmd2 { batches })
}

/// Write one vtest reply: the 2-dword header `[len][cmd_id]` followed by `payload` dwords, all
/// little-endian. `len` is the payload's dword count, matching how the client reads it back.
fn write_reply<W: Write>(w: &mut W, cmd_id: u32, payload: &[u32]) -> Result<(), EngineError> {
    // Assemble header + payload into one buffer so the reply goes out as a single write.
    let mut out = Vec::with_capacity((VTEST_HEADER_DWORDS + payload.len()) * 4);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&cmd_id.to_le_bytes());
    for &dw in payload {
        out.extend_from_slice(&dw.to_le_bytes());
    }
    w.write_all(&out)?;
    Ok(())
}

/// Write the `VCMD_GET_CAPSET` reply, whose payload is `[valid]` followed by the capset *bytes*
/// (not dwords we own) — so it needs its own writer. The capset length must be a multiple of 4.
fn write_capset_reply<W: Write>(w: &mut W, caps: &[u8]) -> Result<(), EngineError> {
    // The renderer always returns a dword-aligned capset; guard it so the framing stays exact.
    if caps.len() % 4 != 0 {
        return Err(protocol_err(format!(
            "GET_CAPSET: capset length {} is not a multiple of 4",
            caps.len()
        )));
    }
    // Payload = 1 validity dword + caps/4 capset dwords (`resp_buf[VTEST_CMD_LEN] = 1 + size/4`).
    let payload_dwords = 1 + caps.len() / 4;
    let mut out = Vec::with_capacity((VTEST_HEADER_DWORDS + payload_dwords) * 4);
    out.extend_from_slice(&(payload_dwords as u32).to_le_bytes());
    out.extend_from_slice(&VCMD_GET_CAPSET.to_le_bytes());
    // `valid = true`.
    out.extend_from_slice(&1u32.to_le_bytes());
    // The capset blob verbatim (already dword-aligned).
    out.extend_from_slice(caps);
    w.write_all(&out)?;
    Ok(())
}

// ----------------------------------------------------------------------------------------------
// The server: read loop + per-command dispatch onto the engine.
// ----------------------------------------------------------------------------------------------

/// Mutable per-session state the dispatcher carries across messages (sync objects, resource ids,
/// negotiated version, and the eventual outcome).
struct Session {
    /// The protocol version negotiated in `VCMD_PROTOCOL_VERSION` (0 until then).
    protocol_version: u32,
    /// Monotonic source of resource ids handed out for `VCMD_RESOURCE_CREATE_BLOB` (must be > 0,
    /// since Mesa asserts `res_id > 0`).
    next_res_id: u32,
    /// Monotonic source of sync ids handed out for `VCMD_SYNC_CREATE`.
    next_sync_id: u32,
    /// Stubbed sync-object timeline values, keyed by sync id (`SYNC_CREATE`/`WRITE`/`READ`).
    syncs: std::collections::HashMap<u32, u64>,
    /// Live blob resource ids we handed out (so `RESOURCE_UNREF` can drop them).
    resources: std::collections::HashSet<u32>,
    /// The outcome accumulated so far, returned when the session ends.
    outcome: VtestOutcome,
}

impl Session {
    /// A fresh session before any message is processed.
    fn new() -> Self {
        Session {
            protocol_version: 0,
            // Start ids at 1 so `0` stays a sentinel (Mesa treats res_id 0 / handle 0 specially).
            next_res_id: 1,
            next_sync_id: 1,
            syncs: std::collections::HashMap::new(),
            resources: std::collections::HashSet::new(),
            outcome: VtestOutcome::default(),
        }
    }
}

/// Serve the vtest protocol on `stream`, driving `engine`, until the client closes the connection.
///
/// This is the entry point Task 2 delivers: it performs the handshake, then reads and dispatches
/// messages in a loop, routing the Venus-relevant ones onto the [`RenderEngine`]:
/// - `VCMD_CONTEXT_INIT` → [`RenderEngine::create_venus_context`],
/// - `VCMD_SUBMIT_CMD2` → [`RenderEngine::submit`] (once per batch),
/// - `VCMD_GET_CAPSET` → [`RenderEngine::venus_capset`].
///
/// It answers the handshake / resource / sync commands in-band (with the fd side channels and
/// pixel readback deferred to later tasks — see the module scope note).
///
/// # Generic over the transport
/// `S: Read + Write` is deliberate: C0 passes a Unix socket; SP2 passes a QUIC stream unchanged.
///
/// # Inputs / outputs
/// - `stream`: the byte transport carrying the vtest protocol.
/// - `engine`: the render engine every Venus command is routed to.
/// - Returns a [`VtestOutcome`] (notably the resource id for Task 3's readback) on a clean end of
///   session, or an [`EngineError`] on I/O failure, a malformed/unsupported message, or an engine
///   error.
pub fn serve_vtest<S: Read + Write>(
    mut stream: S,
    engine: &mut dyn RenderEngine,
) -> Result<VtestOutcome, EngineError> {
    // Per-connection state accumulated across messages.
    let mut session = Session::new();

    // Read and dispatch messages until the peer closes at a message boundary (clean end).
    loop {
        match read_command(&mut stream)? {
            // A decoded command: handle it (may write a reply and/or call the engine).
            Some(cmd) => dispatch(cmd, &mut stream, engine, &mut session)?,
            // Clean EOF: the session is over; hand back what we accumulated.
            None => return Ok(session.outcome),
        }
    }
}

/// Handle one decoded command: write any protocol-mandated reply and route Venus work to `engine`.
///
/// Every command is either answered per the protocol or routed to the engine; none is silently
/// ignored. Commands whose full behavior is deferred (fd replies, pixel readback) still get their
/// in-band reply and a clear boundary, documented at each arm.
fn dispatch<S: Read + Write>(
    cmd: VtestCommand,
    stream: &mut S,
    engine: &mut dyn RenderEngine,
    session: &mut Session,
) -> Result<(), EngineError> {
    match cmd {
        VtestCommand::CreateRenderer { name: _ } => {
            // Opening message; the name is only a debug label. No reply is defined for it.
            Ok(())
        }
        VtestCommand::PingProtocolVersion => {
            // Echo the ping back (empty payload) to signal we support version negotiation. Mesa
            // reads this header and, seeing our id, proceeds to `VCMD_PROTOCOL_VERSION`.
            write_reply(stream, VCMD_PING_PROTOCOL_VERSION, &[])
        }
        VtestCommand::ResourceBusyWait {
            handle: _,
            flags: _,
        } => {
            // The handshake dummy (and later resource polls). We do not track GPU busy state in the
            // skeleton, so we always report "not busy" (0) — a real fence-aware answer is a later
            // task. Reply is `[busy]` (`VCMD_BUSY_WAIT` header + 1 dword).
            write_reply(stream, VCMD_RESOURCE_BUSY_WAIT, &[0])
        }
        VtestCommand::ProtocolVersion { version } => {
            // Negotiate down to the min of what each side supports, exactly like the C server.
            let negotiated = version.min(VTEST_PROTOCOL_VERSION);
            session.protocol_version = negotiated;
            // Reply `[negotiated]`. Mesa requires this be ≥ 3 or it aborts its own init.
            write_reply(stream, VCMD_PROTOCOL_VERSION, &[negotiated])
        }
        VtestCommand::GetParam { param } => {
            // Answer `[valid][value]`. We only know MAX_TIMELINE_COUNT (which Venus requires);
            // anything else is reported invalid (valid=0), never guessed.
            let (valid, value) = if param == VCMD_PARAM_MAX_TIMELINE_COUNT {
                (1, VTEST_MAX_TIMELINE_COUNT)
            } else {
                (0, 0)
            };
            write_reply(stream, VCMD_GET_PARAM, &[valid, value])
        }
        VtestCommand::GetCapset { id, version } => {
            // Only the Venus capset is meaningful to us. For it, fetch the *real* capset from the
            // engine and send `[valid=1] + caps`. For any other id, or if the engine has no Venus
            // capset (no GPU), reply `[valid=0]` so the client fails cleanly rather than hang.
            if id == VENUS_CAPSET_ID {
                match engine.venus_capset(version) {
                    Ok(caps) => write_capset_reply(stream, &caps),
                    Err(_) => write_reply(stream, VCMD_GET_CAPSET, &[0]),
                }
            } else {
                write_reply(stream, VCMD_GET_CAPSET, &[0])
            }
        }
        VtestCommand::ContextInit { capset_id } => {
            // Bind the connection to a capset by creating the GPU context. Venus (4) is the only
            // capset we serve; reject anything else rather than create a wrong context.
            if capset_id != VENUS_CAPSET_ID {
                return Err(protocol_err(format!(
                    "CONTEXT_INIT: unsupported capset id {capset_id} (only Venus/4 is served)"
                )));
            }
            // Create the Venus context on the engine. No reply is defined for context-init.
            engine.create_venus_context(VTEST_CONTEXT_ID)?;
            session.outcome.context_id = Some(VTEST_CONTEXT_ID);
            Ok(())
        }
        VtestCommand::ResourceCreateBlob { size: _, .. } => {
            // Stub: hand out a fresh resource id and record it as the current readback candidate
            // (Task 3 will map Venus's real render-target blob). Reply is `[res_id]`.
            //
            // DEFERRED: the full protocol also returns an mmap'able fd over SCM_RIGHTS right after
            // this header. A generic `Read + Write` stream cannot carry an fd, so the fd half is
            // Task 4's Unix-socket concern (and ultimately Rayland's own memory-sharing protocol).
            // A *live* Mesa client blocks on that fd; the in-band reply here is what the framing
            // unit tests exercise.
            let res_id = session.next_res_id;
            session.next_res_id = session.next_res_id.saturating_add(1);
            session.resources.insert(res_id);
            session.outcome.rendered_resource_id = Some(res_id);
            write_reply(stream, VCMD_RESOURCE_CREATE_BLOB, &[res_id])
        }
        VtestCommand::ResourceUnref { res_handle } => {
            // Drop the resource from our bookkeeping. No reply is defined.
            session.resources.remove(&res_handle);
            Ok(())
        }
        VtestCommand::SyncCreate { initial_value } => {
            // Stub timeline sync object: assign an id and remember its value. Reply is `[sync_id]`.
            let sync_id = session.next_sync_id;
            session.next_sync_id = session.next_sync_id.saturating_add(1);
            session.syncs.insert(sync_id, initial_value);
            write_reply(stream, VCMD_SYNC_CREATE, &[sync_id])
        }
        VtestCommand::SyncUnref { sync_id } => {
            // Drop the sync object. No reply.
            session.syncs.remove(&sync_id);
            Ok(())
        }
        VtestCommand::SyncRead { sync_id } => {
            // Report the stored timeline value (0 if unknown). Reply is `[value_lo][value_hi]`.
            let val = session.syncs.get(&sync_id).copied().unwrap_or(0);
            write_reply(stream, VCMD_SYNC_READ, &[val as u32, (val >> 32) as u32])
        }
        VtestCommand::SyncWrite { sync_id, value } => {
            // Update the stored value. No reply.
            session.syncs.insert(sync_id, value);
            Ok(())
        }
        VtestCommand::SyncWait { waits, .. } => {
            // Signal semantics are stubbed: we treat every waited-on sync as already at its target
            // (we advance our stored value to the requested one) and reply with the empty header.
            //
            // DEFERRED: the real reply is a *pollable fd* over SCM_RIGHTS that becomes readable when
            // the syncs signal — same generic-stream limitation as blob creation, deferred to Task
            // 4 / real fence handling. We emit only the in-band header here.
            for (sync_id, value) in waits {
                session.syncs.insert(sync_id, value);
            }
            write_reply(stream, VCMD_SYNC_WAIT, &[])
        }
        VtestCommand::SubmitCmd2 { batches } => {
            // The load-bearing path: replay each batch's Venus command dwords on the GPU. No reply
            // is defined for a plain submit (fence-fd replies are the deferred fd path).
            for batch in batches {
                // Reinterpret the command dwords as the byte buffer `submit` expects. The length is
                // a whole number of dwords by construction, so it satisfies `submit`'s alignment
                // and multiple-of-4 requirements.
                let bytes = dwords_to_bytes(&batch.cmd);
                engine.submit(VTEST_CONTEXT_ID, &bytes)?;
                // Reflect the batch's completion signals into our stubbed sync values.
                for (sync_id, value) in batch.syncs {
                    session.syncs.insert(sync_id, value);
                }
                session.outcome.submitted_batches += 1;
            }
            Ok(())
        }
    }
}

/// Flatten a dword slice into little-endian bytes for [`RenderEngine::submit`] (which takes a byte
/// buffer and re-aligns it internally). Kept explicit so the endianness is visible at the boundary.
fn dwords_to_bytes(dwords: &[u32]) -> Vec<u8> {
    // 4 bytes per dword; build the little-endian byte image.
    let mut bytes = Vec::with_capacity(dwords.len() * 4);
    for &dw in dwords {
        bytes.extend_from_slice(&dw.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a raw vtest message (header + dword payload) as little-endian bytes, for feeding the
    /// parser synthetic ground-truth exactly as the wire carries it.
    fn msg(cmd_id: u32, payload: &[u32]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(&cmd_id.to_le_bytes());
        for &d in payload {
            v.extend_from_slice(&d.to_le_bytes());
        }
        v
    }

    /// A `RenderEngine` test double that records the calls `serve_vtest` makes, so the full
    /// read-loop + dispatch can be exercised with no GPU. Returns a canned Venus capset.
    struct MockEngine {
        contexts: Vec<u32>,
        submits: Vec<(u32, Vec<u8>)>,
        capset: Vec<u8>,
    }

    impl RenderEngine for MockEngine {
        fn create_venus_context(&mut self, ctx_id: u32) -> Result<(), EngineError> {
            self.contexts.push(ctx_id);
            Ok(())
        }
        fn submit(&mut self, ctx_id: u32, cmd: &[u8]) -> Result<(), EngineError> {
            self.submits.push((ctx_id, cmd.to_vec()));
            Ok(())
        }
        fn venus_capset(&mut self, _version: u32) -> Result<Vec<u8>, EngineError> {
            Ok(self.capset.clone())
        }
    }

    #[test]
    fn create_renderer_length_is_bytes_not_dwords() {
        // Ground truth from the live capture: len field = 11 for the 11-byte name "vulkaninfo\0".
        let name = b"vulkaninfo\0";
        let mut raw = Vec::new();
        raw.extend_from_slice(&(name.len() as u32).to_le_bytes()); // len = 11 BYTES
        raw.extend_from_slice(&VCMD_CREATE_RENDERER.to_le_bytes()); // cmd_id = 8
        raw.extend_from_slice(name);

        let mut cur = Cursor::new(raw);
        let cmd = read_command(&mut cur).expect("decodes").expect("a command");
        assert_eq!(
            cmd,
            VtestCommand::CreateRenderer {
                name: name.to_vec()
            }
        );
    }

    #[test]
    fn handshake_sequence_parses_in_order() {
        // The exact handshake order Mesa's vtest_init performs (matches the captured bytes), fed
        // back-to-back through one stream to prove message boundaries are tracked correctly.
        let mut raw = Vec::new();
        raw.extend_from_slice(&msg(VCMD_PING_PROTOCOL_VERSION, &[]));
        raw.extend_from_slice(&msg(VCMD_RESOURCE_BUSY_WAIT, &[0, 0]));
        raw.extend_from_slice(&msg(VCMD_PROTOCOL_VERSION, &[4]));
        raw.extend_from_slice(&msg(VCMD_GET_PARAM, &[VCMD_PARAM_MAX_TIMELINE_COUNT]));
        raw.extend_from_slice(&msg(VCMD_GET_CAPSET, &[VENUS_CAPSET_ID, 0]));
        raw.extend_from_slice(&msg(VCMD_CONTEXT_INIT, &[VENUS_CAPSET_ID]));

        let mut cur = Cursor::new(raw);
        let mut got = Vec::new();
        while let Some(c) = read_command(&mut cur).expect("each message decodes") {
            got.push(c);
        }
        assert_eq!(
            got,
            vec![
                VtestCommand::PingProtocolVersion,
                VtestCommand::ResourceBusyWait {
                    handle: 0,
                    flags: 0
                },
                VtestCommand::ProtocolVersion { version: 4 },
                VtestCommand::GetParam { param: 1 },
                VtestCommand::GetCapset { id: 4, version: 0 },
                VtestCommand::ContextInit { capset_id: 4 },
            ]
        );
    }

    #[test]
    fn resource_create_blob_joins_64bit_fields() {
        // size = 0x1_0000_0000 (lo=0, hi=1); blob_id = 0xDEAD_BEEF (lo, hi=0).
        let cmd = read_command(&mut Cursor::new(msg(
            VCMD_RESOURCE_CREATE_BLOB,
            &[2 /*HOST3D*/, 1 /*MAPPABLE*/, 0, 1, 0xDEAD_BEEF, 0],
        )))
        .unwrap()
        .unwrap();
        assert_eq!(
            cmd,
            VtestCommand::ResourceCreateBlob {
                blob_type: 2,
                flags: 1,
                size: 0x1_0000_0000,
                blob_id: 0xDEAD_BEEF,
            }
        );
    }

    #[test]
    fn submit_cmd2_extracts_batch_command_stream() {
        // One batch: header at dwords 1..9, command stream of 3 dwords placed at offset 9.
        // Layout: [batch_count=1]
        //         [flags, cmd_offset=9, cmd_size=3, sync_offset=0, sync_count=0, ring_idx=0,
        //          num_in=0, num_out=0]
        //         [0xAAAA, 0xBBBB, 0xCCCC]   <- the command dwords at offset 9
        let payload = [
            1u32, // batch_count
            0x1,  // flags (RING_IDX)
            9, 3, // cmd_offset, cmd_size
            0, 0, // sync_offset, sync_count
            0, // ring_idx
            0, 0, // num_in_syncobj, num_out_syncobj
            0xAAAA, 0xBBBB, 0xCCCC, // the command stream
        ];
        let cmd = read_command(&mut Cursor::new(msg(VCMD_SUBMIT_CMD2, &payload)))
            .unwrap()
            .unwrap();
        let VtestCommand::SubmitCmd2 { batches } = cmd else {
            panic!("expected SubmitCmd2, got {cmd:?}");
        };
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].cmd, vec![0xAAAA, 0xBBBB, 0xCCCC]);
        assert_eq!(batches[0].ring_idx, 0);
        assert!(batches[0].syncs.is_empty());
    }

    #[test]
    fn submit_cmd2_with_syncs_parses_signal_pairs() {
        // One batch with a 2-dword cmd at offset 9 and one sync (id=7, value=0x5_0000_0001) at
        // offset 11 (3 dwords: id, lo, hi).
        let payload = [
            1u32, // batch_count
            0x1,  // flags
            9, 2, // cmd_offset, cmd_size
            11, 1, // sync_offset, sync_count
            0, // ring_idx
            0, 0, // num_in, num_out
            0x11, 0x22, // cmd dwords (offset 9,10)
            7, 1, 5, // sync triple (offset 11,12,13): id=7, lo=1, hi=5
        ];
        let cmd = read_command(&mut Cursor::new(msg(VCMD_SUBMIT_CMD2, &payload)))
            .unwrap()
            .unwrap();
        let VtestCommand::SubmitCmd2 { batches } = cmd else {
            panic!("expected SubmitCmd2");
        };
        assert_eq!(batches[0].cmd, vec![0x11, 0x22]);
        assert_eq!(batches[0].syncs, vec![(7, 0x5_0000_0001)]);
    }

    #[test]
    fn submit_cmd2_rejects_out_of_range_cmd_offset() {
        // cmd_offset+cmd_size (9+100) runs off the end of the payload → must be a protocol error,
        // never a panic or an out-of-bounds slice.
        let payload = [1u32, 0x1, 9, 100, 0, 0, 0, 0, 0, 0xAAAA];
        let err = read_command(&mut Cursor::new(msg(VCMD_SUBMIT_CMD2, &payload))).unwrap_err();
        assert!(
            matches!(err, EngineError::VtestProtocol { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn oversized_length_prefix_is_refused_before_allocating() {
        // Header claims a payload of u32::MAX dwords (~16 GiB). No body follows: the parser must
        // reject it from the length prefix alone, before allocating.
        let mut raw = Vec::new();
        raw.extend_from_slice(&u32::MAX.to_le_bytes()); // length = u32::MAX dwords
        raw.extend_from_slice(&VCMD_SUBMIT_CMD2.to_le_bytes());
        let err = read_command(&mut Cursor::new(raw)).unwrap_err();
        assert!(
            matches!(err, EngineError::VtestFrameTooLarge { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn unknown_command_id_is_an_error_not_a_silent_drop() {
        // A command id we do not implement must surface as a typed protocol error.
        let err = read_command(&mut Cursor::new(msg(9999, &[1, 2, 3]))).unwrap_err();
        assert!(
            matches!(err, EngineError::VtestProtocol { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn fixed_command_wrong_length_is_rejected() {
        // CONTEXT_INIT must be exactly 1 dword; a 2-dword payload is malformed.
        let err = read_command(&mut Cursor::new(msg(VCMD_CONTEXT_INIT, &[4, 4]))).unwrap_err();
        assert!(
            matches!(err, EngineError::VtestProtocol { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn clean_eof_at_message_boundary_ends_the_stream() {
        // An empty stream (peer closed at a boundary) decodes to None, not an error.
        let mut cur = Cursor::new(Vec::<u8>::new());
        assert_eq!(read_command(&mut cur).unwrap(), None);
    }

    #[test]
    fn truncated_header_is_an_io_error() {
        // Only 3 bytes of an 8-byte header present → truncation, not a clean end.
        let err = read_command(&mut Cursor::new(vec![1u8, 2, 3])).unwrap_err();
        assert!(matches!(err, EngineError::VtestIo(_)), "got {err:?}");
    }

    #[test]
    fn full_session_drives_the_engine_and_writes_replies() {
        // A complete no-GPU run through serve_vtest: full handshake, context init, and a submit.
        // The MockEngine records the routed calls; a Vec collects the replies we write back.
        let capset = vec![0u8; 16]; // 4 dwords of canned Venus capset
        let mut input = Vec::new();
        input.extend_from_slice(&{
            // create_renderer with a byte-length name
            let name = b"probe\0";
            let mut m = Vec::new();
            m.extend_from_slice(&(name.len() as u32).to_le_bytes());
            m.extend_from_slice(&VCMD_CREATE_RENDERER.to_le_bytes());
            m.extend_from_slice(name);
            m
        });
        input.extend_from_slice(&msg(VCMD_PING_PROTOCOL_VERSION, &[]));
        input.extend_from_slice(&msg(VCMD_RESOURCE_BUSY_WAIT, &[0, 0]));
        input.extend_from_slice(&msg(VCMD_PROTOCOL_VERSION, &[4]));
        input.extend_from_slice(&msg(VCMD_GET_PARAM, &[VCMD_PARAM_MAX_TIMELINE_COUNT]));
        input.extend_from_slice(&msg(VCMD_GET_CAPSET, &[VENUS_CAPSET_ID, 0]));
        input.extend_from_slice(&msg(VCMD_CONTEXT_INIT, &[VENUS_CAPSET_ID]));
        // A submit: one batch, 2 command dwords at offset 9.
        input.extend_from_slice(&msg(
            VCMD_SUBMIT_CMD2,
            &[1, 0x1, 9, 2, 0, 0, 0, 0, 0, 0xCAFE, 0xF00D],
        ));

        // `Cursor` is Read; a `Vec` is Write; a duplex pair lets serve_vtest read requests and
        // write replies. We combine them into one Read+Write object.
        struct Duplex {
            input: Cursor<Vec<u8>>,
            output: Vec<u8>,
        }
        impl Read for Duplex {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.input.read(buf)
            }
        }
        impl Write for Duplex {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.output.write(buf)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.output.flush()
            }
        }

        let mut engine = MockEngine {
            contexts: Vec::new(),
            submits: Vec::new(),
            capset: capset.clone(),
        };
        let mut duplex = Duplex {
            input: Cursor::new(input),
            output: Vec::new(),
        };
        let outcome = serve_vtest(&mut duplex, &mut engine).expect("session completes cleanly");

        // The engine saw exactly the routed Venus work.
        assert_eq!(engine.contexts, vec![VTEST_CONTEXT_ID]);
        assert_eq!(engine.submits.len(), 1);
        assert_eq!(engine.submits[0].0, VTEST_CONTEXT_ID);
        // The 2 command dwords 0xCAFE, 0xF00D as little-endian bytes.
        assert_eq!(
            engine.submits[0].1,
            vec![0xFE, 0xCA, 0, 0, 0x0D, 0xF0, 0, 0]
        );
        assert_eq!(outcome.context_id, Some(VTEST_CONTEXT_ID));
        assert_eq!(outcome.submitted_batches, 1);

        // The replies begin with ping-echo, busy-wait, protocol-version, get-param, get-capset.
        // Decode them back to confirm the handshake responses are well-formed.
        let mut rcur = Cursor::new(duplex.output);
        let ping = read_reply(&mut rcur);
        assert_eq!(ping, (VCMD_PING_PROTOCOL_VERSION, vec![]));
        let busy = read_reply(&mut rcur);
        assert_eq!(busy, (VCMD_RESOURCE_BUSY_WAIT, vec![0]));
        let proto = read_reply(&mut rcur);
        assert_eq!(proto, (VCMD_PROTOCOL_VERSION, vec![4]));
        let param = read_reply(&mut rcur);
        assert_eq!(param, (VCMD_GET_PARAM, vec![1, VTEST_MAX_TIMELINE_COUNT]));
        // get-capset reply: [valid=1] + 4 capset dwords (all zero here).
        let caps = read_reply(&mut rcur);
        assert_eq!(caps.0, VCMD_GET_CAPSET);
        assert_eq!(caps.1, vec![1, 0, 0, 0, 0]);
    }

    /// Read one reply (header + dword payload) back out of a buffer, for asserting on server output.
    fn read_reply(cur: &mut Cursor<Vec<u8>>) -> (u32, Vec<u32>) {
        let mut hdr = [0u8; 8];
        cur.read_exact(&mut hdr).expect("reply header");
        let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let cmd = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let mut payload = Vec::with_capacity(len);
        for _ in 0..len {
            let mut d = [0u8; 4];
            cur.read_exact(&mut d).expect("reply payload dword");
            payload.push(u32::from_le_bytes(d));
        }
        (cmd, payload)
    }
}
