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
        let mut link = self.tx.lock().expect("the send-link mutex is never poisoned");
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
        let mut link = self.tx.lock().expect("the send-link mutex is never poisoned");
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
        let blobs = self.blobs.lock().expect("the blob table mutex is never poisoned");
        blobs
            .iter()
            .find_map(|(res_id, blob)| (blob.inode() == (dev, ino)).then_some(*res_id))
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
}
