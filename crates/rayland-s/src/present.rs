//! **Putting the frame on S's screen** — and the one question (c)1 has to ask that its own spec
//! forbids.
//!
//! §1 of the (c)1 spec asks for the frame to be *presented in a window on S's display*, by an
//! independent path from the application's own PNG on C. This module is that path. It is short, and
//! almost all of it is the consequence of two facts that are not obvious and were both bought with
//! live runs rather than reading.
//!
//! # Fact 1: S cannot see the frame it is rendering. It can only see the app's readback of it.
//! The natural design — "present the application's render target" — **is not available**. C0 Task 4b
//! established that the refapp's `DEVICE_LOCAL` `VkImage` produces **no blob at all**: it is created
//! by Venus commands *inside the command ring*, which (c)1 relays as opaque bytes, and it never
//! appears in the engine's resource table. There is nothing there for the host to name, let alone
//! export.
//!
//! What S *can* see is the **readback buffer**: the app does `vkCmdCopyImageToBuffer` into
//! `HOST_VISIBLE` memory so it can write its PNG, and C0 caught exactly that buffer — `res=6`,
//! 16384 B = 64×64×4, holding the blue clear colour. Its real memory lives on S, written by S's GPU.
//! So S presents from the readback blob: the same bytes the app will later read on C.
//!
//! ## This is therefore **not zero-copy**, and (c)1 does not inherit SP3's headline property
//! Stated plainly here because it would otherwise be the easiest thing in the repository to
//! misread. The pixels take a **GPU→CPU round trip on S** and reach the compositor through
//! `rayland-present`'s **`wl_shm`** path — a CPU copy into a shared-memory buffer. SP3's dmabuf path
//! is right there in the same library and (c)1 **cannot use it**, for a reason that has nothing to
//! do with the GPU or the compositor: dmabuf-exporting a resource requires *seeing* the resource,
//! and S never sees this one. [`BlobFrameSource::supports_dmabuf`] answers `false` structurally, not
//! conditionally.
//!
//! SP3's work is not wasted — `rayland-server` still uses it, and it is what a real presentation
//! path will use. (c)1 just cannot reach it yet.
//!
//! ## The shortcut has a known expiry date, and it is the next slice
//! This borrows the application's readback, **which b2 will not have**. A swapchain image has no
//! `vkCmdCopyImageToBuffer` and no host-visible buffer to borrow, so the whole trick evaporates the
//! moment the application presents its own window. Spec §7.1 and §12.6: b2 forces the zero-copy
//! question, and that is the right time to spend C0 Task 4c's deferred spike. **Do not build on
//! this.**
//!
//! # Fact 2: identifying *the* frame is the one place (c)1 must ask "whose memory is this?"
//! Spec §7.2's hard-won lesson — the one that corrected the whole S→C return path — was:
//!
//! > Stop asking *"whose memory is this?"* and ask *"did I write it?"* — on one machine every byte
//! > S writes is instantly visible to C, so ownership predicates are a **guess** at that
//! > relationship while observed writes **are** it.
//!
//! **Presentation cannot honour that, and this module does not pretend otherwise.** "Did I write
//! it?" is answerable without knowing what a blob *is*, which is exactly why it is sound — and
//! exactly why it cannot help here. It identifies *bytes to ship*; it does not identify *a frame to
//! show*. Showing a frame means picking one blob out of several, which is irreducibly a question
//! about what the blob is. There is no observation-based predicate for "this one is the picture".
//!
//! So [`FrameCapture`] guesses, by **size**: `width * height * 4` — 16384 for the reference app.
//! Three things make that acceptable, and they are the whole argument:
//!
//! 1. **It is only a guess about what to *show*, never about what to *ship*.** The relay's
//!    correctness does not depend on it. A wrong answer here is a wrong picture in a window, not a
//!    corrupted application. The app's own PNG on C comes back through §7.2's rule and is untouched
//!    by anything in this file — which is what makes §1's "two independent paths" actually
//!    independent.
//! 2. **It fails loudly rather than picking.** If two blobs match the size, [`FrameCapture::into_frame`]
//!    returns [`FrameError::Ambiguous`] naming both, and `rayland-s` exits non-zero having presented
//!    nothing. A coin-flip that shows the vertex buffer as a picture, or shows frame N when the
//!    session had two, is the failure this refusal exists to prevent. **The guess must be visible
//!    when it cannot be made.**
//! 3. **b2 removes the need entirely.** A swapchain image is *explicit*: the application says
//!    "present this", by name, through a WSI call. There is nothing to infer. This module is
//!    inference standing in for an API call that b1's application never makes — which is the same
//!    reason it expires with the shortcut in Fact 1.
//!
//! ## The size has to be *configured*, which is the tension showing through
//! Note what Fact 1 costs Fact 2: since S cannot see the application's render target, **S also
//! cannot learn its size**. The predicate needs a number that the only party who knows it (the
//! application, on the other machine) never says. So [`frame_size_from_env`] reads it from
//! [`ENV_PRESENT_SIZE`], defaulting to the reference app's 64×64. That is not a missing feature to
//! be filled in later — under b1 there is nothing to fill it in *from*. It is the same gap as the
//! ambiguity, seen from the other side.
//!
//! ## And one assumption that is not checked at all
//! The blob's bytes are handed to `rayland-present` as **RGBA8**, because `rayland-refapp`'s render
//! target is `R8G8B8A8_UNORM` (`pipeline::COLOR_FORMAT`, chosen so its PNG needs no swizzle). S has
//! no way to know that — the format, like the size, lives in commands S relays without reading. An
//! application rendering `B8G8R8A8` would present with **red and blue swapped**, and nothing here
//! would notice. It is worth knowing that this failure is *visible and harmless*: the window would
//! look wrong, the app's PNG on C would still be right, and nothing would be corrupted. Recorded
//! rather than guarded, because guarding it would mean decoding the ring to make a presentation
//! decision, which spec §7 rules out for much better reasons than this one.
//!
//! # When the window appears
//! After the session ends, not during it — see [`present_frame`] for why that is a real constraint
//! rather than laziness.

// The presentation library, extracted from `rayland-server`'s `window.rs` by this task. `rayland-s`
// is its second consumer; being its second consumer is why it exists.
use rayland_present::{FrameSource, RenderedFrame, WindowConfig};
// The relay protocol: `S2C::BlobCreated` is the event that tells us a blob now exists, and — since
// spec §7.3 — is the moment its contents are already there to look at.
use rayland_relay::S2C;

use crate::apply::Applier;

/// Environment variable naming the frame size to present, as `WIDTHxHEIGHT`.
///
/// It exists because S cannot learn this any other way — see the module docs' "the size has to be
/// configured" note. It is the input to the *only* predicate (c)1 has for finding the frame, so
/// getting it wrong does not degrade presentation, it disables it: no blob matches, and
/// [`FrameError::NoCandidate`] names the size it was looking for precisely because that is the thing
/// most likely to be wrong.
pub const ENV_PRESENT_SIZE: &str = "RAYLAND_C1_PRESENT_SIZE";

/// Environment variable that, when set to anything, disables presentation entirely.
///
/// # Why this exists, which is not "to make a feature optional"
/// [`present_frame`] **blocks until a human closes the window**. That is correct for the thing (c)1
/// §1 asks for and fatal for anything automated: `tests/loopback_e2e.rs` launches `rayland-s` as a
/// child process, and a child that waits forever for a click is a test suite that hangs. So the
/// e2e test sets this, and says so at the call site.
///
/// It is deliberately **not** the default. A daemon that quietly declines to do the one thing the
/// spec's success criterion names would be exactly the kind of silent nothing this branch has
/// shipped before.
pub const ENV_NO_PRESENT: &str = "RAYLAND_C1_NO_PRESENT";

/// The reference app's frame size: 64×64 (`rayland-refapp`'s `IMAGE_WIDTH`/`IMAGE_HEIGHT`), giving
/// the 16384-byte readback buffer C0 Task 4b caught as `res=6`.
///
/// A default rather than a required setting because (c)1's entire success criterion is stated in
/// terms of that one application, and making the manual bring-up require an environment variable to
/// do the thing it exists to do would be ceremony.
const DEFAULT_PRESENT_SIZE: (u32, u32) = (64, 64);

/// The window title `rayland-s` labels its window with. Human-facing, and deliberately says where
/// the picture came from: this window is the *remote* render, not a local one.
const WINDOW_TITLE: &str = "Rayland — (c)1: rendered on S, driven from C";

/// The stable application id `rayland-s` claims. Not human-facing.
const WINDOW_APP_ID: &str = "nl.rayland.C1";

/// Why S could not identify a frame to present.
///
/// # Why these are typed rather than a string
/// Both variants are things a human has to act on, and they call for opposite actions: `NoCandidate`
/// usually means [`ENV_PRESENT_SIZE`] is wrong or the application never rendered, while `Ambiguous`
/// means the size predicate itself has stopped being usable for this workload. Collapsing them would
/// throw that distinction away before anyone could use it — and would leave the tests grepping prose
/// to tell them apart.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// No blob in the whole session was the frame's size.
    #[error(
        "no blob in this session was {expected_len} bytes ({width}x{height}x4), so S has nothing to \
         present. S cannot see the application's render target at all — C0 Task 4b: it produces no \
         blob — so a readback buffer of exactly this size is the only thing S can recognise as the \
         frame. Either the application does not render at {width}x{height} (set {ENV_PRESENT_SIZE} \
         to WIDTHxHEIGHT), or it never got as far as reading its pixels back"
    )]
    NoCandidate {
        /// The byte length that was being looked for, named because it is the likeliest thing wrong.
        expected_len: usize,
        /// The configured width, so the message can restate the setting in the form it was given.
        width: u32,
        /// The configured height.
        height: u32,
    },

    /// Two or more blobs were the frame's size, and size is the only predicate S has.
    #[error(
        "S cannot tell which blob is the frame: resources {res_ids:?} are all exactly \
         {expected_len} bytes, and size is the only thing S can identify a frame by (spec §7.2 — S \
         has no sound way to ask *what* a blob is, only whether it wrote it). Refusing rather than \
         showing one at random: a wrong guess here paints the wrong picture with no error anywhere. \
         This is a known limit of b1 and it expires with it — a b2 swapchain image is named \
         explicitly by the application instead of inferred"
    )]
    Ambiguous {
        /// Every candidate, in creation order, so a human can see what it could not choose between.
        res_ids: Vec<u32>,
        /// The size they all shared.
        expected_len: usize,
    },
}

/// A frame-sized blob's pixels, copied out at the moment the blob was created.
struct CapturedBlob {
    /// The engine's resource id, kept only so a refusal can name it.
    res_id: u32,
    /// The blob's bytes, **copied**. See [`FrameCapture::observe_replies`] for why a copy.
    pixels: Vec<u8>,
}

/// Watches a session's blobs go by and keeps the ones that could be the frame.
///
/// # The rule, in one line
/// A blob is a frame candidate if and only if its size is exactly `width * height * 4`. At the end
/// of the session, exactly one candidate is the frame; zero or several is a refusal. See the module
/// docs for why this predicate is a guess, why the guess is acceptable, and why it must fail loudly.
///
/// # Why the decision is made over the whole session rather than incrementally
/// "Present the first frame-sized blob you see" would be simpler and would silently defeat the
/// ambiguity check: it would pick, which is the one thing this type exists not to do. So every
/// candidate is kept and the choice is made once, at [`Self::into_frame`], when the session is over
/// and the set is complete. The cost is bounded by construction — a second candidate is already an
/// error, so this never accumulates.
pub struct FrameCapture {
    /// The configured frame width, carried into the presented [`RenderedFrame`].
    width: u32,
    /// The configured frame height.
    height: u32,
    /// `width * height * 4`: the predicate, precomputed.
    expected_len: usize,
    /// Every blob whose size matched, in creation order.
    candidates: Vec<CapturedBlob>,
}

impl FrameCapture {
    /// A capture looking for a `width` × `height` frame.
    ///
    /// # Inputs / outputs
    /// - `width`, `height`: the application's frame size, which S has no way to discover — see
    ///   [`frame_size_from_env`] and the module docs.
    /// - Returns a capture that has seen nothing. [`Self::into_frame`] on it is
    ///   [`FrameError::NoCandidate`].
    pub fn new(width: u32, height: u32) -> Self {
        FrameCapture {
            width,
            height,
            // `as usize` is safe on any platform S can run on: S is the machine with the GPU and the
            // display, so it is 64-bit, and the product of two u32s fits a u64 regardless.
            expected_len: width as usize * height as usize * 4,
            candidates: Vec::new(),
        }
    }

    /// Inspect the replies `Applier::apply` just produced and capture any newly-created blob that
    /// could be the frame.
    ///
    /// # Why this hangs off `BlobCreated` and not off a poll
    /// Because spec §7.3 says that is when the pixels are there, and (c)1 Task 6 learned it the hard
    /// way. **Mesa creates a blob resource lazily, at `vkMapMemory`** — so the readback buffer's
    /// blob is born *after* `vkCmdCopyImageToBuffer` has already run, with the finished frame
    /// already in it. S has the pixels at `BlobCreated` time, not at some later retirement. Task 6's
    /// return path shipped blob bytes only when a **ring** retired, and that gate never fires for
    /// this blob: the application reads it and exits without touching the ring again. Hanging
    /// presentation off the same gate would reproduce that bug exactly — a correct render and a
    /// blank window.
    ///
    /// # Why the bytes are copied and not borrowed
    /// Three reasons, any one sufficient. The mapping is **live shared memory** the application's
    /// own writes reach with no API call to intercept (the `vkMapMemory` problem, spec §7). The
    /// engine's `unref_resource` can drop the mapping when the application frees the buffer, which it
    /// does on its way out. And presentation happens **after** the session (see [`present_frame`]),
    /// by which time neither of those is hypothetical. 16 KiB is not a cost worth reasoning about.
    ///
    /// # Inputs / outputs
    /// - `applier`: the session state, borrowed only to read a blob's live pages. Must be the same
    ///   `Applier` that produced `replies`, and must still hold the lock the caller took around
    ///   `apply` — otherwise the pages read here are not the ones the reply describes.
    /// - `replies`: exactly what `Applier::apply` returned. Anything that is not an
    ///   [`S2C::BlobCreated`] is ignored.
    /// - Returns nothing; candidates accumulate on `self`.
    pub fn observe_replies(&mut self, applier: &Applier, replies: &[S2C]) {
        for reply in replies {
            // Only a blob's *creation* is interesting: it is both the first moment S can read the
            // pages and — per §7.3 — the moment they are already correct.
            let S2C::BlobCreated { res_id, .. } = reply else {
                continue;
            };
            // The blob is registered in `applier` before the reply is emitted, so this always
            // resolves; `else` rather than `expect` because a future refactor that reorders those
            // two must not turn into a panic in a daemon.
            let Some(blob) = applier.blob(*res_id) else {
                continue;
            };
            let bytes = blob.bytes();
            // The predicate, and the whole of it. Note what is deliberately *not* consulted:
            // `blob_id`, `blob_flags`, creation order, or anything decoded out of the ring.
            if bytes.len() != self.expected_len {
                continue;
            }
            eprintln!(
                "rayland-s: resource {res_id} is {} bytes = {}x{}x4, so it is a candidate for the \
                 frame to present",
                bytes.len(),
                self.width,
                self.height
            );
            self.candidates.push(CapturedBlob {
                res_id: *res_id,
                pixels: bytes.to_vec(),
            });
        }
    }

    /// Decide which candidate is the frame, now that the session is over and the set is complete.
    ///
    /// # Failure modes
    /// - [`FrameError::NoCandidate`] — nothing in the session was the frame's size.
    /// - [`FrameError::Ambiguous`] — several were, and S has no way to choose. **This is a refusal,
    ///   not a fallback**: see the module docs for why picking one would be the worse answer.
    ///
    /// # Inputs / outputs
    /// - Consumes `self`, because the decision is final and the pixels move into the returned frame.
    /// - Returns the frame, ready for [`present_frame`]. Its `pixels` are assumed RGBA8 — see the
    ///   module docs for the one assumption nothing here checks.
    pub fn into_frame(self) -> Result<RenderedFrame, FrameError> {
        // Exactly one candidate is the only answer S can defend. Both other arms are refusals.
        match self.candidates.len() {
            0 => Err(FrameError::NoCandidate {
                expected_len: self.expected_len,
                width: self.width,
                height: self.height,
            }),
            1 => {
                // `into_iter().next()` rather than indexing: it moves the `Vec<u8>` out instead of
                // cloning 16 KiB to satisfy the borrow checker.
                let only = self
                    .candidates
                    .into_iter()
                    .next()
                    .expect("a length-1 vector has a first element");
                eprintln!(
                    "rayland-s: presenting resource {} as the frame ({}x{})",
                    only.res_id, self.width, self.height
                );
                Ok(RenderedFrame {
                    width: self.width,
                    height: self.height,
                    pixels: only.pixels,
                })
            }
            _ => Err(FrameError::Ambiguous {
                res_ids: self.candidates.iter().map(|c| c.res_id).collect(),
                expected_len: self.expected_len,
            }),
        }
    }
}

/// A [`RenderedFrame`] that came out of a blob, presented as something `rayland-present` can drive.
///
/// # Why this is a type rather than a closure
/// Because of what its [`FrameSource::supports_dmabuf`] has to say, and where a reader will look for
/// it. See that method.
struct BlobFrameSource {
    /// The captured pixels. `Option` because [`FrameSource::produce_pixels`] takes `&mut self` and
    /// must hand the `Vec` over rather than clone it; `present` calls it exactly once.
    frame: Option<RenderedFrame>,
    /// The frame's width, kept separately because `width()` is called *before* `produce_pixels`
    /// takes the frame (the window is sized before it is drawn).
    width: u32,
    /// The frame's height. See `width`.
    height: u32,
}

impl FrameSource for BlobFrameSource {
    /// The configured frame width — the size the window is pinned to.
    fn width(&self) -> u32 {
        self.width
    }

    /// The configured frame height.
    fn height(&self) -> u32 {
        self.height
    }

    /// **Always `false`, structurally — this is spec §7.1's consequence, in one method.**
    ///
    /// It is tempting to read this as "(c)1 has not got round to the fast path yet". It is not that.
    /// S's GPU can export dmabufs perfectly well — `rayland-server` does it on this very machine.
    /// What S does not have is **anything to export**: the application's `DEVICE_LOCAL` render
    /// target produces no blob (C0 Task 4b), so it is not in the engine's resource table and has no
    /// name S could pass to `vkGetMemoryFdKHR`. All S has is a `Vec<u8>` that was copied out of the
    /// app's readback buffer, and a `Vec<u8>` cannot be dmabuf-exported by anyone.
    ///
    /// So this returns `false` by inheriting [`FrameSource::supports_dmabuf`]'s default — spelled
    /// out explicitly anyway, because "the default happened to be right" is not what a reader needs
    /// to know here. **(c)1 does not inherit SP3's zero-copy property**, the `wl_shm` path with its
    /// GPU→CPU round trip on S is the only one available, and b2 is where that changes. See the
    /// module docs.
    fn supports_dmabuf(&self) -> bool {
        false
    }

    /// Hand over the captured pixels.
    ///
    /// No GPU work happens here and nothing can fail: the frame was produced long before, by S's
    /// GPU, into a blob, and copied out at [`FrameCapture::observe_replies`]. This method is a move.
    ///
    /// # Failure modes
    /// Errors only if called twice, which `present` does not do — the contract is one produce call
    /// per `present`. It is an error rather than an `unwrap` because a daemon should not abort over
    /// a contract change in another crate.
    fn produce_pixels(&mut self) -> anyhow::Result<RenderedFrame> {
        self.frame.take().ok_or_else(|| {
            anyhow::anyhow!(
                "rayland-present asked this blob-backed source for pixels twice; it has exactly one \
                 frame, captured from the application's readback buffer, and there is no second one \
                 to render"
            )
        })
    }
}

/// Read the frame size to look for from [`ENV_PRESENT_SIZE`], falling back to the reference app's.
///
/// # Why S needs to be told at all
/// See the module docs: S cannot see the application's render target, so it cannot see its size
/// either. The size is not a preference — it is the entire predicate by which S finds the frame.
///
/// # Inputs / outputs
/// - Reads the process environment.
/// - Returns `(width, height)`; [`DEFAULT_PRESENT_SIZE`] if the variable is unset.
///
/// # Errors
/// Returns an error if the variable is set but is not `WIDTHxHEIGHT` with two non-zero decimal
/// numbers. A malformed value is refused rather than ignored: silently falling back to 64×64 after
/// the operator explicitly asked for something else would present the wrong thing, or nothing, with
/// no indication that the setting had not taken.
pub fn frame_size_from_env() -> anyhow::Result<(u32, u32)> {
    let Ok(raw) = std::env::var(ENV_PRESENT_SIZE) else {
        return Ok(DEFAULT_PRESENT_SIZE);
    };
    // `split_once` rather than `split`: exactly one separator is valid, and "64x64x4" must be
    // refused rather than quietly read as 64×64.
    let (w, h) = raw.split_once('x').ok_or_else(|| {
        anyhow::anyhow!("{ENV_PRESENT_SIZE}={raw:?} is not WIDTHxHEIGHT (e.g. \"64x64\")")
    })?;
    let width: u32 = w
        .parse()
        .map_err(|e| anyhow::anyhow!("{ENV_PRESENT_SIZE}={raw:?}: width {w:?} is not a number: {e}"))?;
    let height: u32 = h.parse().map_err(|e| {
        anyhow::anyhow!("{ENV_PRESENT_SIZE}={raw:?}: height {h:?} is not a number: {e}")
    })?;
    // Zero would make `expected_len` zero, and a zero-length blob would then "match" — turning a
    // typo into a silently absurd candidate rather than a refusal.
    anyhow::ensure!(
        width > 0 && height > 0,
        "{ENV_PRESENT_SIZE}={raw:?}: a frame must have a non-zero width and height"
    );
    Ok((width, height))
}

/// Show `frame` in a window on S's display, and stay up until a human closes it.
///
/// # Why this runs after the session rather than during it
/// A deliberate choice with a real reason, not an ordering that fell out. (c)1 presents **exactly
/// one static frame** — that is what §7.1's shortcut gives us, since the thing being borrowed is the
/// application's one readback buffer — so there is no stream of frames that would need a window
/// running alongside the session. And the window must **outlive** the session: the reference app
/// reads its pixels and exits within a second, C then disconnects, and a window torn down with the
/// session would be a flash on the screen. The whole point is for a human to look at it.
///
/// The pixels themselves are captured *during* the session, at the earliest moment S has them
/// ([`FrameCapture::observe_replies`]); only the window waits. What this costs is honest and worth
/// naming: an application that rendered more than one frame would need presentation on its own
/// thread, and would need a real answer to "which frame is this?" rather than "which blob is this?".
/// Both are b2's problem, along with the shortcut this whole module rests on.
///
/// # The window closes only when the user closes it
/// `rayland_present::present` ends its loop when the window is closed **or** its `disconnect` source
/// reaches EOF. There is no peer left to disconnect by this point, so this passes one end of a
/// `UnixStream` pair and holds the other end alive for the duration of the call — a source that can
/// never EOF, leaving the close button as the only exit. This is the same mechanism SP1/SP2/SP3 use
/// with a `TcpStream`/QUIC `Liveness`; only the choice of never-EOF source is (c)1's.
///
/// # Inputs / outputs
/// - `frame`: the captured readback pixels, assumed RGBA8 (see the module docs).
/// - Returns when the window is closed.
///
/// # Errors
/// Returns an error if no compositor is reachable, a required Wayland global is missing, or the
/// event loop fails. **Callers on a machine that may have no display should check first** — see
/// `main.rs`, which treats a missing compositor as a skip rather than a failure, so that
/// `rayland-s` on a headless box still relays correctly.
pub fn present_frame(frame: RenderedFrame) -> anyhow::Result<()> {
    let (width, height) = (frame.width, frame.height);
    let mut source = BlobFrameSource {
        frame: Some(frame),
        width,
        height,
    };
    let config = WindowConfig {
        title: WINDOW_TITLE,
        app_id: WINDOW_APP_ID,
        // Not a choice: `BlobFrameSource::supports_dmabuf` is already `false`, so the `wl_shm` path
        // is taken regardless. Left `false` so the log line names the *real* reason ("this frame
        // source cannot export a dmabuf") rather than the misleading "--force-shm was passed",
        // which no operator passed.
        force_shm: false,
    };
    // A socket pair whose other end this function holds until it returns: `present` watches `theirs`
    // for EOF, and `_ours` cannot EOF while it is alive, so only the close button ends the loop.
    let (ours, theirs) = std::os::unix::net::UnixStream::pair()
        .map_err(|e| anyhow::anyhow!("creating the window's liveness socket pair: {e}"))?;
    // `present` requires a non-blocking source: its calloop callback reads until `WouldBlock`, and a
    // blocking fd would stall the event loop there forever on the first readiness event.
    theirs
        .set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("making the window's liveness socket non-blocking: {e}"))?;

    println!("presenting the frame in a window; close it to exit");
    let result = rayland_present::present(&mut source, &config, theirs);
    // Explicit rather than implicit: `_ours` staying alive across the `present` call above is the
    // entire mechanism keeping the window open, so dropping it is worth a line a reader can see.
    drop(ours);
    result
}
