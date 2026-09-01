//! Integration proof for WP0's `wl_shm` support: a real Wayland client binds `wl_shm`, creates a pool
//! and a buffer, attaches and commits — and the proxy produces the right bytes, **in the right order**.
//!
//! # What this proves that the unit tests cannot
//! `ShmTracker` is unit-tested as pure arithmetic, but two things only exist end to end:
//!
//! 1. **The `wl_shm` global is actually advertised and bindable.** That is the whole reason this
//!    feature exists — `winit`, GTK and Qt treat `wl_shm` as fatal at event-loop creation, so a proxy
//!    that does not offer it kills the application before it reaches Vulkan. A unit test cannot bind a
//!    global.
//! 2. **`ShmPoolData` is emitted before the commit that depends on it.** Both travel the same ordered
//!    link, so bytes-then-commit is sufficient — and commit-then-bytes presents whatever S's pool held
//!    from the *previous* frame. That failure is intermittent by construction, which is exactly why the
//!    order is pinned by a test rather than by a comment.
//!
//! Skips gracefully where libwayland is absent, like its sibling proxy tests.

use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rayland_c::wayland_proxy::{ResourceResolver, WaylandSink};
use rayland_relay::{WaylandArg, WaylandMessage};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_shm::{Format, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, QueueHandle};

/// One thing the proxy sent S, in the order it was sent.
///
/// The **order** is the point: a collector that kept requests and pool data in separate lists could not
/// tell bytes-before-commit from commit-before-bytes, which is the one failure this test exists for.
#[derive(Debug)]
enum Sent {
    /// A forwarded Wayland request.
    Request(WaylandMessage),
    /// Pool contents: `(app_pool_id, offset, byte count)`. The pool id is carried for the debug
    /// dump a failing assertion prints, which is how this test explains itself when it breaks.
    PoolData(#[allow(dead_code)] u32, u32, usize),
}

/// A sink recording everything the proxy sent, interleaved.
#[derive(Default)]
struct Collector {
    sent: Mutex<Vec<Sent>>,
    binds: Mutex<Vec<(String, u32, u32)>>,
}
impl WaylandSink for Collector {
    fn forward_request(&self, msg: WaylandMessage) {
        self.sent.lock().unwrap().push(Sent::Request(msg));
    }
    fn forward_bind(&self, interface: &str, version: u32, app_object_id: u32) {
        self.binds
            .lock()
            .unwrap()
            .push((interface.to_string(), version, app_object_id));
    }
    fn forward_shm_pool_data(&self, app_pool_id: u32, offset: u32, bytes: Vec<u8>) {
        self.sent
            .lock()
            .unwrap()
            .push(Sent::PoolData(app_pool_id, offset, bytes.len()));
    }
}

/// Recognises no inode: this test never passes a dma-buf, so the resolver is never consulted.
struct NullResolver;
impl ResourceResolver for NullResolver {
    fn resolve_inode(&self, _dev: u64, _ino: u64) -> Option<u32> {
        None
    }
}

struct AppData;
impl Dispatch<WlRegistry, GlobalListContents> for AppData {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for AppData {
            fn event(
                _: &mut Self,
                _: &$t,
                _: <$t as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}
ignore_events!(WlShm, WlShmPool, WlBuffer, WlCompositor, WlSurface);

/// Connect to the proxy socket, retrying until it is listening or the deadline passes.
fn connect_with_retry(path: &PathBuf, within: Duration) -> Option<UnixStream> {
    let deadline = Instant::now() + within;
    loop {
        if let Ok(s) = UnixStream::connect(path) {
            return Some(s);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Bind `wl_shm`, draw into a pool, attach and commit — then assert the bytes crossed and that they
/// crossed **before** the commit.
#[test]
fn shm_ordering_puts_the_bytes_before_the_commit() {
    const WIDTH: i32 = 8;
    const HEIGHT: i32 = 4;
    const STRIDE: i32 = WIDTH * 4;
    const POOL: i32 = STRIDE * HEIGHT;

    let socket_path: PathBuf =
        std::env::temp_dir().join(format!("rayland-wp-proxy-shm-{}.sock", std::process::id()));
    let collector = Arc::new(Collector::default());
    let proxy_path = socket_path.clone();
    let proxy_sink = collector.clone();
    std::thread::spawn(move || {
        if let Err(e) =
            rayland_c::wayland_proxy::run(proxy_path, proxy_sink, Arc::new(NullResolver))
        {
            eprintln!("proxy exited with error: {e:#}");
        }
    });

    let stream = connect_with_retry(&socket_path, Duration::from_secs(2))
        .expect("connect to the proxy socket within the timeout");
    let conn = match Connection::from_socket(stream) {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("skipping: cannot init wayland-client backend (libwayland absent?): {e}");
            return;
        }
    };
    let (globals, mut queue) =
        registry_queue_init::<AppData>(&conn).expect("registry round-trips against the proxy");
    let qh = queue.handle();

    // **The claim this feature exists for:** `wl_shm` is advertised, so a toolkit that treats it as
    // mandatory can start at all. Before this change the bind below failed with `NotPresent`.
    let shm: WlShm = globals
        .bind(&qh, 1..=1, ())
        .expect("the proxy must advertise wl_shm, or winit/GTK/Qt cannot start");
    let compositor: WlCompositor = globals.bind(&qh, 1..=4, ()).expect("wl_compositor");
    let surface: WlSurface = compositor.create_surface(&qh, ());

    // A real pool the client draws into, exactly as a cursor or a decoration would be.
    // A `memfd` rather than a temp file: it is what a real client uses, it needs no filesystem, and it
    // is the same allocator the rest of this project uses for shared memory.
    let fd =
        rayland_vtest::transport::create_memfd(POOL as u64).expect("a backing memfd for the pool");
    {
        use std::os::fd::AsRawFd;
        // SAFETY: a fresh, exclusively-owned descriptor; `ManuallyDrop` keeps the `File` from closing
        // it, since `fd` must stay open for `create_pool` below.
        let mut file = std::mem::ManuallyDrop::new(unsafe {
            <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd.as_raw_fd())
        });
        file.write_all(&vec![0xC7u8; POOL as usize])
            .expect("draw into the pool");
        file.flush().expect("flush");
    }
    let pool: WlShmPool = shm.create_pool(fd.as_fd(), POOL, &qh, ());
    let buffer: WlBuffer = pool.create_buffer(0, WIDTH, HEIGHT, STRIDE, Format::Xrgb8888, &qh, ());

    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    queue
        .roundtrip(&mut AppData)
        .expect("round-trip after commit");

    let sent = collector.sent.lock().unwrap();

    // The pool bytes crossed, and they are the buffer's whole range.
    let pool_data_at = sent.iter().position(|s| matches!(s, Sent::PoolData(..)));
    let Some(pool_data_at) = pool_data_at else {
        panic!("the proxy sent no ShmPoolData; it sent: {sent:?}");
    };
    if let Sent::PoolData(_, offset, len) = &sent[pool_data_at] {
        assert_eq!(
            *offset, 0,
            "the buffer starts at the pool's origin in this test"
        );
        assert_eq!(
            *len,
            (STRIDE * HEIGHT) as usize,
            "the whole buffer must cross: stride x height, padding included"
        );
    }

    // **The ordering, which is the reason this test exists.** The commit is `wl_surface.commit`,
    // opcode 6 — and it must come *after* the bytes, on the same ordered link. Reversed, S's
    // compositor is told to look at a surface whose pool still holds the previous frame, and whether
    // that looks wrong depends on what was there before, which is the worst way for a bug to present.
    let commit_at = sent.iter().position(
        |s| matches!(s, Sent::Request(m) if m.opcode == 6 && m.object_id == surface_id(&surface)),
    );
    let Some(commit_at) = commit_at else {
        panic!("the proxy never forwarded the commit; it sent: {sent:?}");
    };
    assert!(
        pool_data_at < commit_at,
        "ShmPoolData must precede the commit that depends on it, but the proxy sent \
         index {pool_data_at} (bytes) and {commit_at} (commit): {sent:?}"
    );

    // And the pool creation itself crossed with the fd replaced by a size, never as a descriptor.
    let saw_pool_arg = sent.iter().any(|s| {
        matches!(s, Sent::Request(m)
            if m.args.iter().any(|a| matches!(a, WaylandArg::ShmPool { size } if *size == POOL as u32)))
    });
    assert!(
        saw_pool_arg,
        "wl_shm.create_pool must cross with WaylandArg::ShmPool in place of the fd: {sent:?}"
    );
}

/// The client-side protocol id of a `wl_surface`, for matching the forwarded commit.
fn surface_id(surface: &WlSurface) -> u32 {
    use wayland_client::Proxy;
    surface.id().protocol_id()
}
