//! Coverage for `VirglEngine`'s **GUEST-backed blob** path — the one branch of
//! `create_blob_resource` that no real client reaches.
//!
//! # Why this file exists
//! Mesa's Venus ICD hardcodes `shmem_blob_mem = VCMD_BLOB_TYPE_HOST3D` (`vn_renderer_vtest.c`), so
//! every live client takes the *export* path and the GUEST branch never runs. C0's final review
//! flagged the consequence: a branch with **zero** coverage of any kind, holding an `mmap` lifetime
//! invariant (virglrenderer keeps a raw iovec into our mapping for the resource's whole life, so
//! unmapping early is a use-after-free driven by an untrusted client's command stream). Untested
//! code holding an invariant like that is not defensible; this file is the "test it" half of that
//! review's "test it or delete it" verdict.
//!
//! # What is actually asserted, and what deliberately is not
//! The invariant itself — "the mapping outlives the resource" — is a *lifetime* property that no
//! runtime assertion can observe directly: getting it wrong yields a use-after-free, which is
//! undefined behaviour, not a failed `assert!`. What this test can do, and does, is exercise the
//! whole path end to end: create a GUEST blob on a live Venus context, confirm the returned
//! descriptor names a writable memfd of exactly the requested size, then unref and drop in the
//! documented order. Under a sanitizer, or after a careless reorder of `unref_resource`'s
//! release-then-drop sequence, this is the test that has a chance of catching it. Without it,
//! nothing exercised these lines at all.
//!
//! **What this test does *not* prove:** that the fd is genuinely `MAP_SHARED` with the engine's
//! own mapping of the same memfd. Its write-then-read step writes through `client_fd` and reads
//! back through that *same* `client_fd` — and for the GUEST path, `client_fd` is a `dup()` of the
//! engine's own memfd, so that round-trip is an ordinary same-file write/read. It would pass
//! identically even if [`rayland_vtest::transport::ShmMapping::map`] used `MAP_PRIVATE` and the
//! engine's mapping were silently disconnected from what the client wrote — a mutation test
//! confirmed exactly this (see below). The property this test cannot observe is instead covered
//! by `rayland_vtest::transport::tests::shm_mapping_shares_memory_with_the_memfd`, the
//! pre-existing unit test that moved unchanged into `rayland-vtest` in Task 1: it maps the same
//! memfd through two independent mappings and checks visibility across them, which is the only
//! way to actually distinguish `MAP_SHARED` from `MAP_PRIVATE`. Anyone hardening (c)1's
//! `RelayEngine` who reads only this file's name and assertions could conclude shared-memory
//! wiring is proven end-to-end and refactor `ShmMapping::map`'s flags without noticing — the doc
//! above exists so that mistake requires ignoring it, not just missing it.
//!
//! # Skip, don't fail, without a GPU
//! Like `reliability.rs`, this gates on [`virgl_available`] and prints a SKIP line where no usable
//! Venus render node exists, so CI without a GPU stays green.

// The engine, trait, and probe under test.
use rayland_engine::{RenderEngine, VirglEngine, virgl_available};
// Positional reads: the memfd we get back is a *duplicate* of the engine's descriptor and shares
// its file offset, so `read_at` avoids disturbing/depending on that shared cursor.
use std::os::unix::fs::FileExt;
use std::path::Path;

/// The DRM render node the C0 spike used, matching `reliability.rs`.
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// `VIRGL_RENDERER_BLOB_MEM_GUEST` (virglrenderer.h). Hardcoded here rather than imported because
/// the engine's `ffi` module is private — and deliberately so: the constant is part of the *wire
/// protocol* this test is speaking, not an implementation detail it should be reaching into.
const VIRGL_RENDERER_BLOB_MEM_GUEST: u32 = 0x0001;

/// `VIRGL_RENDERER_BLOB_FLAG_USE_MAPPABLE` — the flag a client sets when it intends to `mmap` the
/// blob, which is exactly what a guest-backed ring buffer is for.
const VIRGL_RENDERER_BLOB_FLAG_USE_MAPPABLE: u32 = 0x0001;

/// The context id this test drives. Arbitrary; it only must not collide within the one engine.
const CTX_ID: u32 = 1;

/// A GUEST-backed blob's fd must be accepted by virglrenderer on a live Venus context, must name
/// a writable memfd of exactly the requested size, and the create → use → unref → drop lifecycle
/// must complete cleanly. (Whether that fd is genuinely `MAP_SHARED` with the engine's own mapping
/// is a separate property this test cannot observe — see the module doc for why, and for which
/// test does cover it.)
///
/// # What a failure here means
/// - `create_blob_resource` erroring: virglrenderer rejected a guest-backed blob on a Venus
///   context. That would be a real finding — it would mean the branch is not merely unused but
///   *unusable*, and should be deleted rather than tested.
/// - The size check or the write/read-back failing: the returned fd is not an ordinary writable,
///   correctly-sized memfd — i.e. not even the minimal shape a real client's `mmap` depends on.
///   This does **not** exercise `MAP_SHARED` sharing with the engine's own mapping; see the module
///   doc's cross-reference to `rayland_vtest::transport::tests::shm_mapping_shares_memory_with_the_memfd`
///   for that coverage.
#[test]
fn guest_backed_blob_is_accepted_by_virglrenderer_and_unrefs_cleanly() {
    // Gate: skip cleanly where no usable Venus-capable render node exists.
    let node = Path::new(RENDER_NODE);
    if !virgl_available(node) {
        eprintln!(
            "SKIP guest_backed_blob_is_accepted_by_virglrenderer_and_unrefs_cleanly: \
             no usable Venus render node at {RENDER_NODE}"
        );
        return;
    }

    let mut engine = VirglEngine::new(node).expect("engine initializes on a Venus-capable node");
    engine
        .create_venus_context(CTX_ID)
        .expect("venus context creates");

    // One page: the smallest allocation that is still a realistic `mmap` unit. The real Venus ring
    // is 128 KiB + headers, but nothing in this path is size-dependent.
    const SIZE: u64 = 4096;
    let blob = engine
        .create_blob_resource(
            CTX_ID,
            VIRGL_RENDERER_BLOB_MEM_GUEST,
            VIRGL_RENDERER_BLOB_FLAG_USE_MAPPABLE,
            // blob_id 0 is valid and is what Venus's own shmem allocations use.
            0,
            SIZE,
        )
        .expect("virglrenderer accepts a guest-backed blob on a venus context");

    // The whole point of the GUEST path: the client is handed a descriptor for the memfd the engine
    // allocated and mapped. `None` here would mean a live client would hang forever in `recvmsg`.
    let fd = blob.fd.expect("the GUEST path always yields a client fd");

    // Prove the descriptor names a real, correctly-sized memfd rather than an arbitrary fd: the
    // engine `ftruncate`d its memfd to exactly `SIZE`, so the duplicate must report that too. This
    // is a size check only — it says nothing about whether the fd is *shared* with the engine's
    // mapping, only that it is the right kind and size of thing for a client to `mmap`.
    let file = std::fs::File::from(fd);
    let len = file.metadata().expect("the blob fd stats").len();
    assert_eq!(
        len, SIZE,
        "the client's blob fd must name a memfd sized exactly as requested; a live client maps \
         this length verbatim and would fault past the end if it were short"
    );

    // Write through the client's descriptor, then read it back through the *same* descriptor.
    // What this proves and does not prove matters: `file` is a `dup()` of the engine's own memfd
    // (that is the whole GUEST design — see the module doc), so writing and reading through it
    // both times is an ordinary same-file round-trip. It cannot observe the engine's separate
    // `ShmMapping` of that memfd at all, so it cannot distinguish `MAP_SHARED` from `MAP_PRIVATE`
    // wiring — a mutation to `MAP_PRIVATE` in `ShmMapping::map` leaves this assertion passing. What
    // it *does* prove: the fd is a live, writable, seekable regular file, i.e. not some other
    // descriptor type a real client's `mmap` would reject outright.
    let marker: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];
    file.write_at(&marker, 0)
        .expect("the blob memory is writable through the client's fd");
    let mut seen = [0u8; 4];
    file.read_at(&mut seen, 0)
        .expect("the blob memory is readable through the client's fd");
    assert_eq!(
        seen, marker,
        "the client's blob fd must be an ordinary writable, seekable file; this same-fd round-trip \
         does NOT prove MAP_SHARED sharing with the engine's own mapping (see the module doc) — \
         that property is covered by \
         rayland_vtest::transport::tests::shm_mapping_shares_memory_with_the_memfd"
    );

    // Release in the documented order. `unref_resource` must release the resource inside
    // virglrenderer *before* the engine drops the `ShmMapping` its iovec points at; this call is
    // what exercises that ordering. `file` (holding the client's fd) is still live at this point —
    // it is not dropped until this function returns — so nothing here has unmapped anything yet.
    engine.unref_resource(blob.resource_id);

    // Dropping the engine here tears down the context and the renderer. If `unref_resource` had
    // left virglrenderer holding an iovec into a freed mapping, this is where it would surface.
    drop(engine);
}
