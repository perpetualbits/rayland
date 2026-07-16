//! The concrete Unix-socket transport for the vtest wire protocol, and the three POSIX
//! primitives the protocol needs that `std` does not expose.
//!
//! # Why this module exists (the Task 4a discovery, stated plainly)
//! The vtest protocol is **not** a pure byte stream. Two of its replies —
//! `VCMD_RESOURCE_CREATE_BLOB` and `VCMD_SYNC_WAIT` — hand the client a **file descriptor** over an
//! `SCM_RIGHTS` ancillary control message, and Mesa's Venus ICD blocks forever in `recvmsg` until
//! it arrives. Tasks 2 and 3 wrote only the in-band half of those replies and assumed a generic
//! `Read + Write` stream would do; that assumption is what kept a *live* client from ever reaching
//! our engine. [`crate::VtestTransport`] replaces it, and this module is its one real
//! implementation.
//!
//! # Why the `unsafe` lives here and not in `virgl.rs`
//! This crate's rule is that all C FFI is confined to `VirglEngine`. The `sendmsg`/`memfd_create`/
//! `eventfd` calls below are the single sanctioned exception: they are *kernel* calls the wire
//! protocol mandates, not `libvirglrenderer` calls, and they must be reachable from the vtest layer
//! (which has no engine handle). Keeping them in this one small file — rather than sprinkling
//! `unsafe` through `vtest.rs` — is what preserves the reviewability the confinement rule exists
//! for. Everything here is a thin, checked wrapper that maps a failed syscall to a typed
//! [`EngineError`]; no raw fd or pointer escapes un-owned.
//!
//! # The fd wire format (pinned from both sides of the real protocol)
//! Server: `vtest_renderer.c:vtest_send_fd` (virglrenderer). Client: `vn_renderer_vtest.c:
//! vtest_receive_fd` (Mesa 26.0.3). The fd travels in a **separate `sendmsg`, issued immediately
//! after the in-band reply**, carrying:
//! - exactly **one dummy data byte** (`char c = 0`) in `msg_iov`, and
//! - one `SCM_RIGHTS` control message holding exactly **one `int`**.
//!
//! The dummy byte is load-bearing, not decoration: a `sendmsg` with a control message but no data
//! byte is not delivered to a stream-socket peer, so the client's matching `recvmsg` (1 byte +
//! `CMSG_SPACE(sizeof(int))`) would block forever. On a `SOCK_STREAM` socket the ancillary data
//! stays associated with the byte it was sent with, so the client's preceding plain `read()` of the
//! in-band reply cannot accidentally consume it — but only as long as the in-band reply is written
//! with **exactly** its own bytes and no more. That is why [`crate::vtest`] writes the reply and
//! the fd as two distinct operations, in that order.

// The trait this module implements for `UnixStream`, and the crate's typed error.
use crate::{EngineError, VtestTransport};
// Raw C types for the `msghdr`/`cmsghdr` plumbing.
use std::ffi::{c_int, c_uint, c_void};
// Owned/borrowed fd types: the ownership contract of every fd below is expressed in the type.
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
// The one transport C0 actually runs the protocol over.
use std::os::unix::net::UnixStream;

/// Size of the ancillary-data buffer for exactly one `SCM_RIGHTS` control message carrying one
/// `int`. `CMSG_SPACE` (not `CMSG_LEN`) is the right macro here: it includes the trailing padding
/// the kernel requires between control messages, and it is what both the C server and Mesa's client
/// size their buffers with — under-sizing it silently truncates the control message and the fd
/// never arrives.
const CMSG_BUF_LEN: usize = unsafe { libc::CMSG_SPACE(size_of::<c_int>() as c_uint) as usize };

/// A correctly-**aligned** ancillary-data buffer for [`send_fd_over_socket`].
///
/// A bare `[u8; N]` is only 1-byte aligned, but `CMSG_FIRSTHDR`/`CMSG_NXTHDR` cast the buffer to a
/// `struct cmsghdr` and the kernel reads it as such. Forcing the buffer's alignment to that of
/// `cmsghdr` is what makes the pointer casts below well-defined rather than incidentally-working
/// undefined behavior. (The C code gets this for free: a C `char buf[CMSG_SPACE(...)]` on the stack
/// is aligned by the compiler for the union it is used as; Rust requires us to say it.)
#[repr(C)]
union CmsgBuffer {
    /// The bytes the kernel actually reads.
    bytes: [u8; CMSG_BUF_LEN],
    /// Never read — present only to force `cmsghdr`'s alignment onto the union.
    _align: libc::cmsghdr,
}

/// Sends `fd` to the peer of `socket` as `SCM_RIGHTS` ancillary data, byte-for-byte as
/// virglrenderer's `vtest_send_fd` does.
///
/// This is the whole point of Task 4a: without it a live Mesa Venus client blocks forever on the
/// first `VCMD_RESOURCE_CREATE_BLOB`. See the module docs for the pinned wire format and why the
/// single dummy data byte is mandatory.
///
/// # Inputs / outputs
/// - `socket`: a **`SOCK_STREAM` Unix** socket. `SCM_RIGHTS` is a Unix-domain feature; sending on
///   any other socket family fails with the kernel's error rather than silently doing nothing.
/// - `fd`: the descriptor to duplicate into the peer. Borrowed, not consumed: the kernel duplicates
///   it into the receiving process, so the sender still owns its own copy afterwards and remains
///   responsible for closing it (the C server's `close(fd)` right after `vtest_send_fd` is exactly
///   this, not a cancellation of the send — "closing the file descriptor does not unmap the
///   region").
/// - Returns `Ok(())` once the kernel has accepted the message.
///
/// # Failure modes
/// - [`EngineError::FdSendFailed`] if `sendmsg` fails (peer gone, socket not Unix-domain, ...), or
///   if it reports a short send. A short send cannot actually happen for a 1-byte payload — the
///   kernel delivers a stream socket's ancillary data atomically with at least one byte or fails —
///   but it is checked rather than assumed, because "the fd silently did not arrive" is precisely
///   the failure this task exists to eliminate, and it would present to the client as an
///   unexplainable hang.
fn send_fd_over_socket(socket: BorrowedFd<'_>, fd: BorrowedFd<'_>) -> Result<(), EngineError> {
    // The one dummy data byte. `sendmsg` needs a non-empty `msg_iov` for the peer to receive
    // anything at all on a stream socket (see the module docs); the value is irrelevant and the
    // client discards it, so 0 matches the C server's `char c = 0`.
    let mut dummy: u8 = 0;
    let mut iov = libc::iovec {
        iov_base: (&raw mut dummy) as *mut c_void,
        iov_len: 1,
    };

    // Zeroed, correctly-aligned control buffer. Zeroing matters: the kernel reads the padding
    // bytes of the control message, and leaving them uninitialized would leak stack contents to
    // the peer.
    let mut cmsg_buffer = CmsgBuffer {
        bytes: [0u8; CMSG_BUF_LEN],
    };

    // Assemble the message header: no address (a connected stream socket), one iovec, one control
    // message. Built by zeroing the whole struct first so every field libc's `msghdr` may carry on
    // this platform (including any padding) is defined.
    // SAFETY: `msghdr` is a plain-old-data C struct with no invalid bit patterns; all-zero is the
    // documented "empty message header" starting point, exactly as the C server's `= { 0 }` does.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    // Take the address of the union's byte arm. Nothing is *read* here — `&raw mut` only computes
    // an address — but Rust 1.85, the MSRV this crate declares, still classes any access to a union
    // field as unsafe and rejects the bare expression (E0133). Rust 1.87 later relaxed exactly this
    // case, which is why the `unsafe` looks redundant on a modern toolchain and is not optional on
    // the floor we promise; `unused_unsafe` is allowed below rather than the MSRV being bumped,
    // because CLAUDE.md names RISC-V as a target for machine C and that floor is a deliberate claim.
    // SAFETY: no union field is read. `&raw mut` yields an address without loading the (zeroed, and
    // therefore anyway fully initialized) bytes, `cmsg_buffer` is live for the whole call, and the
    // kernel receives only this pointer plus the matching `CMSG_BUF_LEN` length.
    #[allow(unused_unsafe)]
    let control = unsafe { (&raw mut cmsg_buffer.bytes) as *mut c_void };
    msg.msg_control = control;
    msg.msg_controllen = CMSG_BUF_LEN as _;

    // Fill in the single SCM_RIGHTS control message.
    // SAFETY: `msg.msg_control`/`msg_controllen` describe the live, aligned, zeroed `cmsg_buffer`
    // above, which is exactly `CMSG_SPACE(sizeof(int))` bytes — so `CMSG_FIRSTHDR` returns a valid
    // pointer into it (never null for a buffer of at least this size), and writing `cmsg_len =
    // CMSG_LEN(sizeof(int))` bytes' worth of header + one `int` of data stays inside it.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        // A null here would mean our own buffer sizing is wrong — a programming error in this
        // file, not a runtime condition. Fail closed rather than dereference.
        if cmsg.is_null() {
            return Err(EngineError::FdSendFailed {
                source: std::io::Error::other(
                    "CMSG_FIRSTHDR returned null for a CMSG_SPACE(sizeof(int)) buffer",
                ),
            });
        }
        // SOL_SOCKET/SCM_RIGHTS is the "these are file descriptors" contract; the client asserts
        // on exactly this level/type pair and aborts if it sees anything else.
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<c_int>() as c_uint) as _;
        // The payload: exactly one `int`. `CMSG_DATA` may be unaligned relative to `int`, so write
        // it unaligned rather than through a plain `*mut c_int` deref.
        let data = libc::CMSG_DATA(cmsg) as *mut c_int;
        data.write_unaligned(fd.as_raw_fd());
    }

    // Hand the message to the kernel. SAFETY: `socket` is a live borrowed fd; `msg` and everything
    // it points at (`iov`, `dummy`, `cmsg_buffer`) are live for the duration of this call.
    let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &msg, 0) };
    if sent < 0 {
        // Capture the kernel's own errno rather than inventing one.
        return Err(EngineError::FdSendFailed {
            source: std::io::Error::last_os_error(),
        });
    }
    // See this function's "Failure modes": a 0-byte send would mean the fd did not travel, which
    // must never be reported as success (it would surface to the client as an unexplained hang).
    if sent != 1 {
        return Err(EngineError::FdSendFailed {
            source: std::io::Error::other(format!(
                "sendmsg sent {sent} bytes of the 1-byte SCM_RIGHTS carrier message"
            )),
        });
    }
    Ok(())
}

/// The Unix-socket transport C0 actually serves the vtest protocol over: a real `sendmsg`-based
/// `send_fd`, and `Read`/`Write` inherited from `std`'s own `UnixStream` impls.
///
/// This is the impl that makes a live Mesa Venus client work. A future QUIC transport (SP2/(c)1)
/// cannot inherit it — QUIC has no fd passing — which is precisely why [`VtestTransport`] is a
/// trait: it forces that transport to confront `send_fd` explicitly instead of silently inheriting
/// a broken assumption. See [`VtestTransport::send_fd`]'s doc comment for what that will mean.
impl VtestTransport for UnixStream {
    /// Sends `fd` over this socket as `SCM_RIGHTS`, exactly as virglrenderer's `vtest_send_fd`
    /// does. See [`send_fd_over_socket`] for the wire details and failure modes.
    fn send_fd(&mut self, fd: BorrowedFd<'_>) -> Result<(), EngineError> {
        send_fd_over_socket(self.as_fd(), fd)
    }
}

/// Creates an anonymous, in-memory file of exactly `size` bytes — the backing store for a
/// server-allocated blob resource's shared memory.
///
/// This mirrors virglrenderer's `vtest_new_shm`: the vtest server, for the `GUEST`-family blob
/// types, allocates the shared memory itself, hands the *client* the fd (which the client `mmap`s
/// and writes Venus commands into), and hands *virglrenderer* an iovec pointing at its own mapping
/// of the same pages. Both sides then look at literally the same physical memory.
///
/// `memfd_create` is used rather than a `shm_open` temp file because it needs no filesystem path,
/// no name collisions, and no cleanup: the object exists exactly as long as some fd or mapping
/// refers to it.
///
/// # Inputs / outputs
/// - `size`: the required size in bytes. The memfd is `ftruncate`d to exactly this, so a client
///   `mmap` of `size` bytes is fully backed (touching a page beyond a memfd's length would raise
///   `SIGBUS`, so the truncate is not optional).
/// - Returns an [`OwnedFd`] — the caller owns it and must eventually close it (dropping it does).
///
/// # Failure modes
/// - [`EngineError::ShmCreateFailed`] if `memfd_create` or `ftruncate` fails (typically
///   `ENOMEM`, or an `RLIMIT_NOFILE`/memlock limit).
pub fn create_memfd(size: u64) -> Result<OwnedFd, EngineError> {
    // A debug name; the kernel shows it as `/memfd:rayland-blob (deleted)` in `/proc/*/maps`,
    // which is worth having when diagnosing a live client's mappings. It is not an identifier —
    // memfd names need not be unique.
    let name = c"rayland-blob";
    // `MFD_CLOEXEC` matches the rest of this crate's fd discipline: an fd must never leak into an
    // exec'd child (notably virglrenderer's forked render server).
    // SAFETY: `name` is a valid, NUL-terminated C string that outlives the call.
    let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return Err(EngineError::ShmCreateFailed {
            size,
            source: std::io::Error::last_os_error(),
        });
    }
    // Take ownership immediately, so every error path below closes the fd via `Drop` rather than
    // leaking it.
    // SAFETY: `memfd_create` just returned this fd to us and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    // Give the object its real length. Without this the memfd is 0 bytes and any access to the
    // client's or our own mapping would fault with SIGBUS rather than read memory.
    let len = libc::off_t::try_from(size).map_err(|_| EngineError::ShmCreateFailed {
        size,
        // Not an OS error: `size` came off the wire and simply cannot name a file length on this
        // platform. Reported as an error rather than truncated to something plausible.
        source: std::io::Error::other("requested blob size does not fit in an off_t"),
    })?;
    // SAFETY: `fd` is a live memfd we own; `len` is a valid non-negative length.
    if unsafe { libc::ftruncate(fd.as_raw_fd(), len) } < 0 {
        return Err(EngineError::ShmCreateFailed {
            size,
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(fd)
}

/// A live `mmap` of a blob resource's shared memory, owned by the engine for the resource's whole
/// lifetime, that `munmap`s itself on drop.
///
/// # Why this must outlive the fd (the lifecycle pitfall this type encodes)
/// For a `GUEST`-family blob, virglrenderer is handed an **iovec pointing into this mapping** and
/// keeps that pointer until the resource is unref'd. The *fd* may be closed the instant it has been
/// sent to the client ("closing the file descriptor does not unmap the region" — virglrenderer's
/// own comment), but unmapping while virglrenderer still holds the iovec would leave it reading
/// freed address space: a use-after-free driven by an untrusted client's command stream. Tying the
/// mapping's lifetime to a Rust value the engine stores alongside the resource — and unrefing the
/// resource *before* this drops — is what makes that ordering structural rather than a comment
/// someone must remember.
pub struct ShmMapping {
    /// Start of the mapping. Never null (`mmap` failure is turned into an error at construction).
    ptr: *mut c_void,
    /// Mapping length in bytes, needed verbatim by `munmap`.
    len: usize,
}

// `len` below reports a fixed mapping size, not a collection's element count, so clippy's usual
// "a public `len` wants an `is_empty`" pairing does not apply: `map` rejects a zero-sized mapping
// outright (`mmap` itself returns EINVAL for one), so an `is_empty` here could only ever return a
// constant `false` — a method that answers a question no caller has, and implies this type might
// sometimes be empty when by construction it never is. Silencing the lint is the honest option.
// The lint only began firing at all when (c)1 Task 1 widened these methods from `pub(crate)` to
// `pub` for the crate split; nothing about the type itself changed.
#[allow(clippy::len_without_is_empty)]
impl ShmMapping {
    /// Maps all `size` bytes of `fd` shared and read/write, for handing to virglrenderer as an
    /// iovec.
    ///
    /// `MAP_SHARED` is the entire point: a `MAP_PRIVATE` mapping would copy-on-write, so the
    /// client's writes into its own mapping of the same memfd would never be visible to
    /// virglrenderer, and the Venus command ring would silently read stale zeros.
    ///
    /// # Inputs / outputs
    /// - `fd`: the memfd (from [`create_memfd`]) to map. Borrowed — `mmap` keeps its own reference
    ///   to the underlying object, so the caller may close the fd afterwards.
    /// - `size`: bytes to map; must not exceed the fd's length (see [`create_memfd`]'s `ftruncate`).
    /// - Returns the owning [`ShmMapping`].
    ///
    /// # Failure modes
    /// - [`EngineError::ShmMapFailed`] if `mmap` fails, or if `size` is 0 (which `mmap` rejects
    ///   with `EINVAL` anyway; reported honestly rather than papered over with a 1-page mapping).
    pub fn map(fd: BorrowedFd<'_>, size: u64) -> Result<Self, EngineError> {
        // `size` originates from an untrusted client's wire message; it must fit this platform's
        // address space before we ask the kernel for a mapping of it.
        let len = usize::try_from(size).map_err(|_| EngineError::ShmMapFailed {
            size,
            source: std::io::Error::other("requested blob size does not fit in a usize"),
        })?;
        // SAFETY: a null `addr` lets the kernel choose the address; `fd` is a live memfd of at
        // least `len` bytes (its creator `ftruncate`d it to exactly this size).
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                // Read/write: virglrenderer reads the client's commands out of these pages and may
                // write results back into them.
                libc::PROT_READ | libc::PROT_WRITE,
                // Shared, so our pages and the client's mapping of the same memfd are one memory.
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(EngineError::ShmMapFailed {
                size,
                source: std::io::Error::last_os_error(),
            });
        }
        Ok(ShmMapping { ptr, len })
    }

    /// The mapping's base address, for filling in the `iov_base` of the iovec handed to
    /// virglrenderer. The pointer is valid for exactly as long as this `ShmMapping` lives — which
    /// is the invariant [`ShmMapping`]'s doc comment explains the resource lifecycle around.
    pub fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }

    /// The mapping's length in bytes, for the iovec's `iov_len`.
    pub fn len(&self) -> usize {
        self.len
    }
}

// SAFETY: `ShmMapping` is a raw pointer plus a length, which makes it `!Send` by default — but the
// thing it names has no thread affinity whatsoever. An `mmap` region belongs to the *process*, not
// to the thread that created it, and `munmap` is valid from any thread. This type owns its mapping
// exclusively (nothing else may unmap it, and it hands out the pointer only through `as_ptr`), so
// moving one between threads cannot create a data race or a double-unmap.
//
// This impl is not decoration: `VirglEngine` stores `ShmMapping`s, and without it the engine would
// silently stop being `Send` — an API property it had since Task 1 — as an accidental side effect of
// Task 4a adding shared memory to a resource, rather than as anyone's decision.
unsafe impl Send for ShmMapping {}

impl Drop for ShmMapping {
    /// Unmaps the region. By construction this runs only after the resource whose iovec pointed
    /// here has been unref'd (see [`ShmMapping`]'s doc comment and `VirglEngine::unref_resource`),
    /// so virglrenderer can no longer be holding the pointer.
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are exactly what `mmap` returned and were never changed; this type
        // owns the mapping, so this is the only `munmap` of it. A failure here is unreachable
        // (both arguments came from the kernel) and nothing could be done about it in `drop`
        // anyway, so the return value is deliberately ignored.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

/// Creates an `eventfd` in the given readiness state — the pollable fd `VCMD_SYNC_WAIT`'s reply
/// hands to the client.
///
/// # What the client does with it (why "readable" is the whole contract)
/// virglrenderer's `vtest_sync_wait` creates `eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK)`, sends it to
/// the client, and — either immediately (if every waited-on sync is already at its target) or later
/// from its own event loop — `write()`s a `1` to it. The client polls the fd; it becoming readable
/// *is* the "your wait is satisfied" signal. Nothing else about the fd matters to the client.
///
/// # Inputs / outputs
/// - `ready`: whether to pre-signal the fd (write the initial count of 1) so it is readable the
///   moment the client polls it. C0's vtest layer always passes `true` — see the `SyncWait` arm in
///   [`crate::vtest`] for the honest statement of why (its sync objects are bookkeeping stubs, so
///   every wait is by construction already satisfied; this is virglrenderer's own `is_ready` branch,
///   not a fabricated fd).
/// - Returns an [`OwnedFd`] the caller owns and must close (dropping it does; the vtest layer drops
///   it right after `send_fd`, matching the C server's `close(fd)`).
///
/// # Failure modes
/// - [`EngineError::EventFdFailed`] if `eventfd` or the readiness `write` fails.
pub(crate) fn create_eventfd(ready: bool) -> Result<OwnedFd, EngineError> {
    // `EFD_CLOEXEC`: never leak into an exec'd child. `EFD_NONBLOCK`: match the C server exactly —
    // the client may `read()` this fd, and a blocking read of an unsignaled eventfd would hang it.
    // SAFETY: no pointers involved; an initval of 0 with these flags is always a valid call.
    let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if raw < 0 {
        return Err(EngineError::EventFdFailed {
            source: std::io::Error::last_os_error(),
        });
    }
    // Own it immediately so the error path below closes it.
    // SAFETY: `eventfd` just returned this fd and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    if ready {
        // Signal it, exactly as the C server's `write_ready` does: an eventfd counter increment of
        // 1, written as a native-endian u64. This is what makes the fd poll as readable.
        let val: u64 = 1;
        // SAFETY: `fd` is a live eventfd we own; `&val` points at exactly the 8 bytes an eventfd
        // write requires.
        let written = unsafe {
            libc::write(
                fd.as_raw_fd(),
                (&raw const val) as *const c_void,
                size_of::<u64>(),
            )
        };
        // An eventfd write is all-or-nothing (8 bytes or an error), so anything else means the fd
        // is not actually signaled — which would present to the client as a hung wait. Never
        // report that as success.
        if written != size_of::<u64>() as isize {
            return Err(EngineError::EventFdFailed {
                source: std::io::Error::last_os_error(),
            });
        }
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Reading back what travelled over the socket, and what the received fd points at.
    use std::io::{Read, Write};
    // `read_exact_at` — a positional read, needed because a duplicated fd shares the sender's file
    // offset (see `send_fd_delivers_a_real_readable_fd_over_a_unix_socket`).
    use std::os::unix::fs::FileExt;

    /// Receive one fd from `socket` the way Mesa's `vtest_receive_fd` does: a `recvmsg` of one
    /// dummy byte plus `CMSG_SPACE(sizeof(int))` of ancillary space, then pull the `int` out of the
    /// single `SCM_RIGHTS` control message.
    ///
    /// This is a **test-only mirror of the real client**, deliberately written from
    /// `vn_renderer_vtest.c:117` rather than from our own `send_fd`, so the two sides are
    /// independent: a test passing here means our sender matches the client's reader, not merely
    /// that it matches itself.
    ///
    /// Returns the received fd (owned by the caller), or `None` if no control message arrived.
    fn receive_fd(socket: BorrowedFd<'_>) -> Option<OwnedFd> {
        // The dummy data byte the sender is required to include.
        let mut dummy: u8 = 0xFF;
        let mut iov = libc::iovec {
            iov_base: (&raw mut dummy) as *mut c_void,
            iov_len: 1,
        };
        // Same aligned, zeroed control buffer discipline as the sender.
        let mut cmsg_buffer = CmsgBuffer {
            bytes: [0u8; CMSG_BUF_LEN],
        };
        // SAFETY: all-zero is a valid empty `msghdr`, as in `send_fd_over_socket`.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        // Address of the live union's byte arm; see `send_fd_over_socket` for why it is aligned, and
        // for why the `unsafe` that reads as redundant on a modern toolchain is what the declared
        // 1.85 MSRV requires.
        // SAFETY: no union field is read — `&raw mut` only computes an address into the live,
        // zeroed `cmsg_buffer` above.
        #[allow(unused_unsafe)]
        let control = unsafe { (&raw mut cmsg_buffer.bytes) as *mut c_void };
        msg.msg_control = control;
        msg.msg_controllen = CMSG_BUF_LEN as _;

        // SAFETY: `socket` is live; `msg` and its buffers are live for the call.
        let n = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, 0) };
        assert!(
            n >= 0,
            "recvmsg failed: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(n, 1, "the sender must deliver exactly one dummy data byte");

        // SAFETY: `msg` was just filled in by the kernel, so its control buffer is well-formed.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null() {
                return None;
            }
            // Exactly the level/type pair the real client asserts on.
            assert_eq!((*cmsg).cmsg_level, libc::SOL_SOCKET);
            assert_eq!((*cmsg).cmsg_type, libc::SCM_RIGHTS);
            let fd = (libc::CMSG_DATA(cmsg) as *const c_int).read_unaligned();
            Some(OwnedFd::from_raw_fd(fd))
        }
    }

    /// The core Task 4a capability, proven with no GPU: `send_fd` must deliver a **real, usable**
    /// descriptor to the peer of a Unix socket — not merely "a call that returns Ok".
    ///
    /// The proof is content-based rather than identity-based: an fd number is meaningless across
    /// the boundary (the kernel picks a fresh number in the receiver), so the test writes known
    /// bytes into a memfd, sends *that* fd, and reads the bytes back through the descriptor the
    /// receiver got. Only a genuinely duplicated descriptor can produce them.
    #[test]
    fn send_fd_delivers_a_real_readable_fd_over_a_unix_socket() {
        // A connected Unix stream-socket pair: the same socket type a real Mesa client connects
        // over, so this exercises the actual SCM_RIGHTS path rather than a simulation.
        let (mut server, client) = UnixStream::pair().expect("socketpair");

        // Known content in an anonymous file, standing in for a blob resource's shared memory.
        let payload = b"rayland scm_rights payload";
        let memfd = create_memfd(payload.len() as u64).expect("memfd_create");
        // Write through a borrowed `File` view so we do not consume the `OwnedFd` we still need.
        {
            let mut file = std::fs::File::from(memfd.try_clone().expect("dup memfd"));
            file.write_all(payload).expect("seed the memfd");
        }

        // The operation under test.
        server
            .send_fd(memfd.as_fd())
            .expect("send_fd should succeed");

        // Receive it exactly as Mesa's client does.
        let received = receive_fd(client.as_fd()).expect("an SCM_RIGHTS fd must arrive");

        // Read the content back through the *received* descriptor. Identical bytes prove the peer
        // holds a real duplicate of our memfd, which is the only thing that makes a live client work.
        //
        // Read at an explicit offset (`pread`), not with a plain `read`: SCM_RIGHTS duplicates the
        // open *file description*, so the receiver shares the sender's file offset — which the
        // `write_all` above left at EOF. A plain read here would return zero bytes and look like a
        // failed send. (This costs a real client nothing: it `mmap`s the descriptor, which ignores
        // the offset entirely.)
        let mut got = vec![0u8; payload.len()];
        std::fs::File::from(received)
            .read_exact_at(&mut got, 0)
            .expect("read the received fd");
        assert_eq!(
            got, payload,
            "the received fd must refer to the same object we sent"
        );
    }

    /// The ordering contract a live client depends on: the in-band reply bytes must arrive first
    /// and be readable with a plain `read()`, and the fd must arrive *after* them, on its own
    /// carrier byte. If the fd's dummy byte were interleaved into the reply the client would
    /// mis-frame the protocol; if the reply were written after the fd the client's `recvmsg` would
    /// consume reply bytes as the carrier.
    #[test]
    fn in_band_reply_precedes_the_fd_and_stays_separable() {
        let (mut server, mut client) = UnixStream::pair().expect("socketpair");

        // An 8-byte stand-in for a vtest reply header, written exactly as `write_reply` does.
        let reply = [0xDEu8, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];
        server.write_all(&reply).expect("write the in-band reply");
        let memfd = create_memfd(4).expect("memfd_create");
        server.send_fd(memfd.as_fd()).expect("send_fd");

        // The client reads exactly the reply it expects, with a plain read: the fd's carrier byte
        // must not have been mixed into these bytes.
        let mut got = [0u8; 8];
        client.read_exact(&mut got).expect("read the in-band reply");
        assert_eq!(got, reply, "the in-band reply must arrive intact and first");

        // Only then does the fd arrive, on its own dummy byte.
        let received = receive_fd(client.as_fd());
        assert!(
            received.is_some(),
            "the fd must still be receivable after the in-band reply was read"
        );
    }

    /// `create_memfd` must produce a descriptor of exactly the requested length. This is not
    /// pedantry: the client `mmap`s `size` bytes of it, and touching a page past a memfd's end
    /// raises `SIGBUS` — so an un-`ftruncate`d (0-length) memfd would crash a live client the
    /// instant it wrote its first Venus command.
    #[test]
    fn create_memfd_is_sized_exactly_as_requested() {
        const SIZE: u64 = 8192;
        let fd = create_memfd(SIZE).expect("memfd_create");
        let len = std::fs::File::from(fd)
            .metadata()
            .expect("stat memfd")
            .len();
        assert_eq!(len, SIZE, "the memfd must be ftruncate'd to the full size");
    }

    /// A `ShmMapping` must be genuine `MAP_SHARED` memory over the memfd: bytes written through
    /// the mapping must be visible when reading the *file*, and vice versa. That two-way visibility
    /// is exactly what makes a GUEST-family blob work — virglrenderer reads Venus commands through
    /// this mapping that the client wrote through its own mapping of the same object. A
    /// `MAP_PRIVATE` mapping would pass a naive "can I write to it" test and silently fail here.
    #[test]
    fn shm_mapping_shares_memory_with_the_memfd() {
        const SIZE: u64 = 4096;
        let fd = create_memfd(SIZE).expect("memfd_create");
        let mapping = ShmMapping::map(fd.as_fd(), SIZE).expect("mmap");
        assert_eq!(mapping.len(), SIZE as usize);

        // Write a marker through the mapping, as virglrenderer would write into a blob's iovec.
        let marker = b"venus-ring";
        // SAFETY: `mapping` is a live, writable mapping of at least `marker.len()` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                marker.as_ptr(),
                mapping.as_ptr() as *mut u8,
                marker.len(),
            )
        };

        // Read it back through the file descriptor — a different view of the same object. Equal
        // bytes prove the mapping is shared, not a private copy.
        let mut got = vec![0u8; marker.len()];
        let mut file = std::fs::File::from(fd);
        file.read_exact(&mut got).expect("read the memfd");
        assert_eq!(
            got,
            marker.as_slice(),
            "writes through a MAP_SHARED mapping must be visible in the memfd itself"
        );
    }

    /// A pre-signaled `eventfd` must actually be readable — that readability *is* the "your
    /// VCMD_SYNC_WAIT is satisfied" signal the client polls for. An eventfd that was created but
    /// never written would leave a live client waiting forever, which looks identical to the bug
    /// this whole task exists to fix.
    #[test]
    fn ready_eventfd_is_immediately_readable_with_the_expected_count() {
        let fd = create_eventfd(true).expect("eventfd");
        // An eventfd read returns its 8-byte counter (and resets it). Non-blocking, so if it were
        // *not* signaled this would fail with EAGAIN rather than hang the test.
        let mut buf = [0u8; 8];
        // SAFETY: `fd` is a live eventfd; `buf` is exactly the 8 bytes an eventfd read requires.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut c_void, buf.len()) };
        assert_eq!(
            n,
            8,
            "a ready eventfd must be readable: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(
            u64::from_ne_bytes(buf),
            1,
            "write_ready's counter increment of 1, matching the C server"
        );
    }

    /// An unsignaled `eventfd` must *not* be readable — the negative half of the test above,
    /// proving the readiness signal is real rather than an artifact of eventfd always being
    /// readable.
    #[test]
    fn unready_eventfd_is_not_readable() {
        let fd = create_eventfd(false).expect("eventfd");
        let mut buf = [0u8; 8];
        // SAFETY: as above.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut c_void, buf.len()) };
        assert_eq!(n, -1, "an unsignaled eventfd must not be readable");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN),
            "and must report EAGAIN (it is EFD_NONBLOCK), not block"
        );
    }
}
