//! **C0's proof**: a real, unmodified Vulkan application renders on this host's GPU by shipping its
//! *command stream* to Rayland's engine — not its pixels.
//!
//! # What this test actually demonstrates
//! It launches `rayland-refapp` — an ordinary off-screen Vulkan triangle program that depends on no
//! Rayland crate and contains no mention of Venus, vtest, or remoting — **twice**:
//!
//! 1. **Natively**, on the host's own Vulkan driver, with nothing of Rayland involved.
//! 2. **Through Rayland**, by pointing the Vulkan loader at Mesa's Venus ICD and pointing Venus's
//!    vtest backend at a socket that *this test* is serving with a real [`VirglEngine`].
//!
//! Then it asserts that the second PNG shows the same picture as the first. The binary is byte-for-byte
//! the same in both runs; only the environment differs. That is the whole claim: the application does
//! not know, and cannot tell, that its Vulkan commands were serialized, sent across a socket, and
//! replayed by a different driver — and the picture comes out the same anyway.
//!
//! # Why this test lives in `rayland-engine` rather than in `rayland-refapp`
//! Two reasons, and the first is the important one. **The reference app must not depend on any
//! `rayland-*` crate** — that independence is precisely what makes it a valid witness, and a
//! dev-dependency on `rayland-engine` would compromise it just as surely as a real one. So the test
//! that needs both halves has to live on this side of the boundary. Second, this is where the
//! GPU-gating helper ([`virgl_available`]) and the vtest server ([`serve_vtest`]) already are, so
//! the test reuses this crate's established skip idiom rather than inventing a second one.
//!
//! # Skip, don't fail, without a GPU
//! Gates on [`virgl_available`] exactly as `live_socket.rs` and `reliability.rs` do, so a machine
//! with no usable Venus render node prints SKIP and stays green. CI stays light.
//!
//! Run on a GPU host with: `cargo test -p rayland-engine --test refapp_venus_e2e -- --nocapture`.

// The engine and the vtest protocol server this test puts behind the socket.
use rayland_engine::{VirglEngine, virgl_available, vtest::serve_vtest};
// Reading back the two PNGs and comparing them pixel by pixel.
use image::GenericImageView;
// Building and launching the reference app, and serving its connection.
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The DRM render node this crate's GPU tests use.
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// Mesa's Venus ICD manifest. Naming it explicitly via `VK_ICD_FILENAMES` is what makes the Vulkan
/// loader hand the reference app the Venus driver instead of the host's native one.
const VENUS_ICD: &str = "/usr/share/vulkan/icd.d/virtio_icd.json";

/// The image size the reference app renders at; asserted against the decoded PNGs rather than
/// assumed, so a change on the app's side surfaces here rather than silently moving the pixels the
/// assertions below index.
const IMAGE_SIZE: u32 = 64;

/// How long to wait for the reference app to connect to the socket before declaring the run failed.
///
/// Generous, because this covers Venus initialising, `virgl_renderer_init` having already forked its
/// render server, and the Vulkan loader enumerating drivers. It exists only so that a client which
/// never connects at all — the classic `VN_DEBUG=vtest` omission, which fails *silently* on the
/// client side — surfaces as a diagnosable timeout instead of hanging the test suite forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How often to re-check for an incoming connection while waiting out [`CONNECT_TIMEOUT`].
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Build `rayland-refapp` and return the path to its executable.
///
/// # Why this shells out to Cargo instead of using `CARGO_BIN_EXE_*`
/// Cargo only defines `CARGO_BIN_EXE_<name>` for binaries belonging to the *same package* as the
/// test. `rayland-refapp` is deliberately a different package (see the module docs), and Cargo has
/// no stable way for one package's test to depend on another package's *binary* — artifact
/// dependencies remain unstable, and an ordinary dependency is both forbidden here and impossible
/// (the refapp has no library target). So the test asks Cargo for the binary directly. `cargo build`
/// is a no-op when the binary is already current, which it is under `cargo test --workspace`.
///
/// # Panics
/// Panics if Cargo cannot be run, if the build fails, or if the expected executable is missing
/// afterwards — all of which are environment faults that would make the test meaningless rather
/// than results worth reporting.
fn build_refapp() -> PathBuf {
    // `CARGO` is set by Cargo when it runs a test, and points at the very same Cargo that is
    // driving this run — safer than assuming a `cargo` on `PATH` is the same toolchain.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "-p", "rayland-refapp"])
        .status()
        .expect("cargo must be runnable to build the reference app");
    assert!(status.success(), "building rayland-refapp must succeed");

    // The test binary lives at `<target>/<profile>/deps/<name>-<hash>`, so the profile directory
    // that holds the workspace's binaries is two levels up. Deriving it from `current_exe` rather
    // than hardcoding `target/debug` keeps this correct under `CARGO_TARGET_DIR`, `--release`, and
    // cross-compilation, none of which this test has any business caring about.
    let test_exe = std::env::current_exe().expect("the test binary must have a path");
    let profile_dir = test_exe
        .parent()
        .and_then(Path::parent)
        .expect("the test binary must live in <target>/<profile>/deps/");
    let refapp = profile_dir.join("rayland-refapp");
    assert!(
        refapp.is_file(),
        "cargo built rayland-refapp but no executable is at {}",
        refapp.display()
    );
    refapp
}

/// Run the reference app on the host's **own** Vulkan driver and return the PNG path it wrote.
///
/// The three Venus variables are explicitly *removed* rather than merely left unset: this test
/// process may itself have been launched from a shell that exported them, and inheriting one here
/// would silently turn the "native" baseline into a second Venus run — which would make the
/// comparison below compare Venus against itself and pass while proving nothing at all.
///
/// # Panics
/// Panics if the app cannot be launched or exits unsuccessfully.
fn render_natively(refapp: &Path, output: &Path) {
    let status = Command::new(refapp)
        .arg(output)
        .env_remove("VK_ICD_FILENAMES")
        .env_remove("VN_DEBUG")
        .env_remove("VTEST_SOCKET_NAME")
        .status()
        .expect("the reference app must be launchable");
    assert!(
        status.success(),
        "the reference app must render natively on a GPU host; got {status}"
    );
}

/// Assert the PNG at `path` is the expected picture: centre red, all four corners blue.
///
/// Returns the decoded raw RGBA bytes so the caller can compare two runs byte for byte.
///
/// The colours are checked for *exact* equality, which is correct rather than strict: the shader
/// writes a constant `(1, 0, 0, 1)` and the clear is a constant `(0, 0, 1, 1)`, both of which land
/// on exact `UNORM` bytes with no interpolation or rounding in between. A tolerance here would only
/// serve to hide a real defect.
///
/// # Panics
/// Panics if the PNG is missing, undecodable, the wrong size, or shows the wrong picture.
fn assert_triangle_png(path: &Path, label: &str) -> Vec<u8> {
    let image = image::open(path)
        .unwrap_or_else(|e| panic!("{label}: the app must have written a decodable PNG: {e}"));
    assert_eq!(
        image.dimensions(),
        (IMAGE_SIZE, IMAGE_SIZE),
        "{label}: wrong image size"
    );

    let red = image::Rgba([255, 0, 0, 255]);
    let blue = image::Rgba([0, 0, 255, 255]);
    // The centre is deep inside the triangle: if the draw did not land, this is still blue.
    assert_eq!(
        image.get_pixel(IMAGE_SIZE / 2, IMAGE_SIZE / 2),
        red,
        "{label}: the centre pixel must be the triangle's red"
    );
    // Every corner is outside the triangle: if the clear did not happen, or the geometry is the
    // wrong size, or the image is flipped or transposed, at least one of these stops being blue.
    let last = IMAGE_SIZE - 1;
    for (x, y, corner) in [
        (0, 0, "top-left"),
        (last, 0, "top-right"),
        (0, last, "bottom-left"),
        (last, last, "bottom-right"),
    ] {
        assert_eq!(
            image.get_pixel(x, y),
            blue,
            "{label}: the {corner} corner must be the cleared blue"
        );
    }
    image.to_rgba8().into_raw()
}

/// The headline test: the same unmodified binary, rendered natively and through Rayland, produces
/// the same picture.
///
/// # Structure, and why the engine runs on this thread
/// The reference app runs as a **child process** — it must, because selecting Venus is done through
/// environment variables read by the Vulkan loader at driver-enumeration time, which is far too
/// early to arrange from inside an already-running test process. This thread plays the server: it
/// binds the socket, brings the engine up, launches the child, accepts its connection, and serves
/// the vtest protocol until the app disconnects. The protocol is request/response with a blocking
/// read on each side, so the two halves genuinely must run concurrently; a process boundary is the
/// most faithful way to arrange that, and is what a real deployment would have anyway.
#[test]
fn refapp_renders_the_same_triangle_through_venus_as_it_does_natively() {
    let node = Path::new(RENDER_NODE);
    if !virgl_available(node) {
        eprintln!(
            "SKIP refapp_renders_the_same_triangle_through_venus_as_it_does_natively: no usable Venus render node at {RENDER_NODE}"
        );
        return;
    }
    if !Path::new(VENUS_ICD).is_file() {
        eprintln!(
            "SKIP refapp_renders_the_same_triangle_through_venus_as_it_does_natively: no Venus ICD manifest at {VENUS_ICD}"
        );
        return;
    }

    let refapp = build_refapp();

    // --- The baseline: the same binary, the host's own driver, no Rayland anywhere ---
    let native_png = std::env::temp_dir().join("rayland-e2e-native.png");
    let _ = std::fs::remove_file(&native_png);
    render_natively(&refapp, &native_png);
    let native_pixels = assert_triangle_png(&native_png, "native");

    // --- The socket. `sockaddr_un::sun_path` is 108 bytes, so this path must stay SHORT: a path
    // under a temp-session or scratchpad directory overflows it and `bind` fails outright. The pid
    // keeps concurrent runs from colliding while staying well inside the limit. ---
    let socket_path = PathBuf::from(format!("/tmp/rl4b-{}.sock", std::process::id()));
    // A Unix socket leaves its filesystem entry behind, so a stale one from a crashed run would
    // make `bind` fail with EADDRINUSE even with nothing listening. A missing file is normal.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("binding the vtest socket must succeed");
    // Non-blocking so the accept loop below can give up rather than hang forever if the client
    // never arrives — see `CONNECT_TIMEOUT`.
    listener
        .set_nonblocking(true)
        .expect("the listener must accept a non-blocking mode");

    // Bring the engine up BEFORE launching the client: `virgl_renderer_init` forks a render server
    // and initialises EGL, which is slow enough that doing it while a client is already waiting on
    // the handshake invites confusing timeouts that look like protocol faults.
    let mut engine = VirglEngine::new(node).expect("VirglEngine::new must succeed on a GPU host");

    // --- The run under test: the same binary, pointed at Venus, pointed at us ---
    let venus_png = std::env::temp_dir().join("rayland-e2e-venus.png");
    let _ = std::fs::remove_file(&venus_png);
    let started = Instant::now();
    let mut child = Command::new(&refapp)
        .arg(&venus_png)
        // Mesa's Venus ICD prefers its virtgpu backend and only tries the vtest one when told to.
        // Without this the app fails at driver enumeration and NEVER CONNECTS AT ALL — a failure
        // that looks like a hang on our side rather than a client that never tried to reach us.
        .env("VN_DEBUG", "vtest")
        // Point the loader at Venus rather than the host's native driver. This single variable is
        // the entire difference between this run and the native one above.
        .env("VK_ICD_FILENAMES", VENUS_ICD)
        // Tell Venus's vtest backend which socket to dial. Read inside `vtest_init`, i.e. only
        // *after* `VN_DEBUG` has already caused the vtest backend to be chosen — which is why this
        // variable alone is not sufficient to select vtest.
        .env("VTEST_SOCKET_NAME", &socket_path)
        // A driver filter left in the environment (e.g. `*intel*`) makes the loader silently hide
        // the Venus ICD, and the app then reports "no Vulkan devices" — nothing to do with Rayland.
        .env_remove("VK_LOADER_DRIVERS_SELECT")
        .spawn()
        .expect("the reference app must be launchable");

    // Accept the app's connection, giving up if it never comes. The child is polled on the same
    // loop so that a client which dies during Vulkan init (rather than merely being slow) is
    // reported as what it is, immediately, instead of stalling until the timeout.
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _addr)) => break stream,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if let Ok(Some(status)) = child.try_wait() {
                    let _ = std::fs::remove_file(&socket_path);
                    panic!(
                        "the reference app exited ({status}) without ever connecting to the vtest socket — \
                         Venus almost certainly failed to initialise; re-run with --nocapture to see its stderr"
                    );
                }
                assert!(
                    started.elapsed() < CONNECT_TIMEOUT,
                    "the reference app did not connect within {CONNECT_TIMEOUT:?}"
                );
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&socket_path);
                panic!("accepting the client connection failed: {e}");
            }
        }
    };
    // Linux does not propagate the listener's non-blocking mode to accepted sockets, but that is a
    // platform detail this test should not rely on: `serve_vtest` does blocking reads, and a
    // non-blocking stream would make it fail spuriously with WouldBlock. Say so explicitly.
    stream
        .set_nonblocking(false)
        .expect("the accepted stream must be switchable to blocking mode");

    // Serve the session. This is the call that replays the app's Vulkan command stream on the real
    // GPU. It returns when the app disconnects at a message boundary, i.e. when it has exited.
    let outcome = serve_vtest(&mut stream, &mut engine).expect("the vtest session must complete");
    assert_eq!(
        outcome.context_id,
        Some(1),
        "the session must have created a Venus context"
    );

    let status = child.wait().expect("the reference app must be waitable");
    // Clean up the socket file regardless of what the assertions below decide.
    let _ = std::fs::remove_file(&socket_path);
    assert!(
        status.success(),
        "the reference app must exit successfully when rendered through Rayland; got {status}"
    );

    // --- The proof ---
    let venus_pixels = assert_triangle_png(&venus_png, "venus");

    // Bit-identical or not, both are interesting and both are reported. The pixel assertions above
    // are what this test *enforces*; this comparison is what it *reports*. They are deliberately
    // separate: an exact match is a strictly stronger result than C0 needs to claim, so a future
    // driver or GPU difference that breaks byte-equality while still drawing the right triangle
    // should be surfaced loudly to a human, not turned into a red test.
    if native_pixels == venus_pixels {
        eprintln!(
            "OK: the venus-rendered image is BIT-IDENTICAL to the native one ({} bytes)",
            venus_pixels.len()
        );
    } else {
        let differing = native_pixels
            .iter()
            .zip(venus_pixels.iter())
            .filter(|(a, b)| a != b)
            .count();
        eprintln!(
            "NOTE: the venus-rendered image shows the correct triangle but is NOT bit-identical to \
             the native one: {differing} of {} bytes differ. This is not a failure — the pixel \
             assertions above passed — but it is worth understanding (different GPU path, rounding, \
             or precision).",
            native_pixels.len()
        );
    }

    eprintln!(
        "OK: an unmodified Vulkan application rendered a red triangle on blue by shipping its \
         COMMAND STREAM to Rayland's engine, which replayed it on {RENDER_NODE}"
    );
}
