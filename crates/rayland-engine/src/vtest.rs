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
//! # fd passing is mandatory, and that has consequences (the Task 4a correction)
//! Tasks 2/3 served this protocol over a generic `S: Read + Write` and deferred the fd side
//! channel, on the stated assumption that SP2's QUIC transport could then swap in unchanged. **A
//! live Mesa client proved that assumption false**: `VCMD_RESOURCE_CREATE_BLOB` and
//! `VCMD_SYNC_WAIT` must reply with a real file descriptor over an `SCM_RIGHTS` control message,
//! and the client blocks in `recvmsg` forever without it — so the old bound could never have served
//! a real client at all. `serve_vtest` is now generic over [`VtestTransport`] (a byte stream **plus**
//! `send_fd`), and C0 serves it over a `UnixStream`.
//!
//! The honest consequence for SP2/(c)1, recorded here because it is a *finding*, not a to-do: a
//! blob resource is **shared memory**, and the client writes its Venus command ring directly into
//! the pages the descriptor names. QUIC has neither fd passing nor shared memory, so (c)1 cannot
//! "just swap the socket" — those ring writes have to become explicit, shipped bytes, which is a
//! protocol design question rather than a transport substitution. [`VtestTransport::send_fd`] being
//! a *required* method is what forces that question to be confronted at compile time.
//!
//! # Deliberate scope (C0 Tasks 2, 3 and 4a)
//! This is the parser + dispatcher. The handshake, `VCMD_CONTEXT_INIT` → `create_venus_context`,
//! `VCMD_SUBMIT_CMD2` → `submit` (Task 2), `VCMD_RESOURCE_CREATE_BLOB` → `create_blob_resource` /
//! `VCMD_RESOURCE_UNREF` → `unref_resource` (Task 3), and both fd replies (Task 4a) are wired for
//! real — every one of them reaches the actual `RenderEngine` or the actual kernel, not vtest-local
//! bookkeeping. What remains deliberately stubbed, and honestly so:
//! - **real timeline semantics.** The `VCMD_SYNC_*` family is still a local sync-id → value map
//!   (see `Session`) rather than a model of actual GPU timeline completion. Its `VCMD_SYNC_WAIT`
//!   reply now sends a **real, pollable eventfd** — not a fabricated descriptor — but, because
//!   every sync in the stub is by construction already at its target, that eventfd is always
//!   pre-signaled. That is precisely virglrenderer's own `is_ready` branch, taken unconditionally;
//!   a wait that should genuinely block does not. Task 3's fence-wait
//!   (`RenderEngine::read_back`) is a separate, real mechanism that does not depend on this stub.
//!
//! Unimplemented opcodes are reported as a typed error — never silently dropped.

// Reading requests and writing in-band replies. The transport also carries file descriptors, which
// is why `serve_vtest` is generic over `VtestTransport` rather than these two traits alone.
use std::io::{Read, Write};
// Borrowing an owned descriptor to hand to `send_fd`, which duplicates rather than consumes it.
use std::os::fd::AsFd;

// The traits we drive and the crate error type every failure maps into.
use crate::{EngineError, RenderEngine, VtestTransport};

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

/// `VCMD_SUBMIT_CMD2_FLAG_RING_IDX` (bit 0) — "this batch's `ring_idx` field is meaningful".
///
/// The **only** `SUBMIT_CMD2` batch flag this server implements, and the only one Mesa's Venus vtest
/// backend sets (confirmed by a live capture: `flags = 0x1`). The other two defined flags,
/// `..._IN_FENCE_FD` (1<<1) and `..._OUT_FENCE_FD` (1<<2), make the C server receive or send a file
/// descriptor out of band; `decode_submit_cmd2` rejects them rather than mis-frame the stream.
const VCMD_SUBMIT_CMD2_FLAG_RING_IDX: u32 = 1 << 0;

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
    /// `VCMD_RESOURCE_CREATE_BLOB`: allocate a host/guest-shared memory blob (the Venus command
    /// ring, staging buffers, device memory). Its reply carries a res_id **and an fd** — both are
    /// sent (Task 4a); the client `mmap`s the fd and writes commands straight into those pages.
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
    /// fd* over `SCM_RIGHTS`, which is sent for real (Task 4a) — though always pre-signaled, since
    /// this server's sync objects are a stub. See the module scope note.
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
/// Its reason for being is `rendered_resource_id`: a resource id that genuinely exists in the
/// engine (Task 3 routes `VCMD_RESOURCE_CREATE_BLOB` through `RenderEngine::create_blob_resource`,
/// not a vtest-local counter). In the vtest/Venus data path the rendered image lives in a blob
/// resource, so we report the most recently created blob as a best-effort readback candidate.
///
/// # What a live client showed about this field (Task 4a) — read before relying on it
/// Observing a real Mesa Venus client makes the "best-effort" above concrete, and mostly negative:
/// the blobs it creates are its **command ring and staging shmem**, not rendered images (a trivial
/// init-only client requested exactly two, both `HOST3D`/`MAPPABLE` with `blob_id = 0`, of ~128 KiB
/// and 1 MiB). So "the most recently created blob" is not the rendered frame in any run observed so
/// far, and there is no reason to think it would be in general — identifying the frame requires
/// understanding the client's object graph, which is Task 4b's concern. `RenderEngine::read_back`'s
/// doc comment explains the second half of the problem: a *blob* resource cannot be read back the
/// way a classic resource can. Both must be resolved before this field means what its name suggests.
/// The counters are for diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VtestOutcome {
    /// The most recently created blob resource id, if any — **not**, as things stand, the rendered
    /// frame. See this struct's doc comment for what a live client actually put here and why
    /// reading it back is still an open problem.
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
    // Before interpreting anything, optionally dump the raw payload (see `dump_submit_cmd2`). This
    // is what byte-verified the layout below against a live Mesa Venus client rather than against
    // the C source alone.
    dump_submit_cmd2(p);

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
        let num_in_syncobj = p[base + 6];
        let num_out_syncobj = p[base + 7];

        // Refuse the batch features this server does not implement, rather than ignore them.
        //
        // This is not defensive boilerplate — ignoring these **silently corrupts the connection**.
        // Reading virglrenderer's `vtest_submit_cmd2_batch` shows why: a nonzero `num_in_syncobj` /
        // `num_out_syncobj` makes the server read *additional bytes off the socket* beyond this
        // message's declared `length` (an array of `drm_virtgpu_execbuffer_syncobj` per batch), and
        // `VCMD_SUBMIT_CMD2_FLAG_IN_FENCE_FD` / `..._OUT_FENCE_FD` make it receive / send a file
        // descriptor. A decoder that skipped any of them would leave those bytes in the stream and
        // mis-frame every subsequent message, or leave the client waiting on a fence fd forever —
        // failures that would surface far from their cause.
        //
        // Mesa's Venus vtest backend sets `flags = VCMD_SUBMIT_CMD2_FLAG_RING_IDX` and leaves both
        // syncobj counts at 0, which a live capture confirms (flags=0x1, num_in=0, num_out=0), so
        // this rejects nothing a real Venus client sends today. It is what makes a *future* client
        // that does use them fail loudly here instead of subtly downstream.
        if num_in_syncobj != 0 || num_out_syncobj != 0 {
            return Err(protocol_err(format!(
                "SUBMIT_CMD2 batch {i}: syncobj passing is not supported (num_in_syncobj={num_in_syncobj}, num_out_syncobj={num_out_syncobj}); it carries out-of-band bytes this server would mis-frame"
            )));
        }
        if flags & !VCMD_SUBMIT_CMD2_FLAG_RING_IDX != 0 {
            return Err(protocol_err(format!(
                "SUBMIT_CMD2 batch {i}: unsupported flags {flags:#x} (only VCMD_SUBMIT_CMD2_FLAG_RING_IDX is implemented; the fence-fd flags carry an out-of-band descriptor)"
            )));
        }

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

/// The environment variable that enables [`dump_submit_cmd2`]. Set it to any non-empty value.
const DUMP_ENV_VAR: &str = "RAYLAND_VTEST_DUMP";

/// Dumps a raw `VCMD_SUBMIT_CMD2` payload as hex, plus this decoder's field-by-field
/// interpretation of it, to stderr — **only** when [`DUMP_ENV_VAR`] is set.
///
/// # Why this exists (Task 4a's headline deliverable)
/// Task 2 derived the `SUBMIT_CMD2` body layout from virglrenderer's C source but **never saw a
/// real one**: the client blocks on the blob fd long before it ever sends a submit, so every
/// downstream assumption rested on a reading of C macros. Task 2's own review flagged it as a
/// carry-forward: *do not trust `engine.submit` until real bytes confirm it.* This function is the
/// instrument that confirmed it. It prints what actually arrived, so the decode can be checked
/// against reality rather than against the same source it was derived from.
///
/// It is deliberately behind an environment variable rather than a feature flag or a log level:
/// a live Venus client sends thousands of these, so this is a diagnostic you reach for once, not
/// something that should ever be on by default. Off, it costs one `var_os` lookup per submit.
///
/// # Inputs / outputs
/// - `p`: the message's raw payload dwords, exactly as they came off the wire and before any
///   validation — so a *malformed* submit is dumped too, which is precisely when a dump is most
///   useful. Nothing is returned; this is a pure diagnostic with no effect on decoding.
fn dump_submit_cmd2(p: &[u32]) {
    // Off unless explicitly asked for. `var_os` (not `var`) so a non-UTF-8 value still enables it.
    if std::env::var_os(DUMP_ENV_VAR).is_none() {
        return;
    }

    eprintln!("--- SUBMIT_CMD2 raw payload: {} dwords ---", p.len());
    // Raw hex, 8 dwords per line with the dword index of each line. This is the ground truth: it
    // is printed before any interpretation, so it stays correct even if the decode below is wrong.
    for (line, chunk) in p.chunks(8).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|d| format!("{d:08x}")).collect();
        eprintln!("  [{:4}] {}", line * 8, hex.join(" "));
    }

    // The decoder's reading of those bytes, so the two can be compared by eye. Deliberately
    // re-derived here from `p` rather than taken from the decode below — a dump that shared the
    // decode's own bounds checks could not reveal a decode that is wrong.
    let Some(&batch_count) = p.first() else {
        eprintln!("  (empty payload: no batch count)");
        return;
    };
    eprintln!("  batch_count = {batch_count}");
    for i in 0..batch_count as usize {
        // Per the pinned layout each batch header is 8 dwords at `1 + 8*i`. Bounds-check by hand:
        // this runs before validation, so a lying `batch_count` must not panic the dump.
        let base = 1 + i * 8;
        let Some(h) = p.get(base..base + 8) else {
            eprintln!("  batch {i}: header at dword {base} runs past the payload — TRUNCATED");
            return;
        };
        eprintln!(
            "  batch {i}: flags={:#x} cmd_offset={} cmd_size={} sync_offset={} sync_count={} ring_idx={} num_in_syncobj={} num_out_syncobj={}",
            h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]
        );
    }
}

/// Dumps a `VCMD_RESOURCE_CREATE_BLOB` request to stderr — **only** when [`DUMP_ENV_VAR`] is set.
///
/// # Why this is worth its own dump (the (c)1 input)
/// Which `blob_mem` kinds a live client actually asks for, and how large they are, is not a
/// curiosity: it is the input to Rayland's transport design. A blob is **shared memory**, and the
/// client writes its Venus command ring straight into it. Whatever appears here is precisely what a
/// future QUIC transport — which has no shared memory and no fd passing — will have to replace with
/// explicitly shipped bytes. Observing it from a real client, rather than reasoning about what a
/// client "should" request, is the point.
///
/// # Inputs / outputs
/// - `blob_type`, `flags`, `size`, `blob_id`: the request's decoded fields, exactly as they came off
///   the wire. Nothing is returned; this is a pure diagnostic.
fn dump_blob_request(blob_type: u32, flags: u32, size: u64, blob_id: u64) {
    // Off unless explicitly asked for; see `dump_submit_cmd2` for why this is an env var.
    if std::env::var_os(DUMP_ENV_VAR).is_none() {
        return;
    }
    // Name the two fields whose *meaning* is the finding, rather than making a reader decode them.
    let kind = match blob_type {
        1 => "GUEST (server-allocated memfd + iovec)",
        2 => "HOST3D (driver-allocated, exported)",
        3 => "HOST3D_GUEST (server-allocated memfd + iovec)",
        other => return eprintln!("--- RESOURCE_CREATE_BLOB: unsupported blob_mem {other} ---"),
    };
    eprintln!(
        "--- RESOURCE_CREATE_BLOB: blob_mem={blob_type} [{kind}] flags={flags:#x} size={size} blob_id={blob_id} ---"
    );
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

/// Mutable per-session state the dispatcher carries across messages (sync objects, negotiated
/// version, and the eventual outcome).
///
/// Resource ids are **not** tracked here (Task 2 had a local `next_res_id` counter and a
/// `resources` set standing in for the engine's real resource path — Task 3 removes both: resource
/// creation and release now route through [`RenderEngine::create_blob_resource`] and
/// [`RenderEngine::unref_resource`], which are the single source of truth for which resources
/// exist. Duplicating that bookkeeping locally would only invite the two copies to drift.
struct Session {
    /// The protocol version negotiated in `VCMD_PROTOCOL_VERSION` (0 until then).
    protocol_version: u32,
    /// Monotonic source of sync ids handed out for `VCMD_SYNC_CREATE`.
    next_sync_id: u32,
    /// Stubbed sync-object timeline values, keyed by sync id (`SYNC_CREATE`/`WRITE`/`READ`).
    syncs: std::collections::HashMap<u32, u64>,
    /// The outcome accumulated so far, returned when the session ends.
    outcome: VtestOutcome,
}

impl Session {
    /// A fresh session before any message is processed.
    fn new() -> Self {
        Session {
            protocol_version: 0,
            // Start ids at 1 so `0` stays a sentinel (Mesa treats sync id 0 specially, matching the
            // engine's own resource-id convention — see `VirglEngine::next_resource_id`).
            next_sync_id: 1,
            syncs: std::collections::HashMap::new(),
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
/// It answers the handshake / resource / sync commands in-band, and passes the two protocol-
/// mandated file descriptors (`VCMD_RESOURCE_CREATE_BLOB`, `VCMD_SYNC_WAIT`) over the transport's
/// `send_fd` — the Task 4a change that makes a live client possible at all.
///
/// # Generic over the transport, but not over "does it have fds"
/// `T: VtestTransport` replaces Task 2's `S: Read + Write`. That bound was not merely narrower than
/// needed; it was *wrong* — see the module docs. A transport for this protocol must be able to pass
/// a descriptor, and the trait says so.
///
/// # Inputs / outputs
/// - `stream`: the transport carrying the vtest protocol (bytes **and** descriptors). Taken by
///   `&mut` so the caller retains the socket after the session (e.g. to close it explicitly, or to
///   inspect it in a test).
/// - `engine`: the render engine every Venus command is routed to.
/// - Returns a [`VtestOutcome`] (notably the resource id for readback) on a clean end of session,
///   or an [`EngineError`] on I/O failure, a malformed/unsupported message, an fd-passing failure,
///   or an engine error.
pub fn serve_vtest<T: VtestTransport>(
    stream: &mut T,
    engine: &mut dyn RenderEngine,
) -> Result<VtestOutcome, EngineError> {
    // Per-connection state accumulated across messages.
    let mut session = Session::new();

    // Read and dispatch messages until the peer closes at a message boundary (clean end).
    loop {
        match read_command(stream)? {
            // A decoded command: handle it (may write a reply, send an fd, and/or call the engine).
            Some(cmd) => dispatch(cmd, stream, engine, &mut session)?,
            // Clean EOF: the session is over; hand back what we accumulated.
            None => return Ok(session.outcome),
        }
    }
}

/// Handle one decoded command: write any protocol-mandated reply and route Venus work to `engine`.
///
/// Every command is either answered per the protocol or routed to the engine; none is silently
/// ignored. The two commands whose replies carry a file descriptor (`VCMD_RESOURCE_CREATE_BLOB`,
/// `VCMD_SYNC_WAIT`) send it here for real — in-band reply first, then the fd, the order the client
/// reads in. Where behaviour is still a stub (sync-object timelines) the arm says so explicitly
/// rather than looking complete.
fn dispatch<T: VtestTransport>(
    cmd: VtestCommand,
    stream: &mut T,
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
        VtestCommand::ResourceCreateBlob {
            blob_type,
            flags,
            size,
            blob_id,
        } => {
            // Record what the client asked for before acting on it — see `dump_blob_request`.
            dump_blob_request(blob_type, flags, size, blob_id);

            // Route through the engine's *real* resource path (Task 3):
            // `virgl_renderer_resource_create_blob`, attached to this session's Venus context.
            //
            // Task 4a completes the reply. The client is not merely being told an id — it is being
            // handed **shared memory**, and it blocks in `recvmsg` until the descriptor for that
            // memory arrives. The order below is virglrenderer's own and the client depends on it:
            // in-band `[res_id]` first, then the fd on its own carrier message, then close our copy
            // ("closing the file descriptor does not unmap the region").
            let blob =
                engine.create_blob_resource(VTEST_CONTEXT_ID, blob_type, flags, blob_id, size)?;
            session.outcome.rendered_resource_id = Some(blob.resource_id);

            // The engine must have produced a descriptor; without one the client would hang
            // forever, so refuse loudly rather than write a reply we cannot complete.
            // `VirglEngine` always supplies one — this guards a *different* engine impl's mistake
            // (see `EngineError::BlobFdMissing`).
            let Some(fd) = blob.fd else {
                return Err(EngineError::BlobFdMissing {
                    resource_id: blob.resource_id,
                });
            };

            // (4) the in-band reply.
            write_reply(stream, VCMD_RESOURCE_CREATE_BLOB, &[blob.resource_id])?;
            // (5) the descriptor. Borrowed: the kernel duplicates it into the client.
            stream.send_fd(fd.as_fd())?;
            // (6) drop our copy. The resource — and, for a GUEST blob, the engine's mapping of the
            // same pages — lives on until `VCMD_RESOURCE_UNREF`; only this descriptor goes away.
            drop(fd);
            Ok(())
        }
        VtestCommand::ResourceUnref { res_handle } => {
            // Release the resource for real (Task 3): a no-op on the engine side if `res_handle`
            // was never ours, matching `VCMD_RESOURCE_UNREF`'s fire-and-forget, no-reply wire
            // semantics. No reply is defined for this command.
            engine.unref_resource(res_handle);
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
            // Signal semantics remain stubbed: we treat every waited-on sync as already at its
            // target (advancing our stored value to the requested one).
            for (sync_id, value) in waits {
                session.syncs.insert(sync_id, value);
            }

            // The reply is a *pollable* fd: the client polls it, and its becoming readable is the
            // "your wait is satisfied" signal. virglrenderer sends an `eventfd`, pre-signaled via
            // `write_ready` when every waited-on sync is already at its target (its `is_ready`
            // branch) and signaled later from its event loop otherwise.
            //
            // We send a real, pre-signaled eventfd — not a fabricated descriptor. Because the sync
            // objects above are a bookkeeping stub, every wait *is* by construction already
            // satisfied, so `is_ready` is unconditionally true here and taking that branch is the
            // faithful thing to do. The honest limitation, recorded rather than hidden: a wait that
            // should genuinely block does not, because this server has no real timeline to block
            // on. Modelling real timelines (and signaling the eventfd from GPU fence retirement
            // instead) is future work — see the module docs.
            let fd = crate::transport::create_eventfd(true)?;
            // In-band header first (payload is empty: `resp_buf[VTEST_CMD_LEN] = 0`), then the fd —
            // the same order as the blob reply, and the order the client reads in.
            write_reply(stream, VCMD_SYNC_WAIT, &[])?;
            stream.send_fd(fd.as_fd())?;
            // The client now holds its own duplicate; ours has no further use (the C server closes
            // it here too, via `vtest_free_sync_wait` on the already-ready path).
            drop(fd);
            Ok(())
        }
        VtestCommand::SubmitCmd2 { batches } => {
            // The load-bearing path: replay each batch's Venus command dwords on the GPU. A submit
            // has no reply at all — not an empty one — which a live capture confirms. (The C server
            // does send a descriptor for a batch that sets `VCMD_SUBMIT_CMD2_FLAG_OUT_FENCE_FD`,
            // but `decode_submit_cmd2` refuses such a batch outright rather than half-answer it.)
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
    // `MockEngine`'s signatures need these (Tasks 3 and 4a); no production code in this module
    // constructs or names either type.
    use crate::{BlobResource, EngineFrame};
    use std::io::Cursor;
    // The test transport records the descriptors `serve_vtest` sends, and `MockEngine` hands out
    // real memfds so those recordings are of real, checkable descriptors.
    use crate::transport::create_memfd;
    use std::os::fd::{AsRawFd, OwnedFd};

    /// A [`VtestTransport`] test double: canned request bytes in, replies and **sent descriptors**
    /// recorded out, with no socket, no client and no GPU.
    ///
    /// # Why it records fds rather than ignoring them
    /// The Task 4a bug was precisely "the server wrote a reply and then did not send a descriptor",
    /// which no in-band assertion can detect — the reply bytes are identical either way. A test
    /// double that silently accepted `send_fd` would keep every test here green while a live client
    /// hung. So `send_fd` duplicates what it is given into `sent_fds`, and the tests assert on
    /// **how many** descriptors were sent, **when** relative to the reply bytes, and — via
    /// `MockEngine`'s real memfds — that they name what they should.
    struct RecordingTransport {
        /// The request bytes `serve_vtest` reads.
        input: Cursor<Vec<u8>>,
        /// Every in-band byte `serve_vtest` wrote, in order.
        output: Vec<u8>,
        /// Every descriptor `send_fd` was called with, in order. Duplicated (not borrowed), so
        /// they stay valid and inspectable after the session ends and the originals are dropped —
        /// exactly as the kernel duplicates them into a real client.
        sent_fds: Vec<OwnedFd>,
        /// `output.len()` at the moment of each `send_fd`, so a test can prove the in-band reply
        /// was written **before** the descriptor. Order is part of the wire contract: a client that
        /// receives them the other way round mis-frames the protocol.
        fd_send_offsets: Vec<usize>,
    }

    impl RecordingTransport {
        /// A transport that will feed `input` to the server and record everything it sends back.
        fn new(input: Vec<u8>) -> Self {
            RecordingTransport {
                input: Cursor::new(input),
                output: Vec::new(),
                sent_fds: Vec::new(),
                fd_send_offsets: Vec::new(),
            }
        }
    }

    impl Read for RecordingTransport {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for RecordingTransport {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.output.flush()
        }
    }

    impl VtestTransport for RecordingTransport {
        /// Record the descriptor (duplicated, so it outlives the caller's copy) and where in the
        /// output stream it was sent. `try_clone` is `dup(2)`: the same thing a real `SCM_RIGHTS`
        /// send does to the receiving process, which makes this double faithful rather than merely
        /// convenient.
        fn send_fd(&mut self, fd: std::os::fd::BorrowedFd<'_>) -> Result<(), EngineError> {
            self.fd_send_offsets.push(self.output.len());
            self.sent_fds.push(
                fd.try_clone_to_owned()
                    .map_err(|source| EngineError::FdSendFailed { source })?,
            );
            Ok(())
        }
    }

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
    /// read-loop + dispatch can be exercised with no GPU. Returns a canned Venus capset and
    /// maintains an in-memory resource id counter (Task 3), so `ResourceCreateBlob`/`ResourceUnref`
    /// dispatch can be exercised end-to-end without a real virglrenderer.
    struct MockEngine {
        contexts: Vec<u32>,
        submits: Vec<(u32, Vec<u8>)>,
        capset: Vec<u8>,
        /// `(resource_id, ctx_id)` pairs currently "live", in creation order — mirrors
        /// `VirglEngine::resources` closely enough to prove the dispatch wiring without needing a
        /// real GPU.
        resources: Vec<(u32, u32)>,
        next_resource_id: u32,
        /// The `blob_mem` value of each `create_blob_resource` call, in order — so a test can prove
        /// the client's requested memory *kind* reaches the engine unchanged rather than being
        /// silently normalized somewhere in dispatch.
        blob_mems: Vec<u32>,
    }

    impl MockEngine {
        /// A fresh mock with the given canned capset and nothing recorded yet.
        fn new(capset: Vec<u8>) -> Self {
            MockEngine {
                contexts: Vec::new(),
                submits: Vec::new(),
                capset,
                resources: Vec::new(),
                next_resource_id: 1,
                blob_mems: Vec::new(),
            }
        }
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
        fn create_resource(
            &mut self,
            ctx_id: u32,
            _width: u32,
            _height: u32,
            _format: u32,
        ) -> Result<u32, EngineError> {
            let id = self.next_resource_id;
            self.next_resource_id += 1;
            self.resources.push((id, ctx_id));
            Ok(id)
        }
        /// Hands back a **real** memfd, not a placeholder. `VirglEngine`'s two blob paths both
        /// produce a genuine, mappable descriptor, and the whole point of the dispatch tests is
        /// that such a descriptor reaches the client — so a mock that returned `None`, or a
        /// borrowed stand-in like `/dev/null`, would test a code path that cannot exist in
        /// production. The memfd is sized and content-tagged with the resource id so a test can
        /// prove the descriptor the transport recorded is the one this call created.
        fn create_blob_resource(
            &mut self,
            ctx_id: u32,
            blob_mem: u32,
            _blob_flags: u32,
            _blob_id: u64,
            size: u64,
        ) -> Result<BlobResource, EngineError> {
            let id = self.next_resource_id;
            self.next_resource_id += 1;
            self.resources.push((id, ctx_id));
            self.blob_mems.push(blob_mem);
            // A real anonymous shared-memory object of the requested size, standing in for what
            // virglrenderer would allocate or export.
            let fd = create_memfd(size)?;
            Ok(BlobResource {
                resource_id: id,
                fd: Some(fd),
            })
        }
        fn unref_resource(&mut self, resource_id: u32) {
            self.resources.retain(|&(id, _)| id != resource_id);
        }
        fn read_back(&mut self, resource_id: u32) -> Result<EngineFrame, EngineError> {
            // MockEngine has no real GPU / pixels; these no-GPU framing tests never call
            // `read_back` (the real capability is proven on real hardware in `virgl.rs`'s own GPU
            // tests), so an honest "not found" is the right stand-in rather than fabricating pixels.
            Err(EngineError::VtestProtocol {
                detail: format!("MockEngine has no pixels for resource {resource_id}"),
            })
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

    /// The real bytes a live Mesa Venus client sends, replayed through the decoder verbatim.
    ///
    /// # Why this test exists in this exact form
    /// Task 2 derived the `SUBMIT_CMD2` layout from virglrenderer's C macros and never saw a real
    /// message — its own review flagged that as "do not trust `engine.submit` until real bytes
    /// confirm it", because the client blocks on the blob fd long before it ever submits. Task 4a
    /// unblocked it and captured this payload from a live client (Mesa 26.0.3, Venus over vtest,
    /// `RAYLAND_VTEST_DUMP=1`). Every dword below is transcribed from that capture, not constructed
    /// from the layout the decoder assumes — so agreement here is evidence about reality, not a
    /// tautology. It confirms the source-derived layout was **correct**, including the 8-dword
    /// per-batch stride.
    #[test]
    fn submit_cmd2_decodes_bytes_captured_from_a_live_venus_client() {
        // Verbatim from the capture: a 44-dword payload, one batch, 35 command dwords at offset 9.
        //   [   0] 00000001 00000001 00000009 00000023 0000002c 00000000 00000000 00000000
        //   [   8] 00000000 000000bc 00000000 41faf130 00005793 00000001 00000000 3ba0a600
        //   [  16] 00000001 00000000 3ba0a606 00000000 00000000 002dc6c0 00000000 00000001
        //   [  24] 00000000 00000000 000200c4 00000000 000f4240 00000000 00000000 00000000
        //   [  32] 00000040 00000000 00000080 00000000 000000c0 00000000 00020000 00000000
        //   [  40] 000200c0 00000000 00000004 00000000
        let captured: [u32; 44] = [
            0x00000001, 0x00000001, 0x00000009, 0x00000023, 0x0000002c, 0x00000000, 0x00000000,
            0x00000000, 0x00000000, 0x000000bc, 0x00000000, 0x41faf130, 0x00005793, 0x00000001,
            0x00000000, 0x3ba0a600, 0x00000001, 0x00000000, 0x3ba0a606, 0x00000000, 0x00000000,
            0x002dc6c0, 0x00000000, 0x00000001, 0x00000000, 0x00000000, 0x000200c4, 0x00000000,
            0x000f4240, 0x00000000, 0x00000000, 0x00000000, 0x00000040, 0x00000000, 0x00000080,
            0x00000000, 0x000000c0, 0x00000000, 0x00020000, 0x00000000, 0x000200c0, 0x00000000,
            0x00000004, 0x00000000,
        ];

        let cmd = read_command(&mut Cursor::new(msg(VCMD_SUBMIT_CMD2, &captured)))
            .expect("real client bytes must decode")
            .expect("a command");
        let VtestCommand::SubmitCmd2 { batches } = cmd else {
            panic!("expected SubmitCmd2, got {cmd:?}");
        };

        // One batch, exactly as `[0] = 0x00000001` says.
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        // flags = 0x1 = RING_IDX only: the client sets no fence-fd flags (see `decode_submit_cmd2`).
        assert_eq!(batch.flags, VCMD_SUBMIT_CMD2_FLAG_RING_IDX);
        // ring_idx = 0. This is the finding the fence path depends on: `read_back` hardcodes ring 0,
        // and a live client using a nonzero ring would make that silently wrong. It does not.
        assert_eq!(batch.ring_idx, 0);
        // sync_count = 0: this batch signals no timeline syncs.
        assert!(batch.syncs.is_empty());
        // cmd_offset = 9 and cmd_size = 0x23 = 35. Offset 9 is `1 + 8*1` — the 8-dword-per-batch
        // stride, confirmed against real bytes rather than assumed from the C macros.
        assert_eq!(batch.cmd.len(), 35);
        // 9 + 35 = 44 = the whole payload: the command stream runs to the end, so a stride of
        // anything other than 8 would have produced a different (and out-of-range) slice.
        assert_eq!(batch.cmd, captured[9..44]);
        // The first command dword the GPU actually replayed, spot-checked against the hex dump.
        assert_eq!(batch.cmd[0], 0x000000bc);
    }

    /// A batch asking for syncobj passing must be **rejected**, not ignored. virglrenderer's server
    /// reads extra bytes off the socket for a nonzero syncobj count, so a decoder that skipped the
    /// field would leave those bytes in the stream and mis-frame every message after it — a
    /// corruption that surfaces nowhere near its cause. (No Venus client sends this today; the test
    /// pins the behaviour for the one that eventually does.)
    #[test]
    fn submit_cmd2_rejects_syncobj_passing_it_cannot_frame() {
        // A well-formed batch except for num_in_syncobj = 1 (dword +6 of the batch header).
        let payload = [1u32, 0x1, 9, 1, 10, 0, 0, 1, 0, 0xAAAA];
        let err = read_command(&mut Cursor::new(msg(VCMD_SUBMIT_CMD2, &payload))).unwrap_err();
        assert!(
            matches!(err, EngineError::VtestProtocol { .. }),
            "got {err:?}"
        );
    }

    /// A batch setting a fence-fd flag must likewise be rejected: those flags make the C server
    /// receive or send a descriptor out of band, which this server does not do — and a client left
    /// waiting on an out-fence descriptor that never arrives is exactly the hang Task 4a exists to
    /// eliminate, not one to reintroduce.
    #[test]
    fn submit_cmd2_rejects_fence_fd_flags_it_does_not_implement() {
        // flags = 0x4 = VCMD_SUBMIT_CMD2_FLAG_OUT_FENCE_FD, which we do not implement.
        let payload = [1u32, 0x4, 9, 1, 10, 0, 0, 0, 0, 0xAAAA];
        let err = read_command(&mut Cursor::new(msg(VCMD_SUBMIT_CMD2, &payload))).unwrap_err();
        assert!(
            matches!(err, EngineError::VtestProtocol { .. }),
            "got {err:?}"
        );
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

        let mut engine = MockEngine::new(capset.clone());
        let mut duplex = RecordingTransport::new(input);
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

        // This session creates no blob and performs no sync-wait, so no descriptor should have been
        // sent. Asserted because `send_fd` on the wrong command would inject a stray carrier byte
        // into the client's byte stream and mis-frame everything after it.
        assert!(
            duplex.sent_fds.is_empty(),
            "a handshake+submit session must send no file descriptors"
        );

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

    /// `VCMD_RESOURCE_CREATE_BLOB` and `VCMD_RESOURCE_UNREF` must route through the engine's real
    /// resource path — not vtest-local bookkeeping — `VtestOutcome` must carry the engine-assigned
    /// id back out (Task 3), **and the reply must deliver the client's descriptor** (Task 4a).
    ///
    /// The last part is the one that matters most: everything up to it was already green while a
    /// live client hung forever, because an in-band reply looks identical whether or not a
    /// descriptor follows it. So this asserts the descriptor was sent, that it was sent *after* the
    /// in-band reply, and that it is a real, `stat`able object of the size the client asked for.
    #[test]
    fn resource_create_blob_replies_with_the_id_and_the_client_fd() {
        // Payload per the pinned wire layout: `[type][flags][size_lo][size_hi][id_lo][id_hi]`.
        // type=2 (VIRGL_RENDERER_BLOB_MEM_HOST3D — what a live Mesa Venus client actually asks
        // for), flags=1 (USE_MAPPABLE), size=4096, blob_id=0 (Venus's shmem blob id).
        const BLOB_SIZE: u32 = 4096;
        let mut input = Vec::new();
        input.extend_from_slice(&msg(VCMD_RESOURCE_CREATE_BLOB, &[2, 1, BLOB_SIZE, 0, 0, 0]));
        input.extend_from_slice(&msg(VCMD_RESOURCE_UNREF, &[1]));

        let mut engine = MockEngine::new(Vec::new());
        let mut duplex = RecordingTransport::new(input);
        let outcome = serve_vtest(&mut duplex, &mut engine).expect("session completes cleanly");

        // The engine-assigned id (not a vtest-local counter) flows into the outcome.
        assert_eq!(
            outcome.rendered_resource_id,
            Some(1),
            "VtestOutcome must carry the id the engine itself assigned"
        );
        // The client's requested memory *kind* reached the engine unchanged — the engine dispatches
        // on it to decide where the shared pages come from, so a normalized value would silently
        // pick the wrong allocation strategy.
        assert_eq!(
            engine.blob_mems,
            vec![2],
            "the client's blob_mem must reach the engine verbatim"
        );

        // The in-band reply is `[res_id]` under VCMD_RESOURCE_CREATE_BLOB.
        let mut rcur = Cursor::new(duplex.output.clone());
        let reply = read_reply(&mut rcur);
        assert_eq!(reply, (VCMD_RESOURCE_CREATE_BLOB, vec![1]));

        // Exactly one descriptor was sent: the blob's. (Not zero — the bug this task fixes; and not
        // two — a stray extra carrier byte would mis-frame the client's stream.)
        assert_eq!(
            duplex.sent_fds.len(),
            1,
            "the blob reply must deliver exactly one file descriptor"
        );
        // It was sent *after* all 12 bytes of the in-band reply (8-byte header + 1 dword), which is
        // the order virglrenderer uses and Mesa's client reads in.
        assert_eq!(
            duplex.fd_send_offsets,
            vec![12],
            "the descriptor must follow the complete in-band reply, never precede or interleave it"
        );

        // And it is a real object of the size the client asked for — not a placeholder that
        // happened to satisfy the type checker. A live client `mmap`s exactly this many bytes.
        let sent = std::fs::File::from(duplex.sent_fds.pop().expect("the blob fd"));
        assert_eq!(
            sent.metadata().expect("stat the sent fd").len(),
            BLOB_SIZE as u64,
            "the descriptor sent to the client must name the memory it asked for"
        );

        // The engine actually released the resource: proves RESOURCE_UNREF reached the engine, not
        // just a vtest-side stub that would leave the engine's own tracking untouched.
        assert!(
            engine.resources.is_empty(),
            "unref must reach the engine's real resource tracking"
        );
    }

    /// `VCMD_SYNC_WAIT`'s reply must be an empty in-band header **followed by a pollable
    /// descriptor** — the client polls that descriptor and treats its readability as "your wait is
    /// satisfied". Task 3 sent only the header, which leaves a live client waiting on a descriptor
    /// that never arrives.
    ///
    /// This also pins the honest limitation stated in the module docs: because this server's sync
    /// objects are a bookkeeping stub, every wait is already satisfied, so the eventfd is sent
    /// pre-signaled (virglrenderer's own `is_ready` branch). The test asserts that pre-signaling,
    /// so the day real timeline semantics arrive, this test is what notices.
    #[test]
    fn sync_wait_replies_with_a_header_and_a_ready_pollable_fd() {
        // `[flags][timeout]` then one `(sync_id, value_lo, value_hi)` triple: wait for sync 1 to
        // reach 5, with an infinite timeout.
        let input = msg(VCMD_SYNC_WAIT, &[0, u32::MAX, 1, 5, 0]);

        let mut engine = MockEngine::new(Vec::new());
        let mut duplex = RecordingTransport::new(input);
        serve_vtest(&mut duplex, &mut engine).expect("session completes cleanly");

        // The in-band half: an empty payload under VCMD_SYNC_WAIT (`resp_buf[VTEST_CMD_LEN] = 0`).
        let mut rcur = Cursor::new(duplex.output.clone());
        assert_eq!(read_reply(&mut rcur), (VCMD_SYNC_WAIT, vec![]));

        // The fd half, sent after the complete 8-byte header.
        assert_eq!(
            duplex.sent_fds.len(),
            1,
            "the sync-wait reply must deliver exactly one file descriptor"
        );
        assert_eq!(
            duplex.fd_send_offsets,
            vec![8],
            "the descriptor must follow the complete in-band header"
        );

        // The descriptor must be *readable right now*: that readability is the entire signal. A
        // descriptor that was created but never signaled would leave a live client blocked, which
        // is indistinguishable from the bug this task fixes.
        let fd = &duplex.sent_fds[0];
        let mut buf = [0u8; 8];
        // SAFETY: `fd` is the live eventfd the dispatcher sent; `buf` is the 8 bytes an eventfd
        // read requires. It is EFD_NONBLOCK, so an unsignaled fd would return EAGAIN, not hang.
        let n = unsafe {
            libc::read(
                fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut std::ffi::c_void,
                buf.len(),
            )
        };
        assert_eq!(
            n,
            8,
            "the sync-wait eventfd must be pre-signaled (the stub treats every wait as satisfied): {}",
            std::io::Error::last_os_error()
        );
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
