//! What S does with every message C sends it — tested against **real shared memory**, with no GPU,
//! no Mesa and no network.
//!
//! # Why these tests can be this thorough without a GPU
//! S's job splits cleanly in two, and only one half needs a GPU. The half that does is
//! `RenderEngine` (C0 built it, `rayland-engine` tests it against a real Intel GPU). The half this
//! file covers is everything *around* it: which messages reach the engine, which deliberately do
//! not, and — the load-bearing part — how a ring delta is written into S's ring memory. That half is
//! pointer arithmetic over an `mmap`, and a memfd is an honest stand-in for a virglrenderer-exported
//! blob because **it is the same kind of object**: ring-findings §2.1 records that
//! `virgl_renderer_resource_export_blob` returns `fd_type = 3 = VIRGL_RENDERER_BLOB_FD_TYPE_SHM` —
//! plain shared memory. So these tests map the very same way S does in production.
//!
//! # The one thing these tests cannot see
//! Nothing here runs virglrenderer's ring thread, so nothing here proves S's bytes are *executed*.
//! What they prove is that the bytes land where that thread reads, in the order it expects. The
//! execution half is (c)1 Task 6's loopback end-to-end test, which is the first time the two halves
//! meet. That distinction is stated rather than blurred: a test that claimed more than it checks is
//! how this branch has been bitten before.

// The unit under test.
use rayland_s::apply::{Applier, ApplyError};
use rayland_s::ring_mirror::RingDeltaError;
// The relay protocol S speaks.
use rayland_relay::{C2S, S2C};
// The ring layout S must honour, and the shared-memory primitives S maps blobs with. These come
// from `rayland-vtest` — the repository's single copy of its ring knowledge — rather than being
// restated here, so a test cannot drift from the code it checks.
use rayland_vtest::transport::{ShmMapping, create_memfd};
use rayland_vtest::venus_ring::{
    RING_BUFFER_OFFSET, RING_HEAD_OFFSET, RING_SHMEM_SIZE, RING_TAIL_OFFSET,
};
use rayland_vtest::{BlobResource, EngineError, EngineFrame, RenderEngine};

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};

/// The context id used throughout, mirroring the live capture's single-context session.
const CTX_ID: u32 = 1;

/// `VIRGL_RENDERER_BLOB_MEM_HOST3D`. Ring-findings §2.1: Mesa **hardcodes** this on the vtest
/// backend (`vn_renderer_vtest.c:1055`), so it is what every real blob request carries.
const BLOB_MEM_HOST3D: u32 = 2;

/// The observed ring's command-buffer size: 128 KiB (ring-findings §4).
const RING_BUFFER_SIZE: u32 = 131_072;

// ---------------------------------------------------------------------------------------------
// The test double
// ---------------------------------------------------------------------------------------------

/// A [`RenderEngine`] that records what reached it and hands out **real** memfds.
///
/// # Why this is written here rather than reused
/// This task's brief said to reuse the `RenderEngine` double in `rayland-vtest`'s `vtest.rs`. That
/// double (`MockEngine`) lives inside a `#[cfg(test)] mod tests`, so it is not reachable from
/// another crate and cannot be reused without first promoting it to a shared test-support target.
/// Two doubles serving different crates is the cheaper answer, and this one has a requirement
/// `MockEngine` does not: its blob descriptors must be **inspectable by the test**, because the
/// whole point here is asserting what S wrote into a blob's pages.
///
/// # Why the memfds are real
/// The same reason `MockEngine`'s are: S maps the descriptor this returns and writes a Venus command
/// stream into it. A double that returned `None`, or a borrowed stand-in, would exercise a code path
/// that cannot exist in production — and would make the central assertion of this file untestable,
/// since there would be no memory to read back.
#[derive(Default)]
struct RecordingEngine {
    /// Contexts created, in order.
    contexts: Vec<u32>,
    /// Every `(ctx_id, cmd)` that reached [`RenderEngine::submit`] — the **inline** command path.
    /// Ring deltas must never appear here; that is what `ring_delta_never_reaches_submit` pins.
    submits: Vec<(u32, Vec<u8>)>,
    /// The canned Venus capset. C has no GPU and cannot invent one, so S must answer from here.
    capset: Vec<u8>,
    /// A duplicate descriptor for every blob handed out, keyed by resource id, so a test can map
    /// the *same pages S mapped* and read back what S wrote.
    blob_fds: HashMap<u32, OwnedFd>,
    /// Blob sizes, so a test can map them without restating the number.
    blob_sizes: HashMap<u32, u64>,
    /// Resource ids released via `unref_resource`, in order.
    unreffed: Vec<u32>,
    /// The next resource id to assign. Starts at 1, matching `VirglEngine`.
    next_resource_id: u32,
    /// When set, `create_blob_resource` fails with this instead of allocating — so a test can prove
    /// an engine failure is *reported*, not swallowed.
    fail_blob_with: Option<EngineError>,
}

impl RecordingEngine {
    /// A fresh double with a canned capset and ids starting at 1.
    fn new() -> Self {
        RecordingEngine {
            capset: vec![1, 2, 3, 4],
            next_resource_id: 1,
            ..Default::default()
        }
    }

    /// Read a blob's pages back, exactly as another mapper of the same memory would see them.
    ///
    /// This is what makes the central assertions possible: it maps the identical shared-memory
    /// object S mapped, so what it reads *is* what virglrenderer's ring thread would read.
    fn read_blob(&self, res_id: u32) -> Vec<u8> {
        let fd = self
            .blob_fds
            .get(&res_id)
            .expect("a blob this double created");
        let size = self.blob_sizes[&res_id];
        let mapping = ShmMapping::map(fd.as_fd(), size).expect("mapping the double's memfd");
        let len = size as usize;
        // SAFETY: `mapping` is a live MAP_SHARED mapping of exactly `len` bytes, and it outlives
        // this copy. The pages may be written concurrently in production; in this test nothing else
        // touches them while this runs.
        unsafe { std::slice::from_raw_parts(mapping.as_ptr().cast::<u8>(), len) }.to_vec()
    }

    /// Write a 32-bit control word into a blob's pages, standing in for virglrenderer's ring thread.
    ///
    /// The ring thread is the only thing that ever writes `head` (`vkr_ring_store_head`,
    /// `vkr_ring.c:60-67`). Tests that need S to observe a consumed ring use this to play its part.
    fn write_control(&self, res_id: u32, offset: usize, value: u32) {
        let fd = self
            .blob_fds
            .get(&res_id)
            .expect("a blob this double created");
        let size = self.blob_sizes[&res_id];
        let mapping = ShmMapping::map(fd.as_fd(), size).expect("mapping the double's memfd");
        // SAFETY: `offset + 4 <= size` for every control word of a ring-sized blob, and the mapping
        // is live and writable for the duration of this write.
        unsafe {
            std::ptr::copy_nonoverlapping(
                value.to_le_bytes().as_ptr(),
                mapping.as_ptr().cast::<u8>().add(offset),
                4,
            );
        }
    }
}

impl RenderEngine for RecordingEngine {
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
        _ctx_id: u32,
        _width: u32,
        _height: u32,
        _format: u32,
    ) -> Result<u32, EngineError> {
        // Unreachable through the relay protocol: `C2S` has no classic-resource message, because
        // Mesa's Venus ICD allocates everything as blobs. Panicking rather than returning a plausible
        // id is deliberate — if a future `apply` arm ever reached this, a stub answer would let the
        // test suite stay green over a code path that cannot exist in production.
        unreachable!("the relay protocol never asks S to create a classic 2D resource")
    }

    fn create_blob_resource(
        &mut self,
        _ctx_id: u32,
        _blob_mem: u32,
        _blob_flags: u32,
        _blob_id: u64,
        size: u64,
    ) -> Result<BlobResource, EngineError> {
        if let Some(err) = self.fail_blob_with.take() {
            return Err(err);
        }
        let res_id = self.next_resource_id;
        self.next_resource_id += 1;
        // A real anonymous shared-memory object, standing in for what virglrenderer's HOST3D path
        // allocates and exports as an SHM descriptor.
        let fd = create_memfd(size)?;
        // Keep a duplicate so the test can map the same pages S is about to map. `try_clone`
        // duplicates the descriptor; both refer to one memory object.
        let ours = fd.try_clone().expect("duplicating the blob descriptor");
        self.blob_fds.insert(res_id, ours);
        self.blob_sizes.insert(res_id, size);
        Ok(BlobResource {
            resource_id: res_id,
            fd: Some(fd),
        })
    }

    fn unref_resource(&mut self, resource_id: u32) {
        self.unreffed.push(resource_id);
        self.blob_fds.remove(&resource_id);
        self.blob_sizes.remove(&resource_id);
    }

    fn read_back(&mut self, _resource_id: u32) -> Result<EngineFrame, EngineError> {
        // Unreachable through the relay protocol; (c)1 Task 7 presents from a blob, not from here.
        unreachable!("the relay protocol never asks S to read back a classic resource")
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Bring a session up to the point where a ring exists: a context, then the ring blob.
///
/// Returns the applier, the engine double, and the ring's resource id.
fn session_with_ring() -> (Applier, RecordingEngine, u32) {
    let mut engine = RecordingEngine::new();
    let mut applier = Applier::new();

    let out = applier.apply(&mut engine, C2S::CreateContext { ctx_id: CTX_ID });
    assert!(out.is_empty(), "CONTEXT_INIT has no reply on the wire");

    // The ring blob, at the exact size the live capture observed (ring-findings §4).
    let out = applier.apply(
        &mut engine,
        C2S::CreateBlob {
            blob_mem: BLOB_MEM_HOST3D,
            blob_flags: 0,
            blob_id: 0,
            size: RING_SHMEM_SIZE as u64,
        },
    );
    let res_id = match out.as_slice() {
        [S2C::BlobCreated { res_id }] => *res_id,
        other => panic!("expected exactly one BlobCreated, got {other:?}"),
    };

    (applier, engine, res_id)
}

/// Read a 32-bit little-endian control word out of a blob snapshot.
fn control(blob: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(blob[offset..offset + 4].try_into().expect("a control word"))
}

/// The sole `S2C::Error` in `out`, or a panic naming what was actually produced.
fn sole_error(out: &[S2C]) -> &str {
    match out {
        [S2C::Error { message }] => message,
        other => panic!("expected exactly one S2C::Error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// The central pair: the ring path and the inline path are different channels
// ---------------------------------------------------------------------------------------------

/// **The load-bearing test of this task.** A ring delta must be written into S's ring *memory* —
/// the pages virglrenderer's ring thread polls — and must **never** be handed to
/// `RenderEngine::submit`.
///
/// # Why submitting it would be wrong, from source
/// The two paths are consumed by *different decoder instances* (ring-findings §3.1):
/// - the **ring** path is `vkr_ring.c:220-223`, decoding into the ring's own private decoder. It is
///   fed by `vkr_ring_thread` (`vkr_ring.c:262-266`), which polls `vkr_ring_load_tail(ring)` and
///   reads out of `ring->buffer.data`. Both of those point straight into the *blob resource's*
///   memory: `vkr_ring_init_control`/`vkr_ring_init_buffer` (`vkr_ring.c:33-58`) set them with
///   `get_resource_pointer(layout->resource, ...)`.
/// - the **inline** path is `vkr_context.c:170-173`, decoding into the *context's* decoder. That is
///   what `virgl_renderer_submit_cmd` — i.e. `RenderEngine::submit` — reaches.
///
/// So `submit`ting ring bytes would splice the application's command stream into a byte stream it
/// was never part of, decoded by the wrong decoder, while the ring thread went on polling memory
/// that never changed. Nothing the application draws would execute.
#[test]
fn a_ring_delta_is_written_into_s_ring_memory_and_never_submitted() {
    let (mut applier, mut engine, ring) = session_with_ring();
    // Four dwords of Venus command language, standing in for whatever Mesa wrote.
    let bytes = vec![0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04];

    let out = applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: ring,
            tail: bytes.len() as u32,
            bytes: bytes.clone(),
        },
    );

    assert!(
        engine.submits.is_empty(),
        "ring bytes must not reach the engine's inline submit path (vkr_context.c:170); the ring \
         thread reads them out of the blob's own memory (vkr_ring.c:262). Got {:?}",
        engine.submits
    );

    let blob = engine.read_blob(ring);
    assert_eq!(
        &blob[RING_BUFFER_OFFSET..RING_BUFFER_OFFSET + bytes.len()],
        &bytes[..],
        "the delta must land verbatim at the ring buffer's base — a single dword of drift \
         corrupts every subsequent command"
    );
    assert_eq!(
        control(&blob, RING_TAIL_OFFSET),
        bytes.len() as u32,
        "`tail` is the ring thread's only signal that there is work: vkr_ring_thread computes \
         `vkr_ring_load_tail(ring) - ring->buffer.cur` and does nothing at all while it is zero"
    );
    assert!(
        out.is_empty(),
        "the ring thread has not consumed anything yet, so there is no true progress to report; \
         got {out:?}"
    );
}

/// The inline path is the mirror image: `C2S::SubmitCmd` **must** reach `RenderEngine::submit`
/// verbatim, because it is the channel that carries `vkCreateRingMESA` — the command that makes S
/// create the ring at all (ring-findings §3.2, caught in a live `SUBMIT_CMD2` capture).
///
/// Payload is the real one: `0xbc` = opcode 188 = `vkCreateRingMESA`.
#[test]
fn submit_cmd_reaches_the_engine_verbatim_because_it_creates_the_ring() {
    let mut engine = RecordingEngine::new();
    let mut applier = Applier::new();
    applier.apply(&mut engine, C2S::CreateContext { ctx_id: CTX_ID });

    let out = applier.apply(
        &mut engine,
        C2S::SubmitCmd {
            ctx_id: CTX_ID,
            cmd: vec![0xbc, 0, 0, 0],
        },
    );

    assert_eq!(
        engine.contexts,
        vec![CTX_ID],
        "the context must reach the engine: every resource is attached to one, and S has nothing to \
         submit into without it"
    );
    assert_eq!(
        engine.submits,
        vec![(CTX_ID, vec![0xbc, 0, 0, 0])],
        "inline vtest commands are the context decoder's (vkr_context.c:170); without this the \
         ring is never created and nothing the application draws ever runs"
    );
    assert!(out.is_empty(), "SUBMIT_CMD2 has no reply on the wire");
}

// ---------------------------------------------------------------------------------------------
// `head`: the reply-ready signal, reported only when it is genuinely true
// ---------------------------------------------------------------------------------------------

/// S reports progress **only** from the `head` its engine actually wrote — never from the tail it
/// was handed.
///
/// # Why this is the difference between working and a corrupt frame
/// `head` is not a space counter. Mesa polls it as the **reply-ready signal**:
/// `vn_ring_get_seqno_status` is `vn_ring_ge_seqno(ring, vn_ring_load_head(ring), seqno)`
/// (`vn_ring.c:176-179`), and `vn_ring_wait_seqno` spins on it. C advances its local `head` *only*
/// from `S2C::RingProgress`, so whatever S reports here is what releases the application's
/// synchronous waits. Report a tail S has not actually executed and the application resumes and
/// decodes a reply arena that was never written.
#[test]
fn progress_reports_the_head_the_engine_wrote_not_the_tail_that_was_relayed() {
    let (mut applier, mut engine, ring) = session_with_ring();
    let bytes = vec![0u8; 64];
    applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: ring,
            tail: 64,
            bytes,
        },
    );

    // The ring thread consumes half of it and stores `head` (vkr_ring.c:230-233 advances head
    // intra-cs, after each command, so a partial head is entirely normal).
    engine.write_control(ring, RING_HEAD_OFFSET, 32);

    let out = applier.poll_progress();

    assert!(
        matches!(
            out.as_slice(),
            [S2C::RingProgress {
                ring_res_id,
                consumed_tail: 32
            }] if *ring_res_id == ring
        ),
        "S must report the 32 bytes its engine genuinely retired, not the 64 it was handed; got \
         {out:?}"
    );
}

/// Before the engine has consumed anything, S owes C **no** progress. Reporting the relayed tail
/// here is the exact "release the wait before the reply exists" bug the message set's docs warn of.
#[test]
fn no_progress_is_reported_while_the_engine_has_consumed_nothing() {
    let (mut applier, mut engine, ring) = session_with_ring();
    applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: ring,
            tail: 8,
            bytes: vec![1, 2, 3, 4, 5, 6, 7, 8],
        },
    );

    // `head` is untouched: the ring thread has not run.
    assert!(
        applier.poll_progress().is_empty(),
        "an unconsumed ring has no progress to report; reporting one would release the \
         application's wait on a reply that does not exist"
    );
}

/// Progress is reported on **movement**, never on repetition.
///
/// A `RingProgress` resent while `head` stands still is exactly the keepalive-while-wedged pattern
/// ring-findings §5.4 names: it would prove S's process is scheduled and nothing about the ring. C's
/// stall detector already refuses to count it as progress; S must not manufacture it either.
#[test]
fn progress_is_reported_once_per_movement_not_on_every_poll() {
    let (mut applier, mut engine, ring) = session_with_ring();
    applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: ring,
            tail: 16,
            bytes: vec![0u8; 16],
        },
    );
    engine.write_control(ring, RING_HEAD_OFFSET, 16);

    assert_eq!(
        applier.poll_progress().len(),
        1,
        "the first poll sees the move"
    );
    assert!(
        applier.poll_progress().is_empty(),
        "`head` has not moved since, so there is nothing new to say; a repeat would be a \
         keepalive that proves only that S's process is running"
    );
}

// ---------------------------------------------------------------------------------------------
// Remote input is attacker-controlled: every bound from the wire is refused, never trusted
// ---------------------------------------------------------------------------------------------

/// A delta whose byte count disagrees with the `tail` it claims is refused.
///
/// Both fields come off the network. Trusting `tail` and writing `bytes` would desynchronize S's
/// ring frontier from C's by exactly the difference, and every later delta would then be written at
/// the wrong offset — a silently corrupt command stream rather than an error.
#[test]
fn a_ring_delta_whose_length_contradicts_its_tail_is_refused() {
    let (mut applier, mut engine, ring) = session_with_ring();

    let out = applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: ring,
            // Claims 64 bytes of progress while carrying 4.
            tail: 64,
            bytes: vec![1, 2, 3, 4],
        },
    );

    assert!(
        sole_error(&out).contains("64"),
        "the refusal must name the contradiction; got {out:?}"
    );
    let blob = engine.read_blob(ring);
    assert_eq!(
        control(&blob, RING_TAIL_OFFSET),
        0,
        "a refused delta must not publish a tail: doing so would hand the ring thread a frontier \
         over bytes that were never written"
    );
}

/// A delta larger than the ring's whole command buffer is refused rather than written.
///
/// Mesa cannot produce one — its producer refuses to write past `head + buffer_size`
/// (`vn_ring_has_space`, `vn_ring.c:213`) — so this can only be a broken or hostile C. Writing it
/// would run off the end of the buffer region and into the `extra` word and beyond.
#[test]
fn a_ring_delta_larger_than_the_ring_buffer_is_refused() {
    let (mut applier, mut engine, ring) = session_with_ring();
    let oversized = RING_BUFFER_SIZE as usize + 4;

    let out = applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: ring,
            tail: oversized as u32,
            bytes: vec![0u8; oversized],
        },
    );

    assert!(
        sole_error(&out).contains("131072"),
        "the refusal must name the buffer it would have overrun; got {out:?}"
    );
}

/// A delta naming a resource that is not a ring is refused, not panicked on.
///
/// S reads everything off a network. An unknown or non-ring `ring_res_id` must produce a message a
/// human can act on — indexing a table with it would take the daemon down on a remote peer's say-so.
#[test]
fn a_ring_delta_for_a_resource_that_is_not_a_ring_is_refused() {
    let mut engine = RecordingEngine::new();
    let mut applier = Applier::new();
    applier.apply(&mut engine, C2S::CreateContext { ctx_id: CTX_ID });
    // The app's 64-byte vertex buffer from the live capture: a real blob, but not a ring.
    let out = applier.apply(
        &mut engine,
        C2S::CreateBlob {
            blob_mem: BLOB_MEM_HOST3D,
            blob_flags: 0,
            blob_id: 16,
            size: 64,
        },
    );
    let vertex_buffer = match out.as_slice() {
        [S2C::BlobCreated { res_id }] => *res_id,
        other => panic!("expected BlobCreated, got {other:?}"),
    };

    let out = applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: vertex_buffer,
            tail: 4,
            bytes: vec![1, 2, 3, 4],
        },
    );

    assert!(
        sole_error(&out).contains(&vertex_buffer.to_string()),
        "the refusal must name the resource; got {out:?}"
    );
}

/// Blob data that would write past the end of a blob is refused.
///
/// `offset` and `bytes.len()` are both remote. This is the same standard `rayland-c`'s
/// `apply_blob_data` already holds: an unchecked write here is a mapping overflow driven by a remote
/// peer.
#[test]
fn blob_data_past_the_end_of_the_blob_is_refused() {
    let mut engine = RecordingEngine::new();
    let mut applier = Applier::new();
    applier.apply(&mut engine, C2S::CreateContext { ctx_id: CTX_ID });
    let out = applier.apply(
        &mut engine,
        C2S::CreateBlob {
            blob_mem: BLOB_MEM_HOST3D,
            blob_flags: 0,
            blob_id: 16,
            size: 64,
        },
    );
    let res_id = match out.as_slice() {
        [S2C::BlobCreated { res_id }] => *res_id,
        other => panic!("expected BlobCreated, got {other:?}"),
    };

    let out = applier.apply(
        &mut engine,
        C2S::BlobData {
            res_id,
            offset: 60,
            bytes: vec![0xff; 8],
        },
    );

    assert!(
        sole_error(&out).contains("64"),
        "the refusal must name the blob's real size; got {out:?}"
    );
}

/// An `offset` chosen to overflow the address arithmetic is refused rather than wrapping into a
/// valid-looking range.
#[test]
fn blob_data_whose_offset_overflows_is_refused() {
    let mut engine = RecordingEngine::new();
    let mut applier = Applier::new();
    applier.apply(&mut engine, C2S::CreateContext { ctx_id: CTX_ID });
    let out = applier.apply(
        &mut engine,
        C2S::CreateBlob {
            blob_mem: BLOB_MEM_HOST3D,
            blob_flags: 0,
            blob_id: 16,
            size: 64,
        },
    );
    let res_id = match out.as_slice() {
        [S2C::BlobCreated { res_id }] => *res_id,
        other => panic!("expected BlobCreated, got {other:?}"),
    };

    let out = applier.apply(
        &mut engine,
        C2S::BlobData {
            res_id,
            offset: u64::MAX,
            bytes: vec![0xff; 8],
        },
    );

    assert!(
        !out.is_empty(),
        "an offset that overflows must be refused, not silently wrapped into range"
    );
}

// ---------------------------------------------------------------------------------------------
// The ring is circular: the wrap must be reassembled the way Mesa's producer laid it out
// ---------------------------------------------------------------------------------------------

/// A delta that straddles the buffer's physical end is written in two runs, exactly as Mesa's own
/// producer would have written it.
///
/// # Why this needs its own test
/// `C2S::RingDelta::bytes` arrives **already un-wrapped** — `rayland-c`'s `RingWatcher::take_delta`
/// joins the two halves in producer order so the wire carries one contiguous run. S therefore has to
/// *re*-wrap it, because virglrenderer's consumer masks its cursor (`buf->cur & buf->mask`,
/// `vkr_ring_read_buffer`, `vkr_ring.c:83-99`) and will read the second half from the buffer's
/// **start**. Writing the delta linearly would run off the end of the buffer and leave the wrapped
/// half unwritten. Ring-findings §8 records that no wrap has ever been reached in a live run, so
/// this arithmetic has never been exercised against real Mesa — which is exactly why it is pinned
/// here rather than assumed.
#[test]
fn a_ring_delta_that_wraps_the_buffer_is_re_wrapped_into_two_runs() {
    let (mut applier, mut engine, ring) = session_with_ring();

    // Park the frontier 4 bytes short of the buffer's end, then write 8 bytes across it.
    let first_tail = RING_BUFFER_SIZE - 4;
    applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: ring,
            tail: first_tail,
            bytes: vec![0u8; first_tail as usize],
        },
    );

    let straddling = vec![0xa1, 0xa2, 0xa3, 0xa4, 0xb1, 0xb2, 0xb3, 0xb4];
    let out = applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: ring,
            tail: first_tail + 8,
            bytes: straddling,
        },
    );
    assert!(out.is_empty(), "no progress yet; got {out:?}");

    let blob = engine.read_blob(ring);
    let buf_end = RING_BUFFER_OFFSET + RING_BUFFER_SIZE as usize;
    assert_eq!(
        &blob[buf_end - 4..buf_end],
        &[0xa1, 0xa2, 0xa3, 0xa4],
        "the first half belongs at the buffer's physical end"
    );
    assert_eq!(
        &blob[RING_BUFFER_OFFSET..RING_BUFFER_OFFSET + 4],
        &[0xb1, 0xb2, 0xb3, 0xb4],
        "the second half must continue from the buffer's start — that is where \
         vkr_ring_read_buffer's masked cursor will look for it"
    );
    assert_eq!(
        control(&blob, RING_TAIL_OFFSET),
        first_tail + 8,
        "the free-running counter keeps counting past the buffer's end; it is masked only at \
         access time"
    );
}

// ---------------------------------------------------------------------------------------------
// The rest of the message set
// ---------------------------------------------------------------------------------------------

/// The capset must come from S's real engine: C has no GPU and Mesa refuses to initialize without
/// a valid one.
#[test]
fn the_capset_is_answered_from_the_engine() {
    let mut engine = RecordingEngine::new();
    let mut applier = Applier::new();

    let out = applier.apply(&mut engine, C2S::GetCapset { version: 0 });

    assert!(
        matches!(out.as_slice(), [S2C::Capset { bytes }] if bytes == &[1, 2, 3, 4]),
        "got {out:?}"
    );
}

/// Blob data lands in the blob's actual pages — the mechanism by which the application's vertex
/// buffer (ring-findings §6, `res=3`) ever reaches S's GPU at all.
#[test]
fn blob_data_is_written_into_s_blob_pages() {
    let mut engine = RecordingEngine::new();
    let mut applier = Applier::new();
    applier.apply(&mut engine, C2S::CreateContext { ctx_id: CTX_ID });
    let out = applier.apply(
        &mut engine,
        C2S::CreateBlob {
            blob_mem: BLOB_MEM_HOST3D,
            blob_flags: 0,
            blob_id: 16,
            size: 64,
        },
    );
    let res_id = match out.as_slice() {
        [S2C::BlobCreated { res_id }] => *res_id,
        other => panic!("expected BlobCreated, got {other:?}"),
    };

    let out = applier.apply(
        &mut engine,
        C2S::BlobData {
            res_id,
            offset: 8,
            bytes: vec![0xaa, 0xbb, 0xcc, 0xdd],
        },
    );
    assert!(out.is_empty(), "a blob sync has no reply; got {out:?}");

    let blob = engine.read_blob(res_id);
    assert_eq!(&blob[8..12], &[0xaa, 0xbb, 0xcc, 0xdd]);
    assert_eq!(
        &blob[0..8],
        &[0u8; 8],
        "the write must not spill before its offset"
    );
}

/// A blob request before any context exists is refused. `create_blob_resource` must attach the
/// resource to a context, and `C2S::CreateBlob` does not carry one — S remembers the one
/// `C2S::CreateContext` created. Guessing an id would attach the application's memory to a context
/// that does not exist.
#[test]
fn a_blob_before_a_context_is_refused() {
    let mut engine = RecordingEngine::new();
    let mut applier = Applier::new();

    let out = applier.apply(
        &mut engine,
        C2S::CreateBlob {
            blob_mem: BLOB_MEM_HOST3D,
            blob_flags: 0,
            blob_id: 0,
            size: RING_SHMEM_SIZE as u64,
        },
    );

    assert!(
        !out.is_empty(),
        "a blob with no context to attach to must be refused"
    );
    sole_error(&out);
}

/// An engine failure is **reported**, never swallowed. C is blocked in a request/reply waiting for
/// `BlobCreated`; a silent drop hangs the application forever with no explanation anywhere.
#[test]
fn an_engine_failure_is_reported_as_an_error_not_swallowed() {
    let mut engine = RecordingEngine::new();
    engine.fail_blob_with = Some(EngineError::UnsupportedBlobMem { blob_mem: 99 });
    let mut applier = Applier::new();
    applier.apply(&mut engine, C2S::CreateContext { ctx_id: CTX_ID });

    let out = applier.apply(
        &mut engine,
        C2S::CreateBlob {
            blob_mem: 99,
            blob_flags: 0,
            blob_id: 0,
            size: 4096,
        },
    );

    assert!(
        sole_error(&out).contains("99"),
        "the engine's own complaint must survive to C; got {out:?}"
    );
}

/// Unref releases the engine's resource **and** S's mapping and ring mirror. Without it every blob
/// C ever created lives in S's resource table for the whole session — a real leak the moment (c)1
/// runs anything longer than a toy.
#[test]
fn unref_releases_the_resource_the_mapping_and_the_ring_mirror() {
    let (mut applier, mut engine, ring) = session_with_ring();

    let out = applier.apply(&mut engine, C2S::UnrefResource { res_id: ring });
    assert!(
        out.is_empty(),
        "UNREF is fire-and-forget on the wire; got {out:?}"
    );
    assert_eq!(
        engine.unreffed,
        vec![ring],
        "the engine must release its resource"
    );

    // The mirror is gone: a delta for it is now an unknown ring rather than a write into a
    // mapping S no longer owns.
    let out = applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: ring,
            tail: 4,
            bytes: vec![1, 2, 3, 4],
        },
    );
    assert!(
        sole_error(&out).contains(&ring.to_string()),
        "a delta for an unref'd ring must be refused; got {out:?}"
    );
}

/// A protocol-version mismatch is rejected loudly at the handshake, rather than left to surface as
/// misdecoded bytes later. This is the whole reason `C2S::Hello` carries the version.
#[test]
fn a_vtest_protocol_mismatch_is_rejected_at_the_handshake() {
    let mut engine = RecordingEngine::new();
    let mut applier = Applier::new();

    let out = applier.apply(
        &mut engine,
        C2S::Hello {
            vtest_protocol_version: 99,
        },
    );

    assert!(
        sole_error(&out).contains("99"),
        "a version S does not implement must be named and refused; got {out:?}"
    );
}

/// The version S does implement is accepted silently.
#[test]
fn the_supported_vtest_protocol_version_is_accepted() {
    let mut engine = RecordingEngine::new();
    let mut applier = Applier::new();

    let out = applier.apply(
        &mut engine,
        C2S::Hello {
            vtest_protocol_version: rayland_s::apply::SUPPORTED_VTEST_PROTOCOL_VERSION,
        },
    );

    assert!(
        out.is_empty(),
        "a matching handshake needs no reply; got {out:?}"
    );
}

/// `ApplyError` is a typed refusal, not a string. The daemon renders it to `S2C::Error` for the
/// wire, but a test — and a future caller that wants to treat, say, a desynchronized ring
/// differently from an unknown resource — needs the discrimination.
#[test]
fn refusals_are_typed_so_a_caller_can_tell_them_apart() {
    let (mut applier, mut engine, ring) = session_with_ring();

    let err = applier
        .try_apply(
            &mut engine,
            C2S::RingDelta {
                ring_res_id: ring,
                tail: 64,
                bytes: vec![1, 2, 3, 4],
            },
        )
        .expect_err("a contradiction must be refused");

    assert!(
        matches!(
            err,
            ApplyError::RingDelta {
                source: RingDeltaError::LengthMismatch {
                    claimed: 64,
                    carried: 4,
                    ..
                },
                ..
            }
        ),
        "got {err:?}"
    );
}
