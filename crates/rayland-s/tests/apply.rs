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

/// The return path's three steps, composed — the shape [`Applier`] used to expose as one method.
///
/// # Why these tests may skip the barrier that production must not
/// The real loop in `rayland-s` is: [`Applier::take_ring_progress`] →
/// [`RenderEngine::wait_for_work_retired`] → [`Applier::take_blob_writes`], and **skipping the
/// middle step is (c)1's stale-frame defect** (`docs/c1-the-network.md` §3.1): a diff answers "did
/// these bytes change?", never "has the GPU finished?".
///
/// It is a genuine no-op here rather than a convenience. These tests drive `Applier` against mock
/// engines and a plain memfd: nothing renders asynchronously, so no GPU work is ever in flight and
/// there is nothing to retire. That is the same reasoning behind
/// `RenderEngine::wait_for_work_retired`'s default implementation.
///
/// It also means **these tests cannot catch a missing barrier** — only the live e2e can
/// (`tests/loopback_e2e.rs`'s icosa test). That is not a gap to be closed here: a mock engine has no
/// GPU to be ahead of, so the defect is unrepresentable rather than merely absent.
fn poll_progress(applier: &mut Applier) -> Vec<S2C> {
    let progress = applier.take_ring_progress();
    if progress.is_empty() {
        // Nothing retired: no wait to release, and nothing S can honestly claim its GPU wrote.
        return Vec::new();
    }
    let mut out = applier.take_blob_writes();
    // Blob bytes first, progress last: progress releases the application's wait and must not precede
    // the pixels it is about to read.
    out.extend(progress);
    out
}

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

    /// Fill a blob's pages with `fill`, standing in for **S's GPU** writing the application's
    /// mapped memory.
    ///
    /// This is the half of the `vkMapMemory` problem that runs on S: the GPU writes rendered pixels
    /// into a `HOST_VISIBLE` blob with no command, no event and nothing on any wire (ring-findings
    /// §6). C0 Task 4b caught exactly that — `res=6`, 16384 B = 64×64×4, holding the blue clear
    /// colour. Tests that need S to have pixels worth shipping back use this to play the GPU's part.
    fn write_blob(&self, res_id: u32, fill: u8) {
        let fd = self
            .blob_fds
            .get(&res_id)
            .expect("a blob this double created");
        let size = self.blob_sizes[&res_id];
        let mapping = ShmMapping::map(fd.as_fd(), size).expect("mapping the double's memfd");
        // SAFETY: the mapping is live, writable, and exactly `size` bytes for the duration of this
        // write; nothing else touches these pages while this test runs.
        unsafe {
            std::ptr::write_bytes(mapping.as_ptr().cast::<u8>(), fill, size as usize);
        }
    }

    /// Fill `len` bytes at `offset` of a blob with `fill`, standing in for **S's engine** writing
    /// part — and only part — of a blob.
    ///
    /// The whole-blob [`RecordingEngine::write_blob`] is the readback buffer's shape: the GPU
    /// rewrites every pixel. This is the reply arena's shape: virglrenderer writes one reply of a
    /// few hundred bytes into a 1 MiB pool and leaves the rest alone. Spec §7.2's rule ships *the
    /// bytes S wrote*, so a test that can only write everything cannot tell "the bytes S wrote" from
    /// "the blob they are in" — nor, since the Task 5b amendment, from "the page they are in".
    fn write_blob_range(&self, res_id: u32, offset: u64, fill: u8, len: usize) {
        let fd = self
            .blob_fds
            .get(&res_id)
            .expect("a blob this double created");
        let size = self.blob_sizes[&res_id];
        assert!(
            offset + len as u64 <= size,
            "the test's own write must fit the blob it names"
        );
        let mapping = ShmMapping::map(fd.as_fd(), size).expect("mapping the double's memfd");
        // SAFETY: `offset + len <= size` was just asserted, and the mapping is live, writable and
        // exactly `size` bytes for the duration of this write; nothing else touches these pages
        // while this test runs.
        unsafe {
            std::ptr::write_bytes(
                mapping.as_ptr().cast::<u8>().add(offset as usize),
                fill,
                len,
            );
        }
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
        [S2C::BlobCreated { res_id, .. }] => *res_id,
        other => panic!("expected exactly one BlobCreated, got {other:?}"),
    };

    (applier, engine, res_id)
}

/// Read a 32-bit little-endian control word out of a blob snapshot.
fn control(blob: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(blob[offset..offset + 4].try_into().expect("a control word"))
}

/// The sole `S2C::Error` in `out`, or a panic naming what was actually produced.
///
/// `solicited` is ignored here — the tests that care about it assert it directly, by name, rather
/// than through a helper that would make it easy to forget.
fn sole_error(out: &[S2C]) -> &str {
    match out {
        [S2C::Error { message, .. }] => message,
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

    let out = poll_progress(&mut applier);

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
        poll_progress(&mut applier).is_empty(),
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
        poll_progress(&mut applier).len(),
        1,
        "the first poll sees the move"
    );
    assert!(
        poll_progress(&mut applier).is_empty(),
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
        [S2C::BlobCreated { res_id, .. }] => *res_id,
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
        [S2C::BlobCreated { res_id, .. }] => *res_id,
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
        [S2C::BlobCreated { res_id, .. }] => *res_id,
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
        [S2C::BlobCreated { res_id, .. }] => *res_id,
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

    // A substring specific enough that only the engine's own message text could produce it (not,
    // say, "GUEST/1" or "HOST3D/2" from the same sentence), so `contains` genuinely proves the
    // engine's complaint survived rather than matching on a stray digit.
    let needle = "unsupported blob_mem 99";
    let message = sole_error(&out);
    assert!(
        message.contains(needle),
        "the engine's own complaint must survive to C; got {out:?}"
    );
    // Every source-bearing `ApplyError` variant already interpolates its cause into its own
    // `#[error(...)]` string, so rendering the wire message as `ApplyError`'s plain `Display` must
    // produce this text exactly once. A rendering path that additionally walks `Error::source()`
    // on top of that `Display` (as this crate's `render_error_chain` briefly did — (c)1 Task 4 fix
    // pass, review finding 2) would duplicate it instead, and the earlier `.contains("99")`
    // assertion could not tell the two apart: it passes whether the text appears once or twice.
    assert_eq!(
        message.matches(needle).count(),
        1,
        "S's engine refusal must appear exactly once in the wire message, not duplicated by \
         Applier's error rendering; got {message:?}"
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

// ---------------------------------------------------------------------------------------------
// (c)1 Tasks 5 / 5b: the S->C half of the blob sync — getting the replies and the picture back
//
// **Task 5b replaced the rule these tests check.** Task 5 asked "whose memory is this?" and routed
// on `blob_id` (ring-findings §6). Spec §7.2 retracted that: it never carried the reply arena at
// all (channel 2 crossed nowhere, so the application was released onto an arena of zeros and
// `vkCreateInstance` failed on `instance_version = 0`), and for the app's own blobs it was a
// last-writer-wins race — S shipped back vertex buffers its GPU never wrote.
//
// The rule now is **"did I write it?"**: S ships back exactly the bytes S wrote, found by diffing
// each blob against a baseline re-taken whenever C's own bytes land in it. So these tests are
// written the same way — they never tell S what a blob *is*, only who wrote it.
//
// The grain is the **byte**, not the page. §7.2 said "page" until Task 5b amended it: a page-grain
// run carries S's stale copy of whatever the application owns in the rest of that page, which is the
// whole-blob race again at 4096-byte scale. A `memcmp` is byte-granular for free, so the page bought
// nothing. Several tests below are written specifically to fail under the page rule — that is the
// point of them, not an incidental property.
// ---------------------------------------------------------------------------------------------

/// The app's readback buffer from the live capture (ring-findings §6): `res=6`, 16384 B = 64×64×4,
/// `blob_id = 18`. C0 Task 4b caught it holding the blue clear colour — it is how the pixels reach
/// the application at all.
const READBACK_BLOB_ID: u64 = 18;
/// That buffer's size: 64 × 64 × 4, exactly. Large enough to place writes several pages apart, which
/// is why it is the blob the granularity tests use.
const READBACK_SIZE: u64 = 16384;

/// The grain the **retracted** page rule used (spec §7.2, as amended in Task 5b).
///
/// Nothing in `rayland-s` knows this number any more — S diffs byte-granular. It survives here only
/// so these tests can place writes at page boundaries and inside single pages, which is what makes
/// them able to tell the byte rule from the page rule at all. A test that cannot distinguish the two
/// is not testing the rule it names.
const A_PAGE: usize = 4096;

/// The reply arena from the live capture (ring-findings §6): `res=2`, 1 MiB, `blob_id = 0`.
///
/// **This is spec §5's channel 2, and the whole reason Task 5b exists.** It is Venus-internal
/// memory — `blob_id == 0`, exactly like the ring and the staging pool — which is why Task 5's
/// ownership predicate excluded it and nothing ever shipped it. S's engine writes every synchronous
/// command's answer here, so it must cross, and under §7.2's rule it does: not because S knows what
/// it is, but because S observed itself write it.
const REPLY_ARENA_BLOB_ID: u64 = 0;
/// The arena's size: 1 MiB (`vn_renderer_shmem_pool` grows in 1 MiB chunks — `vn_instance.c:160`).
const REPLY_ARENA_SIZE: u64 = 1_048_576;

/// The command-buffer staging pool from the live capture: `res=4`, 8 MiB, `blob_id = 0`.
///
/// C's Mesa records command buffers into it; **S never writes it**. It is the blob that makes
/// "widen the predicate to cover `blob_id == 0`" a corruption bug rather than a fix — S's copy is
/// whatever the engine's fresh allocation held, and shipping it back would wipe a recording in
/// progress.
const STAGING_POOL_BLOB_ID: u64 = 0;
/// The staging pool's size: 8 MiB.
const STAGING_POOL_SIZE: u64 = 8_388_608;

/// The app's vertex buffer from the live capture: `res=3`, 64 B, `blob_id = 16`.
///
/// The app writes it; S's GPU only ever **reads** it. It is the blob §7.2's race is about.
const VERTEX_BUFFER_BLOB_ID: u64 = 16;
/// That buffer's size: three vertices, decoding float-for-float (C0 Task 4b).
const VERTEX_BUFFER_SIZE: u64 = 64;

/// Create a blob and return the resource id S assigned it, panicking on any other outcome.
fn create_blob(
    applier: &mut Applier,
    engine: &mut RecordingEngine,
    blob_id: u64,
    size: u64,
) -> u32 {
    let out = applier.apply(
        engine,
        C2S::CreateBlob {
            blob_mem: BLOB_MEM_HOST3D,
            blob_flags: 0,
            blob_id,
            size,
        },
    );
    match out.as_slice() {
        [S2C::BlobCreated { res_id, .. }] => *res_id,
        other => panic!("expected exactly one BlobCreated, got {other:?}"),
    }
}

/// Bring up a session with a ring **and** the application's readback blob, and return both ids.
///
/// Mirrors the live capture's shape: Venus's own shmems carry `blob_id == 0`, the application's
/// `VkDeviceMemory` allocations carry a non-zero one. Nothing S does with them depends on that any
/// more — see this section's header — but the tests speak the capture's terms so a reader can check
/// them against ring-findings §6.
fn session_with_ring_and_app_blob() -> (Applier, RecordingEngine, u32, u32) {
    let (mut applier, mut engine, ring) = session_with_ring();
    let app_blob = create_blob(&mut applier, &mut engine, READBACK_BLOB_ID, READBACK_SIZE);
    (applier, engine, ring, app_blob)
}

/// Relay a delta and have virglrenderer's ring thread retire it, storing `head`.
///
/// The two are separate events in reality — the ring thread runs asynchronously and S's daemon has
/// to *poll* for the result — but every test below wants the moment after both, so they are driven
/// together here.
fn relay_and_retire(
    applier: &mut Applier,
    engine: &mut RecordingEngine,
    ring: u32,
    from_tail: u32,
    to_tail: u32,
) {
    let out = applier.apply(
        engine,
        C2S::RingDelta {
            ring_res_id: ring,
            tail: to_tail,
            bytes: vec![0xaa; (to_tail - from_tail) as usize],
        },
    );
    assert!(out.is_empty(), "a relayed delta has no reply; got {out:?}");
    // virglrenderer's ring thread consumes the bytes and stores `head` (`vkr_ring_store_head`,
    // `vkr_ring.c:60-67`). This is the only evidence S ever has that anything retired.
    engine.write_control(ring, RING_HEAD_OFFSET, to_tail);
}

/// Every `S2C::BlobData` in `out` for `res_id`, as `(offset, bytes)` pairs, in the order S emitted
/// them.
fn blob_runs_for(out: &[S2C], res_id: u32) -> Vec<(u64, Vec<u8>)> {
    out.iter()
        .filter_map(|m| match m {
            S2C::BlobData {
                res_id: r,
                offset,
                bytes,
            } if *r == res_id => Some((*offset, bytes.clone())),
            _ => None,
        })
        .collect()
}

/// Drive a session to the point where S's engine has retired commands and its GPU has written the
/// application's readback blob — i.e. exactly the moment S owes C both pixels and progress.
fn a_retired_batch_with_pixels(fill: u8) -> (Applier, RecordingEngine, u32, u32) {
    let (mut applier, mut engine, ring, app_blob) = session_with_ring_and_app_blob();
    let out = applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: ring,
            tail: 64,
            bytes: vec![0xaa; 64],
        },
    );
    assert!(out.is_empty(), "a relayed delta has no reply; got {out:?}");
    // The GPU renders: the readback blob now holds the picture.
    engine.write_blob(app_blob, fill);
    // virglrenderer's ring thread retires the batch and stores `head`.
    engine.write_control(ring, RING_HEAD_OFFSET, 64);
    (applier, engine, ring, app_blob)
}

/// **The blocker Task 5b exists to remove: the reply arena must cross S->C.**
///
/// Spec §5's channel 2 is where S's engine writes the answer to every synchronous Vulkan command.
/// Until this test passed, nothing shipped it, and the failure was **not** the hang one would
/// expect: `head` advances from `S2C::RingProgress` regardless, so `vn_ring_wait_seqno` returns and
/// the application is released onto an arena that is still zeros.
/// `vn_instance_init_renderer_versions` reads `instance_version = 0`, fails the
/// `VN_MIN_RENDERER_VERSION` check, and `vkCreateInstance` fails. Silent garbage.
///
/// The arena's `blob_id` is **0** — Venus-internal, indistinguishable by that signal from the ring
/// and the staging pool. It crosses here for one reason only: S observed itself write those bytes.
#[test]
fn the_reply_arena_crosses_because_s_wrote_it_not_because_s_knows_what_it_is() {
    let (mut applier, mut engine, ring) = session_with_ring();
    let arena = create_blob(
        &mut applier,
        &mut engine,
        REPLY_ARENA_BLOB_ID,
        REPLY_ARENA_SIZE,
    );

    // C relays a reply-bearing command; S's engine answers into the arena and the batch retires.
    let out = applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: ring,
            tail: 64,
            bytes: vec![0xaa; 64],
        },
    );
    assert!(out.is_empty(), "a relayed delta has no reply; got {out:?}");
    engine.write_blob_range(arena, 0, 0x5a, 128);
    engine.write_control(ring, RING_HEAD_OFFSET, 64);

    let out = poll_progress(&mut applier);

    let runs = blob_runs_for(&out, arena);
    assert_eq!(
        runs.len(),
        1,
        "S wrote one contiguous region of the arena, so it owes C exactly one run; got {runs:?}"
    );
    assert_eq!(runs[0].0, 0, "the reply starts at the arena's beginning");
    assert_eq!(
        &runs[0].1[..128],
        &[0x5a; 128],
        "the bytes C's Mesa will decode as its reply must be the ones S's engine actually wrote; \
         without them vn_instance_init_renderer_versions reads instance_version = 0 and \
         vkCreateInstance fails"
    );
}

/// **The race regression test, and the sharpest one in this file: a blob S never wrote must not be
/// shipped back, even though C wrote it and it is therefore dirty.**
///
/// Spec §7.2's scenario, concretely: the application `memcpy`s frame *N+1*'s vertices into `res=3`
/// and C relays them; S's GPU **reads** them and never writes them; S's poll fires on the `head`
/// movement and — under Task 5's rule — shipped S's copy of `res=3` straight back, where C's reader
/// overwrote the application's fresh vertices with it. Nothing detects that: it is a silent,
/// timing-dependent lost write.
///
/// It is invisible in the reference app, which writes its vertices exactly once, so C's and S's
/// copies converge no matter what — which is precisely why every test passed before this one.
#[test]
fn a_blob_s_never_wrote_is_not_shipped_back_even_though_c_wrote_it() {
    let (mut applier, mut engine, ring) = session_with_ring();
    let vertices = create_blob(
        &mut applier,
        &mut engine,
        VERTEX_BUFFER_BLOB_ID,
        VERTEX_BUFFER_SIZE,
    );

    // C's own C->S sync ships the application's fresh vertices ahead of the delta that draws them.
    let out = applier.apply(
        &mut engine,
        C2S::BlobData {
            res_id: vertices,
            offset: 0,
            bytes: vec![0x33; VERTEX_BUFFER_SIZE as usize],
        },
    );
    assert!(out.is_empty(), "a blob sync has no reply; got {out:?}");
    // The batch retires. S's GPU read the vertices; a vertex buffer is the app's alone to write, so
    // S wrote nothing into it.
    relay_and_retire(&mut applier, &mut engine, ring, 0, 64);

    let out = poll_progress(&mut applier);

    assert_eq!(
        blob_runs_for(&out, vertices),
        Vec::new(),
        "S's GPU never wrote the vertex buffer, so S has nothing to say about it. Shipping it back \
         would overwrite whatever the application has written into it since — a lost write with \
         nothing anywhere naming the cause. Got {out:?}"
    );
    assert!(
        out.iter().any(|m| matches!(m, S2C::RingProgress { .. })),
        "the retirement itself must still be reported, or this test would pass for an S that had \
         simply stopped: {out:?}"
    );
}

/// **The ring is excluded by `res_id` — structurally, not by a guess about its bytes.**
///
/// S's engine genuinely writes the ring's pages: `vkr_ring_store_head` (`vkr_ring.c:60-67`) stores
/// `head` into them after each dispatched command. So the "did I write it?" predicate says **yes**,
/// and it must not be allowed to answer: `head` and `status` are the ring thread's, C's `tail` and
/// command bytes are C's, and `S2C::RingProgress` already owns the only part of that C needs.
/// Shipping the ring back as blob data would overwrite the commands C is in the middle of relaying
/// with S's copy of them.
///
/// S already holds `self.rings`, so this exclusion costs nothing and cannot be fooled — unlike a
/// `blob_id` test, which is a claim about a number a remote peer chose.
#[test]
fn the_ring_is_excluded_by_res_id_even_though_s_wrote_its_head() {
    let (mut applier, mut engine, ring) = session_with_ring();

    relay_and_retire(&mut applier, &mut engine, ring, 0, 64);

    let out = poll_progress(&mut applier);

    assert_eq!(
        blob_runs_for(&out, ring),
        Vec::new(),
        "S wrote `head` into the ring's own pages, so an observed-writes rule that did not exclude \
         rings would ship them; `RingProgress` owns that news. Got {out:?}"
    );
    assert!(
        out.iter().any(|m| matches!(m, S2C::RingProgress { .. })),
        "the `head` movement must still be reported — as progress, not as bytes: {out:?}"
    );
}

/// **C's own writes must never come back to it as if they were S's.**
///
/// S's baseline for "what does C already have?" is re-taken every time an inbound `C2S::BlobData`
/// lands bytes in a blob. Without that, the sequence below is an infinite echo: C writes, S's diff
/// sees pages that differ from an older baseline, S ships them back as its own, C applies them over
/// whatever the application has written since.
///
/// This is the one property that cannot be checked by shipping nothing and cannot be checked by
/// shipping everything, so it pins the snapshot specifically.
#[test]
fn an_inbound_blob_data_re_snapshots_so_c_s_own_write_is_never_shipped_back() {
    let (mut applier, mut engine, ring, app_blob) = session_with_ring_and_app_blob();

    // Round 1: S's GPU renders and the batch retires, so the pixels legitimately cross.
    relay_and_retire(&mut applier, &mut engine, ring, 0, 64);
    engine.write_blob(app_blob, 0x5a);
    assert!(
        !blob_runs_for(&poll_progress(&mut applier), app_blob).is_empty(),
        "S wrote this blob, so round 1 must ship it — otherwise round 2 proves nothing"
    );

    // C now writes the whole blob itself and relays another batch. S's GPU writes nothing.
    let out = applier.apply(
        &mut engine,
        C2S::BlobData {
            res_id: app_blob,
            offset: 0,
            bytes: vec![0x77; READBACK_SIZE as usize],
        },
    );
    assert!(out.is_empty(), "a blob sync has no reply; got {out:?}");
    relay_and_retire(&mut applier, &mut engine, ring, 64, 128);

    let out = poll_progress(&mut applier);

    assert_eq!(
        blob_runs_for(&out, app_blob),
        Vec::new(),
        "those 0x77 bytes are C's own, applied to S's pages by C's message. Shipping them back \
         would be S echoing C's write as if it were its GPU's, and it would clobber anything the \
         application wrote in the meantime. Got {out:?}"
    );
}

/// **Only the bytes S wrote cross — not the blob they are in, and not the page they are in.**
///
/// The reply arena is the case that makes this matter: virglrenderer writes a few hundred bytes of
/// reply into a 1 MiB pool. Shipping the pool would be ~256x the traffic, and — worse — it would
/// carry S's stale copy of every byte C has written into the same blob since.
///
/// The `offset` field this uses has been on the wire since Task 4 and was always 0 until now.
#[test]
fn only_the_bytes_s_wrote_cross_not_the_whole_blob() {
    let (mut applier, mut engine, ring, app_blob) = session_with_ring_and_app_blob();

    // S's engine writes four bytes, partway into the blob.
    engine.write_blob_range(app_blob, 2 * A_PAGE as u64, 0xc3, 4);
    relay_and_retire(&mut applier, &mut engine, ring, 0, 64);

    let out = poll_progress(&mut applier);

    let runs = blob_runs_for(&out, app_blob);
    assert_eq!(
        runs,
        vec![(2 * A_PAGE as u64, vec![0xc3; 4])],
        "S wrote four bytes, so four bytes cross — not the blob's {READBACK_SIZE}, and not the \
         4096-byte page containing them, of which 4092 bytes are memory S never touched. Got \
         {runs:?}"
    );
}

/// Neighbouring changed bytes cross as **one** run, and a gap of bytes S did not write splits them.
///
/// A reply spanning a large region must not arrive as a scatter of unrelated messages, and two
/// distant writes must not drag the untouched bytes between them along. Both fall out of coalescing
/// contiguous changed bytes, which is what this pins.
#[test]
fn contiguous_changed_bytes_coalesce_and_a_gap_splits_them() {
    let (mut applier, mut engine, ring, app_blob) = session_with_ring_and_app_blob();

    // One region spanning two pages, to prove a run is not chopped at a page boundary; then a gap;
    // then a second, small write, to prove the gap is not bridged and the run is not padded out.
    engine.write_blob_range(app_blob, 0, 0xa1, 2 * A_PAGE);
    engine.write_blob_range(app_blob, 3 * A_PAGE as u64, 0xb2, 16);
    relay_and_retire(&mut applier, &mut engine, ring, 0, 64);

    let out = poll_progress(&mut applier);

    let runs = blob_runs_for(&out, app_blob);
    let shapes: Vec<(u64, usize)> = runs.iter().map(|(o, b)| (*o, b.len())).collect();
    assert_eq!(
        shapes,
        vec![(0, 2 * A_PAGE), (3 * A_PAGE as u64, 16)],
        "the 8192-byte write is one run across a page boundary; the untouched bytes after it must \
         not be dragged along; and the 16-byte write is 16 bytes, not the page holding it"
    );
}

/// **The amendment's test: S writes one region of a page, the application owns another region of
/// the *same* page, and only S's bytes cross.**
///
/// This is the hole the page grain left, and the reason spec §7.2 was amended during Task 5b.
/// `VkDeviceMemory` is page-aligned and applications suballocate, so an application's buffer sharing
/// a 4096-byte page with something S's engine writes is ordinary rather than contrived — and the two
/// need **no Vulkan synchronization between them**, because they are different memory. The
/// application is entitled to be writing its region at the very moment S writes its own.
///
/// Under the page rule S's run was rounded out to the whole page, so it carried S's copy of the
/// application's region — which is stale by construction, being whatever C last sent — and C's reader
/// laid it down over whatever the application had written since. That is the whole-blob race §7.2
/// exists to remove, reappearing at 4096-byte scale: invisible in the reference app, which writes its
/// vertices exactly once, and live for the first real application.
///
/// The assertion is therefore not merely that the run is short. It is that **no byte of the
/// application's region crosses at all**, which is the property the length is evidence for.
#[test]
fn s_s_run_does_not_carry_the_application_s_bytes_from_the_same_page() {
    let (mut applier, mut engine, ring, app_blob) = session_with_ring_and_app_blob();

    // The application's bytes arrive from C and land in this page, as they do in production: through
    // `C2S::BlobData`, which is also what re-takes S's baseline over exactly this range.
    const APP_REGION: u64 = 64;
    const APP_LEN: usize = 64;
    let out = applier.apply(
        &mut engine,
        C2S::BlobData {
            res_id: app_blob,
            offset: APP_REGION,
            bytes: vec![0xaa; APP_LEN],
        },
    );
    assert!(out.is_empty(), "a blob sync has no reply; got {out:?}");

    // S's engine writes its own region of the very same page — the first four bytes.
    engine.write_blob_range(app_blob, 0, 0xc3, 4);
    relay_and_retire(&mut applier, &mut engine, ring, 0, 64);

    let out = poll_progress(&mut applier);

    let runs = blob_runs_for(&out, app_blob);
    assert_eq!(
        runs,
        vec![(0, vec![0xc3; 4])],
        "S wrote bytes 0..4 and nothing else in this page; the application owns bytes 64..128 of it. \
         Only S's four bytes may cross. Got {runs:?}"
    );

    // The property the length above is evidence for, asserted directly: whatever the shape of the
    // runs, not one byte of the application's region may be inside any of them. A page-grain run
    // starting at 0 spans the application's region and would clobber it on C.
    for (offset, bytes) in &runs {
        let end = offset + bytes.len() as u64;
        assert!(
            end <= APP_REGION || *offset >= APP_REGION + APP_LEN as u64,
            "a run covering [{offset}, {end}) overlaps the application's region \
             [{APP_REGION}, {}) — S never wrote those bytes, so S's copy of them is stale by \
             construction, and shipping them clobbers whatever the application has written since",
            APP_REGION + APP_LEN as u64
        );
    }
}

/// **The command-buffer staging pool must never cross S->C**, because S never writes it.
///
/// This is the blob that made Task 5's obvious fix — "widen the ownership predicate to include
/// `blob_id == 0` so the arena crosses" — a corruption bug: the pool is `blob_id == 0` too, C's Mesa
/// is actively recording command buffers into it, and S's copy is whatever its engine's fresh
/// allocation held. Shipping it back would wipe a recording in progress. Under §7.2's rule the
/// question never arises: S wrote nothing here, so S says nothing.
#[test]
fn the_command_buffer_staging_pool_never_crosses_because_s_never_writes_it() {
    let (mut applier, mut engine, ring) = session_with_ring();
    let pool = create_blob(
        &mut applier,
        &mut engine,
        STAGING_POOL_BLOB_ID,
        STAGING_POOL_SIZE,
    );

    relay_and_retire(&mut applier, &mut engine, ring, 0, 64);

    let out = poll_progress(&mut applier);

    assert_eq!(
        blob_runs_for(&out, pool),
        Vec::new(),
        "S never writes the staging pool; shipping its copy back would wipe the command buffers \
         C's Mesa is recording into it. Got {out:?}"
    );
}

/// **The mirror of Task 5's central assertion, and the sharper of the two: the pixels must reach C
/// before the message that releases the application's wait.**
///
/// `S2C::RingProgress` is what advances C's local `head`, and `head` is the **reply-ready signal** —
/// `vn_ring_get_seqno_status` is `vn_ring_ge_seqno(ring, vn_ring_load_head(ring), seqno)`
/// (`vn_ring.c:176-179`), which `vn_ring_wait_seqno` busy-polls. So progress *releases the
/// application*. Sent before the blob, it releases the application onto memory that is still zeros.
///
/// Ring-findings §7 names this exact ordering constraint — *a transport must ship the shmem contents
/// before it ships the head update that releases the client's wait* — and warns it produces
/// once-an-hour heisenbugs. Here it would not be once an hour: it is every frame the app reads back.
#[test]
fn the_gpu_s_pixels_are_shipped_before_the_progress_that_releases_the_app_s_wait() {
    let (mut applier, _engine, _ring, app_blob) = a_retired_batch_with_pixels(0x5a);

    let out = poll_progress(&mut applier);

    let blob_at = out
        .iter()
        .position(|m| matches!(m, S2C::BlobData { res_id, .. } if *res_id == app_blob))
        .expect("S must ship back the blob its GPU wrote; without it the app reads zeros");
    let progress_at = out
        .iter()
        .position(|m| matches!(m, S2C::RingProgress { .. }))
        .expect("progress must still be reported");

    assert!(
        blob_at < progress_at,
        "the pixels must be on their way before the head update that releases the application's \
         wait, but progress came first (blob at {blob_at}, progress at {progress_at}); the app \
         would return from its synchronous call and read a buffer S had not yet sent: {out:?}"
    );
}

/// The bytes S ships back must be the ones its GPU actually wrote. A message with the right id and
/// stale contents would satisfy an ordering test and still hand the application the wrong picture.
#[test]
fn the_shipped_blob_carries_the_bytes_s_s_gpu_actually_wrote() {
    let (mut applier, _engine, _ring, app_blob) = a_retired_batch_with_pixels(0x5a);

    let out = poll_progress(&mut applier);

    let (offset, bytes) = out
        .iter()
        .find_map(|m| match m {
            S2C::BlobData {
                res_id,
                offset,
                bytes,
            } if *res_id == app_blob => Some((*offset, bytes)),
            _ => None,
        })
        .expect("the readback blob");
    assert_eq!(
        offset, 0,
        "S's GPU rewrote every pixel, so every page changed and the whole blob is one run starting \
         at its beginning"
    );
    assert_eq!(
        bytes,
        &vec![0x5au8; READBACK_SIZE as usize],
        "the application's frame is exactly what S's GPU left in these pages"
    );
}

/// A poll that finds no movement must ship **nothing** — not even the blobs.
///
/// This is what keeps the 200 µs poll loop a latency mechanism rather than a bandwidth source. S has
/// no evidence its GPU wrote anything, so it has nothing to say; shipping a 16 KiB blob 5000 times a
/// second on the chance something changed would saturate the link the sub-project exists to measure.
#[test]
fn a_poll_that_finds_no_movement_ships_nothing_at_all() {
    let (mut applier, mut engine, ring, app_blob) = session_with_ring_and_app_blob();
    applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: ring,
            tail: 8,
            bytes: vec![0u8; 8],
        },
    );
    // The GPU has written something, but the ring thread has not retired anything: `head` is
    // untouched, so S has no evidence to report and no wait to release.
    engine.write_blob(app_blob, 0x77);

    assert!(
        poll_progress(&mut applier).is_empty(),
        "with `head` unmoved there is no retired batch, so no progress and therefore no reason to \
         ship a blob; a blob per poll would make this loop a bandwidth source"
    );
}

/// Progress reported a second time for the same `head` ships no blobs either.
///
/// `take_progress` already gates on movement (a repeat proves only that S's process is scheduled —
/// ring-findings §5.4), and the blob sync must inherit that gate rather than re-open it. Otherwise
/// every poll of a quiescent session would re-ship every mapped blob.
#[test]
fn a_quiescent_ring_re_ships_nothing_on_subsequent_polls() {
    let (mut applier, _engine, _ring, _app_blob) = a_retired_batch_with_pixels(0x5a);

    // First poll: the batch retired, so pixels and progress both go.
    assert!(
        !poll_progress(&mut applier).is_empty(),
        "the first poll after a retired batch owes C both"
    );

    // Nothing has moved since.
    assert!(
        poll_progress(&mut applier).is_empty(),
        "an unmoved `head` is not progress and not a reason to re-ship a blob"
    );
}

// ---------------------------------------------------------------------------------------------
// (c)1 Task 5: `solicited`, the field that keeps C's reply channel from desynchronizing
// ---------------------------------------------------------------------------------------------

/// Whether the sole `S2C::Error` in `out` claims to answer something C is blocked on.
fn sole_error_is_solicited(out: &[S2C]) -> bool {
    match out {
        [S2C::Error { solicited, .. }] => *solicited,
        other => panic!("expected exactly one S2C::Error, got {other:?}"),
    }
}

/// **A refusal of a fire-and-forget message must be marked unsolicited.**
///
/// This is the field C's reply routing depends on, and `C2S::BlobData` is the case (c)1 Task 5 made
/// dangerous: it is sent from C's **ring watcher** thread, which waits for nothing, on **every ring
/// delta** — many times a second on the application's hot path. Marked solicited, S's refusal would
/// be queued by C as an answer, where it would satisfy the *next* `GetCapset` or `CreateBlob` and
/// desynchronize every request thereafter, permanently and unboundedly.
///
/// The blob id here is one S has no resource for, which is precisely the refusal a real
/// desynchronized session would produce.
#[test]
fn a_refusal_of_a_fire_and_forget_blob_data_is_marked_unsolicited() {
    let (mut applier, mut engine, _ring) = session_with_ring();

    let out = applier.apply(
        &mut engine,
        C2S::BlobData {
            res_id: 999,
            offset: 0,
            bytes: vec![1, 2, 3, 4],
        },
    );

    assert!(
        !sole_error_is_solicited(&out),
        "nothing on C waits for a BlobData — it comes from the ring watcher. Marking its refusal \
         solicited would have C answer its next request with this error, and every request after \
         that with the previous one's reply, forever"
    );
}

/// The same, for the other message C's watcher fires and forgets: the ring delta itself.
#[test]
fn a_refusal_of_a_fire_and_forget_ring_delta_is_marked_unsolicited() {
    let (mut applier, mut engine, _ring) = session_with_ring();

    // A delta naming a resource that is not a ring: S has no blob for 999 at all.
    let out = applier.apply(
        &mut engine,
        C2S::RingDelta {
            ring_res_id: 999,
            tail: 4,
            bytes: vec![1, 2, 3, 4],
        },
    );

    assert!(
        !sole_error_is_solicited(&out),
        "a RingDelta comes from the ring watcher, which never waits for a reply"
    );
}

/// **The other half: a refusal of a message C genuinely blocks on must stay solicited.**
///
/// Without this, the fix above would be a different bug rather than a fix. C's Mesa sits in
/// `recvmsg` waiting for a blob's descriptor, and `RelayEngine::create_blob_resource` is blocked in
/// a request/reply; an error S declines to route is an application that hangs forever with no
/// explanation anywhere — exactly what `Applier::apply` being total exists to prevent.
#[test]
fn a_refusal_of_a_request_c_blocks_on_stays_solicited() {
    let mut engine = RecordingEngine::new();
    let mut applier = Applier::new();
    applier.apply(&mut engine, C2S::CreateContext { ctx_id: CTX_ID });
    engine.fail_blob_with = Some(EngineError::NoVenusCapset);

    let out = applier.apply(
        &mut engine,
        C2S::CreateBlob {
            blob_mem: BLOB_MEM_HOST3D,
            blob_flags: 0,
            blob_id: 0,
            size: 4096,
        },
    );

    assert!(
        sole_error_is_solicited(&out),
        "C is blocked in a request/reply for this blob's resource id, and its Mesa is blocked in \
         recvmsg behind that. An error not routed to it is an application that hangs forever"
    );
}

// ---------------------------------------------------------------------------------------------
// (c)1 fence-feedback: `Applier::fingerprint_nonring_blobs`
// ---------------------------------------------------------------------------------------------

/// `fingerprint_nonring_blobs` reports every blob that is not a ring, and omits rings — the contract
/// the fence-feedback delivery loop relies on to watch the feedback buffer (a non-ring Venus-internal
/// blob) from its very first write, while never mistaking a ring's `head`/command bytes for a write to
/// return.
#[test]
fn fingerprint_nonring_blobs_covers_non_rings_and_omits_rings() {
    let (mut applier, mut engine, ring_res_id) = session_with_ring();

    // The app's 64-byte vertex buffer from the live capture: a real blob, but not a ring (its
    // power-of-two size is not ring-shaped, so `RingIdentity::from_blob_request` rejects it).
    let out = applier.apply(
        &mut engine,
        C2S::CreateBlob {
            blob_mem: BLOB_MEM_HOST3D,
            blob_flags: 0,
            blob_id: 16,
            size: 64,
        },
    );
    let plain_res_id = match out.as_slice() {
        [S2C::BlobCreated { res_id, .. }] => *res_id,
        other => panic!("expected exactly one BlobCreated, got {other:?}"),
    };

    let ids: std::collections::HashSet<u32> = applier
        .fingerprint_nonring_blobs()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(
        ids.contains(&plain_res_id),
        "a non-ring blob must be fingerprinted"
    );
    assert!(
        !ids.contains(&ring_res_id),
        "a ring blob must be omitted"
    );
}

// ---------------------------------------------------------------------------------------------
// (c)2 Task 1: the blob-write split — Venus-internal writes at retirement, app writes post-fence
// ---------------------------------------------------------------------------------------------

/// Fill one byte of a blob, standing in for **S's engine** touching a single byte of it.
///
/// A thin wrapper over [`RecordingEngine::write_blob_range`] rather than a new engine-double API:
/// the brief for this task is explicit that a one-byte write is enough to produce a run, and the
/// double already has the primitive.
fn write_blob_byte(engine: &RecordingEngine, res_id: u32, offset: u64, fill: u8) {
    engine.write_blob_range(res_id, offset, fill, 1);
}

/// The `res_id` of an [`S2C::BlobData`], or `None` for any other message — for filtering a
/// `take_*_blob_writes` result down to which resources it named, independent of run count.
fn res_id_of_blobdata(msg: &S2C) -> Option<u32> {
    match msg {
        S2C::BlobData { res_id, .. } => Some(*res_id),
        _ => None,
    }
}

/// The blob-write split feeds the fence-gated return path: reply-arena (Venus-internal) ships at
/// ring retirement, the readback + feedback word (application blobs, largest first so the big
/// readback leads the tiny feedback word) ship after the GPU fence. This pins that partition and
/// that ordering.
#[test]
fn blob_write_split_partitions_venus_from_app_and_orders_app_largest_first() {
    let (mut applier, mut engine, _ring) = session_with_ring();

    // A Venus-internal blob (blob_id == 0) and two application blobs (blob_id != 0) of different
    // sizes, so the largest-first ordering is observable.
    let venus = create_blob(&mut applier, &mut engine, /*blob_id*/ 0, /*size*/ 256);
    let app_small = create_blob(&mut applier, &mut engine, /*blob_id*/ 16, /*size*/ 64);
    let app_big = create_blob(&mut applier, &mut engine, /*blob_id*/ 16, /*size*/ 1024);

    // Make S "write" a byte into each so each produces a run.
    write_blob_byte(&engine, venus, 0, 0xAA);
    write_blob_byte(&engine, app_small, 0, 0xBB);
    write_blob_byte(&engine, app_big, 0, 0xCC);

    let venus_out = applier.take_venus_blob_writes();
    let venus_ids: Vec<u32> = venus_out.iter().filter_map(res_id_of_blobdata).collect();
    assert_eq!(venus_ids, vec![venus], "only the Venus-internal blob ships in the venus split");

    let app_out = applier.take_app_blob_writes();
    let app_ids: Vec<u32> = app_out.iter().filter_map(res_id_of_blobdata).collect();
    assert_eq!(app_ids, vec![app_big, app_small], "app split ships app blobs, largest first");
}
