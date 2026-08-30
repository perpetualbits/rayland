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

use rayland_relay::{BufferToken, WaylandArg, WaylandMessage};
use std::os::fd::OwnedFd;

use rayland_s::wayland_client::{
    EventSink, ExportedFdSource, WaylandReplay, plan_buffer_requests,
};
use wayland_client::backend::protocol::Argument;

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

/// An [`ExportedFdSource`] that resolves nothing.
///
/// The compositor-facing tests below never drive a buffer token, and a fake that *did* hand out a
/// descriptor would be lying: only a real virglrenderer export produces a dma-buf a compositor will
/// import. Refusing everything is the honest stand-in, and it exercises the refusal path for free.
#[derive(Default)]
struct NoExports;
impl ExportedFdSource for NoExports {
    fn dup_exported_fd(&self, _resource_id: u32) -> Option<OwnedFd> {
        None
    }
    // No buffer is ever built in these tests, so nothing is ever presented.
    fn note_presented(&self, _resource_id: u32) {}
}

#[test]
fn a_bind_and_a_request_replay_against_the_real_compositor() {
    let mut replay = WaylandReplay::new(Arc::new(Recorder::default()), Arc::new(NoExports));

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
    let mut replay = WaylandReplay::new(recorder.clone(), Arc::new(NoExports));

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


/// The three synthesized requests have the right opcodes, order, and argument layout.
///
/// # Why this test is the real gate on Task 4.3's request construction
/// Everything else about the token path needs a compositor and a GPU. This does not: the planner is a pure
/// function, so the one thing that can be pinned down anywhere — *exactly what goes on the wire* — is
/// pinned down here.
///
/// The fixture is chosen so that plausible bugs **fail** rather than coincide:
/// - a **non-zero modifier with different halves**, so swapping hi and lo is caught (a symmetric value
///   like 0 or `u64::MAX` would pass either way);
/// - a **stride that is not `width × 4`**, so an implementation that derived it from the geometry fails
///   instead of accidentally agreeing;
/// - a **non-zero offset**, so one that assumed zero fails.
#[test]
fn the_synthesized_buffer_requests_have_the_right_shape() {
    // Deliberately awkward values — see the doc comment. width x 4 would be 1024, not 1280.
    const WIDTH: u32 = 256;
    const HEIGHT: u32 = 128;
    const STRIDE: u32 = 1280;
    const OFFSET: u32 = 8192;
    const FORMAT: u32 = 0x3432_5258; // DRM_FORMAT_XRGB8888
    const MOD_HI: u32 = 0x0100_0000;
    const MOD_LO: u32 = 0x0000_0002;
    const MODIFIER: u64 = ((MOD_HI as u64) << 32) | MOD_LO as u64;
    const DMABUF_VERSION: u32 = 3;
    // A descriptor number the planner only copies; it is never opened, so any value proves the placement.
    const FD: std::os::fd::RawFd = 42;

    let token = BufferToken {
        resource_id: 7,
        width: WIDTH,
        height: HEIGHT,
        drm_format: FORMAT,
        modifier: MODIFIER,
        stride: STRIDE,
        offset: OFFSET,
    };
    let [create_params, add, create_immed] = plan_buffer_requests(&token, DMABUF_VERSION, FD);

    // 1. create_params on the dmabuf global: one null new_id, and a params child at the bound version.
    assert_eq!(create_params.opcode, 1, "create_params is opcode 1 on zwp_linux_dmabuf_v1");
    assert!(
        matches!(create_params.args.as_slice(), [Argument::NewId(id)] if id.is_null()),
        "create_params takes exactly one null new_id; got {:?}",
        create_params.args
    );
    let (iface, version) = create_params.child.expect("create_params creates the params object");
    assert_eq!(iface.name, "zwp_linux_buffer_params_v1");
    assert_eq!(
        version, DMABUF_VERSION,
        "the params object must be created at the version the dmabuf global was bound at"
    );

    // 2. add: [fd, plane_idx, offset, stride, modifier_hi, modifier_lo] — the order the protocol specifies.
    assert_eq!(add.opcode, 1, "add is opcode 1 on zwp_linux_buffer_params_v1");
    assert!(add.child.is_none(), "add creates no object");
    match add.args.as_slice() {
        [
            Argument::Fd(fd),
            Argument::Uint(plane),
            Argument::Uint(offset),
            Argument::Uint(stride),
            Argument::Uint(hi),
            Argument::Uint(lo),
        ] => {
            assert_eq!(*fd, FD, "add must carry the descriptor it was given");
            assert_eq!(*plane, 0, "only plane 0 is ever synthesized — C refuses anything else");
            assert_eq!(*offset, OFFSET, "offset must come from the token, not be assumed zero");
            assert_eq!(
                *stride, STRIDE,
                "stride must come from the token; a derived width x bpp would be {}",
                WIDTH * 4
            );
            assert_eq!(*hi, MOD_HI, "modifier HIGH half comes first");
            assert_eq!(*lo, MOD_LO, "modifier LOW half comes second");
        }
        other => panic!("add has the wrong argument shape: {other:?}"),
    }

    // 3. create_immed: [new_id, width, height, format, flags] and a wl_buffer child.
    assert_eq!(create_immed.opcode, 3, "create_immed is opcode 3 on zwp_linux_buffer_params_v1");
    match create_immed.args.as_slice() {
        [
            Argument::NewId(id),
            Argument::Int(w),
            Argument::Int(h),
            Argument::Uint(fmt),
            Argument::Uint(flags),
        ] => {
            assert!(id.is_null(), "the new buffer id is null on the wire; child_spec creates it");
            assert_eq!(*w, WIDTH as i32);
            assert_eq!(*h, HEIGHT as i32);
            assert_eq!(*fmt, FORMAT);
            assert_eq!(*flags, 0, "WP0 sends no buffer flags");
        }
        other => panic!("create_immed has the wrong argument shape: {other:?}"),
    }
    let (iface, version) = create_immed.child.expect("create_immed creates the wl_buffer");
    assert_eq!(iface.name, "wl_buffer");
    // NOT 1, even though `wl_buffer` has only ever had version 1: a Wayland child inherits its parent's
    // version, and `wayland-backend` panics if child_spec disagrees with the sender's version. Declaring
    // v1 here against a v3 params object is what killed the first two-machine run of this code.
    assert_eq!(
        version, DMABUF_VERSION,
        "the wl_buffer inherits the params object's version, not the interface's own maximum"
    );
}

/// A child object created from a **capped** bind is replayed at the capped version, not the wire's.
///
/// # Why this is an integration test and not a unit test
/// The first attempt at this was a unit test that computed `child = sender_version` in the test body
/// and asserted it equalled the capped value. It passed, and it proved nothing: it restated the rule
/// rather than exercising the code, which is exactly the hazard `OVERVIEW.md` §6.4 records — *a test
/// written from the same belief as the code it tests can only confirm that belief*. It would not have
/// failed if `handle_request` had gone on using the wire version.
///
/// So this drives the real path: bind, create a surface, then create an `xdg_surface` from the bound
/// `xdg_wm_base`, and assert the replay survived and mapped the child.
///
/// # The scenario, which is `vkgears`' real one
/// The application binds `xdg_wm_base` at the descriptor's maximum (C advertises `u32::MAX`, capped to
/// the descriptor). S's compositor may offer less — headless weston offers v5 — so `handle_bind` caps.
/// Every object the app then creates from that global must be created at S's version. Using the wire's
/// makes `wayland-backend` panic, and that panic is **fatal**: it poisons the backend's own connection
/// mutex, so the replay cannot continue.
///
/// # It skips where it cannot be meaningful
/// Against a compositor that offers the same version the app asked for, nothing is capped and the test
/// would pass whatever the code did. It detects that and skips, rather than reporting a green that
/// means nothing. Run it under `WAYLAND_DISPLAY=<headless weston>` to exercise the real case.
#[test]
fn a_child_of_a_capped_bind_is_created_at_the_capped_version() {
    /// The version the application believes in — C advertises the descriptor's maximum.
    const APP_VERSION: u32 = 6;
    const COMPOSITOR_APP_ID: u32 = 4;
    const SURFACE_APP_ID: u32 = 3;
    const WM_BASE_APP_ID: u32 = 5;
    const XDG_SURFACE_APP_ID: u32 = 7;
    /// `wl_compositor.create_surface` — request 0, one new_id.
    const OP_CREATE_SURFACE: u16 = 0;
    /// `xdg_wm_base.get_xdg_surface` — request 2, `[new_id, surface]`.
    const OP_GET_XDG_SURFACE: u16 = 2;

    let mut replay = WaylandReplay::new(Arc::new(Recorder::default()), Arc::new(NoExports));

    replay.handle_bind("wl_compositor".into(), APP_VERSION, COMPOSITOR_APP_ID);
    let Some(compositor_version) = replay.version_of(COMPOSITOR_APP_ID) else {
        eprintln!("skipping: no compositor to replay against");
        return;
    };
    replay.handle_bind("xdg_wm_base".into(), APP_VERSION, WM_BASE_APP_ID);
    let Some(wm_base_version) = replay.version_of(WM_BASE_APP_ID) else {
        eprintln!("skipping: this compositor advertises no xdg_wm_base");
        return;
    };

    if wm_base_version == APP_VERSION && compositor_version == APP_VERSION {
        eprintln!(
            "skipping: this compositor offers v{APP_VERSION} for both globals, so nothing was capped \
             and this test cannot distinguish the sender's version from the wire's. Run it against a \
             headless weston (which offers xdg_wm_base v5) to exercise the real case."
        );
        return;
    }

    // A surface to hang the xdg_surface on. Its NewId carries the app's version on the wire, which is
    // precisely the value that must NOT reach child_spec.
    replay.handle_request(WaylandMessage {
        object_id: COMPOSITOR_APP_ID,
        opcode: OP_CREATE_SURFACE,
        args: vec![WaylandArg::NewId {
            id: SURFACE_APP_ID,
            interface: "wl_surface".into(),
            version: APP_VERSION,
        }],
    });
    // The request that used to kill the process: a child of the capped xdg_wm_base.
    replay.handle_request(WaylandMessage {
        object_id: WM_BASE_APP_ID,
        opcode: OP_GET_XDG_SURFACE,
        args: vec![
            WaylandArg::NewId {
                id: XDG_SURFACE_APP_ID,
                interface: "xdg_surface".into(),
                version: APP_VERSION,
            },
            WaylandArg::Object(SURFACE_APP_ID),
        ],
    });

    assert!(
        !replay.is_replay_dead(),
        "the replay died: send_request panicked, which means child_spec carried a version the sender \
         does not have (S capped xdg_wm_base to v{wm_base_version}, the wire said v{APP_VERSION})"
    );
    assert_eq!(
        replay.version_of(XDG_SURFACE_APP_ID),
        Some(wm_base_version),
        "the child must be recorded at its sender's version, so that ITS children inherit correctly"
    );
    assert!(
        replay.is_mapped(XDG_SURFACE_APP_ID),
        "the xdg_surface should have been created on S's compositor"
    );
}
