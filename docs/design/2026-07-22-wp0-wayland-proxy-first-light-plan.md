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

---

## Task 4 — PREPARED (2026-07-22): the S side, and a reshaping finding

Two investigations mapped the C and S code before decomposing Task 4. The C side is straightforward; the
S side is **larger and riskier than §4 of the spec implied**, because one load-bearing sentence in the
spec is aspirational rather than true.

### The reshaping finding: S does NOT already re-export resources as compositor dma-bufs

Spec §4 leg (b) says "S re-exports the resource as a dma-buf (`rayland-s` already does this for presentation
— `virgl_renderer_resource_export_blob`)". Investigation shows this is **not** how the code works today:

- `virgl_renderer_resource_export_blob` exists (`crates/rayland-engine/src/ffi.rs:431`) with a **private,
  SHM-oriented** wrapper `export_blob_fd` (`crates/rayland-engine/src/virgl.rs:1037`), called in exactly
  one place — at blob *creation* for the `HOST3D` path, to hand the client an mmap fd. There is **no public
  `resource_id → dma-buf` re-export API** callable at present time.
- Live Venus blobs come back as `fd_type = SHM (3)` (used for the CPU readback mapping —
  `crates/rayland-s/src/blob.rs:11,530`), **not** as a `DMABUF (1)` for zero-copy display.
- `rayland-present` is `wl_shm`-only in practice: `rayland-s`'s `BlobFrameSource::supports_dmabuf()` returns
  `false` structurally (`crates/rayland-s/src/present.rs:403`), and `present()` is a **one-static-frame,
  own-the-loop, blocks-until-window-closed** shape (`crates/rayland-present/src/window.rs:689,250-253`) that
  runs *after* the session ends — it cannot drive a persistent, app-animated surface.

So **whether S can turn a `BufferToken.resource_id` into a `wl_buffer` the real compositor displays is
unproven**, and the spec itself deferred confirming it "to Task 4". This is a decision-gate, not a detail:
the swapchain image may export cleanly as a dma-buf, or it may be SHM-only / tiled / DEVICE_LOCAL-invisible
(the (c)2 readback wall), in which case presentation must fall back to a **local readback → `wl_shm`**
present on S. Note **either path keeps WP0's network invariant** (§2, §5): no pixels cross the *network*
either way — the fallback reads back and presents locally on S.

### C-side wiring (low risk — the seams already exist)

- **Sink → link.** Hand the proxy a clone of `Arc<Mutex<QuicSendLink>>` (`main.rs:953`); a `WaylandSink`
  impl does `tx.lock().send(&C2S::WaylandRequest { message })` (`link.rs:101`). `C2S::WaylandRequest` exists.
- **Resolver → inode side-table.** No `(st_dev,st_ino) → res_id` map exists. Build one: `fstat` the memfd
  at `LocalBlob::create` (`shm.rs:116`, the fd is live there and not retained afterward), record
  `(dev,ino) → res_id` in a new `Arc<Mutex<HashMap<(u64,u64),u32>>>` beside the `BlobTable`
  (`relay_engine.rs:66`), and back `ResourceResolver::resolve_inode` with it.
- **Wire the proxy into the daemon.** Spawn a 4th thread (`rayland-c-wayland`) beside the reader/watcher
  spawns (`main.rs:987-1006`); add a `RAYLAND_C1_WAYLAND_DISPLAY` env (none exists yet) via the `env_or`
  pattern (`main.rs:901`). The proxy's `run` is itself a blocking loop, so it wants its own thread.
- **Inbound events.** `reader_thread` (`main.rs:497`) owns `rx.recv()`; add an `S2C::WaylandEvent` arm that
  injects into the proxy. The proxy's poll loop (`serve`, `wayland_proxy.rs:564`) needs a **wakeup fd**
  (eventfd) added so events can be delivered from another thread and flushed to the app via
  `Handle::send_event`. (This is the review's HIGH finding — without it a real vkcube blocks on
  `xdg_surface.configure`.)

### S-side (the new subsystem — `crates/rayland-s/src/wayland_client.rs`)

- **Router.** Split `C2S::WaylandRequest { message }` in the `serve` loop **before** `session.apply(...)`
  (`main.rs:363`) — `apply.rs` deliberately refuses it (`apply.rs:832`). Dispatch into the new client.
- **The client.** A live `wayland-client` connection to S's real compositor holding a **persistent** surface
  + xdg_surface + xdg_toplevel, an `app_id ↔ s_id` object-id map, replaying relayed requests with repeated
  attach/commit, and sending compositor events back as `S2C::WaylandEvent`. Written **fresh** — `present()`'s
  one-shot shape is not reusable, though its dmabuf `create_params`/`add`/`create_immed` mechanics are a
  reference.
- **Token → wl_buffer.** The spike-gated crux (above): resolve `resource_id` → a real `wl_buffer`, either
  via a new public engine dma-buf re-export (zero-copy) or via local readback → `wl_shm` (fallback).
- **Format/modifier.** Present's negotiation is hard-coded XRGB8888+LINEAR at dmabuf v3
  (`window.rs:608,797`); vkcube's real swapchain format/modifier need generalizing or a LINEAR fallback.

### Decomposition (spike-gated)

- **4.0 — dma-buf export spike (GATE).** Can S export a real swapchain-style resource as a `DMABUF` fd the
  compositor imports and shows? Decides zero-copy vs `wl_shm`-readback. Front-loaded, like Task 1's spike.
- **4.1 — C-side wiring.** Link-backed sink + inode→res_id resolver + proxy-as-4th-thread + env var.
  Testable: `C2S::WaylandRequest` reaches S's router (stub).
- **4.2 — S router + object-id map + session replay** against the real compositor (surface/toplevel).
- **4.3 — token → wl_buffer** by the spike's chosen path; attach/commit.
- **4.4 — event return path** (eventfd wakeup + `send_event` + `S2C::WaylandEvent`), incl. `configure`.
- **4.5 — end-to-end:** vkcube's cube on S's screen (Task 5's success criterion folded in).

### Task 4.0 spike OUTCOME (2026-07-22): zero-copy is unreachable; WP0 takes the readback→wl_shm path

The dma-buf export spike (reading virglrenderer 1.2.0 source, the linked version) returned a decisive **no**
for the zero-copy path, and the reason is structural, not a missing implementation:

- virglrenderer fixes a resource's `fd_type` at **creation**, not at export (`virgl_resource.c:104,266-275`;
  `virglrenderer.c:1327-1354`).
- A **guest blob** (`VIRGL_RENDERER_BLOB_MEM_GUEST` — Rayland's `memfd:rayland-blob`, the buffer-by-token
  correlation key) is created as pure guest iovecs with **no host fd** (`virglrenderer.c:1177-1186`,
  `virgl_resource.c:104`). `virgl_renderer_resource_export_blob` on it returns **`-EINVAL`** — not DMABUF,
  not SHM, nothing.
- DMABUF export is real and unstubbed but lives on the **`HOST3D`** (host-allocated) path
  (`vkr_device_memory.c:499-616`), reachable only if the guest allocated with
  `VkExportMemoryAllocateInfo{DMA_BUF}`. Rayland's guest blobs never take that path.

So zero-copy would require re-architecting the swapchain resource from a guest blob to a `HOST3D` blob — a
deep change to the resource model, **out of WP0 scope** (and possibly reopening (c)2's mapped-memory
questions, since HOST3D memory is host-owned). Recorded as a future zero-copy investigation.

**WP0 presentation is therefore readback→`wl_shm`** (the pre-blessed fallback; still no pixels over the
*network*). It reuses proven machinery: the swapchain image is a linear guest-memfd render target (Venus
negotiates LINEAR for wlroots/cosmic compositors), so after the app's submit completes, **S's own mirror of
that memfd holds the finished pixels** — read out completion-gated exactly as the (c)2 G' return path does.

**Revised decomposition (post-spike):**
- **~~4.0 spike~~** — done; outcome above.
- **4.1 — C-side wiring.** Link-backed `WaylandSink` (`Arc<Mutex<QuicSendLink>>` → `C2S::WaylandRequest`),
  inode→res_id `ResourceResolver` (fstat the memfd at `LocalBlob::create`, correlate to `res_id` at
  `commit_pending_blob`), proxy as a 4th daemon thread, `RAYLAND_C1_WAYLAND_DISPLAY` env. Testable: a
  `C2S::WaylandRequest` reaches S.
- **4.2 — S router + persistent Wayland client.** Route `C2S::WaylandRequest` in `serve` before
  `session.apply`; a fresh `wayland_client.rs` holding a live connection to S's compositor, a persistent
  surface + xdg_toplevel, and an `app_id ↔ s_id` object-id map replaying relayed requests.
- **4.3 — token → wl_shm buffer.** On the app's relayed `wl_surface.attach(buffer)`+`commit`, read the
  `BufferToken.resource_id`'s bytes from S's blob mirror (completion-gated via the existing readback gate),
  build a `wl_shm` buffer (reuse `rayland-present`'s shm mechanics), attach+commit to S's real surface.
- **4.4 — event return path.** eventfd wakeup into the proxy's poll loop + `Handle::send_event` +
  `S2C::WaylandEvent`, delivering `xdg_surface.configure` (possibly synthesised) and the frame callbacks a
  real app needs to keep drawing.
- **4.5 — end-to-end.** vkcube's spinning cube in a `wl_shm` window on S, animating.

### Task 4.0-bis OUTCOME (2026-07-22): zero-copy IS viable — measurement overturned the earlier spike

The "zero-copy impossible" conclusion above (Task 4.0) was **wrong**, and is left in place per the diary's
honesty rule. It conflated C's local placeholder memfd with the *S-side* resource. Two source reads plus an
empirical run corrected it:

- **Mesa** (`vn_renderer_vtest.c:760-761`, `wsi_common_drm.c:759-798`, `vn_device_memory.c:285-318`):
  Venus's vtest backend requests the swapchain image's memory as `VCMD_BLOB_TYPE_HOST3D` with
  `VkExportMemoryAllocateInfo{DMA_BUF_BIT_EXT}` **unconditionally**, DEVICE_LOCAL, and **never CPU-maps it**.
- **Rayland** (`virgl.rs:690-758`, `apply.rs:515`): `blob_mem` is honored end-to-end; S creates a real
  HOST3D resource (no iovecs) and calls `export_blob_fd` for it. So the S-side swapchain resource is HOST3D,
  not a guest blob.
- **Empirical** (`RAYLAND_EXPORT_SPIKE` + vkcube over loopback on dop561, NVIDIA A500): resources 1–3
  exported `fd_type=3` (SHM); resources **4–7 exported `fd_type=1` (DMABUF)** — a swapchain's worth of real,
  compositor-importable dma-bufs on S.

**WP0 presentation returns to zero-copy dma-buf** (the design's thesis). Build notes from the measurement:
- The dma-buf export **already happens once at blob creation** (that is what the spike observed), and
  virglrenderer's `mem->exported` guard forbids a second export — so S must **retain** the creation-time
  dma-buf fd for the resource rather than re-export on demand. `create_blob_resource` currently hands that
  fd onward as the "client fd"; on S there is no local vtest client, so the fd must be kept, keyed by
  `resource_id`, for presentation.
- The compositor needs stride/offset/modifier alongside the fd. For the LINEAR swapchain images here these
  are trivial (offset 0, stride = width·bpp) or already carried on the `BufferToken`.

**Revised decomposition (zero-copy):**
- **4.1 — C-side wiring.** (unchanged) link-backed sink, inode→res_id resolver, proxy as 4th thread, env.
- **4.2 — S router + persistent Wayland client** against the real compositor; `app_id ↔ s_id` map.
- **4.3 — token → wl_buffer (zero-copy).** Retain each HOST3D resource's creation-time dma-buf fd keyed by
  `resource_id`; on the app's relayed `attach`+`commit`, resolve `BufferToken.resource_id` → that dma-buf,
  build a `wl_buffer` via S's compositor's `zwp_linux_dmabuf` (reuse `rayland-present`'s dmabuf mechanics),
  attach+commit. Gate the commit on the frame's completion (the existing (c)2 G' signal).
- **4.4 — event return path.** eventfd wakeup + `send_event` + `S2C::WaylandEvent`, incl. `xdg_surface.configure`
  and the dmabuf format/modifier feedback Mesa needs to pick the native (HOST3D-export) WSI path.
- **4.5 — end-to-end.** vkcube's spinning cube on S, zero-copy, animating.
