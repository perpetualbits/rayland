//! Integration proof for WP0 Task 3b **sub-step 3 (request forwarding)**: a real Wayland client's request
//! crosses the proxy, is translated, and arrives at the (stubbed) S sink.
//!
//! # What this proves
//! The proxy's job for every non-buffer request is: receive it as a `wayland-backend` `Message`, translate
//! it to a wire [`rayland_relay::WaylandMessage`], and forward it. The scalar argument translation is
//! unit-tested in the module itself; what a pure test *cannot* build is a real `ObjectId`, so the
//! `Object`/`NewId` cases — the ones carrying object ids — are proven here instead, end to end: a real
//! client binds `wl_compositor` and calls `create_surface`, and we assert the collector recorded a
//! forwarded message bearing a translated `NewId` (the new surface's id). That closes the loop the unit
//! tests cannot.
//!
//! It deliberately does **not** exercise the fd→token path (`create_immed`); that is sub-step 4.
//!
//! Like the connect test, it skips gracefully where libwayland is absent (the client backend is dlopen'd).

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rayland_c::wayland_proxy::{ResourceResolver, WaylandSink};
use rayland_relay::{WaylandArg, WaylandMessage};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, QueueHandle};

/// A sink that records every forwarded request, so the test can inspect what crossed the proxy. This is
/// the "stubbed S collector" the plan specifies for the Task 3b proof; it stands in for Task 4's real link.
#[derive(Default)]
struct Collector {
    /// Every [`WaylandMessage`] the proxy forwarded, in arrival order, behind a mutex (the proxy forwards
    /// from its own dispatch thread; the test reads from the main thread).
    messages: Mutex<Vec<WaylandMessage>>,
    /// Every global bind the proxy forwarded, as `(interface, version, app_object_id)`.
    binds: Mutex<Vec<(String, u32, u32)>>,
}
impl WaylandSink for Collector {
    /// Record one forwarded request for the test to assert on.
    fn forward_request(&self, msg: WaylandMessage) {
        self.messages.lock().unwrap().push(msg);
    }
    /// Record one forwarded bind for the test to assert on.
    fn forward_bind(&self, interface: &str, version: u32, app_object_id: u32) {
        self.binds
            .lock()
            .unwrap()
            .push((interface.to_string(), version, app_object_id));
    }
}

/// A resolver that recognises no inode — this test forwards only `create_surface`, never a buffer, so the
/// resolver is never consulted; it only satisfies `run`'s signature.
struct NullResolver;
impl ResourceResolver for NullResolver {
    fn resolve_inode(&self, _dev: u64, _ino: u64) -> Option<u32> {
        None
    }
}

/// The client-side dispatch state — carries nothing; the test reacts to no events.
struct AppData;

// The registry Dispatch impl `registry_queue_init` requires; no post-init registry events are handled.
impl Dispatch<WlRegistry, GlobalListContents> for AppData {
    fn event(
        _state: &mut Self,
        _registry: &WlRegistry,
        _event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// wl_compositor has no events; the handler is unreachable but the trait bound is required to bind it.
impl Dispatch<WlCompositor, ()> for AppData {
    fn event(
        _state: &mut Self,
        _compositor: &WlCompositor,
        _event: <WlCompositor as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// wl_surface has events (enter/leave/preferred_buffer_*); the test ignores all of them.
impl Dispatch<WlSurface, ()> for AppData {
    fn event(
        _state: &mut Self,
        _surface: &WlSurface,
        _event: <WlSurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// Bind `wl_compositor` through the proxy, call `create_surface`, and assert the proxy forwarded a request
/// carrying the new surface's id as a translated `NewId`.
#[test]
fn create_surface_is_forwarded_with_a_translated_new_id() {
    let socket_path: PathBuf = std::env::temp_dir()
        .join(format!("rayland-wp-proxy-fwd-{}.sock", std::process::id()));

    // Stand the proxy up with a recording sink on its own thread; keep our handle to the collector.
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

    // Discover globals and bind wl_compositor (version capped low — the proxy advertises the full
    // descriptor version, and binding at or below it is always allowed).
    let (globals, mut queue) =
        registry_queue_init::<AppData>(&conn).expect("registry round-trips against the proxy");
    let qh = queue.handle();
    let compositor: WlCompositor = globals
        .bind(&qh, 1..=4, ())
        .expect("proxy lets the client bind wl_compositor");

    // The request under test: create a surface. This emits `wl_compositor.create_surface(new_id)`, which
    // reaches the proxy's request hook (unlike registry.bind, a backend built-in).
    let _surface: WlSurface = compositor.create_surface(&qh, ());

    // Round-trip so the proxy has certainly dispatched the request before we inspect the collector.
    queue
        .roundtrip(&mut AppData)
        .expect("round-trip after create_surface");

    // Assert: the wl_compositor bind crossed as a forwarded bind — S needs it to reconstruct the object
    // the create_surface below targets (the app's wl_registry.bind itself never reaches the proxy).
    let binds = collector.binds.lock().unwrap();
    assert!(
        binds.iter().any(|(iface, _v, _id)| iface == "wl_compositor"),
        "the proxy did not forward the wl_compositor bind; binds were: {binds:?}"
    );

    // Assert: at least one forwarded message carries a translated NewId that names its interface — the
    // surface's id crossed as an id *with* its interface (`wl_surface`), which is what lets S create the
    // right child object via send_request's child_spec. This proves the Object/NewId translation, the
    // interface stamping, and the forward path end to end.
    let messages = collector.messages.lock().unwrap();
    assert!(
        !messages.is_empty(),
        "the proxy forwarded nothing; expected at least the create_surface request"
    );
    let saw_typed_new_id = messages.iter().any(|m| {
        m.args.iter().any(|a| {
            matches!(a, WaylandArg::NewId { interface, .. } if interface == "wl_surface")
        })
    });
    assert!(
        saw_typed_new_id,
        "no forwarded message carried a wl_surface NewId; forwarded messages were: {messages:?}"
    );
}

/// Connect to `path`, retrying briefly to absorb the listener-bind race (see the connect test for detail).
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
