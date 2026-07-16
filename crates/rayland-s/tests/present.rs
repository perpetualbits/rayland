//! Identifying *the frame* among S's blobs — tested against **real shared memory**, with no GPU, no
//! Mesa, no network and no compositor.
//!
//! # What is under test, and why it is the delicate part
//! Spec §7.2's lesson was: stop asking *"whose memory is this?"* and ask *"did I write it?"*, because
//! ownership predicates are guesses while observed writes are facts. **Presentation cannot honour
//! that**, and [`rayland_s::present`]'s module docs say so at length: showing a frame means
//! identifying *the* frame, which is irreducibly a "which blob is this?" question. The answer (c)1
//! ships is a size match, and the only thing that makes a guess acceptable is that it **fails loudly
//! instead of picking**. So that is what these tests are mostly about.
//!
//! # The test that exists because of (c)1 Task 6
//! Task 6 named exactly why its four findings were invisible to every unit test on this branch:
//! *"they were testing a world in which blobs are born empty. Venus's are not."* Spec §7.3: Mesa
//! creates a blob resource lazily, at `vkMapMemory`, so a readback buffer's blob comes into
//! existence **after** `vkCmdCopyImageToBuffer` has already run — S's first sight of those pages is
//! of the finished frame.
//!
//! `capture_sees_a_blob_born_with_the_frame_already_in_it` is written against that world rather than
//! the comfortable one: [`PrefillEngine`] writes the pixels into the blob's memory **before**
//! `Applier` ever maps it, which is what a GPU does. A double that handed back a zero-filled memfd
//! would let a completely broken capture pass.
//!
//! # What these tests still cannot see
//! Nothing here opens a Wayland connection, so nothing here proves a compositor accepts the buffer
//! or that a human sees a triangle. That is `rayland-present`'s `tests/live_window.rs` (which needs
//! a real compositor and skips without one) and, finally, a person looking at the screen. The
//! distinction is stated rather than blurred.

// The unit under test.
use rayland_s::apply::Applier;
use rayland_s::present::{FrameCapture, FrameError};
// The relay protocol S speaks.
use rayland_relay::{C2S, S2C};
// Real shared memory, from the repository's single copy of its shm knowledge.
use rayland_vtest::transport::create_memfd;
use rayland_vtest::{BlobResource, EngineError, EngineFrame, RenderEngine};

use std::os::fd::OwnedFd;

/// The context id used throughout, mirroring the live capture's single-context session.
const CTX_ID: u32 = 1;

/// `VIRGL_RENDERER_BLOB_MEM_HOST3D`. Ring-findings §2.1: Mesa **hardcodes** this on the vtest
/// backend, so it is what every real blob request carries.
const BLOB_MEM_HOST3D: u32 = 2;

/// The reference app's frame: 64×64 (`rayland-refapp`'s `IMAGE_WIDTH`/`IMAGE_HEIGHT`).
const FRAME_W: u32 = 64;
const FRAME_H: u32 = 64;

/// 64 × 64 × 4 = 16384 bytes — the size C0 Task 4b caught the refapp's readback buffer at (`res=6`).
const FRAME_LEN: u64 = (FRAME_W as u64) * (FRAME_H as u64) * 4;

// ---------------------------------------------------------------------------------------------
// The test double
// ---------------------------------------------------------------------------------------------

/// A [`RenderEngine`] whose blobs are **born with contents already in them** — the world spec §7.3
/// describes, and the one a real Venus session lives in.
///
/// # Why this double exists rather than reusing `tests/apply.rs`'s `RecordingEngine`
/// Two reasons, and the second is the real one. The mechanical reason is that `RecordingEngine`
/// lives in another integration-test binary and is not reachable from here. The substantive reason
/// is that it hands back **zero-filled** memfds, which is precisely the world (c)1 Task 6 found does
/// not exist: a readback blob's pages already hold the finished frame at the moment the blob is
/// created, because the GPU wrote them before `vkMapMemory` caused the blob to exist at all. A
/// capture tested only against zero-filled blobs is a capture tested against nothing.
struct PrefillEngine {
    /// Written into every blob this engine creates, before it is handed over. Truncated to the
    /// blob's size, or zero-padded if the blob is larger — so one pattern serves any size.
    prefill: Vec<u8>,
    /// The next resource id to assign. Starts at 1, matching `VirglEngine`.
    next_resource_id: u32,
}

impl PrefillEngine {
    /// A double that stamps `prefill` into the front of every blob it creates.
    fn new(prefill: Vec<u8>) -> Self {
        PrefillEngine {
            prefill,
            next_resource_id: 1,
        }
    }
}

impl RenderEngine for PrefillEngine {
    fn create_venus_context(&mut self, _ctx_id: u32) -> Result<(), EngineError> {
        Ok(())
    }

    fn venus_capset(&mut self, _version: u32) -> Result<Vec<u8>, EngineError> {
        Ok(vec![1, 2, 3, 4])
    }

    fn submit(&mut self, _ctx_id: u32, _cmd: &[u8]) -> Result<(), EngineError> {
        Ok(())
    }

    fn create_resource(
        &mut self,
        _ctx_id: u32,
        _width: u32,
        _height: u32,
        _format: u32,
    ) -> Result<u32, EngineError> {
        // Unreachable through the relay protocol: `C2S` has no classic-resource message, because
        // Mesa's Venus ICD allocates everything as blobs. Panicking rather than returning a
        // plausible id is deliberate — a stub answer would let this file stay green over a code path
        // that cannot exist in production.
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
        let res_id = self.next_resource_id;
        self.next_resource_id += 1;
        // A real anonymous shared-memory object, standing in for what virglrenderer's HOST3D path
        // allocates and exports as an SHM descriptor (ring-findings §2.1: `fd_type = 3 = SHM`).
        let fd: OwnedFd = create_memfd(size)?;
        // **Write the pixels in before anyone maps this.** This is the whole point of the double:
        // it stands where the GPU stands, and the GPU has already finished by the time Mesa asks
        // for the blob. `pwrite` at offset 0 rather than an mmap because it needs no unsafe and the
        // memory is a plain file object.
        let n = (size as usize).min(self.prefill.len());
        if n > 0 {
            // SAFETY-free path: `write_all_at` is the safe positional-write API on any file.
            use std::os::unix::fs::FileExt;
            let file =
                std::fs::File::from(fd.try_clone().expect("duplicating the blob descriptor"));
            file.write_all_at(&self.prefill[..n], 0)
                .expect("pre-filling the blob's pages");
            // `file` drops here, closing only its own duplicate; `fd` still refers to the memory.
        }
        Ok(BlobResource {
            resource_id: res_id,
            fd: Some(fd),
        })
    }

    fn unref_resource(&mut self, _resource_id: u32) {}

    fn read_back(&mut self, _resource_id: u32) -> Result<EngineFrame, EngineError> {
        unreachable!("the relay protocol never asks S to read back a classic resource")
    }
}

/// Drive an [`Applier`] through the messages that create one blob of `size`, and hand the resulting
/// replies to `capture` exactly as `rayland-s`'s `serve` loop does.
///
/// Returns the resource id the engine assigned, so a test can name it in an assertion.
fn create_blob(
    applier: &mut Applier,
    engine: &mut PrefillEngine,
    capture: &mut FrameCapture,
    size: u64,
) -> u32 {
    let out = applier.apply(
        engine,
        C2S::CreateBlob {
            blob_mem: BLOB_MEM_HOST3D,
            blob_flags: 0,
            blob_id: 0,
            size,
        },
    );
    // This is the production wiring under test, not a test-only shortcut: `serve` calls exactly
    // this, with exactly these arguments, at exactly this point.
    capture.observe_replies(applier, &out);
    // Dig the id back out of the reply so the caller can assert on it.
    match out.first().expect("CreateBlob always answers") {
        S2C::BlobCreated { res_id, .. } => *res_id,
        other => panic!("expected a BlobCreated, got {other:?}"),
    }
}

/// A fresh session with a context already created — every blob needs one.
fn session(prefill: Vec<u8>) -> (Applier, PrefillEngine, FrameCapture) {
    let mut applier = Applier::new();
    let mut engine = PrefillEngine::new(prefill);
    applier.apply(&mut engine, C2S::CreateContext { ctx_id: CTX_ID });
    let capture = FrameCapture::new(FRAME_W, FRAME_H);
    (applier, engine, capture)
}

/// A recognisable 16384-byte pattern: no run of it is equal to any other, so a capture that
/// truncated, offset or transposed it would fail rather than pass by luck.
fn frame_pattern() -> Vec<u8> {
    (0..FRAME_LEN as usize).map(|i| (i % 251) as u8).collect()
}

// ---------------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------------

/// **Spec §7.3's world.** The blob's pages hold the finished frame *before* `Applier` maps them,
/// because the GPU wrote them before `vkMapMemory` made the blob exist. The capture must see them.
///
/// If this passes with a `PrefillEngine` whose blobs were zero-filled, it is testing nothing — see
/// the module docs.
#[test]
fn capture_sees_a_blob_born_with_the_frame_already_in_it() {
    let pattern = frame_pattern();
    let (mut applier, mut engine, mut capture) = session(pattern.clone());

    create_blob(&mut applier, &mut engine, &mut capture, FRAME_LEN);

    let frame = capture
        .into_frame()
        .expect("exactly one frame-sized blob was created");
    assert_eq!(
        frame.width, FRAME_W,
        "the frame's width is the configured one"
    );
    assert_eq!(
        frame.height, FRAME_H,
        "the frame's height is the configured one"
    );
    assert_eq!(
        frame.pixels, pattern,
        "the captured pixels are the blob's actual bytes, verbatim — not zeros, not truncated"
    );
}

/// The frame is picked out from among the blobs a real session actually creates: Venus's internal
/// shmems are nothing like 16384 bytes, and the app's vertex buffer (C0 caught it at `res=3`, 64
/// bytes) is nothing like it either.
#[test]
fn capture_ignores_blobs_that_are_not_frame_sized() {
    let pattern = frame_pattern();
    let (mut applier, mut engine, mut capture) = session(pattern.clone());

    // The reply arena (1 MiB, spec §12.4), the app's 64-byte vertex buffer, then the readback.
    create_blob(&mut applier, &mut engine, &mut capture, 1 << 20);
    create_blob(&mut applier, &mut engine, &mut capture, 64);
    let readback = create_blob(&mut applier, &mut engine, &mut capture, FRAME_LEN);

    let frame = capture.into_frame().expect("one blob is frame-sized");
    assert_eq!(
        frame.pixels, pattern,
        "the frame came from the frame-sized blob (res {readback}), not from a bigger or smaller one"
    );
}

/// **The one that matters.** Two frame-sized blobs means the size predicate cannot tell them apart,
/// and (c)1's answer is to refuse rather than show a coin-flip. The error must name both, because a
/// human debugging this needs to know what it could not choose between.
#[test]
fn capture_refuses_to_guess_between_two_frame_sized_blobs() {
    let (mut applier, mut engine, mut capture) = session(frame_pattern());

    let first = create_blob(&mut applier, &mut engine, &mut capture, FRAME_LEN);
    let second = create_blob(&mut applier, &mut engine, &mut capture, FRAME_LEN);
    assert_ne!(first, second, "the double hands out distinct resource ids");

    match capture.into_frame() {
        Err(FrameError::Ambiguous { res_ids, .. }) => {
            assert_eq!(
                res_ids,
                vec![first, second],
                "the refusal names every candidate it could not choose between, in creation order"
            );
        }
        Err(other) => panic!("expected an ambiguity refusal, got {other}"),
        Ok(_) => panic!(
            "capture picked one of two identically-sized blobs; a guess here shows the wrong \
             picture and (c)1 must refuse instead"
        ),
    }
}

/// A session in which no blob is frame-sized produces a named refusal, not a panic and not a blank
/// window. This is the shape a size mismatch takes — e.g. `RAYLAND_C1_PRESENT_SIZE` set wrong, or an
/// application that is not the 64×64 reference app.
#[test]
fn capture_refuses_when_no_blob_is_frame_sized() {
    let (mut applier, mut engine, mut capture) = session(frame_pattern());

    create_blob(&mut applier, &mut engine, &mut capture, 1 << 20);
    create_blob(&mut applier, &mut engine, &mut capture, 64);

    match capture.into_frame() {
        Err(FrameError::NoCandidate { expected_len, .. }) => {
            assert_eq!(
                expected_len, FRAME_LEN as usize,
                "the refusal names the size it was looking for, which is the thing most likely wrong"
            );
        }
        Err(other) => panic!("expected a no-candidate refusal, got {other}"),
        Ok(_) => panic!("capture invented a frame out of a session that never rendered one"),
    }
}

/// The capture takes a **copy**, not a view: S's mapping of a blob is live shared memory that the
/// application's own writes and the engine's teardown can both reach, and presentation happens after
/// the session is over. A capture that borrowed would show whatever those pages became.
#[test]
fn capture_copies_the_pixels_rather_than_tracking_the_live_pages() {
    let pattern = frame_pattern();
    let (mut applier, mut engine, mut capture) = session(pattern.clone());

    let res_id = create_blob(&mut applier, &mut engine, &mut capture, FRAME_LEN);

    // Scribble over S's live mapping of the very blob that was captured, exactly as a later writer
    // to those shared pages would.
    applier.apply(
        &mut engine,
        C2S::BlobData {
            res_id,
            offset: 0,
            bytes: vec![0xAB; FRAME_LEN as usize],
        },
    );

    let frame = capture.into_frame().expect("the frame was captured");
    assert_eq!(
        frame.pixels, pattern,
        "the captured frame is the one that was there at BlobCreated, not what the pages hold now"
    );
}
