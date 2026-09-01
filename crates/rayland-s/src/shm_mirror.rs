//! S's mirror of the application's `wl_shm` pools.
//!
//! # The one sentence to keep in mind while reading this
//! **S's memfd is a different file from the application's.** Nothing is shared, no page is mapped
//! twice, and no kernel mechanism keeps the two in step. They are the same size only because
//! [`rayland_relay::WaylandArg::ShmPool`] says so at creation and `wl_shm_pool.resize` says so again,
//! and they hold the same bytes only because [`rayland_relay::C2S::ShmPoolData`] copies them. Anything
//! that changes one size must change the other, and a mismatch is not a protocol error — it is a
//! `SIGBUS` in the compositor, or a window of garbage.
//!
//! # Why a mirror at all
//! `wl_shm.create_pool` passes a file descriptor, and a descriptor cannot cross a network. But the
//! descriptor never needs to: `rayland-c` runs on the same machine as the application and maps the
//! pool itself. So C keeps the app's fd, tells S only how large a pool to make, and S makes its own —
//! a real `wl_shm_pool` against its real compositor, backed by a memfd only S holds. See
//! `docs/superpowers/specs/2026-08-31-wl-shm-proxy-design.md` §4.
//!
//! # What this module does not do
//! It does not talk to the compositor. Creating the real `wl_shm_pool` object, and every later request
//! naming it, is [`crate::wayland_client`]'s job through the ordinary object-mapping path; this module
//! only owns the memory those objects are backed by, and hands out the descriptor at the one moment
//! the replay needs it.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};

use rayland_vtest::transport::{ShmMapping, create_memfd};

/// One mirrored pool: S's own descriptor, S's mapping of it, and how large both are.
struct MirrorPool {
    /// S's memfd. Held for the pool's life because the compositor keeps its own mapping of it, and
    /// because `resize` needs to `ftruncate` the same file rather than make a new one.
    fd: OwnedFd,
    /// S's mapping, which [`ShmMirror::write`] copies into.
    mapping: ShmMapping,
    /// The current size, in bytes. Kept so a write can be bounds-checked against the mapping that
    /// actually exists rather than against what C last said.
    size: u32,
}

/// Why a mirrored-pool operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorError {
    /// The memfd could not be created or sized. Reported rather than panicked: this is driven by a
    /// size that arrived over the network, and a daemon that dies on remote input is a worse outcome
    /// than an application whose window does not appear.
    Allocation(String),
    /// A message named a pool S has no mirror for.
    UnknownPool(u32),
    /// A write would land outside the mirror.
    ///
    /// C checks the same arithmetic before sending, so reaching this means the two sides disagree
    /// about a pool's size — exactly the drift the module docs warn about. Refusing keeps it a logged
    /// disagreement instead of a fault inside the compositor.
    WriteOutsidePool {
        /// Last byte the write would touch, exclusive.
        end: u64,
        /// The mirror's size.
        size: u32,
    },
}

/// Every `wl_shm` pool S mirrors, keyed by the **application's** object id.
///
/// Keyed by the app's id rather than S's because that is what travels: [`rayland_relay::C2S::ShmPoolData`]
/// names `app_pool_id`, and the app id is the one identifier both sides already agree on for every
/// other WP0 object.
#[derive(Default)]
pub struct ShmMirror {
    pools: HashMap<u32, MirrorPool>,
    /// Total bytes written into mirrors this session, for the teardown summary.
    total_bytes: u64,
    /// Number of `ShmPoolData` messages applied, for the teardown summary.
    writes: u64,
}

impl ShmMirror {
    /// Create S's own pool of `size` bytes for the application's pool `app_pool_id`.
    ///
    /// # Inputs / outputs
    /// - Returns the **raw descriptor** of S's memfd, for the replay to pass to the compositor in the
    ///   `wl_shm.create_pool` it is about to send. Ownership stays here: the compositor keeps its own
    ///   mapping, and this side must hold the descriptor for the pool's life so `resize` can
    ///   `ftruncate` the same file.
    ///
    /// # Failure modes
    /// [`MirrorError::Allocation`] if the memfd cannot be created or mapped. A pool that already
    /// exists under this id is replaced, which is what a client recycling an id after `destroy`
    /// produces — a protocol id is a slot number, not an identity, and this project has been bitten by
    /// assuming otherwise.
    pub fn create_pool(&mut self, app_pool_id: u32, size: u32) -> Result<RawFd, MirrorError> {
        let fd = create_memfd(u64::from(size)).map_err(|e| {
            MirrorError::Allocation(format!("creating a {size}-byte shm mirror: {e}"))
        })?;
        let mapping = ShmMapping::map(fd.as_fd(), u64::from(size)).map_err(|e| {
            MirrorError::Allocation(format!("mapping a {size}-byte shm mirror: {e}"))
        })?;
        let raw = fd.as_raw_fd();
        self.pools
            .insert(app_pool_id, MirrorPool { fd, mapping, size });
        Ok(raw)
    }

    /// Copy `bytes` into the mirror at `offset`.
    ///
    /// # Failure modes
    /// [`MirrorError::UnknownPool`] or [`MirrorError::WriteOutsidePool`]; see those variants for why
    /// each is refused rather than clamped.
    pub fn write(
        &mut self,
        app_pool_id: u32,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), MirrorError> {
        let pool = self
            .pools
            .get_mut(&app_pool_id)
            .ok_or(MirrorError::UnknownPool(app_pool_id))?;
        let end = u64::from(offset) + bytes.len() as u64;
        if end > u64::from(pool.size) {
            return Err(MirrorError::WriteOutsidePool {
                end,
                size: pool.size,
            });
        }
        // SAFETY: `mapping` is a live `MAP_SHARED` mapping of exactly `len()` bytes owned by this
        // struct; the range was just bounds-checked against that length; `u8` has no invalid patterns.
        // The compositor may read these pages concurrently, which is the same bargain every shm client
        // makes — and the reason C copies at `commit`, the moment the protocol says drawing is done.
        let dst = unsafe {
            std::slice::from_raw_parts_mut(pool.mapping.as_ptr() as *mut u8, pool.mapping.len())
        };
        dst[offset as usize..end as usize].copy_from_slice(bytes);
        self.total_bytes += bytes.len() as u64;
        self.writes += 1;
        Ok(())
    }

    /// Grow or shrink the mirror to `new_size`, before the `resize` request reaches the compositor.
    ///
    /// # Why the order matters
    /// The compositor will map the new length as soon as it sees the request. If S's file were still
    /// the old size, the compositor would map past its end and fault on read — so the `ftruncate` and
    /// remap must both happen *first*, which is why this returns a `Result` the caller must handle
    /// before forwarding.
    pub fn resize(&mut self, app_pool_id: u32, new_size: u32) -> Result<(), MirrorError> {
        let pool = self
            .pools
            .get_mut(&app_pool_id)
            .ok_or(MirrorError::UnknownPool(app_pool_id))?;
        // SAFETY: `ftruncate` takes a descriptor and a length; the descriptor is owned and live.
        if unsafe { libc::ftruncate(pool.fd.as_raw_fd(), i64::from(new_size)) } != 0 {
            return Err(MirrorError::Allocation(format!(
                "resizing the shm mirror to {new_size} bytes: {}",
                std::io::Error::last_os_error()
            )));
        }
        let mapping = ShmMapping::map(pool.fd.as_fd(), u64::from(new_size))
            .map_err(|e| MirrorError::Allocation(format!("remapping the shm mirror: {e}")))?;
        pool.mapping = mapping;
        pool.size = new_size;
        Ok(())
    }

    /// Forget a pool, unmapping it and closing S's descriptor.
    pub fn destroy(&mut self, app_pool_id: u32) {
        self.pools.remove(&app_pool_id);
    }

    /// `(total bytes written, writes applied, pools currently mirrored)`, for the teardown summary.
    pub fn summary(&self) -> (u64, u64, usize) {
        (self.total_bytes, self.writes, self.pools.len())
    }
}
