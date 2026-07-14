//! Hand-written FFI bindings to the subset of `libvirglrenderer` that Rayland's C0 slice needs,
//! plus the Rust-side callback functions virglrenderer calls back into.
//!
//! # Why hand-written (not bindgen)
//! The surface is tiny (~12 functions, a handful of `#[repr(C)]` structs) and every item here
//! must carry the domain/safety documentation this repository requires on FFI. Hand-writing the
//! bindings keeps that documentation attached to each symbol and keeps the exact, reviewed shape
//! of the C ABI visible in one file. The signatures and struct layouts below are transcribed
//! directly from `/usr/include/virgl/virglrenderer.h` (libvirglrenderer 1.10.0) and are the
//! single source of truth for later C0 tasks.
//!
//! # The one hard constraint: virglrenderer is a process-global singleton
//! None of these functions take a handle. They all operate on one hidden global renderer, so at
//! most one initialized renderer may exist per process at a time. `VirglEngine` enforces that
//! (see `src/virgl.rs`); this module only exposes the raw calls.
//!
//! # Safety discipline
//! All `unsafe` C entry points are declared here; callers in `src/virgl.rs` document the domain
//! reasoning at each call site. The three Rust callbacks below (`write_fence`, `get_drm_fd`,
//! `write_context_fence`) run on virglrenderer's threads and therefore must never unwind across
//! the C boundary — each wraps its body accordingly.

// Raw C integer / pointer types for the ABI.
use std::ffi::{c_char, c_int, c_void};
// Opening a fresh render-node fd inside the `get_drm_fd` callback.
use std::fs::OpenOptions;
// Guard the callback bodies against unwinding into C.
use std::panic::{AssertUnwindSafe, catch_unwind};
// Path to the render node, carried through the C `cookie` pointer.
use std::path::PathBuf;

// ----------------------------------------------------------------------------------------------
// Pinned constants (from virglrenderer.h) — the source of truth for Tasks 2-4.
// ----------------------------------------------------------------------------------------------

/// `VIRGL_RENDERER_USE_EGL` (bit 0). Initialize virglrenderer's own EGL winsys (using the fd our
/// `get_drm_fd` callback hands it) instead of a caller-provided GL context.
pub const VIRGL_RENDERER_USE_EGL: c_int = 1;

/// `VIRGL_RENDERER_USE_SURFACELESS` (bit 3). Use a surfaceless EGL display — there is no window
/// or scanout on S's render node; we only ever render off-screen. Matches the feasibility
/// spike's working `virgl_test_server --venus --use-egl-surfaceless`.
pub const VIRGL_RENDERER_USE_SURFACELESS: c_int = 1 << 3;

/// `VIRGL_RENDERER_VENUS` (bit 6). Enable the Venus renderer — the Vulkan-command replay path
/// that Mesa's Venus ICD on C targets. This is the whole point of the C0 pivot.
pub const VIRGL_RENDERER_VENUS: c_int = 1 << 6;

/// `VIRGL_RENDERER_RENDER_SERVER` (bit 9). Move actual GPU rendering into a forked render-server
/// (vkr proxy) subprocess. **Load-bearing for Venus:** without this flag,
/// `virgl_renderer_context_create_with_flags` with the Venus capset returns `EINVAL` (22). The
/// reliability spike proved this is the missing ingredient that made the throwaway harness look
/// flaky. It also sandboxes the untrusted client's Vulkan away from our process — desirable for
/// Rayland's threat model. virglrenderer reaps the subprocess on `virgl_renderer_cleanup`.
pub const VIRGL_RENDERER_RENDER_SERVER: c_int = 1 << 9;

/// The exact init flag set Rayland uses to bring up a Venus-capable renderer on S's GPU:
/// `USE_EGL | USE_SURFACELESS | VENUS | RENDER_SERVER`. Proven reliable across ≥50 init/teardown
/// cycles in the C0 spike.
pub const RAYLAND_INIT_FLAGS: c_int = VIRGL_RENDERER_USE_EGL
    | VIRGL_RENDERER_USE_SURFACELESS
    | VIRGL_RENDERER_VENUS
    | VIRGL_RENDERER_RENDER_SERVER;

/// The Venus capset id. `virgl_renderer_context_create_with_flags` reads the capset id from the
/// low 8 bits of `ctx_flags` (`VIRGL_RENDERER_CONTEXT_FLAG_CAPSET_ID_MASK == 0xff`). Venus is
/// capset 4 in virtio-gpu's numbering (VIRGL=1, VIRGL2=2, GFXSTREAM=3, **VENUS=4**,
/// CROSS_DOMAIN=5, DRM=6). Confirmed at runtime: `virgl_renderer_get_cap_set(4, ..)` reports a
/// non-zero capset size on this host, while capsets 3/5/6 report zero (unsupported).
pub const VIRGL_RENDERER_CAPSET_VENUS: u32 = 4;

/// `VIRGL_RENDERER_CALLBACKS_VERSION` — the ABI version of the callbacks struct below. Must be 4
/// to match the v4 struct layout virglrenderer 1.x expects.
pub const VIRGL_RENDERER_CALLBACKS_VERSION: c_int = 4;

// ----------------------------------------------------------------------------------------------
// #[repr(C)] structs (layouts transcribed from virglrenderer.h).
// ----------------------------------------------------------------------------------------------

/// Opaque GL-context handle type (`typedef void *virgl_renderer_gl_context;`). We never create GL
/// contexts (Venus is a Vulkan path and we let virglrenderer own its EGL), so this only appears
/// in the unused-by-us callback signatures below, kept for ABI fidelity.
pub type VirglRendererGlContext = *mut c_void;

/// `struct virgl_renderer_gl_ctx_param` — parameters for a caller-provided GL context. We never
/// supply one (our `create_gl_context` callback is null), but the type must exist so the callback
/// pointer's signature matches the C ABI exactly.
#[repr(C)]
pub struct VirglRendererGlCtxParam {
    pub version: c_int,
    pub shared: bool,
    pub major_ver: c_int,
    pub minor_ver: c_int,
    pub compat_ctx: c_int,
}

/// `struct virgl_renderer_callbacks` (version 4). virglrenderer stores the pointer we pass to
/// `virgl_renderer_init` and calls these function pointers on its own threads for the renderer's
/// entire lifetime, so the instance we pass must outlive the renderer (we use a `'static`).
///
/// Field-by-field (every callback is documented; Rayland supplies only three, the rest are null):
/// - `version`: must equal `VIRGL_RENDERER_CALLBACKS_VERSION` (4).
/// - `write_fence`: legacy ctx0 fence signal. Supplied as a no-op (Venus uses per-context fences).
/// - `create_gl_context` / `destroy_gl_context` / `make_current`: for a *caller-provided* GL
///   winsys. Null — we pass `USE_EGL` so virglrenderer initializes its own EGL from `get_drm_fd`.
/// - `get_drm_fd`: **required.** Returns a DRM render-node fd; virglrenderer takes ownership and
///   `close()`s it. Must return a *fresh* fd each call. This is the winsys fd for EGL init.
/// - `write_context_fence`: per-context fence signal (v3+). Supplied as a no-op for now; Task 3/4
///   will wire real fence handling for readback synchronization.
/// - `get_server_fd`: lets the caller supply the render-server socket externally. Null — we let
///   virglrenderer fork and manage the render server itself (`RENDER_SERVER`).
/// - `get_egl_display`: supply a caller-owned EGLDisplay (v4). Null — virglrenderer creates its own.
#[repr(C)]
pub struct VirglRendererCallbacks {
    pub version: c_int,
    pub write_fence: Option<unsafe extern "C" fn(cookie: *mut c_void, fence: u32)>,
    pub create_gl_context: Option<
        unsafe extern "C" fn(
            cookie: *mut c_void,
            scanout_idx: c_int,
            param: *mut VirglRendererGlCtxParam,
        ) -> VirglRendererGlContext,
    >,
    pub destroy_gl_context:
        Option<unsafe extern "C" fn(cookie: *mut c_void, ctx: VirglRendererGlContext)>,
    pub make_current: Option<
        unsafe extern "C" fn(
            cookie: *mut c_void,
            scanout_idx: c_int,
            ctx: VirglRendererGlContext,
        ) -> c_int,
    >,
    pub get_drm_fd: Option<unsafe extern "C" fn(cookie: *mut c_void) -> c_int>,
    pub write_context_fence: Option<
        unsafe extern "C" fn(cookie: *mut c_void, ctx_id: u32, ring_idx: u32, fence_id: u64),
    >,
    pub get_server_fd: Option<unsafe extern "C" fn(cookie: *mut c_void, version: u32) -> c_int>,
    pub get_egl_display: Option<unsafe extern "C" fn(cookie: *mut c_void) -> *mut c_void>,
}

// The struct's fields (a `c_int` and function pointers) are all `Sync`, so it auto-derives
// `Sync` — no manual `unsafe impl` is needed. That auto-`Sync` is what lets us keep the
// callbacks in a `static` (§ below) whose address stays valid for the whole process, which is
// exactly what virglrenderer requires (it retains the pointer past `init` and only ever reads it).

/// `struct virgl_renderer_resource_create_args` — arguments to `virgl_renderer_resource_create`.
/// Not used in Task 1; declared now to pin the exact layout (11 × `u32`, 44 bytes) for the Task 3
/// resource/readback path so later tasks don't re-derive it.
#[repr(C)]
pub struct VirglRendererResourceCreateArgs {
    pub handle: u32,
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
}

/// `struct virgl_renderer_resource_create_blob_args` — arguments to
/// `virgl_renderer_resource_create_blob`. Pinned for Task 3 (blob resources are how Venus shares
/// memory for readback). Not used in Task 1.
#[repr(C)]
pub struct VirglRendererResourceCreateBlobArgs {
    pub res_handle: u32,
    pub ctx_id: u32,
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub blob_id: u64,
    pub size: u64,
    pub iovecs: *const IoVec,
    pub num_iovs: u32,
}

/// Mirror of POSIX `struct iovec` (`{ void *iov_base; size_t iov_len; }`). std does not expose an
/// FFI `iovec`, so we declare our own with the identical C layout. Pinned for Task 3's readback
/// (`transfer_read_iov` and blob resources scatter/gather into these).
#[repr(C)]
pub struct IoVec {
    pub iov_base: *mut c_void,
    pub iov_len: usize,
}

/// Mirror of virglrenderer's `struct virgl_box` (a 3D copy region: origin `x,y,z` + extent
/// `w,h,d`, all `u32`). It is declared in virglrenderer's `virgl_protocol.h`, not the public
/// header, but its layout is stable ABI. Pinned for Task 3's `transfer_read_iov` readback region.
#[repr(C)]
pub struct VirglBox {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

// ----------------------------------------------------------------------------------------------
// The C entry points. Linked via build.rs (`pkg-config` → `-lvirglrenderer`).
// ----------------------------------------------------------------------------------------------

// SAFETY: each declaration transcribes an exported `virgl_renderer_*` prototype from
// virglrenderer.h verbatim. The library is process-global and NOT thread-safe against concurrent
// init/teardown; `VirglEngine` serializes access and enforces the single-instance rule.
unsafe extern "C" {
    /// `int virgl_renderer_init(void *cookie, int flags, struct virgl_renderer_callbacks *cb)`.
    /// Initializes the global renderer. `cookie` is passed back to every callback. Returns 0 on
    /// success, non-zero (an errno) on failure.
    pub fn virgl_renderer_init(
        cookie: *mut c_void,
        flags: c_int,
        cb: *mut VirglRendererCallbacks,
    ) -> c_int;

    /// `void virgl_renderer_cleanup(void *cookie)`. Tears the global renderer down: destroys all
    /// contexts/resources, releases the EGL winsys, and reaps the render-server subprocess. Must
    /// be paired with each successful `virgl_renderer_init`.
    pub fn virgl_renderer_cleanup(cookie: *mut c_void);

    /// `void virgl_renderer_get_cap_set(uint32_t set, uint32_t *max_ver, uint32_t *max_size)`.
    /// Reports the max supported version and blob size of a capset. A capset with `max_size == 0`
    /// is unsupported. Used to probe Venus (capset 4) availability. Safe to call without init.
    pub fn virgl_renderer_get_cap_set(set: u32, max_ver: *mut u32, max_size: *mut u32);

    /// `void virgl_renderer_fill_caps(uint32_t set, uint32_t version, void *caps)`. Fills `caps`
    /// with the capability-set blob for `(set, version)`. The caller must first learn the required
    /// buffer size from `virgl_renderer_get_cap_set` (the `max_size` out-param) and provide a
    /// buffer of at least that many bytes — `fill_caps` writes exactly that many. Used by the
    /// vtest server's `VCMD_GET_CAPSET` handler to answer Mesa's Venus ICD with the real Venus
    /// capset (its `struct virgl_renderer_capset_venus`, which carries the wire-format/protocol
    /// versions the client needs before it will proceed past the handshake).
    pub fn virgl_renderer_fill_caps(set: u32, version: u32, caps: *mut c_void);

    /// `int virgl_renderer_context_create_with_flags(uint32_t ctx_id, uint32_t ctx_flags,
    /// uint32_t nlen, const char *name)`. Creates a rendering context; the low 8 bits of
    /// `ctx_flags` select the capset (4 = Venus). `name`/`nlen` is a debug label (not
    /// NUL-terminated-dependent; `nlen` gives the length). Returns 0 on success, errno otherwise.
    pub fn virgl_renderer_context_create_with_flags(
        ctx_id: u32,
        ctx_flags: u32,
        nlen: u32,
        name: *const c_char,
    ) -> c_int;

    /// `void virgl_renderer_context_destroy(uint32_t handle)`. Destroys a context created above.
    pub fn virgl_renderer_context_destroy(handle: u32);

    /// `int virgl_renderer_submit_cmd(void *buffer, int ctx_id, int ndw)`. Submits a command
    /// buffer of `ndw` 4-byte words to a context. The buffer must be ≥4-byte aligned (violations
    /// return `EFAULT`). Returns 0 on success, errno otherwise. Used by `RenderEngine::submit`.
    pub fn virgl_renderer_submit_cmd(buffer: *mut c_void, ctx_id: c_int, ndw: c_int) -> c_int;

    // ----- Forward-declared for Tasks 2-4 (API pinned now; not exercised in Task 1). -----

    /// `int virgl_renderer_resource_create(struct virgl_renderer_resource_create_args *args,
    /// struct iovec *iov, uint32_t num_iovs)`. Creates a classic (non-blob) resource. Task 3.
    #[allow(dead_code)]
    pub fn virgl_renderer_resource_create(
        args: *mut VirglRendererResourceCreateArgs,
        iov: *mut IoVec,
        num_iovs: u32,
    ) -> c_int;

    /// `int virgl_renderer_resource_create_blob(const struct
    /// virgl_renderer_resource_create_blob_args *args)`. Creates a blob resource (host/guest
    /// shared memory) — the readback path for Venus. Task 3.
    #[allow(dead_code)]
    pub fn virgl_renderer_resource_create_blob(
        args: *const VirglRendererResourceCreateBlobArgs,
    ) -> c_int;

    /// `int virgl_renderer_transfer_read_iov(uint32_t handle, uint32_t ctx_id, uint32_t level,
    /// uint32_t stride, uint32_t layer_stride, struct virgl_box *box, uint64_t offset, struct
    /// iovec *iov, int iovec_cnt)`. Reads rendered pixels out of a resource into host iovecs. Task 3.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub fn virgl_renderer_transfer_read_iov(
        handle: u32,
        ctx_id: u32,
        level: u32,
        stride: u32,
        layer_stride: u32,
        r#box: *mut VirglBox,
        offset: u64,
        iov: *mut IoVec,
        iovec_cnt: c_int,
    ) -> c_int;

    /// `void virgl_renderer_ctx_attach_resource(int ctx_id, int res_handle)`. Makes a resource
    /// visible to a context. Task 3.
    #[allow(dead_code)]
    pub fn virgl_renderer_ctx_attach_resource(ctx_id: c_int, res_handle: c_int);

    /// `void virgl_renderer_context_poll(uint32_t ctx_id)`. Forces retirement of a context's
    /// fences (drives fence callbacks). Task 4 (fence/synchronization).
    #[allow(dead_code)]
    pub fn virgl_renderer_context_poll(ctx_id: u32);

    /// `int virgl_renderer_context_get_poll_fd(uint32_t ctx_id)`. Returns an fd that becomes
    /// readable when the context has fences to retire (for event-loop integration). Task 4.
    #[allow(dead_code)]
    pub fn virgl_renderer_context_get_poll_fd(ctx_id: u32) -> c_int;

    /// `int virgl_renderer_get_poll_fd(void)`. Global poll fd used with `THREAD_SYNC`. Task 4.
    #[allow(dead_code)]
    pub fn virgl_renderer_get_poll_fd() -> c_int;

    /// `int virgl_renderer_create_fence(int client_fence_id, uint32_t ctx_id)`. Creates a legacy
    /// ctx0-style fence. Task 4 (readback completion). Returns 0 on success, errno otherwise.
    #[allow(dead_code)]
    pub fn virgl_renderer_create_fence(client_fence_id: c_int, ctx_id: u32) -> c_int;

    /// `void virgl_renderer_force_ctx_0(void)`. Forces the underlying GL/EGL context back to the
    /// renderer's ctx0. A known knob for winsys-state hygiene between operations; pinned in case
    /// Task 3/4 needs it, though the spike proved reliability without it.
    #[allow(dead_code)]
    pub fn virgl_renderer_force_ctx_0();
}

// ----------------------------------------------------------------------------------------------
// The Rust callbacks virglrenderer calls back into, and the cookie that carries state to them.
// ----------------------------------------------------------------------------------------------

/// State handed to virglrenderer as the opaque `void *cookie` and passed back to every callback.
/// It carries the render-node path so `get_drm_fd` can open a fresh fd on demand. It is heap-boxed
/// and owned by the `VirglEngine`, whose lifetime brackets the renderer's, so the pointer stays
/// valid for every callback invocation.
pub struct Cookie {
    /// The DRM render node (e.g. `/dev/dri/renderD128`) to open in `get_drm_fd`.
    pub render_node: PathBuf,
}

/// `write_fence` callback: legacy ctx0 fence retirement notification. Rayland does not use ctx0
/// fencing (Venus signals via per-context fences), so this is a deliberate no-op. It must not
/// unwind into C; a bare no-op cannot panic, so no guard is needed.
///
/// # Safety
/// Called by virglrenderer on its own thread with the `cookie` we registered. Parameters are
/// unused.
unsafe extern "C" fn write_fence(_cookie: *mut c_void, _fence: u32) {
    // Intentionally empty: no ctx0 fence handling in Rayland.
}

/// `write_context_fence` callback: per-context fence retirement notification (v3+). A no-op for
/// now; Task 3/4 will wire real readback synchronization here. Cannot panic, so no unwind guard.
///
/// # Safety
/// Called by virglrenderer on its own thread. Parameters are unused for now.
unsafe extern "C" fn write_context_fence(
    _cookie: *mut c_void,
    _ctx_id: u32,
    _ring_idx: u32,
    _fence_id: u64,
) {
    // Intentionally empty: fence-driven readback is a later task.
}

/// `get_drm_fd` callback: returns a DRM render-node file descriptor for virglrenderer's EGL
/// winsys initialization.
///
/// # Ownership contract (critical — this is the spike's fix)
/// virglrenderer **takes ownership** of the returned fd and `close()`s it itself, and may call
/// this more than once, so we must open a **fresh** fd every call — never hand back a cached or
/// shared descriptor. Getting this wrong (handing back one fd that virglrenderer then closes) is
/// exactly the kind of DRM/EGL lifecycle bug that made the throwaway `virgl_test_server` harness
/// look flaky across repeated init/teardown.
///
/// Rust's `File` always opens with `O_CLOEXEC`, matching the C harness's `O_RDWR | O_CLOEXEC`.
/// We hand the fd to C via `into_raw_fd`, which relinquishes Rust's ownership so `File`'s `Drop`
/// does not also close it.
///
/// # Returns
/// A non-negative fd on success, or `-1` if the node cannot be opened (virglrenderer treats a
/// negative return as failure and aborts winsys init, which surfaces as an init error).
///
/// # Safety
/// Called by virglrenderer with the `cookie` we registered (a `*const Cookie`). The body is
/// wrapped in `catch_unwind` because unwinding across the C boundary is undefined behavior.
unsafe extern "C" fn get_drm_fd(cookie: *mut c_void) -> c_int {
    // Bring the fd-ownership transfer trait into scope.
    use std::os::fd::IntoRawFd;

    // Guard against any panic escaping into C (UB). `AssertUnwindSafe` is justified: the only
    // captured state is a raw pointer we read once, with no broken invariant on unwind.
    let result = catch_unwind(AssertUnwindSafe(|| {
        // A null cookie would be a programming error on our side; fail closed rather than deref.
        if cookie.is_null() {
            return -1;
        }
        // Recover the render-node path from the cookie virglrenderer handed back to us.
        // SAFETY: `cookie` is the `*const Cookie` we registered in `virgl_renderer_init`, and the
        // owning `VirglEngine` keeps the `Cookie` alive for the renderer's whole lifetime.
        let cookie = unsafe { &*(cookie as *const Cookie) };
        // Open a brand-new read/write fd on the render node. read+write is required: EGL/DRM
        // needs to submit work, not just query. `File` sets `O_CLOEXEC` automatically.
        match OpenOptions::new()
            .read(true)
            .write(true)
            .open(&cookie.render_node)
        {
            // Relinquish Rust ownership so only virglrenderer will `close()` this fd.
            Ok(file) => file.into_raw_fd(),
            // Node unavailable (absent, or permission denied): signal failure to virglrenderer.
            Err(_) => -1,
        }
    }));

    // On a caught panic, report failure rather than propagating across FFI.
    result.unwrap_or(-1)
}

/// The single, process-`static` callbacks struct handed to `virgl_renderer_init`. virglrenderer
/// keeps this pointer and calls through it for the renderer's lifetime, so it must outlive every
/// engine — a `'static` is the simplest correct storage. Only the three callbacks Rayland
/// implements are set; the rest are null (see `VirglRendererCallbacks` docs for why each is safe
/// to omit).
pub static RAYLAND_CALLBACKS: VirglRendererCallbacks = VirglRendererCallbacks {
    version: VIRGL_RENDERER_CALLBACKS_VERSION,
    write_fence: Some(write_fence),
    create_gl_context: None,
    destroy_gl_context: None,
    make_current: None,
    get_drm_fd: Some(get_drm_fd),
    write_context_fence: Some(write_context_fence),
    get_server_fd: None,
    get_egl_display: None,
};
