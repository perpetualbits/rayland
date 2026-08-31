//! WP0 Task 4.1 — the bridge between the daemon's live state and the Wayland proxy's abstract seams.
//!
//! `wayland_proxy` defines two traits it needs satisfied but does not itself know how to satisfy: a
//! [`WaylandSink`] (where a translated request goes) and a [`ResourceResolver`] (turning a passed
//! swapchain fd's inode into an S-side resource id). Task 3b proved the proxy against stub
//! implementations; this module supplies the *real* ones, wired to the daemon's QUIC link and blob table,
//! so `wayland_proxy::run` can be spawned inside the daemon.
//!
//! Keeping this bridge in its own module keeps `wayland_proxy` free of any daemon/link/blob dependency
//! (it stays unit-testable in isolation) and keeps the daemon's `main.rs` free of the trait glue.

use std::sync::{Arc, Mutex};

use rayland_relay::{C2S, WaylandMessage};

use crate::link::QuicSendLink;
use crate::relay_engine::{BlobTable, RelayLink};
use crate::wayland_proxy::{ResourceResolver, WaylandSink};

/// A [`WaylandSink`] that forwards each translated request to S over the daemon's existing QUIC link.
///
/// It holds a clone of the same `Arc<Mutex<QuicSendLink>>` the ring watcher and vtest thread send
/// through, so a Wayland request crosses on the one connection alongside the ring/blob traffic — no
/// second link. The send is wrapped as [`C2S::WaylandRequest`].
pub struct LinkSink {
    /// The shared send half of the link to S. Locked only for the duration of one framed send, never
    /// held across anything else — the same discipline every other producer on this mutex follows.
    tx: Arc<Mutex<QuicSendLink>>,
}

impl LinkSink {
    /// Build a sink over the daemon's shared send link.
    pub fn new(tx: Arc<Mutex<QuicSendLink>>) -> Self {
        LinkSink { tx }
    }
}

impl WaylandSink for LinkSink {
    /// Forward one translated request to S as a [`C2S::WaylandRequest`].
    ///
    /// Fire-and-forget: WP0 does not acknowledge individual Wayland requests, so a send failure is
    /// logged rather than propagated — there is no caller waiting on a return value here (the proxy's
    /// object-dispatch hook has none), and the link failing is a session-fatal condition the reader
    /// thread surfaces independently. Locking the mutex briefly matches the ring watcher's discipline.
    fn forward_request(&self, msg: WaylandMessage) {
        // Lock only for the framed send; drop the guard immediately after.
        let mut link = self
            .tx
            .lock()
            .expect("the send-link mutex is never poisoned");
        if let Err(e) = link.send(&C2S::WaylandRequest { message: msg }) {
            // A failed send is not traffic and not recoverable here; the reader thread will see the
            // link die too. Log so the cause is visible rather than silently dropping the request.
            eprintln!("rayland-c: forwarding a Wayland request to S failed: {e}");
        }
    }

    /// Forward a global bind to S as a [`C2S::WaylandBind`], on the same link and with the same
    /// fire-and-forget discipline as [`Self::forward_request`]. It must reach S **before** any request
    /// naming the bound object; the proxy's single dispatch thread calls this synchronously from
    /// `GlobalHandler::bind`, before the object's first request, and the link preserves order — so the
    /// causal ordering (bind, then requests) is maintained.
    fn forward_bind(&self, interface: &str, version: u32, app_object_id: u32) {
        let mut link = self
            .tx
            .lock()
            .expect("the send-link mutex is never poisoned");
        let bind = C2S::WaylandBind {
            interface: interface.to_string(),
            version,
            app_object_id,
        };
        if let Err(e) = link.send(&bind) {
            eprintln!("rayland-c: forwarding a Wayland bind to S failed: {e}");
        }
    }
}

/// A [`ResourceResolver`] that maps a passed fd's memfd inode to the S-side resource id, by scanning
/// the daemon's blob table.
///
/// The swapchain image's dma-buf fd Mesa hands the compositor at `params.add` is the exact memfd
/// `rayland-c` allocated for that resource (WP0 Task-1 spike). Each [`crate::shm::LocalBlob`] records
/// its memfd's inode at creation, and the blob table is keyed by the resource id S assigned — so the
/// lookup is a scan for the blob whose inode matches, returning its key.
///
/// A linear scan is right here: the table holds at most a few dozen blobs, and `params.add` is a rare
/// event (once per swapchain image), so an index would be more machinery than the cost warrants.
pub struct BlobInodeResolver {
    /// The daemon's blob table — the same `Arc` the reader thread commits into. Locked only for the
    /// scan, never across a send, per [`BlobTable`]'s discipline.
    blobs: BlobTable,
}

impl BlobInodeResolver {
    /// Build a resolver over the daemon's blob table.
    pub fn new(blobs: BlobTable) -> Self {
        BlobInodeResolver { blobs }
    }
}

impl ResourceResolver for BlobInodeResolver {
    /// Return the resource id whose blob's memfd has inode `(dev, ino)`, or `None` if no tracked blob
    /// matches (a foreign fd the proxy must not turn into a token).
    fn resolve_inode(&self, dev: u64, ino: u64) -> Option<u32> {
        // Scan the table for the blob whose recorded inode matches; its key is the resource id.
        // `lock_mut` because a match is also a *decision about that blob's future*: see below.
        let mut blobs = self
            .blobs
            .lock()
            .expect("the blob table mutex is never poisoned");
        let found = blobs
            .iter()
            .find_map(|(res_id, blob)| (blob.inode() == (dev, ino)).then_some(*res_id))?;
        // **Mark it presented.** Resolving this inode is C deciding that this blob becomes a
        // `wl_buffer` on S's compositor — which means S's GPU will render into it and, since the
        // 2026-08-29 presented-buffer exclusion, will deliberately NOT report those writes back to C.
        // C's baseline for it is therefore stale by design, and the forward diff must not coalesce
        // its runs (coalescing re-ships the unchanged gap bytes *from that stale baseline*, laying
        // C's old news over S's freshly rendered pixels). See `LocalBlob::presented`.
        //
        // This runs at `params.add`, before `create_immed` decides whether a token is really emitted,
        // so it marks a superset of the blobs that truly become buffers. That is the safe direction:
        // an over-mark costs a missed optimisation, an under-mark costs corrupted pixels.
        if let Some(blob) = blobs.get_mut(&found) {
            blob.note_presented();
        }
        Some(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::LocalBlob;
    use std::collections::HashMap;
    use std::os::fd::{AsFd, AsRawFd};

    /// The resolver returns the resource id of the blob whose memfd inode matches the queried fd, and
    /// `None` for an inode no blob owns. This is the buffer-by-token correlation the whole WP0 present
    /// path depends on.
    #[test]
    fn resolves_a_blobs_inode_to_its_resource_id() {
        // Two blobs registered under two resource ids, as the reader thread would commit them.
        let (blob_a, fd_a) = LocalBlob::create(1, 4096).expect("blob a");
        let (blob_b, _fd_b) = LocalBlob::create(2, 4096).expect("blob b");
        let ino_a = blob_a.inode();

        let mut table = HashMap::new();
        table.insert(77u32, blob_a);
        table.insert(88u32, blob_b);
        let resolver = BlobInodeResolver::new(Arc::new(Mutex::new(table)));

        // The fd for blob_a resolves to its resource id (77).
        assert_eq!(resolver.resolve_inode(ino_a.0, ino_a.1), Some(77));

        // An independent memfd's inode (no blob owns it) resolves to None.
        let (_other, other_fd) = LocalBlob::create(3, 4096).expect("an unrelated blob");
        let (odev, oino) = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            assert_eq!(libc::fstat(other_fd.as_fd().as_raw_fd(), &mut st), 0);
            (st.st_dev as u64, st.st_ino as u64)
        };
        assert_eq!(resolver.resolve_inode(odev, oino), None);

        // Keep fd_a alive to the end so the memfd (and thus its inode) is unambiguously live.
        drop(fd_a);
    }

    /// **Resolving an inode marks that blob presented, and marks no other.**
    ///
    /// This is the wiring the C→S coalescing safety rule hangs on, and it is tested separately
    /// because the rule is *unreachable* without it: `messages_for_delta` asks `is_presented()`, and
    /// if nothing ever sets the flag then every blob — swapchain images included — gets coalesced and
    /// C re-ships its stale copy of bytes S rendered. That failure is silent, produces no error, and
    /// shows up only as corrupted pixels on someone's screen.
    ///
    /// Resolving is the right moment because it is exactly when C decides this memfd is the swapchain
    /// image behind a `params.add`. It is deliberately earlier than `create_immed`, so the mark is a
    /// superset of the blobs that truly become buffers — an over-mark costs a missed optimisation,
    /// an under-mark costs pixels.
    #[test]
    fn resolving_an_inode_marks_that_blob_presented_and_no_other() {
        let (blob_a, fd_a) = LocalBlob::create(1, 4096).expect("blob a");
        let (blob_b, _fd_b) = LocalBlob::create(2, 4096).expect("blob b");
        let ino_a = blob_a.inode();

        let mut table = HashMap::new();
        table.insert(77u32, blob_a);
        table.insert(88u32, blob_b);
        let blobs = Arc::new(Mutex::new(table));
        let resolver = BlobInodeResolver::new(blobs.clone());

        // Nothing is presented until something is resolved.
        {
            let t = blobs.lock().expect("the blob table");
            assert!(
                !t[&77].is_presented(),
                "nothing is presented before a resolve"
            );
            assert!(!t[&88].is_presented());
        }

        assert_eq!(resolver.resolve_inode(ino_a.0, ino_a.1), Some(77));

        {
            let t = blobs.lock().expect("the blob table");
            assert!(
                t[&77].is_presented(),
                "the resolved blob must be marked presented, or the forward diff will coalesce a \
                 swapchain image and re-ship C's stale copy over S's rendered pixels"
            );
            assert!(
                !t[&88].is_presented(),
                "an unrelated blob must NOT be marked — over-marking every blob would silently \
                 disable coalescing everywhere and this test would still pass on the line above"
            );
        }
        drop(fd_a);
    }
}
