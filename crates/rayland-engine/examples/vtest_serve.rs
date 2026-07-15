//! A minimal live-drive harness: bind a Unix socket, accept **one** connection, and serve the vtest
//! protocol on it against a real `VirglEngine` on this host's GPU.
//!
//! # What this is for
//! Task 4a's whole point is that no *live* Vulkan client had ever driven this engine. Everything
//! before it was proven against synthetic bytes and mock engines, which cannot reveal the one thing
//! that mattered — that the protocol requires passing real file descriptors, and a real Mesa Venus
//! ICD blocks forever without them. This harness is what puts a real client on the other end.
//!
//! It is deliberately *minimal*: bind, accept once, serve, report, exit. This is C0's live-drive
//! instrument, not a placeholder for something else that will be built later: the reference app
//! (`rayland-refapp`) produces the PNG itself, exactly as an ordinary offscreen Vulkan program
//! would — see Amendment 1 of the C0 design spec
//! (`docs/design/2026-07-14-c0-venus-first-light.md`) and
//! `docs/c0-venus-first-light.md`'s "Who writes the PNG" section for why. There is no separate
//! `rayland-engine-host` binary to build: the app already produces the picture, so a host-side
//! equivalent would only duplicate it. Host-side frame *extraction* — reading a rendered image back
//! out of the engine without the app's cooperation — remains an open question, deferred by the
//! owner as Task 4c; it is not this task's job either.
//!
//! # Running it against a real client
//! ```text
//! # terminal 1 — the server (this harness)
//! cargo run -p rayland-engine --example vtest_serve -- /tmp/rayland-vtest.sock
//!
//! # terminal 2 — any unmodified Vulkan program, via Mesa's Venus ICD
//! env -u VK_LOADER_DRIVERS_SELECT \
//!     VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json \
//!     VN_DEBUG=vtest \
//!     VTEST_SOCKET_NAME=/tmp/rayland-vtest.sock \
//!     ./some-vulkan-app
//! ```
//!
//! Three environment/setup pitfalls, all of which cost real debugging time:
//! - **`env -u VK_LOADER_DRIVERS_SELECT`** — if the host has that set to a driver filter (e.g.
//!   `*intel*`), the Vulkan loader silently hides the Venus ICD and the client never connects at
//!   all. The failure looks like "no Vulkan devices", not like a Rayland problem.
//! - **`VN_DEBUG=vtest` is required, and its absence fails silently.** Mesa's Venus ICD prefers its
//!   **virtgpu** backend and only tries the vtest backend when told to via `VN_DEBUG=vtest`.
//!   Setting `VTEST_SOCKET_NAME` alone is *not* sufficient to select vtest: that variable is read
//!   only inside `vtest_init`, which runs **after** the ICD has already chosen a backend. So the
//!   socket name cannot cause vtest to be selected — it only configures vtest once vtest has
//!   already been chosen. Without `VN_DEBUG=vtest` the client fails at
//!   `STEP enumerate FAILED: ERROR_INITIALIZATION_FAILED` and **never connects to the socket at
//!   all** — this harness logs no incoming connection, which makes the failure look like a
//!   Rayland-side hang rather than the client never having tried to reach us.
//! - **No validation layer.** Validation and Venus do not mix; enabling one produces failures that
//!   have nothing to do with the code under test.
//!
//! A further, more mundane pitfall: **the Unix socket path must be short.** `sockaddr_un::sun_path`
//! is 108 bytes; a long path (e.g. one nested under a scratchpad or temp-session directory) overflows
//! it, and `UnixListener::bind` fails with *"path must be shorter than SUN_LEN"*. Unlike the
//! `VN_DEBUG` trap above, at least this one fails loudly and immediately at `bind`, rather than
//! silently at the client. Live drives of this harness used the short path `/tmp/rl4a.sock`.
//!
//! Set `RAYLAND_VTEST_DUMP=1` on *this* process to dump every `VCMD_SUBMIT_CMD2` payload as hex —
//! see `vtest::dump_submit_cmd2`.

// The engine, the protocol server, and the availability probe this harness needs.
use rayland_engine::{VirglEngine, virgl_available, vtest::serve_vtest};
// Binding and accepting the client connection. `UnixStream` is the transport that can pass
// descriptors, which is why the protocol is served over it and not over TCP.
use std::os::unix::net::UnixListener;
use std::path::Path;

/// The DRM render node the engine renders on. Hardcoded rather than made an option: C0 targets this
/// host's real GPU, and a wrong node should fail loudly at startup rather than be configurable.
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// Binds the socket named by `argv[1]`, serves one vtest session on it, and reports the outcome.
///
/// # Exit status
/// 0 if the session ended cleanly (the client disconnected at a message boundary); 1 on a usage
/// error, an unavailable GPU, or any protocol/engine failure. The failure is printed in full —
/// this harness exists to diagnose, so a silent or summarized error would defeat it.
fn main() {
    // The socket path the client will be pointed at via `VTEST_SOCKET_NAME`.
    let Some(socket_path) = std::env::args().nth(1) else {
        eprintln!("usage: vtest_serve <socket-path>");
        eprintln!(
            "  then run a Vulkan app with VTEST_SOCKET_NAME=<socket-path> and Mesa's Venus ICD"
        );
        std::process::exit(1);
    };

    // Fail early and specifically if this host cannot serve Venus at all, rather than surfacing it
    // later as an opaque context-creation error once a client is already connected.
    if !virgl_available(Path::new(RENDER_NODE)) {
        eprintln!("no usable Venus render node at {RENDER_NODE}: cannot serve a live client here");
        std::process::exit(1);
    }

    // A stale socket file from a previous run would make `bind` fail with EADDRINUSE even though
    // nothing is listening — Unix sockets leave their filesystem entry behind. Remove it; a
    // missing file is the normal case and not an error.
    let _ = std::fs::remove_file(&socket_path);

    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("failed to bind {socket_path}: {err}");
            std::process::exit(1);
        }
    };
    eprintln!("listening on {socket_path} (one connection, then exit)");

    // Bring the engine up *before* accepting: `virgl_renderer_init` forks the render server and
    // initializes EGL, which is slow enough that doing it while a client waits on the handshake
    // invites confusing timeouts.
    let mut engine = match VirglEngine::new(Path::new(RENDER_NODE)) {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("failed to initialize the render engine: {err}");
            std::process::exit(1);
        }
    };
    eprintln!("engine up on {RENDER_NODE}");

    // Exactly one client, as advertised. vtest is one context per connection and `serve_vtest` maps
    // that connection to a single Venus context, so serving concurrent clients is a design question
    // (context id allocation, engine sharing) rather than a loop — deliberately not answered here.
    let mut stream = match listener.accept() {
        Ok((stream, _addr)) => stream,
        Err(err) => {
            eprintln!("failed to accept a connection: {err}");
            std::process::exit(1);
        }
    };
    eprintln!("client connected; serving vtest");

    // Serve until the client disconnects. This is the call that, for the first time in C0, has a
    // real Mesa Venus ICD on the other end of it.
    match serve_vtest(&mut stream, &mut engine) {
        Ok(outcome) => {
            eprintln!("session ended cleanly: {outcome:?}");
        }
        Err(err) => {
            // Print the whole error, including the source chain: in a live drive the *specific*
            // failure is the entire deliverable.
            eprintln!("session failed: {err}");
            let mut source = std::error::Error::source(&err);
            while let Some(err) = source {
                eprintln!("  caused by: {err}");
                source = err.source();
            }
            // Clean up the socket file even on the failure path, so a rerun does not trip over it.
            let _ = std::fs::remove_file(&socket_path);
            std::process::exit(1);
        }
    }

    // Remove the socket file so the next run starts clean.
    let _ = std::fs::remove_file(&socket_path);
}
