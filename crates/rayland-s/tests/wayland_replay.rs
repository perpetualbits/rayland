//! Integration proof for WP0 Task 4.2b-ii: the S-side replay binds a global and replays a request against
//! **S's real compositor**, creating the corresponding objects.
//!
//! # What this proves
//! vkcube cannot exercise the request-replay path yet — it aborts at swapchain format selection before it
//! creates its `wl_surface`, because the compositor's dmabuf format events are not yet returned to it (the
//! event-return path, Task 4.4). So the request path is proven here directly: feed [`WaylandReplay`] a
//! synthetic `wl_compositor` bind and a synthetic `create_surface` request, and assert both objects end up
//! mapped — which only happens if the bind and the `send_request` both succeeded against the real
//! compositor (a protocol error would return `Err` and leave the object unmapped; a signature mismatch
//! would panic and be caught, also leaving it unmapped).
//!
//! # Why it may skip
//! The replay connects to a real compositor via `WAYLAND_DISPLAY`. Where none is reachable (headless CI),
//! the bind's `ensure_connected` fails, nothing maps, and the test skips rather than failing — exactly as
//! the sibling `rayland-present`/proxy tests do for libwayland/compositor absence.

use rayland_relay::{WaylandArg, WaylandMessage};
use rayland_s::wayland_client::WaylandReplay;

/// `wl_compositor.create_surface` request opcode (creates a `wl_surface`).
const OP_COMPOSITOR_CREATE_SURFACE: u16 = 0;

#[test]
fn a_bind_and_a_request_replay_against_the_real_compositor() {
    // Skip cleanly where no compositor is reachable: try to connect once via a throwaway bind and, if the
    // replay never mapped it, treat the environment as compositor-less rather than failing.
    let mut replay = WaylandReplay::new();

    // Replay the app binding wl_compositor v4 as its object 3. If a compositor is present this creates a
    // real wl_compositor on it and maps app-id 3; if not, ensure_connected logs and nothing maps.
    replay.handle_bind("wl_compositor".to_string(), 4, 3);

    if !replay.is_mapped(3) {
        eprintln!("skipping: no reachable compositor (WAYLAND_DISPLAY), so the replay never connected");
        return;
    }

    // Replay `wl_compositor.create_surface(new_id)` on that bound object: object 3, opcode 0, one NewId
    // naming the new wl_surface (app id 9). A successful send_request creates the surface on the real
    // compositor and maps app-id 9 to it.
    replay.handle_request(WaylandMessage {
        object_id: 3,
        opcode: OP_COMPOSITOR_CREATE_SURFACE,
        args: vec![WaylandArg::NewId {
            id: 9,
            interface: "wl_surface".to_string(),
            version: 4,
        }],
    });

    assert!(
        replay.is_mapped(9),
        "the replayed create_surface did not map the new wl_surface — send_request failed or panicked"
    );
}
