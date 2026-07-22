# WP0 — Wayland proxy first light — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put vkcube's spinning cube on S's screen, launched on C against a Wayland proxy, with the frame rendered on S (existing command relay) and presented on S via **buffer-by-token** — never shipping presentation pixels.

**Architecture:** A C-side Wayland proxy (the app's `WAYLAND_DISPLAY`) forwards the app's Wayland protocol to a S-side Wayland client over the existing QUIC link; the one special case is buffer creation, where the app's swapchain-image dma-buf is correlated to a Venus resource id (a token) instead of relayed, and S re-exports that already-rendered resource as the `wl_buffer` it attaches to its compositor.

**Tech Stack:** Rust; `rayland-c`/`rayland-s`/`rayland-relay`/`rayland-transport` (existing relay); `wayland-rs` (`wayland-server` to the app, `wayland-client` to S's compositor) — dependency confirmed/finalised in the spike; virglrenderer via `rayland-engine` (resource dma-buf re-export already exists for presentation).

**Spec:** [`2026-07-22-wp0-wayland-proxy-first-light.md`](2026-07-22-wp0-wayland-proxy-first-light.md).

## Global Constraints

- **All Rust.** Our code is 100% Rust; virglrenderer is a linked C dependency behind the `RenderEngine` trait. `wayland-rs` is an external Rust dependency like any other.
- **No Mesa fork.** The proxy sits *above* Mesa, intercepting the app's Wayland wire protocol; it never patches Mesa or virglrenderer.
- **Doc-comment (`///`/`//!`) every function, type, trait, module** — what it does, inputs/outputs, failure modes, domain pitfalls. **Intent comment on every non-trivial line** (the *why*). **Code and comments always agree.**
- **S/C vocabulary:** S = the strong machine (GPU, display, where the user sits); C = where the app runs. The user is at S; the window appears on S; the app runs on C.
- **No pixel path for presentation.** Only Wayland protocol and buffer tokens cross the link; the frame is rendered on S and shown on S. A design that ships readback/pixel `BlobData` for presentation is wrong.
- **Never `pkill`/pattern-kill.** Kill only by an exact captured PID (`cmd & PID=$!`).
- **Scope is WP0-minimal:** one Vulkan app (vkcube), one `xdg_toplevel`, only the Wayland interfaces vkcube binds. Input (WP2), sync/timing polish (WP3), and the interface long tail (WP1) are OUT — stub or ignore anything vkcube does not exercise.
- **The vtest/ring relay is unchanged** and runs alongside the new Wayland channel. The offscreen fixtures (`icosa_cpu`, `refapp`) and their e2e must keep passing.
- **Build/test target dir:** `CARGO_TARGET_DIR=/tmp/rayland-c1-target`. `ssh apollo` is C; this host (dop561) is S with the display.

## Plan shape: the spike gates the rest

Task 1 is a **mechanism spike with a decision gate**. Per spec §7/§9, the buffer-by-token design rests
entirely on the dma-buf↔resource correlation holding; if it does not, §4 of the spec must be revised
before any proxy plumbing is written. Therefore **only Task 1 is specified to bite-sized code depth here.**
Tasks 2–5 are given as a concrete roadmap — files, interfaces, deliverables, and how each is proven — and
are expanded into bite-sized TDD tasks *after* Task 1 confirms the mechanism (and folds in what it learned:
the real correlation key, the exact request flow, the confirmed `wayland-rs` fit). This is deliberate, not
a placeholder: writing fixed code for the proxy before the spike would be fiction.

---

### Task 1: Mechanism spike — confirm dma-buf ↔ resource correlation (GATE)

**Goal:** Prove, by observation, the three legs the whole design stands on, before building anything:
(a) the dma-buf fd the app passes at `zwp_linux_dmabuf.create_immed` for a swapchain image **matches a
Venus resource the vtest relay already tracks**; (b) S **can re-export** that resource as a dma-buf; (c)
that resource **actually receives the app's render on S**. Throwaway instrumentation; no production code.

**Files:**
- Read (no edit): `crates/rayland-c/src/ring.rs`, `blob_sync.rs`, `shm.rs` (how C learns resource ids and
  their fds); `crates/rayland-s/src/apply.rs` (resource/blob tracking), `crates/rayland-present/src` (how S
  already re-exports a resource as a dma-buf); Mesa `wsi_common_wayland` + `vn_wsi.c` (already read — how
  the swapchain image dma-buf becomes the `create_immed` fd).
- Modify (throwaway, env-gated `RAYLAND_WP0_SPIKE`): `rayland-c` — where blob/resource fds are handled,
  log each resource id with its dma-buf fd's identity (`fstat` `st_dev`+`st_ino`).

- [ ] **Step 1: Learn the fd→resource path in `rayland-c`.** Read `ring.rs`/`blob_sync.rs`/`shm.rs` and
  write down, in the plan's progress ledger, exactly where C receives a resource's dma-buf fd (the
  `SCM_RIGHTS` fd on the vtest socket at `CreateBlob`) and how it keys it to a resource id. This is the set
  the correlation will match against.

- [ ] **Step 2: Capture the app-side `create_immed` dma-buf and the vtest resource fds together.** Run
  vkcube through the relay on loopback with the app's Wayland socket observed. Concretely: launch
  `rayland-s` + `rayland-c`; run vkcube under `strace -f -e trace=recvmsg,sendmsg,openat -y` (the `-y`
  prints the path/inode fds resolve to) capturing both the Wayland socket and the vtest socket, and in
  parallel with `WAYLAND_DEBUG=1` to see the `zwp_linux_dmabuf` `create_params`/`create_immed`. Save both.
  ```
  # S + C up (loopback), then:
  WAYLAND_DEBUG=1 VN_DEBUG=vtest VN_PERF=no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback \
    VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json VTEST_SOCKET_NAME=$SOCK \
    env -u VK_LOADER_DRIVERS_SELECT timeout 15 strace -f -y -e trace=recvmsg,sendmsg vkcube --c 3 2>/tmp/wp0-spike.log
  ```

- [ ] **Step 3: Match the fds by identity.** From the capture, take the dma-buf fd(s) `wsi_common_wayland`
  passes at `create_immed` (over the Wayland socket) and the resource dma-buf fd(s) C received over vtest,
  and confirm they resolve to the **same underlying object** (`-y` path, or `fstat` `st_ino`/`st_dev` from
  the env-gated `rayland-c` log). Record the match (or the mismatch) in the ledger.

- [ ] **Step 4: Confirm S re-exports and renders the matched resource.** In `rayland-s`, confirm the
  matched resource id (the swapchain image) is one S created and can re-export as a dma-buf (the
  `rayland-present` export path), and that S's copy receives the app's draws (it is a render target of the
  relayed submits — check the `apply`/blob logs show writes/renders to it).

- [ ] **Step 5: Decision gate.**
  - **All three legs hold** → the design stands. Record the exact correlation key that worked (raw fd
    identity, or a Venus resource-metadata key if fd identity was insufficient), revert the throwaway
    instrumentation, and expand Tasks 2–5 into bite-sized TDD tasks informed by the finding.
  - **A leg fails** → STOP. Record which and why; revise spec §4 (e.g. the correlation must use relay
    resource metadata, or Venus does a prime-blit into a *different* scanout resource that must be tracked)
    before writing any proxy code.

- [ ] **Step 6: Commit the finding** (throwaway instrumentation reverted; the durable output is the ledger
  note + any spec §4 revision):
  ```bash
  git add docs/design/2026-07-22-wp0-wayland-proxy-first-light.md   # only if §4 was revised
  git commit -m "wp0: spike — confirm (or revise) the dma-buf↔resource correlation for buffer-by-token"
  ```

---

## Roadmap for Tasks 2–5 (expanded into bite-sized TDD tasks after Task 1)

Each is a normal walking-skeleton task with an independently checkable deliverable; the step-level TDD
code is written once Task 1 fixes the correlation key and the exact request flow.

### Task 2 — The Wayland relay channel (`rayland-relay`)
- **Create/modify:** `crates/rayland-relay/src/message.rs` — add `C2S::WaylandData { bytes, fds_or_tokens }`
  and `S2C::WaylandData { bytes }` (opaque Wayland wire bytes forward/back), plus a typed
  `BufferToken { resource_id, width, height, format, modifier }` side-band for the intercepted buffer.
- **Deliverable + proof:** unit tests (pure, no GPU/net) round-trip the new messages through the existing
  `postcard` framing; `no_gpu_linkage` guard on `rayland-relay` still passes. Interfaces the later tasks
  consume: the two message arms and `BufferToken`.

### Task 3 — The C-side Wayland proxy (`rayland-c`)
- **Create:** `crates/rayland-c/src/wayland_proxy.rs` — a `wayland-server` listener on a socket named by a
  new env var (`RAYLAND_C1_WAYLAND_DISPLAY`); advertises exactly the spec §6 globals; forwards the app's
  requests as `C2S::WaylandData`; **intercepts `zwp_linux_dmabuf` `create_params`/`create_immed`**,
  correlating the dma-buf fd to a resource id (Task 1's key) and emitting a `BufferToken` instead of the fd;
  applies S's events (`S2C::WaylandData`) back to the app.
- **Deliverable + proof:** with `rayland-s`'s client stubbed to a loopback echo, vkcube connects, binds the
  globals, and reaches `create_immed` producing a correct `BufferToken` (asserted from a log/probe) — no
  `create_immed` assert. Consumes Task 2's messages.

### Task 4 — The S-side Wayland client (`rayland-s`)
- **Create:** `crates/rayland-s/src/wayland_client.rs` — a `wayland-client` connection to S's real
  compositor; replays the forwarded protocol (surface, `xdg_surface`, `xdg_toplevel`); resolves each
  `BufferToken` by re-exporting the S-side resource as a dma-buf (the `rayland-present` export) and creating
  a `wl_buffer` via the compositor's `zwp_linux_dmabuf`; forwards compositor events back as
  `S2C::WaylandData`.
- **Deliverable + proof:** a **window appears on S's compositor** for vkcube's surface (may be a blank/last
  buffer at this task), created and mapped, no protocol error. Consumes Tasks 2–3.

### Task 5 — Present coordination and the moving cube (`rayland-s` + `rayland-c`)
- **Modify:** the S client's attach/commit is gated on the frame's completion (reuse the existing
  return-path completion signal — `Applier::reply_arena_fence_signaled` / the progress thread's per-frame
  signal); frame callbacks throttle the app (may fire promptly in WP0).
- **Deliverable + proof (the WP0 success criterion, spec §8):** `vkcube` on C shows its **spinning cube
  animating in a window on S**, many frames, no wedge; over two machines the window is on dop561 and vkcube
  runs on apollo. Regression: `icosa_cpu` + `refapp` loopback e2e still pass (the vtest/ring relay is
  untouched).

---

## Self-review notes (author)

- **Spec coverage:** spec §2 invariant → Tasks 1,3,4,5; §3 architecture → Tasks 2 (transport),3 (C proxy),4
  (S client); §4 buffer-by-token → Task 1 (confirm) + 3 (token) + 4 (resolve); §6 scope → Task 3 globals;
  §7 spike-first → Task 1 gate; §8 success → Task 5; §9 risks → Task 1 legs + Task 4 modifier fallback.
- **Deliberate depth choice:** only Task 1 is bite-sized code, by design (spec §7 spike gate). This is
  called out above and is not a placeholder; Tasks 2–5 carry concrete files, interfaces, deliverables, and
  proofs, and are expanded post-spike.
- **Naming consistency:** `C2S::WaylandData`, `S2C::WaylandData`, `BufferToken { resource_id, width,
  height, format, modifier }`, `wayland_proxy.rs`, `wayland_client.rs`, `RAYLAND_C1_WAYLAND_DISPLAY`,
  `RAYLAND_WP0_SPIKE` — used identically across tasks.

---

## Task 3b — PREPARED (2026-07-22): the wayland-backend C-side proxy

Investigation of `wayland-backend 0.3.x` (already a transitive workspace dep via `rayland-present`/smithay)
pins the concrete shape. Task 3b builds `crates/rayland-c/src/wayland_proxy.rs`.

**Dependencies to add to `rayland-c`:** `wayland-backend` (server feature), `wayland-server` (core interface
descriptors: `wl_compositor`, `wl_surface`, `wl_buffer`, `wl_callback`, `wl_seat`, `wl_registry`), and
`wayland-protocols` (`xdg_shell` → `xdg_wm_base`/`xdg_surface`/`xdg_toplevel`; `unstable`/`staging`
`linux_dmabuf` → `zwp_linux_dmabuf_v1`/`zwp_linux_buffer_params_v1`). The Interface descriptors are needed
both to advertise globals and so the backend can parse each interface's request signatures.

**Server API (verified in the crate source):**
- `let backend = wayland_backend::server::Backend::<D>::new()?;` → `let handle = backend.handle();`.
- Advertise a global: `handle.create_global(&WlCompositor::interface(), version, Arc::new(ProxyGlobal))`
  for each of the WP0 interfaces; `GlobalHandler::bind(...) -> Arc<dyn ObjectData<D>>` mints the object.
- Accept the app: a `UnixListener` at `RAYLAND_C1_WAYLAND_DISPLAY`; on connect,
  `handle.insert_client(stream, Arc::new(ProxyClientData))`.
- The forward hook: `ObjectData::request(self, handle, data: &mut D, client, msg: Message<ObjectId,OwnedFd>)
  -> Option<Arc<dyn ObjectData<D>>>`. For each request: translate `msg` → `WaylandMessage` (the
  `Argument<ObjectId,OwnedFd>` cases map 1:1 to `WaylandArg`; `Object(id)`/`NewId(id)` carry
  `id.protocol_id()`), forward `C2S::WaylandRequest` to S via the link, and return a fresh
  `Arc<ProxyObjectData>` for the request's `NewId` (so the new object also forwards). **The one special
  case:** an `Argument::Fd` on `zwp_linux_buffer_params_v1.add` — do not forward the fd; `fstat` it
  (`st_dev`+`st_ino`), look the inode up in the resource→memfd map `rayland-c` holds (`shm.rs` owns each
  resource's memfd), and emit `WaylandArg::Buffer(BufferToken{ resource_id, width, height, drm_format,
  modifier })` (the geometry comes from the other `add`/`create_immed` args).
- Deliver S's events: on `S2C::WaylandEvent`, `handle.send_event(Message<ObjectId,RawFd>{..})`.
- Driving loop (a new thread): `poll(handle... backend.poll_fd())` → `backend.dispatch_all_clients(&mut d)`
  → `backend.flush(None)`; wake on either the app socket (poll_fd) or an inbound `S2C::WaylandEvent`.

**Correlation state:** the resource→memfd inode map. `rayland-c` creates each resource's memfd in
`shm.rs`; expose an inode→resource_id lookup (behind the existing send-side mutex or a small shared map) so
the proxy thread can resolve a `params.add` fd. This is the buffer-by-token key confirmed by the Task-1
spike (memfd inode).

**Deferred to Task 4 (S side):** the `app_id ↔ s_id` object-id mapping — Task 3b forwards the app's object
ids as-is against a **stubbed S** (a collector that records the forwarded `WaylandMessage`s + `BufferToken`);
Task 4's S client is what maps them to compositor ids and replays.

**Task 3b deliverable / proof:** vkcube launched against `RAYLAND_C1_WAYLAND_DISPLAY` connects, binds the
WP0 globals, and reaches `create_immed` — with the stub collector showing a correct `BufferToken`
(non-zero `resource_id`, right dimensions) and **no `create_immed` assert** (the proxy consumed the memfd;
no real compositor saw it). Pure pieces get unit tests (Argument↔WaylandArg translation; inode→resource
lookup); the connect-and-bind is the integration proof.

**Risk note:** Task 3b is integration-heavy (new library + threading + correlation). Given a prior subagent
stalled on a simpler task, the controller should drive this directly or brief a subagent in small sub-steps
(deps+skeleton; globals+connect; request-forward+translate; fd→token) each independently checkable.
