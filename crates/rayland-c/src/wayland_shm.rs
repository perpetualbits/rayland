//! `wl_shm` support for the C-side Wayland proxy: pool bookkeeping and the byte ranges to sync.
//!
//! # Why an ordinary desktop application needs this before it needs anything else
//! An application that draws with Vulkan still cannot start over Rayland if it was built with a
//! normal toolkit. `winit` 0.30 — the windowing layer under most Rust GUI applications — treats
//! **three** Wayland globals as fatal at event-loop creation: `wl_compositor`, `xdg_wm_base`, and
//! **`wl_shm`**. The proxy advertises the first two but not the third, so `winit` aborts with
//! `WaylandError(Bind(NotPresent))` **before it creates a window, touches wgpu, or reaches Vulkan**.
//! Nothing about the GPU path is exercised; the application dies during setup. GTK and Qt make the
//! same demand.
//!
//! # Why the fd is not the problem it looks like
//! `wl_shm.create_pool` passes a **file descriptor**, and this project exists because a shared page
//! has no network representation. That reasoning does not apply here: `rayland-c` runs on the *same
//! machine as the application*, so the app hands its pool fd to a process sitting next to it, exactly
//! as it would to a local compositor. C maps the pool itself. **The fd never needs to reach S.**
//!
//! S allocates its own, separate memfd of the same size and the two are kept in step by copying — see
//! [`rayland_relay::C2S::ShmPoolData`]. **S's memfd is a different file.** They are the same size only
//! because two messages say so; nothing shares a page and nothing keeps them in step automatically.
//!
//! # What this module is, and what it is not
//! It is the *bookkeeping*: which pools exist, what geometry each buffer has, what is attached to
//! which surface, and — the one question that matters at commit time — **which byte range must be
//! copied**. It performs the `mmap` and reads the bytes, and it decides nothing about the wire.
//! Routing, interception and message construction stay in [`crate::wayland_proxy`], which is already
//! 1,400 lines and should be doing one job.
//!
//! Deliberately kept free of `wayland-server` types so the arithmetic can be tested with a synthetic
//! `memfd` and no compositor, no Mesa, no GPU and no network — which is most of what §11 of the design
//! asks for.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use rayland_vtest::transport::ShmMapping;

/// How many bytes per pixel the two mandatory `wl_shm` formats use.
///
/// `ARGB8888` (0) and `XRGB8888` (1) are the two every Wayland compositor must support, and the only
/// two this proxy advertises — see [`SUPPORTED_FORMATS`]. Both are 32 bits per pixel, which is what
/// makes the stride check in [`ShmTracker::create_buffer`] a single multiplication rather than a
/// format table.
const BYTES_PER_PIXEL: u32 = 4;

/// The `wl_shm` formats this proxy advertises to the application, as their protocol enum values.
///
/// # Why exactly these two, and no attempt to mirror S's real list
/// `ARGB8888` and `XRGB8888` are **mandatory for every Wayland compositor**, so advertising exactly
/// them is always truthful without plumbing S's format list back across the network — and a format
/// list that had to make a round trip before the application could bind would be a startup stall on
/// the one path that must not have one. Any other format a client asks for is refused at
/// `create_buffer`; see [`ShmError::UnsupportedFormat`].
pub const SUPPORTED_FORMATS: [u32; 2] = [0, 1];

/// Why a `wl_shm` request was refused.
///
/// # Why refusals are typed rather than logged in place
/// Each of these is a case where proceeding would produce something *worse than nothing*: a crash
/// with no error path, a read outside a mapping, or a window full of garbage that is harder to
/// diagnose than a window that never appears. Returning the reason lets the caller log it in the
/// proxy's existing `drop:` vocabulary, and lets a test assert *which* refusal fired rather than
/// merely that something failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShmError {
    /// The pool's backing file is shorter than the size the client declared.
    ///
    /// **This is the SIGBUS guard, and it is the reason this enum exists.** A client may legitimately
    /// pass any size it likes; if the file behind it is smaller, then reading the mapping past the
    /// file's end raises `SIGBUS` — a crash with no error path, no `errno`, and a thoroughly baffling
    /// diagnosis, in whichever process happens to touch it first. One `fstat` prevents it.
    PoolShorterThanDeclared {
        /// What the client said the pool was.
        declared: u32,
        /// What the descriptor actually holds.
        actual: u64,
    },
    /// The buffer's bytes would extend past the end of its pool.
    ///
    /// The same class of fault as [`Self::PoolShorterThanDeclared`], caught by arithmetic instead of
    /// by `fstat`: `offset + stride × height` must fit.
    BufferOutsidePool {
        /// The last byte the buffer would occupy, exclusive.
        end: u64,
        /// The pool's size.
        pool_size: u32,
    },
    /// The stride is narrower than one row of pixels.
    ///
    /// Proceeding would misread the layout and present garbage. A window showing scrambled pixels is
    /// harder to diagnose than a window that never appears, so this refuses rather than approximates.
    StrideTooSmall {
        /// The stride the client gave.
        stride: u32,
        /// The minimum a row of `width` pixels needs.
        minimum: u32,
    },
    /// A format outside [`SUPPORTED_FORMATS`].
    UnsupportedFormat(u32),
    /// A request named a pool this tracker has never seen, or that has been destroyed.
    UnknownPool(u32),
    /// A request named a buffer this tracker has never seen, or that has been destroyed.
    UnknownBuffer(u32),
    /// A resize would leave a live buffer hanging off the end of its pool.
    ResizeWouldOrphanBuffer {
        /// The proposed new size.
        new_size: u32,
        /// The end of the buffer that would no longer fit.
        buffer_end: u64,
    },
}

/// One application `wl_shm` pool, as C sees it: the app's own mapping, and how big it claims to be.
struct Pool {
    /// C's mapping of the **application's** memfd. This is the app's memory, mapped locally because C
    /// is on the same machine; S has a different file of the same size.
    mapping: ShmMapping,
    /// A **duplicate** of the application's descriptor, kept for the pool's life.
    ///
    /// # Why the descriptor must be retained
    /// `wl_shm_pool.resize` carries **no fd** — the client has already `ftruncate`d the file it
    /// shares, and the request only announces the new length. Without a descriptor to re-`mmap`,
    /// there would be nothing to remap and the pool would be stuck at its original size.
    ///
    /// A `dup` is right here, and its offset-sharing (the thing that produced a SIGBUS in the keymap
    /// path) is harmless: nothing ever reads this descriptor by offset, only maps it. What a `dup`
    /// *does* give is the guarantee that matters — it refers to the same open file, so when the
    /// client grows the file, this descriptor sees the new length.
    fd: OwnedFd,
    /// The size the client declared, which after [`ShmTracker::create_pool`]'s check is known to be no
    /// larger than the descriptor.
    size: u32,
}

/// One `wl_buffer` carved out of a pool.
///
/// Recorded at `create_buffer` and used at `commit`: the geometry is what turns "this surface is
/// showing buffer 12" into "copy these bytes".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferGeometry {
    /// The pool this buffer's bytes live in, by the application's object id.
    pub pool_id: u32,
    /// Byte offset of the buffer's first row within the pool.
    pub offset: u32,
    /// Bytes between the start of one row and the start of the next. **Not** `width × 4`: a client may
    /// pad rows, and using the width instead of the stride is how a picture ends up sheared.
    pub stride: u32,
    /// Width in pixels. Recorded for the stride check and for the instrumentation summary.
    pub width: u32,
    /// Height in pixels — the number of rows the copy must cover.
    pub height: u32,
    /// The `wl_shm` format enum value.
    pub format: u32,
}

impl BufferGeometry {
    /// The half-open byte range `[offset, offset + stride × height)` this buffer occupies in its pool.
    ///
    /// # Why `stride × height` and not `width × height × 4`
    /// The last row's padding is inside the buffer as far as the pool is concerned, and copying a
    /// short final row would leave S's mirror differing from C's in bytes the compositor may still
    /// read. Copying the padding is harmless; not copying it is not.
    ///
    /// # Inputs / outputs
    /// - Returns `(start, end)` as `u64` so the multiplication cannot overflow a `u32` and wrap into a
    ///   range that looks valid — which is exactly how an out-of-bounds read gets past a bounds check.
    pub fn byte_range(&self) -> (u64, u64) {
        let start = u64::from(self.offset);
        let end = start + u64::from(self.stride) * u64::from(self.height);
        (start, end)
    }
}

/// The C-side `wl_shm` bookkeeping: pools, buffers, and what each surface has attached.
///
/// # What it is for
/// One question, asked at `wl_surface.commit`: *which bytes must reach S before its compositor is
/// told to look at this surface?* Everything else here exists to answer that.
#[derive(Default)]
pub struct ShmTracker {
    /// Application `wl_shm_pool` object id → the pool.
    pools: HashMap<u32, Pool>,
    /// Application `wl_buffer` object id → its geometry.
    buffers: HashMap<u32, BufferGeometry>,
    /// Application `wl_surface` object id → the `wl_buffer` currently attached, if any.
    ///
    /// Absent means nothing was ever attached; `None` means a null buffer was attached, which is how
    /// a client *detaches*. The distinction matters: a commit with nothing attached must copy nothing
    /// rather than resend whatever was there before.
    attached: HashMap<u32, Option<u32>>,
    /// Total bytes synced this session, for the teardown summary.
    total_bytes: u64,
    /// Number of commits that carried shm bytes, for the teardown summary.
    synced_commits: u64,
    /// The largest single buffer observed, for the teardown summary — the number that decides whether
    /// this path is carrying cursors or windows.
    largest_buffer: u64,
}

impl ShmTracker {
    /// Record a new pool, mapping the application's descriptor locally.
    ///
    /// # Inputs / outputs
    /// - `pool_id`: the application's new `wl_shm_pool` object id.
    /// - `fd`: the application's pool descriptor. **Borrowed** — C keeps the application's fd only for
    ///   the lifetime of the mapping it makes here, and never sends it anywhere.
    /// - `size`: the size the client declared.
    /// - Returns the size to put on the wire, or a refusal.
    ///
    /// # Failure modes
    /// - [`ShmError::PoolShorterThanDeclared`] if `fstat` says the file is smaller than `size`. This is
    ///   the SIGBUS guard; see that variant.
    /// - A failed `mmap` is reported as the same refusal, because the effect on the client is
    ///   identical: no pool.
    pub fn create_pool(
        &mut self,
        pool_id: u32,
        fd: BorrowedFd<'_>,
        size: u32,
    ) -> Result<u32, ShmError> {
        // `fstat` before mapping: a pool larger than its file maps fine and faults on *read*, which is
        // a crash with no error path in whichever process touches it first.
        // SAFETY: `fstat` fills a `stat` it is given; the descriptor is live for this call.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let actual = if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } == 0 {
            u64::try_from(st.st_size).unwrap_or(0)
        } else {
            0
        };
        if actual < u64::from(size) {
            return Err(ShmError::PoolShorterThanDeclared {
                declared: size,
                actual,
            });
        }
        let mapping = ShmMapping::map(fd, u64::from(size)).map_err(|_| {
            ShmError::PoolShorterThanDeclared {
                declared: size,
                actual,
            }
        })?;
        // Keep a duplicate for `resize`, which carries no descriptor of its own. See `Pool::fd`.
        // SAFETY: `dup` returns a fresh descriptor this struct exclusively owns; the `OwnedFd` closes
        // only that duplicate, never the application's.
        let duplicate = unsafe { libc::dup(fd.as_raw_fd()) };
        if duplicate < 0 {
            return Err(ShmError::PoolShorterThanDeclared {
                declared: size,
                actual,
            });
        }
        // SAFETY: `duplicate` is a valid open descriptor owned solely by this struct from here on.
        let owned = unsafe { OwnedFd::from_raw_fd(duplicate) };
        self.pools.insert(
            pool_id,
            Pool {
                mapping,
                fd: owned,
                size,
            },
        );
        Ok(size)
    }

    /// Record a buffer carved from a pool, after checking it actually fits inside one.
    ///
    /// # Inputs / outputs
    /// - Returns the recorded geometry, or a refusal naming which check failed.
    ///
    /// # Failure modes
    /// [`ShmError::UnknownPool`], [`ShmError::UnsupportedFormat`], [`ShmError::StrideTooSmall`], and
    /// [`ShmError::BufferOutsidePool`] — each of which would otherwise produce a garbled window or an
    /// out-of-range read rather than a clean refusal.
    #[allow(clippy::too_many_arguments)]
    pub fn create_buffer(
        &mut self,
        buffer_id: u32,
        pool_id: u32,
        offset: u32,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
    ) -> Result<BufferGeometry, ShmError> {
        let pool = self
            .pools
            .get(&pool_id)
            .ok_or(ShmError::UnknownPool(pool_id))?;
        if !SUPPORTED_FORMATS.contains(&format) {
            return Err(ShmError::UnsupportedFormat(format));
        }
        // A row must at least hold its pixels. Padding beyond that is the client's business.
        let minimum = width.saturating_mul(BYTES_PER_PIXEL);
        if stride < minimum {
            return Err(ShmError::StrideTooSmall { stride, minimum });
        }
        let geometry = BufferGeometry {
            pool_id,
            offset,
            stride,
            width,
            height,
            format,
        };
        let (_, end) = geometry.byte_range();
        if end > u64::from(pool.size) {
            return Err(ShmError::BufferOutsidePool {
                end,
                pool_size: pool.size,
            });
        }
        self.buffers.insert(buffer_id, geometry);
        Ok(geometry)
    }

    /// Grow or shrink a pool, remapping C's view.
    ///
    /// # Failure modes
    /// [`ShmError::UnknownPool`], or [`ShmError::ResizeWouldOrphanBuffer`] if a buffer already carved
    /// from this pool would no longer fit — refused rather than left to fault at the next commit.
    /// `wl_shm_pool.resize` may only grow a pool per the protocol, but a client is remote input and
    /// this code does not assume it behaved.
    pub fn resize_pool_in_place(&mut self, pool_id: u32, new_size: u32) -> Result<(), ShmError> {
        let pool = self
            .pools
            .get(&pool_id)
            .ok_or(ShmError::UnknownPool(pool_id))?;
        // Every buffer already carved from this pool must still fit, or a later commit reads past the
        // end of the new mapping. `wl_shm_pool.resize` may only grow a pool per the protocol, but a
        // client is remote input and this code does not assume it behaved.
        for geometry in self.buffers.values().filter(|g| g.pool_id == pool_id) {
            let (_, end) = geometry.byte_range();
            if end > u64::from(new_size) {
                return Err(ShmError::ResizeWouldOrphanBuffer {
                    new_size,
                    buffer_end: end,
                });
            }
        }
        // The same SIGBUS guard as at creation, and it must be repeated: the client announces the new
        // size in the request, and whether it actually grew the file is its business, not ours.
        // SAFETY: `fstat` fills a `stat` it is given; the descriptor is owned and live.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let actual = if unsafe { libc::fstat(pool.fd.as_raw_fd(), &mut st) } == 0 {
            u64::try_from(st.st_size).unwrap_or(0)
        } else {
            0
        };
        if actual < u64::from(new_size) {
            return Err(ShmError::PoolShorterThanDeclared {
                declared: new_size,
                actual,
            });
        }
        let mapping = ShmMapping::map(pool.fd.as_fd(), u64::from(new_size)).map_err(|_| {
            ShmError::PoolShorterThanDeclared {
                declared: new_size,
                actual,
            }
        })?;
        // Replace the mapping and size, keeping the same descriptor: it is the same open file, and a
        // second `dup` would be a second thing to keep in step for no gain.
        let pool = self.pools.get_mut(&pool_id).expect("checked above");
        pool.mapping = mapping;
        pool.size = new_size;
        Ok(())
    }

    /// Record what a surface has attached. `None` is a null buffer, which detaches.
    pub fn attach(&mut self, surface_id: u32, buffer_id: Option<u32>) {
        self.attached.insert(surface_id, buffer_id);
    }

    /// Forget a pool and its mapping.
    pub fn destroy_pool(&mut self, pool_id: u32) {
        self.pools.remove(&pool_id);
    }

    /// Forget a buffer. Any surface still naming it will find nothing at commit and copy nothing.
    pub fn destroy_buffer(&mut self, buffer_id: u32) {
        self.buffers.remove(&buffer_id);
    }

    /// **The question this module exists to answer:** what must be copied for this surface's commit?
    ///
    /// # Inputs / outputs
    /// - `surface_id`: the surface being committed.
    /// - Returns `Ok(None)` when there is nothing to do — nothing attached, a null buffer attached, or
    ///   the attached buffer is not an shm buffer this tracker knows (a dma-buf one, which is the
    ///   common case for a GPU application and must pass through untouched).
    /// - Returns `Ok(Some((pool_id, offset, bytes)))` with the bytes read out of C's mapping, ready to
    ///   become a [`rayland_relay::C2S::ShmPoolData`].
    ///
    /// # Failure modes
    /// [`ShmError::UnknownPool`] if the buffer names a pool that has been destroyed under it. A buffer
    /// that is simply unknown is *not* an error — see the `Ok(None)` case above, because every
    /// dma-buf commit takes that path and a GPU application must not be spammed with refusals.
    pub fn commit(&mut self, surface_id: u32) -> Result<Option<(u32, u32, Vec<u8>)>, ShmError> {
        let Some(Some(buffer_id)) = self.attached.get(&surface_id).copied() else {
            return Ok(None);
        };
        // Not an shm buffer: this is the dma-buf path, which is the whole point of the project and
        // must pass through this function without a word.
        let Some(geometry) = self.buffers.get(&buffer_id).copied() else {
            return Ok(None);
        };
        let pool = self
            .pools
            .get(&geometry.pool_id)
            .ok_or(ShmError::UnknownPool(geometry.pool_id))?;
        let (start, end) = geometry.byte_range();
        // Re-checked here, not just at `create_buffer`: the pool may have been resized since, and this
        // is the read that would fault.
        if end > u64::from(pool.size) {
            return Err(ShmError::BufferOutsidePool {
                end,
                pool_size: pool.size,
            });
        }
        let bytes = pool.mapping_bytes()[start as usize..end as usize].to_vec();
        let len = bytes.len() as u64;
        self.total_bytes += len;
        self.synced_commits += 1;
        self.largest_buffer = self.largest_buffer.max(len);
        Ok(Some((geometry.pool_id, geometry.offset, bytes)))
    }

    /// The session summary: total bytes synced, commits that carried bytes, and the largest single
    /// buffer seen.
    ///
    /// # Why this is the deliverable and not decoration
    /// v1 deliberately ships no content hashing, no damage intersection and no compression, on the
    /// grounds that the traffic here is *predicted* to be cursors and window decorations rather than
    /// frames. This triple is what turns that prediction into a measurement: a largest-buffer figure
    /// in the tens of kilobytes says "cursor" and closes the question; one in the megabytes says a
    /// real application is pushing whole windows through a path that must never become the main road.
    pub fn summary(&self) -> (u64, u64, u64) {
        (self.total_bytes, self.synced_commits, self.largest_buffer)
    }
}

impl Pool {
    /// The pool's bytes, for reading.
    ///
    /// # Pitfall: the application writes these pages concurrently
    /// This is the application's own memory, and it may draw into it at any moment. Wayland's answer
    /// is `wl_surface.commit`: it is the point at which the client has declared it finished, which is
    /// why the copy happens there and not at `attach`. Reading outside that discipline gives a torn
    /// frame — the same bargain the ring's mapped-memory path makes, for the same reason.
    fn mapping_bytes(&self) -> &[u8] {
        // SAFETY: `mapping` is a live `MAP_SHARED` mapping of exactly `len()` bytes that outlives this
        // borrow, and `u8` has no invalid bit patterns. The concurrent-writer caveat is documented
        // above and is a data race in the abstract model, not an aliasing violation.
        unsafe {
            std::slice::from_raw_parts(self.mapping.as_ptr() as *const u8, self.mapping.len())
        }
    }
}

#[cfg(test)]
mod tests {
    //! `ShmTracker` is requests in, byte ranges out, so all of this runs against a synthetic `memfd`
    //! with no compositor, no Mesa, no GPU and no network — which is what makes it worth having.

    use super::*;
    use rayland_vtest::transport::create_memfd;
    use std::os::fd::AsFd;

    /// A pool descriptor of exactly `size` bytes, standing in for the application's.
    fn a_pool_fd(size: u64) -> OwnedFd {
        create_memfd(size).expect("a memfd for the test pool")
    }

    /// Fill the mapping through a second mapping of the same file, playing the part of the
    /// application drawing into its pool.
    fn draw(fd: &OwnedFd, value: u8, size: u64) {
        let m = ShmMapping::map(fd.as_fd(), size).expect("a writable second mapping");
        // SAFETY: a live `MAP_SHARED` mapping of exactly `size` bytes, exclusively borrowed here.
        let bytes = unsafe { std::slice::from_raw_parts_mut(m.as_ptr() as *mut u8, m.len()) };
        bytes.fill(value);
    }

    /// The happy path, and the arithmetic that matters: a commit yields exactly the buffer's bytes,
    /// taken from the right offset.
    #[test]
    fn a_commit_yields_the_attached_buffers_byte_range() {
        // A 4096-byte pool holding a 2-row, 8-pixel-wide buffer at a non-zero offset. The non-zero
        // offset is the point: a tracker that ignored it would return the right *length* of the wrong
        // bytes, which is the kind of bug that shows up as a shifted image rather than a crash.
        const SIZE: u64 = 4096;
        let fd = a_pool_fd(SIZE);
        draw(&fd, 0xAB, SIZE);
        let mut t = ShmTracker::default();
        t.create_pool(1, fd.as_fd(), SIZE as u32).expect("the pool");
        t.create_buffer(2, 1, 64, 8, 2, 32, 0).expect("the buffer");
        t.attach(3, Some(2));

        let (pool_id, offset, bytes) = t.commit(3).expect("commit must succeed").expect("bytes");
        assert_eq!(pool_id, 1);
        assert_eq!(
            offset, 64,
            "the copy must start at the buffer's offset, not at the pool's"
        );
        assert_eq!(
            bytes.len(),
            32 * 2,
            "stride x height, including any row padding"
        );
        assert!(bytes.iter().all(|b| *b == 0xAB));
    }

    /// **The SIGBUS guard.** A pool larger than its backing file maps fine and faults on *read*, in
    /// whichever process touches it first, with no error path and no `errno`. One `fstat` prevents it,
    /// and this test is the reason that `fstat` cannot be optimised away by a later reader who sees a
    /// syscall on a creation path and wonders what it is for.
    #[test]
    fn a_pool_larger_than_its_file_is_refused() {
        let fd = a_pool_fd(1024);
        let mut t = ShmTracker::default();
        let err = t.create_pool(1, fd.as_fd(), 8192).expect_err("must refuse");
        assert_eq!(
            err,
            ShmError::PoolShorterThanDeclared {
                declared: 8192,
                actual: 1024
            }
        );
    }

    /// A buffer whose bytes would run past the end of its pool is refused, by arithmetic rather than
    /// by faulting.
    #[test]
    fn a_buffer_outside_its_pool_is_refused() {
        let fd = a_pool_fd(1024);
        let mut t = ShmTracker::default();
        t.create_pool(1, fd.as_fd(), 1024).expect("the pool");
        // offset 512 + stride 64 x height 16 = 1536 > 1024.
        let err = t
            .create_buffer(2, 1, 512, 16, 16, 64, 0)
            .expect_err("must refuse");
        assert_eq!(
            err,
            ShmError::BufferOutsidePool {
                end: 1536,
                pool_size: 1024
            }
        );
    }

    /// A stride narrower than one row of pixels is refused rather than approximated: presenting a
    /// sheared image is harder to diagnose than presenting nothing.
    #[test]
    fn a_stride_narrower_than_a_row_is_refused() {
        let fd = a_pool_fd(4096);
        let mut t = ShmTracker::default();
        t.create_pool(1, fd.as_fd(), 4096).expect("the pool");
        // 16 pixels need 64 bytes; 32 is not enough.
        let err = t
            .create_buffer(2, 1, 0, 16, 2, 32, 0)
            .expect_err("must refuse");
        assert_eq!(
            err,
            ShmError::StrideTooSmall {
                stride: 32,
                minimum: 64
            }
        );
    }

    /// A format outside the two mandatory ones is refused, naming the format so the log says which.
    #[test]
    fn an_unsupported_format_is_refused() {
        let fd = a_pool_fd(4096);
        let mut t = ShmTracker::default();
        t.create_pool(1, fd.as_fd(), 4096).expect("the pool");
        let err = t
            .create_buffer(2, 1, 0, 8, 2, 32, 0x34325241)
            .expect_err("must refuse");
        assert_eq!(err, ShmError::UnsupportedFormat(0x34325241));
    }

    /// A resize that would leave a live buffer hanging off the end is refused.
    #[test]
    fn a_resize_that_orphans_a_buffer_is_refused() {
        let fd = a_pool_fd(4096);
        let mut t = ShmTracker::default();
        t.create_pool(1, fd.as_fd(), 4096).expect("the pool");
        t.create_buffer(2, 1, 0, 8, 16, 32, 0).expect("the buffer"); // ends at 512
        let err = t.resize_pool_in_place(1, 256).expect_err("must refuse");
        assert_eq!(
            err,
            ShmError::ResizeWouldOrphanBuffer {
                new_size: 256,
                buffer_end: 512
            }
        );
    }

    /// Growing a pool works, and the grown region is readable — which is the whole point of keeping a
    /// duplicate descriptor rather than only a mapping.
    #[test]
    fn a_pool_can_grow_and_the_new_region_is_readable() {
        let fd = a_pool_fd(8192);
        draw(&fd, 0x5A, 8192);
        let mut t = ShmTracker::default();
        // Start mapped at half the file, then grow into the rest.
        t.create_pool(1, fd.as_fd(), 4096).expect("the pool");
        t.resize_pool_in_place(1, 8192)
            .expect("the grow must succeed");
        t.create_buffer(2, 1, 4096, 16, 16, 64, 0)
            .expect("a buffer in the new region");
        t.attach(3, Some(2));
        let (_, offset, bytes) = t.commit(3).expect("commit").expect("bytes");
        assert_eq!(offset, 4096);
        assert_eq!(bytes.len(), 64 * 16);
        assert!(
            bytes.iter().all(|b| *b == 0x5A),
            "the grown region must be readable"
        );
    }

    /// **A commit with nothing attached, a null buffer, or a dma-buf buffer copies nothing and
    /// reports no error.** This is the case that runs on every frame of every GPU application, and it
    /// must be silent: a refusal here would fill the log of a perfectly healthy vkcube session.
    #[test]
    fn a_commit_with_no_shm_buffer_is_silent() {
        let mut t = ShmTracker::default();
        // Never attached.
        assert_eq!(t.commit(3), Ok(None));
        // Attached, then detached with a null buffer.
        t.attach(3, Some(9));
        t.attach(3, None);
        assert_eq!(t.commit(3), Ok(None));
        // Attached to a buffer this tracker has never seen — i.e. a dma-buf one, the GPU path.
        t.attach(3, Some(9));
        assert_eq!(t.commit(3), Ok(None));
    }

    /// Several buffers carved from one pool each yield their own range.
    #[test]
    fn several_buffers_share_one_pool() {
        const SIZE: u64 = 4096;
        let fd = a_pool_fd(SIZE);
        draw(&fd, 0x11, SIZE);
        let mut t = ShmTracker::default();
        t.create_pool(1, fd.as_fd(), SIZE as u32).expect("the pool");
        t.create_buffer(2, 1, 0, 8, 4, 32, 0).expect("first");
        t.create_buffer(3, 1, 2048, 8, 4, 32, 0).expect("second");

        t.attach(9, Some(2));
        let (_, off_a, a) = t.commit(9).expect("commit a").expect("bytes a");
        t.attach(9, Some(3));
        let (_, off_b, b) = t.commit(9).expect("commit b").expect("bytes b");
        assert_eq!((off_a, a.len()), (0, 128));
        assert_eq!((off_b, b.len()), (2048, 128));
    }

    /// A pool destroyed while a buffer still names it makes the next commit refuse rather than read
    /// through a dangling mapping.
    #[test]
    fn a_commit_after_its_pool_was_destroyed_is_refused() {
        let fd = a_pool_fd(4096);
        let mut t = ShmTracker::default();
        t.create_pool(1, fd.as_fd(), 4096).expect("the pool");
        t.create_buffer(2, 1, 0, 8, 2, 32, 0).expect("the buffer");
        t.attach(3, Some(2));
        t.destroy_pool(1);
        assert_eq!(t.commit(3), Err(ShmError::UnknownPool(1)));
    }

    /// The session summary counts what §10 of the design says it must, because that triple is the
    /// evidence deciding whether this path ever needs a cache or a damage intersection.
    #[test]
    fn the_summary_counts_bytes_commits_and_the_largest_buffer() {
        const SIZE: u64 = 8192;
        let fd = a_pool_fd(SIZE);
        draw(&fd, 1, SIZE);
        let mut t = ShmTracker::default();
        t.create_pool(1, fd.as_fd(), SIZE as u32).expect("the pool");
        t.create_buffer(2, 1, 0, 8, 2, 32, 0).expect("small"); // 64 bytes
        t.create_buffer(3, 1, 0, 16, 16, 64, 0).expect("large"); // 1024 bytes
        t.attach(9, Some(2));
        t.commit(9).expect("small commit");
        t.attach(9, Some(3));
        t.commit(9).expect("large commit");
        assert_eq!(t.summary(), (64 + 1024, 2, 1024));
    }
}
