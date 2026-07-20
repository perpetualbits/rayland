//! **The readback-completion gate.** Deciding *when* a pending readback delivery on S may complete.
//!
//! # The defect this exists to fix
//! S delivers an application's readback (the finished pixels) once its (c)2 completion barrier fires.
//! That barrier's trigger is content-independent — it fires on a newer `vkQueueSubmit` position plus a
//! drained ring — which is correct only when every submit produces a readback. An application that
//! issues **more than one submit per frame** (the `rayland-icosa-cpu` fixture issues two: a fractal
//! upload copy, then the draw-and-readback) breaks that assumption: S cannot tell the copy submit from
//! the draw submit without parsing the opaque ring, so the barrier can fire on the copy — whose fence
//! retires with the readback blob still holding the *previous* frame — and ship those stale pixels.
//! Over a real network this loses ~2/120 frames; on loopback the timing window never opens. Full
//! evidence: `docs/design/2026-07-19-c2-true-remote-mapped-sync.md`.
//!
//! # The signal this gate keys on
//! With an opaque ring and N submits per frame, the *only* thing that distinguishes a readback-bearing
//! submit is that **S's own write into an application blob advanced** — the copy submit writes a
//! texture image, not an application blob, so it produces no such advance. `Applier::take_app_blob_writes`
//! already reports exactly S's newly-written app-blob bytes and is empty when there are none, so
//! "readback advanced" is "that call returned something". This module holds the pure decision built on
//! that boolean, kept separate from `main.rs`'s `progress_thread` so it can be tested without an engine
//! or a socket.

use std::time::Duration;

/// How long a pending readback delivery will wait for the readback to advance before completing anyway.
///
/// # Why a bound exists at all
/// The gate below waits for S's readback write to advance past the last delivered frame. For two
/// *byte-identical* consecutive frames the readback never advances (the fixture never produces such a
/// pair — its fractal zooms and its model-view rotates every frame — but a real application could), and
/// an unbounded wait would hang the application in `vkWaitForFences` until Mesa's ~3.5 s stall-abort.
/// Completing after this bound with the unchanged — and therefore still correct — bytes avoids that.
///
/// # Why this value
/// It must sit comfortably **above** the inter-submit round trip (a draw submit's readback lands within
/// a single network RTT of its copy submit, single-digit milliseconds on a LAN) so a normal frame never
/// waits it out, and comfortably **below** both Mesa's ~3.5 s abort and S's own `QUEUE_REGISTER_DEADLINE`
/// (5 s) so it is the *first* backstop to act. 250 ms satisfies both with wide margin.
pub const READBACK_ADVANCE_BOUND: Duration = Duration::from_millis(250);

/// Decide whether a pending readback delivery may complete now.
///
/// # Inputs / outputs
/// - `readback_advanced`: whether S has written new bytes into an application blob since the last
///   delivery — i.e. whether `Applier::take_app_blob_writes` returned a non-empty set this poll. This is
///   the proof that a readback-bearing submit (not a bare copy submit) has retired.
/// - `pending_elapsed`: how long the current delivery has been pending. Only consulted for the
///   identical-frame fallback described on [`READBACK_ADVANCE_BOUND`].
/// - Returns `true` to complete the delivery now, `false` to keep it pending and poll again.
///
/// # Failure modes
/// None; it is a total function of its two arguments.
pub fn readback_delivery_ready(readback_advanced: bool, pending_elapsed: Duration) -> bool {
    // A fresh readback is the normal, immediate completion; the elapsed-time fallback only rescues the
    // pathological identical-frame case, where the unchanged bytes are already correct.
    readback_advanced || pending_elapsed >= READBACK_ADVANCE_BOUND
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The normal path: the readback advanced, so the delivery completes at once regardless of how
    /// little time has passed. This is the every-frame case for any well-behaved application.
    #[test]
    fn a_fresh_readback_completes_immediately() {
        assert!(readback_delivery_ready(true, Duration::ZERO));
    }

    /// The defect's fix: the readback has not advanced (a copy submit fired the trigger, or the draw's
    /// readback DMA has not landed) and we are still within the bound, so the delivery must NOT complete
    /// — completing here is exactly what shipped the previous frame's pixels.
    #[test]
    fn an_unadvanced_readback_waits_within_the_bound() {
        assert!(!readback_delivery_ready(false, READBACK_ADVANCE_BOUND / 2));
    }

    /// The identical-frame fallback: the readback never advances, so past the bound the delivery
    /// completes anyway rather than hanging the application until Mesa's stall-abort.
    #[test]
    fn an_unadvanced_readback_completes_past_the_bound() {
        assert!(readback_delivery_ready(false, READBACK_ADVANCE_BOUND));
        assert!(readback_delivery_ready(false, READBACK_ADVANCE_BOUND + Duration::from_millis(1)));
    }
}
