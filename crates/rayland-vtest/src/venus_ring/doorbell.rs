//! The **doorbell**: reading a ring's identity out of `vkCreateRingMESA`, and building a
//! `vkNotifyRingMESA` to wake that ring's consumer.
//!
//! # Why a *host* needs to build a doorbell at all — the (c)1 Task 6 finding
//! On one machine, nothing here would exist. Mesa rings the doorbell; the host consumes it. (c)1
//! Task 6 was the first time C and S ran against each other, and it established that **the doorbell
//! does not survive the split** — the application hangs and Mesa aborts. The chain is short and
//! every link is in the source:
//!
//! 1. virglrenderer's ring thread parks. After `idleTimeout` with no new commands it sets the IDLE
//!    bit **in its own copy of the ring's `status` word** and blocks on a condition variable
//!    (`vkr_ring_thread`, `vkr_ring.c:265-285`). Mesa sets that timeout to **1 ms**
//!    (`VN_RING_IDLE_TIMEOUT_NS`, `vn_ring.c:18`), which is *shorter than a relay hop*, so on a
//!    networked setup the thread is parked most of the time rather than rarely.
//! 2. Only `vkNotifyRingMESA` wakes it (`vkr_ring_notify`, `vkr_ring.c:368-378`).
//! 3. Mesa sends that doorbell **only if it reads the IDLE bit** — plus a ≥1 ms throttle
//!    (`vn_ring.c:471-481`).
//! 4. **But Mesa reads C's `status` word, and the parked thread is on S.** They are different words
//!    in different machines' memory, and nothing in (c)1 connects them. What C's word actually
//!    reports is C's own ring *watcher's* park state — a thread that is a relay, not the ring's
//!    consumer. So Mesa's doorbell decision is driven by the wrong thread entirely, and the ring's
//!    `status` word turns out to be a shared-memory channel the spec's §5 inventory never listed.
//!
//! Shipping S's `status` back to C does **not** fix it, and the reason is worth recording so nobody
//! spends a day rediscovering it: Mesa decides on the doorbell *at submit time* and never revisits
//! it (`vn_ring_submit_command`). By the time C learns S has parked, Mesa has already made — and
//! throttled — its decision, and is blocked in `vn_ring_wait_seqno` where no doorbell will ever be
//! sent again. The information arrives strictly too late, however fast the link.
//!
//! **So the wake has to be generated where the knowledge is: on S.** S is the only party that knows
//! both that new ring bytes arrived and what its own consumer is doing. That is what this module is
//! for.
//!
//! # Why an unconditional doorbell is correct, and a conditional one is a hang
//! It is tempting for S to read its ring's `status`, and only ring the doorbell when the IDLE bit
//! is actually set. **That is subtly broken.** The thread's park sequence is:
//!
//! ```c
//! /* vkr_ring.c:265-285 */
//! if (vkr_ring_now() >= last_submit + ring->idle_timeout) {
//!    ring->pending_notify = false;
//!    vkr_ring_set_status_bits(ring, VK_RING_STATUS_IDLE_BIT_MESA);
//!    wait = ring->buffer.cur == vkr_ring_load_tail(ring);   /* re-reads tail */
//!    if (!wait) vkr_ring_unset_status_bits(ring, VK_RING_STATUS_IDLE_BIT_MESA);
//! }
//! if (wait) {
//!    mtx_lock(&ring->mutex);
//!    if (ring->started && !ring->pending_notify)
//!       cnd_wait(&ring->cond, &ring->mutex);   /* parks here */
//!    ...
//! ```
//!
//! A conditional doorbell reads `status`, sees IDLE clear, and skips — and the thread parks a
//! microsecond later. Nothing ever wakes it. The window is real and it is exactly the interval
//! between the two lines above.
//!
//! Unconditional has no such gap, because of two properties of that same code:
//!
//! - **The thread re-reads `tail` after setting IDLE and only parks if it is caught up.** So if the
//!   `Release` store of `tail` lands before that re-read, the thread does not park at all — this is
//!   the same double-check-before-park discipline `rayland-c`'s own watcher uses.
//! - **`pending_notify` closes the rest.** If the doorbell lands after the re-read but before
//!   `mtx_lock`, the thread sees `pending_notify` and skips `cnd_wait`; if it lands after that, the
//!   `cnd_signal` wakes it. And if it lands *before* `pending_notify = false`, the flag is cleared —
//!   but then the `tail` re-read (which follows) sees the new bytes, so `wait` is false anyway.
//!
//! In every interleaving the thread ends up looking at the new `tail`, **provided the doorbell is
//! rung after the `Release` store of `tail`, never before.** That ordering is this module's one
//! real contract, and it is the caller's to honour — see [`notify_ring_command`].
//!
//! # Cost, stated rather than hidden
//! This makes S ring a doorbell on **every** relayed delta, where a shared-memory host rings none
//! (ring-findings §5.2 measured the steady state at zero notifications in either direction). It is
//! not free: each one is a `virgl_renderer_submit_cmd` into the render-server subprocess. It is,
//! however, entirely **local to S** — no network bytes — and it is what the doorbell's shared page
//! bought for free on one machine. Buying it back properly (only ringing when S's consumer is
//! genuinely parked, using S's own live `status` word, with the park race closed some other way) is
//! a (c)2 optimization, and the measurement that would justify it does not exist yet.

// The command types this module reads and writes; the repository's single copy of that knowledge.
use super::decode::{VK_COMMAND_TYPE_VK_CREATE_RING_MESA, VK_COMMAND_TYPE_VK_NOTIFY_RING_MESA};

/// Byte offset of the `ring` handle inside a `vkCreateRingMESA` or `vkNotifyRingMESA` command.
///
/// Both encoders emit the same prologue and then the handle first (`vn_encode_vkCreateRingMESA`,
/// `vn_encode_vkNotifyRingMESA`, `vn_protocol_driver_transport.h:736,822`):
///
/// ```text
///   [0..4]   VkCommandTypeEXT   (u32)  -- 188 or 190
///   [4..8]   VkCommandFlagsEXT  (u32)
///   [8..16]  ring               (u64)  -- the handle, here
/// ```
///
/// Everything after byte 16 differs between the two and is deliberately not modelled: reading the
/// handle needs no knowledge of `vkCreateRingMESA`'s variable-size `VkRingCreateInfoMESA` payload,
/// so this module does not acquire any.
const RING_HANDLE_OFFSET: usize = 8;

/// The number of bytes required to hold a command's prologue plus its ring handle.
const RING_HANDLE_END: usize = RING_HANDLE_OFFSET + 8;

/// The exact size of an encoded `vkNotifyRingMESA`.
///
/// `[type][flags][ring][seqno][flags]` = `4 + 4 + 8 + 4 + 4` (`vn_sizeof_vkNotifyRingMESA`,
/// `vn_protocol_driver_transport.h:809-820`). A live capture confirms it: (c)1 Task 6 observed
/// Mesa's own doorbells arriving on the inline path at exactly 24 bytes.
pub const NOTIFY_RING_COMMAND_BYTES: usize = 24;

/// Read the ring handle out of an inline `vkCreateRingMESA`, or `None` if `cmd` is not one.
///
/// # What this is for
/// S needs a ring's handle in order to ring its doorbell ([`notify_ring_command`]), and the handle
/// is a *Mesa pointer value* — S cannot invent or derive it, and it appears exactly once per ring,
/// in the `vkCreateRingMESA` that S already forwards on the inline path.
///
/// # Why reading these eight bytes does not violate the "no decoding" rule
/// The spec (§7) forbids decoding **the ring** to make a correctness decision, on the grounds that a
/// decoding bug would become a corruption bug. That rule is intact here, for three reasons worth
/// separating:
///
/// 1. **This is not the ring.** It is the inline vtest path — a different channel, measured at
///    140–236 bytes for an entire Vulkan initialization (ring-findings §2), of which
///    `vkCreateRingMESA` is the first command.
/// 2. **It is a fixed offset in a fixed prologue**, not a walk over a variable-length stream. There
///    is no cursor to lose, and nothing after byte 16 is looked at.
/// 3. **A wrong answer cannot corrupt anything.** The handle is only ever used to *name* a ring in a
///    doorbell that carries no data. virglrenderer looks it up and, if it does not match, calls
///    `vkr_context_set_fatal` (`vkr_dispatch_vkNotifyRingMESA`, `vkr_transport.c:301-305`) — so a
///    mistake here is a loud, immediate failure on S, not a silent misrender. That is the opposite
///    of the risk §7 exists to prevent.
///
/// # Inputs / outputs
/// - `cmd`: an inline command batch, exactly as it arrived on the vtest socket.
/// - Returns the ring handle if `cmd` begins with a `vkCreateRingMESA`, else `None`.
///
/// # Pitfall: this reads only the batch's *first* command
/// A `VCMD_SUBMIT_CMD2` batch may in principle carry several commands, and this looks at the first
/// one only. That is sufficient and is not a shortcut being taken quietly: Mesa encodes
/// `vkCreateRingMESA` into its own local encoder and submits it alone (`vn_ring.c:366-369`), and
/// every live capture shows it arriving as a batch of one. A batch that buried a ring creation
/// behind another command would be missed here, and the symptom would be honest — S would never
/// learn the handle, never ring a doorbell, and the ring would visibly stall rather than
/// misbehave.
pub fn ring_handle_from_create(cmd: &[u8]) -> Option<u64> {
    // Too short to be this command at all. Checked first so the reads below cannot panic.
    if cmd.len() < RING_HANDLE_END {
        return None;
    }
    // The command type: little-endian, as every `vn_cs_encoder` field is.
    let cmd_type = u32::from_le_bytes([cmd[0], cmd[1], cmd[2], cmd[3]]);
    if cmd_type != VK_COMMAND_TYPE_VK_CREATE_RING_MESA {
        return None;
    }
    // The handle itself. `expect` is unreachable: the length check above guarantees the slice.
    let handle = cmd[RING_HANDLE_OFFSET..RING_HANDLE_END]
        .try_into()
        .expect("the length check above guarantees exactly eight bytes");
    Some(u64::from_le_bytes(handle))
}

/// Build a `vkNotifyRingMESA` naming `ring_handle`: the doorbell that wakes a parked ring thread.
///
/// # The caller's ordering contract, which is the whole correctness of this
/// **Ring this only after the ring's `tail` has been stored with `Release`.** The module docs walk
/// every interleaving; the short version is that each one is safe *because* the new `tail` is
/// already visible when the consumer looks, and reversing the order reintroduces exactly the lost
/// wakeup this module exists to prevent.
///
/// # Why `seqno` and `flags` are zero rather than something meaningful
/// **virglrenderer ignores both.** `vkr_dispatch_vkNotifyRingMESA` (`vkr_transport.c:290-308`) uses
/// `args->ring` to look the ring up and then calls `vkr_ring_notify(ring)`; `args->seqno` and
/// `args->flags` are never read. So there is no value they could carry that would change anything,
/// and inventing a plausible-looking `seqno` would only invite a future reader to believe it was
/// load-bearing. Mesa fills `seqno` with its `tail` — the doorbell means *"my tail is now X, come
/// look"* — but the "come look" is the entire message, and the consumer re-reads `tail` itself.
///
/// # Inputs / outputs
/// - `ring_handle`: the value [`ring_handle_from_create`] read from `vkCreateRingMESA`. A handle
///   virglrenderer does not know is a **fatal** error on S, not a silent no-op.
/// - Returns exactly [`NOTIFY_RING_COMMAND_BYTES`] bytes, ready for
///   [`RenderEngine::submit`](crate::RenderEngine::submit) — i.e. the **context** decoder's path
///   (`vkr_context.c:170-173`), which is where `vkr_dispatch_vkNotifyRingMESA` insists it be
///   dispatched from (`is_dispatched_from_vkr_context`, `vkr_transport.c:295-299`). Sending it as a
///   ring delta would splice it into the byte stream it is supposed to be announcing.
pub fn notify_ring_command(ring_handle: u64) -> Vec<u8> {
    let mut cmd = Vec::with_capacity(NOTIFY_RING_COMMAND_BYTES);
    // `[type]`: the doorbell's command type.
    cmd.extend_from_slice(&VK_COMMAND_TYPE_VK_NOTIFY_RING_MESA.to_le_bytes());
    // `[flags]`: `VkCommandFlagsEXT`. Zero means "no reply wanted", which is right — a doorbell that
    // generated a reply would make S wait for itself.
    cmd.extend_from_slice(&0u32.to_le_bytes());
    // `[ring]`: the only field that carries information.
    cmd.extend_from_slice(&ring_handle.to_le_bytes());
    // `[seqno]` and `[flags]`: ignored by the renderer. See the doc comment.
    cmd.extend_from_slice(&0u32.to_le_bytes());
    cmd.extend_from_slice(&0u32.to_le_bytes());
    debug_assert_eq!(cmd.len(), NOTIFY_RING_COMMAND_BYTES);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ring handle from (c)1 Task 6's live run: a Mesa pointer value, which is what makes the
    /// point that S could never have derived it and must read it off the wire.
    const CAPTURED_RING_HANDLE: u64 = 0x633c_d1c5_7150;

    /// A doorbell must be the exact 24 bytes `vn_sizeof_vkNotifyRingMESA` computes, laid out in the
    /// order `vn_encode_vkNotifyRingMESA` emits. The live capture confirms the length independently:
    /// Task 6 observed Mesa's own doorbells arriving at exactly this size.
    #[test]
    fn a_doorbell_matches_mesa_s_encoding_byte_for_byte() {
        let cmd = notify_ring_command(CAPTURED_RING_HANDLE);

        assert_eq!(
            cmd.len(),
            NOTIFY_RING_COMMAND_BYTES,
            "virglrenderer counts commands in dwords and sizes this one exactly; a different \
             length is a different command"
        );
        let mut expected = Vec::new();
        expected.extend_from_slice(&190u32.to_le_bytes()); // VK_COMMAND_TYPE_vkNotifyRingMESA_EXT
        expected.extend_from_slice(&0u32.to_le_bytes()); // cmd_flags: no reply
        expected.extend_from_slice(&CAPTURED_RING_HANDLE.to_le_bytes());
        expected.extend_from_slice(&0u32.to_le_bytes()); // seqno: ignored by the renderer
        expected.extend_from_slice(&0u32.to_le_bytes()); // flags: ignored by the renderer
        assert_eq!(cmd, expected);
    }

    /// A doorbell must be a whole number of dwords, or virglrenderer rejects the batch outright.
    #[test]
    fn a_doorbell_is_a_whole_number_of_dwords() {
        assert_eq!(notify_ring_command(1).len() % 4, 0);
    }

    /// The handle must round-trip out of a `vkCreateRingMESA` built the way Mesa builds one: the
    /// prologue, the handle, then a `VkRingCreateInfoMESA` this module deliberately knows nothing
    /// about. The trailing bytes are present precisely to prove they are not consulted.
    #[test]
    fn the_ring_handle_is_read_out_of_a_create_ring_command() {
        let mut cmd = Vec::new();
        cmd.extend_from_slice(&188u32.to_le_bytes()); // VK_COMMAND_TYPE_vkCreateRingMESA_EXT
        cmd.extend_from_slice(&0u32.to_le_bytes()); // cmd_flags
        cmd.extend_from_slice(&CAPTURED_RING_HANDLE.to_le_bytes());
        // A stand-in for the pointer marker and the VkRingCreateInfoMESA that really follow.
        cmd.extend_from_slice(&[0xab; 96]);

        assert_eq!(ring_handle_from_create(&cmd), Some(CAPTURED_RING_HANDLE));
    }

    /// The doorbell Mesa itself sends is *not* a ring creation, and must not be mistaken for one —
    /// it carries a handle at the very same offset, so a check that forgot the command type would
    /// "work" on it and latch the right value for the wrong reason.
    #[test]
    fn a_doorbell_is_not_mistaken_for_a_ring_creation() {
        let doorbell = notify_ring_command(CAPTURED_RING_HANDLE);
        assert_eq!(
            ring_handle_from_create(&doorbell),
            None,
            "only vkCreateRingMESA declares a ring; latching a handle from anything else would be \
             right by accident"
        );
    }

    /// Any other command is refused. `vkEnumerateInstanceVersion` is the realistic case: it is
    /// command #2 of every capture.
    #[test]
    fn an_unrelated_command_yields_no_handle() {
        let mut cmd = Vec::new();
        cmd.extend_from_slice(&2u32.to_le_bytes());
        cmd.extend_from_slice(&0u32.to_le_bytes());
        cmd.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(ring_handle_from_create(&cmd), None);
    }

    /// A truncated command must yield `None` rather than panic. These bytes arrive from Mesa over a
    /// socket; a short read must not take the daemon down.
    #[test]
    fn a_truncated_command_is_refused_rather_than_panicking() {
        // The right command type, but the handle is cut in half.
        let mut cmd = Vec::new();
        cmd.extend_from_slice(&188u32.to_le_bytes());
        cmd.extend_from_slice(&0u32.to_le_bytes());
        cmd.extend_from_slice(&[0u8; 4]);
        assert_eq!(ring_handle_from_create(&cmd), None);
        // And the degenerate cases.
        assert_eq!(ring_handle_from_create(&[]), None);
        assert_eq!(ring_handle_from_create(&[188, 0, 0, 0]), None);
    }
}
