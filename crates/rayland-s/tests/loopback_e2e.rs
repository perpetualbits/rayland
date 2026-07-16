//! **(c)1's proof**: an unmodified Vulkan application's command stream crosses a real network and
//! still produces a correct frame on a remote GPU.
//!
//! # What this test actually demonstrates, and how it differs from C0's
//! C0's `rayland-engine/tests/refapp_venus_e2e.rs` proved that Venus's command stream replays
//! faithfully on a real GPU — but it ran **in one process, over shared memory**. It proved the
//! stream is *language*; it proved nothing about remoting, because nothing crossed anything.
//!
//! This test puts a genuine network in the path. It launches three processes:
//!
//! 1. **`rayland-s`** — the S-side host, holding the real GPU, listening on `127.0.0.1`.
//! 2. **`rayland-c`** — the C-side daemon: a vtest server on a local Unix socket, relaying to S.
//! 3. **`rayland-refapp`** — the same unmodified Vulkan triangle program C0 used, which depends on
//!    no `rayland-*` crate and has never heard of Venus, vtest, or remoting.
//!
//! The application's Vulkan commands leave C's shared-memory ring, cross a QUIC connection, are
//! replayed on S's GPU, and the resulting pixels cross back — and the app writes the same PNG it
//! would have written natively. **Loopback is not a real network's latency, but it is a real
//! network stack**: real UDP, real QUIC framing, real serialization, and a real process boundary
//! with no shared page between the two halves. It catches protocol bugs, not timing ones, and the
//! spec (§10.1) is explicit about being honest which.
//!
//! # Why this test lives in `rayland-s`
//! It needs `virgl_available` for its GPU gate, which lives behind `rayland-engine` — the one crate
//! that FFI-links `libvirglrenderer` and which **`rayland-c` must never depend on**. `rayland-s`
//! already depends on it legitimately (it *is* the GPU machine), so this is the only side of the
//! boundary the test can live on without compromising the C-has-no-GPU claim that
//! `rayland-c/tests/no_gpu_linkage.rs` enforces.
//!
//! # Skip, don't fail, without a GPU
//! Gates on [`virgl_available`] exactly as `refapp_venus_e2e.rs`, `live_socket.rs` and
//! `reliability.rs` do, so a machine with no usable Venus render node prints SKIP and stays green.
//!
//! Run on a GPU host with: `cargo test -p rayland-s --test loopback_e2e -- --nocapture`.

// The GPU gate. `rayland-s` depends on `rayland-engine` legitimately — it is the GPU machine.
use rayland_engine::virgl_available;
// Reading back the two PNGs and comparing them pixel by pixel.
use image::GenericImageView;
// Launching the three processes and watching what they say.
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The DRM render node S renders on. The same node C0 ran its whole proof against.
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// Mesa's Venus ICD manifest. Naming it explicitly via `VK_ICD_FILENAMES` is what makes the Vulkan
/// loader hand the reference app the Venus driver instead of the host's native one.
const VENUS_ICD: &str = "/usr/share/vulkan/icd.d/virtio_icd.json";

/// The image size the reference app renders at; asserted against the decoded PNGs rather than
/// assumed, so a change on the app's side surfaces here rather than silently moving the pixels the
/// assertions below index.
const IMAGE_SIZE: u32 = 64;

/// How long to wait for a daemon to announce it is ready, or for the application to finish.
///
/// Generous, because this covers `virgl_renderer_init` forking a render server and initialising
/// EGL, Venus initialising, the Vulkan loader enumerating drivers, and — new in (c)1 — a QUIC
/// handshake and every one of the application's synchronous Vulkan calls becoming a network round
/// trip. It exists only so that a path which never completes surfaces as a diagnosable timeout
/// naming *which* stage stalled, instead of hanging the test suite forever.
const STAGE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often to re-check a pending condition while waiting out [`STAGE_TIMEOUT`].
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A child process whose stderr is captured line by line while still being echoed.
///
/// # Why the log is captured rather than merely inherited
/// The brief for this task is blunt about the failure mode it exists to prevent: *a test that
/// passes when nothing happened is worse than no test*, and this branch has produced ten of them.
/// The reference app writing a correct PNG is **not on its own proof that anything crossed the
/// network** — so this type exists so the test can assert on what the daemons actually reported
/// doing. [`Daemon::wait_for_log`] is what turns "the picture is right" into "the picture is right
/// *and* the application connected to our socket *and* the ring was relayed".
///
/// The lines are echoed to the test's own stderr as they arrive rather than only on failure,
/// because under `--nocapture` this is the only view a human debugging a stalled run has.
struct Daemon {
    /// The process handle, for liveness checks and teardown.
    child: Child,
    /// Every stderr line seen so far, in order. Shared with the reader thread.
    log: Arc<Mutex<Vec<String>>>,
    /// A human-readable name, used in panic messages so a timeout names the guilty process.
    name: &'static str,
}

impl Daemon {
    /// Spawn `command` with its stderr piped into a background reader thread.
    ///
    /// # Panics
    /// Panics if the process cannot be spawned or its stderr cannot be captured — both are
    /// environment faults that would make the test meaningless rather than a result worth having.
    fn spawn(name: &'static str, mut command: Command) -> Self {
        let mut child = command
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("{name} must be launchable: {e}"));
        let stderr = child
            .stderr
            .take()
            .unwrap_or_else(|| panic!("{name}'s stderr must be pipeable"));

        let log = Arc::new(Mutex::new(Vec::new()));
        // The reader must run on its own thread: a pipe has a finite buffer, and a daemon whose
        // stderr nobody drains will eventually block *inside a write to stderr* — which would look
        // exactly like the ring stall this test is trying to detect, with none of the same cause.
        std::thread::spawn({
            let log = Arc::clone(&log);
            move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    // Echo as it arrives: under `--nocapture` this is the only live view of a run
                    // that is still going, which is exactly when it is most needed.
                    eprintln!("[{name}] {line}");
                    log.lock()
                        .expect("the log lock is never poisoned")
                        .push(line);
                }
            }
        });

        Daemon { child, log, name }
    }

    /// Block until a line containing `marker` appears in this daemon's log.
    ///
    /// The process is polled on the same loop, so a daemon that *dies* rather than merely being
    /// slow is reported as what it is, immediately, instead of stalling until the timeout.
    ///
    /// # Panics
    /// Panics if the process exits first, or if `marker` has not appeared within
    /// [`STAGE_TIMEOUT`]. Both panics quote the daemon's whole log, because the interesting
    /// information is always the line *before* the one that never came.
    fn wait_for_log(&mut self, marker: &str) {
        let started = Instant::now();
        loop {
            if self.log_contains(marker) {
                return;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "{} exited ({status}) before reporting {marker:?}.\n--- {} log ---\n{}",
                    self.name,
                    self.name,
                    self.log_text()
                );
            }
            assert!(
                started.elapsed() < STAGE_TIMEOUT,
                "{} did not report {marker:?} within {STAGE_TIMEOUT:?}.\n--- {} log ---\n{}",
                self.name,
                self.name,
                self.log_text()
            );
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Whether any line logged so far contains `marker`.
    fn log_contains(&self, marker: &str) -> bool {
        self.log
            .lock()
            .expect("the log lock is never poisoned")
            .iter()
            .any(|l| l.contains(marker))
    }

    /// The whole log as one string, for panic messages.
    fn log_text(&self) -> String {
        self.log
            .lock()
            .expect("the log lock is never poisoned")
            .join("\n")
    }
}

impl Drop for Daemon {
    /// Kill the daemon when the test drops it, on **every** path including a panic.
    ///
    /// Rust does not kill a `Child` on drop. Without this, a failing assertion anywhere below would
    /// unwind out of the test leaving `rayland-s` holding a GPU context and `rayland-c` owning the
    /// vtest socket — which would then make the *next* run fail for a reason that has nothing to do
    /// with the code under test. Reliability is a first-class criterion for this task (the spec's
    /// §1: "no orphaned processes, no wedged sessions"), so the cleanup is structural rather than
    /// something each path remembers to do.
    fn drop(&mut self) {
        // Best-effort throughout: the child may already have exited, in which case both calls
        // harmlessly fail, and there is nothing useful to do about a failure during teardown.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Build a workspace binary by package name and return the path to its executable.
///
/// # Why this shells out to Cargo instead of using `CARGO_BIN_EXE_*`
/// Cargo only defines `CARGO_BIN_EXE_<name>` for binaries belonging to the *same package* as the
/// test. `rayland-c` and `rayland-refapp` are deliberately different packages — the refapp because
/// depending on any `rayland-*` crate would destroy its value as an unmodified witness, and
/// `rayland-c` because it must never link the GPU stack this package does — and Cargo has no stable
/// way for one package's test to depend on another package's *binary*. So the test asks Cargo
/// directly. `cargo build` is a no-op when the binary is already current, which it is under
/// `cargo test --workspace`.
///
/// # Panics
/// Panics if Cargo cannot be run, if the build fails, or if the expected executable is missing
/// afterwards — all environment faults that would make the test meaningless.
fn build_binary(package: &str) -> PathBuf {
    // `CARGO` is set by Cargo when it runs a test and points at the very same Cargo driving this
    // run — safer than assuming a `cargo` on `PATH` is the same toolchain.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "-p", package])
        .status()
        .unwrap_or_else(|e| panic!("cargo must be runnable to build {package}: {e}"));
    assert!(status.success(), "building {package} must succeed");

    // The test binary lives at `<target>/<profile>/deps/<name>-<hash>`, so the profile directory
    // holding the workspace's binaries is two levels up. Deriving it from `current_exe` rather than
    // hardcoding `target/debug` keeps this correct under `CARGO_TARGET_DIR` and `--release`.
    let test_exe = std::env::current_exe().expect("the test binary must have a path");
    let profile_dir = test_exe
        .parent()
        .and_then(Path::parent)
        .expect("the test binary must live in <target>/<profile>/deps/");
    let binary = profile_dir.join(package);
    assert!(
        binary.is_file(),
        "cargo built {package} but no executable is at {}",
        binary.display()
    );
    binary
}

/// Find a UDP port nobody is listening on, for S to bind.
///
/// # Why a discovered port rather than a fixed one
/// A fixed port makes the test fail spuriously when a previous run left a process behind, or when
/// anything else on the box happens to want it — both of which present as an unexplained
/// connection failure rather than as what they are. QUIC is UDP, so the probe binds UDP.
///
/// There is an unavoidable race here: the port is free when probed and could in principle be taken
/// before S binds it. It is accepted rather than solved because the alternative — having S report
/// its bound port back — is a change to the daemon's interface made solely for a test, and the
/// window is microseconds wide on a box running one test at a time.
///
/// # Panics
/// Panics if no ephemeral port can be bound at all, which is an environment fault.
fn free_udp_port() -> u16 {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0")
        .expect("binding an ephemeral UDP port must succeed");
    socket
        .local_addr()
        .expect("a bound socket must have a local address")
        .port()
}

/// Run the reference app on the host's **own** Vulkan driver and return the PNG path it wrote.
///
/// # The `env_remove` calls are the whole point of this function
/// The Venus variables are explicitly *removed* rather than merely left unset: this test process may
/// itself have been launched from a shell that exported them, and inheriting one here would silently
/// turn the "native" baseline into a second Venus run — which would make the comparison below
/// compare Venus against itself and pass while proving nothing at all.
///
/// # Panics
/// Panics if the app cannot be launched or exits unsuccessfully.
fn render_natively(refapp: &Path, output: &Path) {
    let status = Command::new(refapp)
        .arg(output)
        .env_remove("VK_ICD_FILENAMES")
        .env_remove("VN_DEBUG")
        .env_remove("VN_PERF")
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
/// serve to hide a real defect — and this task's brief explicitly forbids weakening these
/// assertions to manufacture a pass.
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

/// **The headline test**: the same unmodified binary renders the same triangle whether it runs on
/// the local GPU or ships its command stream across a network to another process's GPU.
///
/// # Why three processes rather than threads
/// Selecting Venus is done through environment variables read by the Vulkan loader at
/// driver-enumeration time, which is far too early to arrange from inside an already-running test
/// process — so the application must be a child. And once it is, C and S may as well be too: it is
/// what a real deployment has, it is the only way the "C needs no GPU" claim is even meaningful,
/// and it means a crash in any one of them is visible as a crash rather than as a corrupted test.
#[test]
fn refapp_renders_across_the_network_the_same_triangle_it_renders_natively() {
    let node = Path::new(RENDER_NODE);
    if !virgl_available(node) {
        eprintln!(
            "SKIP refapp_renders_across_the_network_the_same_triangle_it_renders_natively: \
             no usable Venus render node at {RENDER_NODE}"
        );
        return;
    }
    if !Path::new(VENUS_ICD).is_file() {
        eprintln!(
            "SKIP refapp_renders_across_the_network_the_same_triangle_it_renders_natively: \
             no Venus ICD manifest at {VENUS_ICD}"
        );
        return;
    }

    let refapp = build_binary("rayland-refapp");
    let rayland_c = build_binary("rayland-c");
    let rayland_s = build_binary("rayland-s");

    // --- The baseline: the same binary, this host's own driver, no Rayland anywhere.
    //
    // C and S are the same machine here, so this host *is* S and this is the right baseline. The
    // spec (§10.2) is precise about why that matters: bit-identity is only a legitimate assertion
    // against a native run on **S's** GPU, because that is the GPU that will actually draw the
    // remote frame. Comparing against a different machine's GPU would be meaningless.
    let native_png = std::env::temp_dir().join("rayland-c1-native.png");
    let _ = std::fs::remove_file(&native_png);
    render_natively(&refapp, &native_png);
    let native_pixels = assert_triangle_png(&native_png, "native");

    // --- S: the machine with the GPU.
    let s_port = free_udp_port();
    let s_addr = format!("127.0.0.1:{s_port}");
    let mut s = {
        let mut command = Command::new(&rayland_s);
        command
            .env("RAYLAND_C1_S_LISTEN", &s_addr)
            .env("RAYLAND_C1_RENDER_NODE", RENDER_NODE)
            // (c)1 Task 7: S now ends a session by opening a **window** and waiting for a human to
            // close it (spec §1's second verification path). That is exactly right for the manual
            // bring-up and impossible here: nothing in an automated test can click a close button,
            // so `rayland-s` would never exit and this suite would pop a window on the developer's
            // desktop on every run. Disabling it costs this test nothing — it asserts the
            // application's PNG on C, which is §1's *other*, independent path and does not go near
            // presentation. The presentation path has its own coverage:
            // `rayland-s/tests/present.rs` for the frame identification, `rayland-present`'s
            // `tests/live_window.rs` for a real compositor, and a human for the actual triangle.
            .env("RAYLAND_C1_NO_PRESENT", "1");
        Daemon::spawn("rayland-s", command)
    };
    // Wait for S to be listening before starting C. C connects to S at startup and exits if it
    // cannot reach it, so starting them in the wrong order is a guaranteed spurious failure.
    s.wait_for_log("listening on");

    // --- C: the machine with the application and no GPU.
    //
    // `sockaddr_un::sun_path` is 108 bytes, so this path must stay SHORT: a path under a temp-session
    // or scratchpad directory overflows it and `bind` fails outright. C0 hit exactly this.
    let socket_path = PathBuf::from(format!("/tmp/rl-c1-{}.sock", std::process::id()));
    // A Unix socket leaves its filesystem entry behind, so a stale one from a crashed run would make
    // `bind` fail with EADDRINUSE even with nothing listening. (The daemon does this too; doing it
    // here as well means a failure to bind is unambiguously not our leftovers.)
    let _ = std::fs::remove_file(&socket_path);
    let mut c = {
        let mut command = Command::new(&rayland_c);
        command
            .env("RAYLAND_C1_SOCKET", &socket_path)
            .env("RAYLAND_C1_S_ADDR", &s_addr);
        Daemon::spawn("rayland-c", command)
    };
    // C binds its socket only after it has reached S, so this one line proves the link is up.
    c.wait_for_log("listening at");

    // --- The application: unmodified, unaware, and pointed at C.
    let venus_png = std::env::temp_dir().join("rayland-c1-venus.png");
    let _ = std::fs::remove_file(&venus_png);
    let started = Instant::now();
    let mut app = Command::new(&refapp)
        .arg(&venus_png)
        // Mesa's Venus ICD prefers its virtgpu backend and only tries the vtest one when told to.
        // Without this the app fails at driver enumeration and NEVER CONNECTS AT ALL — a failure
        // that looks like a hang on our side rather than a client that never tried to reach us.
        // `VTEST_SOCKET_NAME` is read *after* backend selection, so it cannot select vtest itself.
        .env("VN_DEBUG", "vtest")
        // Spec §6's crutch table, in full. Every one of these is a **declared crutch with an exit
        // condition**, not "how Rayland works" — the table exists precisely to stop them quietly
        // becoming permanent:
        //   - `no_multi_ring` forces a single ring, making the `ring_idx = 0` assumption in the
        //     fence path legitimate rather than lucky (latent since C0 Task 3).
        //   - the four `no_*_feedback` switches remove Venus's S->C shared status pages (the spec's
        //     channel 3), which a network cannot carry, making the stream self-contained. This is
        //     the first thing (c)2 should buy back.
        // `VN_DEBUG=no_abort` is deliberately **not** set; see the test body's comment below.
        .env(
            "VN_PERF",
            "no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback",
        )
        // Point the loader at Venus rather than the host's native driver.
        .env("VK_ICD_FILENAMES", VENUS_ICD)
        // Tell Venus's vtest backend which socket to dial — i.e. point the application at C.
        .env("VTEST_SOCKET_NAME", &socket_path)
        // A driver filter left in the environment (e.g. `*intel*`) makes the loader silently hide
        // the Venus ICD, and the app then reports "no Vulkan devices" — nothing to do with Rayland.
        .env_remove("VK_LOADER_DRIVERS_SELECT")
        .spawn()
        .expect("the reference app must be launchable");

    // **The proof that anything happened at all.** A correct PNG is not on its own evidence that
    // the network was involved — so before looking at a single pixel, require C to report both that
    // the application reached *our* vtest socket and that the command ring was found and watched.
    // Without these two lines the test could pass having proved nothing, which this branch has
    // managed ten times.
    c.wait_for_log("Mesa connected");
    c.wait_for_log("watching command ring");

    // Wait for the app, failing loudly rather than hanging if it never finishes. The daemons are
    // polled on the same loop: if S dies mid-session the app will hang forever waiting for replies,
    // and reporting *that* is far more useful than a bare timeout.
    let status = loop {
        if let Some(status) = app.try_wait().expect("the reference app must be waitable") {
            break status;
        }
        if let Ok(Some(status)) = s.child.try_wait() {
            let _ = app.kill();
            panic!(
                "rayland-s exited ({status}) while the application was still running.\n\
                 --- rayland-s log ---\n{}\n--- rayland-c log ---\n{}",
                s.log_text(),
                c.log_text()
            );
        }
        if let Ok(Some(status)) = c.child.try_wait() {
            let _ = app.kill();
            panic!(
                "rayland-c exited ({status}) while the application was still running.\n\
                 --- rayland-c log ---\n{}\n--- rayland-s log ---\n{}",
                c.log_text(),
                s.log_text()
            );
        }
        if started.elapsed() >= STAGE_TIMEOUT {
            let _ = app.kill();
            panic!(
                "the reference app did not finish within {STAGE_TIMEOUT:?}. This is the shape a \
                 ring stall takes: `head` never advances, so the app spins in `vn_ring_wait_seqno` \
                 forever.\n--- rayland-c log ---\n{}\n--- rayland-s log ---\n{}",
                c.log_text(),
                s.log_text()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    let elapsed = started.elapsed();
    // Cleanup of the socket file happens regardless of what the assertions below decide; the two
    // daemons are killed by `Daemon::drop` on every path, panic included.
    let _ = std::fs::remove_file(&socket_path);

    assert!(
        status.success(),
        "the reference app must exit successfully when rendered across the network; got {status}.\n\
         --- rayland-c log ---\n{}\n--- rayland-s log ---\n{}",
        c.log_text(),
        s.log_text()
    );

    // --- The proof.
    let venus_pixels = assert_triangle_png(&venus_png, "relayed");

    // Bit-identical or not, both are interesting and both are reported. The pixel assertions above
    // are what this test *enforces*; this comparison is what it *reports*. Spec §10.2 asks for
    // exactly this split: an exact match is a strictly stronger result than (c)1 needs to claim, so
    // a future driver or GPU difference that breaks byte-equality while still drawing the right
    // triangle should surface loudly to a human rather than turning CI red.
    if native_pixels == venus_pixels {
        eprintln!(
            "OK: the relayed image is BIT-IDENTICAL to the native one ({} bytes)",
            venus_pixels.len()
        );
    } else {
        let differing = native_pixels
            .iter()
            .zip(venus_pixels.iter())
            .filter(|(a, b)| a != b)
            .count();
        eprintln!(
            "NOTE: the relayed image shows the correct triangle but is NOT bit-identical to the \
             native one: {differing} of {} bytes differ. This is not a failure — the pixel \
             assertions above passed — but it is worth understanding, because both runs used the \
             same GPU and C0 measured this path at 0/16384 bytes differing.",
            native_pixels.len()
        );
    }

    eprintln!(
        "OK: an unmodified Vulkan application rendered a red triangle on blue by shipping its \
         COMMAND STREAM across a QUIC connection to another process, which replayed it on \
         {RENDER_NODE}. Wall-clock for the relayed run: {elapsed:?}."
    );
}
