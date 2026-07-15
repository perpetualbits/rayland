# C0 — Venus First Light — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove a real, unmodified headless Vulkan app on C, captured by Mesa's Venus ICD, replays correctly on S's real GPU through a Rayland-owned host that FFI-embeds `libvirglrenderer` — verified by reading back the rendered image to a PNG. Same machine, local socket.

**Architecture:** A new `rayland-engine` crate FFI-binds the ~12 `virgl_renderer_*` functions it needs, behind a clean `RenderEngine` trait (`VirglEngine` impl holds all the `unsafe`). A host binary implements enough of the **vtest** wire protocol that Mesa's Venus ICD speaks over a Unix socket, drives a virglrenderer **venus-capset** context, replays the stream on the GPU, reads back the result, and writes a PNG. The captured workload is a small unmodified offscreen Vulkan reference app.

**Tech Stack:** Rust edition 2024; FFI to `libvirglrenderer` **1.2.0** (`virgl_renderer_*` C API; `bindgen` or hand-written bindings); Mesa 26 Venus ICD on the client side (unmodified); `ash` for the reference app (already in-tree); `image` for the PNG (already in-tree).

> **Corrected 2026-07-15 (Task 4d).** This line originally said "1.10". **There is no virglrenderer 1.10** — the installed package is `libvirglrenderer-dev 1.2.0-2ubuntu2`, and `pkg-config --modversion virglrenderer` reports **1.2.0**. The false version was invented, propagated into `ffi.rs`'s module docs, and corrected there in Task 4a; this was the last surviving copy in the repository.

## Global Constraints

- **Edition:** `edition = "2024"`, `rust-version = "1.85"`.
- **This is research-heavy: the API and protocol are pinned by spikes, not assumed.** The exact `libvirglrenderer` venus calls and the vtest wire framing are discovered against the real library/sources in Tasks 1–2 and then used by later tasks — as SP2's crypto spike and SP3's dmabuf spike pinned theirs. Where this plan names a symbol/struct, verify it against the installed headers and the compiler; record deviations.
- **The FFI boundary is confined and checked.** All `unsafe` C-FFI lives in `VirglEngine`; every `virgl_renderer_*` return code is checked and mapped to a typed error — a C error must never become a silent success. No `unwrap`/`expect` on runtime-fallible non-test paths (`expect` in tests OK).
- **Errors:** `rayland-engine` is a library → `thiserror` + `LGPL-3.0-or-later`; the host binary → `anyhow` + `GPL-3.0-or-later`.
- **Comments:** doc-block on every fn/type/module; intent comment on every non-trivial line (the *why*, and safety/domain reasoning at the FFI boundary); code and comments must always agree.
- **Reliability is a first-class requirement** (the spike's warning): the engine must init/replay/teardown a venus context **repeatedly** without the flakiness `virgl_test_server` showed.
- **CI stays light:** all GPU/DRM/venus-dependent tests **skip cleanly** when a render node / virglrenderer / Venus is unavailable (as the SP3 dmabuf tests do). No new system-lib pull affects CI unless a task explicitly adds it.
- **Verify against the real library + GPU, not the IDE.** This host filters software Vulkan globally; the engine uses the real GPU render node (`/dev/dri/renderD128`). The (c)0 spike's recipe and gotchas are recorded in project memory ([[project-rayland]]): `VK_ICD_FILENAMES=…/virtio_icd.json` + the vtest socket on the client; `env -u VK_LOADER_DRIVERS_SELECT`; validation-layer + Venus don't mix; a render node is required.

---

## File Structure

- `crates/rayland-engine/` — **new library crate.** `Cargo.toml`, `build.rs` (link `virglrenderer`; optionally run `bindgen`), `src/lib.rs` (the `RenderEngine` trait + `EngineError`), `src/ffi.rs` (the raw `virgl_renderer_*` bindings), `src/virgl.rs` (`VirglEngine`: the safe FFI wrapper + reliability-correct context lifecycle).
- `crates/rayland-engine/src/vtest.rs` — **new (Task 2).** The vtest wire-protocol server: accepts a byte stream, frames the commands Mesa's Venus ICD emits, and drives the `RenderEngine`.
- `crates/rayland-engine-host/` — **new binary crate (Task 4).** Wires a Unix-socket listener → `vtest` server → `VirglEngine` → readback → PNG.
- `crates/rayland-refapp/` — **new (Task 4).** The small unmodified offscreen Vulkan reference app (draws a known triangle to an image, reads back), usable natively and as the captured workload.
- `Cargo.toml` (workspace) — **modify.** Add the new members + any `[workspace.dependencies]`.
- `docs/c0-venus-first-light.md` — **new (Task 4).**

---

## Task 1: `rayland-engine` FFI foundation + venus-context reliability spike (DE-RISK FIRST)

The load-bearing task: FFI-bind the virglrenderer calls we need, bring up a **venus-capset context**, and prove it initializes/tears down **reliably and repeatedly** (the flakiness the feasibility spike hit — the open question is whether it lives in the library or only in `virgl_test_server`). This task also **pins the exact API** for Tasks 2–4.

**Files:**
- Create: `crates/rayland-engine/Cargo.toml`, `build.rs`, `src/lib.rs`, `src/ffi.rs`, `src/virgl.rs`
- Modify: `Cargo.toml` (workspace: add the member)

**Interfaces (produced for Tasks 2–4; exact signatures pinned here):**
- `pub trait RenderEngine` — `fn create_venus_context(&mut self, ctx_id: u32) -> Result<(), EngineError>`; `fn submit(&mut self, ctx_id: u32, cmd: &[u8]) -> Result<(), EngineError>`; resource/readback methods (finalized in Task 3). `pub enum EngineError` (thiserror).
- `pub struct VirglEngine` implementing it; `VirglEngine::new(render_node: &Path) -> Result<VirglEngine, EngineError>` (calls `virgl_renderer_init`).
- `pub fn virgl_available(render_node: &Path) -> bool` — cheap probe for test-gating.

- [ ] **Step 1: Install headers and add the crate**

Run: `sudo apt-get install -y libvirglrenderer-dev` (provides `virglrenderer.h`). Record the header path.
Add `"crates/rayland-engine",` to the workspace `members`. Create `crates/rayland-engine/Cargo.toml` (library, `license = "LGPL-3.0-or-later"`, edition 2024, `rust-version = "1.85"`; deps: `thiserror` (workspace), `anyhow` (workspace, for the setup surface); build-deps: `bindgen` (or none if hand-writing) + `pkg-config`).

`build.rs`: link `virglrenderer` (`println!("cargo:rustc-link-lib=virglrenderer")`, and `pkg_config` to find it), and — if using bindgen — generate bindings for `virglrenderer.h`. If hand-writing bindings instead, `build.rs` only emits the link directive.

- [ ] **Step 2: Bind the minimal virgl_renderer_* surface**

In `src/ffi.rs`, expose (via bindgen or hand-written `extern "C"`) exactly the functions C0 needs (confirmed present in the installed `libvirglrenderer.so`): `virgl_renderer_init`, `virgl_renderer_cleanup`, `virgl_renderer_get_cap_set`, `virgl_renderer_context_create_with_flags` (the **venus** context — the flag/capset id for venus is discovered from `virglrenderer.h`; document it), `virgl_renderer_context_destroy`, `virgl_renderer_submit_cmd`, `virgl_renderer_resource_create` / `_create_blob`, `virgl_renderer_transfer_read_iov`, `virgl_renderer_ctx_attach_resource`, `virgl_renderer_context_poll` / `get_poll_fd`, `virgl_renderer_create_fence`. The `virgl_renderer_init` callback struct (`virgl_renderer_callbacks`) must be provided — pin its exact shape from the header and document each callback. **Record the pinned API in the task report** (later tasks depend on it).

- [ ] **Step 3: `VirglEngine::new` + a venus context, with correct lifecycle**

Implement `VirglEngine::new(render_node)`: open the render node fd, `virgl_renderer_init` with the venus-capable flags + the callbacks struct, checking the return code. Implement `create_venus_context` via `virgl_renderer_context_create_with_flags` with the venus capset. Implement `Drop` to `virgl_renderer_context_destroy` + `virgl_renderer_cleanup` in the correct order. **The flakiness de-risk lives here:** get the init/teardown ordering and fd/EGL lifecycle right so repeated cycles are reliable.

- [ ] **Step 4: The reliability test (the spike's core)**

Write a test that, when `virgl_available()` (else skip cleanly), performs **≥20 iterations** of: `VirglEngine::new` → `create_venus_context` → drop, asserting every iteration succeeds. Add a second test doing many contexts within one `VirglEngine`. Run:
`cargo test -p rayland-engine -- --nocapture` on the real GPU.
Expected: all iterations succeed (reliable). **If it flakes like `virgl_test_server` did**, diagnose the lifecycle (init flags, fd ownership, EGL/DRM teardown, `force_ctx_0`) until reliable; if it proves unfixable at the library level, STOP and report BLOCKED with the evidence — that is the trigger to reconsider the engine (surface to the owner). Under a no-GPU selector, the test skips.

- [ ] **Step 5: Lints, workspace green, commit**

`cargo clippy -p rayland-engine -- -D warnings`; `cargo fmt`; `cargo test --workspace` (existing SP0–SP3 tests unaffected; the new tests pass-or-skip). Then:
```bash
git add crates/rayland-engine Cargo.toml
git commit -m "C0 Task 1: rayland-engine FFI foundation + venus-context reliability spike"
```

---

## Task 2: The vtest protocol server

Implement enough of the vtest wire protocol (what Mesa's Venus ICD emits) to accept its connection, frame its messages, and drive the `RenderEngine`. **API/framing discovered here against the Mesa/virglrenderer sources**, then implemented.

**Files:**
- Create: `crates/rayland-engine/src/vtest.rs`
- Modify: `crates/rayland-engine/src/lib.rs` (`pub mod vtest;`)

**Interfaces:**
- Consumes: `RenderEngine` (Task 1).
- Produces: `pub fn serve_vtest<S: Read + Write>(stream: S, engine: &mut dyn RenderEngine) -> Result<VtestOutcome, EngineError>` — reads vtest messages from `stream` and drives `engine` until the client finishes; `VtestOutcome` carries the id of the rendered resource for readback (Task 3). Generic over the byte stream so (c)1 can pass a QUIC stream instead of a Unix socket.

- [ ] **Step 1: Discover and document the vtest framing**

Read the installed Mesa Venus vtest client (`src/virtio/vulkan/vn_renderer_vtest.c` region) and virglrenderer's vtest server to pin: the message header (length + command id), the handshake (protocol version — the spike saw a "vtest protocol version too old" path), the `VCMD_*` opcodes C0 must handle (context create, resource create, transfer/submit-cmd, sync/fence), and how the venus command payload is framed inside `VCMD_SUBMIT_CMD`. **Document the exact framing in the task report.** (Fetch the matching Mesa/virglrenderer source by version if not on disk.)

- [ ] **Step 2: Implement the minimal server**

Implement `serve_vtest`: the handshake, the read loop, and handlers for the minimal opcode set — routing context-create → `engine.create_venus_context`, submit-cmd → `engine.submit`, resource-create → the engine's resource path, and sync → the engine's fence path. Unhandled/optional opcodes: respond per protocol or error clearly (no silent drops). Frame every read defensively (length-checked, like `rayland-wire`'s `MAX_FRAME_BYTES`).

- [ ] **Step 3: Test the framing in isolation**

Unit-test the message framing/parsing with captured/synthetic vtest byte sequences (assert opcodes and payloads round-trip/parse correctly) — no GPU needed, so it runs in CI. The live end-to-end drive is Task 4.

- [ ] **Step 4: Lints + commit**

`cargo clippy -p rayland-engine -- -D warnings`; `cargo fmt --check`; `cargo test -p rayland-engine`. Then:
```bash
git add crates/rayland-engine/src
git commit -m "C0 Task 2: vtest protocol server driving the RenderEngine"
```

---

## Task 3: Replay → rendered resource → readback (`RenderedFrame`)

Finish the `RenderEngine` resource/readback surface so a replayed venus stream yields CPU pixels.

**Files:**
- Modify: `crates/rayland-engine/src/{lib.rs,virgl.rs,vtest.rs}`

**Interfaces:**
- Produces: `RenderEngine::read_back(&mut self, resource_id: u32) -> Result<EngineFrame, EngineError>` where `EngineFrame { width, height, pixels: Vec<u8>, format }` (tightly-packed RGBA/BGRA — format pinned from the resource info). Reuses SP0's readback discipline (honor row stride).

- [ ] **Step 1: Resource creation + attach + readback**

Implement the resource path in `VirglEngine` (`virgl_renderer_resource_create`/`_create_blob`, `ctx_attach_resource`) so the venus context's framebuffer target exists, and `read_back` via `virgl_renderer_transfer_read_iov` into a CPU buffer, honoring the resource's stride (`virgl_renderer_resource_get_info`). Fence-wait for replay completion (`virgl_renderer_create_fence` + poll) before readback — the correctness point (analogous to SP0/SP3's fence-wait).

- [ ] **Step 2: Wire `VtestOutcome` → readback**

Have `serve_vtest` record the rendered resource id, and expose it so the host (Task 4) can call `read_back`.

- [ ] **Step 3: Lints + commit** (no standalone GPU test here — exercised end-to-end in Task 4)
```bash
git add crates/rayland-engine/src
git commit -m "C0 Task 3: engine resource creation + fence-waited readback to EngineFrame"
```

---

## Task 4: Host binary + reference app + end-to-end PNG + docs

Assemble the host, the captured workload, the machine-verified end-to-end test, and the doc.

> **Amended 2026-07-15 — Task 4 was SPLIT into 4a/4b/4c/4d, and was not executable as one unit.**
> It carried three unresolved carry-forwards and a false premise (Step 2, below). The split as
> executed:
>
> | | what it did | status |
> |---|---|---|
> | **4a** | SCM_RIGHTS fd-passing; byte-verified `SUBMIT_CMD2` against a live Venus client; first live Vulkan app through the engine | **done** (`75e79f4`) |
> | **4b** | The reference app; C0's headline proof (PNG bit-identical to native, 15/15); the five research answers | **done** (`b6544f3`) |
> | **4c** | Bounded host-side frame-extraction spike (can the engine find/export the app's Venus-internal `VkImage`?) | **DEFERRED by the owner — not done.** An open question (c)1/SP1 must close, since putting a frame on a screen requires host-side pixels. |
> | **4d** | These docs + the corrections C0's findings forced | **this change** |
>
> A ring-decoder cleanup landed between 4a and 4b (`9ab55b7`), moving the ring discovery out of
> git-excluded scratch into `src/venus_ring/` as a CI fixture test.
>
> **Step 2's premise is false and was superseded** (owner decision, Task 4b): the host does **not**
> `read_back` a rendered resource and write a PNG, because **there is no such resource to read** —
> the app's `VkImage` is created by Venus commands inside the ring and never enters our resource
> table, a `DEVICE_LOCAL` image produces no blob at all, and blob resources have no queryable
> format. The **app** does its own `vkMapMemory` readback and writes the PNG. `rayland-engine-host`
> was **not built**: the existing `examples/vtest_serve` harness covers the live drive, and a binary
> would only have duplicated it. See the spec's Amendment 1.

**Files:**
- Create: `crates/rayland-engine-host/` (Cargo.toml + src/main.rs), `crates/rayland-refapp/` (Cargo.toml + src/main.rs)
- Create: `crates/rayland-engine/tests/e2e_venus.rs`, `docs/c0-venus-first-light.md`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: The reference app** (`rayland-refapp`) — a small **offscreen** Vulkan app (ash): render a known triangle (red on blue, matching SP0's colours for easy comparison) into an image, read it back, and either print/save the pixels. It must be a *normal* Vulkan program (no Rayland awareness) so Venus captures it transparently. Add a unit test asserting its **native** output (centre red, corners blue) — so an e2e failure localizes to the engine, not the app.

- [ ] **Step 2: The host** (`rayland-engine-host`) — bind a Unix socket at a given path; on connection, `serve_vtest(stream, &mut VirglEngine)`, then `read_back` the rendered resource and `image::save_buffer` a PNG (like SP0's server). Log clearly. `anyhow`.

- [ ] **Step 3: End-to-end test** (`tests/e2e_venus.rs`, GPU-gated) — if `!virgl_available()` skip cleanly; else: start the host on a temp socket (background thread), run `rayland-refapp` as a subprocess with `VK_ICD_FILENAMES=…/virtio_icd.json`, `VTEST_SOCKET_NAME=<sock>`, and `env -u VK_LOADER_DRIVERS_SELECT`; wait; assert the produced PNG's pixels match the reference app's native output (centre red, corners blue). This is C0's headline proof: a real unmodified Vulkan app replayed on the GPU through our host.

- [ ] **Step 4: Verify (GPU) + lints**

`cargo test -p rayland-engine --test e2e_venus -- --nocapture` on the real GPU → PASS (PNG pixels correct). `cargo test --workspace` green (new GPU tests pass-or-skip; SP0–SP3 unaffected). `cargo clippy --workspace -- -D warnings`; `cargo fmt --check`. **Run the e2e several times** to confirm the reliability fix from Task 1 holds end-to-end.

- [ ] **Step 5: Docs** — `docs/c0-venus-first-light.md`: what C0 proves (a real unmodified Vulkan app runs, replayed on S's GPU via our virglrenderer host — the graduation from the hand-emitted triangle); the exact run commands; the PNG; that this is same-machine/local-socket and (c)1 adds QUIC + the dmabuf window; the reliability note; the deferred pieces and the arc map.

- [ ] **Step 6: Commit**
```bash
git add crates/rayland-engine-host crates/rayland-refapp crates/rayland-engine/tests docs/c0-venus-first-light.md Cargo.toml
git commit -m "C0 Task 4: engine host + reference app + end-to-end venus render PNG test + docs"
```

---

## Self-Review

**1. Spec coverage** — every C0 spec section maps to a task:
- §1 success criterion (real app → Venus → host → replay → PNG; reliability) → Task 4 e2e + Task 1 reliability spike.
- §2 scope/non-goals (local socket, PNG-not-screen, small ref app, no QUIC/coherence/assets) → respected; documented in Task 4 doc.
- §3 architecture (C = unmodified Venus ICD; S = rayland-engine host) → Tasks 1–4.
- §4 FFI boundary (RenderEngine trait + VirglEngine) → Task 1.
- §5 reliability de-risk → Task 1 Step 4 (the gate).
- §6 vtest transport seam (generic byte stream for (c)1) → Task 2 (`serve_vtest<S: Read+Write>`).
- §7 output extraction (readback → PNG) → Task 3 + Task 4.
- §8 testing (reliability spike, e2e GPU-gated skip-clean, SP0–SP3 unchanged) → Tasks 1–4.
- §9 errors/deps/licenses → all tasks (thiserror lib/anyhow bin; libvirglrenderer FFI).
- §12 API/protocol-discovery spikes → Task 1 Step 2 (virgl API) + Task 2 Step 1 (vtest framing).

**2. Placeholder scan** — the intentional discovery points are Task 1 Step 2 (pin the venus capset/callbacks from the header) and Task 2 Step 1 (pin the vtest framing from Mesa source) — genuine spikes that record their findings for later tasks, matching SP2/SP3. No lazy placeholders; every step names concrete symbols/files/assertions.

**3. Type consistency** — `RenderEngine` trait (Task 1) is consumed by `serve_vtest` (Task 2) and the host (Task 4); `VirglEngine` implements it; `EngineFrame`/`read_back` (Task 3) feed the host's PNG (Task 4); `virgl_available()` gates every GPU test. `serve_vtest<S: Read+Write>` is generic so (c)1 swaps the Unix socket for a QUIC stream unchanged.

> **Amended 2026-07-15 — the final sentence is SUPERSEDED and was the plan's most consequential error.**
>
> **`serve_vtest<S: Read + Write>` is not "generic so (c)1 swaps in QUIC unchanged".** A live Venus
> client **cannot work at all** without **`SCM_RIGHTS` fd-passing**: the protocol replies to
> `VCMD_RESOURCE_CREATE_BLOB` with a **file descriptor**, which is how the client maps the shared
> memory it writes every command into. A bare `Read + Write` cannot carry a file descriptor, so the
> generic bound did not abstract the transport — **it hid the one thing that does not port.**
>
> The signature is now `serve_vtest<T: VtestTransport>`, where
> `trait VtestTransport: Read + Write { fn send_fd(&mut self, fd: BorrowedFd) -> …; }` and the
> `UnixStream` impl does a real `sendmsg`/`SCM_RIGHTS`. **`send_fd` is a required trait method
> deliberately**, so a future QUIC transport confronts the gap **at compile time** rather than
> inheriting a broken assumption silently.
>
> The deeper point (spec Amendment 2): QUIC has no fd-passing and two machines cannot share a page,
> so **(c)1 is a protocol design task, not a transport substitution.** Also note `EngineFrame`/
> `read_back` do **not** feed the host's PNG — nothing does; see the Task 4 amendment.

**Note for the executor — this is the most research-forward plan in the project.** Tasks 1–2 discover real, currently-unread API/protocol against the installed library and Mesa sources; treat their reports as the source of truth for Tasks 3–4, and expect to refine Task 3/4 details once Tasks 1–2 land. If Task 1's reliability spike or Task 2's framing discovery reveals the vtest/venus library path is impractical, that is a real finding to surface — the fallback (vendor `virgl_test_server`'s core, or reconsider the engine) is a decision for the owner, not something to force.
