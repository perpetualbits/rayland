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
use rayland_relay::{BlobRun, C2S, S2C};
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

/// A blob shadow that has been allocated on C but does not yet know the id **S** will give it.
///
/// # Why a staging slot exists at all — the (c)1 Task 6 finding
/// The obvious code registers the shadow in [`BlobTable`] as soon as the `S2C::BlobCreated` reply
/// comes back, since that reply is what carries the id. **That is too late, and it loses the
/// readback buffer's pixels every single run.**
///
/// C's *reader thread* is the one that sees the id first. If the shadow is only registered afterwards,
/// by the vtest thread waking from its request/reply, then S's very next message — routinely an
/// `S2C::BlobData` for that same blob — reaches a reader that has no shadow to put it in, and the
/// bytes are dropped with *"S sent BlobData for resource 5, which C has no shadow of"*.
///
/// That is not a narrow window a faster machine would close. Mesa creates a blob resource lazily at
/// `vkMapMemory`, so a readback buffer's blob is created *after* the GPU has already rendered into
/// it: S has data to send the instant it maps the blob, every run.
///
/// So the shadow is **staged before the request is sent**, and the reader commits it — with the
/// blob's initial contents, which [`S2C::BlobCreated`] carries for a related but distinct reason
/// (this reply is what lets Mesa `mmap` the pages, so the bytes must be down *before* it is
/// forwarded, and no separate message can be early enough). See [`commit_pending_blob`].
///
/// # Why one slot rather than a queue
/// `RelayEngine::create_blob_resource` blocks for its reply, and the vtest thread is the only caller,
/// so at most one blob is ever in flight. A queue would model a concurrency that cannot occur and
/// would hide a real bug — two stages without an intervening commit — behind plausible behaviour.
pub type PendingBlob = Arc<Mutex<Option<LocalBlob>>>;

/// Register the staged shadow under the id S has just assigned it.
///
/// **C's reader thread must call this on every `S2C::BlobCreated`, before forwarding the reply and
/// before reading another message.** See [`PendingBlob`] for what goes wrong otherwise; the short
/// version is that S's next message is routinely the blob's own data, and a shadow that is not in
/// the table yet means those bytes are dropped on the floor.
///
/// # Inputs / outputs
/// - `pending`: the staging slot; emptied by this call.
/// - `blobs`: the live shadow table.
/// - `res_id`: the id from [`S2C::BlobCreated`].
/// - `initial`: whatever was already in the blob on S, laid into the shadow **before** the blob is
///   published. See [`S2C::BlobCreated`]: a readback buffer arrives with the finished frame already
///   in it, and this reply is what lets Mesa map the pages — so the bytes must be down first.
/// - Returns nothing. A `BlobCreated` with nothing staged is **ignored**: S answering a request C
///   never made is S's protocol error, and C's reader reports the ones it can attribute. Panicking
///   here would take out the reader thread — the one thing that delivers every reply — over a
///   message that harms nothing by being dropped.
///
/// # Failure modes
/// An `initial` run that does not fit the shadow is **skipped, with a message**, and the blob is
/// still registered. The runs are remote input and a bad one is S's protocol error, but refusing the
/// whole blob over it would strand Mesa in `recvmsg` waiting for a descriptor that never comes —
/// trading S's bug for a hang on C. The bounds check itself is not optional: `offset` and the run's
/// length arrive over the network, and an unchecked write would be a remote peer scribbling past a
/// mapping.
pub fn commit_pending_blob(
    pending: &PendingBlob,
    blobs: &BlobTable,
    res_id: u32,
    initial: &[BlobRun],
) {
    let Some(mut blob) = pending
        .lock()
        .expect("the pending blob lock is never poisoned")
        .take()
    else {
        return;
    };

    // Lay S's bytes down *before* the blob is reachable, so nothing can observe it half-filled.
    for run in initial {
        // Computed in `u64` first: a `usize` cast before the check could wrap on a 32-bit target —
        // and C is meant to be the weak machine, so a 32-bit C is a real target rather than a
        // hypothetical one — turning an out-of-range write into an in-range one.
        let end = match run.offset.checked_add(run.bytes.len() as u64) {
            Some(end) if end <= blob.size() => end,
            _ => {
                eprintln!(
                    "rayland-c: S's initial contents for resource {res_id} claim bytes \
                     {}..{} of a {}-byte blob; skipping the run. This is a protocol error on S, and \
                     the blob is still registered — refusing it would leave Mesa waiting forever for \
                     a descriptor.",
                    run.offset,
                    run.offset.saturating_add(run.bytes.len() as u64),
                    blob.size()
                );
                continue;
            }
        };
        blob.bytes_mut()[run.offset as usize..end as usize].copy_from_slice(&run.bytes);
    }

    // Keyed by **S's** id, because that is what every later message naming this resource uses.
    // Deriving the key from the wire rather than from a local counter means there is no translation
    // table that can drift.
    blobs
        .lock()
        .expect("the blob table lock is never poisoned")
        .insert(res_id, blob);
}

/// The transport that carries the relay protocol to S.
///
/// # Why this is a trait
/// Two reasons, and the second is the one that matters. The first is testability: a mock link makes
/// every method of [`RelayEngine`] exercisable with no network and no S, which is what lets Task 3
/// be tested at all — when Task 3 was written neither S nor the QUIC transport existed. Task 6 has
/// since put a real link ([`crate::link::QuicSendLink`]) behind this trait and run the two against
/// each other, and the mock remains what keeps these tests GPU-free. It is worth recording what that
/// division bought and what it did not: the mock tests caught real logic bugs, and **not one of the
/// four faults Task 6 found was in this crate's logic** — every one was an assumption about Mesa or
/// virglrenderer that only a live peer could refute.
///
/// The second reason is that (c)1's transport is genuinely undecided: ring-findings §7 concluded
/// that **latency, not bandwidth, is what will hurt** — the reply arena was ~12x the command traffic
/// and its replies are *round trips* the application blocks on. v1 puts everything on one QUIC
/// stream, which has TCP's head-of-line behaviour; splitting the reply path onto its own stream is a
/// change behind this trait and nowhere else. Whatever answers that is what implements it.
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

/// Guarantees that everything Mesa wrote into the ring **before now** has reached S, before an
/// inline command that depends on it is allowed to cross.
///
/// # The race this exists to close, and the live evidence for it
/// **Found by (c)1 Task 6, the first run of C against a real S.** Mesa's ring protocol has an
/// ordering rule it never states, because on one machine it cannot be broken: *the ring bytes are
/// visible before the socket command that refers to them.* Mesa stores `tail` into shared memory and
/// only then sends `vkWaitRingSeqnoMESA` / `vkNotifyRingMESA` on the vtest socket, so the host
/// cannot observe the command without also observing the bytes.
///
/// **(c)1 breaks that rule, because C has two independent producers feeding one link:**
///
/// - the **vtest thread**, which relays inline commands the instant Mesa sends them — a short,
///   direct path;
/// - the **ring watcher**, which must first *notice* the bytes by polling `tail`, and only then
///   relays them — a path with a poll interval in it.
///
/// So the socket command routinely **overtakes** the ring bytes it depends on. virglrenderer catches
/// it and refuses to guess, which is a mercy — it kills the context outright
/// (`vkr_ring_thread`, `vkr_ring.c`):
///
/// ```text
/// vkr: vkr_ring_thread: ring seqno(7072) unable to reach wait seqno(7144)
/// vkr: vkWaitRingSeqnoMESA resulted in CS error
/// vkr: destroying context 1 (rayland-venus) with a valid instance
/// ```
///
/// Those are real numbers from a real run: Mesa wrote 72 bytes, stored `tail = 7144`, and sent the
/// wait; the wait arrived while S's ring still stood at 7072, and virglrenderer concluded — quite
/// reasonably — that the driver had emitted an invalid asynchronous ring wait.
///
/// **This is not a timing bug to be tuned away.** Polling faster shortens the window; it cannot
/// close it, because there is no interval at which "notice, then relay" beats "relay". The barrier
/// restores the invariant where it was broken: on receiving an inline command, C waits until the
/// watcher has shipped the ring as far as Mesa had written it at that moment. Since Mesa stored
/// `tail` *before* sending the command, that frontier necessarily covers everything the command can
/// refer to.
///
/// # Why this is a trait rather than a direct call
/// [`RelayEngine`] must not know about threads, sockets or the daemon's shared state — its whole
/// value is being testable against a mock link with none of those present. The barrier's real
/// implementation lives in `main.rs`, where those things are; here it is a seam, and its tests use a
/// recording stub.
pub trait RingFlush: Send + Sync {
    /// Block until the ring watcher has relayed everything Mesa wrote before this call.
    ///
    /// Must be **infallible from the caller's point of view**: it returns `()` because there is
    /// nothing useful the engine could do with a failure. An implementation that cannot make the
    /// guarantee (a dead watcher, a vanished S) must report the fact itself and return, rather than
    /// block forever — the session is over either way, and hanging inside a barrier converts a
    /// diagnosable failure into a silent one.
    ///
    /// Must be a **no-op before the ring exists**: `vkCreateRingMESA` is itself an inline command,
    /// and it necessarily precedes any ring byte.
    fn flush_ring(&self);
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
    /// The shadow awaiting the id S will assign it. Committed by the reader thread, not here — see
    /// [`PendingBlob`] for the run-losing race that forces the split.
    pending: PendingBlob,
    /// The barrier that keeps an inline command from overtaking the ring bytes it refers to. See
    /// [`RingFlush`] for the race, and the live evidence that it is not hypothetical.
    ///
    /// `None` means no barrier is installed, which is correct **only** where no ring watcher is
    /// running — i.e. this module's own tests. `main.rs` always installs one; see
    /// [`RelayEngine::set_ring_flush`].
    flush: Option<Arc<dyn RingFlush>>,
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
            pending: Arc::new(Mutex::new(None)),
            // Installed by `main.rs` once the watcher's shared state exists; see `set_ring_flush`.
            flush: None,
        }
    }

    /// A handle to the staging slot, for the daemon's reader thread.
    ///
    /// The reader must pair this with [`commit_pending_blob`] on every `S2C::BlobCreated`. See
    /// [`PendingBlob`] for why the engine cannot do it itself.
    pub fn pending(&self) -> PendingBlob {
        Arc::clone(&self.pending)
    }

    /// Install the barrier that stops an inline command overtaking the ring bytes it refers to.
    ///
    /// **`main.rs` must call this**, and the consequence of forgetting is not subtle: without it,
    /// `vkWaitRingSeqnoMESA` can reach S ahead of the delta that satisfies it, and virglrenderer
    /// destroys the context. [`RingFlush`]'s docs carry the evidence.
    ///
    /// It is a setter rather than a constructor argument because of an ordering knot in the daemon's
    /// startup: the barrier needs the blob table and the ring slot, and both are **owned by the
    /// engine** and only reachable through [`Self::blobs`] and [`Self::ring`] once it exists. So the
    /// engine has to be built first and told second. Leaving it unset is legitimate only where there
    /// is no watcher to race with, which in practice means tests.
    pub fn set_ring_flush(&mut self, flush: Arc<dyn RingFlush>) {
        self.flush = Some(flush);
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
        //
        // `solicited` is ignored here on purpose, and it is not redundant: the daemon's reader
        // thread routes only *solicited* errors into this channel, precisely so a refusal of a
        // fire-and-forget message cannot arrive as an answer to a request that has nothing to do
        // with it. By the time an error reaches here it has already passed that filter, so
        // re-checking it would only invent a second, weaker copy of the rule. See `S2C::Error`.
        if let S2C::Error { message, .. } = reply {
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
    /// # The barrier, and why it is the first thing this does
    /// Mesa stores the ring's `tail` **before** it sends the command that refers to it, and
    /// virglrenderer relies on that ordering absolutely — a `vkWaitRingSeqnoMESA` naming a seqno S's
    /// ring has not reached is not treated as "early", it is treated as a **broken driver** and the
    /// context is destroyed. C's two producers (this thread, and the ring watcher) would otherwise
    /// deliver them in whichever order won the race. See [`RingFlush`] for the live evidence.
    ///
    /// The barrier runs before the dword check on purpose: the check is about *this* command's
    /// shape, while the barrier is about everything Mesa wrote *before* it. Refusing a malformed
    /// command is no reason to leave the ring un-shipped, and the two have nothing to do with each
    /// other.
    ///
    /// # Failure modes
    /// [`EngineError::UnalignedCommand`] if `cmd` is not a whole number of dwords — virglrenderer
    /// counts commands in dwords and would reject it on S. Checked here so the error names the byte
    /// length, on the machine where the mistake is visible, rather than arriving as a remote
    /// rejection a network away from its cause.
    fn submit(&mut self, ctx_id: u32, cmd: &[u8]) -> Result<(), EngineError> {
        // Everything Mesa wrote before it sent this command must be on S before this command is.
        if let Some(flush) = &self.flush {
            flush.flush_ring();
        }
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
        // Local first: Mesa is already waiting on the descriptor this produces. `blob_id` is
        // recorded with the shadow because it is the only signal that separates the application's
        // own memory from Venus's internal plumbing (ring-findings §6), and `crate::blob_sync`
        // routes on exactly that — see `LocalBlob::is_application_memory`.
        let (blob, fd) = LocalBlob::create(blob_id, size)?;

        // **Stage the shadow before the request goes out**, so that the reader thread can commit it
        // the instant S names it — before it reads whatever S sends next, which for a readback
        // buffer is that blob's own pixels. See `PendingBlob`: doing this after the reply instead
        // loses those bytes on every run, and the application reads its own zeros.
        *self
            .pending
            .lock()
            .expect("the pending blob lock is never poisoned") = Some(blob);

        // Now ask S for the real, GPU-backed counterpart. Only S can create it; only C can map it.
        // By the time this returns, the reader has already moved the shadow into `blobs` under the
        // id in the reply.
        let res_id = self.request(
            &C2S::CreateBlob {
                blob_mem,
                blob_flags,
                blob_id,
                size,
            },
            "BlobCreated",
            |reply| match reply {
                // `initial` is ignored here on purpose, and it is not dropped: the reader thread has
                // already laid those bytes into the shadow via `commit_pending_blob`, before this
                // reply was ever forwarded. It has to be that way round — see `S2C::BlobCreated`.
                S2C::BlobCreated { res_id, .. } => Ok(res_id),
                other => Err(other),
            },
        )?;

        // Only now, with the pages actually in the table, announce the ring to the watcher. The
        // order is load-bearing: the watcher looks the ring up in the blob table the instant it
        // sees an identity here, so publishing first would let it find an id whose shadow does not
        // exist yet. It has nothing to do until the ring appears, and a ring it never learns about
        // is a silent hang rather than an error — see `RingIdentity`.
        if let Some(identity) = RingIdentity::from_blob_request(res_id, blob_id, size) {
            let mut slot = self
                .ring
                .lock()
                .expect("the ring slot lock is never poisoned");
            // **First match only.** `from_blob_request` is a shape heuristic and Mesa's per-thread
            // TLS ring fits it too (16580 bytes, `blob_id == 0`; see `RingIdentity`). Mesa creates
            // the instance ring first and it is the one that carries the application's drawing, so
            // latching it and refusing later matches keeps the watcher pointed at the right ring.
            // Overwriting unconditionally would silently repoint it at a 16 KiB ring carrying
            // nothing the application draws — and the watcher, which latches this slot once at
            // startup, would not even notice the change.
            if slot.is_none() {
                *slot = Some(identity);
            } else {
                // Worth a human's attention rather than silence: it means the session is doing
                // something (c)1 has not scoped. The plan pins `VN_PERF=no_multi_ring`, under which
                // `vn_tls_get_ring` hands back the instance ring and this should never fire.
                eprintln!(
                    "rayland-c: ignoring a second ring-shaped blob (res_id={res_id}, size={size}); \
                     the command ring is already latched. This is probably Mesa's per-thread TLS \
                     ring, which (c)1 does not support relaying — set VN_PERF=no_multi_ring."
                );
            }
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
    /// This is what makes Task 3 testable at all: when it was written there was no S to talk to.
    /// Task 6 has since wired the two together over QUIC, and `rayland-s/tests/loopback_e2e.rs` is
    /// where a real S is exercised; a mock is still the right S *here*, because these tests must run
    /// on a machine with no GPU.
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

    /// A [`RingFlush`] that records whether it was called, and when relative to the link's traffic.
    ///
    /// The ordering matters more than the count, so it records the link's send-count at the moment
    /// it ran: that is what distinguishes "the barrier fired" from "the barrier fired *before the
    /// command crossed*", and only the second is the property worth having.
    struct RecordingFlush {
        /// How many times `flush_ring` was called.
        calls: Mutex<u32>,
        /// The number of messages already sent at each call. Empty until the first call.
        sends_at_call: Mutex<Vec<usize>>,
        /// The link's send log, shared so the barrier can observe it as the engine sees it.
        sent: Arc<Mutex<Vec<C2S>>>,
    }

    impl RingFlush for RecordingFlush {
        fn flush_ring(&self) {
            *self.calls.lock().unwrap() += 1;
            let sends = self.sent.lock().unwrap().len();
            self.sends_at_call.lock().unwrap().push(sends);
        }
    }

    /// A [`RelayLink`] that appends to a shared log, so a [`RingFlush`] can see the same history.
    struct SharedLogLink {
        /// Everything sent, shared with the barrier under test.
        sent: Arc<Mutex<Vec<C2S>>>,
    }

    impl RelayLink for SharedLogLink {
        fn send(&mut self, m: &C2S) -> Result<(), EngineError> {
            self.sent.lock().unwrap().push(m.clone());
            Ok(())
        }
        fn recv(&mut self) -> Result<S2C, EngineError> {
            Err(EngineError::RelayLinkFailed {
                detail: "this link never receives".into(),
            })
        }
    }

    /// **The regression test for (c)1 Task 6's ordering finding.**
    ///
    /// An inline command must not cross until the ring watcher has shipped everything Mesa wrote
    /// before it. Mesa stores `tail` and *then* sends the socket command, and virglrenderer treats a
    /// `vkWaitRingSeqnoMESA` whose seqno its ring has not reached as a **broken driver**, destroying
    /// the context — it does not wait. A live run produced exactly that: `ring seqno(7072) unable to
    /// reach wait seqno(7144)`.
    ///
    /// So the assertion is specifically that the barrier ran **before** the send, not merely that it
    /// ran: a barrier that fires afterwards is decoration.
    #[test]
    fn an_inline_command_flushes_the_ring_before_it_crosses() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let flush = Arc::new(RecordingFlush {
            calls: Mutex::new(0),
            sends_at_call: Mutex::new(Vec::new()),
            sent: Arc::clone(&sent),
        });
        let mut engine = RelayEngine::new(SharedLogLink {
            sent: Arc::clone(&sent),
        });
        engine.set_ring_flush(Arc::clone(&flush) as Arc<dyn RingFlush>);

        // `0xfd` = opcode 253 = `vkWaitRingSeqnoMESA`: the exact command that destroyed the context
        // in the live run, and the reason this barrier exists.
        engine.submit(1, &[0xfd, 0, 0, 0]).expect("the submit");

        assert_eq!(
            *flush.calls.lock().unwrap(),
            1,
            "every inline command must be preceded by a ring flush"
        );
        assert_eq!(
            *flush.sends_at_call.lock().unwrap(),
            vec![0],
            "the flush must run BEFORE the command is sent; firing afterwards would let \
             vkWaitRingSeqnoMESA overtake the ring delta that satisfies it, which virglrenderer \
             treats as a broken driver and answers by destroying the context"
        );
        assert_eq!(sent.lock().unwrap().len(), 1, "and the command still crosses");
    }

    /// A malformed command is still refused, and the barrier still runs.
    ///
    /// The two are independent: the barrier is about everything Mesa wrote *before* this command,
    /// which is no less owed to S because this particular command is the wrong shape.
    #[test]
    fn a_malformed_inline_command_is_refused_but_the_ring_is_still_flushed() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let flush = Arc::new(RecordingFlush {
            calls: Mutex::new(0),
            sends_at_call: Mutex::new(Vec::new()),
            sent: Arc::clone(&sent),
        });
        let mut engine = RelayEngine::new(SharedLogLink {
            sent: Arc::clone(&sent),
        });
        engine.set_ring_flush(Arc::clone(&flush) as Arc<dyn RingFlush>);

        let err = engine.submit(1, &[0xbc, 0, 0]).expect_err("a refusal");
        assert!(matches!(err, EngineError::UnalignedCommand { len: 3 }));
        assert_eq!(*flush.calls.lock().unwrap(), 1);
        assert!(
            sent.lock().unwrap().is_empty(),
            "nothing malformed may reach the wire"
        );
    }

    /// A blob request must produce a **usable** descriptor for Mesa, adopt the id **S** chose, and
    /// leave the shadow staged for the reader to commit under that id.
    ///
    /// The id matters more than it looks: every subsequent message naming this resource uses S's
    /// id, so a local counter that merely happened to agree today would silently address the wrong
    /// resource the moment the two drifted.
    ///
    /// The **staging** is (c)1 Task 6's finding: the engine deliberately does not register the
    /// shadow itself, because by the time its reply arrives S has often already sent the blob's data
    /// and C's reader has already dropped it for want of a shadow. See [`PendingBlob`].
    #[test]
    fn creating_a_blob_stages_a_local_shadow_and_adopts_s_s_resource_id() {
        let link = MockLink::with_replies([S2C::BlobCreated { res_id: 7, initial: Vec::new() }]);
        let mut engine = RelayEngine::new(link);
        let pending = engine.pending();
        let blobs = engine.blobs();

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
        // Staged, not yet registered: in the daemon the reader thread has already committed it by
        // now, but nothing here plays that role, so this is the honest intermediate state.
        assert_eq!(
            pending.lock().unwrap().as_ref().expect("a staged shadow").size(),
            131268
        );
        assert!(
            blobs.lock().unwrap().is_empty(),
            "the engine must not register the shadow itself; the reader commits it under S's id \
             before it reads the blob's data, which S often sends immediately"
        );

        // What the reader does on `S2C::BlobCreated`.
        commit_pending_blob(&pending, &blobs, blob.resource_id, &[]);

        assert_eq!(
            blobs.lock().unwrap().get(&7).expect("a shadow under S's id").size(),
            131268
        );
        assert!(
            pending.lock().unwrap().is_none(),
            "the staging slot must be empty again, or the next blob's commit would find this one"
        );
    }

    /// A `BlobCreated` with nothing staged must be ignored rather than panic.
    ///
    /// It means S answered a request C never made — S's protocol error. This runs on the **reader
    /// thread**, which is the one thing that delivers every reply and writes every byte S sends, so
    /// panicking here would end the session over a message that harms nothing by being dropped.
    #[test]
    fn committing_with_nothing_staged_is_ignored() {
        let pending: PendingBlob = Arc::new(Mutex::new(None));
        let blobs: BlobTable = Arc::new(Mutex::new(HashMap::new()));

        commit_pending_blob(&pending, &blobs, 7, &[]);

        assert!(blobs.lock().unwrap().is_empty());
    }

    /// Allocating the ring must publish its identity, because the watcher has no other way to learn
    /// which blob to follow — and a ring it never learns about is a silent hang, not an error.
    ///
    /// The size is the real one from the live capture (ring-findings §4), and the resource id it is
    /// published under must be **S's**, since that is what every `RingDelta` will be addressed to.
    #[test]
    fn allocating_the_ring_publishes_its_identity_for_the_watcher() {
        let link = MockLink::with_replies([S2C::BlobCreated { res_id: 4, initial: Vec::new() }]);
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
        let link = MockLink::with_replies([S2C::BlobCreated { res_id: 2, initial: Vec::new() }]);
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
        let link = MockLink::with_replies([S2C::BlobCreated { res_id: 3, initial: Vec::new() }]);
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
        let link = MockLink::with_replies([S2C::BlobCreated { res_id: 1, initial: Vec::new() }]);
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
            // A `GetCapset` is one of the two messages C genuinely blocks on, so its refusal is
            // solicited and the daemon's reader would legitimately route it here.
            solicited: true,
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
        let link = MockLink::with_replies([S2C::BlobCreated { res_id: 9, initial: Vec::new() }]);
        let mut engine = RelayEngine::new(link);
        let blobs = engine.blobs();
        let pending = engine.pending();
        engine
            .create_blob_resource(1, 2, 0, 0, 4096)
            .expect("a blob");
        // Stand in for the reader thread, which commits the staged shadow on `S2C::BlobCreated`.
        commit_pending_blob(&pending, &blobs, 9, &[]);
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
