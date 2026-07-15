//! The (c)1 message set: everything that crosses the wire between `rayland-c` and
//! `rayland-s`.
//!
//! The two enums below are a direct translation of the vtest/Venus concepts documented
//! in `docs/design/2026-07-15-venus-ring-findings.md` into messages that *can* cross a
//! network — see the crate-level doc comment (`src/lib.rs`) for why this translation is
//! needed at all (the short version: Venus's real data path is a shared memory page,
//! and shared memory does not survive a network hop).

// serde's derive macros generate the (de)serialization code for our message types.
use serde::{Deserialize, Serialize};

/// Messages travelling **C → S**: the application's side of the conversation.
///
/// `rayland-c` is the weak, possibly headless machine where the actual Vulkan
/// application runs under Mesa's Venus ICD. It has no GPU of its own, so every one of
/// these variants either asks S to do GPU work on its behalf, or hands S bytes that S's
/// copy of the Venus/virglrenderer engine needs in order to replay the application's
/// commands faithfully.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum C2S {
    /// Session opening. `vtest_protocol_version` is whatever version our local vtest
    /// server (the one embedded in `rayland-c`, speaking to the real Mesa Venus ICD)
    /// negotiated with Mesa, so S can reject a mismatch loudly and early rather than
    /// misinterpreting bytes it decodes under a different protocol revision later.
    Hello {
        /// The vtest protocol version C's local Mesa negotiation settled on.
        vtest_protocol_version: u32,
    },

    /// Create the Venus rendering context on S. Mirrors the vtest command
    /// `VCMD_CONTEXT_INIT`: before this arrives, S has no context to attach any
    /// subsequent resource, blob, or ring to.
    CreateContext {
        /// The context id C's local vtest server assigned to this session.
        ctx_id: u32,
    },

    /// The Venus capability set (a versioned, opaque byte blob describing what the
    /// Vulkan ICD may assume the host driver supports) that the client asked for. C
    /// cannot answer this itself — it has no GPU — so S must answer from its own real
    /// driver and send the bytes back in [`S2C::Capset`].
    GetCapset {
        /// The capset version Mesa requested.
        version: u32,
    },

    /// A blob (Venus's term for a chunk of GPU-visible shared memory: the command ring,
    /// the reply arena, a vertex buffer, ...) that the client asked to allocate. **C has
    /// already allocated its own local memfd-backed shadow of this blob** so that Mesa's
    /// `mmap` succeeds locally; this message asks S to create the *real* GPU-backed
    /// resource so virglrenderer has something to read from and write into. The two
    /// allocations are deliberately not the same memory — there is no shared page across
    /// a network — and keeping them synchronised is exactly what [`C2S::BlobData`] and
    /// [`S2C::BlobData`] are for.
    CreateBlob {
        /// Which memory type Venus asked for (mirrors vtest's `VCMD_PARAM_BLOB_MEM`).
        blob_mem: u32,
        /// Blob creation flags (mirrors vtest's `VCMD_PARAM_BLOB_FLAGS`), e.g. mappable.
        blob_flags: u32,
        /// The client-chosen blob id. Non-zero identifies an application `VkDeviceMemory`
        /// allocation; zero identifies Venus's own internal shmems (ring, reply arena,
        /// staging pool) per the ring-findings document's §6 observation.
        blob_id: u64,
        /// The blob size in bytes, as Mesa requested it.
        size: u64,
    },

    /// The contents of a blob that C's mapped memory holds, sent C → S. (c)1 v1 ships
    /// the **entire** blob on every sync (see the C0 spec §7): there is no dirty-range
    /// tracking yet, because Venus gives no API-level signal for exactly which bytes
    /// changed (this is the "no seam to hook" problem the ring-findings document's §5.1
    /// describes; it is deeper than this crate and is future work). `offset` is carried
    /// now, always `0` in v1, so that a later version can ship partial ranges without
    /// changing this message's shape.
    BlobData {
        /// Which S-side resource this data belongs to (the id S returned in
        /// [`S2C::BlobCreated`]).
        res_id: u32,
        /// Byte offset within the blob where `bytes` begins. Always `0` until a future
        /// dirty-range version.
        offset: u64,
        /// The blob bytes being synchronised.
        bytes: Vec<u8>,
    },

    /// New command-ring bytes: everything Mesa wrote into the ring's circular buffer in
    /// the half-open range `[previous_tail, tail)`. **This is the payload the whole
    /// project is about.** The ring-findings document proved that 100% of the
    /// application's Vulkan commands live here, and 0% ever touch the vtest socket; a
    /// working (c)1 transport exists to move exactly these bytes from C to S.
    RingDelta {
        /// Which S-side resource is the command ring (the id S returned for the blob
        /// whose `blob_id == 0` and whose size decomposes as
        /// `192 (control) + buffer_size + 4 (extra)`, per the ring-findings document §4).
        ring_res_id: u32,
        /// The ring's `tail` counter *after* this delta — a free-running byte count, not
        /// a buffer index (see the ring-findings document §4.1 on wraparound). S applies
        /// `bytes` and then advances its own mirror of `tail` to this value.
        tail: u32,
        /// The raw bytes Mesa wrote into `[previous_tail, tail)` of the ring buffer.
        bytes: Vec<u8>,
    },

    /// An **inline** Venus command batch: the bytes that arrived on the vtest socket itself, in a
    /// `VCMD_SUBMIT_CMD2` message, rather than through the command ring.
    ///
    /// # Why this is a separate message from [`C2S::RingDelta`], and why conflating them breaks S
    /// It is tempting to reuse `RingDelta` for these bytes — they are the same Venus command
    /// language, after all (ring-findings §3 proves that twice over). **They must not be**, because
    /// the two paths are consumed by *different decoders on S*. Ring-findings §3.1 pins both call
    /// sites: the ring path is `vkr_ring.c:220-223`, which decodes into the ring's own private
    /// encoder/decoder pair; the inline path is `vkr_context.c:170-173`, which decodes into the
    /// **context's** decoder and is what `virgl_renderer_submit_cmd` reaches. Same language, same
    /// dispatch table, different decoder instance — so routing inline bytes into S's ring mirror
    /// would append them to a byte stream they were never part of and desynchronize it.
    ///
    /// This is not a hypothetical tidiness argument. The socket's *one* real command is
    /// `vkCreateRingMESA` (opcode 188 = `0xbc`), caught in a live `SUBMIT_CMD2` capture that
    /// predates the ring's discovery (ring-findings §3.2) — it is the message that **creates the
    /// ring on S in the first place**. Deliver it as a ring delta and S has no ring to deliver it
    /// to; nothing is ever created, and nothing the application draws is ever executed.
    ///
    /// Ring-findings §2 measured this channel at 140–236 bytes for a complete Vulkan
    /// initialization, against 4024 bytes in the ring: **100% of what crosses here is ring
    /// management, and 0% of it is application drawing.** It is small, and it is indispensable.
    SubmitCmd {
        /// The context these commands target — the context id C's local vtest server assigned, and
        /// the same one [`C2S::CreateContext`] created.
        ctx_id: u32,
        /// The Venus command bytes verbatim, as they arrived in the batch. Length is a multiple of
        /// 4: virglrenderer counts commands in dwords and rejects anything else.
        cmd: Vec<u8>,
    },

    /// The doorbell: Mesa's `vkNotifyRingMESA`. Carried here purely for fidelity with
    /// the vtest protocol S's embedded engine expects to see; S's ring-consuming thread
    /// may equally well notice new work from the arrival of [`C2S::RingDelta`] bytes
    /// themselves. **Never** treat a count of these messages as a measurement of work
    /// done: the ring-findings document (§5.2) measured 1 notification in one run and 4
    /// in another for **byte-identical** ring traffic, because Mesa rings this doorbell
    /// only when its host-side ring-consumer thread has been idle for a while — the
    /// count is a fact about scheduling timing, not about the workload.
    NotifyRing {
        /// Which ring this doorbell is for.
        ring_id: u64,
        /// The `tail` value Mesa observed at the moment it decided to ring the doorbell.
        seqno: u32,
    },

    /// Release a resource S is holding. Mirrors vtest's `VCMD_RESOURCE_UNREF`. Without
    /// this, every blob C ever created would live in S's resource table for the whole
    /// session, which is a real leak once (c)1 runs anything longer than a toy.
    UnrefResource {
        /// The S-side resource id to release.
        res_id: u32,
    },
}

/// Messages travelling **S → C**: the GPU's side of the conversation.
///
/// S is the strong machine: real GPU, real display, a live virglrenderer/Venus engine.
/// Every one of these variants is either an answer S owes C for a `C2S` request, or data
/// S produced that C's local Mesa is waiting to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum S2C {
    /// The real capset bytes, read from S's actual GPU driver, answering a
    /// [`C2S::GetCapset`]. C has no GPU and could not have produced these itself.
    Capset {
        /// The capset bytes, opaque to this crate — Venus itself defines their shape.
        bytes: Vec<u8>,
    },

    /// The S-side resource id assigned to a [`C2S::CreateBlob`]. C records this id and
    /// attaches it to all future [`C2S::BlobData`] / [`C2S::RingDelta`] messages for the
    /// same blob so S can find the matching resource.
    BlobCreated {
        /// The engine-assigned resource id.
        res_id: u32,
    },

    /// The contents of a blob that **S wrote**, sent S → C. This is how C's local Mesa
    /// ever learns anything S's GPU produced: the reply arena a synchronous Vulkan call
    /// blocks on (Mesa spins reading the ring's `head` word until the matching reply
    /// lands here — see the ring-findings document §5.4/§7), and the readback buffer the
    /// GPU renders pixels into. Without this message, the application on C would spin
    /// forever waiting for a reply that never crosses the network, or would never see
    /// its own rendered frame.
    BlobData {
        /// Which C-side blob shadow this data is destined for.
        res_id: u32,
        /// Byte offset within the blob where `bytes` begins.
        offset: u64,
        /// The blob bytes S is handing back.
        bytes: Vec<u8>,
    },

    /// S has replayed and retired every ring command up to `consumed_tail`. This is
    /// **not** how C advances its own local ring `head` (Task 3's design note covers
    /// why C does not need a network round trip on this hot path); it exists purely for
    /// progress *detection* — Task 3's stall timeout consults it to tell "S is slow" from
    /// "S has stopped", the exact distinction the ring-findings document's §5.4 says the
    /// engine's own watchdog cannot make, because Mesa's watchdog reports host liveness,
    /// not ring progress.
    RingProgress {
        /// Which ring this progress report is about.
        ring_res_id: u32,
        /// The highest `tail` value S has fully replayed and retired.
        consumed_tail: u32,
    },

    /// A typed failure on S (e.g. a malformed blob request, an engine error). Sent as a
    /// message rather than simply dropping the connection, so that whatever is driving C
    /// can log something a human can act on instead of just observing a dead socket.
    Error {
        /// A human-readable description of what went wrong on S.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    // Bring C2S/S2C into scope for the tests below.
    use super::*;

    // A round-trip helper mirroring rayland-wire's: serialize with postcard, deserialize,
    // and hand back the result, so each test can assert "what went in comes back out"
    // without going through the framing layer (that is frame.rs's job to test).
    fn round_trip<M: Serialize + serde::de::DeserializeOwned>(message: &M) -> M {
        let bytes =
            postcard::to_stdvec(message).expect("serialization must succeed for a valid message");
        postcard::from_bytes(&bytes)
            .expect("deserialization must succeed for bytes we just produced")
    }

    #[test]
    fn create_blob_round_trips() {
        // A representative C2S::CreateBlob, the message that asks S to allocate the
        // real GPU-backed counterpart of a blob C already shadows locally.
        let original = C2S::CreateBlob {
            blob_mem: 1,
            blob_flags: 0,
            blob_id: 16,
            size: 64,
        };
        assert_eq!(round_trip(&original), original);
    }

    #[test]
    fn ring_progress_round_trips() {
        // A representative S2C::RingProgress, the progress-detection message Task 3's
        // stall timeout will consult.
        let original = S2C::RingProgress {
            ring_res_id: 1,
            consumed_tail: 4024,
        };
        assert_eq!(round_trip(&original), original);
    }
}
