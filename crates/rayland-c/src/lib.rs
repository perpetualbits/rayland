//! **`rayland-c`**: the daemon that runs on machine **C**, where the application is.
//!
//! # What this crate is, in one paragraph
//! Mesa's Venus ICD does not send Vulkan commands over its socket — it writes them into a
//! shared-memory ring, and the socket carries only ring management. C0 proved that
//! (`docs/design/2026-07-15-venus-ring-findings.md`), and it is what killed (c)1's original
//! one-line scope of *"swap the local socket for QUIC"*: there is no socket carrying commands to
//! swap. The insight this crate rests on is that **the vtest protocol's "host" is whoever allocates
//! that ring, and we can be that host.** So `rayland-c` speaks stock vtest over a *local* Unix
//! socket, hands the application plain local memfds for its ring and its blobs, watches the ring,
//! and relays the bytes to S. Stock Mesa, stock application, no fork, no patch. The application
//! cannot tell.
//!
//! # The S / C vocabulary (X11-era, and the reverse of cloud usage)
//! **C** is where the *application executable* runs: possibly weak, possibly headless, eventually
//! RISC-V, and with **no GPU**. **S** is where the *user sits*: the display, the GPU, the
//! compositor. This crate is C's half. It must never link a GPU stack, which
//! `tests/no_gpu_linkage.rs` enforces mechanically rather than by convention — if `rayland-c` ever
//! links `libvirglrenderer`, the project's central claim is quietly false.
//!
//! # Layout, and where the interesting problem is
//! - [`shm`] — the local memfd shadows Mesa maps. The mechanism that lets an unpatched Mesa work.
//! - [`ring`] — **the ring watcher.** The load-bearing piece, and the one most likely to hang. Read
//!   its module docs before touching it; the hang it avoids is subtle, intermittent, and named.
//! - [`relay_engine`] — the [`RenderEngine`](rayland_vtest::RenderEngine) whose GPU is another
//!   machine.
//! - [`link`] — the [`RelayLink`](relay_engine::RelayLink) over SP2's QUIC transport. Thin on
//!   purpose: `rayland-transport` owns the QUIC, and this is the adapter to the relay's framing.
//! - [`blob_sync`] — what must cross the wire *alongside* a ring delta, and in what order. The
//!   ring is not the whole story: the application writes its vertices straight into mapped memory
//!   with no API call to intercept (ring-findings §6), so those bytes have to be shipped, and they
//!   have to arrive **before** the commands that read them.
//!
//! A reader looking for "where the application's Vulkan commands are handled" will instinctively go
//! to [`relay_engine`], because that is where the trait methods are. **They are not there.**
//! `RenderEngine::submit` never sees a single application Vulkan command — it sees the ring's
//! address and then a series of pokes (ring-findings §2). The commands are in [`ring`], read out of
//! shared memory that no trait method is ever called for. This is stated in three places in this
//! crate on purpose: it is the single most counter-intuitive fact in the sub-project, and it cost
//! C0 two tasks to discover.
//!
//! # Why there is a library target at all
//! The binary is the product; the library exists so the ring watcher can be tested from `tests/`
//! against a synthetic ring — a `Vec<u8>` — with no GPU, no Mesa and no network. That is not
//! convenience. A stall found only in a live drive looks like a network problem and costs a day;
//! the same stall provoked deterministically in a unit test costs a minute.

// The local memfd shadows Mesa maps and writes its command stream into.
pub mod shm;
// The ring watcher: notices Mesa's writes, extracts them, and owns the park/wake decision.
pub mod ring;
// The RenderEngine implementation that forwards to S.
pub mod relay_engine;
// The C->S half of (c)1's coherence strategy: which blobs accompany a ring delta, and in what order.
pub mod blob_sync;
// The RelayLink over SP2's QUIC transport: the network itself.
pub mod link;
