//! **Fixture B's end-to-end proof: the same unmodified binary, rendered through the remoting
//! path, must produce bit-identical PNGs to its native run — for all 120 frames.**
//!
//! # What this test actually demonstrates (or fails to)
//! It launches `rayland-icosa-gpu` — the GPU fixture, fixture A's "volume control": the exact same
//! spinning icosahedron, geometry, animation schedule, and render loop as `rayland-icosa-cpu`, but
//! the fractal is evaluated **in a fragment shader** instead of being computed on the CPU and
//! uploaded as a texture. It still writes its per-frame uniforms (the MVP matrix and the fractal's
//! view parameters) through a persistently-mapped `HOST_COHERENT` buffer with no flush and no
//! interceptable Vulkan call — the same mechanism fixture A uses for its texture, just 80 bytes of
//! it per frame instead of roughly a megabyte. See that fixture's own module doc, "This fixture does
//! NOT avoid mapped memory", for why that distinction matters: this pair isolates how much crosses
//! mapped memory, not whether anything does.
//!
//! This test launches the fixture twice, exactly as `icosa_cpu_venus_e2e.rs` does for fixture A:
//!
//! 1. **Natively**, on the host's own Vulkan driver, with nothing of Rayland involved.
//! 2. **Through Rayland**, by pointing the Vulkan loader at Mesa's Venus ICD and pointing Venus's
//!    vtest backend at a socket this test is serving with a real [`VirglEngine`].
//!
//! Then it compares every one of the 120 PNG pairs, byte for byte.
//!
//! # Why bit-identical is a fair demand and not a harsh one
//! Both runs draw on the same GPU with the same driver — the native baseline via the local ICD, the
//! remoted run through the engine — so it is the same fragment shader doing the same arithmetic on
//! the same hardware in both legs of this comparison. That argument has nothing to do with fixture
//! A: this test never compares fixture B's pixels against fixture A's, only against fixture B's own
//! native run (the two fixtures are never bit-compared to each other — `f32` shader arithmetic
//! against `f64` CPU arithmetic, plus the CPU path's extra bilinear resample through its 512×512
//! texture, guarantee they would not match even natively). The **only** thing that differs between
//! the two runs compared here, for this fixture, is how the 80-byte per-frame uniform write and the
//! draw commands reached the GPU. Any pixel difference is therefore a defect in that path.
//!
//! # Why every frame is compared, not just the last
//! A defect that corrupts one intermediate frame and then self-corrects — a stale uniform, a delta
//! applied out of order — is invisible to a final-frame check, and is exactly the kind of thing a
//! relay or a coherence layer can produce. Comparing all 120 costs nothing worth saving.
//!
//! # A tolerance would be worse than useless here
//! The bugs this path produces are usually *small* before they are large: a dropped mapped write, a
//! stale uniform, a delta applied out of order. A tolerance is precisely where those would live, so
//! this test does not have one — the comparison is `assert_eq!` on raw bytes.
//!
//! # Why this fixture's failure (if any) means something different from fixture A's
//! Fixture A's mapped write is a whole texture — if it fails, mapped-memory *volume* is a live
//! suspect. Fixture B has **no texture upload at all**: its mapped write is 80 bytes of uniforms,
//! the same order of magnitude as data that already crosses other Vulkan paths successfully (e.g.
//! `refapp_venus_e2e.rs`'s tiny geometry). So if this fixture *also* fails, the coherence question is
//! not primarily about volume — the depth attachment or the geometry pipeline (both new relative to
//! `rayland-refapp`'s single untextured triangle) become the more likely suspects, and if this
//! fixture fails *differently* from fixture A (a different frame, a different symptom), that
//! difference is itself the finding. See `docs/c0-venus-first-light.md`.
//!
//! # This test was expected to fail. It passes, and that is a finding.
//! This pair of fixtures exists to make (c)2 ("mapped-memory coherence") executable, and (c)2 has
//! not been built — so the design spec predicted this test would fail. **It does not: all 120
//! frames are bit-identical.**
//!
//! The prediction conflated "through Venus" with "across a network". This path is C0's: one
//! machine, a local Unix socket, and a Venus ICD that hands the ring and blobs to the engine as
//! **memfds passed over `SCM_RIGHTS`**. Nothing is transported, so nothing can be lost — mapped
//! writes work perfectly here not because Rayland solved anything but because on one machine there
//! is nothing to solve.
//!
//! For **this** fixture the point is sharper still: it barely writes mapped memory at all (80 bytes
//! of uniforms per frame against fixture A's megabyte), so it was never the one (c)2 threatened.
//! Its job is to be the volume control — and its passing says the shared scaffolding, the geometry,
//! the depth attachment and the fragment path all survive the C0 round trip intact. That is the
//! baseline against which fixture A's behaviour across the real relay must be read.
//!
//! Still do not loosen these assertions. If this test ever *starts* failing, something on the C0
//! path regressed, and the bit-exact comparison is what will say so. See `docs/icosa-fixtures.md`
//! for the full findings and `docs/design/2026-07-16-icosa-fixtures.md` §9 for the corrected
//! expectation.
//!
//! # The one place this test crosses the fixtures' isolation rule, and why it does not violate it
//! `rayland-engine` depends on the fixture binary (via [`build_icosa_gpu`], which shells out to
//! `cargo build -p rayland-icosa-gpu`), never the other way round. `rayland-icosa-gpu` itself still
//! depends on no `rayland-*` crate beyond the two icosa scaffolding crates, and contains no mention
//! of Venus, vtest, or remoting. The arrow's direction is the whole distinction: the fixture stays a
//! valid, ignorant witness; only this test, standing outside it, knows both halves exist.
//!
//! # Deliberately not shared with `icosa_cpu_venus_e2e.rs`
//! This file repeats rather than reuses that one's helper functions. The two tests are expected to
//! diverge as their findings do — a shared helper would couple them at exactly the point where their
//! results are supposed to be independently informative.
//!
//! Run on a GPU host with: `cargo test -p rayland-engine --test icosa_gpu_venus_e2e -- --nocapture`.

// The engine and the vtest protocol server this test puts behind the socket — the exact mechanism
// `refapp_venus_e2e.rs` already established and this test reuses without modification.
use rayland_engine::{VirglEngine, virgl_available, vtest::serve_vtest};
// Decoding the two PNGs for a pixel-level diagnostic once a byte mismatch is already known — the
// byte comparison itself needs no decoding at all.
use image::GenericImageView;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The DRM render node this crate's GPU tests use.
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// Mesa's Venus ICD manifest — see `refapp_venus_e2e.rs`'s identical constant for why naming it
/// explicitly via `VK_ICD_FILENAMES` is what selects Venus over the host's native driver.
const VENUS_ICD: &str = "/usr/share/vulkan/icd.d/virtio_icd.json";

/// The number of frames the fixture renders, mirrored from `rayland_icosa_core::FRAME_COUNT` rather
/// than imported from it: this crate takes no dependency on the icosa crates (see this module's doc,
/// "The one place this test crosses the fixtures' isolation rule"), and the two fixtures' own
/// `tests/native_render.rs` already hardcode the same value for the identical reason.
const FRAME_COUNT: u32 = 120;

/// How long to wait for the fixture to connect to the vtest socket before declaring the run failed.
/// See `refapp_venus_e2e.rs`'s identical constant for the full rationale.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How often to re-check for an incoming connection while waiting out [`CONNECT_TIMEOUT`].
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long the *whole* 120-frame Venus render may run before this test gives up and kills it.
///
/// Fixture B takes roughly 1.5ms/frame natively (~180ms for all 120 — there is no CPU-side fractal
/// iteration to dominate the frame time, unlike fixture A). Through Venus, per-command overhead
/// applies to every frame's draw the same way it would for fixture A, so this is set generously
/// rather than scaled down to match the native speed-up: five minutes is still an enormous multiple
/// of any plausible per-frame overhead, and exists so a real stall (a fence that never signals, a
/// deadlock in the replay path) is distinguishable from "still working". If this fires, the failure
/// is reported as a **timeout**, not folded into the pixel-comparison failure path — those are
/// different findings.
const RUN_TIMEOUT: Duration = Duration::from_secs(300);

/// How often the watchdog thread checks whether the render has finished.
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Build `rayland-icosa-gpu` and return the path to its executable.
///
/// # Why this shells out to Cargo instead of using `CARGO_BIN_EXE_*`
/// Identical reasoning to `refapp_venus_e2e.rs`'s `build_refapp` and this crate's own
/// `icosa_cpu_venus_e2e.rs::build_icosa_cpu`: Cargo only defines `CARGO_BIN_EXE_<name>` for binaries
/// belonging to the *same package* as the test, and adding `rayland-icosa-gpu` as a `dev-dependency`
/// of `rayland-engine` would not change that — `CARGO_BIN_EXE_*` is defined only for a package's own
/// binary targets. So this test asks Cargo for the binary directly. `cargo build` is a no-op when
/// the binary is already current, which it is under `cargo test --workspace`.
///
/// # Panics
/// Panics if Cargo cannot be run, if the build fails, or if the expected executable is missing
/// afterwards — all environment faults that would make the test meaningless rather than a result
/// worth reporting.
fn build_icosa_gpu() -> PathBuf {
    // `CARGO` is set by Cargo when it runs a test, and points at the very same Cargo driving this
    // run — safer than assuming a `cargo` on `PATH` is the same toolchain.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "-p", "rayland-icosa-gpu"])
        .status()
        .expect("cargo must be runnable to build the GPU fixture");
    assert!(status.success(), "building rayland-icosa-gpu must succeed");

    // The test binary lives at `<target>/<profile>/deps/<name>-<hash>`, so the profile directory
    // that holds the workspace's binaries is two levels up — see `build_refapp`'s identical
    // derivation for why this is preferred over hardcoding `target/debug`.
    let test_exe = std::env::current_exe().expect("the test binary must have a path");
    let profile_dir = test_exe
        .parent()
        .and_then(Path::parent)
        .expect("the test binary must live in <target>/<profile>/deps/");
    let fixture = profile_dir.join("rayland-icosa-gpu");
    assert!(
        fixture.is_file(),
        "cargo built rayland-icosa-gpu but no executable is at {}",
        fixture.display()
    );
    fixture
}

/// Run the fixture on the host's **own** Vulkan driver, into `output_dir` (which must already
/// exist — the fixture exits 1 with `os error 2` otherwise), and return its stdout (the CSV timing
/// report).
///
/// The three Venus variables are explicitly *removed* rather than merely left unset, for the same
/// reason `refapp_venus_e2e.rs::render_natively` removes them: this test process may itself have
/// been launched from a shell that exported them, and inheriting one here would silently turn the
/// "native" baseline into a second Venus run — comparing Venus against itself, passing, and proving
/// nothing.
///
/// # Panics
/// Panics if the fixture cannot be launched or exits unsuccessfully.
fn render_natively(fixture: &Path, output_dir: &Path) -> String {
    let output = Command::new(fixture)
        .arg(output_dir)
        .env_remove("VK_ICD_FILENAMES")
        .env_remove("VN_DEBUG")
        .env_remove("VTEST_SOCKET_NAME")
        .output()
        .expect("the fixture must be launchable natively");
    assert!(
        output.status.success(),
        "the fixture must render natively on a GPU host; got {}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("the timing report must be valid UTF-8")
}

/// Decode the two PNGs at `native_path`/`venus_path` and describe the first pixel where they
/// differ, for a diagnostic message once [`std::fs::read`] has already proven the raw files are not
/// byte-identical.
///
/// This is purely a reporting aid — the pass/fail decision is made on raw file bytes, never on this
/// function's output — because a PNG encoder is free to be non-deterministic in ways that leave the
/// decoded picture unchanged, and a test that decided pass/fail on decoded pixels would silently
/// paper over that case rather than surface it. Decoding is only how a real pixel difference gets
/// described in terms a human can act on: a byte offset in a *compressed* PNG stream means nothing
/// on its own.
///
/// # Panics
/// Never panics; decode failures are folded into the returned string instead, so a corrupt PNG on
/// either side is reported rather than losing the whole test to an unrelated panic mid-diagnostic.
fn describe_first_pixel_difference(native_path: &Path, venus_path: &Path) -> String {
    let native = match image::open(native_path) {
        Ok(image) => image,
        Err(error) => return format!("native PNG undecodable: {error}"),
    };
    let venus = match image::open(venus_path) {
        Ok(image) => image,
        Err(error) => return format!("venus PNG undecodable: {error}"),
    };
    if native.dimensions() != venus.dimensions() {
        return format!(
            "dimensions differ: native {:?} vs venus {:?}",
            native.dimensions(),
            venus.dimensions()
        );
    }
    let native = native.to_rgba8();
    let venus = venus.to_rgba8();
    let (width, height) = native.dimensions();
    let mut differing_pixels = 0usize;
    let mut first: Option<(u32, u32, image::Rgba<u8>, image::Rgba<u8>)> = None;
    for y in 0..height {
        for x in 0..width {
            let native_pixel = *native.get_pixel(x, y);
            let venus_pixel = *venus.get_pixel(x, y);
            if native_pixel != venus_pixel {
                differing_pixels += 1;
                if first.is_none() {
                    first = Some((x, y, native_pixel, venus_pixel));
                }
            }
        }
    }
    match first {
        Some((x, y, native_pixel, venus_pixel)) => format!(
            "{differing_pixels} of {} pixels differ; first divergence at ({x},{y}): native \
             {native_pixel:?} vs venus {venus_pixel:?}",
            width * height
        ),
        // The raw files differ (that is why this function was called) but every decoded pixel
        // matches: an encoder-level difference, not a rendering one. Worth reporting distinctly —
        // it would be a false alarm for the coherence question this test exists to answer.
        None => "raw file bytes differ but every decoded pixel is identical — an encoder-level \
                  difference (e.g. compression or metadata), not a rendering one"
            .to_string(),
    }
}

/// Compare every one of [`FRAME_COUNT`]'s frames between `native_dir` and `venus_dir`, byte for
/// byte, stopping at and reporting only the **first** mismatch.
///
/// # Why stop at the first mismatch instead of collecting all of them
/// The first divergence is the diagnostic. Once frame N differs, the render loop has already gone
/// wrong by frame N — whatever happens in frames N+1..120 is downstream noise from the same root
/// cause, not 120 independent findings. Dumping all of them would bury the one fact that matters.
///
/// # Panics
/// Panics on the first frame whose files differ (with a pixel-level diagnostic from
/// [`describe_first_pixel_difference`]), or if either file is missing or unreadable.
fn assert_all_frames_bit_identical(native_dir: &Path, venus_dir: &Path) {
    for frame in 0..FRAME_COUNT {
        let name = format!("frame_{frame:04}.png");
        let native_path = native_dir.join(&name);
        let venus_path = venus_dir.join(&name);
        let native_bytes = std::fs::read(&native_path).unwrap_or_else(|e| {
            panic!("frame {frame}: native PNG unreadable at {native_path:?}: {e}")
        });
        let venus_bytes = std::fs::read(&venus_path).unwrap_or_else(|e| {
            panic!("frame {frame}: venus PNG unreadable at {venus_path:?}: {e}")
        });
        if native_bytes != venus_bytes {
            let diagnosis = describe_first_pixel_difference(&native_path, &venus_path);
            panic!(
                "frame {frame} is the FIRST diverging frame (of {FRAME_COUNT}): native is {} \
                 bytes, venus is {} bytes. {diagnosis}",
                native_bytes.len(),
                venus_bytes.len()
            );
        }
    }
    eprintln!(
        "OK: all {FRAME_COUNT} frames are BIT-IDENTICAL between the native and Venus-rendered runs"
    );
}

/// The headline test: fixture B, run natively and through Rayland, must produce bit-identical PNGs
/// for all 120 frames.
///
/// # Structure
/// Follows `refapp_venus_e2e.rs`'s and `icosa_cpu_venus_e2e.rs`'s identical pattern: this thread
/// plays the server (binds the socket, brings the engine up, launches the fixture as a child
/// process, accepts its connection, serves the vtest protocol), because selecting Venus is done
/// through environment variables read at Vulkan driver-enumeration time, which is far too early to
/// arrange from inside an already-running test process.
///
/// A watchdog thread (see [`RUN_TIMEOUT`]) bounds how long the Venus render may run, so a genuine
/// stall in the replay path is reported as a labelled timeout rather than hanging this test, and the
/// whole `cargo test` invocation, forever.
#[test]
fn icosa_gpu_renders_bit_identical_frames_through_venus_as_natively() {
    let node = Path::new(RENDER_NODE);
    if !virgl_available(node) {
        eprintln!(
            "SKIP icosa_gpu_renders_bit_identical_frames_through_venus_as_natively: no usable \
             Venus render node at {RENDER_NODE}"
        );
        return;
    }
    if !Path::new(VENUS_ICD).is_file() {
        eprintln!(
            "SKIP icosa_gpu_renders_bit_identical_frames_through_venus_as_natively: no Venus ICD \
             manifest at {VENUS_ICD}"
        );
        return;
    }

    let fixture = build_icosa_gpu();

    // --- The baseline: the same binary, the host's own driver, no Rayland anywhere ---
    let native_dir = std::env::temp_dir().join(format!(
        "rayland-icosa-gpu-e2e-native-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&native_dir);
    std::fs::create_dir_all(&native_dir).expect("the native output directory must be creatable");
    let native_started = Instant::now();
    let native_report = render_natively(&fixture, &native_dir);
    eprintln!(
        "native render of {FRAME_COUNT} frames took {:?} ({} CSV lines)",
        native_started.elapsed(),
        native_report.lines().count()
    );

    // --- The socket. `sockaddr_un::sun_path` is 108 bytes: see `refapp_venus_e2e.rs`'s identical
    // note on why this path must stay short and under `/tmp`, not a scratch/session directory. ---
    let socket_path = PathBuf::from(format!("/tmp/rl-igp-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("binding the vtest socket must succeed");
    listener
        .set_nonblocking(true)
        .expect("the listener must accept a non-blocking mode");

    // Bring the engine up BEFORE launching the client — see `refapp_venus_e2e.rs`'s identical
    // ordering note on why this avoids confusing timeouts that look like protocol faults.
    let mut engine = VirglEngine::new(node).expect("VirglEngine::new must succeed on a GPU host");

    let venus_dir = std::env::temp_dir().join(format!(
        "rayland-icosa-gpu-e2e-venus-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&venus_dir);
    std::fs::create_dir_all(&venus_dir).expect("the venus output directory must be creatable");

    let connect_started = Instant::now();
    let mut child = Command::new(&fixture)
        .arg(&venus_dir)
        // REQUIRED and fails silently without it — see `docs/c0-venus-first-light.md`'s "The
        // environment pitfalls": Venus prefers its virtgpu backend and only tries vtest when told.
        .env("VN_DEBUG", "vtest")
        .env("VK_ICD_FILENAMES", VENUS_ICD)
        .env("VTEST_SOCKET_NAME", &socket_path)
        // A driver filter left in the environment hides the Venus ICD silently — see the same doc.
        .env_remove("VK_LOADER_DRIVERS_SELECT")
        .spawn()
        .expect("the fixture must be launchable");
    let child_pid = child.id();

    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _addr)) => break stream,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if let Ok(Some(status)) = child.try_wait() {
                    let _ = std::fs::remove_file(&socket_path);
                    panic!(
                        "the fixture exited ({status}) without ever connecting to the vtest \
                         socket — Venus almost certainly failed to initialise; re-run with \
                         --nocapture to see its stderr"
                    );
                }
                assert!(
                    connect_started.elapsed() < CONNECT_TIMEOUT,
                    "the fixture did not connect within {CONNECT_TIMEOUT:?}"
                );
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&socket_path);
                panic!("accepting the client connection failed: {e}");
            }
        }
    };
    stream
        .set_nonblocking(false)
        .expect("the accepted stream must be switchable to blocking mode");

    // The watchdog: if the render is still running past RUN_TIMEOUT, kill the child so the blocking
    // `serve_vtest` read below returns (with an I/O error) instead of hanging this test, and by
    // extension the whole suite, forever. `rendering_done` is the handoff: the main thread sets it
    // the instant `serve_vtest` returns, so the watchdog never fires on a run that merely finished
    // slowly.
    let rendering_done = Arc::new(AtomicBool::new(false));
    let watchdog_flag = Arc::clone(&rendering_done);
    let watchdog_started = Instant::now();
    let watchdog = std::thread::spawn(move || {
        while watchdog_started.elapsed() < RUN_TIMEOUT {
            if watchdog_flag.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(WATCHDOG_POLL_INTERVAL);
        }
        if !watchdog_flag.load(Ordering::SeqCst) {
            eprintln!(
                "WATCHDOG: the Venus render exceeded {RUN_TIMEOUT:?}; killing pid {child_pid}"
            );
            // SAFETY: `child_pid` is a plain integer captured before this thread started; sending
            // SIGKILL to it is exactly what `kill -9 <pid>` does at the syscall level, and is safe
            // regardless of whether the process has already exited (the syscall just reports ESRCH).
            unsafe {
                libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
            }
        }
    });

    // Serve the session. This is the call that replays the fixture's Vulkan command stream —
    // including its per-frame 80-byte mapped uniform write — on the real GPU.
    let serve_result = serve_vtest(&mut stream, &mut engine);
    // Signal the watchdog immediately: whatever happens below, the render itself has finished (or
    // definitively errored), so a slow-but-successful run must not be killed out from under the
    // cleanup code that follows.
    rendering_done.store(true, Ordering::SeqCst);
    let watchdog_fired = watchdog_started.elapsed() >= RUN_TIMEOUT;
    let _ = watchdog.join();

    let outcome = match serve_result {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = child.kill();
            let _ = std::fs::remove_file(&socket_path);
            if watchdog_fired {
                panic!(
                    "TIMEOUT: the Venus render did not complete within {RUN_TIMEOUT:?} and was \
                     killed; this is a hang/deadlock finding, distinct from a pixel mismatch. \
                     Underlying I/O error after the kill: {error}"
                );
            }
            panic!("the vtest session must complete: {error}");
        }
    };
    assert_eq!(
        outcome.context_id,
        Some(1),
        "the session must have created a Venus context"
    );

    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            let _ = child.kill();
            let _ = std::fs::remove_file(&socket_path);
            panic!("the fixture must be waitable: {error}");
        }
    };
    let _ = std::fs::remove_file(&socket_path);
    eprintln!(
        "venus render of {FRAME_COUNT} frames took {:?} (including connection setup)",
        connect_started.elapsed()
    );
    assert!(
        status.success(),
        "the fixture must exit successfully when rendered through Rayland; got {status}"
    );

    // --- The proof: every frame, byte for byte, first divergence only ---
    assert_all_frames_bit_identical(&native_dir, &venus_dir);
}
