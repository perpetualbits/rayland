//! `VirglEngine`: the concrete `RenderEngine` backed by an FFI-embedded `libvirglrenderer`.
//!
//! This module owns all interaction with the C library beyond the raw declarations in `ffi.rs`.
//! It brings up a Venus-capable renderer on a DRM render node, creates Venus contexts, submits
//! command buffers, and tears everything down in the correct order on `Drop`.
//!
//! # The global-singleton rule
//! virglrenderer keeps its entire state in process-global variables (none of its functions take a
//! handle). Therefore at most one initialized `VirglEngine` may exist per process at any time.
//! We enforce this with a process-global flag: `VirglEngine::new` fails with
//! `EngineError::AlreadyActive` if another engine is live, and `Drop` releases the flag only after
//! `virgl_renderer_cleanup` returns. This is what makes *repeated* new→use→drop cycles safe, and
//! is the core of the C0 reliability result.

// The raw FFI surface (constants, structs, C functions, callbacks).
use crate::ffi;
// The trait this engine implements.
use crate::RenderEngine;
// Raw C string / char for the context debug label.
use std::ffi::{CString, c_char, c_int, c_void};
// Global single-instance guard.
use std::sync::atomic::{AtomicBool, Ordering};

// Path handling for the render node.
use std::path::Path;

/// Process-global "an engine is initialized" flag. virglrenderer is a global singleton, so this
/// serializes the whole init→use→cleanup lifecycle to one engine at a time. `false` = no engine.
static ENGINE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Errors from the render engine. Every fallible C call maps into exactly one variant here — a C
/// error can never silently become success. Variants that wrap a C return code carry both the raw
/// code and a human-readable errno name (see [`errno_name`]).
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// Another `VirglEngine` is already initialized in this process. virglrenderer is a global
    /// singleton; only one engine may be live at a time.
    #[error("a virglrenderer engine is already active in this process (it is a global singleton)")]
    AlreadyActive,

    /// The DRM render node could not be opened (absent, or permission denied). This is the
    /// expected "no usable GPU" condition on a CI runner and the reason tests skip rather than fail.
    #[error("render node {path} could not be opened: {source}")]
    RenderNodeUnavailable {
        /// The render-node path we tried to open.
        path: String,
        /// The underlying OS error.
        source: std::io::Error,
    },

    /// `virgl_renderer_init` returned a non-zero code. The renderer's EGL/Venus winsys failed to
    /// come up (e.g. the render node does not support the required EGL/Vulkan features).
    #[error("virgl_renderer_init failed for {path} (rc={rc}: {reason})")]
    InitFailed {
        /// The render-node path passed to the engine.
        path: String,
        /// The raw return code from `virgl_renderer_init`.
        rc: c_int,
        /// Human-readable errno name for `rc`.
        reason: String,
    },

    /// `virgl_renderer_context_create_with_flags` returned a non-zero code for a Venus context.
    /// The most common cause (`EINVAL`) is Venus being unavailable or the render server not
    /// running — but with `RAYLAND_INIT_FLAGS` the render server is enabled, so a failure here is
    /// a genuine capability problem, not the missing-flag artifact the spike diagnosed.
    #[error(
        "virgl_renderer_context_create_with_flags(ctx_id={ctx_id}, capset=venus) failed (rc={rc}: {reason})"
    )]
    ContextCreateFailed {
        /// The context id we tried to create.
        ctx_id: u32,
        /// The raw return code.
        rc: c_int,
        /// Human-readable errno name for `rc`.
        reason: String,
    },

    /// A command buffer's length is not a whole number of 4-byte words. virglrenderer measures
    /// command buffers in `ndw` (number of dwords), so any byte length must be a multiple of 4.
    #[error("command buffer length {len} bytes is not a multiple of 4")]
    UnalignedCommand {
        /// The offending byte length.
        len: usize,
    },

    /// A command buffer is too long to express as `ndw` in the C API's `int` word count.
    #[error("command buffer of {len} bytes is too large to submit")]
    CommandTooLarge {
        /// The offending byte length.
        len: usize,
    },

    /// `virgl_renderer_submit_cmd` returned a non-zero code. The command stream was rejected
    /// (e.g. malformed, or the context is gone).
    #[error("virgl_renderer_submit_cmd(ctx_id={ctx_id}) failed (rc={rc}: {reason})")]
    SubmitFailed {
        /// The context id the command targeted.
        ctx_id: u32,
        /// The raw return code.
        rc: c_int,
        /// Human-readable errno name for `rc`.
        reason: String,
    },

    /// A context id does not fit in the C API's `int`. `virgl_renderer_submit_cmd` takes `ctx_id`
    /// as an `int`; ids come from the (untrusted) vtest client, so an id above `INT_MAX` would
    /// wrap to a negative context under a bare `as c_int` cast. We reject it up front instead.
    #[error("context id {ctx_id} does not fit in a C int and cannot be submitted to")]
    SubmitCtxIdOutOfRange {
        /// The offending context id.
        ctx_id: u32,
    },

    /// The renderer reports no Venus capset, so the vtest handshake cannot answer `VCMD_GET_CAPSET`
    /// with real capability data (typically: no GPU, or this build/host lacks Venus support).
    #[error("no Venus capset available to answer the vtest handshake (Venus unsupported here)")]
    NoVenusCapset,

    /// The stream carrying the vtest protocol failed (peer closed mid-message, socket error, ...).
    #[error("vtest stream I/O failed")]
    VtestIo(#[from] std::io::Error),

    /// A vtest length prefix exceeded [`crate::vtest::MAX_VTEST_PAYLOAD_BYTES`]; the message is
    /// refused before any buffer is allocated (an untrusted peer must not force a huge allocation).
    #[error("vtest message payload of {len} bytes exceeds the maximum allowed")]
    VtestFrameTooLarge {
        /// The offending declared payload length in bytes.
        len: u64,
    },

    /// A vtest message was malformed or an opcode arrived that this server does not implement.
    /// Carries a human-readable description; the server never silently drops an unhandled message.
    #[error("vtest protocol error: {detail}")]
    VtestProtocol {
        /// What was wrong (bad length, unknown/unsupported opcode, out-of-range offset, ...).
        detail: String,
    },
}

/// Maps a C return code (an errno, possibly returned as a positive value) to a short readable
/// name, so `EngineError` messages say `EINVAL` rather than a bare `22`. Falls back to the numeric
/// code for anything `std` does not recognize.
///
/// # Inputs / outputs
/// - `rc`: the raw return code from a `virgl_renderer_*` call (treated by absolute value, since
///   these functions variously return positive or negative errnos).
/// - Returns an owned `String` like `"EINVAL"` or `"os error 22"`.
fn errno_name(rc: c_int) -> String {
    // Normalize sign: virglrenderer returns errnos both as +22 (context create) and, elsewhere,
    // as negatives; `std::io::Error` wants the positive errno.
    let errno = rc.unsigned_abs() as i32;
    // `from_raw_os_error(0)` is not a real error, so guard it to avoid a misleading "success".
    if errno == 0 {
        return "unknown error (rc=0)".to_string();
    }
    // Let std translate the errno to its platform description (e.g. "Invalid argument (os error 22)").
    std::io::Error::from_raw_os_error(errno).to_string()
}

/// The Venus render engine: an initialized `libvirglrenderer` bound to one DRM render node.
///
/// Construct with [`VirglEngine::new`]; it holds the single-instance lock for its lifetime and
/// releases it on `Drop` after cleaning up. All contexts created through it are destroyed on
/// `Drop`, so callers cannot leak GPU contexts by forgetting to tear down.
pub struct VirglEngine {
    /// The cookie handed to virglrenderer, heap-boxed so its address is stable for the whole
    /// lifetime (virglrenderer passes this pointer back to `get_drm_fd`). Kept alive until after
    /// `virgl_renderer_cleanup` in `Drop`.
    cookie: Box<ffi::Cookie>,
    /// Ids of the Venus contexts we created and have not destroyed, so `Drop` can destroy them
    /// before `virgl_renderer_cleanup`.
    contexts: Vec<u32>,
}

impl VirglEngine {
    /// Initializes virglrenderer against `render_node` and brings up its Venus-capable EGL winsys.
    ///
    /// Steps:
    /// 1. Acquire the process-global single-instance lock (fail with `AlreadyActive` if taken).
    /// 2. Verify the render node can be opened (fail with `RenderNodeUnavailable` otherwise) — a
    ///    fast, clear failure before we touch the C library, and the condition tests skip on.
    /// 3. Box a `Cookie` carrying the render-node path and call `virgl_renderer_init` with
    ///    `RAYLAND_INIT_FLAGS` (`USE_EGL | USE_SURFACELESS | VENUS | RENDER_SERVER`) and the
    ///    `'static` callbacks. virglrenderer calls our `get_drm_fd` during this call to open its
    ///    winsys fd.
    ///
    /// # Inputs / outputs
    /// - `render_node`: path to a DRM render node (e.g. `/dev/dri/renderD128`).
    /// - Returns the initialized engine, or an `EngineError` (releasing the lock on any failure).
    ///
    /// # Failure modes
    /// - `AlreadyActive`: another engine is live in this process.
    /// - `RenderNodeUnavailable`: the node is absent or not permitted (the CI-skip condition).
    /// - `InitFailed`: virglrenderer's winsys/Venus init failed on an otherwise-openable node.
    pub fn new(render_node: &Path) -> Result<Self, EngineError> {
        // Render-node path as a string, reused in error messages.
        let render_node_str = render_node.display().to_string();

        // (1) Take the single-instance lock. `compare_exchange` succeeds only if it was `false`.
        // On failure another engine is live and we must not touch the global renderer.
        if ENGINE_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(EngineError::AlreadyActive);
        }
        // From here on, any early return MUST release the lock (via the `guard` closure below).

        // (2) Prove the node is openable before initializing the C library. This gives a clean,
        // specific error (and the tests' skip condition) instead of an opaque init failure, and
        // matches exactly what `get_drm_fd` will do repeatedly during init.
        if let Err(source) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(render_node)
        {
            // Release the lock we just took before returning.
            ENGINE_ACTIVE.store(false, Ordering::Release);
            return Err(EngineError::RenderNodeUnavailable {
                path: render_node_str,
                source,
            });
        }

        // (3) Box the cookie so its heap address is stable; virglrenderer stores the pointer and
        // hands it back to `get_drm_fd` for the engine's whole life.
        let cookie = Box::new(ffi::Cookie {
            render_node: render_node.to_path_buf(),
        });
        // Raw pointer into the boxed cookie, valid as long as `cookie` (a field of `self`) lives.
        let cookie_ptr = (&*cookie as *const ffi::Cookie) as *mut c_void;

        // The callbacks struct is a `'static`; cast away const because the C prototype takes a
        // non-const pointer, though virglrenderer only reads it.
        let cb_ptr = (&ffi::RAYLAND_CALLBACKS as *const ffi::VirglRendererCallbacks)
            as *mut ffi::VirglRendererCallbacks;

        // Call into C. SAFETY: `cookie_ptr` points at a live boxed `Cookie` that outlives this
        // renderer (we move the box into `self` on success, and cleanup runs before it drops);
        // `cb_ptr` points at a `'static`; the flags are the spike-proven Venus set. virglrenderer
        // may invoke `get_drm_fd` before returning.
        let rc = unsafe { ffi::virgl_renderer_init(cookie_ptr, ffi::RAYLAND_INIT_FLAGS, cb_ptr) };
        if rc != 0 {
            // Init failed: release the lock and report. `cookie` drops here, which is safe because
            // a failed init means virglrenderer is not holding onto its pointer.
            ENGINE_ACTIVE.store(false, Ordering::Release);
            return Err(EngineError::InitFailed {
                path: render_node_str,
                rc,
                reason: errno_name(rc),
            });
        }

        // Success: hand the cookie to the engine, which now owns the lifecycle.
        Ok(VirglEngine {
            cookie,
            contexts: Vec::new(),
        })
    }
}

impl RenderEngine for VirglEngine {
    /// Creates a Venus capset context with the given id.
    ///
    /// Calls `virgl_renderer_context_create_with_flags(ctx_id, VENUS, ..)`. The context is
    /// recorded so `Drop` will destroy it. Reusing an id that is already live is a caller error
    /// that virglrenderer will reject; ids are chosen by the caller (Task 2 maps vtest client
    /// connections to ids).
    ///
    /// # Failure modes
    /// - `ContextCreateFailed`: virglrenderer rejected the context (with `RAYLAND_INIT_FLAGS` this
    ///   indicates a real Venus capability problem, since the render server is enabled).
    fn create_venus_context(&mut self, ctx_id: u32) -> Result<(), EngineError> {
        // A short debug label virglrenderer attaches to the context (aids its own logging).
        // `CString::new` cannot fail here: the literal has no interior NUL.
        let name = CString::new("rayland-venus").unwrap_or_default();
        // Length virglrenderer expects (bytes, not counting the NUL it does not require).
        let nlen = name.as_bytes().len() as u32;

        // Create the context. SAFETY: `name` is a valid C string living for the call; `nlen`
        // matches its length; the capset id selects Venus (low 8 bits of ctx_flags). The renderer
        // is initialized (we hold an initialized engine).
        let rc = unsafe {
            ffi::virgl_renderer_context_create_with_flags(
                ctx_id,
                ffi::VIRGL_RENDERER_CAPSET_VENUS,
                nlen,
                name.as_ptr() as *const c_char,
            )
        };
        if rc != 0 {
            // Map the non-zero code to a typed error — never treat it as success.
            return Err(EngineError::ContextCreateFailed {
                ctx_id,
                rc,
                reason: errno_name(rc),
            });
        }

        // Record the id so `Drop` destroys the context even if the caller never does.
        self.contexts.push(ctx_id);
        Ok(())
    }

    /// Submits a raw command buffer to a context.
    ///
    /// virglrenderer measures command buffers in 4-byte words (`ndw`). We validate the byte length
    /// is a multiple of 4 and copy it into a `u32`-aligned buffer (a `Vec<u32>`), which guarantees
    /// the ≥4-byte alignment `virgl_renderer_submit_cmd` requires (a bare `&[u8]` is not
    /// guaranteed aligned, and a misaligned buffer returns `EFAULT`).
    ///
    /// # Failure modes
    /// - `UnalignedCommand`: `cmd.len()` is not a multiple of 4.
    /// - `CommandTooLarge`: the word count would overflow the C API's `int`.
    /// - `SubmitFailed`: virglrenderer rejected the command stream.
    fn submit(&mut self, ctx_id: u32, cmd: &[u8]) -> Result<(), EngineError> {
        // Command buffers are dword-counted; a non-multiple-of-4 length is meaningless.
        // (`% 4` rather than `usize::is_multiple_of`, which is only stable since Rust 1.87 > MSRV.)
        if cmd.len() % 4 != 0 {
            return Err(EngineError::UnalignedCommand { len: cmd.len() });
        }
        // Number of 4-byte words; must fit in the C API's `int`.
        let ndw = cmd.len() / 4;
        let ndw =
            c_int::try_from(ndw).map_err(|_| EngineError::CommandTooLarge { len: cmd.len() })?;

        // Guard the context id the same way. `virgl_renderer_submit_cmd` takes `ctx_id` as a C
        // `int`, and the id originates from the untrusted vtest client; a bare `as c_int` cast on
        // a value above `INT_MAX` would silently wrap to a negative (wrong) context. Convert
        // fallibly and reject out-of-range ids instead of wrapping (Task 1 review follow-up).
        let ctx_id_c =
            c_int::try_from(ctx_id).map_err(|_| EngineError::SubmitCtxIdOutOfRange { ctx_id })?;

        // Copy into a `u32` buffer to guarantee 4-byte alignment regardless of the caller's slice.
        // `Vec<u32>`'s allocation is 4-byte aligned by construction.
        let mut words: Vec<u32> = vec![0; cmd.len() / 4];
        // Reinterpret the word buffer as bytes and fill it from the command slice.
        // SAFETY: `words` holds exactly `cmd.len()` bytes; the pointer is writable and non-null.
        let byte_view =
            unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, cmd.len()) };
        byte_view.copy_from_slice(cmd);

        // Submit. SAFETY: `words` is a live, 4-byte-aligned buffer of `ndw` dwords; virglrenderer
        // never mutates it; `ctx_id_c` was range-checked above to fit a C `int`.
        let rc = unsafe {
            ffi::virgl_renderer_submit_cmd(words.as_mut_ptr() as *mut c_void, ctx_id_c, ndw)
        };
        if rc != 0 {
            return Err(EngineError::SubmitFailed {
                ctx_id,
                rc,
                reason: errno_name(rc),
            });
        }
        Ok(())
    }

    /// Returns the real Venus capset blob for the vtest `VCMD_GET_CAPSET` handshake step.
    ///
    /// Two C calls: `virgl_renderer_get_cap_set(VENUS, &max_ver, &max_size)` learns the blob size,
    /// then `virgl_renderer_fill_caps(VENUS, version, buf)` writes exactly `max_size` bytes into a
    /// buffer we allocate. A `max_size` of 0 means Venus is unsupported here, which we surface as
    /// `NoVenusCapset` (the client would then fail its own init — the honest outcome on a host
    /// without Venus).
    ///
    /// # Failure modes
    /// - `NoVenusCapset`: the renderer reports no Venus capset (no GPU / Venus unavailable).
    fn venus_capset(&mut self, version: u32) -> Result<Vec<u8>, EngineError> {
        // Learn the capset blob size (and max version) from the renderer. `get_cap_set` writes both
        // out-params; a zero `max_size` means the capset is unsupported on this host.
        let mut max_ver: u32 = 0;
        let mut max_size: u32 = 0;
        // SAFETY: both out-params are valid, writable locals; the call only writes through them.
        unsafe {
            ffi::virgl_renderer_get_cap_set(
                ffi::VIRGL_RENDERER_CAPSET_VENUS,
                &mut max_ver,
                &mut max_size,
            )
        };
        // No Venus capability advertised → we cannot answer the handshake with real data.
        if max_size == 0 {
            return Err(EngineError::NoVenusCapset);
        }

        // Allocate exactly the blob the renderer will fill. The vtest wire framing counts the
        // capset in dwords, and `max_size` from virglrenderer is always a multiple of 4, so this
        // buffer frames cleanly; we do not need to pad.
        let mut caps = vec![0u8; max_size as usize];
        // Fill the buffer with the capset for the requested version. SAFETY: `caps` is a live,
        // writable allocation of exactly `max_size` bytes, which is what `fill_caps` writes for
        // this `(set, version)` per the size just returned by `get_cap_set`.
        unsafe {
            ffi::virgl_renderer_fill_caps(
                ffi::VIRGL_RENDERER_CAPSET_VENUS,
                version,
                caps.as_mut_ptr() as *mut c_void,
            )
        };
        Ok(caps)
    }
}

impl Drop for VirglEngine {
    /// Tears the engine down in the correct order and releases the single-instance lock.
    ///
    /// Order matters: destroy every context we created, then `virgl_renderer_cleanup` (which
    /// releases the EGL winsys and reaps the render-server subprocess), and only then release the
    /// global lock so a subsequent `VirglEngine::new` can safely re-initialize. The boxed cookie
    /// is a field, so it is dropped *after* this method returns — i.e. it stays valid through
    /// `virgl_renderer_cleanup`, which is the last C call that could touch it.
    fn drop(&mut self) {
        // Destroy each live context. SAFETY: these ids were returned by successful context
        // creation on the still-initialized renderer; destroy is idempotent-safe here.
        for &ctx_id in &self.contexts {
            unsafe { ffi::virgl_renderer_context_destroy(ctx_id) };
        }

        // Cookie pointer for cleanup — the same one we registered at init.
        let cookie_ptr = (&*self.cookie as *const ffi::Cookie) as *mut c_void;
        // Tear down the global renderer. SAFETY: we hold an initialized engine, so exactly one
        // `virgl_renderer_init` is outstanding; `cookie_ptr` is still valid (the box lives until
        // after this returns).
        unsafe { ffi::virgl_renderer_cleanup(cookie_ptr) };

        // Release the lock last, so the global renderer is fully torn down before another engine
        // may initialize.
        ENGINE_ACTIVE.store(false, Ordering::Release);
    }
}

/// Cheap probe for whether a usable Venus-capable renderer is available on `render_node`, for
/// gating tests (and, later, runtime capability checks). Returns `true` only if BOTH hold:
/// 1. the render node can be opened read/write (a real, permitted GPU render node exists), and
/// 2. virglrenderer reports the Venus capset (4) as supported (non-zero capset size).
///
/// It does not initialize the renderer, so it is safe to call without holding the single-instance
/// lock and cheap enough to call at the top of every GPU test. On a CI runner without a render
/// node (or without Venus), it returns `false` and the tests skip cleanly rather than failing.
///
/// # Inputs / outputs
/// - `render_node`: path to a DRM render node.
/// - Returns `true` if a Venus context is plausibly creatable there, `false` otherwise.
pub fn virgl_available(render_node: &Path) -> bool {
    // (1) Must be able to open the node read/write — the same access `get_drm_fd` needs.
    if std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(render_node)
        .is_err()
    {
        return false;
    }

    // (2) Ask virglrenderer whether Venus (capset 4) is supported. `get_cap_set` reads static
    // capability tables and is safe to call without init. A zero `max_size` means unsupported.
    let mut max_ver: u32 = 0;
    let mut max_size: u32 = 0;
    // SAFETY: both out-params are valid, writable locals; the call only writes through them.
    unsafe {
        ffi::virgl_renderer_get_cap_set(
            ffi::VIRGL_RENDERER_CAPSET_VENUS,
            &mut max_ver,
            &mut max_size,
        )
    };

    // Venus is usable only if the library reports a non-zero capset size for it.
    max_size > 0
}
