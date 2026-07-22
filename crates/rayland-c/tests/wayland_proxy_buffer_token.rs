//! Integration proof for WP0 Task 3b **sub-step 4 (fd→token)**: the crux. A real dmabuf client runs the
//! swapchain buffer-creation sequence through the proxy, and the passed dma-buf fd is turned into a
//! [`rayland_relay::BufferToken`] — no fd, no pixels crossing — with the right resource id and geometry.
//!
//! # What this proves
//! The proxy intercepts `zwp_linux_dmabuf_v1.create_params` → `zwp_linux_buffer_params_v1.add` →
//! `create_immed`, resolves the `add` fd's memfd inode to an S-side resource id, gathers the geometry from
//! `create_immed`, and forwards exactly one message binding the new `wl_buffer` to a fully-populated
//! `BufferToken`. Critically, **no `create_immed` assert fires** — the whole reason a plain vkcube aborts
//! is that a real compositor rejects the memfd; the proxy consumes it and a real compositor never sees it.
//!
//! The inode→resource correlation is injected as a fixed stub resolver (mapping the test's own memfd to a
//! chosen resource id), exactly as the real `shm.rs`-backed resolver will behave once the proxy joins the
//! daemon. Like the sibling tests, it skips where libwayland is absent.

use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rayland_c::wayland_proxy::{ResourceResolver, WaylandSink};
use rayland_relay::{WaylandArg, WaylandMessage};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{
    Flags, ZwpLinuxBufferParamsV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;

// The concrete buffer facts the test drives through the proxy and expects to see reflected in the token.
const RESOURCE_ID: u32 = 4242; // the S-side resource id the stub resolver returns for our memfd
const WIDTH: i32 = 64;
const HEIGHT: i32 = 48;
const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241; // fourcc 'AR24'
const MODIFIER_HI: u32 = 0x0123_4567;
const MODIFIER_LO: u32 = 0x89ab_cdef;
const MODIFIER: u64 = ((MODIFIER_HI as u64) << 32) | MODIFIER_LO as u64;

/// A sink that records forwarded requests, so the test can inspect the emitted token.
#[derive(Default)]
struct Collector {
    messages: Mutex<Vec<WaylandMessage>>,
}
impl WaylandSink for Collector {
    fn forward_request(&self, msg: WaylandMessage) {
        self.messages.lock().unwrap().push(msg);
    }
}

/// A resolver that maps exactly one memfd identity (the test's) to [`RESOURCE_ID`], mirroring the real
/// `shm.rs`-backed resolver's behaviour for a single tracked resource.
struct FixedResolver {
    dev: u64,
    ino: u64,
}
impl ResourceResolver for FixedResolver {
    fn resolve_inode(&self, dev: u64, ino: u64) -> Option<u32> {
        (dev == self.dev && ino == self.ino).then_some(RESOURCE_ID)
    }
}

/// Client dispatch state — reacts to no events; all the interfaces below have events the test ignores.
struct AppData;

macro_rules! ignore_events {
    ($iface:ty, $udata:ty) => {
        impl Dispatch<$iface, $udata> for AppData {
            fn event(
                _state: &mut Self,
                _proxy: &$iface,
                _event: <$iface as wayland_client::Proxy>::Event,
                _data: &$udata,
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }
    };
}
ignore_events!(WlRegistry, GlobalListContents);
ignore_events!(ZwpLinuxDmabufV1, ());
ignore_events!(ZwpLinuxBufferParamsV1, ());
ignore_events!(WlBuffer, ());

/// Run the swapchain buffer-creation sequence through the proxy and assert the emitted `BufferToken`.
#[test]
fn create_immed_emits_a_correct_buffer_token_and_never_asserts() {
    let socket_path: PathBuf = std::env::temp_dir()
        .join(format!("rayland-wp-proxy-token-{}.sock", std::process::id()));

    // A memfd standing in for a swapchain image's `memfd:rayland-blob`. Its inode is what the resolver
    // recognises; the same inode arrives at the proxy when the client passes this fd over `params.add`.
    let memfd = make_memfd();
    let (dev, ino) = fd_inode(&memfd).expect("fstat the test memfd");

    let collector = Arc::new(Collector::default());
    let proxy_path = socket_path.clone();
    let proxy_sink = collector.clone();
    let resolver = Arc::new(FixedResolver { dev, ino });
    std::thread::spawn(move || {
        if let Err(e) = rayland_c::wayland_proxy::run(proxy_path, proxy_sink, resolver) {
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

    // Bind the dmabuf global at a version that supports create_immed (since v2) and modifiers (since v3).
    let dmabuf: ZwpLinuxDmabufV1 = globals
        .bind(&qh, 3..=3, ())
        .expect("proxy lets the client bind zwp_linux_dmabuf_v1");

    // The swapchain buffer-creation sequence Mesa's WSI runs, driven explicitly here.
    let params: ZwpLinuxBufferParamsV1 = dmabuf.create_params(&qh, ());
    params.add(
        memfd.as_fd(),
        0,                       // plane_idx
        0,                       // offset
        (WIDTH as u32) * 4,      // stride (ARGB8888 = 4 bytes/pixel)
        MODIFIER_HI,
        MODIFIER_LO,
    );
    let _buffer: WlBuffer =
        params.create_immed(WIDTH, HEIGHT, DRM_FORMAT_ARGB8888, Flags::empty(), &qh, ());

    // Round-trip so the proxy has dispatched the whole sequence. If create_immed had produced a protocol
    // error (the "invalid wl_buffer" abort), this round-trip would surface it as an error — it must not.
    queue
        .roundtrip(&mut AppData)
        .expect("round-trip after create_immed — no create_immed protocol error");

    // Exactly one message should have been forwarded (the token); create_params/add do not cross.
    let messages = collector.messages.lock().unwrap();
    let token = messages
        .iter()
        .find_map(|m| {
            m.args.iter().find_map(|a| match a {
                WaylandArg::Buffer(tok) => Some(tok.clone()),
                _ => None,
            })
        })
        .unwrap_or_else(|| panic!("no BufferToken was forwarded; messages: {messages:?}"));

    // The token must name the resolved resource and carry create_immed's geometry and add's modifier.
    assert_eq!(token.resource_id, RESOURCE_ID, "wrong resource id in token");
    assert_eq!(token.width, WIDTH as u32, "wrong width in token");
    assert_eq!(token.height, HEIGHT as u32, "wrong height in token");
    assert_eq!(token.drm_format, DRM_FORMAT_ARGB8888, "wrong format in token");
    assert_eq!(token.modifier, MODIFIER, "wrong modifier in token");

    // And the message must also name the new wl_buffer (its app-side id) so S can bind it.
    let named_buffer = messages
        .iter()
        .any(|m| m.args.iter().any(|a| matches!(a, WaylandArg::NewId(_))));
    assert!(named_buffer, "the token message did not name the wl_buffer id");
}

/// Create an anonymous in-memory file (memfd) to stand in for a swapchain image's blob memfd.
fn make_memfd() -> OwnedFd {
    use std::ffi::CString;
    let name = CString::new("rayland-blob-test").unwrap();
    // SAFETY: `memfd_create` returns a fresh fd or -1; we check and take ownership via OwnedFd.
    let raw = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    assert!(raw >= 0, "memfd_create failed: {}", std::io::Error::last_os_error());
    // SAFETY: `raw` is a valid, freshly-owned fd we have not otherwise registered.
    unsafe { std::os::fd::FromRawFd::from_raw_fd(raw) }
}

/// Read an fd's `(st_dev, st_ino)` — mirrors the proxy's own inode read so the resolver key matches.
fn fd_inode(fd: &OwnedFd) -> Option<(u64, u64)> {
    // SAFETY: zeroed stat is a valid target; fstat populates it on success (return 0), read only then.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut st) == 0 {
            Some((st.st_dev as u64, st.st_ino as u64))
        } else {
            None
        }
    }
}

/// Connect to `path`, retrying briefly to absorb the listener-bind race.
fn connect_with_retry(path: &PathBuf, timeout: Duration) -> std::io::Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}
