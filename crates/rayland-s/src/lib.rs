//! **`rayland-s`**: the machine with the GPU. It replays the Venus command stream `rayland-c` relays
//! to it, on a real virglrenderer, and reports back what its engine actually did.
//!
//! # The one-paragraph version
//! An application runs on **C**, a machine with no usable GPU. Mesa's Venus ICD serializes its Vulkan
//! calls, but — and this is C0's central finding — it does **not** put them on a socket. It `memcpy`s
//! them into a shared-memory **command ring** and stores a new `tail`, and that is the whole
//! notification (ring-findings §2: 100% of the application's commands are in the ring, 0% on the
//! socket). On one machine the host's ring thread simply reads the same physical pages. Across a
//! network there is no shared page, so `rayland-c` polls that ring, ships the bytes, and **this
//! crate lays them back down** into the equivalent ring memory on S — where S's own virglrenderer
//! ring thread is polling for exactly them.
//!
//! # The thing to understand before reading any of this
//! **S does not "receive commands and execute them".** There is no function to call. The Venus
//! command stream reaches S's engine by being *written into memory that a thread inside
//! virglrenderer is already polling* — see [`ring_mirror`] for the source that pins this, because it
//! is counter-intuitive and this task's brief specified the opposite. The engine trait's `submit`
//! **is** used, but only for the inline vtest path, which ring-findings §2 measured at 140–236 bytes
//! across an entire Vulkan initialization, none of it application drawing.
//!
//! So the two halves of this crate are:
//!
//! - [`apply`] — the message-driven half: what to do with each `C2S`, and what S owes back.
//! - [`ring_mirror`] / [`blob`] — the memory half: S's writable view of the pages its own engine
//!   allocated, and the arithmetic that puts C's bytes at the offsets virglrenderer will read them
//!   from.
//!
//! # Status: what has and has not been run
//! **This crate has never run against a real `rayland-c`.** The QUIC transport is (c)1 Task 6. Task
//! 5 shipped the blob synchronisation and Task 5b corrected its S→C half to spec §7.2's rule — **S
//! ships back exactly the bytes S wrote** — so every channel spec §5 lists as (c)1's now has an
//! owner: the application's vertex buffer reaches S, its rendered pixels go back, and so do the
//! command replies in channel 2's reply arena, which nothing carried before 5b. See
//! `Applier::poll_progress` for the rule and for the two ways the rule it replaced was wrong. What
//! *is* covered, and covered hard, is everything that does not need a peer:
//! `tests/apply.rs` exercises every message against a real shared-memory mapping, with no GPU, no
//! Mesa and no network — including the ring wrap, which ring-findings §8 records has **never been
//! reached in a live run** and is therefore untested code in Mesa, in virglrenderer, and here.
//!
//! What remains genuinely unverified is that the bytes S writes are *executed*: nothing here runs
//! virglrenderer's ring thread. That is Task 6's loopback end-to-end test, and it is the first time
//! the two halves meet. The distinction is stated rather than blurred, because this branch has
//! repeatedly shipped tests that asserted more than they could detect.

// The message-driven half: what S does with each `C2S`, and what it owes back.
pub mod apply;
// S's writable view of a blob resource's shared pages.
pub mod blob;
// S's side of the command ring: where C's relayed bytes are laid down for virglrenderer to find.
pub mod ring_mirror;
