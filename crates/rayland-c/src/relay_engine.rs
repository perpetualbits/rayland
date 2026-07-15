//! [`RelayEngine`]: a [`RenderEngine`] whose GPU is another machine.
//!
//! # The trick this module plays, and why it works
//! `rayland-vtest`'s [`serve_vtest`](rayland_vtest::vtest::serve_vtest) drives the [`RenderEngine`]
//! *trait*, never a concrete engine. C0 built that seam so the borrowed C engine could later be
//! Rustified or swapped — the locked decision in CLAUDE.md. (c)1 cashes it in for something nobody
//! anticipated when it was written: **the implementation being swapped in is a network.** A
//! `RelayEngine` that forwards these calls to S is a `RenderEngine`, and the vtest server cannot
//! tell the difference. Neither can Mesa, and neither can the application.
//!
//! # What each method has to do, and why they are not alike
//! The trait's methods look uniform and are not. Sorting them out is most of this module:
//!
//! - **`create_venus_context`** is pure forwarding. C has nothing to do locally.
//! - **`venus_capset`** *must* round-trip. The capset describes what a real Vulkan driver supports,
//!   and Mesa refuses to initialize without a valid one ("no venus capset"). C has no GPU, so it
//!   cannot invent an answer — only S can, from its actual driver.
//! - **`create_blob_resource`** does both. It allocates the **local** memfd shadow first, because
//!   Mesa is blocked in `recvmsg` waiting for a descriptor and will hang forever without one; then
//!   it asks S for the real GPU-backed counterpart. The two allocations are deliberately different
//!   memory — see [`crate::shm`].
//! - **`submit`** forwards the *inline* vtest command path. Ring-findings §2 measured this at
//!   140–236 bytes for a complete Vulkan init, all of it ring management and none of it application
//!   drawing. It is tiny and it is indispensable: the one real command it carries is
//!   `vkCreateRingMESA`, which is what creates the ring on S.
//! - **`read_back`** and **`create_resource`** refuse, in type. See their doc comments.
//!
//! # The pitfall that shapes the whole module: `submit` is not the data path
//! Everything above is the *small* half of (c)1. C0's central finding is that
//! [`RenderEngine::submit`] — the path the vtest socket feeds, and the only path this trait exposes
//! — **never sees a single application Vulkan command** (ring-findings §2). It sees the ring's
//! address, then a series of pokes. The application's actual drawing travels through shared memory
//! that no trait method is ever called for, which is why [`crate::ring`] exists and runs on its own
//! thread. A reader who assumes this module is where the commands are will be looking in the wrong
//! place, so it is said here plainly.

// The ring recognizer, and the blob shadow this engine allocates for every resource Mesa asks for.
use crate::ring::RingIdentity;
use crate::shm::LocalBlob;
// The relay message set and the two engine-facing types the trait speaks in.
use rayland_relay::{C2S, S2C};
use rayland_vtest::{BlobResource, EngineError, EngineFrame, RenderEngine};
// Blob shadows are addressed by the resource id S assigns them.
use std::collections::HashMap;
// The blob table and the ring's identity are read by the daemon's other threads; see `BlobTable`.
use std::sync::{Arc, Mutex};

/// The live blob shadows, keyed by the resource id **S** assigned.
///
/// # Why this is shared rather than owned by the engine
/// Three threads in the daemon need these pages, and the split is not arbitrary:
/// - the **vtest thread** creates and destroys them, as Mesa asks;
/// - the **reader thread** writes [`S2C::BlobData`] into them — this is how a reply S produced
///   reaches the application at all, and ring-findings §7 measured the reply arena at ~12x the
///   command traffic, so it is the *bulk* of the session, not an edge case;
/// - the **ring watcher** reads the ring's pages out of it on every poll.
///
/// Keyed by S's id rather than a locally invented one deliberately: that id is what every message
/// on the wire uses to name the resource, so making it the key means there is no translation table
/// that can drift out of step with the wire.
///
/// # Lock discipline
/// Hold this lock for the shortest possible time and **never across a network send**. The ring
/// watcher polls it continuously; blocking it behind a slow socket write would stall the very loop
/// whose job is to notice the application's commands promptly.
pub type BlobTable = Arc<Mutex<HashMap<u32, LocalBlob>>>;

/// The identity of the command ring, once one has been recognized, shared with the ring watcher.
///
/// `None` until Mesa allocates its ring, which it does early in initialization. The watcher polls
/// this and starts work when it appears — it cannot be given the ring at construction because the
/// ring does not exist until the application has started running.
pub type RingSlot = Arc<Mutex<Option<RingIdentity>>>;

/// The transport that carries the relay protocol to S.
///
/// # Why this is a trait
/// Two reasons, and the second is the one that matters. The first is testability: a mock link makes
/// every method of [`RelayEngine`] exercisable with no network and no S, which is what lets Task 3
/// be tested at all — Task 5 (S) and Task 6 (QUIC) do not exist yet. The second is that (c)1's
/// transport is genuinely undecided: ring-findings §7 concluded that **latency, not bandwidth, is
/// what will hurt** — the reply arena was ~12x the command traffic and its replies are *round
/// trips* the application blocks on. Whatever answers that is what implements this trait.
///
/// # Contract: `send` and `recv` are not independently safe to interleave
/// [`RelayEngine`] uses this as a strict request/reply channel: it sends, then blocks for the
/// answer. If anything *else* can call `recv` on the same link concurrently, replies land in the
/// wrong caller and the session desynchronizes silently. `main.rs` is where that is arranged (a
/// single reader owns `recv`, and hands this engine its replies through a channel); an implementor
/// of this trait does not have to solve it, but must not assume it has been solved for them.
pub trait RelayLink {
    /// Send one message to S. Returns once the message has been handed to the transport — **not**
    /// once S has acted on it.
    ///
    /// # Failure modes
    /// [`EngineError::RelayLinkFailed`] if the message could not be framed or written (connection
    /// dropped, peer gone).
    fn send(&mut self, m: &C2S) -> Result<(), EngineError>;

    /// Block until the next message from S arrives.
    ///
    /// # Failure modes
    /// [`EngineError::RelayLinkFailed`] if the link failed or S closed the connection. A closed
    /// link is an error rather than an end-of-stream: every caller of this method is waiting for a
    /// specific answer, and "no answer is coming" is a failure for all of them.
    fn recv(&mut self) -> Result<S2C, EngineError>;
}

/// A [`RenderEngine`] that owns no GPU and forwards everything to S.
///
/// Holds the local blob shadows for the whole session: Mesa maps those pages and writes into them,
/// so they must outlive every resource (see [`crate::shm::LocalBlob`] for the lifecycle).
pub struct RelayEngine<T: RelayLink> {
    /// The link to S. Every method here is a request over it, and most are request/reply.
    link: T,
    /// The local shared-memory shadow of every live blob. Shared with the daemon's reader thread
    /// and ring watcher — see [`BlobTable`].
    blobs: BlobTable,
    /// The command ring's identity, published here the moment Mesa allocates it so the ring watcher
    /// can start work. See [`RingSlot`].
    ring: RingSlot,
}

impl<T: RelayLink> RelayEngine<T> {
    /// Create a relay engine over `link`, with fresh, empty shared state.
    ///
    /// The link is assumed to be already connected and to have completed whatever handshake it
    /// needs; this type sends no [`C2S::Hello`] of its own, because the session-level handshake
    /// belongs to whoever owns the connection (`main.rs`), not to the engine that borrows it.
    pub fn new(link: T) -> Self {
        RelayEngine {
            link,
            blobs: Arc::new(Mutex::new(HashMap::new())),
            ring: Arc::new(Mutex::new(None)),
        }
    }

    /// A handle to the blob shadows, for the daemon's reader thread and ring watcher.
    ///
    /// See [`BlobTable`] for the lock discipline; the short version is that the ring watcher polls
    /// this continuously and must never be blocked behind a network send.
    pub fn blobs(&self) -> BlobTable {
        Arc::clone(&self.blobs)
    }

    /// A handle to the command ring's identity, for the ring watcher. `None` until Mesa allocates
    /// its ring. See [`RingSlot`].
    pub fn ring(&self) -> RingSlot {
        Arc::clone(&self.ring)
    }

    /// Send a request and block for its reply, refusing anything that is not the expected answer.
    ///
    /// # Why unexpected messages are an error rather than something to skip past
    /// Skipping a message that does not match leaves the *real* reply queued behind it, so the next
    /// request would be answered by this one's response, and every request after that by the
    /// previous one's — an unbounded desynchronization that surfaces arbitrarily far from its cause.
    /// Failing here names the problem where it happened.
    ///
    /// [`S2C::Error`] is unwrapped into [`EngineError::RelayRemoteError`] so that a failure S
    /// *reported* does not masquerade as a protocol violation: those are different bugs and want
    /// different fixes.
    ///
    /// # Inputs / outputs
    /// - `request`: the message to send.
    /// - `expected`: a name for the reply variant, used only in the error message.
    /// - `extract`: pulls the payload out of the reply, returning `None` if it is the wrong variant.
    /// - Returns the extracted payload.
    fn request<R>(
        &mut self,
        request: &C2S,
        expected: &'static str,
        extract: impl FnOnce(S2C) -> Result<R, S2C>,
    ) -> Result<R, EngineError> {
        self.link.send(request)?;
        let reply = self.link.recv()?;
        // A failure S reported about itself is not a protocol error; report it as what it is.
        if let S2C::Error { message } = reply {
            return Err(EngineError::RelayRemoteError { message });
        }
        extract(reply).map_err(|got| EngineError::RelayUnexpectedReply {
            expected,
            // `{got:?}` rather than a hand-written name: the Debug output carries the payload too,
            // which is what a human debugging a desynchronized session actually needs.
            got: format!("{got:?}"),
        })
    }
}

impl<T: RelayLink> RenderEngine for RelayEngine<T> {
    /// Create the Venus context on S. Fire-and-forget: `VCMD_CONTEXT_INIT` has no vtest reply, so
    /// there is nothing for the application to wait on and no reason to make it pay an RTT here.
    ///
    /// A failure to create the context therefore surfaces later, on the first request that does
    /// have a reply. That is a deliberate trade — see ring-findings §7: latency is what will hurt
    /// Rayland, and adding a round trip to a path the protocol says has none would be paying for
    /// diagnostics on the application's critical path.
    fn create_venus_context(&mut self, ctx_id: u32) -> Result<(), EngineError> {
        self.link.send(&C2S::CreateContext { ctx_id })
    }

    /// Forward an **inline** Venus command batch to S.
    ///
    /// # This is not the application's command stream — see the module docs
    /// It is ring management: one `vkCreateRingMESA` and a handful of `vkNotifyRingMESA` doorbells,
    /// 140–236 bytes across an entire Vulkan initialization (ring-findings §2). The application's
    /// actual drawing never arrives here. It is nonetheless indispensable, because
    /// `vkCreateRingMESA` is what makes S create the ring that everything else depends on.
    ///
    /// Sent as [`C2S::SubmitCmd`], **not** [`C2S::RingDelta`]: S must decode these through its
    /// *context's* decoder (`vkr_context.c:170-173`), not its *ring's* (`vkr_ring.c:220-223`). Same
    /// command language, different decoder instance; routing them into the ring mirror would splice
    /// them into a byte stream they were never part of.
    ///
    /// # Failure modes
    /// [`EngineError::UnalignedCommand`] if `cmd` is not a whole number of dwords — virglrenderer
    /// counts commands in dwords and would reject it on S. Checked here so the error names the byte
    /// length, on the machine where the mistake is visible, rather than arriving as a remote
    /// rejection a network away from its cause.
    fn submit(&mut self, ctx_id: u32, cmd: &[u8]) -> Result<(), EngineError> {
        if cmd.len() % 4 != 0 {
            return Err(EngineError::UnalignedCommand { len: cmd.len() });
        }
        self.link.send(&C2S::SubmitCmd {
            ctx_id,
            cmd: cmd.to_vec(),
        })
    }

    /// Fetch the Venus capability set from S's real driver.
    ///
    /// **This one genuinely cannot be answered locally.** The capset is a
    /// `struct virgl_renderer_capset_venus` describing what the host's actual Vulkan driver
    /// supports, and Mesa refuses to proceed without a valid one. C has no GPU and no driver to ask,
    /// so guessing would mean telling the application it may use capabilities S might not have —
    /// which would fail later, on S, as an unexplainable command rejection. The round trip is the
    /// point.
    fn venus_capset(&mut self, version: u32) -> Result<Vec<u8>, EngineError> {
        self.request(&C2S::GetCapset { version }, "Capset", |reply| match reply {
            S2C::Capset { bytes } => Ok(bytes),
            other => Err(other),
        })
    }

    /// Allocate a blob: a **local** memfd shadow for Mesa to map, and its real GPU-backed
    /// counterpart on S.
    ///
    /// # The order matters, and the reason is a hang
    /// The local allocation happens first. Mesa is blocked in `recvmsg` waiting for a descriptor —
    /// a blob is shared memory, and the client cannot use memory it has no descriptor for — so
    /// every failure path must still be able to answer or explain. Allocating locally first also
    /// means an out-of-memory condition on C is reported as C's error, before a message is sent
    /// that would leave S holding a resource C cannot shadow.
    ///
    /// # What the returned fd is, and what it is emphatically not
    /// It is a descriptor for **C's own memfd**, which Mesa will map and `memcpy` its Vulkan command
    /// stream into. It has nothing to do with S. Ring-findings §2.1 is blunt about why that is the
    /// only option: `SCM_RIGHTS` is a Unix-domain feature that cannot cross a network, and there is
    /// no such thing as a page shared between two machines — the descriptor S's GPU memory would
    /// need simply cannot be produced here. Handing Mesa a local one is not a compromise; it is the
    /// mechanism, and it is what lets a stock, unpatched Mesa work at all.
    ///
    /// # Inputs / outputs
    /// - `ctx_id`: unused locally — S attaches the resource to its own context. Kept because the
    ///   trait declares it and S needs no reminding of a context it created.
    /// - `blob_mem` / `blob_flags` / `blob_id` / `size`: forwarded verbatim from the wire.
    ///   `blob_id != 0` marks an application `VkDeviceMemory` allocation, `0` marks one of Venus's
    ///   internal shmems (ring, reply arena, staging pool) — ring-findings §6 found that
    ///   discrimination to be clean, and it is the best handle available on telling the
    ///   application's memory from the transport's plumbing.
    /// - Returns the resource id S assigned, and C's local descriptor.
    ///
    /// # Failure modes
    /// - [`EngineError::ShmCreateFailed`] / [`EngineError::ShmMapFailed`] — the local shadow could
    ///   not be allocated or mapped.
    /// - [`EngineError::RelayLinkFailed`] / [`EngineError::RelayRemoteError`] /
    ///   [`EngineError::RelayUnexpectedReply`] — S could not create its counterpart.
    fn create_blob_resource(
        &mut self,
        _ctx_id: u32,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
    ) -> Result<BlobResource, EngineError> {
        // Local first: Mesa is already waiting on the descriptor this produces.
        let (blob, fd) = LocalBlob::create(size)?;

        // Now ask S for the real, GPU-backed counterpart. Only S can create it; only C can map it.
        let res_id = self.request(
            &C2S::CreateBlob {
                blob_mem,
                blob_flags,
                blob_id,
                size,
            },
            "BlobCreated",
            |reply| match reply {
                S2C::BlobCreated { res_id } => Ok(res_id),
                other => Err(other),
            },
        )?;

        // Keep the shadow alive under S's id: the mapping must outlive the resource, and every
        // later message names the resource by exactly this id.
        self.blobs
            .lock()
            .expect("the blob table lock is never poisoned")
            .insert(res_id, blob);

        // Only now, with the pages actually in the table, announce the ring to the watcher. The
        // order is load-bearing: the watcher looks the ring up in the blob table the instant it
        // sees an identity here, so publishing first would let it find an id whose shadow does not
        // exist yet. It has nothing to do until the ring appears, and a ring it never learns about
        // is a silent hang rather than an error — see `RingIdentity`.
        if let Some(identity) = RingIdentity::from_blob_request(res_id, blob_id, size) {
            *self
                .ring
                .lock()
                .expect("the ring slot lock is never poisoned") = Some(identity);
        }

        Ok(BlobResource {
            resource_id: res_id,
            // The vtest layer sends this to Mesa over SCM_RIGHTS and then drops it. Dropping it
            // does not unmap anything — the `LocalBlob` above owns the mapping.
            fd: Some(fd),
        })
    }

    /// Release a blob: drop C's local shadow and tell S to drop its resource.
    ///
    /// # Why the local drop happens first
    /// Dropping the shadow `munmap`s the pages. That must not happen while anything is still reading
    /// them — but by the time Mesa sends `VCMD_RESOURCE_UNREF` it has finished with the blob, and
    /// C's ring watcher only ever reads the ring, which is never unref'd mid-session. The ordering
    /// here is nonetheless the safe one: local teardown, then the remote message.
    ///
    /// Mirrors `VCMD_RESOURCE_UNREF`'s fire-and-forget wire semantics: it has no reply and cannot
    /// fail from the caller's point of view, so an id we never created is ignored rather than an
    /// error. A link failure is likewise swallowed — the trait method returns `()`, and the session
    /// is already over if the link is gone. Without this message every blob C ever created would
    /// live in S's resource table for the whole session, which is a real leak the moment (c)1 runs
    /// anything longer than a toy.
    fn unref_resource(&mut self, resource_id: u32) {
        // Unmaps C's pages when it drops. `None` for an unknown id: fire-and-forget, as on the wire.
        self.blobs
            .lock()
            .expect("the blob table lock is never poisoned")
            .remove(&resource_id);
        // Best-effort: there is no reply to wait for and no way to report a failure through `()`.
        let _ = self.link.send(&C2S::UnrefResource {
            res_id: resource_id,
        });
    }

    /// Refused: C has no GPU and never holds rendered pixels.
    ///
    /// See [`EngineError::RelayNoPixelsOnC`] — this is a statement about the architecture, not a
    /// stub. Rendering happens on S because S is where the GPU and the monitor are; there is
    /// nothing on C to read back and there never will be.
    fn read_back(&mut self, resource_id: u32) -> Result<EngineFrame, EngineError> {
        Err(EngineError::RelayNoPixelsOnC { resource_id })
    }

    /// Refused: classic 2D resources have no vtest opcode, and C has no GPU to create one on.
    ///
    /// Unreachable through `serve_vtest` — Mesa's Venus ICD allocates everything as blobs. See
    /// [`EngineError::RelayNoClassicResourceOnC`].
    fn create_resource(
        &mut self,
        ctx_id: u32,
        _width: u32,
        _height: u32,
        _format: u32,
    ) -> Result<u32, EngineError> {
        Err(EngineError::RelayNoClassicResourceOnC { ctx_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // A queue of canned replies, and a log of what was sent.
    use std::collections::VecDeque;

    /// A [`RelayLink`] that answers from a script and records everything sent.
    ///
    /// This is what makes Task 3 testable at all: S (Task 5) and the QUIC transport (Task 6) do not
    /// exist yet, so a mock is the only S there is.
    struct MockLink {
        /// Replies handed out by `recv`, in order.
        replies: VecDeque<S2C>,
        /// Everything `send` was given, for assertions.
        sent: Vec<C2S>,
    }

    impl MockLink {
        fn with_replies(replies: impl IntoIterator<Item = S2C>) -> Self {
            MockLink {
                replies: replies.into_iter().collect(),
                sent: Vec::new(),
            }
        }
    }

    impl RelayLink for MockLink {
        fn send(&mut self, m: &C2S) -> Result<(), EngineError> {
            self.sent.push(m.clone());
            Ok(())
        }
        fn recv(&mut self) -> Result<S2C, EngineError> {
            self.replies
                .pop_front()
                .ok_or_else(|| EngineError::RelayLinkFailed {
                    detail: "the mock link ran out of scripted replies".into(),
                })
        }
    }

    /// A blob request must produce a **usable** descriptor for Mesa, and register the shadow under
    /// the id **S** chose — not a locally invented one.
    ///
    /// The id matters more than it looks: every subsequent message naming this resource uses S's
    /// id, so a local counter that merely happened to agree today would silently address the wrong
    /// resource the moment the two drifted.
    #[test]
    fn creating_a_blob_allocates_a_local_shadow_and_adopts_s_s_resource_id() {
        let link = MockLink::with_replies([S2C::BlobCreated { res_id: 7 }]);
        let mut engine = RelayEngine::new(link);

        let blob = engine
            .create_blob_resource(1, 2, 0, 0, 131268)
            .expect("a blob");

        assert_eq!(
            blob.resource_id, 7,
            "the resource id must be the one S assigned"
        );
        assert!(
            blob.fd.is_some(),
            "Mesa blocks in recvmsg forever without a descriptor"
        );
        // The shadow is registered under S's id and is the size Mesa asked for.
        let blobs = engine.blobs();
        let table = blobs.lock().unwrap();
        assert_eq!(table.get(&7).expect("a shadow under S's id").size(), 131268);
    }

    /// Allocating the ring must publish its identity, because the watcher has no other way to learn
    /// which blob to follow — and a ring it never learns about is a silent hang, not an error.
    ///
    /// The size is the real one from the live capture (ring-findings §4), and the resource id it is
    /// published under must be **S's**, since that is what every `RingDelta` will be addressed to.
    #[test]
    fn allocating_the_ring_publishes_its_identity_for_the_watcher() {
        let link = MockLink::with_replies([S2C::BlobCreated { res_id: 4 }]);
        let mut engine = RelayEngine::new(link);
        let ring = engine.ring();
        assert_eq!(
            *ring.lock().unwrap(),
            None,
            "no ring before Mesa allocates one"
        );

        engine
            .create_blob_resource(1, 2, 0, 0, 131268)
            .expect("the ring blob");

        assert_eq!(
            *ring.lock().unwrap(),
            Some(RingIdentity {
                res_id: 4,
                buffer_size: 131072
            }),
            "the watcher must be told which resource is the ring, under the id S assigned it"
        );
    }

    /// A blob that is not the ring must leave the ring slot alone. Pointing the watcher at, say,
    /// the reply arena would make it relay S's own replies back to S as though they were the
    /// application's commands.
    #[test]
    fn allocating_a_non_ring_blob_does_not_publish_a_ring() {
        // The 1 MiB reply arena from the live capture.
        let link = MockLink::with_replies([S2C::BlobCreated { res_id: 2 }]);
        let mut engine = RelayEngine::new(link);
        let ring = engine.ring();

        engine
            .create_blob_resource(1, 2, 0, 0, 1048576)
            .expect("the reply arena");

        assert_eq!(*ring.lock().unwrap(), None, "the reply arena is not a ring");
    }

    /// The request S sees must carry the blob's fields verbatim. `blob_id` in particular is the
    /// only clean signal separating the application's own memory from Venus's internal plumbing
    /// (ring-findings §6), so corrupting it would destroy information S cannot recover.
    #[test]
    fn a_blob_request_reaches_s_with_its_fields_intact() {
        let link = MockLink::with_replies([S2C::BlobCreated { res_id: 3 }]);
        let mut engine = RelayEngine::new(link);

        // The app's 64-byte vertex buffer from the live capture: blob_id 16, i.e. non-zero, i.e.
        // a real `VkDeviceMemory` rather than one of Venus's own shmems.
        engine
            .create_blob_resource(1, 2, 0, 16, 64)
            .expect("a blob");

        assert_eq!(
            engine.link.sent,
            vec![C2S::CreateBlob {
                blob_mem: 2,
                blob_flags: 0,
                blob_id: 16,
                size: 64
            }]
        );
    }

    /// `submit` must forward inline bytes as [`C2S::SubmitCmd`] — the context-decoder path — and
    /// **never** as [`C2S::RingDelta`].
    ///
    /// The payload here is the real one: `0xbc` = opcode 188 = `vkCreateRingMESA`, the socket's one
    /// genuine command, caught in a live capture (ring-findings §3.2). It is the message that
    /// creates the ring on S. Delivered as a ring delta it would be appended to S's ring mirror —
    /// a byte stream it was never part of — and S would have no ring to create, so nothing the
    /// application ever draws would execute.
    #[test]
    fn submit_forwards_inline_commands_on_the_context_path_not_the_ring_path() {
        let link = MockLink::with_replies([]);
        let mut engine = RelayEngine::new(link);

        engine.submit(1, &[0xbc, 0, 0, 0]).expect("the submit");

        assert_eq!(
            engine.link.sent,
            vec![C2S::SubmitCmd {
                ctx_id: 1,
                cmd: vec![0xbc, 0, 0, 0]
            }],
            "inline vtest commands are decoded by S's context decoder (vkr_context.c:170), not \
             its ring decoder (vkr_ring.c:220); they are not ring bytes and must not be sent as any"
        );
    }

    /// A command buffer that is not a whole number of dwords is refused on C, where the mistake is
    /// visible, rather than being shipped for S to reject a network away from its cause.
    #[test]
    fn submit_refuses_a_command_buffer_that_is_not_a_whole_number_of_dwords() {
        let link = MockLink::with_replies([]);
        let mut engine = RelayEngine::new(link);

        let err = engine.submit(1, &[0xbc, 0, 0]).expect_err("a refusal");
        assert!(matches!(err, EngineError::UnalignedCommand { len: 3 }));
        assert!(
            engine.link.sent.is_empty(),
            "nothing malformed may reach the wire"
        );
    }

    /// The capset must come from S's real driver. C has no GPU and cannot invent one.
    #[test]
    fn the_capset_is_fetched_from_s() {
        let link = MockLink::with_replies([S2C::Capset {
            bytes: vec![1, 2, 3, 4],
        }]);
        let mut engine = RelayEngine::new(link);

        assert_eq!(
            engine.venus_capset(0).expect("the capset"),
            vec![1, 2, 3, 4]
        );
        assert_eq!(engine.link.sent, vec![C2S::GetCapset { version: 0 }]);
    }

    /// A reply that does not answer the request is an error, not something to skip.
    ///
    /// Skipping it would leave the real reply queued, so the *next* request would be answered by
    /// this one's response and every request after that by the previous one's — an unbounded
    /// desynchronization surfacing arbitrarily far from its cause.
    #[test]
    fn a_reply_that_does_not_answer_the_request_is_refused() {
        // A blob id where a capset was due.
        let link = MockLink::with_replies([S2C::BlobCreated { res_id: 1 }]);
        let mut engine = RelayEngine::new(link);

        let err = engine.venus_capset(0).expect_err("a refusal");
        assert!(
            matches!(
                err,
                EngineError::RelayUnexpectedReply {
                    expected: "Capset",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// A failure S reports about itself must surface as S's error, not as a protocol violation:
    /// those are different bugs and want different fixes.
    #[test]
    fn an_error_reported_by_s_is_surfaced_as_such() {
        let link = MockLink::with_replies([S2C::Error {
            message: "no venus capset: this host has no GPU".into(),
        }]);
        let mut engine = RelayEngine::new(link);

        let err = engine.venus_capset(0).expect_err("a refusal");
        match err {
            EngineError::RelayRemoteError { message } => {
                assert!(message.contains("no venus capset"))
            }
            other => panic!("expected S's own error to be surfaced, got {other:?}"),
        }
    }

    /// `read_back` refuses in type. The brief for this task forbids `unimplemented!()` here, and the
    /// reason is worth restating: this is not an omission awaiting an implementation, it is a fact
    /// about the architecture. C never has pixels.
    #[test]
    fn read_back_refuses_because_c_never_has_pixels() {
        let link = MockLink::with_replies([]);
        let mut engine = RelayEngine::new(link);

        let err = engine.read_back(1).expect_err("a refusal");
        assert!(matches!(
            err,
            EngineError::RelayNoPixelsOnC { resource_id: 1 }
        ));
    }

    /// Unref drops C's shadow *and* tells S. Without the message, every blob C ever created would
    /// sit in S's resource table for the whole session.
    #[test]
    fn unref_drops_the_local_shadow_and_tells_s() {
        let link = MockLink::with_replies([S2C::BlobCreated { res_id: 9 }]);
        let mut engine = RelayEngine::new(link);
        let blobs = engine.blobs();
        engine
            .create_blob_resource(1, 2, 0, 0, 4096)
            .expect("a blob");
        assert!(
            blobs.lock().unwrap().contains_key(&9),
            "the shadow exists before the unref"
        );

        engine.unref_resource(9);

        assert!(
            !blobs.lock().unwrap().contains_key(&9),
            "the shadow's mapping must be released"
        );
        assert_eq!(
            engine.link.sent.last(),
            Some(&C2S::UnrefResource { res_id: 9 })
        );
    }
}
