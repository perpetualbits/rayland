//! Integration proof for WP0 **Task 4.4a**: the proxy answers the dmabuf *format capability* locally, so a
//! Venus WSI client gets past `pick_surface_format` without any events crossing the network.
//!
//! # Why this is a local answer, not a relay
//! Mesa's WSI discovers swapchain formats with a **bounded** roundtrip: it binds `zwp_linux_dmabuf_v1`,
//! collects whatever `modifier` events have arrived, and if that set is empty it aborts (`count >= 1`
//! fails). The C-side proxy is a real `wayland-backend` server, and that backend answers `wl_display.sync`
//! **locally and immediately** — so relaying the compositor's format events from S would race the app's
//! roundtrip and lose it on a real network (winning only on loopback timing). Format advertisement is a
//! *capability handshake*; a proxy answers it from known truth, the same way the backend already answers
//! the registry. See `docs/design/2026-07-22-wp0-wayland-proxy-first-light.md` and the diary entry dated
//! 2026-07-24.
//!
//! # What this proves
//! 1. The proxy advertises `zwp_linux_dmabuf_v1` at **version 3** — capped below the v4 `get_default_feedback`
//!    path, whose format table arrives as an fd that cannot cross a network. At v3 Mesa uses the fd-free
//!    `modifier` events instead.
//! 2. On bind, the proxy emits at least one `modifier(format, hi, lo)` event, so the app sees `count >= 1`
//!    supported formats. The advertised formats are LINEAR (modifier 0) XRGB8888/ARGB8888 — what the LINEAR
//!    HOST3D swapchain export uses.
//!
//! Like the sibling proxy tests, it skips where libwayland is absent.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayland_c::wayland_proxy::{ResourceResolver, WaylandSink};
use rayland_relay::WaylandMessage;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::{
    Event as DmabufEvent, ZwpLinuxDmabufV1,
};

/// DRM fourcc 'XR24' — the standard opaque 32-bit swapchain format.
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
/// DRM fourcc 'AR24' — the standard 32-bit swapchain format with alpha.
const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;

/// A sink and resolver that do nothing: this test exercises only the format-advertisement return path, which
/// originates on the proxy itself and touches neither the forward sink nor the inode resolver.
struct NullSink;
impl WaylandSink for NullSink {
    fn forward_request(&self, _msg: WaylandMessage) {}
    fn forward_bind(&self, _interface: &str, _version: u32, _app_object_id: u32) {}

    /// Ignored: this test forwards no `wl_shm` traffic. Present so the sink satisfies the trait.
    fn forward_shm_pool_data(&self, _app_pool_id: u32, _offset: u32, _bytes: Vec<u8>) {}
}
struct NullResolver;
impl ResourceResolver for NullResolver {
    fn resolve_inode(&self, _dev: u64, _ino: u64) -> Option<u32> {
        None
    }
}

/// Client dispatch state: collects the `modifier` events the proxy synthesizes on bind.
#[derive(Default)]
struct AppData {
    /// Each `(format, modifier_hi, modifier_lo)` the proxy advertised.
    modifiers: Vec<(u32, u32, u32)>,
}

impl Dispatch<WlRegistry, GlobalListContents> for AppData {
    fn event(
        _s: &mut Self,
        _r: &WlRegistry,
        _e: <WlRegistry as wayland_client::Proxy>::Event,
        _d: &GlobalListContents,
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for AppData {
    /// Record `modifier` events; ignore the deprecated bare `format` event (Mesa treats it as a no-op).
    fn event(
        state: &mut Self,
        _proxy: &ZwpLinuxDmabufV1,
        event: DmabufEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let DmabufEvent::Modifier {
            format,
            modifier_hi,
            modifier_lo,
        } = event
        {
            state.modifiers.push((format, modifier_hi, modifier_lo));
        }
    }
}

/// Bind the proxy's dmabuf global and assert it is capped at v3 and advertises at least one LINEAR format.
#[test]
fn dmabuf_is_capped_at_v3_and_advertises_a_linear_format() {
    let socket_path: PathBuf =
        std::env::temp_dir().join(format!("rayland-wp-proxy-fmt-{}.sock", std::process::id()));

    let proxy_path = socket_path.clone();
    std::thread::spawn(move || {
        if let Err(e) =
            rayland_c::wayland_proxy::run(proxy_path, Arc::new(NullSink), Arc::new(NullResolver))
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

    // The proxy must advertise dmabuf at v3 — not the descriptor's max (v6), which would let Mesa take the
    // v4 feedback path and its fd-bearing format table.
    let advertised = globals
        .contents()
        .clone_list()
        .into_iter()
        .find(|g| g.interface == "zwp_linux_dmabuf_v1")
        .map(|g| g.version)
        .expect("the proxy advertises zwp_linux_dmabuf_v1");
    assert_eq!(
        advertised, 3,
        "dmabuf must be capped at v3 so Mesa uses the fd-free modifier path, not v4 feedback"
    );

    // Bind at v3 and collect the modifier events the proxy synthesizes on bind. The bound object must stay
    // alive for the roundtrip that reads its events.
    let _dmabuf: ZwpLinuxDmabufV1 = globals
        .bind(&qh, 3..=3, ())
        .expect("proxy lets the client bind zwp_linux_dmabuf_v1 at v3");

    let mut app = AppData::default();
    queue
        .roundtrip(&mut app)
        .expect("round-trip collects the proxy's synthesized modifier events");

    assert!(
        !app.modifiers.is_empty(),
        "the proxy advertised no formats; Mesa's pick_surface_format would abort at count >= 1"
    );
    // The advertised formats must include a LINEAR (modifier 0) 32-bit format — what a LINEAR swapchain uses.
    let has_linear = app.modifiers.iter().any(|(f, hi, lo)| {
        (*f == DRM_FORMAT_ARGB8888 || *f == DRM_FORMAT_XRGB8888) && *hi == 0 && *lo == 0
    });
    assert!(
        has_linear,
        "expected a LINEAR ARGB/XRGB8888 modifier; got {:?}",
        app.modifiers
    );
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
