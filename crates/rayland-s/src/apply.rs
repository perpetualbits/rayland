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
// The ring-command decoder: (c)2 reads the app's per-queue `ring_idx` out of its `vkGetDeviceQueue2`,
// finds its `vkQueueSubmit`s to trigger the readback fence, and closes the gate on its `vkDestroyDevice`.
use rayland_vtest::venus_ring::decode::{
    find_destroy_device, find_get_device_queue2, find_queue_submit,
};
use rayland_vtest::venus_ring::{
    RING_BUFFER_OFFSET, RingIdentity, notify_ring_command, ring_handle_from_create,
};
use rayland_vtest::{EngineError, RenderEngine};
// The relay protocol.
use rayland_relay::{BlobRun, C2S, S2C};

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
    /// The blobs C declared as **Venus's own internal shmems** — `blob_id == 0` per ring-findings
    /// §6, which is the ring, the reply arena, and the staging pool. Everything else is an
    /// application `VkDeviceMemory` allocation.
    ///
    /// # Why this exists, when spec §7.2 deliberately stopped recording `blob_id`
    /// It was removed because it decided **what S publishes** — and it is "a number a remote peer
    /// chose, unverified against anything" (see [`HostBlob::map`]). That rule is retired and stays
    /// retired: [`Applier::take_blob_writes`] still ships *exactly the bytes S wrote*, for every
    /// blob, deciding nothing from this set.
    ///
    /// This uses it only to **order** those bytes on the wire, and that is a different bargain:
    ///
    /// - **The need.** The reply arena is the blob whose contents *release the application's wait* —
    ///   Mesa reads its fence reply and then reads its own mapped memory. If it crosses the wire
    ///   ahead of the application's readback blob, C applies the release first and the application
    ///   reads pixels that have not landed yet. That is (c)1's residual stale-frame defect, measured
    ///   at 2 of 120 frames after the GPU barrier removed the other 36
    ///   (`docs/c1-the-network.md` §3.1). The blobs live in a `HashMap`, so before this the order was
    ///   whatever hashing chose — and it chose the reply arena first.
    /// - **The exposure, in full.** A hostile or buggy C can lie about `blob_id`. The worst it
    ///   achieves is a bad *order*: no byte is dropped, no byte is invented, and nothing S owns is
    ///   published. A peer that lies here makes **its own application** read stale frames. That is
    ///   self-harm, not a hole in S — which is exactly what the retired rule could not say, because
    ///   there a lie silently suppressed data S was obliged to send.
    ///
    /// Identifying the arena precisely is not available: spec §7.2 records that decoding
    /// `vkSetReplyCommandStreamMESA` to learn its `res_id` is **silently unsound**, because the reply
    /// pool mints a new id when it grows. So the choice is this coarse split or no ordering at all.
    venus_internal: std::collections::HashSet<u32>,
    /// The context C created, remembered because [`C2S::CreateBlob`] does not carry one and
    /// `RenderEngine::create_blob_resource` needs one. `None` until [`C2S::CreateContext`] arrives.
    ctx_id: Option<u32>,
    /// The Venus ring handle, read out of the `vkCreateRingMESA` that crosses on the inline path.
    ///
    /// # Why S has to know this at all, and why it is one value rather than a map
    /// S rings its own ring's doorbell after every applied delta — the (c)1 Task 6 finding, whose
    /// evidence and ordering contract live in
    /// [`venus_ring::doorbell`](rayland_vtest::venus_ring::doorbell). The doorbell names its ring by
    /// this handle, which is a Mesa pointer value S cannot derive and can only read off the wire.
    ///
    /// It is a single `Option` rather than a `res_id -> handle` map because the handle and the
    /// resource id arrive in **different messages that cannot be correlated without decoding
    /// `VkRingCreateInfoMESA`'s variable-size body** — which is exactly the kind of decode the spec
    /// (§7) tells us not to acquire a taste for. Under (c)1's pinned `VN_PERF=no_multi_ring` (spec
    /// §6) there is exactly one ring, so there is nothing to correlate; [`Self::latch_ring_handle`]
    /// refuses a second rather than silently picking one, so the day that crutch is removed presents
    /// as a named refusal instead of a doorbell delivered to the wrong ring.
    venus_ring_handle: Option<u64>,
    /// **(c)1 Task 9 diagnostic only.** The res_ids S has *ever* been observed to write — the set
    /// [`Self::take_blob_writes`] (and the born-with-contents [`C2S::CreateBlob`] path) has produced a
    /// run for at least once.
    ///
    /// This is the set Probe A (`rayland-s`'s progress loop) re-fingerprints on idle polls to catch
    /// S's GPU still writing after the return path declared the work retired. Restricting the probe to
    /// this set is what makes an idle-poll content change attributable to the **GPU** rather than to
    /// the message thread: the blobs C writes forward (its vertex/uniform/fractal memory) are applied
    /// via [`HostBlob::copy_in`], which re-baselines rather than counting as an S write, so they never
    /// enter this set. A change here, with no new `RingDelta` applied, is therefore a GPU DMA landing
    /// late — the `T2 < T4` the design note predicts. It decides nothing on the wire; it only tells
    /// the probe which blobs are worth watching.
    s_written: std::collections::HashSet<u32>,
    /// **(c)1 Task 9 diagnostic only.** A monotonic count of `C2S::RingDelta` messages applied.
    ///
    /// Probe A reads this to disambiguate the one thing that would otherwise make it lie: a readback
    /// blob whose contents change during an idle poll could be S's GPU finishing the *shipped* frame
    /// late (the `T2 < T4` we are hunting) **or** the *next* frame's readback already landing before
    /// its retirement. The application is synchronous, so between S releasing frame N and the
    /// application submitting frame N+1 there is a long CPU-bound quiet window with no new delta — and
    /// a content change in that window is unambiguously a late write of frame N. This counter is how
    /// the progress loop knows the window is still quiet: if it has not advanced since the frame was
    /// shipped, any change is attributable to the shipped frame. Once it advances, the probe stops
    /// watching that blob until the next ship re-baselines it.
    applied_ring_deltas: u64,
    /// **(c)2 completion barrier.** The application's queue as learned from its `vkGetDeviceQueue2`
    /// on the ring — `None` until that command has been seen and decoded. See [`QueueRegistration`]
    /// and [`Self::retirement_ring_idx`]; the value is what the readback fence must be issued on.
    queue: Option<QueueRegistration>,
    /// **(c)2 completion-fence trigger.** The free-running ring position of the latest `vkQueueSubmit`
    /// on the app's queue, updated in the `C2S::RingDelta` arm as each delta's bytes are scanned. See
    /// [`Self::latest_submit_pos`] for why this is tracked from the linear delta stream (wrap-safe)
    /// rather than by scanning the circular ring buffer (which breaks once the ring wraps).
    latest_submit_pos: Option<u32>,
}

/// What S has learned about the application's queue from its `vkGetDeviceQueue2` command.
///
/// # Why S needs this at all
/// The readback completion fence is only a real GPU barrier when issued on the app's actual per-queue
/// timeline index (`ring_idx`); on the hardcoded `ring_idx = 0` it retires instantly, tied to no GPU
/// work (the stale/torn-readback bug). Mesa hands that index to the host inside `vkGetDeviceQueue2`,
/// so S decodes it from the ring — see `docs/design/2026-07-19-c2-ringidx-decode.md`.
///
/// # Why the end offset, not just the index
/// A fence on a `ring_idx` whose queue is not yet registered on the host is **render-server-fatal**
/// (`sync_queues[ring_idx] == NULL` → the context worker dies → the app `SIGABRT`s). The queue is
/// registered while virglrenderer's ring thread *dispatches* `vkGetDeviceQueue2`, and that thread
/// stores the ring's `head` after each dispatch — so `head >= end_offset` is exactly "the queue is
/// registered". [`Applier::retirement_ring_idx`] gates on it.
#[derive(Debug, Clone, Copy)]
struct QueueRegistration {
    /// The ring resource whose stream carried the `vkGetDeviceQueue2` — the ring whose `head` the
    /// registration gate reads.
    ring_res_id: u32,
    /// The app's real per-queue `ring_idx` (≥ 1), from `VkDeviceQueueTimelineInfoMESA.ringIdx`.
    ring_idx: u32,
    /// The free-running ring position at which the `vkGetDeviceQueue2` command ends. The gate opens
    /// once the ring's `head` reaches it. It is a **free-running** counter, not a masked buffer offset,
    /// so it stays directly comparable with `head` even after the ring wraps (which it does mid-run);
    /// and because `vkGetDeviceQueue2` is emitted during device init at a tiny `tail`, its buffer offset
    /// equalled its free-running position when it was decoded (design doc §2).
    end_offset: u32,
    /// The `VkDevice` handle this queue belongs to. Used to recognise *this* device's
    /// `vkDestroyDevice` and close the gate before its queue is freed — see the `C2S::RingDelta` arm
    /// of [`Applier::apply`], which clears `Applier::queue` on that command.
    device_handle: u64,
    /// The `VkQueue` handle the application submits to. Used to recognise *this* queue's
    /// `vkQueueSubmit` (see [`Applier::latest_submit_pos`] / [`find_queue_submit`]), so the readback fence
    /// fires only after a real submit has crossed the ring — never on a between-deltas transient drain.
    queue_handle: u64,
}

impl Applier {
    /// A session with nothing created yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A blob S currently holds, by resource id — a **read-only** view of its live pages.
    ///
    /// # Why this exists, and why it is deliberately this narrow
    /// (c)1 Task 7 needs it: presentation must read the application's readback buffer out of S's own
    /// mapping, because spec §1 asks for the frame on S's screen and the frame the app's PNG shows
    /// on C to be **two independent paths**. Reading it here — from the pages S's GPU actually wrote
    /// — is what makes them independent; reconstructing it from what `poll_progress` shipped back
    /// would make the window and the PNG two views of the same diff, agreeing even when that diff is
    /// wrong. Task 6 found exactly such a bug, so the distinction is not academic.
    ///
    /// It is a plain lookup rather than an iterator over the table because the caller has the
    /// resource id already — it came out of the [`S2C::BlobCreated`] it is reacting to — and
    /// exposing the whole map would invite a scan, which is how a presentation concern would end up
    /// making decisions about resources it has no business naming.
    ///
    /// # Inputs / outputs
    /// - `res_id`: the engine's resource id, as it appears on the wire.
    /// - Returns `None` if S has no blob by that id. A shared reference, so nothing outside can
    ///   write these pages; [`HostBlob::bytes`] documents the (inherent, pre-existing) raciness of
    ///   *reading* memory a cross-process writer also touches.
    pub fn blob(&self, res_id: u32) -> Option<&HostBlob> {
        self.blobs.get(&res_id)
    }

    /// Record the ring's handle if `cmd` is the `vkCreateRingMESA` that declares it.
    ///
    /// Called for every inline batch. Most are not ring creations and this does nothing; the one
    /// that is happens once per session, before any ring delta can arrive (Mesa cannot write a ring
    /// it has not created).
    ///
    /// # Why a second ring is refused rather than latched
    /// (c)1 pins `VN_PERF=no_multi_ring` (spec §6), under which Mesa runs exactly one ring — so a
    /// second creation means that crutch is gone or was never applied. S cannot tell which ring a
    /// later doorbell should name without correlating handles to resource ids, which it has no way
    /// to do (see [`Self::venus_ring_handle`]). Keeping the first and complaining makes that day
    /// arrive as a named, visible refusal; overwriting would silently start ringing the wrong ring's
    /// doorbell, and the other ring would simply stop making progress with nothing to point at.
    ///
    /// # Inputs / outputs
    /// - `cmd`: an inline command batch as it arrived from C.
    /// - Returns nothing; the handle, if found, is recorded on `self`.
    fn latch_ring_handle(&mut self, cmd: &[u8]) {
        let Some(handle) = ring_handle_from_create(cmd) else {
            // Not a ring creation. By far the common case — this runs on every inline batch.
            return;
        };
        match self.venus_ring_handle {
            None => self.venus_ring_handle = Some(handle),
            // A repeat of the one we have. Harmless and worth no noise.
            Some(existing) if existing == handle => {}
            Some(existing) => {
                eprintln!(
                    "rayland-s: ignoring a second ring creation (handle {handle:#x}); this session \
                     already has ring {existing:#x}. (c)1 supports exactly one ring and pins \
                     VN_PERF=no_multi_ring to guarantee it — without that, S cannot tell which ring \
                     a delta's doorbell should name, and the extra ring will stall."
                );
            }
        }
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
                // Remember which side of the ordering split this blob falls on. `blob_id == 0` is
                // ring-findings §6's marker for Venus's own shmems — the ring, the reply arena, the
                // staging pool — and the reply arena is the one whose bytes release the
                // application's wait, so it must not cross the wire ahead of the application's own
                // memory. See `venus_internal`'s docs for why using this number for *ordering* is
                // sound while using it for *routing* was not.
                if blob_id == 0 {
                    self.venus_internal.insert(res_id);
                }

                // A ring-shaped blob gets a mirror. Unlike C, S needs no "first match only" rule:
                // every delta names its own ring, so a second ring is simply a second mirror.
                if let Some(identity) = RingIdentity::from_blob_request(res_id, blob_id, size) {
                    self.rings
                        .insert(res_id, RingMirror::new(identity.buffer_size));
                }

                // **Ship whatever is already in the blob, right now.** (c)1 Task 6's finding, and the
                // last thing standing between a working relay and a blank picture.
                //
                // [`Self::poll_progress`] is S's return path, and it ships blob bytes only when a
                // ring **retires** — a sound gate for a running application, and a deliberate one
                // (diffing every blob on every 200 µs poll would make that loop a bandwidth source).
                // But it never fires for the bytes that matter most, because **a blob can be born
                // with its contents already in it**: Mesa creates a blob resource lazily, at
                // `vkMapMemory`, so a readback buffer's blob comes into existence *after*
                // `vkCmdCopyImageToBuffer` has already run. S's first sight of those pages is of the
                // finished frame. The application then maps its own copy, reads it, and exits —
                // there is no further ring traffic to trigger a poll, so under the retirement gate
                // alone the frame is simply never sent. The reference app rendered correctly across
                // the network and wrote a fully transparent PNG.
                //
                // So creation is itself an event on the return path, and the same predicate answers
                // it: every byte that differs from the baseline is a byte C has never seen. For a
                // fresh blob that is nothing (C's memfd is zeros too, so the diff is empty and this
                // costs one `memcmp`); for a readback buffer it is the frame.
                //
                // **Carried inside `BlobCreated`, not sent after it.** Two messages cannot work here,
                // and the reason is not the obvious one: it is not that C would drop data for a
                // resource it has no shadow of (it would, but the reader commits the shadow first).
                // It is that `BlobCreated` is what unblocks C's vtest thread, which then hands Mesa
                // the descriptor — and Mesa `mmap`s it and the application reads it while C's reader
                // is still getting to the next message. Riding along makes "you have an id" and "your
                // pages are correct" one event, as they are on one machine. See `S2C::BlobCreated`.
                //
                // `expect` is unreachable: the blob was inserted a few lines above.
                let created = self
                    .blobs
                    .get_mut(&res_id)
                    .expect("the blob was just inserted");
                let initial_runs = created.take_bytes_s_wrote(0);
                // **(c)1 Task 9 diagnostic.** A blob born with contents is one S has "written" (the
                // GPU rendered into it before Mesa's lazy `vkMapMemory` made it a blob), so it joins
                // the S-written set Probe A watches — this is the very first readback buffer, whose
                // first frame the retirement gate would otherwise never re-examine.
                if !initial_runs.is_empty() {
                    self.s_written.insert(res_id);
                }
                let initial = initial_runs
                    .into_iter()
                    .map(|run| BlobRun {
                        offset: run.offset,
                        bytes: run.bytes,
                    })
                    .collect();

                Ok(vec![S2C::BlobCreated { res_id, initial }])
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

                // **T0 — guest/API submission accepted** (design note §7). The application's Vulkan
                // commands for this delta are now in the ring's memory where S's engine will find
                // them; `tail` names the frontier. Stamped here so the offline join can measure
                // T0→T2 (how long S's ring thread took to retire) and place this cycle on the shared
                // clock. Diagnostic only — the `tail` is the natural correlation key, since the
                // `RingProgress` that eventually releases the wait echoes it back as `consumed_tail`.
                self.applied_ring_deltas = self.applied_ring_deltas.wrapping_add(1);
                rayland_relay::trace::emit(
                    "T0",
                    &format!("side=S res={ring_res_id} tail={tail} bytes={}", bytes.len()),
                );

                // **(c)2 completion barrier — the app's queue lifecycle, decoded from the ring.** The
                // readback fence must be issued on the app's real per-queue `ring_idx` (`ring_idx = 0`
                // fences no GPU work), and *only while that queue is alive on the host* — a fence on a
                // freed queue is render-server-fatal. So S watches the queue's lifecycle events: its
                // birth (`vkGetDeviceQueue2`, carrying the `ring_idx`), its death (`vkDestroyDevice`,
                // which closes the gate), and each `vkQueueSubmit` (which arms the fence trigger). All
                // decided **here, in the message thread, as the delta is applied.** See
                // `docs/design/2026-07-19-c2-ringidx-decode.md` §7–§8.
                match self.queue {
                    // Not yet latched: watch for the queue's birth. `vkGetDeviceQueue2` is emitted
                    // during device init at a tiny `tail`, before the ring first wraps, and has a
                    // four-magic-word signature, so scanning the linear buffer `[0, applied_tail)` for
                    // it — once, until latched — is both cheap and false-positive-proof. `buf_end` is
                    // clamped to the mapping so a wrapped/hostile `applied_tail` can only read stale
                    // bytes, never index out of bounds.
                    None => {
                        let buf_end = (RING_BUFFER_OFFSET + mirror.applied_tail() as usize)
                            .min(blob.size() as usize);
                        if buf_end > RING_BUFFER_OFFSET {
                            let stream = &blob.bytes()[RING_BUFFER_OFFSET..buf_end];
                            if let Some(found) = find_get_device_queue2(stream) {
                                // `found.end_offset` is a buffer offset that (pre-wrap) equals the
                                // command's free-running ring position — so it stays comparable against
                                // the free-running `head` in `retirement_ring_idx` for the whole run.
                                self.queue = Some(QueueRegistration {
                                    ring_res_id,
                                    ring_idx: found.ring_idx,
                                    end_offset: found.end_offset as u32,
                                    device_handle: found.device_handle,
                                    queue_handle: found.queue_handle,
                                });
                                eprintln!(
                                    "rayland-s: decoded application queue ring_idx={} (its \
                                     vkGetDeviceQueue2 ends at ring offset {}; the readback fence \
                                     waits for head to reach it before firing)",
                                    found.ring_idx, found.end_offset
                                );
                            }
                        }
                    }
                    // Latched: scan **this delta's bytes** (never the circular buffer) for the queue's
                    // destroy or a new submit. Scanning the delta — linear, un-wrapped, and read once —
                    // rather than re-scanning the whole wrapped buffer every delta is what makes both
                    // wrap-safe *and* keeps their (low-entropy) signatures off a large aliasing surface:
                    // each byte is inspected exactly once, when it first arrives. Deltas end at command
                    // boundaries (fundamental to the relay), so neither command is ever split.
                    Some(q) => {
                        if find_destroy_device(&bytes, q.device_handle).is_some() {
                            // The app destroyed its device: close the gate. From here
                            // `retirement_ring_idx` returns `None`, so no further fence is issued —
                            // exactly the fences that would otherwise hit the freed queue at teardown.
                            self.queue = None;
                            eprintln!(
                                "rayland-s: application destroyed its device (vkDestroyDevice for \
                                 device {}); retiring the readback gate for ring_idx={} so no fence \
                                 can race the queue's destruction",
                                q.device_handle, q.ring_idx
                            );
                        } else if let Some(off) = find_queue_submit(&bytes, q.queue_handle) {
                            // A new frame's submit: record its **free-running** position, which
                            // `progress_thread` compares against the last delivered to fire the fence.
                            // `bytes` spans free-running `[tail - bytes.len(), tail)`.
                            let frontier_before = tail.wrapping_sub(bytes.len() as u32);
                            self.latest_submit_pos = Some(frontier_before.wrapping_add(off as u32));
                        }
                    }
                }

                // **Wake S's ring thread — and do it here, after `apply_delta`, never before.**
                //
                // `apply_delta` has just stored `tail` with `Release`. That ordering is the entire
                // correctness of the doorbell: every interleaving with virglrenderer's park sequence
                // is safe *because* the new `tail` is already visible when the consumer looks, and
                // ringing first reintroduces the lost wakeup this exists to prevent.
                //
                // Without this the application hangs and Mesa aborts at ~3.5 s. The chain — and why
                // shipping S's `status` word back to C cannot fix it, and why a *conditional*
                // doorbell would still hang — is in `venus_ring::doorbell`'s module docs. The short
                // version: virglrenderer's ring thread parks after 1 ms and only `vkNotifyRingMESA`
                // wakes it, but Mesa decides whether to send one by reading **C's** `status` word,
                // which reports C's relay watcher rather than S's consumer.
                //
                // A failure is reported rather than swallowed: this doorbell is the only thing that
                // will ever make these bytes execute, so an error here is the session ending, not a
                // missed optimization.
                if let (Some(handle), Some(ctx_id)) = (self.venus_ring_handle, self.ctx_id) {
                    engine.submit(ctx_id, &notify_ring_command(handle))?;
                }

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
                // Read the ring's handle before forwarding, if this is the command that declares it.
                // This is the only place it is ever stated (`vn_ring.c:366-369`), and S needs it to
                // ring its own doorbell — see `venus_ring::doorbell` for why a host on the far side
                // of a network has to do that at all.
                self.latch_ring_handle(&cmd);
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
                // Forget the send-order classification too. virglrenderer is free to hand the same
                // `res_id` to a later blob, and a leftover entry here would silently sort a fresh
                // application buffer into Venus's group — putting the application's own pixels
                // *after* the reply that releases it, which is the exact defect this classification
                // exists to prevent. Cheap to drop; invisible and sporadic if not.
                self.venus_internal.remove(&res_id);
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
    /// Rings are excluded **by `res_id`** below, keyed off `self.rings` — which is itself populated
    /// by [`RingIdentity::from_blob_request`], a function whose own docs call it a heuristic over
    /// wire-supplied `blob_id` and `size`. So this is **not** immune to a remote peer's numbers in
    /// the sense of using no heuristic at all; overstating that would be its own bug. The genuine
    /// property is narrower and still worth having: this is the *same* set the `RingDelta` write
    /// path already keys on, so there is no second, independent interpretation of "is this a ring?"
    /// for the two to quietly disagree about — and a misclassification here fails **loudly**, as
    /// [`ApplyError::NotARing`] on the very next `RingDelta` for that resource, rather than silently
    /// publishing the wrong memory as a ring's contents. In practice nothing misclassifies: the live
    /// capture's arena (`1048576 - 196` bytes of buffer) and staging pool (`8388608 - 196`) are both
    /// non-powers-of-two, which `from_blob_request` requires a ring's buffer size to be.
    ///
    /// # Inputs / outputs
    /// - Returns the runs of bytes S wrote, followed by one [`S2C::RingProgress`] per ring that
    ///   moved. Empty — no blobs, no progress — when nothing moved, which is the overwhelmingly
    ///   common case on a poll loop.
    ///
    /// # Pitfall: one blob can produce many messages, and that cost is steady-state, not a one-off
    /// A blob yields one [`S2C::BlobData`] per *run*, and a run breaks wherever a byte S wrote
    /// happens to equal the byte already there. The reference app's first readback — 16 KiB of flat
    /// blue, `00 00 ff ff` per pixel over a zero baseline — is 4096 runs. **That is not the worst
    /// case, and it is not a startup-only cost**: a second frame that rewrites every pixel to a
    /// different flat colour fragments into 8192 one-byte runs, because an opaque render keeps the
    /// alpha byte constant across frames, and the two frames' green bytes happen to coincide too —
    /// so the coincidence that fragments the first readback recurs, undiminished, on every
    /// subsequent frame, and scales with resolution. See [`HostBlob::take_bytes_s_wrote`] for the
    /// full argument and the pinned measurement of both frames. It is a volume cost, not a
    /// correctness one, and it is required to be fixed — with a wire change carrying many runs per
    /// message, never by merging runs across bytes S did not write — before this carries any
    /// non-toy workload; Task 9 measures the exact numbers.
    /// The context C created, if any. The progress loop needs it to name the context whose GPU work
    /// must retire before pixels are shipped; see [`Applier::take_ring_progress`].
    pub fn ctx_id(&self) -> Option<u32> {
        self.ctx_id
    }

    /// **(c)2 completion barrier — the readback fence's `ring_idx`, or `None` if it is not yet safe
    /// to fire.**
    ///
    /// The return-path fence is a real GPU-completion barrier only when issued on the application's
    /// actual per-queue `ring_idx`; on `ring_idx = 0` it retires instantly, tied to no GPU work. But
    /// a fence on a `ring_idx` whose queue is **not yet registered** on the host is render-server-
    /// fatal (it permanently kills the context worker → the app `SIGABRT`s). So this returns
    /// `Some(ring_idx)` only when **both** hold:
    ///
    /// 1. the app's `vkGetDeviceQueue2` has been decoded (its `ring_idx` is known — see the
    ///    `C2S::RingDelta` arm of [`Self::apply`]), and
    /// 2. the ring's `head` has reached that command's end offset — i.e. virglrenderer's ring thread
    ///    has dispatched it (`vkr_ring.c:232-233` stores `head` after each dispatch), so
    ///    `sync_queues[ring_idx]` is populated and a fence on it is valid.
    ///
    /// Until both hold it returns `None`, and the caller (`progress_thread`) must **not** issue the
    /// fence — it leaves the delivery pending and retries. In practice the queue is registered during
    /// device init, long before the first readback, so this is already `Some` by the first delivery;
    /// the `None` window is a safety precondition, not a path a healthy session lingers in.
    ///
    /// # Inputs / outputs
    /// - Takes `&self`; reads the ring's `head` (a cheap acquire load — [`RingMirror::head`]).
    /// - Returns `Some(ring_idx)` when the fence is safe to issue on it, else `None`.
    pub fn retirement_ring_idx(&self) -> Option<u32> {
        // No queue decoded yet: nothing to fence on.
        let q = self.queue?;
        // The ring that carried the vkGetDeviceQueue2, and its live pages. Both must exist — the
        // registration was recorded from a delta on this very ring — but look them up rather than
        // assume, since a hostile/buggy C could have unref'd the ring in between.
        let mirror = self.rings.get(&q.ring_res_id)?;
        let blob = self.blobs.get(&q.ring_res_id)?;
        // `head` reaching the command's end offset is exactly "the ring thread dispatched the
        // vkGetDeviceQueue2, so the queue is registered". A plain `>=` is correct because both are
        // **free-running** counters: `head` keeps growing past the buffer size when the ring wraps
        // mid-run, `end_offset` is a fixed early position, and neither approaches the 2^32 counter
        // boundary in a session — so once `head` passes `end_offset` the gate stays open (design doc §2).
        if mirror.head(blob) >= q.end_offset {
            Some(q.ring_idx)
        } else {
            None
        }
    }

    /// **(c)2 completion-fence trigger:** is the application's ring fully **drained** — the host ring
    /// thread's `head` caught up to every byte S has relayed (`head == applied_tail`)?
    ///
    /// # Why this is the trigger, and why it cannot overtake the application's submit
    /// The readback fence must be issued *after* the application's own `vkQueueSubmit` has been
    /// dispatched on the host, or S's fence (which travels the render-server context-op path,
    /// independent of the ring thread) can enqueue its empty submit *ahead* of the application's on the
    /// shared queue and retire against the previous frame — S then ships a torn readback. `head`
    /// reaching `applied_tail` means the ring thread has consumed **everything S wrote**, which
    /// includes that submit, so the submit is already dispatched and a fence issued now lands strictly
    /// after it. This is **content-independent**: unlike watching the readback bytes change, it is
    /// immune to two frames rendering identical pixels and to the timing races of sampling a buffer a
    /// cross-process GPU is writing. The application is synchronous, so a drained ring also means it is
    /// blocked awaiting this frame's result — exactly when the fence should fire.
    ///
    /// The readback DMA itself may still be in flight when this returns true (dispatched ≠ GPU-
    /// complete); the fence the caller then issues is what waits for that completion.
    ///
    /// # Inputs / outputs
    /// - Returns `true` only when a queue is latched and its ring's `head` has reached `applied_tail`.
    ///   `false` while the ring thread is still catching up (a frame's commands are mid-flight) or when
    ///   no queue is latched. Reads `head` with the same acquire load as [`Self::retirement_ring_idx`].
    pub fn queue_ring_drained(&self) -> bool {
        let Some(q) = self.queue else { return false };
        let (Some(mirror), Some(blob)) =
            (self.rings.get(&q.ring_res_id), self.blobs.get(&q.ring_res_id))
        else {
            return false;
        };
        // Equality, not `>=`: `head` can never pass `applied_tail` (the ring thread cannot consume
        // bytes S has not written), so "caught up" is exactly equality. Both are free-running `u32`
        // counters from the same origin; they grow past the buffer size when the ring wraps mid-run
        // (so this must NOT use a masked buffer offset) but never approach the 2^32 wrap in a session,
        // so the equality is exact (design doc §2).
        mirror.head(blob) == mirror.applied_tail()
    }

    /// **(c)2 completion-fence trigger:** the free-running ring position of the **latest**
    /// `vkQueueSubmit` the application has issued on its queue, or `None` if it has issued none yet.
    ///
    /// # Why this is the trigger, with [`Self::queue_ring_drained`]
    /// The readback fence must fire only once a *new* submit (this frame's) has actually crossed the
    /// ring — not merely when the ring is drained, because a synchronous frame's commands arrive in
    /// several deltas and the ring is transiently drained *between* them, before the submit delta lands.
    /// The caller remembers the position it last delivered and fences only when this returns a **larger**
    /// position (a newer submit) that a drained ring proves is dispatched. That is a structural signal —
    /// no timing settle, immune to identical frames and to the between-deltas transient drain.
    ///
    /// # Why free-running, tracked from the delta stream (not a buffer scan)
    /// The value is recorded in the `C2S::RingDelta` arm of [`Self::apply`] by scanning each delta's
    /// **bytes** — which arrive un-wrapped and linear — for the app's queue submit, at the delta's
    /// free-running position. This is deliberately **not** a scan of the ring's circular *buffer*: over
    /// a 120-frame run the ring **does wrap**, after which a buffer offset no longer equals a ring
    /// position and the "newer than last delivered" comparison would break. A free-running position
    /// grows monotonically past the buffer size and never wraps within a session, so the comparison is
    /// always meaningful. (Deltas end at command boundaries — fundamental to the relay working at all —
    /// so a submit is never split across two deltas, and no cross-delta carry is needed.)
    pub fn latest_submit_pos(&self) -> Option<u32> {
        self.latest_submit_pos
    }

    /// **Step 1 of the return path**: which rings retired, and how far.
    ///
    /// # Why the return path is three steps rather than one function
    /// This used to be the first half of a single `poll_progress`, and that shape carried (c)1's
    /// worst defect. The caller must be able to do something *between* learning that the ring
    /// retired and asking the blobs what changed — namely **wait for S's GPU to actually finish the
    /// work that retirement covers** ([`RenderEngine::wait_for_work_retired`]). Inside one function
    /// that barrier would run with the applier lock held, and a pathological fence wait (5 s) would
    /// starve the message thread that feeds the ring, so Mesa's ~3.5 s stall abort would kill the
    /// application. Splitting lets the caller drop the lock across the barrier.
    ///
    /// The order the three steps must run in, and why each is where it is:
    /// 1. **this** — read the rings' frontiers. Cheap; a handful of loads.
    /// 2. **barrier** — `wait_for_work_retired`, *without* this lock held.
    /// 3. [`Applier::take_blob_writes`] — diff, now that the GPU's writes are guaranteed visible.
    ///
    /// Ship step 3's bytes **before** step 1's progress: progress is what releases the application's
    /// wait, so it must not cross the wire ahead of the pixels the application is about to read.
    ///
    /// # Inputs / outputs
    /// - Returns one [`S2C::RingProgress`] per ring that moved since the last call, empty if none.
    ///   **Empty means the caller must do nothing else**: no retirement, so no wait to release and
    ///   nothing S can honestly claim its GPU wrote.
    pub fn take_ring_progress(&mut self) -> Vec<S2C> {
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
                // **T2 — host Vulkan fence/timeline signal** (design note §7): S's ring retired up to
                // `consumed_tail`, which today's code treats as license to diff and ship. The whole
                // §7 question is whether the GPU's readback (T4) is really done by now; this stamp is
                // the T2 the join compares T4-evidence against. Emitted before the barrier runs, so
                // the trace shows the ordering the code *assumes* (T2 then wait then diff) against
                // what Probe A observes.
                rayland_relay::trace::emit(
                    "T2",
                    &format!("side=S res={res_id} tail={consumed_tail}"),
                );
                progress.push(S2C::RingProgress {
                    ring_res_id: res_id,
                    consumed_tail,
                });
            }
        }
        progress
    }

    /// **Step 3 of the return path**: the bytes S is *observed* to have written.
    ///
    /// # The caller MUST have run the barrier first
    /// This diffs each blob against its baseline, and a diff answers *"did these bytes change?"* —
    /// never *"has the GPU finished?"*. Calling it without [`RenderEngine::wait_for_work_retired`]
    /// in between is precisely (c)1's stale-frame defect: the predecessor of these two methods
    /// reasoned that *"something retired, so S's engine has had the chance to write"*, and **"has
    /// had the chance" is not "has"**. Task 9 measured the result — 22 of 120 frames delivered whole
    /// and one frame old, 16 more torn, application exiting 0 throughout
    /// (`docs/c1-the-network.md` §3.1). The ordering is a contract this type cannot enforce, so it
    /// is stated here and obeyed in `rayland-s`'s progress loop.
    ///
    /// v1 does not know — and deliberately does not ask — *which* blobs the retired commands
    /// touched: that would mean decoding the ring, which spec §7 rules out. It asks each blob a
    /// question it can answer from bytes alone; the barrier is what makes that question's answer
    /// mean what the caller needs it to mean.
    ///
    /// # Inputs / outputs
    /// - Returns one [`S2C::BlobData`] per run of bytes S wrote, across every non-ring blob. Empty
    ///   when S wrote nothing since the last call.
    pub fn take_blob_writes(&mut self) -> Vec<S2C> {
        // **The application's own memory ships before Venus's.**
        //
        // The reply arena is a Venus-internal blob, and its bytes are what release the application's
        // wait: Mesa reads its fence reply and then reads its own mapped pages. Ship it first and C
        // applies the release before the pixels, so the application reads a frame that has not
        // arrived — which is not hypothetical, it is the 2-of-120 residue the GPU barrier alone left
        // behind (`docs/c1-the-network.md` §3.1). Before this, the order was a `HashMap`'s, and
        // hashing happened to put the arena first.
        //
        // Within each group the order is still arbitrary and that is fine: two application blobs
        // have no ordering relationship to each other — neither one's arrival releases anything —
        // and the same holds for two of Venus's. The only edge that matters is between the groups.
        //
        // This decides ORDER only, never whether a blob ships: every blob below is asked the same
        // question and every answer is sent. See `venus_internal` for why that distinction is what
        // makes leaning on a peer-supplied `blob_id` acceptable here and unacceptable in the rule
        // spec §7.2 retired.
        let mut res_ids: Vec<u32> = self.blobs.keys().copied().collect();
        res_ids.sort_by_key(|res_id| {
            (
                // false (0) sorts first: application memory leads.
                self.venus_internal.contains(res_id),
                // Then by id, purely so a run is reproducible rather than hash-ordered. Nothing
                // depends on this and it costs nothing to have.
                *res_id,
            )
        });
        self.emit_blob_writes(&res_ids, 0)
    }

    /// Emit one `S2C::BlobData` per run of bytes S wrote, for the blobs named by `res_ids`, in the
    /// order given.
    ///
    /// # Why this exists
    /// [`Self::take_blob_writes`], [`Self::take_venus_blob_writes`] and [`Self::take_app_blob_writes`]
    /// differ only in *which* blobs they visit and *in what order* — the per-blob diff-and-emit logic
    /// (ring exclusion, the `take_bytes_s_wrote` call, the T5 trace, folding into `s_written`) is
    /// identical across all three, so it lives here once rather than three times where it could drift.
    ///
    /// Rings are never passed in by any of the three callers — a ring's pages are C's command bytes
    /// and S's `head`, not S's writes to return (see [`Self::take_blob_writes`]'s docs on why the
    /// ring is excluded structurally rather than by a guess about its bytes). This function still
    /// guards against one anyway, at no real cost, so a caller mistake fails by omission rather than
    /// by corrupting the ring.
    ///
    /// # Inputs / outputs
    /// - `res_ids`: which blobs to diff, and in what order the resulting messages appear. A blob that
    ///   has since been unref'd (a race between listing it and this call, or a caller composing a
    ///   stale list) is skipped rather than panicked on — this is called from a poll loop, where a
    ///   panic would take out the only thing that ever releases the application's waits.
    /// - Returns one [`S2C::BlobData`] per run of bytes S wrote, across the named blobs, in the order
    ///   given. Empty if none of them had anything to say.
    fn emit_blob_writes(&mut self, res_ids: &[u32], coalesce_gap: usize) -> Vec<S2C> {
        let mut out = Vec::new();
        // **(c)1 Task 9 diagnostic.** The res_ids that produced a run on *this* call, folded into
        // `self.s_written` after the loop rather than during it — inserting mid-loop would need a
        // second mutable borrow of `self` while `blob` still borrows `self.blobs`.
        let mut wrote_this_call: Vec<u32> = Vec::new();
        for &res_id in res_ids {
            // **The ring is excluded structurally, by `res_id`.** S's engine genuinely writes a
            // ring's pages — `vkr_ring_store_head` (`vkr_ring.c:60-67`) stores `head` into them
            // after each dispatched command — so the observed-writes rule would rightly report them,
            // and must not be allowed to: `head` is `RingProgress`'s news to carry, and C's `tail`
            // and command bytes are C's. Shipping a ring back as blob data would overwrite the very
            // commands C is in the middle of relaying with S's copy of them. `self.rings` is S's own
            // record of which blobs it built a mirror for, so this is a fact rather than a guess.
            // Checked before the `get_mut` below so the two field borrows never overlap.
            if self.rings.contains_key(&res_id) {
                continue;
            }
            // A blob may have been unref'd between the caller listing it and this loop reaching it —
            // skip rather than panic on what is, in production, a poll loop.
            let Some(blob) = self.blobs.get_mut(&res_id) else {
                continue;
            };
            let runs = blob.take_bytes_s_wrote(coalesce_gap);
            if runs.is_empty() {
                continue;
            }

            // **T5 — first changed byte observed in mapped host memory** (design note §7). This is
            // the moment the return path *first sees* S's write for this cycle; the whole §7 question
            // is whether the GPU had actually finished (T4) by now. The fingerprint is of the blob's
            // current contents, and becomes Probe A's baseline: if it later changes while the
            // application is blocked and no new `RingDelta` has arrived, the GPU was still writing
            // past this point — the `T2 < T4` the note predicts. Guarded on `enabled()` so the
            // ~1 MiB strided hash is never paid when tracing is off.
            if rayland_relay::trace::enabled() {
                let fp = rayland_relay::trace::fingerprint(blob.bytes());
                rayland_relay::trace::emit(
                    "T5",
                    &format!(
                        "side=S res={res_id} off={} runs={} fp={fp:#x}",
                        runs[0].offset,
                        runs.len()
                    ),
                );
            }
            wrote_this_call.push(res_id);

            for run in runs {
                out.push(S2C::BlobData {
                    res_id,
                    // No longer always 0: `offset` has been on the wire since Task 4 and this is
                    // what it was reserved for — a run starts where S's writes start.
                    offset: run.offset,
                    bytes: run.bytes,
                });
            }
        }
        // Record the S-written set for Probe A. A blob only ever joins this set — once S has written
        // a resource, it is a GPU-write target forever, and the probe wants to keep watching it.
        for res_id in wrote_this_call {
            self.s_written.insert(res_id);
        }
        out
    }

    /// **Return path, retirement half:** the Venus-internal blob writes — the reply arena, whose bytes
    /// answer the application's non-readback synchronous calls and are needed for its forward progress.
    ///
    /// # Why this ships at ring retirement rather than after the GPU fence
    /// Mesa's `vn_ring_wait_seqno` releases the application the instant `head` moves past the seqno it
    /// is waiting on (`vn_ring.c:176-179`) — it never waits on a GPU fence for the reply arena, only on
    /// the ring's own retirement. So the reply arena's bytes must be on the wire by the time C applies
    /// that `RingProgress`, which is ring-retirement time, not fence time; waiting for the fence as well
    /// would needlessly delay a message the application is about to spin-read regardless.
    ///
    /// # Inputs / outputs
    /// - Returns one [`S2C::BlobData`] per run of bytes S wrote, across every Venus-internal, non-ring
    ///   blob (the reply arena; never the ring, and never the staging pool, which S never writes — see
    ///   [`Self::take_blob_writes`]'s docs for why an observed-writes rule already excludes the pool
    ///   without needing to know what it is). Empty when S has written none of them since the last
    ///   call.
    pub fn take_venus_blob_writes(&mut self) -> Vec<S2C> {
        // Collected into an owned `Vec` before the mutable diff pass: `emit_blob_writes` needs
        // `&mut self`, so the filter over `self.rings`/`self.venus_internal` must finish and drop its
        // borrows first.
        let ids: Vec<u32> = self
            .blobs
            .keys()
            .copied()
            .filter(|id| !self.rings.contains_key(id) && self.venus_internal.contains(id))
            .collect();
        // The reply arena keeps the fine byte grain (gap 0): it is small, and shipping a byte
        // S did not write there could clobber the application's — the grain's whole reason to exist.
        self.emit_blob_writes(&ids, 0)
    }

    /// **(c)2 completion barrier:** does the reply arena currently show a `vkGetFenceStatus` reply reading
    /// `VK_SUCCESS`?
    ///
    /// With fence feedback off the application implements `vkWaitForFences` by polling `vkGetFenceStatus`
    /// (Mesa `vn_queue.c`); virglrenderer writes each reply into the reply arena (a Venus-internal blob) as
    /// `[VkCommandTypeEXT][VkResult]`. A live `[38][0]` — type `vkGetFenceStatus`, result `VK_SUCCESS` —
    /// means the polled fence has signalled, i.e. the application's submit and its readback copy have
    /// completed on S's GPU, so `res6` holds a whole, finished frame.
    ///
    /// # Why the **live** bytes, not the shipped diff
    /// [`Self::take_venus_blob_writes`] fragments the reply into one run per *changed* byte (the result
    /// byte often does not change from the previous reply), so the contiguous `[38][0]` pattern is not
    /// visible in what S ships. The live arena holds the whole reply.
    ///
    /// # Why only the reply arena, not every `blob_id == 0` shmem
    /// Three shmems share Venus's `blob_id == 0` marker: the ring, the ~1 MiB reply arena, and the ~8 MiB
    /// command-buffer **staging pool**. The staging pool holds the *application's own* command-buffer bytes
    /// (forward-relayed from C, never written by S), where a coincidental `[38][0]` word could otherwise
    /// stick this signal `true`. So the scan is restricted to [`Self::s_written`] — the set of blobs S has
    /// actually written — which contains the reply arena (S's ring thread writes replies into it) but not
    /// the staging pool. This is **sound across pool growth**: when the reply pool fills, Mesa mints a new
    /// `res_id` (`vn_renderer_util.c`), and S writes replies into that one too, so it joins `s_written` on
    /// its first reply — whereas decoding `vkSetReplyCommandStreamMESA` for the arena's `res_id` would go
    /// stale at exactly that moment (see [`Self::take_blob_writes`]'s docs for why that decode is unsound).
    ///
    /// # Why an early or stale match cannot cause a wrong frame (the real safety property)
    /// A stale `[38][0]` *can* linger — reply streams **chain** at advancing offsets rather than
    /// overwriting in place — and the caller does not treat this as a precise per-frame barrier. It does
    /// not need to. Correctness comes from the **ship order** in `progress_thread`: the readback `BlobData`
    /// and the reply arena are shipped *before* the `RingProgress` head-advance, and the application is
    /// released **only** by that head-advance (`vn_ring_wait_seqno` on `head`), which S ships **last** and
    /// only once the application's own `vkGetFenceStatus` poll actually succeeded. Because
    /// [`HostBlob::take_bytes_s_wrote`](crate::blob::HostBlob::take_bytes_s_wrote) is consuming and
    /// per-byte, any early/partial `res6` shipped on a mid-DMA poll is completed by later polls, so the
    /// union on the wire is the whole frame *before* the releasing head-advance. This signal therefore only
    /// controls *when* `res6` ships (ideally once, at completion), never whether C reads a torn or stale
    /// frame. The caller's [`Self::take_app_blob_writes`]-non-empty gate keeps it to a draw with fresh
    /// pixels (an upload copy or a re-poll ships nothing).
    ///
    /// # Inputs / outputs
    /// - Takes `&self`; scans the live bytes of the reply arena. Returns `true` on the first
    ///   `[38u32][0u32]` little-endian match (aligned scan — Venus encodes 4-byte aligned).
    pub fn reply_arena_fence_signaled(&self) -> bool {
        // `vkGetFenceStatus`'s command type and the result it reports once the fence has signalled.
        const GET_FENCE_STATUS: u32 = 38;
        const VK_SUCCESS: u32 = 0;
        for (&res_id, blob) in &self.blobs {
            // The reply arena only: Venus-internal (`blob_id == 0`), not the ring, and — crucially —
            // one S has written (`s_written`), which excludes the same-marker staging pool whose
            // application command bytes could hold a coincidental `[38][0]`.
            if self.rings.contains_key(&res_id)
                || !self.venus_internal.contains(&res_id)
                || !self.s_written.contains(&res_id)
            {
                continue;
            }
            let b = blob.bytes();
            let mut o = 0;
            while o + 8 <= b.len() {
                let ty = u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
                let res = u32::from_le_bytes(b[o + 4..o + 8].try_into().unwrap());
                if ty == GET_FENCE_STATUS && res == VK_SUCCESS {
                    return true;
                }
                o += 4;
            }
        }
        false
    }

    /// **Return path, post-fence half:** the application's own blob writes — the readback buffer and
    /// the feedback word — **largest blob first**, so the megabyte-scale readback ships ahead of the
    /// tiny feedback word, and the feedback word (which releases the application onto the picture)
    /// lands last, after the pixels it releases the application onto.
    ///
    /// # Why this must wait for the GPU fence and `take_venus_blob_writes` need not
    /// Unlike the reply arena, nothing about these blobs' contents is guaranteed correct merely
    /// because the ring retired: the readback buffer is written by the GPU's own DMA, which can
    /// legitimately still be in flight after virglrenderer's ring thread has moved `head` (this is
    /// (c)1's stale-frame defect — a diff answers "did these bytes change?", never "has the GPU
    /// finished?"; see [`Self::take_blob_writes`]'s docs). So this half of the split is the one the
    /// caller must gate on [`RenderEngine::wait_for_work_retired`], not ring retirement alone.
    ///
    /// # Inputs / outputs
    /// - Returns one [`S2C::BlobData`] per run of bytes S wrote, across every non-Venus-internal,
    ///   non-ring blob, ordered by blob size descending (ties broken by resource id, purely for a
    ///   reproducible order — nothing depends on it). Empty when S has written none of them since the
    ///   last call.
    pub fn take_app_blob_writes(&mut self) -> Vec<S2C> {
        // How near two changed runs must be for the readback path to merge them (re-shipping the
        // unchanged bytes between). 256 collapses the readback's dense small-gap fragmentation to a
        // handful of runs while bounding the redundant bytes. See `blob::coalesce_ranges` for why this
        // is safe here (and only here): `res6` is S-written and C-read-only.
        const READBACK_COALESCE_GAP: usize = 256;
        // `(res_id, size)` pairs so the sort below can order by size without a second map lookup.
        let mut ids: Vec<(u32, u64)> = self
            .blobs
            .iter()
            .filter(|(id, _)| !self.rings.contains_key(id) && !self.venus_internal.contains(id))
            .map(|(&id, blob)| (id, blob.size()))
            .collect();
        // Largest first — the readback buffer (megabytes) must lead the feedback word (bytes) so the
        // pixels it reports on are already in flight before the word that reports them. Ties broken
        // by id purely so the order is reproducible rather than hash-ordered.
        ids.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let ids: Vec<u32> = ids.into_iter().map(|(id, _)| id).collect();
        // Coalesce the readback's fragmented runs (gap 256): `res6` is S-written and C-read-only,
        // so re-shipping an unchanged gap byte is idempotent, and it collapses the ~5000 one-byte
        // messages/frame the return path is rate-bound on. See `blob::coalesce_ranges`.
        self.emit_blob_writes(&ids, READBACK_COALESCE_GAP)
    }

    /// **(c)1 Task 9 Probe A support**: fingerprint every blob S has ever written, cheaply.
    ///
    /// # What this is for
    /// The design note (`docs/design/2026-07-17-return-path-completion.md` §7) predicts the defect is
    /// `T2 < T4`: S's return path treats a ring retirement as proof the GPU's readback is done, and it
    /// is not. This method is how the progress loop *catches the GPU in the act* — it re-fingerprints
    /// the readback blobs on idle polls (when no new `RingDelta` has arrived, so the application is
    /// blocked and nothing on C's side is writing this memory), and a fingerprint that has moved since
    /// the return path shipped the frame is a GPU DMA landing **after** we declared completion.
    ///
    /// It is restricted to [`Self::s_written`] precisely so a moved fingerprint is attributable to the
    /// GPU and not to C's forward writes — see that field's docs.
    ///
    /// # Inputs / outputs
    /// - Returns `(res_id, fingerprint)` for every blob S has ever written that still exists, using
    ///   the same strided [`rayland_relay::trace::fingerprint`] as the T5 baseline so the two are
    ///   directly comparable. Cheap enough (microseconds per blob) to call on every 200 µs poll.
    pub fn fingerprint_written_blobs(&self) -> Vec<(u32, u64)> {
        self.s_written
            .iter()
            // A blob can be unref'd out from under the set; skip a res_id whose blob is gone rather
            // than carry a stale entry. The set is diagnostic, so a lingering id is harmless.
            .filter_map(|&res_id| {
                self.blobs
                    .get(&res_id)
                    .map(|blob| (res_id, rayland_relay::trace::fingerprint(blob.bytes())))
            })
            .collect()
    }

    /// **(c)1 fence-feedback delivery support**: fingerprint every blob that is not a ring, cheaply.
    ///
    /// # What this is for
    /// The return path's delivery loop (`rayland-s`'s `progress_thread`) calls this once per poll and
    /// ships blob writes whenever a fingerprint moves. Unlike [`Self::fingerprint_written_blobs`],
    /// which is scoped to blobs S has already been *observed* to write (Probe A's set), this covers
    /// **every** non-ring blob — because the feedback buffer must be watched from its *first* write,
    /// before it has ever been in the S-written set (see the spec's §3.2 bootstrap-deadlock note).
    ///
    /// Rings are excluded for the same reason [`Self::take_blob_writes`] excludes them: a ring's pages
    /// are C's command bytes and S's `head`, not S's writes to return.
    ///
    /// # Inputs / outputs
    /// - Returns `(res_id, fingerprint)` for every non-ring blob, using the same strided
    ///   [`rayland_relay::trace::fingerprint`] as Probe A so the values are comparable across polls.
    ///   Cheap enough (a strided hash, microseconds per blob) to call on every 200 µs poll.
    pub fn fingerprint_nonring_blobs(&self) -> Vec<(u32, u64)> {
        self.blobs
            .iter()
            // A ring's pages are not S's writes to return — exclude them, exactly as `take_blob_writes`
            // does, so a `head` store is never mistaken for a completion write.
            .filter(|(res_id, _)| !self.rings.contains_key(res_id))
            .map(|(&res_id, blob)| (res_id, rayland_relay::trace::fingerprint(blob.bytes())))
            .collect()
    }


    /// **(c)1 Task 9 Probe A support**: how many `C2S::RingDelta` messages have been applied so far.
    ///
    /// Probe A samples this alongside [`Self::fingerprint_written_blobs`] to tell a late GPU write of
    /// the shipped frame from the next frame's readback arriving early — see [`Self::applied_ring_deltas`]'s
    /// field docs for the full argument. Monotonic (modulo a `u64` wrap that no real session reaches).
    pub fn applied_ring_deltas(&self) -> u64 {
        self.applied_ring_deltas
    }
}
