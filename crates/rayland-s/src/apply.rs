//! [`Applier`]: turn the messages C sends into work on S's real GPU, and produce what S owes back.
//!
//! # The shape of this module, and the two things the task's brief got wrong about it
//! The brief specified `pub fn apply(engine: &mut dyn RenderEngine, msg: C2S) -> Result<Vec<S2C>,
//! EngineError>` — a free function over the engine trait. Both halves of that are wrong, and the
//! reasons are worth stating because they are facts about the protocol rather than matters of taste:
//!
//! 1. **It needs state the trait does not expose.** A `C2S::RingDelta` is not handed to the engine
//!    at all — it is written into the ring blob's *memory* (see [`crate::ring_mirror`] for the
//!    source proving it). That needs the blob's mapping and S's ring frontier, neither of which a
//!    `&mut dyn RenderEngine` can produce. Hence a struct that owns them.
//! 2. **It should not return `Result`.** Every failure here becomes an [`S2C::Error`] on the wire,
//!    because C is often blocked in a request/reply waiting for an answer and a dropped error is an
//!    application that hangs with no explanation anywhere. So [`Applier::apply`] is total: it always
//!    returns the messages S owes, and a refusal is one of them. [`Applier::try_apply`] exposes the
//!    typed error underneath for callers (and tests) that want to discriminate.
//!
//! # What S actually does with each message
//! The variants look uniform and are not; sorting them out is most of this module.
//!
//! - **`Hello`** — check the vtest protocol version and refuse a mismatch loudly, which is the whole
//!   reason the message carries it.
//! - **`CreateContext`** — forwarded, and **remembered**: `C2S::CreateBlob` does not carry a
//!   context, and `RenderEngine::create_blob_resource` requires one.
//! - **`GetCapset`** — answered from S's real driver. C has no GPU and cannot invent this.
//! - **`CreateBlob`** — creates the real GPU-backed resource *and* maps its pages, because S must
//!   write into them on the client's behalf (there is no shared page across a network).
//! - **`BlobData`** — copied into those pages. This is how the application's vertex buffer ever
//!   reaches S's GPU (ring-findings §6 caught it as `res=3`, decoding float-for-float). The return
//!   direction is [`Applier::poll_progress`]'s, not this function's, because S's GPU writes those
//!   pages asynchronously and there is no inbound message to answer with them.
//! - **`RingDelta`** — **the payload the whole project is about.** Written into the ring's memory,
//!   never submitted. See [`crate::ring_mirror`].
//! - **`SubmitCmd`** — forwarded to the engine's inline path. Tiny, and indispensable: its one real
//!   command is the `vkCreateRingMESA` that makes S create the ring at all.
//! - **`NotifyRing`** — refused. Nothing constructs it; see the arm.
//! - **`UnrefResource`** — releases the engine's resource, S's mapping, and any ring mirror.
//!
//! # Everything here is remote input
//! `rayland-c` reads from a local Mesa; **`rayland-s` reads from a network**. Every bound in every
//! message is attacker-controlled, and this module is written to that standard: no wire value
//! indexes anything unchecked, no wire length is trusted against a mapping, and no arithmetic on a
//! wire value is done in a width that could truncate before it is checked.

// The engine seam C0 built, and the errors it speaks.
use rayland_vtest::venus_ring::RingIdentity;
use rayland_vtest::{EngineError, RenderEngine};
// The relay protocol.
use rayland_relay::{C2S, S2C};

use crate::blob::{HostBlob, OutOfRange};
use crate::ring_mirror::{RingDeltaError, RingMirror};
use std::collections::HashMap;
// `BlobResource::fd` is an `OwnedFd`; mapping it needs a borrow, and `mmap` keeps its own reference
// to the underlying object, so the fd may be dropped straight afterwards.
use std::os::fd::AsFd;

/// The vtest protocol version S implements.
///
/// `rayland-c`'s local vtest server negotiates this with Mesa and reports it in [`C2S::Hello`], so
/// that a mismatch is refused **at the handshake** rather than surfacing later as bytes decoded
/// under the wrong protocol revision — which would not look like a version problem at all.
pub const SUPPORTED_VTEST_PROTOCOL_VERSION: u32 = 4;

/// Why S refused a message.
///
/// # Why these are typed rather than strings
/// They all end up as an [`S2C::Error`]'s message on the wire, so a string would have been enough
/// for C. They are typed for the two readers that are not C: the tests, which must be able to assert
/// *which* refusal happened rather than grep prose, and a future caller that may want to treat, say,
/// a desynchronized ring (fatal — the session cannot recover) differently from an unknown resource
/// (a bug, but survivable). Collapsing them into `String` would throw that away before anyone could
/// use it.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// C negotiated a vtest protocol version S does not implement.
    #[error(
        "C negotiated vtest protocol version {got} with Mesa, but S implements \
         {SUPPORTED_VTEST_PROTOCOL_VERSION}; refusing rather than misdecoding this session's bytes \
         under the wrong revision"
    )]
    ProtocolVersionMismatch {
        /// The version C reported.
        got: u32,
    },

    /// A blob was requested before any context existed to attach it to.
    #[error(
        "C asked S to create a blob before creating a context; every resource must be attached to \
         one, and C2S::CreateBlob does not carry a context id for S to use"
    )]
    NoContext,

    /// The engine itself failed.
    #[error("S's render engine refused: {0}")]
    Engine(#[from] EngineError),

    /// The engine created a blob but produced no descriptor for it.
    ///
    /// Unreachable with `VirglEngine` (both of its blob paths produce a descriptor or return an
    /// error), but the trait's return type permits it, and S cannot write a blob it cannot map.
    #[error(
        "S's engine created resource {res_id} without a descriptor; S cannot map its pages, and \
         therefore cannot write the client's commands into them"
    )]
    BlobWithoutDescriptor {
        /// The resource the engine created.
        res_id: u32,
    },

    /// A message named a resource S does not have.
    #[error("C sent {message} for resource {res_id}, which S has no blob for")]
    UnknownResource {
        /// The resource id from the wire.
        res_id: u32,
        /// Which message named it, so the log line is actionable.
        message: &'static str,
    },

    /// A ring delta named a resource that exists but is not a ring.
    #[error(
        "C sent a ring delta for resource {res_id}, which S has as a blob but not as a command \
         ring; relaying application commands into, say, the reply arena would corrupt it"
    )]
    NotARing {
        /// The resource id from the wire.
        res_id: u32,
    },

    /// A blob write would land outside the blob.
    #[error("BlobData for resource {res_id}: {source}")]
    BlobWriteOutOfRange {
        /// The resource id from the wire.
        res_id: u32,
        /// What did not fit.
        #[source]
        source: OutOfRange,
    },

    /// A ring delta did not describe something Mesa could have produced. See [`RingDeltaError`].
    #[error("ring delta for resource {res_id} refused: {source}")]
    RingDelta {
        /// The ring the delta was for.
        res_id: u32,
        /// What was wrong with it.
        #[source]
        source: RingDeltaError,
    },

    /// A `C2S::NotifyRing` arrived. Nothing constructs it — see the arm in [`Applier::try_apply`].
    #[error(
        "C sent a NotifyRing doorbell, which nothing in rayland-c constructs: Mesa's \
         vkNotifyRingMESA arrives inside C2S::SubmitCmd, in the command language S's context \
         decoder already handles. Hoisting it out is a protocol decision that has not been made, so \
         an S that acted on this would be acting on a message no C sends"
    )]
    UnexpectedNotifyRing,
}

/// Whether C is **blocked waiting for a reply** to this message, and will therefore route an
/// [`S2C::Error`] about it to whoever is waiting.
///
/// # Why this exists, and why getting it wrong is unbounded rather than annoying
/// C's reader thread routes every message that is not `BlobData`/`RingProgress` to its reply
/// channel. For an error answering a request C is blocked on, that is exactly right. For an error
/// refusing a **fire-and-forget** message — a `RingDelta` or a `BlobData` from C's ring watcher,
/// which waits for nothing — it is a permanent desynchronization: the unasked-for error answers the
/// *next* request, and every request thereafter is answered by the previous one's reply. See
/// [`S2C::Error`]'s docs for the full argument.
///
/// C cannot make this call itself: an `Error` names no message. S can, because S has the message in
/// hand. So the knowledge lives here, at the only place that has it.
///
/// # Inputs / outputs
/// - `msg`: the message S is about to apply (and may refuse).
/// - Returns `true` only for the two messages `rayland-c`'s [`RelayEngine`] genuinely blocks on.
///
/// # Pitfall: this must be kept in step with `RelayEngine`'s request/reply methods
/// It is a claim about **C's** behaviour, asserted on S, and nothing mechanically couples the two.
/// The list is deliberately exhaustive rather than a `_ => false` catch-all: a new C2S variant will
/// fail to compile here, forcing whoever adds it to answer the question rather than inherit a
/// default that might be wrong. `rayland-c`'s `RelayEngine::venus_capset` and
/// `RelayEngine::create_blob_resource` are the only methods that call `request`, i.e. that send and
/// then block.
///
/// [`RelayEngine`]: https://docs.rs/rayland-c
fn message_is_solicited(msg: &C2S) -> bool {
    match msg {
        // The two requests C blocks on. The capset genuinely cannot be answered locally (C has no
        // GPU), and a blob's resource id is assigned by S — C's Mesa is in `recvmsg` waiting.
        C2S::GetCapset { .. } | C2S::CreateBlob { .. } => true,
        // Everything else is fire-and-forget, and an error about any of them must never enter C's
        // reply channel. `RingDelta` and `BlobData` are the dangerous ones in practice: they come
        // from C's ring watcher thread, on the application's hot path, many times a second.
        C2S::Hello { .. }
        | C2S::CreateContext { .. }
        | C2S::BlobData { .. }
        | C2S::RingDelta { .. }
        | C2S::SubmitCmd { .. }
        | C2S::NotifyRing { .. }
        | C2S::UnrefResource { .. } => false,
    }
}

/// S's session state: the blobs it has mapped, the rings it mirrors, and the context it is serving.
///
/// # Why a struct and not a free function
/// See the module docs: a ring delta is written into memory, not passed to the engine, so applying
/// one needs the blob's mapping and S's frontier through that ring. Those have to live somewhere,
/// and the engine trait is deliberately not the place — C0 built that seam to be swappable, and
/// hanging (c)1's relay state off it would fuse the two.
#[derive(Default)]
pub struct Applier {
    /// Every blob S has created and mapped, keyed by the engine's resource id — the same id every
    /// message on the wire names the resource by, so there is no translation table to drift.
    blobs: HashMap<u32, HostBlob>,
    /// A mirror per ring-shaped blob, keyed the same way.
    ///
    /// **A map, not a single latched ring**, deliberately. `rayland-c` latches exactly one because
    /// its watcher can only follow one and must not be repointed at Mesa's 16 KiB TLS ring
    /// (see `RingIdentity`'s docs). S has no such ambiguity: every `C2S::RingDelta` names its own
    /// `ring_res_id`, so S can simply mirror whatever C tells it about and let the message choose.
    rings: HashMap<u32, RingMirror>,
    /// The context C created, remembered because [`C2S::CreateBlob`] does not carry one and
    /// `RenderEngine::create_blob_resource` needs one. `None` until [`C2S::CreateContext`] arrives.
    ctx_id: Option<u32>,
}

impl Applier {
    /// A session with nothing created yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one message from C, returning everything S owes in reply.
    ///
    /// **Total by construction**: a refusal is an [`S2C::Error`] in the returned vector, never a
    /// dropped message. That is not tidiness — C blocks in a request/reply for `Capset` and
    /// `BlobCreated`, so an error S declines to send is an application that hangs forever on an
    /// answer that is never coming. The rendered message is [`ApplyError`]'s own `Display` (i.e.
    /// `e.to_string()`): every source-bearing variant already interpolates its cause into its own
    /// `#[error(...)]` string, so `Display` alone already carries the engine's complaint end to
    /// end — walking `Error::source()` on top of that would repeat, not add, text. See the note on
    /// this module's (removed) `render_error_chain` helper in the (c)1 Task 4 fix-pass report.
    ///
    /// # Inputs / outputs
    /// - `engine`: S's real GPU. Borrowed per call rather than owned so the daemon can keep it on
    ///   one thread while this state is shared with the progress poller.
    /// - `msg`: the message from C. Consumed, because its `Vec<u8>` payloads are moved into S's
    ///   memory rather than copied again.
    /// - Returns the `S2C` messages to send, in order. Frequently empty: most of the protocol is
    ///   fire-and-forget, and [`S2C::RingProgress`] is deliberately *not* produced here for a delta
    ///   S's engine has not consumed yet (see [`Self::poll_progress`]).
    pub fn apply(&mut self, engine: &mut dyn RenderEngine, msg: C2S) -> Vec<S2C> {
        // Decide this **before** `try_apply` consumes the message: only S knows what it was
        // refusing, and an `S2C::Error` carries no reference to what provoked it. See
        // `message_is_solicited` — C's whole reply-routing correctness rests on this bool.
        let solicited = message_is_solicited(&msg);
        match self.try_apply(engine, msg) {
            Ok(out) => out,
            Err(e) => vec![S2C::Error {
                solicited,
                // `ApplyError`'s own `Display` already carries the full story: every
                // source-bearing variant interpolates `{0}`/`{source}` into its own message
                // (and `EngineError`'s own variants do the same one level further down), so a
                // single `to_string()` already reaches the engine's actual complaint. Walking
                // `Error::source()` on top of this, as an earlier version of this function did,
                // would repeat that same text — see review finding 2 in the (c)1 Task 4 fix-pass
                // report for the duplicate (and, for `EngineError::ShmCreateFailed` /
                // `ShmMapFailed`, triplicate) wire message this used to produce.
                message: e.to_string(),
            }],
        }
    }

    /// The typed half of [`Self::apply`].
    ///
    /// Exposed so tests — and any future caller that wants to distinguish a fatal desynchronization
    /// from a survivable one — can see *which* refusal happened rather than parse prose.
    ///
    /// # Failure modes
    /// Every variant of [`ApplyError`]. Nothing here panics on remote input.
    pub fn try_apply(
        &mut self,
        engine: &mut dyn RenderEngine,
        msg: C2S,
    ) -> Result<Vec<S2C>, ApplyError> {
        match msg {
            // The handshake. Refusing a mismatch here is the entire reason the version is on the
            // wire: the alternative is decoding this session's bytes under a revision that does not
            // describe them, which surfaces as anything except a version problem.
            C2S::Hello {
                vtest_protocol_version,
            } => {
                if vtest_protocol_version != SUPPORTED_VTEST_PROTOCOL_VERSION {
                    return Err(ApplyError::ProtocolVersionMismatch {
                        got: vtest_protocol_version,
                    });
                }
                Ok(Vec::new())
            }

            // Create the Venus context, and remember it: `CreateBlob` will need it and does not
            // carry it. Fire-and-forget, mirroring `VCMD_CONTEXT_INIT`'s wire semantics — C does not
            // block on this, so a failure surfaces on the first request that does have a reply.
            C2S::CreateContext { ctx_id } => {
                engine.create_venus_context(ctx_id)?;
                self.ctx_id = Some(ctx_id);
                Ok(Vec::new())
            }

            // The one answer only S can give: the capset comes from S's actual Vulkan driver, and
            // Mesa refuses to initialize without a valid one.
            C2S::GetCapset { version } => {
                let bytes = engine.venus_capset(version)?;
                Ok(vec![S2C::Capset { bytes }])
            }

            // Create the real, GPU-backed blob *and* map it. The mapping is the point: there is no
            // shared page across a network, so S must write the client's bytes into these pages
            // itself.
            C2S::CreateBlob {
                blob_mem,
                blob_flags,
                blob_id,
                size,
            } => {
                let ctx_id = self.ctx_id.ok_or(ApplyError::NoContext)?;
                let blob =
                    engine.create_blob_resource(ctx_id, blob_mem, blob_flags, blob_id, size)?;
                let res_id = blob.resource_id;
                // From this point on the resource genuinely exists inside the engine (and inside
                // virglrenderer's own resource table) even though `Applier` has not recorded it
                // anywhere yet — so every error path below must `unref_resource` before returning,
                // or the resource outlives this refusal with nothing left able to name it. Before
                // this fix, `BlobWithoutDescriptor` and a mapping failure both leaked it (finding 3,
                // (c)1 Task 4 fix-pass): rare in practice (ENOMEM, or an engine that created a
                // resource but produced no descriptor), and the session is usually dead anyway, but
                // it made the comment below false, which this repository treats as a bug.
                //
                // The descriptor is what makes the pages reachable. Without one S holds a resource
                // it can never write, so the application's commands would never arrive — refuse
                // rather than register a blob that is useless by construction.
                let fd = blob.fd.ok_or_else(|| {
                    engine.unref_resource(res_id);
                    ApplyError::BlobWithoutDescriptor { res_id }
                })?;
                // Map before registering anything in `Applier`'s own tables: a mapping failure must
                // leave no half-built state *there*. It must also not leave the engine holding a
                // resource nobody can reach any more, which is why the error path unrefs it. The fd
                // is dropped at the end of this scope either way — `mmap` holds its own reference to
                // the underlying object, so closing it unmaps nothing.
                let host_blob = HostBlob::map(fd.as_fd(), size).map_err(|source| {
                    engine.unref_resource(res_id);
                    ApplyError::from(source)
                })?;
                self.blobs.insert(res_id, host_blob);

                // A ring-shaped blob gets a mirror. Unlike C, S needs no "first match only" rule:
                // every delta names its own ring, so a second ring is simply a second mirror.
                if let Some(identity) = RingIdentity::from_blob_request(res_id, blob_id, size) {
                    self.rings
                        .insert(res_id, RingMirror::new(identity.buffer_size));
                }

                Ok(vec![S2C::BlobCreated { res_id }])
            }

            // The application's own memory, crossing a boundary it was never designed to cross:
            // ring-findings §6 caught the refapp's vertex buffer here, decoding float-for-float.
            C2S::BlobData {
                res_id,
                offset,
                bytes,
            } => {
                let blob = self
                    .blobs
                    .get_mut(&res_id)
                    .ok_or(ApplyError::UnknownResource {
                        res_id,
                        message: "BlobData",
                    })?;
                blob.copy_in(offset, &bytes)
                    .map_err(|source| ApplyError::BlobWriteOutOfRange { res_id, source })?;
                Ok(Vec::new())
            }

            // **The payload the whole project is about.** Written into the ring's memory, where
            // virglrenderer's ring thread polls — never submitted. See `crate::ring_mirror` for the
            // source that settles this.
            C2S::RingDelta {
                ring_res_id,
                tail,
                bytes,
            } => {
                // Two distinct refusals, deliberately: "S has no such resource" and "S has it but it
                // is not a ring" are different bugs on C, and collapsing them would hide which.
                let mirror = match self.rings.get_mut(&ring_res_id) {
                    Some(m) => m,
                    None if self.blobs.contains_key(&ring_res_id) => {
                        return Err(ApplyError::NotARing {
                            res_id: ring_res_id,
                        });
                    }
                    None => {
                        return Err(ApplyError::UnknownResource {
                            res_id: ring_res_id,
                            message: "RingDelta",
                        });
                    }
                };
                // A mirror exists only for a blob that exists, so this cannot be `None`.
                let blob = self
                    .blobs
                    .get_mut(&ring_res_id)
                    .ok_or(ApplyError::UnknownResource {
                        res_id: ring_res_id,
                        message: "RingDelta",
                    })?;

                mirror
                    .apply_delta(blob, tail, &bytes)
                    .map_err(|source| ApplyError::RingDelta {
                        res_id: ring_res_id,
                        source,
                    })?;

                // **No `RingProgress` here, and that is the point.** The ring thread runs
                // asynchronously; at this instant it has almost certainly consumed nothing. Reporting
                // `tail` back would release the application's wait on a reply that does not exist
                // yet. Progress is reported from `poll_progress`, off the `head` the engine actually
                // wrote.
                Ok(Vec::new())
            }

            // The inline path: 140–236 bytes across a whole Vulkan init, all of it ring management
            // (ring-findings §2) — and it carries the `vkCreateRingMESA` that makes S create the
            // ring, so nothing else works without it.
            C2S::SubmitCmd { ctx_id, cmd } => {
                engine.submit(ctx_id, &cmd)?;
                Ok(Vec::new())
            }

            // Nothing in `rayland-c` constructs this: `RelayEngine::submit` forwards everything off
            // the vtest socket as `C2S::SubmitCmd`, and `vkNotifyRingMESA` arrives on that socket
            // like any other command. So a doorbell *does* reach S — inside `SubmitCmd`, in the
            // command language S's context decoder already handles.
            //
            // Refused rather than quietly ignored: receiving one means the peer is not the `rayland-c`
            // this S was built against, and guessing at what it wants is how a protocol drifts.
            C2S::NotifyRing { .. } => Err(ApplyError::UnexpectedNotifyRing),

            // Fire-and-forget, mirroring `VCMD_RESOURCE_UNREF`. Without it every blob C ever created
            // lives in S's resource table for the whole session — a real leak the moment (c)1 runs
            // anything longer than a toy.
            //
            // Order: tell the engine first, then drop S's mapping. The two are independent (S maps
            // the exported descriptor, which the kernel refcounts separately from virglrenderer's own
            // mapping), so this ordering is for clarity rather than safety — but it is the same order
            // `rayland-engine` uses, and matching it costs nothing.
            C2S::UnrefResource { res_id } => {
                engine.unref_resource(res_id);
                self.blobs.remove(&res_id);
                self.rings.remove(&res_id);
                Ok(Vec::new())
            }
        }
    }

    /// Report every ring whose `head` has moved since the last poll.
    ///
    /// # Why this exists at all, and why it cannot be folded into `apply`
    /// **This is the only thing that ever releases the application's synchronous Vulkan calls**, and
    /// it is asynchronous by nature. `apply` cannot produce it: when a `C2S::RingDelta` is written,
    /// virglrenderer's ring thread has not yet run, so there is no progress to report that would be
    /// true. The thread consumes the bytes some time later and stores `head` — with no callback, no
    /// event and nothing to wait on. Somebody has to look.
    ///
    /// So S's daemon polls this, and the consequence is worth being explicit about: **an S that only
    /// ever answered inbound messages would deadlock.** Mesa spins on `head`; `head` only crosses the
    /// network in an `S2C::RingProgress`; and if those were produced only in response to a
    /// `C2S::RingDelta`, then an application blocked on a reply — sending nothing — would never
    /// receive the reply it is blocked on. The poll loop is what breaks that, and it is the exact
    /// mirror of the `tail` poll `rayland-c`'s ring watcher runs for the same reason (ring-findings
    /// §5.2: in the steady state there is **no notification to listen for**, in either direction).
    ///
    /// # This is gated on evidence, and that is deliberate
    /// [`RingMirror::take_progress`] returns a value only when `head` genuinely moved, so a wedged
    /// ring produces silence rather than a stream of reassuring keepalives. That matters: C's stall
    /// detector distinguishes "S is slow" from "S has stopped" purely by whether `consumed_tail`
    /// advances, and ring-findings §5.4 is emphatic that a liveness signal not gated on real progress
    /// is worthless — it is the exact reason virglrenderer's own watchdog cannot detect a stalled
    /// ring.
    ///
    /// # The blob sync rides here, and its order is a correctness property
    /// A ring that moved means S's engine executed commands, and those commands **wrote memory C
    /// cannot see**: the answers to every synchronous Vulkan call, into spec §5's channel 2 — the
    /// reply arena — and the rendered picture, into whatever `HOST_VISIBLE` blob the application
    /// mapped. C0 Task 4b caught the latter concretely: the reference app's readback buffer,
    /// `res=6`, 16384 B = 64×64×4, holding the blue clear colour. On one machine the application
    /// would simply read those pages. Across a network S must copy them out and ship them.
    ///
    /// **Every [`S2C::BlobData`] therefore precedes every [`S2C::RingProgress`] in the returned
    /// list, and that ordering is the point.** `RingProgress` is what advances C's local `head`, and
    /// `head` is the **reply-ready signal**: `vn_ring_get_seqno_status` is
    /// `vn_ring_ge_seqno(ring, vn_ring_load_head(ring), seqno)` (`vn_ring.c:176-179`), which
    /// `vn_ring_wait_seqno` busy-polls. So the progress message *releases the application's wait*.
    /// Sent before the pixels, it releases the application onto memory that is still zeros.
    /// Ring-findings §7 names this exact constraint — *a transport must ship the shmem contents
    /// before it ships the head update that releases the client's wait* — and warns it produces
    /// once-an-hour heisenbugs. Here it would not be once an hour: it is every frame the application
    /// reads back.
    ///
    /// # What crosses, and the rule that decides it (spec §7.2 — read this before changing it)
    /// **S ships back exactly the bytes S wrote.** Not "the application's blobs", not "the blobs the
    /// GPU may have written", and not the pages containing S's writes — the bytes S is *observed* to
    /// have written, found by [`HostBlob::take_bytes_s_wrote`] diffing each blob against a baseline
    /// re-taken every time C's own bytes land in it. Stop asking *"whose memory is this?"* and ask
    /// *"did I write it?"*: on one machine every byte S writes is instantly visible to C, so an
    /// ownership predicate is a *guess* at that relationship while an observed write **is** it.
    ///
    /// The grain is the **byte** and not the page, which §7.2 says explicitly after amending itself
    /// during Task 5b. A page-grain run would carry S's stale copy of whatever the application owns
    /// in the rest of that page — legal for the app to be writing concurrently, since it is
    /// different memory — and C's reader would lay the lot down. `VkDeviceMemory` is page-aligned
    /// and applications suballocate, so that is the whole-blob race again at 4096-byte scale. The
    /// comparison costs the same either way, so the page bought nothing.
    ///
    /// This replaced Task 5's rule, which routed on ring-findings §6's `blob_id` and was wrong twice
    /// over. It never carried the reply arena at all — a `blob_id == 0` shmem, so the predicate
    /// excluded it, and channel 2 crossed nowhere. That did not present as the hang one would
    /// expect: `head` advances from `RingProgress` regardless, so `vn_ring_wait_seqno` returns and
    /// the application is released onto an arena that is still zeros,
    /// `vn_instance_init_renderer_versions` reads `instance_version = 0`, and `vkCreateInstance`
    /// fails. And for the application's own blobs it was a last-writer-wins race: S shipped back
    /// vertex and uniform buffers its GPU had only ever *read*, over the app's fresh writes to them.
    ///
    /// The obvious repair — decode `vkSetReplyCommandStreamMESA` (opcode 178) out of the ring to
    /// learn the arena's `res_id` — is **silently unsound**, and is recorded in spec §7.2 because it
    /// is the attractive answer: 178 is emitted before *every* reply-bearing command
    /// (`vn_ring_submit_command` -> `vn_ring_set_reply_shmem_locked`, `vn_ring.c:711-715`), so all
    /// but the first sit behind a decoder's stop point at the unsizeable `vkCreateInstance`; and
    /// when the 1 MiB reply pool fills, `vn_renderer_shmem_pool_grow_locked`
    /// (`vn_renderer_util.c:70-96`) mints a **new `res_id`**. C0 measured 48820 bytes of reply
    /// traffic, so the reference app never grows the pool — it would pass every test here and
    /// corrupt the first longer session, S shipping a dead arena while the app read a live one.
    ///
    /// Rings are excluded **by `res_id`** below, and that exclusion is structural rather than
    /// heuristic: S already holds `self.rings`, so it costs nothing and cannot be fooled by a number
    /// a remote peer chose.
    ///
    /// # Inputs / outputs
    /// - Returns the runs of bytes S wrote, followed by one [`S2C::RingProgress`] per ring that
    ///   moved. Empty — no blobs, no progress — when nothing moved, which is the overwhelmingly
    ///   common case on a poll loop.
    ///
    /// # Pitfall: one blob can produce many messages, and that is the byte grain's cost
    /// A blob yields one [`S2C::BlobData`] per *run*, and a run breaks wherever a byte S wrote
    /// happens to equal the byte already there. The reference app's first readback — 16 KiB of flat
    /// blue, `00 00 ff ff` per pixel over a zero baseline — is 4096 runs, because the zero R and G
    /// bytes are not bytes S wrote. See [`HostBlob::take_bytes_s_wrote`]: it is a volume cost, it is
    /// deliberately left visible for Task 9 to measure, and the fix is never to merge runs across
    /// bytes S did not write.
    pub fn poll_progress(&mut self) -> Vec<S2C> {
        // Ask the rings first, before copying anything: on the overwhelming majority of polls
        // nothing moved, and shipping a blob per poll regardless would make this loop a bandwidth
        // source rather than the latency mechanism it is meant to be.
        let mut progress = Vec::new();
        // Disjoint field borrows: `rings` mutably (the frontier advances), `blobs` immutably.
        for (&res_id, mirror) in self.rings.iter_mut() {
            let Some(blob) = self.blobs.get(&res_id) else {
                // Unreachable: a mirror is inserted and removed alongside its blob. Skipped rather
                // than asserted because this runs on a poll loop, where a panic would take out the
                // only thing that ever releases the application's waits.
                continue;
            };
            if let Some(consumed_tail) = mirror.take_progress(blob) {
                progress.push(S2C::RingProgress {
                    ring_res_id: res_id,
                    consumed_tail,
                });
            }
        }
        if progress.is_empty() {
            // Nothing retired, so there is nothing S can honestly claim its GPU wrote — and no wait
            // to release, so nothing that needs pixels shipped ahead of it.
            return Vec::new();
        }

        // Something retired, so S's engine has had the chance to write memory C cannot see. v1 does
        // not know — and deliberately does not ask — *which* blobs those commands touched: that
        // would mean decoding the ring, which spec §7 rules out. It asks each blob a question it can
        // answer from bytes alone: which of your bytes did S write? See the rule above.
        let mut out = Vec::new();
        for (&res_id, blob) in self.blobs.iter_mut() {
            // **The ring is excluded structurally, by `res_id`.** S's engine genuinely writes a
            // ring's pages — `vkr_ring_store_head` (`vkr_ring.c:60-67`) stores `head` into them
            // after each dispatched command — so the observed-writes rule would rightly report them,
            // and must not be allowed to: `head` is `RingProgress`'s news to carry, and C's `tail`
            // and command bytes are C's. Shipping a ring back as blob data would overwrite the very
            // commands C is in the middle of relaying with S's copy of them. `self.rings` is S's own
            // record of which blobs it built a mirror for, so this is a fact rather than a guess.
            if self.rings.contains_key(&res_id) {
                continue;
            }
            for run in blob.take_bytes_s_wrote() {
                out.push(S2C::BlobData {
                    res_id,
                    // No longer always 0: `offset` has been on the wire since Task 4 and this is
                    // what it was reserved for — a run starts where S's writes start.
                    offset: run.offset,
                    bytes: run.bytes,
                });
            }
        }
        // **Last, always.** See the ordering argument above: this is what releases the application's
        // wait, and it must not do so before the bytes it is waiting for are on their way.
        out.extend(progress);
        out
    }
}
