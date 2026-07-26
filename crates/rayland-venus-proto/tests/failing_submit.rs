//! Does *our* decoder accept the exact `vkQueueSubmit` bytes virglrenderer refuses?
//!
//! This settles a contradiction static analysis could not. Our vendored `venus-protocol` headers are
//! byte-identical to virglrenderer 1.2.0's (all 42 files), so both run the *same* generated
//! `vn_decode_vkQueueSubmit_args_temp` over the *same* bytes — yet virglrenderer sets its decoder's
//! fatal flag and we appear not to. Every other branch that could set that flag has been refuted
//! against real source: the handler is installed (`vkr_queue.c:654`), `vkr_dispatch_vkQueueSubmit`
//! sets no fatal at all, a failed object lookup would log `failed to look up object` (it did not),
//! and a temp-pool failure would log `failed to suballocate` (it did not).
//!
//! So either the two decoders genuinely disagree — meaning our replacement `vkr_cs.h` primitives
//! diverge from virglrenderer's — or the belief "our decoder accepts these bytes" was inferred from
//! the ring dump rather than measured. This test measures it.

/// The refused submit, captured verbatim from a live vkcube run (`[ring-queue] submit ... bytes(120)`).
/// By hand it reads: queue `0x6`, one `VkSubmitInfo` (wait semaphore `0x20`, dst stage `0x400`,
/// command buffer `0x19`, signal semaphore `0x2f`), fence `0x1f` — the second swapchain image's set.
const REFUSED: &str = "12000000000000000600000000000000010000000100000000000000040000000000000000000000010000000100000000000000200000000000000001000000000000000004000001000000010000000000000019000000000000000100000001000000000000002f000000000000001f00000000000000";

/// The accepted submit from the same run: identical in structure, differing only in which image's
/// handles it names (wait `0x1e`, command buffer `0x18`, signal `0x2e`, fence `0x1d`). This is the
/// control — whatever the decoder reports for one it must report for the other, or the difference is
/// in the bytes after all.
const ACCEPTED: &str = "120000000000000006000000000000000100000001000000000000000400000000000000000000000100000001000000000000001e0000000000000001000000000000000004000001000000010000000000000018000000000000000100000001000000000000002e000000000000001d00000000000000";

/// Decode a hex string into bytes. Panics on malformed input, which in a test vector is a bug in the
/// test rather than a condition to handle.
fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("test vector is valid hex"))
        .collect()
}

#[test]
fn our_decoder_on_the_bytes_virglrenderer_refuses() {
    let refused = hex(REFUSED);
    let accepted = hex(ACCEPTED);
    assert_eq!(refused.len(), 120, "captured length");
    assert_eq!(accepted.len(), 120, "captured length");

    let r = rayland_venus_proto::command_len(&refused);
    let a = rayland_venus_proto::command_len(&accepted);
    // Printed, not merely asserted: the values are the finding, whichever way they fall.
    println!("refused  -> {r:?}");
    println!("accepted -> {a:?}");

    // The control must decode: virglrenderer executed this one without complaint.
    assert!(a.is_ok(), "the ACCEPTED submit must decode; got {a:?}");
    assert_eq!(a.as_ref().unwrap().len, 120, "accepted consumes its whole command");

    // The finding under test.
    assert!(
        r.is_ok(),
        "our decoder rejects the refused submit too — then the bytes ARE malformed and \
         virglrenderer is right to refuse them; got {r:?}"
    );
    assert_eq!(r.as_ref().unwrap().len, 120, "refused consumes its whole command");
}
