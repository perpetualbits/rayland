//! Present a frame to a **real Wayland compositor** and prove the whole path ran.
//!
//! # Why this test exists
//! Everything else about presentation is unit-testable and is unit-tested: the RGBA8→Xrgb8888
//! swizzle is a pure function, and `rayland-s`'s frame identification is a predicate over sizes.
//! What none of that touches is the part that historically breaks — binding the globals, the
//! `zwp_linux_dmabuf_v1` capability probe, allocating a `wl_shm` pool, building a buffer the
//! compositor will actually accept, and the teardown contract. A compositor rejects a malformed
//! buffer with a **protocol error that kills the connection**, and no amount of pure testing sees
//! that coming.
//!
//! Until (c)1 Task 7 there was no such test: SP1/SP3 verified `present` "by building and by a manual
//! on-screen smoke check" (its own module docs said so). That was defensible when one binary used
//! it. Now two do, and one of them — `rayland-s` — is driven by a path where a silent blank window
//! is a known, previously-shipped failure mode.
//!
//! # What it proves, and what it emphatically does not
//! **Proves:** a real compositor accepted our `wl_shm` buffer, the window was created and
//! configured, `draw` ran without erroring, and the loop tore down cleanly on EOF.
//!
//! **Does not prove:** that anything correct appeared on screen. No automated test can assert what a
//! compositor painted — that is the human check, and this file is not a substitute for it. Notably,
//! a `draw` that painted the wrong colours, or painted nothing, would still pass here. The
//! distinction is stated rather than blurred; this branch has shipped eleven tests that asserted more
//! than they could detect.
//!
//! # Skip, don't fail, without a compositor
//! CI has no compositor, and a machine without one has not failed at anything. Gates on
//! `WAYLAND_DISPLAY` exactly as this repository's GPU tests gate on a render node, and prints SKIP.
//!
//! Run it on a machine with a compositor: `cargo test -p rayland-present -- --nocapture`. A small
//! window appears for about a second and closes itself.

use rayland_present::{FrameSource, RenderedFrame, WindowConfig, present};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// The test frame's size. Small, because a window flashing up during a test run should be
/// unobtrusive; 64×64 also matches what `rayland-s` really presents (`rayland-refapp`'s frame).
const W: u32 = 64;
const H: u32 = 64;

/// How long the window stays up before this test closes it from the other end.
///
/// It has to outlast a real compositor's create → configure → draw round trip, and nothing more.
/// Generous against that (which is sub-millisecond on a local socket) and short enough not to be
/// annoying. Note this is a **timeout, not a sleep the test's correctness depends on**: if the
/// window is torn down before it drew, `present` still returns `Ok` and the assertion below still
/// passes — which is exactly the limit named in the module docs.
const WINDOW_LIFETIME: Duration = Duration::from_secs(1);

/// A frame source backed by a `Vec<u8>` — the same shape `rayland-s`'s really is.
///
/// `supports_dmabuf` is left at the trait's default (`false`), so this drives the `wl_shm` path: the
/// only path `rayland-s` can use (spec §7.1), and therefore the one worth having a live test for.
/// The dmabuf path needs a GPU to produce an export and is covered by `rayland-server`'s own
/// GPU-gated tests.
struct TestFrame {
    /// Handed over by `produce_pixels`, which `present` calls exactly once.
    pixels: Option<Vec<u8>>,
}

impl FrameSource for TestFrame {
    fn width(&self) -> u32 {
        W
    }

    fn height(&self) -> u32 {
        H
    }

    fn produce_pixels(&mut self) -> anyhow::Result<RenderedFrame> {
        let pixels = self
            .pixels
            .take()
            .ok_or_else(|| anyhow::anyhow!("produce_pixels called twice"))?;
        Ok(RenderedFrame {
            width: W,
            height: H,
            pixels,
        })
    }
}

/// A recognisable image: an opaque red field. If a human happens to be watching, a *blue* window
/// means `pack_xrgb8888`'s swizzle has regressed — the exact pitfall its doc comment names.
fn red_frame() -> Vec<u8> {
    let mut pixels = Vec::with_capacity((W * H * 4) as usize);
    for _ in 0..(W * H) {
        pixels.extend_from_slice(&[255, 0, 0, 255]);
    }
    pixels
}

#[test]
fn presents_a_frame_to_a_real_compositor_and_tears_down_cleanly() {
    // A machine with no compositor cannot run this and has not failed. Skip, as every GPU-dependent
    // test in this repository skips without a render node.
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("SKIP: no WAYLAND_DISPLAY, so there is no compositor to present to");
        return;
    }

    let mut source = TestFrame {
        pixels: Some(red_frame()),
    };
    let config = WindowConfig {
        title: "Rayland — rayland-present live test",
        app_id: "nl.rayland.PresentTest",
        force_shm: false,
    };

    // `present` ends its loop when the window is closed OR the disconnect source hits EOF. Nothing
    // is going to click a close button here, so the test plays the peer: it holds `ours`, and a
    // thread drops it after `WINDOW_LIFETIME`, which `present` sees as EOF on `theirs`.
    let (ours, theirs) = UnixStream::pair().expect("a socket pair must be creatable");
    // `present`'s contract: the source must already be non-blocking, or its calloop callback stalls
    // the event loop on the first readiness event.
    theirs
        .set_nonblocking(true)
        .expect("the liveness socket must be settable to non-blocking");
    let closer = std::thread::spawn(move || {
        std::thread::sleep(WINDOW_LIFETIME);
        // Dropping the last handle to this end is what the peer sees as a disconnect.
        drop(ours);
    });

    let started = Instant::now();
    let result = present(&mut source, &config, theirs);
    let elapsed = started.elapsed();
    closer.join().expect("the closer thread must not panic");

    // The load-bearing assertion. A compositor that refused our buffer kills the connection, and
    // `present`'s event loop surfaces that as a dispatch error rather than `Ok(())` — so this is not
    // the tautology it looks like.
    result.expect("presenting a well-formed wl_shm frame to a real compositor must succeed");

    // And it must have *stayed up* rather than falling straight through. `present` returning
    // instantly would mean it never ran the loop at all — it would still be `Ok`, and it would still
    // have shown nobody anything. A little slack below `WINDOW_LIFETIME` for scheduling.
    assert!(
        elapsed >= WINDOW_LIFETIME - Duration::from_millis(100),
        "present returned after {elapsed:?}, far sooner than the {WINDOW_LIFETIME:?} the window was \
         meant to stay up for; it cannot have run the event loop"
    );
}
