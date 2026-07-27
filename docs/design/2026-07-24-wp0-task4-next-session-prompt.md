# Next-session prompt — WP0 Task 4.3 / 4.4 (the Wayland-proxy return path)

**You are picking up Rayland's WP0 sub-project mid-flight.** This document is your entry point. Read it
fully, then read the files it names. It is written for a fresh session with no prior context.

---

## 0. Orient yourself first (do this before touching anything)

Read, in order:

1. `CLAUDE.md` — the project's binding conventions. Note especially: the **S / C vocabulary** (S = the
   strong machine where the user sits, with GPU + display + compositor; C = the weak machine where the app
   runs), the **doc-comment-on-everything / intent-comment-on-every-nontrivial-line** code rules, and the
   **development diary** rule: *add an entry to `docs/DIARY.md` on every working turn.*
2. `docs/DIARY.md` — the last ~8 entries (dated 2026-07-22 to 2026-07-24) are the WP0 story, including the
   spike that concluded "zero-copy impossible" and the **very next entry that overturns it by measurement**.
   Read both; the reversal is load-bearing.
3. `docs/design/2026-07-22-wp0-wayland-proxy-first-light.md` — the WP0 spec (what/why).
4. `docs/design/2026-07-22-wp0-wayland-proxy-first-light-plan.md` — the plan. Read the **"Task 4.0-bis
   OUTCOME"** section (zero-copy confirmed viable) and the **"Revised decomposition (zero-copy)"** at the
   end: tasks 4.1–4.5.
5. `.superpowers/sdd/progress.md` — the SDD ledger (git-ignored scratch). Its tail has the full per-task
   resume state, including the exact NEXT steps. **Trust this over your own recollection.**

Then `git log --oneline -20` and skim the `wp0(...)` commits.

---

## 1. What Rayland/WP0 is (one paragraph)

Rayland runs an unmodified Vulkan app on **C** and renders+displays it on **S** by relaying GPU commands
(not pixels) across the network. The forward render path (Venus/virglrenderer command relay) already works.
**WP0** is the presentation piece: a real app like **vkcube** presents through Wayland, and its swapchain
`wl_buffer` is an invalid virtio-gpu dma-buf because Rayland has no virtio-gpu display path — so WP0 puts a
**Wayland proxy** on C (the app connects to it, not a real compositor), forwards the app's Wayland protocol
to S, and displays on S. The one thing that cannot cross the network — the swapchain buffer's fd — is
replaced by a **buffer-by-token** naming the S-side resource the command relay already rendered. **No pixels
cross the network for presentation.**

---

## 2. What is DONE (Tasks 4.0–4.2, all committed, all tests green)

The **entire forward direction of the Wayland tunnel works end-to-end**: app → C proxy → QUIC link → S → S's
real compositor. Concretely:

- **4.0-bis (MEASURED, decisive):** the swapchain images **export as real dma-bufs** (`fd_type=1`) on S.
  Zero-copy presentation is viable — the design's original thesis holds. (An earlier spike wrongly concluded
  "impossible" by conflating C's local placeholder memfd with S's HOST3D resource; that wrong entry is left
  in the diary, corrected by the next.) **Build note that matters for 4.3:** virglrenderer exports a
  resource's dma-buf **once, at blob creation**, and forbids a second export (`mem->exported` guard) — so S
  must **retain** the creation-time dma-buf fd, keyed by `resource_id`, rather than re-export on demand.
- **4.1:** C-side daemon wiring. `crates/rayland-c/src/proxy_link.rs` has `LinkSink` (forwards over the
  QUIC link as `C2S::WaylandRequest` / `C2S::WaylandBind`) and `BlobInodeResolver` (fd inode → resource id,
  by scanning the blob table). `shm.rs` captures each memfd's inode at creation. `main.rs` spawns the proxy
  thread when `RAYLAND_C1_WAYLAND_DISPLAY` is set (gated so offscreen fixtures/tests are untouched).
- **4.2a:** S router — `crates/rayland-s/src/main.rs` `serve()` splits `C2S::WaylandRequest`/`WaylandBind`
  off before the vtest apply path and hands them to `WaylandReplay`.
- **4.2b-i:** wire additions so S can rebuild the object graph — `C2S::WaylandBind { interface, version,
  app_object_id }` (the app's `wl_registry.bind` is a C-backend built-in that never crosses) and
  `WaylandArg::NewId { id, interface, version }` (C stamps each new object's interface so S's `send_request`
  `child_spec` needs no hand-built protocol table).
- **4.2b-ii:** S replays the session against its **real compositor** — `crates/rayland-s/src/wayland_client.rs`
  (`WaylandReplay`): enumerates S's globals, binds them on `WaylandBind` (by interface name, version-capped),
  builds the `app_id ↔ s_id` map, reconstructs + `send_request`s each request. Proven by
  `crates/rayland-s/tests/wayland_replay.rs` (real compositor) and the vkcube smoke.

**Two hard-won findings to keep in mind:**
- `wl_registry.bind`'s wire signature spells the generic new_id out in full: `[name, interface_string,
  version, new_id]`. Those two middle args are **explicit**, not injected from `child_spec`. `bind` is the
  *only* request like this (its child interface is dynamic). Every other request relies on `child_spec` alone.
- The client `send_request` **panics** on any protocol violation. `WaylandReplay::handle_request` wraps it in
  `catch_unwind` so a translation bug is logged/dropped and the shared message thread (which also serves the
  vtest/ring session) survives.

---

## 3. What is NEXT — Tasks 4.3 and 4.4

vkcube currently drives all the way to binding the WP0 globals on S's compositor, then **aborts at
`pick_surface_format: Assertion 'count >= 1' failed`**. This is *not* a bug — it is the missing **event
return path**: the proxy advertises `zwp_linux_dmabuf` but returns no `format`/`modifier` events to the app,
so Mesa's WSI sees zero surface formats. **This is the critical blocker.**

### Recommended order: **4.4 first, then 4.3.**
4.4 unblocks vkcube (it can't proceed past format selection without it) and lets the app drive further,
exposing whatever comes next naturally. 4.3 makes the pixels actually appear once the app reaches `attach`.

### Task 4.4 — the event return path
- **C proxy needs a `send_event` path.** Today `wayland_proxy.rs`'s `serve()` poll loop watches only the
  listener fd and the backend fd. Add an **eventfd** (or pipe) as a third pollable fd so an inbound
  `S2C::WaylandEvent` from another thread can wake the loop; then deliver via `Handle::send_event`.
- **C reader thread** (`crates/rayland-c/src/main.rs` `reader_thread`) currently owns `rx.recv()` and routes
  every `S2C`. Add an `S2C::WaylandEvent` arm that hands the event to the proxy (via a `Sender` + the eventfd
  wakeup). `S2C::WaylandEvent { message: WaylandMessage }` already exists in `rayland-relay`.
- **S replay** (`WaylandReplay` / `ReplayObjectData::event`) currently drops compositor events. It must
  translate each event's ids **S→app** (needs a reverse `s_id → app_id` map — add it alongside the forward
  map) and send it as `S2C::WaylandEvent`.
- **This delivers `xdg_surface.configure`** (a real client blocks on the first one before presenting) **and
  the dmabuf `format`/`modifier` events** that unblock `pick_surface_format`. Note the proxy advertises
  `zwp_linux_dmabuf_v1` at the descriptor's max version (v4 = feedback); check whether Mesa uses the v3
  `format`/`modifier` events or the v4 `zwp_linux_dmabuf_feedback_v1` path, and make sure the relevant one
  is forwarded. (`rayland-present/src/window.rs` uses the v3 `format`/`modifier` mechanism as a reference.)

### Task 4.3 — token → `wl_buffer` (zero-copy)
- **On S, retain the creation-time dma-buf.** `crates/rayland-engine/src/virgl.rs` has a **private**
  `export_blob_fd(resource_id)` called once at HOST3D blob creation. You need S to **keep** that dma-buf fd
  keyed by `resource_id` (it cannot be re-exported — `mem->exported` guard). Add a public engine method /
  actor message to fetch the retained dma-buf for a `resource_id`, and the retention itself.
- **On a `WaylandArg::Buffer(token)` request** (currently skipped in `handle_request`): resolve
  `token.resource_id` → the retained dma-buf, build a real `wl_buffer` via S's compositor's
  `zwp_linux_dmabuf` (`create_params` / `add` with the fd + `token.width/height/drm_format/modifier` /
  `create_immed`), and map the app's buffer id → the S-side `wl_buffer`. For LINEAR swapchain images:
  offset 0, stride = width·bpp.
- Then `wl_surface.attach(buffer)` + `commit` replay naturally (attach's `Object` arg resolves via the map),
  **gated on frame completion** (reuse the existing (c)2 G' completion signal so the compositor never sees a
  half-rendered frame).

### 4.5 — the proof
vkcube's spinning cube in a window **on S's screen**, animating, no crash/assert/wedge.

---

## 4. How to build and run (important gotchas)

- **BUILD TARGET:** always
  `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo …`. The default `/tmp` target is a tmpfs
  with a per-user quota; a full target dir there makes the **linker die with a bare SIGBUS** (`collect2: ld
  terminated with signal 7`). If you see that, it's `df`, not your diff.
- **This machine (dop561) is S:** it has the GPU (`/dev/dri/renderD128`) and a live compositor
  (`WAYLAND_DISPLAY=wayland-2`). So you can run **both** daemons on loopback here and present on this screen.
- **Running the loopback vkcube smoke** (reconstruct it — the previous session's scratchpad is gone):
  - Start `rayland-s` with env: `RAYLAND_C1_S_LISTEN=127.0.0.1:<port>`,
    `RAYLAND_C1_RENDER_NODE=/dev/dri/renderD128`, `RAYLAND_C1_NO_PRESENT=1` (for now),
    `WAYLAND_DISPLAY=wayland-2` (so the replay can reach the compositor). Wait until it logs `listening`.
  - Start `rayland-c` with env: `RAYLAND_C1_SOCKET=<vtest.sock>`, `RAYLAND_C1_S_ADDR=127.0.0.1:<port>`,
    `RAYLAND_C1_WAYLAND_DISPLAY=<proxy.sock>`. Wait until `<proxy.sock>` exists.
  - Launch vkcube: `VN_DEBUG=vtest VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json
    VTEST_SOCKET_NAME=<vtest.sock> WAYLAND_DISPLAY=<proxy.sock> vkcube --c 3 --wsi wayland`.
  - `RAYLAND_WP_LOG=1` turns on the proxy's per-request stderr; the replay logs to S's stderr already.
- **PROCESS DISCIPLINE (hard rule from the user's global CLAUDE.md):** NEVER `pkill`/`killall`/pattern-kill —
  a pattern once killed the user's Chrome *and* the Claude session. Kill only by the **exact PID** you
  captured (`cmd & PID=$!`). The daemons are thread-only (no child procs), so a direct `kill $PID` is fine;
  for vkcube prefer `timeout 15 … vkcube …`. Always verify no leftovers with `ps` afterward (by inspection,
  not by killing).
- **Never add `VN_DEBUG=no_abort`.** (A deliberate constraint from earlier tasks — it hides the exact aborts
  you need to see.)

---

## 5. Method / conventions expected

- **Diary every working turn** (`docs/DIARY.md`), in the project's honest voice — record uncertainty while
  it's still uncertain; if a belief is overturned, leave the wrong entry and give the reversal its own entry.
- **Update the SDD ledger** (`.superpowers/sdd/progress.md`) as you complete each sub-step.
- **TDD / teeth-check:** watch every new test fail before trusting green (the previous session teeth-checked
  each proof by breaking the thing under test and confirming the test catches it).
- **Read the source of Venus / virglrenderer / the compositor for answers** — the user explicitly values
  this over reasoning from assumptions (it's how 4.0-bis got corrected). Source trees are on disk under
  `/tmp/claude-1000/.../scratchpad/` from prior sessions: `virglrenderer-1.3.0/` (and a 1.2.0 tree — 1.2.0
  is the linked version), Mesa Venus (`venus-ring-src/vn_*.c`), and a full mesa checkout
  (`mesa-fetch/mesa`, `mesa-wsi/`). If they're gone, re-fetch.
- The workspace has **17 crates**; the WP0-relevant ones are `rayland-c` (proxy/daemon), `rayland-s`
  (replay/daemon), `rayland-relay` (wire), `rayland-engine` (virglrenderer FFI), `rayland-present` (existing
  presentation, a reference for the dmabuf/compositor mechanics).
- `rayland-c` must **never** link a GPU stack (`tests/no_gpu_linkage.rs` guards it) — keep it that way when
  adding deps.

---

## 6. Key files map

| File | Role |
|---|---|
| `crates/rayland-relay/src/message.rs` | wire: `C2S::WaylandRequest`/`WaylandBind`, `S2C::WaylandEvent`, `WaylandMessage`, `WaylandArg` (incl. `NewId{id,interface,version}`, `Buffer(BufferToken)`), `BufferToken{resource_id,width,height,drm_format,modifier}` |
| `crates/rayland-c/src/wayland_proxy.rs` | C proxy: wayland-backend **server**; globals, request forward, fd→token intercept, translate_message/translate_new_id. **4.4:** add eventfd wakeup + `send_event` here. |
| `crates/rayland-c/src/proxy_link.rs` | `LinkSink` (→ link) + `BlobInodeResolver` (inode→res_id) |
| `crates/rayland-c/src/shm.rs` | `LocalBlob` + memfd inode capture |
| `crates/rayland-c/src/main.rs` | daemon: proxy thread spawn (`RAYLAND_C1_WAYLAND_DISPLAY`); `reader_thread` (**4.4:** add `S2C::WaylandEvent` arm) |
| `crates/rayland-s/src/wayland_client.rs` | `WaylandReplay`: wayland-backend **client**; bind/request replay, `app_id↔s_id` map. **4.3:** handle `Buffer` token. **4.4:** translate+return events (add reverse map). |
| `crates/rayland-s/src/main.rs` | daemon: `serve()` router (splits Wayland msgs); message/progress/engine-actor threads |
| `crates/rayland-engine/src/virgl.rs` | `export_blob_fd` (private, SHM-oriented). **4.3:** retain the HOST3D dma-buf per `resource_id` + a public fetch. |
| `crates/rayland-s/tests/wayland_replay.rs` | replay integration test (real compositor) |

Recent WP0 commits: `git log --oneline | grep wp0`. HEAD of WP0 is `wp0(task4.2b-ii)`.

---

## 7. First move for the new session

Confirm the baseline still builds and passes, then start 4.4:

```
CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-s -p rayland-c -p rayland-relay
```

(the GPU e2e in that set takes ~6 min; scope to `--lib` + the wayland tests for a fast loop). Then reproduce
the vkcube smoke (§4) to see the `pick_surface_format` abort with your own eyes — that is the wall 4.4
removes — and begin the event return path.
