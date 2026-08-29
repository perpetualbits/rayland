//! Regression proof for the WP0 **recycled-id race**: a Wayland protocol id is a *slot number*, not an
//! object identity, and the proxy's bookkeeping must not confuse the two.
//!
//! # The bug this exists to forbid
//! `wl_callback.done` is a **destructor** event: delivering it destroys the callback. libwayland then
//! immediately reuses the freed id for the next object the application creates. The proxy's
//! `ObjectData::destroyed` used to prune its delivery map by bare `protocol_id`, and the backend reports a
//! destruction *after* it has already dispatched the requests that followed — so callback #1's late
//! `destroyed()` deleted **callback #2's** entry, and #2's `done` was dropped as `unknown-object`.
//!
//! The application then waits forever for a frame callback that was delivered to nobody. That is exactly
//! what froze vkcube's cube on S's screen after a single frame; see
//! `docs/data/2026-08-29-wp0-event-witness/` for the captured logs of the live failure.
//!
//! # Why this test is trustworthy
//! It fails against the old remove-by-number code and passes against remove-by-identity — verified by
//! mutation, not assumed. The second `done` is the whole assertion: it can only arrive if the proxy still
//! holds the *second* callback under the recycled id.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayland_c::wayland_proxy::{wayland_event_channel, ResourceResolver, WaylandSink};
use rayland_relay::{WaylandArg, WaylandMessage};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_callback::{Event as WlCallbackEvent, WlCallback};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

/// `wl_callback.done` event opcode (its only event), wire signature `[callback_data: uint]`.
const OP_WL_CALLBACK_DONE: u16 = 0;
/// How many frame callbacks to drive through the recycled slot.
///
/// # Why a loop rather than a single reuse
/// Whether the bug bites on any one cycle is a **race**: it needs the backend's late `destroyed()` for
/// callback *N* to land after the proxy has registered callback *N+1*. Measured against the unfixed code,
/// a single cycle reproduced it only about 2 times in 10 — a test that catches the defect a fifth of the
/// time is not a regression test, it is a coin flip that would sit green in CI with the bug present.
///
/// A real application re-arms every frame, so the honest fix is to do the same: each cycle is an
/// independent chance, and thirty of them make a miss vanishingly unlikely while costing milliseconds.
/// The assertion is that **every** callback got its `done`, which is exactly the property vkcube needs.
const FRAMES: usize = 30;

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

/// The application's state: which `callback_data` values arrived, and the surface to re-arm from.
#[derive(Default)]
struct AppData {
    /// Every `wl_callback.done` payload the app received, in order.
    dones: Vec<u32>,
    /// The surface, so the `done` handler can request the next frame the way a real application does.
    surface: Option<WlSurface>,
    /// The id of the callback created from inside the `done` handler — the recycled one.
    recycled_id: Option<u32>,
}

impl Dispatch<WlRegistry, GlobalListContents> for AppData {
    fn event(
        _s: &mut Self,
        _p: &WlRegistry,
        _e: <WlRegistry as Proxy>::Event,
        _d: &GlobalListContents,
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
    }
}
macro_rules! ignore_events {
    ($iface:ty) => {
        impl Dispatch<$iface, ()> for AppData {
            fn event(
                _s: &mut Self,
                _p: &$iface,
                _e: <$iface as Proxy>::Event,
                _d: &(),
                _c: &Connection,
                _q: &QueueHandle<Self>,
            ) {
            }
        }
    };
}
ignore_events!(WlCompositor);
ignore_events!(WlSurface);

impl Dispatch<WlCallback, ()> for AppData {
    /// Record each `done` payload. Receiving this event also **destroys** the callback client-side, which
    /// is what frees its id for reuse — the mechanism this whole test is about.
    fn event(
        state: &mut Self,
        _proxy: &WlCallback,
        event: WlCallbackEvent,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let WlCallbackEvent::Done { callback_data } = event else {
            return;
        };
        state.dones.push(callback_data);
        // **Re-arm from inside the handler, which is what makes this test reproduce the race.**
        // A real application (vkcube) asks for its next frame callback the moment the previous one
        // fires, so the new `frame` request reaches the proxy in the *same* dispatch batch that just
        // delivered `done`. The proxy therefore registers the new object BEFORE the backend gets round
        // to reporting the old one's destruction — and it is that ordering, not the id reuse alone,
        // that lets a remove-by-number cleanup delete the live object. An earlier version of this test
        // did a round-trip between the two callbacks, which let the destruction land first and made the
        // test pass against the very bug it exists to catch.
        if state.dones.len() < FRAMES {
            if let Some(surface) = state.surface.clone() {
                let next: WlCallback = surface.frame(qh, ());
                state.recycled_id = Some(next.id().protocol_id());
            }
        }
    }
}

/// A callback whose id was recycled from a destroyed one still receives its own `done`.
#[test]
fn a_recycled_callback_id_still_receives_its_own_done() {
    let socket_path: PathBuf = std::env::temp_dir().join(format!(
        "rayland-wp-proxy-recycle-{}.sock",
        std::process::id()
    ));

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
    let compositor: WlCompositor = globals
        .bind(&qh, 1..=4, ())
        .expect("proxy lets the client bind wl_compositor");
    let surface: WlSurface = compositor.create_surface(&qh, ());
    let mut app = AppData {
        surface: Some(surface.clone()),
        ..Default::default()
    };
    queue.roundtrip(&mut app).expect("surface reaches the proxy");

    // --- Drive FRAMES callbacks through the same recycled slot, as an animating app does. -------------
    let first: WlCallback = surface.frame(&qh, ());
    let mut next_id = first.id().protocol_id();
    let first_id = next_id;
    queue.roundtrip(&mut app).expect("the first callback reaches the proxy");

    let mut recycled_at_least_once = false;
    for frame in 0..FRAMES {
        // Fire the callback the app is currently waiting on. Its `done` handler immediately requests the
        // next one — in the same dispatch batch, which is the ordering the race needs.
        poster.post(done_for(next_id, frame as u32));
        let want = frame + 1;
        wait_for(&mut queue, &mut app, |a| a.dones.len() >= want);
        if app.dones.len() < want {
            // The stall itself. Report which frame died and on which id, since that is the whole finding.
            panic!(
                "frame {frame}: the callback on recycled id {next_id} never received its `done` — a \
                 destroyed object's late cleanup removed the live object that inherited its slot. \
                 {} of {FRAMES} dones arrived.",
                app.dones.len()
            );
        }
        match app.recycled_id.take() {
            Some(id) => {
                recycled_at_least_once |= id == first_id;
                next_id = id;
            }
            // The last iteration deliberately does not re-arm.
            None => break,
        }
        // Let the proxy see the new `frame` request before the next event is posted.
        queue.roundtrip(&mut app).expect("the next callback reaches the proxy");
    }

    assert_eq!(
        app.dones.len(),
        FRAMES,
        "every callback must receive its own `done`; got {:?}",
        app.dones.len()
    );
    assert!(
        recycled_at_least_once,
        "no callback ever reused the first one's id ({first_id}), so the race this test guards against \
         was never exercised and the assertion above proves nothing about it"
    );
}

/// A `wl_callback.done` message addressed to `app_object_id`, as the link reader would post it.
fn done_for(app_object_id: u32, payload: u32) -> WaylandMessage {
    WaylandMessage {
        object_id: app_object_id,
        opcode: OP_WL_CALLBACK_DONE,
        args: vec![WaylandArg::Uint(payload)],
    }
}

/// Pump the queue until `done` holds or a short deadline passes.
///
/// Does **not** assert: the caller does, so a failure reports what actually arrived rather than a timeout.
fn wait_for(
    queue: &mut wayland_client::EventQueue<AppData>,
    app: &mut AppData,
    done: impl Fn(&AppData) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !done(app) {
        queue.roundtrip(app).expect("round-trip while awaiting an event");
        std::thread::sleep(Duration::from_millis(5));
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
