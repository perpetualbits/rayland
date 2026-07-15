//! Recognizing **what each of a session's blobs is**: which one is the Venus command ring, and
//! which ones are the application's own memory rather than Venus's internal plumbing.
//!
//! # Why this lives here rather than in either daemon
//! **Both** ends of (c)1 need these answers, and they must agree on them exactly.
//!
//! - `rayland-c` needs [`RingIdentity`] to know which blob's `tail` to poll — the loop that carries
//!   100% of the application's Vulkan commands — and [`is_application_memory`] to know which blobs
//!   to ship to S before a ring delta the GPU may read them from.
//! - `rayland-s` needs [`RingIdentity`] to know how to lay a relayed delta back down (the ring is
//!   circular, and re-wrapping a delta correctly requires the buffer's size), and
//!   [`is_application_memory`] to know which blobs to ship *back* after its GPU has written them.
//!
//! Two copies of either rule would be two chances to disagree, and a disagreement would not surface
//! as an error — it would surface as S writing the application's commands at offsets Mesa never
//! wrote them to, or as one end shipping a blob the other end considers its own to write. So they
//! live once, in the crate that already holds the repository's ring knowledge (and which links no
//! GPU code, so machine C can depend on it).

// The layout constants this recognizer's arithmetic is expressed in terms of, so the two cannot
// drift: `RING_BUFFER_OFFSET` *is* the control area's size, by construction.
use super::RING_BUFFER_OFFSET;

/// The control area's size in bytes: three 64-byte-aligned words (`head`, `tail`, `status`).
///
/// Stated as the *meaning* of [`RING_BUFFER_OFFSET`] for the arithmetic below — the command buffer
/// begins exactly where the control area ends.
const RING_CONTROL_BYTES: u64 = RING_BUFFER_OFFSET as u64;

/// The `extra` region's size in bytes: one dword after the command buffer.
///
/// Ring-findings §4.2 established it is **vestigial** in Mesa 26.0.3 — declared to the host, mapped
/// by the client, and read by nothing (a grep of all 48 files in Mesa's `src/virtio/vulkan/` finds
/// `shared.extra` only at its assignment). It is accounted for here because it is part of the size
/// arithmetic that identifies a ring, not because it is known to be useful.
const RING_EXTRA_BYTES: u64 = 4;

/// Whether a blob holds the **application's own memory** rather than one of Venus's internal
/// shmems.
///
/// # The signal, and why this one line deserves a function
/// Ring-findings §6 measured this and recorded it as *"a usable signal, recorded for (c)1"*:
/// `blob_id == 0` for Venus's internal shmems, `blob_id != 0` for allocations corresponding to a
/// client `VkDeviceMemory`. The live capture's six blobs split cleanly:
///
/// | res | size | `blob_id` | what it is | application's? |
/// |---|---:|---:|---|---|
/// | 1 | 131268 | 0 | the command ring | no |
/// | 2 | 1048576 | 0 | the reply arena | no |
/// | 3 | 64 | 16 | the app's **vertex buffer** | **yes** |
/// | 4 | 8388608 | 0 | command-buffer staging pool | no |
/// | 5 | 4096 | 23 | a `vn_feedback` pool, never written | **yes** |
/// | 6 | 16384 | 18 | the app's **readback buffer** | **yes** |
///
/// It is a function rather than an inlined `blob_id != 0` because it is a *domain claim*, not a
/// comparison: it is the entire basis on which (c)1 decides which memory crosses the network in
/// which direction, and both daemons must apply it identically. Written out at each call site it
/// would read as an arbitrary null check, and the next person to touch it would have no idea that
/// §6's evidence is what makes it true.
///
/// # Why the answer matters so much, in both directions
/// This predicate is what keeps (c)1's *conservative full sync* (spec §7) from being a corruption
/// bug. Each blob has an owner, and shipping one the wrong way clobbers live data:
///
/// - **C→S:** the application writes its vertices into a mapped blob with **no API call to
///   intercept** (ring-findings §6 — the `vkMapMemory` problem), so C must ship them or S's GPU
///   renders from memory the application never wrote. But shipping Venus's shmems the same way
///   would clobber S's reply arena with C's stale copy.
/// - **S→C:** S's GPU writes the rendered pixels into the app's readback blob, so S must ship those
///   back or the application reads zeros. But shipping Venus's shmems back would clobber C's
///   *staging pool* — which C's Mesa is actively recording into and S never writes — with zeros.
///
/// So "everything, both ways" is not conservative, it is wrong. `blob_id` is the only clean line
/// available between the two.
///
/// # Inputs / outputs
/// - `blob_id`: the client-chosen blob id from `VCMD_RESOURCE_CREATE_BLOB`, carried verbatim on the
///   wire in `C2S::CreateBlob`.
/// - Returns `true` if the blob corresponds to a client `VkDeviceMemory` allocation.
///
/// # Pitfall: this does not tell you *which way* a given application blob flows
/// It separates "the app's memory" from "Venus's plumbing". It does **not** say whether the GPU
/// reads or writes a particular application blob — nothing available to (c)1 does, which is exactly
/// why v1 syncs them whole in both directions rather than guessing (spec §7). It also says nothing
/// about which of Venus's internal shmems is the reply arena as against the staging pool; both are
/// `blob_id == 0` and there is no signal here that separates them.
pub fn is_application_memory(blob_id: u64) -> bool {
    // Ring-findings §6: zero marks Venus's own shmems, non-zero a client `VkDeviceMemory`.
    blob_id != 0
}

/// Which blob is the command ring, and how big its buffer is.
///
/// # Why this has to be inferred at all, and the honest status of the inference
/// The ring's layout is **declared in-band**, by the client, in `vkCreateRingMESA`'s
/// `VkRingCreateInfoMESA` — precisely so a host need not know it a priori (ring-findings §4). The
/// rigorous way to obtain this is therefore to parse that command out of the inline
/// `C2S::SubmitCmd` stream, and this module's sibling constants say plainly that a production reader
/// must do exactly that.
///
/// [`Self::from_blob_request`] does **not** do that. It recognizes the ring by the shape of its
/// allocation request, which is a *heuristic* — a good one, and a documented one, but a heuristic.
/// It is recorded as such rather than dressed up, because the day Mesa picks a different buffer size
/// this silently stops finding the ring and (c)1 relays nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingIdentity {
    /// The S-side resource id of the ring blob.
    pub res_id: u32,
    /// The command buffer's size in bytes, derived from the blob's total size.
    pub buffer_size: u32,
}

impl RingIdentity {
    /// Decide whether a blob allocation request describes a Venus command ring, and if so recover
    /// its buffer size.
    ///
    /// # How the recognition works, and why it is trustworthy enough for now
    /// This is the same reasoning that *found* the ring in the first place (ring-findings §4): the
    /// client's first blob was **131268** bytes, and that number is a 128 KiB power-of-two buffer
    /// plus a 196-byte remainder — *a non-power-of-two remainder next to a power-of-two buffer is
    /// what a header looks like*. The decomposition closes exactly:
    ///
    /// ```text
    ///   192 (control) + 131072 (128 KiB command buffer) + 4 (extra) = 131268
    /// ```
    ///
    /// So a request is taken to be the ring when `size - 196` is a non-zero power of two, and when
    /// `blob_id == 0`. The second condition is the discriminator ring-findings §6 found to be clean:
    /// `blob_id == 0` marks Venus's *internal* shmems (ring, reply arena, staging pool), while a
    /// non-zero id marks an application `VkDeviceMemory` allocation. An application is free to
    /// allocate a buffer whose size happens to decompose this way — 131268 bytes of vertex data is
    /// perfectly legal — and `blob_id` is what stops that from being mistaken for a ring.
    ///
    /// Checked against every blob the live capture observed (ring-findings §6), this matches the
    /// instance ring and nothing else *in that capture*: the 1 MiB reply arena, the 8 MiB staging
    /// pool, and the 64/4096/16384 byte application buffers all fail the power-of-two test on
    /// `size - 196`.
    ///
    /// # Pitfall: it is **not** true that only the ring can match — Mesa's TLS ring does too
    /// The capture was single-threaded, and that is the only reason it saw one match. Mesa creates a
    /// per-thread *TLS ring* for synchronous commands (`vn_common.c:322-327`) with `buf_size =
    /// 16 KiB` and `extra_size = 4`, giving a shmem of `192 + 16384 + 4 = 16580` and, like every
    /// Venus-internal shmem, `blob_id == 0`. Both tests below pass: `16580 - 196 = 16384 = 2^14`. So
    /// a second thread issuing a synchronous call produces a blob this function calls a ring.
    ///
    /// Multi-ring support is out of (c)1's scope — the plan pins `VN_PERF=no_multi_ring`, and
    /// `vn_tls_get_ring` honours that by handing back the instance ring instead. **The two callers
    /// handle the hazard differently, and both are right:**
    ///
    /// - On **C**, the watcher follows exactly one ring, so `rayland-c`'s relay engine latches the
    ///   first match and refuses to overwrite it. The instance ring is the one that carries the
    ///   application's drawing and the one Mesa's watchdog reads; repointing the watcher at a 16 KiB
    ///   TLS ring would silently relay nothing the application draws.
    /// - On **S**, every ring delta names its own `ring_res_id`, so there is no ambiguity to
    ///   resolve: `rayland-s` keeps a mirror per ring-shaped blob and lets the message pick.
    ///
    /// # Inputs / outputs
    /// - `res_id`: the S-side resource id assigned to this blob.
    /// - `blob_id`: the client-chosen blob id from the wire message.
    /// - `size`: the blob's total size in bytes.
    /// - Returns `Some(identity)` if this looks like a ring, `None` otherwise.
    ///
    /// # Pitfall: a false negative is silent
    /// If this fails to recognize the real ring, `rayland-c` watches nothing, relays nothing, and
    /// the application hangs on its first synchronous call with no error anywhere. That is why the
    /// daemon logs the identification rather than performing it quietly.
    pub fn from_blob_request(res_id: u32, blob_id: u64, size: u64) -> Option<Self> {
        // Venus's own shmems only. An application buffer that happens to be ring-shaped is not a
        // ring, and `blob_id` is the signal that separates them — expressed through the shared
        // predicate rather than an inlined `!= 0`, so this and (c)1's blob-sync routing can never
        // come to disagree about what a `blob_id` means.
        if is_application_memory(blob_id) {
            return None;
        }
        // Strip the header and the vestigial tail; whatever remains must be the command buffer.
        let buffer = size.checked_sub(RING_CONTROL_BYTES + RING_EXTRA_BYTES)?;
        // The power-of-two property is not decoration: Mesa asserts it, because it is what makes
        // `tail & buffer_mask` a valid substitute for `tail % buffer_size`. A blob whose remainder
        // is not a power of two cannot be a ring Mesa produced.
        if buffer == 0 || !buffer.is_power_of_two() {
            return None;
        }
        // A ring buffer larger than u32 is not something Mesa can address with a 32-bit counter.
        let buffer_size = u32::try_from(buffer).ok()?;
        Some(RingIdentity {
            res_id,
            buffer_size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The blob classification must match what C0 Task 4b actually measured, blob for blob.
    ///
    /// These are the live capture's six blobs (ring-findings §6), with their real sizes and real
    /// `blob_id`s. The table is the whole evidence base for (c)1's decision about which memory
    /// crosses the network in which direction, so it is asserted rather than trusted to a comment:
    /// if `is_application_memory` ever stopped agreeing with it, `rayland-c` would either ship
    /// Venus's plumbing to S (clobbering its reply arena) or stop shipping the application's
    /// vertices (rendering the triangle from memory the application never wrote).
    #[test]
    fn the_captured_blobs_classify_exactly_as_task_4b_measured_them() {
        // Venus's internal shmems: the ring, the reply arena, and the command-buffer staging pool.
        // All `blob_id == 0`, and none of them is the application's to ship.
        for (blob_id, what) in [
            (0u64, "the command ring (131268 B)"),
            (0, "the reply arena (1 MiB)"),
            (0, "the command-buffer staging pool (8 MiB)"),
        ] {
            assert!(
                !is_application_memory(blob_id),
                "{what} is Venus's own plumbing, not the application's memory"
            );
        }

        // The application's `VkDeviceMemory` allocations. `res=3` and `res=6` are the two that make
        // the `vkMapMemory` problem concrete: C0 Task 4b caught the vertex buffer decoding
        // float-for-float and the readback buffer holding the blue clear colour.
        for (blob_id, what) in [
            (16u64, "the app's vertex buffer (res=3, 64 B)"),
            (23, "the app's vn_feedback pool (res=5, 4096 B)"),
            (18, "the app's readback buffer (res=6, 16384 B)"),
        ] {
            assert!(
                is_application_memory(blob_id),
                "{what} is the application's own memory and must cross the network"
            );
        }
    }
}
