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

    /// The doorbell: Mesa's `vkNotifyRingMESA`, hoisted out of the command stream into a
    /// message of its own.
    ///
    /// # Currently unused — an S that waits for this will wait forever
    /// **Nothing constructs this variant.** `rayland-c`'s `RelayEngine::submit` forwards
    /// everything that arrives on the vtest socket as [`C2S::SubmitCmd`], and
    /// `vkNotifyRingMESA` arrives on that socket like any other command — so the doorbell
    /// does reach S, but *inside* `SubmitCmd`, in the Venus command language S's context
    /// decoder already handles. There is deliberately no separate signal.
    ///
    /// It is kept rather than deleted because hoisting the doorbell out may yet earn its
    /// place: recognising it as a distinct event would let a transport prioritise or
    /// coalesce it, at the cost of a decode on C that nothing currently does. Implementing
    /// that is a protocol decision, not an oversight to be quietly filled in — so if you
    /// are here to write S's handler, write the [`C2S::SubmitCmd`] one instead.
    ///
    /// # Do not build a metric on this
    /// **Never** treat a count of doorbells as a measurement of work done, however they
    /// arrive: the ring-findings document (§5.2) measured 1 notification in one run and 4
    /// in another for **byte-identical** ring traffic, because Mesa rings this doorbell
    /// only when it observes the consumer's IDLE bit and a 1 ms throttle has elapsed — the
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

    /// Opaque Wayland wire bytes read off the application's socket — the C-side proxy's
    /// raw forward path for the WP0 Wayland channel (docs/design/2026-07-22-wp0-wayland-
    /// proxy-first-light.md §3). This carries **every** request the app makes except
    /// buffer creation: `wl_surface::commit`, `wl_surface::attach`, `xdg_surface::ack_configure`,
    /// and so on all cross this way, verbatim, for S's Wayland client to replay against the
    /// real compositor.
    ///
    /// # Why file descriptors never appear here
    /// A Wayland wire message can carry an fd out-of-band (`SCM_RIGHTS`), and this variant
    /// cannot carry one — an fd names a resource local to C's kernel and is meaningless once
    /// it crosses a network socket, exactly as C0/(c)1 already found for the Venus ring's
    /// shared-memory blobs. The one request that *would* need to carry an fd —
    /// `zwp_linux_buffer_params_v1`'s buffer-creation request, whose payload is a dma-buf fd
    /// — is therefore never sent as `WaylandData` at all; it is intercepted by the C-side
    /// proxy and sent as [`C2S::WaylandBuffer`] instead, which names the S-side resource by
    /// token rather than by fd.
    WaylandData {
        /// The Wayland wire bytes exactly as the application wrote them, opaque to this
        /// crate — decoding the protocol is the proxy's job, not the relay's.
        bytes: Vec<u8>,
    },

    /// A `zwp_linux_buffer_params_v1` buffer-creation request the C-side proxy intercepted
    /// instead of forwarding as [`C2S::WaylandData`], per the buffer-by-token invariant
    /// (docs/design/2026-07-22-wp0-wayland-proxy-first-light.md §2/§4): **the app's
    /// `wl_buffer` must denote the same pixels as the S-side resource the command relay
    /// already renders into, and no dma-buf fd or pixel data ever crosses the network to
    /// make that true** — only this small typed side-band does.
    WaylandBuffer {
        /// The application's Wayland object id for the `zwp_linux_buffer_params_v1` request
        /// being answered. S's Wayland client needs this only insofar as the C-side proxy
        /// uses it to correlate the eventual result back to the app's own object; it does
        /// not name anything on S.
        params_id: u32,
        /// Names the S-side resource that IS the buffer, plus the geometry S's Wayland
        /// client needs to build a real `wl_buffer` from its own dma-buf export of that
        /// resource. See [`BufferToken`] for the full invariant.
        token: BufferToken,
    },
}

/// Buffer-by-token: names the S-side resource that a Wayland `wl_buffer` denotes, plus the
/// geometry the compositor needs to build a `wl_buffer` from S's own dma-buf export of that
/// resource. No dma-buf fd or pixels cross — the frame is rendered on S and shown on S (see
/// docs/design/2026-07-22-wp0-wayland-proxy-first-light.md §2, §4).
///
/// This is the crux of the WP0 Wayland channel: the mechanism it stands in for is
/// `wsi_common_wayland` wrapping a swapchain image's dma-buf as a `wl_buffer` via
/// `zwp_linux_dmabuf.create_immed`, passing the fd plus this same geometry over the app's
/// Wayland socket. C intercepts that request, correlates the dma-buf fd to the Venus
/// resource id the vtest/ring relay already tracks (the same resource that crossed as a
/// [`C2S::CreateBlob`] and that S already holds a copy of), and sends this struct instead
/// of the fd. S then re-exports its own copy of that resource as a dma-buf and builds a real
/// `wl_buffer` from it — so the frame's pixels never cross the network at all, only the
/// *name* of where they already are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferToken {
    /// The S-side virglrenderer resource id — the swapchain image the command relay renders
    /// into. This is the same id space [`C2S::BlobData`] and [`S2C::BlobCreated`] use for
    /// the vtest/ring relay's other resources; a Wayland buffer is not a new kind of thing
    /// to S, just an existing resource being shown.
    pub resource_id: u32,
    /// Buffer width in pixels, as the app's `zwp_linux_buffer_params_v1::create_immed` request
    /// specified it. S's Wayland client needs this to describe the `wl_buffer` correctly to
    /// the real compositor; it cannot infer it from the resource id alone.
    pub width: u32,
    /// Buffer height in pixels, alongside `width`.
    pub height: u32,
    /// The DRM fourcc format code (e.g. `DRM_FORMAT_ARGB8888`) the app's request specified.
    /// S's dma-buf re-export must advertise the same format or the real compositor will
    /// reject or misinterpret the buffer.
    pub drm_format: u32,
    /// The DRM format modifier (tiling/compression layout) the app's request specified. Like
    /// `drm_format`, this must match on S's re-export: a modifier mismatch is a class of bug
    /// that produces corrupted or garbled pixels rather than a clean failure, because the
    /// consumer decodes the same bytes under the wrong layout.
    pub modifier: u64,
}

/// A contiguous run of bytes at an offset within a blob.
///
/// The unit S's return path speaks in. It exists as a named type because [`S2C::BlobCreated`] must
/// carry *several* of them — see that variant for why a blob's initial contents cannot follow as
/// separate messages — while [`S2C::BlobData`] carries one inline. Both mean the same thing: *these
/// bytes, at this offset, are bytes C does not have.*
///
/// The grain is deliberately the **run**, never the page. Spec §7.2 amended itself to say so during
/// Task 5b: S finds these by comparison, and a comparison is byte-granular for free, so a page-grain
/// run would carry S's stale copy of whatever the application owns in the rest of that page and
/// clobber the app's own fresh writes to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRun {
    /// Byte offset within the blob where `bytes` begins.
    pub offset: u64,
    /// The bytes themselves.
    pub bytes: Vec<u8>,
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

    /// The S-side resource id assigned to a [`C2S::CreateBlob`], **and whatever is already in the
    /// blob**. C records this id and attaches it to all future [`C2S::BlobData`] /
    /// [`C2S::RingDelta`] messages for the same blob so S can find the matching resource.
    ///
    /// # Why the contents ride along instead of following as an [`S2C::BlobData`]
    /// **(c)1 Task 6's finding, and the last thing between a working relay and a blank picture.** A
    /// blob can be born with data already in it: Mesa creates a blob resource lazily, at
    /// `vkMapMemory`, so a readback buffer's blob only comes into existence *after*
    /// `vkCmdCopyImageToBuffer` has already filled it. S's very first sight of those pages is the
    /// finished frame.
    ///
    /// Sending that frame as a following `BlobData` **loses the race, every time**. This reply is
    /// what unblocks C's vtest thread, which immediately hands Mesa the blob's descriptor; Mesa
    /// `mmap`s it and returns the pointer, and the application reads it — all while C's reader thread
    /// is still getting to the next message. The application therefore reads its own untouched zeros
    /// and writes a fully transparent PNG, across a network that carried every command perfectly.
    ///
    /// There is no ordering of two messages that fixes this, because the gap is not between the two
    /// messages — it is between *this* message and Mesa's `mmap`. So the contents must be **in** it:
    /// C applies `initial` and only then answers Mesa, which makes "the descriptor is valid" and
    /// "the descriptor's pages are correct" the same event, exactly as they are on one machine.
    ///
    /// Empty for a genuinely fresh blob, which is the common case — C's shadow is a new zero-filled
    /// memfd and so is S's, so there is nothing to say.
    BlobCreated {
        /// The engine-assigned resource id.
        res_id: u32,
        /// The runs of bytes already present in the blob when S created it, which C must lay into
        /// its shadow **before** letting Mesa map it. Runs, not the whole blob, for the reason
        /// [`S2C::BlobData`] carries an `offset`: S ships the bytes it has, not the pages holding
        /// them.
        initial: Vec<BlobRun>,
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

    /// S has replayed and retired every ring command up to `consumed_tail`. **This is the
    /// message C's local ring `head` is driven from**, and it is on the application's
    /// critical path — it is not a diagnostic.
    ///
    /// # Why this costs a round trip, and why the alternative is a heisenbug
    /// Task 3's brief originally specified that C would advance `head` locally as soon as it
    /// had *relayed* the bytes, avoiding this round trip on the hot path. That is wrong, and
    /// `rayland-c`'s ring watcher deliberately does not do it: `head` is not only "space is
    /// free". Mesa polls it as the **reply-ready signal** (`vn_ring_wait_seqno`,
    /// `vn_ring.c:181-198`), so a locally-forged `head` releases the application's wait
    /// before S has answered and it reads a reply arena that is still zeros. Ring-findings §7
    /// names the ordering constraint — a transport must ship the reply contents before the
    /// head update that releases the wait — and only S knows when that has happened.
    ///
    /// # It is also the progress signal, and the only trustworthy one
    /// `rayland-c`'s stall timeout consults `consumed_tail` to tell "S is slow" from "S has
    /// stopped", the exact distinction the ring-findings document's §5.4 says the engine's
    /// own watchdog cannot make, because Mesa's watchdog reports host liveness, not ring
    /// progress. **A recipient must gate on `consumed_tail` having actually moved**, never on
    /// this message merely arriving: an S that sends these periodically as a keepalive while
    /// wedged would otherwise be indistinguishable from a healthy one, which is the same
    /// footgun in a new place.
    RingProgress {
        /// Which ring this progress report is about.
        ring_res_id: u32,
        /// The highest `tail` value S has fully replayed and retired.
        consumed_tail: u32,
    },

    /// A typed failure on S (e.g. a malformed blob request, an engine error). Sent as a
    /// message rather than simply dropping the connection, so that whatever is driving C
    /// can log something a human can act on instead of just observing a dead socket.
    ///
    /// # Why this carries `solicited`, and why omitting it desynchronized C permanently
    /// C runs a single reader thread that routes S's messages: unsolicited ones (`BlobData`,
    /// `RingProgress`) it handles itself, and everything else it queues for whoever is blocked in a
    /// request/reply. That rule is correct for an error answering a [`C2S::GetCapset`] or a
    /// [`C2S::CreateBlob`] — C really is waiting for it.
    ///
    /// **It is catastrophic for an error refusing a fire-and-forget message.** Most of this protocol
    /// is fire-and-forget, and the dangerous ones come from C's *ring watcher* thread, which never
    /// waits for anything: a [`C2S::RingDelta`], or — since (c)1 Task 5 — a [`C2S::BlobData`] on
    /// every single delta. If S refuses one of those, an error nobody asked for lands in the reply
    /// queue and answers the **next** request instead. Every request after that is then answered by
    /// the previous one's reply, forever: an unbounded desynchronization that surfaces arbitrarily
    /// far from its cause, and one that looks like anything except an error-routing bug.
    ///
    /// S is the only party that can tell the two apart, because S knows which message it was
    /// refusing and C does not — an `Error` carries no reference to what provoked it. So S says so,
    /// and C routes on the answer. The alternative designs were both worse: giving every message a
    /// request id is a protocol C has no other use for, and having S stay silent about
    /// fire-and-forget failures trades a desynchronization for a silent one.
    Error {
        /// A human-readable description of what went wrong on S.
        message: String,
        /// Whether this error answers a message C is **blocked waiting for a reply to** — i.e. a
        /// [`C2S::GetCapset`] or a [`C2S::CreateBlob`].
        ///
        /// `false` for a refusal of any fire-and-forget message. C must **not** route those to its
        /// reply channel; see this variant's docs for the permanent desynchronization that causes.
        solicited: bool,
    },

    /// Opaque Wayland wire bytes — S's Wayland client's raw return path for the WP0 Wayland
    /// channel. This is how the app on C ever learns anything the real compositor said:
    /// `wl_buffer::release`, `xdg_toplevel::configure`, `wl_surface::enter`, and every other
    /// compositor event cross this way, verbatim, for the C-side proxy to hand back to the
    /// app on its own Wayland socket exactly as the app would have received them from a local
    /// compositor.
    ///
    /// Like [`C2S::WaylandData`], this never carries a file descriptor — none of the events a
    /// minimal WSI-driving client needs to relay (per docs/design/2026-07-22-wp0-wayland-
    /// proxy-first-light.md §3) carry one on the wire in the direction compositor → client.
    WaylandData {
        /// The Wayland wire bytes exactly as S's Wayland client received them from the real
        /// compositor, opaque to this crate.
        bytes: Vec<u8>,
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
    fn submit_cmd_round_trips() {
        // A representative C2S::SubmitCmd carrying the real payload from the live capture:
        // `0xbc` = opcode 188 = `vkCreateRingMESA` (ring-findings §3.2). This is the socket's
        // one genuine command and the message that makes S create the ring at all, so it is
        // the variant an S implementor must handle first — `NotifyRing` never arrives.
        let original = C2S::SubmitCmd {
            ctx_id: 1,
            cmd: vec![0xbc, 0, 0, 0],
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

    #[test]
    fn wayland_data_c2s_round_trips() {
        // A representative C2S::WaylandData: opaque bytes off the app's Wayland socket,
        // e.g. a wl_surface::commit request, forwarded verbatim toward S's compositor
        // client.
        let original = C2S::WaylandData {
            bytes: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        assert_eq!(round_trip(&original), original);
    }

    #[test]
    fn wayland_buffer_round_trips() {
        // A representative C2S::WaylandBuffer: the proxy intercepted a
        // zwp_linux_buffer_params_v1 and is naming the S-side resource that IS the buffer
        // (buffer-by-token) instead of relaying the app's dma-buf fd, which cannot cross
        // the network at all.
        let original = C2S::WaylandBuffer {
            params_id: 42,
            token: BufferToken {
                resource_id: 7,
                width: 1920,
                height: 1080,
                drm_format: 0x3432_3241, // DRM_FORMAT_AR24, little-endian fourcc bytes 'A','R','2','4'
                modifier: 0x00ff_ffff_ffff_ffff, // DRM_FORMAT_MOD_INVALID, a representative modifier value
            },
        };
        assert_eq!(round_trip(&original), original);
    }

    #[test]
    fn wayland_data_s2c_round_trips() {
        // A representative S2C::WaylandData: opaque compositor-event bytes (e.g. a
        // wl_buffer::release or xdg_toplevel::configure) to deliver back to the app.
        let original = S2C::WaylandData {
            bytes: vec![9, 8, 7, 6, 5],
        };
        assert_eq!(round_trip(&original), original);
    }
}
