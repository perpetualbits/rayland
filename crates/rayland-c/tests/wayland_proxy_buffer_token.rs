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
use wayland_client::globals::{GlobalListContents, registry_queue_init};
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
/// The plane's row pitch, **deliberately not `WIDTH * 4`** (which would be 256).
///
/// This is the point of the whole stride field. A driver may pad rows for alignment, and S must use the
/// value the application actually declared rather than recomputing `width × bpp` — a wrong stride skews
/// the image instead of raising an error. A fixture whose stride *happened* to equal the derived value
/// would pass just as well against an implementation that derives it, which is exactly the implementation
/// this field exists to prevent. So the fixture picks a padded stride that no derivation can produce.
const STRIDE: u32 = 320;
/// The plane's byte offset within the dma-buf, **deliberately non-zero**, for the same reason: an
/// implementation that assumed zero would pass against an all-zero fixture.
const OFFSET: u32 = 4096;

/// A sink that records forwarded requests, so the test can inspect the emitted token.
#[derive(Default)]
struct Collector {
    messages: Mutex<Vec<WaylandMessage>>,
}
impl WaylandSink for Collector {
    fn forward_request(&self, msg: WaylandMessage) {
        self.messages.lock().unwrap().push(msg);
    }
    // This test asserts on the forwarded token, not on binds; record nothing for a bind.
    fn forward_bind(&self, _interface: &str, _version: u32, _app_object_id: u32) {}

    /// Ignored: this test forwards no `wl_shm` traffic. Present so the sink satisfies the trait.
    fn forward_shm_pool_data(&self, _app_pool_id: u32, _offset: u32, _bytes: Vec<u8>) {}
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

/// Everything one test needs to drive the buffer-creation sequence: a running proxy, a client connected
/// to it with the dmabuf global bound, and the collector recording whatever the proxy forwards.
struct Harness {
    /// Records forwarded requests, so a test can assert on the token — or on its absence.
    collector: Arc<Collector>,
    /// The client's event queue; `roundtrip` on it flushes requests and surfaces protocol errors.
    queue: wayland_client::EventQueue<AppData>,
    /// The bound `zwp_linux_dmabuf_v1`, the object every buffer-creation sequence starts from.
    dmabuf: ZwpLinuxDmabufV1,
    /// The memfd standing in for a swapchain image, kept alive for the test's duration: the proxy
    /// `fstat`s the fd it receives, and the resolver only recognises *this* file's inode.
    memfd: OwnedFd,
}

/// Start a proxy on its own socket, connect a client, and bind the dmabuf global.
///
/// `tag` distinguishes the socket path, because these tests run in parallel threads of one process and a
/// shared path would have them fight over the same listener.
///
/// Returns `None` when the wayland-client backend cannot start — the same skip the sibling tests take
/// where libwayland is absent — so a caller's `let Some(h) = ... else { return }` reads as "skip".
fn start_proxy(tag: &str) -> Option<Harness> {
    let socket_path: PathBuf = std::env::temp_dir().join(format!(
        "rayland-wp-proxy-{tag}-{}.sock",
        std::process::id()
    ));

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
            return None;
        }
    };

    let (globals, queue) =
        registry_queue_init::<AppData>(&conn).expect("registry round-trips against the proxy");
    // Bind the dmabuf global at a version that supports create_immed (since v2) and modifiers (since v3).
    let dmabuf: ZwpLinuxDmabufV1 = globals
        .bind(&queue.handle(), 3..=3, ())
        .expect("proxy lets the client bind zwp_linux_dmabuf_v1");

    Some(Harness {
        collector,
        queue,
        dmabuf,
        memfd,
    })
}

/// The `BufferToken` the proxy forwarded, or `None` if it forwarded no token at all.
///
/// Both refusal tests below assert on `None`, which is the whole point: a refused buffer must leave S
/// with nothing to present rather than with an approximation.
fn forwarded_token(collector: &Collector) -> Option<rayland_relay::BufferToken> {
    collector.messages.lock().unwrap().iter().find_map(|m| {
        m.args.iter().find_map(|a| match a {
            WaylandArg::Buffer(tok) => Some(tok.clone()),
            _ => None,
        })
    })
}

/// Run the swapchain buffer-creation sequence through the proxy and assert the emitted `BufferToken`.
#[test]
fn create_immed_emits_a_correct_buffer_token_and_never_asserts() {
    let Some(Harness {
        collector,
        mut queue,
        dmabuf,
        memfd,
    }) = start_proxy("token")
    else {
        return; // no libwayland — skip, as the sibling tests do
    };
    let qh = queue.handle();

    // The swapchain buffer-creation sequence Mesa's WSI runs, driven explicitly here.
    let params: ZwpLinuxBufferParamsV1 = dmabuf.create_params(&qh, ());
    params.add(
        memfd.as_fd(),
        0,      // plane_idx — the single supported plane
        OFFSET, // offset — non-zero, so an implementation assuming 0 fails here
        STRIDE, // stride — padded, so an implementation deriving width x bpp fails here
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
    let token = forwarded_token(&collector).unwrap_or_else(|| {
        panic!(
            "no BufferToken was forwarded; messages: {:?}",
            collector.messages.lock().unwrap()
        )
    });

    // The token must name the resolved resource and carry create_immed's geometry and add's modifier.
    assert_eq!(token.resource_id, RESOURCE_ID, "wrong resource id in token");
    assert_eq!(token.width, WIDTH as u32, "wrong width in token");
    assert_eq!(token.height, HEIGHT as u32, "wrong height in token");
    assert_eq!(
        token.drm_format, DRM_FORMAT_ARGB8888,
        "wrong format in token"
    );
    assert_eq!(token.modifier, MODIFIER, "wrong modifier in token");
    // The two fields with teeth. STRIDE is not WIDTH*4 and OFFSET is not 0, so neither of these can be
    // satisfied by a derivation from the geometry — only by carrying what `add` actually supplied.
    assert_eq!(
        token.stride,
        STRIDE,
        "wrong stride in token — a derived width x bpp would be {}",
        (WIDTH as u32) * 4
    );
    assert_eq!(
        token.offset, OFFSET,
        "wrong offset in token — an assumed offset would be 0"
    );

    // And the message must also name the new wl_buffer (its app-side id, interface `wl_buffer`) so S can
    // create the buffer object for the token.
    let messages = collector.messages.lock().unwrap();
    let named_buffer = messages.iter().any(|m| {
        m.args
            .iter()
            .any(|a| matches!(a, WaylandArg::NewId { interface, .. } if interface == "wl_buffer"))
    });
    assert!(
        named_buffer,
        "the token message did not name the wl_buffer id"
    );
}

/// The asynchronous `params.create` path (opcode 2) is unsupported in WP0 and must be refused cleanly:
/// no token is forwarded, and no protocol error is raised (the request is consumed, not mis-forwarded).
#[test]
fn async_create_is_refused_without_forwarding_a_token() {
    let Some(Harness {
        collector,
        mut queue,
        dmabuf,
        memfd,
    }) = start_proxy("async")
    else {
        return; // no libwayland — skip
    };
    let qh = queue.handle();

    let params: ZwpLinuxBufferParamsV1 = dmabuf.create_params(&qh, ());
    params.add(memfd.as_fd(), 0, OFFSET, STRIDE, MODIFIER_HI, MODIFIER_LO);
    // The async variant: no new_id in the request; the wl_buffer would arrive via a `created` event.
    params.create(WIDTH, HEIGHT, DRM_FORMAT_ARGB8888, Flags::empty());

    // Must not raise a protocol error — the proxy consumes the request cleanly.
    queue
        .roundtrip(&mut AppData)
        .expect("round-trip after async create — no protocol error");

    // Nothing at all should have been forwarded: create_params, add, and the async create are each
    // consumed by the interception. In particular the async create must not fall through to the generic
    // forward path (which would ship geometry-with-no-token to S, misrepresenting an unsupported request).
    let messages = collector.messages.lock().unwrap();
    assert!(
        messages.is_empty(),
        "async create path forwarded something (expected nothing); messages: {messages:?}"
    );
}

/// A `params.add` naming a plane other than 0 must refuse the whole buffer: no token is forwarded.
///
/// # Why refusal is the right behaviour rather than a best effort
/// A [`rayland_relay::BufferToken`] describes exactly one plane — one `offset`, one `stride`. A
/// multi-plane buffer (planar YUV, or an auxiliary compression plane) cannot be expressed by it, and the
/// proxy advertises only single-plane LINEAR formats, so a non-zero `plane_idx` means an assumption
/// underneath WP0 has broken. Forwarding one plane's layout and calling it the buffer would put a garbled
/// image on S's screen with nothing logged anywhere; refusing puts the break in the log at the moment it
/// happens, and leaves the app with a locally valid `wl_buffer` that S is simply never told to present.
#[test]
fn a_non_zero_plane_index_refuses_the_buffer() {
    let Some(Harness {
        collector,
        mut queue,
        dmabuf,
        memfd,
    }) = start_proxy("plane")
    else {
        return; // no libwayland — skip
    };
    let qh = queue.handle();

    let params: ZwpLinuxBufferParamsV1 = dmabuf.create_params(&qh, ());
    // plane_idx 1: the second plane of a multi-plane buffer. Everything else is the supported shape, so
    // this test isolates the plane index as the single reason for refusal.
    params.add(memfd.as_fd(), 1, OFFSET, STRIDE, MODIFIER_HI, MODIFIER_LO);
    let _buffer: WlBuffer =
        params.create_immed(WIDTH, HEIGHT, DRM_FORMAT_ARGB8888, Flags::empty(), &qh, ());

    // The refusal must be clean: consumed by the proxy, so the app sees no protocol error at all.
    queue.roundtrip(&mut AppData).expect(
        "round-trip after a refused create_immed — the refusal must not be a protocol error",
    );

    assert!(
        forwarded_token(&collector).is_none(),
        "a plane_idx != 0 buffer was forwarded to S; it must be refused, not approximated. messages: {:?}",
        collector.messages.lock().unwrap()
    );
}

/// A second `params.add` on the same params object must refuse the whole buffer: no token is forwarded.
///
/// This is the same single-plane rule as [`a_non_zero_plane_index_refuses_the_buffer`], reached the other
/// way: a well-formed multi-plane buffer supplies each plane with its own `add`, all of which may name
/// plane indices the proxy would accept individually. Only the *count* gives it away. Without this rule
/// the last `add` would silently win and its stride would describe the whole buffer.
#[test]
fn a_second_add_refuses_the_buffer() {
    let Some(Harness {
        collector,
        mut queue,
        dmabuf,
        memfd,
    }) = start_proxy("twoadd")
    else {
        return; // no libwayland — skip
    };
    let qh = queue.handle();

    let params: ZwpLinuxBufferParamsV1 = dmabuf.create_params(&qh, ());
    // Two adds, each individually acceptable — plane 0, a resolvable fd. The pair is what is refused.
    params.add(memfd.as_fd(), 0, OFFSET, STRIDE, MODIFIER_HI, MODIFIER_LO);
    params.add(memfd.as_fd(), 0, OFFSET, STRIDE, MODIFIER_HI, MODIFIER_LO);
    let _buffer: WlBuffer =
        params.create_immed(WIDTH, HEIGHT, DRM_FORMAT_ARGB8888, Flags::empty(), &qh, ());

    queue.roundtrip(&mut AppData).expect(
        "round-trip after a refused create_immed — the refusal must not be a protocol error",
    );

    assert!(
        forwarded_token(&collector).is_none(),
        "a multi-plane (two-add) buffer was forwarded to S; it must be refused. messages: {:?}",
        collector.messages.lock().unwrap()
    );
}

/// Create an anonymous in-memory file (memfd) to stand in for a swapchain image's blob memfd.
fn make_memfd() -> OwnedFd {
    use std::ffi::CString;
    let name = CString::new("rayland-blob-test").unwrap();
    // SAFETY: `memfd_create` returns a fresh fd or -1; we check and take ownership via OwnedFd.
    let raw = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    assert!(
        raw >= 0,
        "memfd_create failed: {}",
        std::io::Error::last_os_error()
    );
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
