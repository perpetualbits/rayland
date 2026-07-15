//! The Task 4a fd path, end to end over a **real Unix socket** against the **real GPU**.
//!
//! # What this test is for, and why it is not redundant with the unit tests
//! `vtest.rs`'s unit tests prove the dispatcher *calls* `send_fd`, using a recording transport and a
//! mock engine. `transport.rs`'s tests prove `send_fd` delivers a descriptor over a socketpair,
//! using a memfd. Neither one touches virglrenderer, and so neither can prove the thing a live Mesa
//! client actually depends on: that a **real blob resource, allocated by the real 3D driver and
//! exported by `virgl_renderer_resource_export_blob`, arrives at the far end of a real socket as a
//! descriptor the client can map**. That whole chain is what silently did not exist before this
//! task, and this test is what keeps it from silently ceasing to exist again.
//!
//! It plays the client's side itself — speaking the exact bytes Mesa's `vtest_init` sends and
//! reading the reply with the exact `recvmsg` Mesa's `vtest_receive_fd` performs — so it needs no
//! Mesa, no Vulkan app, and no `VN_DEBUG` incantation. A live Mesa Venus client was of course also
//! run against this server (that is the task's headline result); this test is the reproducible,
//! CI-runnable core of it.
//!
//! # Skip, don't fail, without a GPU
//! Like the rest of this crate's GPU tests, this gates on [`virgl_available`] and prints a SKIP
//! line on a host with no usable Venus render node.
//!
//! Run on a GPU host with: `cargo test -p rayland-engine -- --nocapture`.

// The engine and the protocol server under test.
use rayland_engine::{VirglEngine, virgl_available, vtest::serve_vtest};
// Playing the client: raw bytes in both directions, and the `recvmsg` that collects the descriptor.
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

/// The DRM render node this crate's GPU tests use.
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// The Venus capset id, as sent in `VCMD_GET_CAPSET` / `VCMD_CONTEXT_INIT`.
const VENUS_CAPSET_ID: u32 = 4;

/// `VIRGL_RENDERER_BLOB_MEM_HOST3D` — the blob kind a live Mesa Venus client actually requests, for
/// both its command ring and its device memory. Confirmed by observing one (see the task report);
/// this test therefore exercises the same path real traffic takes, not a hypothetical one.
const BLOB_MEM_HOST3D: u32 = 2;

/// `VCMD_BLOB_FLAG_MAPPABLE` — the flag a live client sets, meaning "I intend to `mmap` this".
const BLOB_FLAG_MAPPABLE: u32 = 1;

/// The blob size this test asks for. A real client's first blob is its ~128 KiB command ring; one
/// page is plenty to prove the mechanism, and keeps the test cheap.
const BLOB_SIZE: u32 = 4096;

// vtest command ids used below (see `vtest_protocol.h`).
const VCMD_RESOURCE_UNREF: u32 = 3;
const VCMD_CREATE_RENDERER: u32 = 8;
const VCMD_PROTOCOL_VERSION: u32 = 11;
const VCMD_GET_CAPSET: u32 = 16;
const VCMD_CONTEXT_INIT: u32 = 17;
const VCMD_RESOURCE_CREATE_BLOB: u32 = 18;

/// Encode one vtest message: `[len][cmd_id]` + dword payload, little-endian — the framing Mesa's
/// client writes.
fn msg(cmd_id: u32, payload: &[u32]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(&cmd_id.to_le_bytes());
    for &d in payload {
        v.extend_from_slice(&d.to_le_bytes());
    }
    v
}

/// Encode `VCMD_CREATE_RENDERER`, whose length field is a **byte** count (the name string), not a
/// dword count — the protocol's one framing exception.
fn create_renderer_msg(name: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&(name.len() as u32).to_le_bytes());
    v.extend_from_slice(&VCMD_CREATE_RENDERER.to_le_bytes());
    v.extend_from_slice(name);
    v
}

/// Read one reply header + dword payload back off the socket, exactly as Mesa's client does (a
/// plain `read`, *not* a `recvmsg` — the descriptor is collected separately).
fn read_reply(stream: &mut UnixStream) -> (u32, Vec<u32>) {
    let mut hdr = [0u8; 8];
    stream.read_exact(&mut hdr).expect("reply header");
    let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
    let cmd = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let mut payload = Vec::with_capacity(len);
    for _ in 0..len {
        let mut d = [0u8; 4];
        stream.read_exact(&mut d).expect("reply payload dword");
        payload.push(u32::from_le_bytes(d));
    }
    (cmd, payload)
}

/// Receive one descriptor the way Mesa's `vtest_receive_fd` does: `recvmsg` of one dummy byte plus
/// `CMSG_SPACE(sizeof(int))` of ancillary space, then take the `int` out of the single `SCM_RIGHTS`
/// control message.
///
/// Written from the client's source rather than from our sender, so agreement between the two is
/// evidence rather than tautology.
fn receive_fd(stream: &UnixStream) -> OwnedFd {
    // Ancillary buffer, aligned for `cmsghdr` — a bare `[u8; N]` is only 1-byte aligned, and the
    // kernel reads this as a `struct cmsghdr`.
    #[repr(C)]
    union CmsgBuffer {
        bytes: [u8; 64],
        _align: libc::cmsghdr,
    }
    // 64 bytes comfortably exceeds `CMSG_SPACE(sizeof(int))` (24 on x86_64 Linux); asserted rather
    // than assumed, so a platform where that is false fails loudly here instead of truncating the
    // control message and losing the descriptor.
    assert!(
        unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as libc::c_uint) as usize } <= 64,
        "the ancillary buffer must fit one SCM_RIGHTS control message"
    );

    let mut dummy: u8 = 0;
    let mut iov = libc::iovec {
        iov_base: (&raw mut dummy) as *mut libc::c_void,
        iov_len: 1,
    };
    let mut cmsg_buffer = CmsgBuffer { bytes: [0u8; 64] };
    // SAFETY: all-zero is a valid empty `msghdr`.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    // The `unsafe` reads as redundant on a modern toolchain and is not: Rust 1.85, this crate's
    // declared MSRV, classes any access to a union field as unsafe (E0133) and only 1.87 relaxed
    // the `&raw mut` case. See `rayland-vtest`'s `send_fd_over_socket` for the full note.
    // SAFETY: no union field is read — `&raw mut` only computes an address into the live, zeroed
    // `cmsg_buffer` above.
    #[allow(unused_unsafe)]
    let control = unsafe { (&raw mut cmsg_buffer.bytes) as *mut libc::c_void };
    msg.msg_control = control;
    msg.msg_controllen = 64;

    // SAFETY: `stream` is a live connected socket; `msg` and its buffers are live for the call.
    let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    assert_eq!(
        n,
        1,
        "the server must deliver exactly one dummy carrier byte: {}",
        std::io::Error::last_os_error()
    );

    // SAFETY: the kernel just filled `msg` in, so its control buffer is well-formed.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        assert!(!cmsg.is_null(), "an SCM_RIGHTS control message must arrive");
        // The exact level/type pair the real client asserts on before using the descriptor.
        assert_eq!((*cmsg).cmsg_level, libc::SOL_SOCKET);
        assert_eq!((*cmsg).cmsg_type, libc::SCM_RIGHTS);
        OwnedFd::from_raw_fd((libc::CMSG_DATA(cmsg) as *const libc::c_int).read_unaligned())
    }
}

/// The full Task 4a chain on real hardware: handshake → Venus context → **HOST3D blob** → the
/// exported descriptor arrives over `SCM_RIGHTS` and is genuinely `mmap`able by the client.
///
/// Every step here failed, or could not even be attempted, before this task: the server had no way
/// to produce a descriptor, so a client reaching `VCMD_RESOURCE_CREATE_BLOB` blocked forever.
///
/// The client half runs on a spawned thread and the server half on this one, so that both sides of
/// the socket are live at once — the protocol is request/response with a blocking read on each side,
/// so a single thread would deadlock on the first reply. The socket is what crosses the thread
/// boundary, exactly as it would cross a process boundary in reality.
#[test]
fn live_socket_blob_creation_delivers_a_mappable_fd_to_the_client() {
    let node = Path::new(RENDER_NODE);
    if !virgl_available(node) {
        eprintln!(
            "SKIP live_socket_blob_creation_delivers_a_mappable_fd_to_the_client: no usable Venus render node at {RENDER_NODE}"
        );
        return;
    }

    // A connected pair standing in for the client's connection. Same socket type, same kernel path.
    let (server_sock, client_sock) = UnixStream::pair().expect("socketpair");

    // The client: speaks Mesa's exact handshake, then asks for a blob and collects its descriptor.
    let client = std::thread::spawn(move || {
        let mut sock = client_sock;
        // Mesa's `vtest_init` order. `VCMD_CREATE_RENDERER` has no reply.
        sock.write_all(&create_renderer_msg(b"rayland-live-test\0"))
            .expect("create_renderer");
        // Negotiate the protocol version; the server answers `min(client, server)`, and Venus
        // requires ≥ 3.
        sock.write_all(&msg(VCMD_PROTOCOL_VERSION, &[4]))
            .expect("protocol_version");
        let (cmd, payload) = read_reply(&mut sock);
        assert_eq!(cmd, VCMD_PROTOCOL_VERSION);
        assert!(
            payload[0] >= 3,
            "Venus aborts init below protocol version 3; got {}",
            payload[0]
        );

        // The real Venus capset must come back valid, or a real client would abort here.
        sock.write_all(&msg(VCMD_GET_CAPSET, &[VENUS_CAPSET_ID, 0]))
            .expect("get_capset");
        let (cmd, payload) = read_reply(&mut sock);
        assert_eq!(cmd, VCMD_GET_CAPSET);
        assert_eq!(payload[0], 1, "the Venus capset must be reported valid");
        assert!(
            payload.len() > 1,
            "a valid capset must carry actual capability data"
        );

        // Create the Venus context on the real GPU. No reply is defined.
        sock.write_all(&msg(VCMD_CONTEXT_INIT, &[VENUS_CAPSET_ID]))
            .expect("context_init");

        // The step this test exists for: a HOST3D blob, exactly as a live client requests it.
        sock.write_all(&msg(
            VCMD_RESOURCE_CREATE_BLOB,
            &[BLOB_MEM_HOST3D, BLOB_FLAG_MAPPABLE, BLOB_SIZE, 0, 0, 0],
        ))
        .expect("resource_create_blob");

        // In-band reply first: `[res_id]`, read with a plain `read` — the descriptor must not have
        // leaked its carrier byte into these bytes.
        let (cmd, payload) = read_reply(&mut sock);
        assert_eq!(cmd, VCMD_RESOURCE_CREATE_BLOB);
        let res_id = payload[0];
        assert!(res_id > 0, "resource ids are 1-based; 0 is the sentinel");

        // Then the descriptor, via the client's own `recvmsg`.
        let fd = receive_fd(&sock);

        // The descriptor must be genuinely mappable — this is the client's *only* use for it, and
        // an unmappable one (e.g. an OPAQUE handle) would satisfy every assertion above and still
        // break a real client. `MAP_SHARED`, exactly as `vtest_shmem_create` maps it.
        // SAFETY: `fd` is the live descriptor just received; a null `addr` lets the kernel choose.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                BLOB_SIZE as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(
            ptr,
            libc::MAP_FAILED,
            "the exported blob descriptor must be mappable by the client: {}",
            std::io::Error::last_os_error()
        );

        // Write to the mapping the way a client writes its Venus command ring, proving the pages
        // are real and writable rather than merely a successful `mmap` of nothing.
        // SAFETY: `ptr` is a live, writable mapping of `BLOB_SIZE` bytes.
        unsafe { std::ptr::write_bytes(ptr as *mut u8, 0xAB, BLOB_SIZE as usize) };
        // SAFETY: unmapping exactly what `mmap` returned.
        unsafe { libc::munmap(ptr, BLOB_SIZE as usize) };

        // Release the resource and close the connection at a message boundary, so the server's
        // read loop sees a clean end of session rather than a truncated message.
        sock.write_all(&msg(VCMD_RESOURCE_UNREF, &[res_id]))
            .expect("resource_unref");
        drop(sock);
        res_id
    });

    // The server: a real engine on the real GPU, serving the real protocol over the real socket.
    let mut engine = VirglEngine::new(node).expect("VirglEngine::new should succeed on a GPU host");
    let mut sock = server_sock;
    let outcome = serve_vtest(&mut sock, &mut engine).expect("the vtest session must complete");

    let client_res_id = client.join().expect("the client thread must not panic");

    // The session did what the client asked, and the ids agree across the socket.
    assert_eq!(outcome.context_id, Some(1), "the Venus context was created");
    assert_eq!(
        outcome.rendered_resource_id,
        Some(client_res_id),
        "the resource id the client received must be the one the engine assigned"
    );

    eprintln!(
        "OK: a real HOST3D blob was created on the GPU, exported, and its descriptor delivered over SCM_RIGHTS to a client that mapped and wrote {BLOB_SIZE} bytes of it"
    );
}
