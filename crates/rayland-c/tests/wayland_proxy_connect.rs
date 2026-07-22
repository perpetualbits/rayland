//! Integration proof for WP0 Task 3b **sub-step 2 (globals + connect)**: a real Wayland client connects to
//! the C-side proxy and sees the minimal global set vkcube binds.
//!
//! # What this proves, and what it deliberately does not
//! The proxy is a Wayland *server* to the application. This test plays the application's opening move —
//! connect, `get_registry`, read the advertised globals — and asserts the four WP0 globals
//! (`wl_compositor`, `xdg_wm_base`, `zwp_linux_dmabuf_v1`, `wl_seat`) are present. That is exactly the
//! "connects and binds the WP0 globals" bar the plan sets for this sub-step. It does **not** exercise
//! request forwarding to S or the `create_immed` fd→token interception — those are later sub-steps with
//! their own proofs (the translation unit tests and, ultimately, vkcube).
//!
//! # Why it may skip
//! The Wayland client backend loads `libwayland-client` at runtime (the `dlopen` feature). On a machine
//! without that library the connection cannot be constructed; the test then skips rather than fails, so CI
//! images lacking libwayland stay green. Where libwayland is present (any Wayland desktop, including the
//! developer box this was written on) the test runs for real.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayland_c::wayland_proxy::WaylandSink;
use rayland_relay::WaylandMessage;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch};

/// A sink that discards every forwarded request. This test only checks that globals are advertised, so it
/// does not care what the app forwards — the forwarding path itself is proven in the sibling forward test.
struct NullSink;
impl WaylandSink for NullSink {
    fn forward_request(&self, _msg: WaylandMessage) {}
}

/// The client-side dispatch state. It carries nothing: `registry_queue_init` performs the initial global
/// dump internally, and the test reads the resulting list — no runtime registry events are handled.
struct AppData;

// The registry Dispatch impl `registry_queue_init` requires. We react to no post-init registry events
// (globals appearing/vanishing mid-run), so the handler is intentionally empty.
impl Dispatch<WlRegistry, GlobalListContents> for AppData {
    /// Ignore all `wl_registry` events; the initial global set is captured by `registry_queue_init`.
    fn event(
        _state: &mut Self,
        _registry: &WlRegistry,
        _event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

/// Connect to the proxy, read its advertised globals, and assert the WP0 minimum set is present.
///
/// # Mechanism
/// 1. Spawn `wayland_proxy::run` on a detached thread bound to a unique socket path. It listens and serves
///    forever; the test lets the process reap it at exit.
/// 2. Connect a `UnixStream` to that socket, retrying briefly to absorb the listener-bind race.
/// 3. Hand the stream to a `wayland-client` `Connection` and run `registry_queue_init`, which round-trips
///    the registry and returns the global list.
/// 4. Assert each WP0 interface name appears.
#[test]
fn app_connects_and_sees_wp0_globals() {
    // A unique socket path for this test run. The pid keeps concurrent test binaries from colliding; the
    // system temp dir is writable and a socket inode is negligible in size.
    let socket_path: PathBuf =
        std::env::temp_dir().join(format!("rayland-wp-proxy-test-{}.sock", std::process::id()));

    // Stand the proxy up on its own thread. `run` blocks in its serve loop, so it must not be joined; the
    // test only needs it listening. A clone of the path goes to the thread; the test keeps the original.
    let proxy_path = socket_path.clone();
    std::thread::spawn(move || {
        // If the proxy errors, surface it on stderr; the test will then fail at connect/roundtrip below.
        if let Err(e) = rayland_c::wayland_proxy::run(proxy_path, Arc::new(NullSink)) {
            eprintln!("proxy exited with error: {e:#}");
        }
    });

    // Connect, retrying to absorb the race between this thread and the proxy thread's `bind`.
    let stream = connect_with_retry(&socket_path, Duration::from_secs(2))
        .expect("connect to the proxy socket within the timeout");

    // Build a client connection over that raw socket. This is where libwayland is dlopen'd; if it is
    // absent, skip rather than fail (see the module docs).
    let conn = match Connection::from_socket(stream) {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("skipping: cannot init wayland-client backend (libwayland absent?): {e}");
            return;
        }
    };

    // Round-trip the registry and capture the advertised globals.
    let (globals, _queue) =
        registry_queue_init::<AppData>(&conn).expect("registry round-trips against the proxy");
    let names: Vec<String> = globals
        .contents()
        .clone_list()
        .into_iter()
        .map(|g| g.interface)
        .collect();

    // The WP0 minimum set (spec §6): the app must be able to bind each of these.
    for expected in [
        "wl_compositor",
        "xdg_wm_base",
        "zwp_linux_dmabuf_v1",
        "wl_seat",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "proxy did not advertise `{expected}`; advertised globals were: {names:?}"
        );
    }
}

/// Connect to `path`, retrying every few milliseconds until `timeout` elapses.
///
/// The proxy binds its listener on another thread, so the first connect attempts may race ahead of the
/// `bind` and see `ENOENT`/`ECONNREFUSED`. Retrying briefly makes the test deterministic without a fixed
/// sleep. Returns the connected stream, or the last error if the timeout is reached.
fn connect_with_retry(path: &PathBuf, timeout: Duration) -> std::io::Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                // Out of time: report the last failure so the assertion message is informative.
                if Instant::now() >= deadline {
                    return Err(e);
                }
                // The proxy is probably mid-bind; back off briefly and retry.
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}
