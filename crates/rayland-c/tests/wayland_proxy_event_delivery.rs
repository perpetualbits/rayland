//! Integration proof for WP0 **Task 4.4b (C-side delivery)**: a compositor event handed to the proxy from
//! another thread reaches the application as a real Wayland event.
//!
//! # What this proves
//! The proxy's event return path works end to end on the C side: [`wayland_event_channel`] links a
//! [`WaylandEventPoster`] (which the daemon's link reader thread uses when an `S2C::WaylandEvent` arrives)
//! to the proxy's serve loop. Posting a [`rayland_relay::WaylandMessage`] wakes the loop (via its eventfd
//! third pollfd), which resolves the event's app-side object id to the live object and delivers it with
//! `Handle::send_event`. Here the test plays both the app (a real `wayland-client`) and the reader thread
//! (calling `post`), and asserts a posted `xdg_wm_base.ping` is received by the client.
//!
//! The S-side half — translating a *real* compositor event's ids S→app and posting it — is proven
//! separately (`rayland-s`'s replay test). Like the sibling proxy tests, this skips where libwayland is
//! absent.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayland_c::wayland_proxy::{wayland_event_channel, ResourceResolver, WaylandSink};
use rayland_relay::{WaylandArg, WaylandMessage};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::xdg_wm_base::{Event as XdgWmBaseEvent, XdgWmBase};

/// `xdg_wm_base.ping` event opcode (event 0), wire signature `[serial: uint]`. The event the test posts.
const OP_XDG_WM_BASE_PING: u16 = 0;
/// The serial the test pushes through the return path and expects to read back unchanged.
const PING_SERIAL: u32 = 0xABCD_1234;

struct NullSink;
impl WaylandSink for NullSink {
    fn forward_request(&self, _msg: WaylandMessage) {}
    fn forward_bind(&self, _interface: &str, _version: u32, _app_object_id: u32) {}
}
struct NullResolver;
impl ResourceResolver for NullResolver {
    fn resolve_inode(&self, _dev: u64, _ino: u64) -> Option<u32> {
        None
    }
}

/// Client dispatch state: records the serials of any `xdg_wm_base.ping` events the app receives.
#[derive(Default)]
struct AppData {
    pings: Vec<u32>,
}

impl Dispatch<WlRegistry, GlobalListContents> for AppData {
    fn event(
        _s: &mut Self,
        _r: &WlRegistry,
        _e: <WlRegistry as Proxy>::Event,
        _d: &GlobalListContents,
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgWmBase, ()> for AppData {
    /// Record the serial of a `ping` event — the event the proxy delivered from the posted message.
    fn event(
        state: &mut Self,
        _proxy: &XdgWmBase,
        event: XdgWmBaseEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let XdgWmBaseEvent::Ping { serial } = event {
            state.pings.push(serial);
        }
    }
}

/// Post an `xdg_wm_base.ping` through the proxy's return path and assert the app receives it.
#[test]
fn a_posted_compositor_event_reaches_the_app() {
    let socket_path: PathBuf =
        std::env::temp_dir().join(format!("rayland-wp-proxy-evt-{}.sock", std::process::id()));

    // The return-path channel: the inbox goes to the proxy, the poster stays here (the test plays the
    // daemon's reader thread).
    let (poster, inbox) = wayland_event_channel().expect("create the event channel");

    let proxy_path = socket_path.clone();
    std::thread::spawn(move || {
        if let Err(e) = rayland_c::wayland_proxy::run_with_events(
            proxy_path,
            Arc::new(NullSink),
            Arc::new(NullResolver),
            inbox,
        ) {
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

    // Bind xdg_wm_base and let the proxy record the object, so the posted event can resolve to it.
    let wm_base: XdgWmBase = globals
        .bind(&qh, 1..=1, ())
        .expect("proxy lets the client bind xdg_wm_base");
    queue
        .roundtrip(&mut AppData::default())
        .expect("round-trip so the proxy has recorded the bound object");

    // Now, as the reader thread would on an S2C::WaylandEvent, post a ping targeting that object.
    poster.post(WaylandMessage {
        object_id: wm_base.id().protocol_id(),
        opcode: OP_XDG_WM_BASE_PING,
        args: vec![WaylandArg::Uint(PING_SERIAL)],
    });

    // Dispatch until the ping arrives (the delivery is asynchronous to our round-trip) or a deadline.
    let mut app = AppData::default();
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.pings.is_empty() && Instant::now() < deadline {
        queue.roundtrip(&mut app).expect("round-trip while awaiting the ping");
        if app.pings.is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    assert_eq!(
        app.pings,
        vec![PING_SERIAL],
        "the posted xdg_wm_base.ping did not reach the app through the proxy's return path"
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
