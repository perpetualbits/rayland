//! WP0 — **the S-side Wayland session replay.**
//!
//! The application on C presents through the C-side proxy (`rayland-c`'s `wayland_proxy`), which forwards
//! every Wayland request across the link as a [`rayland_relay::C2S::WaylandRequest`]. This module is the
//! other end: it replays that session against **S's real compositor**, so the app's window actually appears
//! on S's screen. It is the mirror of the C proxy — the proxy is a Wayland *server* to the app; this is a
//! Wayland *client* to S's compositor.
//!
//! # Why the low-level backend, symmetric to the proxy
//! Like the proxy, the replay works at the structured-message layer (`wayland_client::backend`), not the
//! high-level typed API: it forwards *whatever* the app did, so it reconstructs each relayed
//! [`rayland_relay::WaylandMessage`] as a `wayland-backend` `Message` and submits it with
//! `Backend::send_request`, translating object ids across an `app_id ↔ s_id` map. The one thing that must
//! be interpreted rather than forwarded verbatim is the swapchain buffer: a [`rayland_relay::WaylandArg::Buffer`]
//! token is resolved to S's own dma-buf and wrapped as a real `wl_buffer` (WP0 Task 4.3).
//!
//! **Status: skeleton (WP0 Task 4.2a).** This establishes the router's destination: a live connection to
//! S's compositor, opened lazily on the first relayed request, and a log of every request that arrives —
//! proving the forward path reaches S and S can reach its compositor. Reconstructing and submitting the
//! requests (the `app_id ↔ s_id` map, `send_request`, the surface/toplevel, the token→`wl_buffer`
//! resolution, and the event return path) are the following sub-steps.

use rayland_relay::WaylandMessage;
use wayland_client::Connection;

/// The S-side replay of one application's Wayland session.
///
/// Holds the connection to S's compositor, opened lazily: an offscreen session (a fixture, the whole test
/// suite) sends no `WaylandRequest`, so this never connects and never needs a compositor to exist. Only a
/// real presenting app, proxied from C, drives it — and that only happens on S, which by definition has a
/// compositor.
pub struct WaylandReplay {
    /// The connection to S's real compositor, `None` until the first relayed request opens it. Kept for
    /// the whole session so the surface and its buffers live as long as the app's window.
    connection: Option<Connection>,
}

impl WaylandReplay {
    /// Create an unconnected replay. No compositor connection is made until [`Self::handle_request`] is
    /// first called, so constructing one is free and side-effect-free.
    pub fn new() -> Self {
        WaylandReplay { connection: None }
    }

    /// Handle one relayed Wayland request from the application.
    ///
    /// On the first call, this opens the connection to S's compositor (via `WAYLAND_DISPLAY`), so the
    /// connection's lifetime begins with the app's first Wayland activity rather than at daemon startup.
    /// A connection failure is logged and the request dropped — the daemon's vtest/ring session is
    /// independent and must not die because a compositor is momentarily unreachable.
    ///
    /// **Skeleton (Task 4.2a):** the request is logged, not yet submitted to the compositor. Reconstructing
    /// it as a `wayland-backend` `Message` and submitting it via `send_request`, with the `app_id ↔ s_id`
    /// translation, is Task 4.2b.
    pub fn handle_request(&mut self, msg: WaylandMessage) {
        // Open the compositor connection on first use, so offscreen sessions never touch a compositor.
        if self.connection.is_none() {
            match Connection::connect_to_env() {
                Ok(conn) => {
                    eprintln!("rayland-s: WP0 replay connected to S's compositor");
                    self.connection = Some(conn);
                }
                Err(e) => {
                    // Not fatal to the session: log and continue. The request cannot be replayed without
                    // a compositor, but the vtest/ring relay is unaffected.
                    eprintln!("rayland-s: WP0 replay could not reach a compositor: {e}");
                }
            }
        }
        // Record the request so the forward path is visible end to end. Actual replay is Task 4.2b.
        eprintln!(
            "rayland-s: WP0 replay request obj {} opcode {} ({} args)",
            msg.object_id,
            msg.opcode,
            msg.args.len()
        );
    }
}

impl Default for WaylandReplay {
    /// Same as [`WaylandReplay::new`]; provided because the type has an obvious empty state.
    fn default() -> Self {
        Self::new()
    }
}
