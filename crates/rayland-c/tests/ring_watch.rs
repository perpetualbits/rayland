//! The ring watcher is where (c)1 is most likely to hang intermittently, so it is tested against a
//! synthetic ring rather than only in a live drive where a stall looks like a network problem.
//!
//! # Why a `Vec<u8>` is a faithful stand-in for the real thing
//! The real ring is a shared-memory blob that Mesa's Venus ICD writes and `rayland-c` reads. From
//! the watcher's point of view it is nothing but a byte array with three control words at known
//! offsets and a command buffer after them, so a `vec![0u8; 131268]` — the exact size a live client
//! asked for (`192 control + 131072 buffer + 4 extra`, ring-findings §4) — exercises every line of
//! the watcher's logic. What it deliberately does *not* reproduce is the concurrency: here the test
//! plays Mesa's part by writing `tail` itself, at exactly the moment that provokes the race. That
//! is the point. A live drive can only *hope* to hit the interleaving these tests *force*.
//!
//! # The two properties worth this much trouble
//! 1. **Delta extraction must be exact.** Re-sending a byte replays a Vulkan command on S's GPU;
//!    dropping one desynchronizes the decoder and every command after it is confident nonsense.
//! 2. **The park decision must never sleep through pending work.** See
//!    `park_is_refused_when_tail_moved_after_idle_was_published` for the mechanism.

use rayland_c::ring::{ParkDecision, RingIdentity, RingWatcher};
use rayland_vtest::venus_ring::{
    RING_BUFFER_OFFSET, RING_BUFFER_SIZE, RING_HEAD_OFFSET, RING_SHMEM_SIZE, RING_STATUS_OFFSET,
    RING_TAIL_OFFSET,
};

/// A tail that advanced must yield exactly the bytes written between the old and new tail.
#[test]
fn advancing_tail_yields_exactly_the_new_bytes() {
    let mut ring = vec![0u8; 131268];
    ring[RING_BUFFER_OFFSET..RING_BUFFER_OFFSET + 4].copy_from_slice(&[0xb2, 0, 0, 0]);
    write_u32(&mut ring, RING_TAIL_OFFSET, 4);

    let mut w = RingWatcher::new(1, 131072);
    let delta = w.take_delta(&ring).expect("a delta");
    assert_eq!(delta.tail, 4);
    assert_eq!(delta.bytes, vec![0xb2, 0, 0, 0]);

    // Draining twice must not re-send bytes: duplicate commands would be replayed on the GPU.
    assert!(w.take_delta(&ring).is_none(), "no new bytes, no delta");
}

/// THE HANG BUG THIS TEST EXISTS FOR: Mesa only sends a doorbell when the IDLE bit is set *and* at
/// least 1 ms has passed since the last kick (`vn_ring.c:475-483`). So a kick is NOT guaranteed for
/// every write. A watcher that sets IDLE and sleeps unconditionally will miss work and stall.
/// It MUST re-read `tail` after publishing IDLE and stay awake if it changed.
#[test]
fn park_is_refused_when_tail_moved_after_idle_was_published() {
    let mut ring = vec![0u8; 131268];
    let mut w = RingWatcher::new(1, 131072);
    w.take_delta(&ring);

    // Simulate the race: the watcher publishes IDLE, and Mesa writes before it can sleep.
    w.publish_idle(&mut ring);
    write_u32(&mut ring, RING_TAIL_OFFSET, 8);

    assert_eq!(
        w.decide_park(&mut ring),
        ParkDecision::StayAwake,
        "tail moved after IDLE was published; parking here would sleep through pending work \
         because Mesa's >=1ms throttle may suppress the kick"
    );
    // And IDLE must be cleared again, or Mesa keeps paying for kicks we do not need.
    assert_eq!(read_u32(&ring, RING_STATUS_OFFSET) & 1, 0);
}

/// The *other half* of the same race, and the one a "snapshot the tail inside `publish_idle`"
/// implementation gets wrong.
///
/// The window in which Mesa can write is not merely "after IDLE was published" — it opens the
/// instant `take_delta` finishes reading `tail`. A watcher that decided whether to park by
/// comparing against the tail it observed *at `publish_idle` time* would be blind to anything
/// written between the drain and the publish: both reads would return the same value, the watcher
/// would conclude "nothing new", and it would park on top of pending work. Mesa's >=1 ms kick
/// throttle then makes the resulting stall silent and intermittent, exactly as in the sibling test.
///
/// The correct question is not "did tail move since I published IDLE?" but **"is there anything I
/// have not yet drained?"** — i.e. compare against `last_tail`, the frontier `take_delta` reached.
/// This test writes `tail` *before* `publish_idle` precisely so that the two implementations
/// disagree: the correct one refuses to park, the snapshot one parks and hangs.
#[test]
fn park_is_refused_when_tail_moved_before_idle_was_published() {
    let mut ring = vec![0u8; 131268];
    let mut w = RingWatcher::new(1, 131072);
    w.take_delta(&ring);

    // Mesa writes in the window between the drain and the publish — the case a publish-time
    // snapshot cannot see.
    write_u32(&mut ring, RING_TAIL_OFFSET, 12);
    w.publish_idle(&mut ring);

    assert_eq!(
        w.decide_park(&mut ring),
        ParkDecision::StayAwake,
        "tail moved between the drain and the IDLE publish; the park decision must be made \
         against the drained frontier (last_tail), not against a publish-time snapshot"
    );
    assert_eq!(read_u32(&ring, RING_STATUS_OFFSET) & 1, 0);
}

/// The only condition under which parking is safe: everything produced has been drained. The
/// positive case matters as much as the negative one — a watcher hard-coded to `StayAwake` would
/// pass both tests above while busy-spinning a core forever and never letting Mesa's doorbell
/// mechanism do its job.
#[test]
fn park_is_allowed_only_when_the_ring_is_fully_drained() {
    let mut ring = vec![0u8; 131268];
    write_u32(&mut ring, RING_TAIL_OFFSET, 16);

    let mut w = RingWatcher::new(1, 131072);
    w.take_delta(&ring).expect("the 16 produced bytes");

    // Nothing new since the drain, so IDLE may stand and the watcher may sleep.
    w.publish_idle(&mut ring);
    assert_eq!(w.decide_park(&mut ring), ParkDecision::Park);
    assert_eq!(
        read_u32(&ring, RING_STATUS_OFFSET) & 1,
        1,
        "a watcher that parks must leave IDLE set, or Mesa will never know to kick it awake"
    );
}

/// **Ring wrap: the case the repository has never once executed.**
///
/// Ring-findings §8.3 records that peak `tail` was 9936 bytes of 131072 (7.58%) for a full render,
/// so wrap handling is untested code in Mesa *and* in ours, and §8 says it must be reached
/// deliberately rather than waited for. This test reaches it deliberately.
///
/// The layout is taken straight from Mesa's producer (`vn_ring_write_buffer`, `vn_ring.c:127-142`),
/// which is the only authority that matters:
///
/// ```c
/// const uint32_t offset = ring->cur & ring->buffer_mask;
/// if (offset + size <= ring->buffer_size) { memcpy(buffer + offset, data, size); }
/// else { s = buffer_size - offset;
///        memcpy(buffer + offset, data, s);      /* tail-to-end   */
///        memcpy(buffer, data + s, size - s); }  /* start-to-tail */
/// ring->cur += size;
/// ```
///
/// Two things follow, and both are easy to get backwards:
/// - `cur` (which *is* `tail`) is **free-running**: it is incremented, never masked in storage.
///   The buffer offset is `tail & buffer_mask`, computed only at access time.
/// - A wrap is therefore detected by `offset + size > buffer_size` — **not** by `tail < last_tail`,
///   which under a free-running counter happens only on the 2^32 overflow, once per 4 GiB.
///
/// So a delta straddling the buffer's physical end still has `tail > last_tail`. A watcher using
/// `tail < last_tail` as its wrap test would take the linear slice `ring[BUF+131064 .. BUF+131080]`
/// here, which runs past the end of a 131268-byte ring and panics — the first time the ring wraps,
/// which is to say the first time a real application runs.
#[test]
fn a_delta_straddling_the_buffer_end_is_reassembled_in_producer_order() {
    let mut ring = vec![0u8; RING_SHMEM_SIZE];

    // Park the watcher's frontier 8 bytes short of the buffer's physical end, as if the ring had
    // already carried 131064 bytes of commands.
    let last_tail: u32 = (RING_BUFFER_SIZE - 8) as u32;
    let mut w = RingWatcher::new(1, RING_BUFFER_SIZE as u32);
    write_u32(&mut ring, RING_TAIL_OFFSET, last_tail);
    w.take_delta(&ring);

    // Mesa now writes 16 bytes: 8 land at the end of the buffer, 8 wrap to its start — exactly the
    // two-part memcpy above. The bytes are distinguishable so their *order* is checked, not just
    // their multiset: reassembling the halves backwards would still produce 16 bytes.
    let end_half = [0xA0u8, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7];
    let start_half = [0xB0u8, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7];
    let end_offset = RING_BUFFER_OFFSET + RING_BUFFER_SIZE - 8;
    ring[end_offset..end_offset + 8].copy_from_slice(&end_half);
    ring[RING_BUFFER_OFFSET..RING_BUFFER_OFFSET + 8].copy_from_slice(&start_half);
    // `cur += size` with no masking: 131064 + 16 = 131080, which is *greater* than `last_tail` and
    // greater than `buffer_size`. Both facts are what the naive wrap test gets wrong.
    write_u32(&mut ring, RING_TAIL_OFFSET, last_tail.wrapping_add(16));

    let delta = w.take_delta(&ring).expect("a wrapped delta");
    assert_eq!(
        delta.tail, 131080,
        "tail is free-running, not masked into the buffer"
    );
    let mut expected = end_half.to_vec();
    expected.extend_from_slice(&start_half);
    assert_eq!(
        delta.bytes, expected,
        "a wrapped delta must be reassembled tail-to-end first, then start-to-tail — the order \
         Mesa's two-part memcpy wrote them in"
    );
}

/// The 2^32 counter overflow, which is the *only* thing `tail < last_tail` actually signals.
///
/// After 4 GiB of commands `tail` wraps through zero. The difference `tail - last_tail` stays
/// correct under wrapping arithmetic — this is why Mesa computes occupancy as a difference
/// (`vn_ring_has_space`, `vn_ring.c:213`: `ring->cur + size - head <= ring->buffer_size`) rather
/// than a comparison. A watcher that treated `tail < last_tail` as "the buffer wrapped, emit
/// end-then-start" would here emit a bogus multi-kilobyte delta instead of the 4 bytes actually
/// produced, and every command after it would be garbage.
#[test]
fn the_32_bit_counter_overflow_yields_only_the_bytes_actually_produced() {
    let mut ring = vec![0u8; RING_SHMEM_SIZE];

    // Four bytes short of the u32 ceiling. This is a legal, if rare, steady state — and it is
    // seeded directly rather than drained into, because reaching it through `take_delta` would mean
    // relaying 4 GiB of commands. That is exactly why this arithmetic would otherwise never be
    // exercised until the day it mattered.
    let last_tail: u32 = u32::MAX - 3;
    let mut w = RingWatcher::resuming_at(1, RING_BUFFER_SIZE as u32, last_tail);
    write_u32(&mut ring, RING_TAIL_OFFSET, last_tail);
    assert!(
        w.take_delta(&ring).is_none(),
        "the frontier starts level with tail"
    );

    // Mesa writes 4 bytes; `cur += 4` overflows to 0. `tail (0) < last_tail (4294967292)`.
    let offset = RING_BUFFER_OFFSET + (last_tail as usize & (RING_BUFFER_SIZE - 1));
    ring[offset..offset + 4].copy_from_slice(&[0xC0, 0xC1, 0xC2, 0xC3]);
    write_u32(&mut ring, RING_TAIL_OFFSET, last_tail.wrapping_add(4));

    let delta = w
        .take_delta(&ring)
        .expect("a delta across the counter overflow");
    assert_eq!(delta.tail, 0, "the counter wrapped through zero");
    assert_eq!(
        delta.bytes,
        vec![0xC0, 0xC1, 0xC2, 0xC3],
        "only the 4 bytes Mesa actually produced; `tail < last_tail` means counter overflow, \
         not buffer wrap"
    );
}

/// `advance_head` publishes how much C has relayed, which is what frees ring space for Mesa
/// (`vn_ring_has_space`: `cur + size - head <= buffer_size`). Publishing a head *ahead* of the
/// frontier we actually drained would tell Mesa it may overwrite bytes we have not yet shipped —
/// silent command loss, discovered later as a corrupt stream on S. The guard is wrap-safe, so it
/// keeps working across the 2^32 overflow the test above covers.
#[test]
#[should_panic(expected = "past the frontier")]
fn advancing_head_past_the_relayed_frontier_is_refused() {
    let mut ring = vec![0u8; RING_SHMEM_SIZE];
    write_u32(&mut ring, RING_TAIL_OFFSET, 64);
    let mut w = RingWatcher::new(1, RING_BUFFER_SIZE as u32);
    w.take_delta(&ring).expect("the 64 produced bytes");

    // One byte beyond what was drained: Mesa would be free to overwrite a byte we never relayed.
    w.advance_head(&mut ring, 65);
}

/// The happy path of `advance_head`: the drained frontier is a legal head, and it lands in the
/// `head` control word where Mesa reads it (offset 0x00 — *not* adjacent to `tail`, which sits a
/// full 64-byte cache line away at 0x40; ring-findings §4).
#[test]
fn advancing_head_to_the_relayed_frontier_publishes_it_where_mesa_reads_it() {
    let mut ring = vec![0u8; RING_SHMEM_SIZE];
    write_u32(&mut ring, RING_TAIL_OFFSET, 64);
    let mut w = RingWatcher::new(1, RING_BUFFER_SIZE as u32);
    w.take_delta(&ring).expect("the 64 produced bytes");

    w.advance_head(&mut ring, 64);
    assert_eq!(read_u32(&ring, RING_HEAD_OFFSET), 64);
    // Publishing head must not disturb the client's own write frontier.
    assert_eq!(
        read_u32(&ring, RING_TAIL_OFFSET),
        64,
        "tail is the client's word, not ours"
    );
}

/// Ring identification must pick out the ring **and nothing else** from the blobs a real client
/// allocates.
///
/// The oracle here is not invented: it is the complete blob table a live, rendering Mesa Venus
/// client produced, recorded in ring-findings §6. All six were `HOST3D` and `MAPPABLE`, so nothing
/// in the request's flags distinguishes them — only the size arithmetic and `blob_id` do.
///
/// Both directions matter, and the negative one matters more. A **false positive** points the
/// watcher at the wrong blob, so it relays an application's vertex buffer as though it were a
/// command stream. A **false negative** is worse because it is silent: the watcher finds no ring,
/// relays nothing, and the application hangs on its first synchronous call with no error anywhere.
#[test]
fn ring_identification_picks_the_ring_out_of_a_real_clients_blob_table() {
    // res=1: the command ring. 192 + 131072 + 4. The only one that is a ring.
    assert_eq!(
        RingIdentity::from_blob_request(1, 0, 131268),
        Some(RingIdentity {
            res_id: 1,
            buffer_size: 131072
        }),
        "the 131268-byte blob is the ring: its size decomposes as 192 + 128KiB + 4"
    );

    // Every other blob the same client allocated must be rejected. Sizes and blob_ids verbatim
    // from ring-findings §6's table.
    for (res_id, blob_id, size, what) in [
        (2u32, 0u64, 1048576u64, "the 1 MiB reply arena"),
        (3, 16, 64, "the app's vertex buffer"),
        (4, 0, 8388608, "the 8 MiB command-buffer staging pool"),
        (5, 23, 4096, "an unwritten feedback pool"),
        (6, 18, 16384, "the app's 64x64x4 readback buffer"),
    ] {
        assert_eq!(
            RingIdentity::from_blob_request(res_id, blob_id, size),
            None,
            "{what} (res={res_id}, blob_id={blob_id}, size={size}) is not a ring"
        );
    }
}

/// `blob_id` is load-bearing, not belt-and-braces: an application may legally allocate a buffer
/// whose size decomposes exactly like a ring's. Only `blob_id` separates Venus's own shmems
/// (`== 0`) from an application `VkDeviceMemory` (`!= 0`) — the discrimination ring-findings §6
/// found to be clean. Without this check, a 131268-byte vertex buffer would be watched as a command
/// ring and its contents relayed to S's GPU as Vulkan commands.
#[test]
fn an_application_buffer_that_is_ring_shaped_is_not_mistaken_for_a_ring() {
    assert_eq!(
        RingIdentity::from_blob_request(9, 16, 131268),
        None,
        "a non-zero blob_id marks application memory, whatever its size arithmetic looks like"
    );
}

/// The size arithmetic must reject a blob whose remainder is not a power of two. Mesa asserts the
/// power-of-two property because it is what makes `tail & buffer_mask` a valid substitute for
/// `tail % buffer_size`, so a blob without it cannot be a ring Mesa produced — and pointing a
/// watcher at one would make every buffer offset it computes silently wrong.
#[test]
fn a_blob_whose_remainder_is_not_a_power_of_two_is_not_a_ring() {
    // One byte off the real ring: 131269 - 196 = 131073, not a power of two.
    assert_eq!(RingIdentity::from_blob_request(1, 0, 131269), None);
    // Too small to hold even the control area.
    assert_eq!(RingIdentity::from_blob_request(1, 0, 64), None);
    // Exactly the control area plus extra, with a zero-length buffer.
    assert_eq!(RingIdentity::from_blob_request(1, 0, 196), None);
}

fn write_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn read_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
