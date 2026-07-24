//! Integration proofs for the WP0 S-side replay against **S's real compositor**:
//! - Task 4.2b-ii: binding a global and replaying a request creates the corresponding objects.
//! - Task 4.4b: a compositor event is translated S→app and emitted back through the [`EventSink`].
//!
//! # Why these run against a real compositor
//! The replay's whole job is to drive a real compositor, and the failure modes (a protocol error, a
//! signature mismatch, an event on an object the replay never created) only surface there. vkcube cannot
//! exercise these paths directly yet — it needs the buffer-token path (4.3) to present — so they are proven
//! here with synthetic binds/requests and a recorder sink.
//!
//! # Why they may skip
//! The replay connects to a real compositor via `WAYLAND_DISPLAY`. Where none is reachable (headless CI),
//! `ensure_connected` fails, nothing maps, and the tests skip rather than failing — exactly as the sibling
//! `rayland-present`/proxy tests do for libwayland/compositor absence.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rayland_relay::{WaylandArg, WaylandMessage};
use rayland_s::wayland_client::{EventSink, WaylandReplay};

/// `wl_compositor.create_surface` request opcode (creates a `wl_surface`).
const OP_COMPOSITOR_CREATE_SURFACE: u16 = 0;

/// A recorder sink: keeps every compositor event the replay translated and emitted, so a test can inspect it.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<WaylandMessage>>,
}
impl EventSink for Recorder {
    fn emit(&self, event: WaylandMessage) {
        self.events.lock().unwrap().push(event);
    }
}

#[test]
fn a_bind_and_a_request_replay_against_the_real_compositor() {
    let mut replay = WaylandReplay::new(Arc::new(Recorder::default()));

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

/// The event-return path (Task 4.4b): binding `wl_seat` makes a real compositor emit `wl_seat.capabilities`
/// (and usually `name`); the compositor-reader thread must dispatch it, [`WaylandReplay`] must translate its
/// sender S→app, and the sink must receive it keyed by the **app's** object id.
#[test]
fn a_compositor_event_is_translated_and_emitted_to_the_app() {
    let recorder = Arc::new(Recorder::default());
    let mut replay = WaylandReplay::new(recorder.clone());

    // Bind wl_seat as the app's object 6. A real compositor announces the seat's capabilities right after
    // the bind, on the seat object — the event the return path must carry back.
    const SEAT_APP_ID: u32 = 6;
    replay.handle_bind("wl_seat".to_string(), 1, SEAT_APP_ID);

    if !replay.is_mapped(SEAT_APP_ID) {
        eprintln!("skipping: no reachable compositor with a wl_seat, so the replay never connected");
        return;
    }

    // The event arrives asynchronously on the compositor-reader thread; wait briefly for it. A real seat
    // always reports capabilities, so this is not flaky where a compositor is present.
    let deadline = Instant::now() + Duration::from_secs(3);
    let seat_event = loop {
        if let Some(ev) = recorder
            .events
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.object_id == SEAT_APP_ID)
            .cloned()
        {
            break Some(ev);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let seat_event = seat_event.expect(
        "no compositor event was emitted for the bound wl_seat (app id 6) within the deadline — the \
         event-return path did not translate/emit it",
    );
    // wl_seat events are `capabilities` (opcode 0, one uint) or `name` (opcode 1, one string). Either
    // proves the path; assert the shape is one of them rather than pinning a compositor-specific value.
    assert!(
        (seat_event.opcode == 0 && matches!(seat_event.args.first(), Some(WaylandArg::Uint(_))))
            || (seat_event.opcode == 1
                && matches!(seat_event.args.first(), Some(WaylandArg::Str(_)))),
        "the emitted seat event was not a well-formed capabilities/name event: {seat_event:?}"
    );
}
