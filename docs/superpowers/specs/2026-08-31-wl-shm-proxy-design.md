# WP0: `wl_shm` in the C-side Wayland proxy

**Status:** design, approved 2026-08-31. Not yet implemented.
**Author:** solsim session, at the repository owner's request. Implementation is assigned to the
active rayland session, which owns the WP0 proxy internals.

---

## 1. The problem, in one paragraph

An application that draws with Vulkan still cannot run over Rayland if it was built with a
normal desktop toolkit. `winit` 0.30 — the windowing layer under `solarsim`, and under most
Rust GUI applications — treats **three** Wayland globals as fatal at event-loop creation:
`wl_compositor`, `xdg_wm_base`, and **`wl_shm`**. The proxy in `rayland-c` advertises the first
two (plus `zwp_linux_dmabuf_v1` and `wl_seat`) but not `wl_shm`, so `winit` aborts with
`WaylandError(Bind(NotPresent))` **before it creates a window, touches wgpu, or reaches
Vulkan**. Nothing about the GPU path is exercised; the application dies during setup. This is
not specific to `solarsim`: any `winit`, GTK or Qt application will make the same demand.

## 2. What `wl_shm` actually is, and why it looks alarming at first

`wl_shm` is the original, pre-GPU way for a Wayland client to put a picture on screen. The
client asks the compositor for a **pool** — a shared-memory region — draws pixels into it with
the CPU, carves **buffers** out of it, and attaches one to a surface. It is software rendering,
and it predates every client drawing with a GPU.

The reason it looks like a problem for Rayland is that `wl_shm.create_pool` passes a **file
descriptor**, and Rayland exists precisely because a file descriptor cannot be sent across a
network. The venus-ring findings (`docs/design/2026-07-15-venus-ring-findings.md`) are the
canonical statement of this: a shared memory page has no network representation, which is why
remoting had to be redesigned rather than tunnelled.

**That reasoning does not apply here, and this is the insight the whole design rests on.**
`rayland-c` runs on the *same machine as the application*. When the app passes its pool fd, it
is passing it to a process sitting next to it, over a local Unix socket, exactly as it would to
a local compositor. That works perfectly. The fd never needs to reach S, because `rayland-c` can
map the pool itself and read the pixels out.

So the question was never "can this be done". It is only "what does `rayland-c` do with the
pixels once it has them", which is an ordinary engineering choice.

## 3. What this costs, and why it is cheap in practice

For a GPU application, `wl_shm` carries almost nothing. The actual rendered frames go through
`zwp_linux_dmabuf_v1` as they do today. What comes through `wl_shm` is *furniture*: the mouse
cursor, and whatever `libdecor` paints for window decorations. These are small, they change
rarely, and they are frequently re-committed unchanged.

The hazard is the opposite case — a genuinely software-rendered application attaching a
full-window shm buffer every frame, which at 1080p is roughly 8 MB per frame. That is the
traffic the presented-buffer exclusion was built to eliminate, and it must not creep back in
through this door.

There is no cheap escape for that case on the current C hardware. The Milk-V Mars (StarFive
JH7110) has no usable hardware video encoder: mainline support for its Chips&Media **WAVE420L**
encoder is effectively never coming (it is a Wave4-generation IP needing a near-from-scratch
driver, gated on firmware that is not redistributable, from a vendor whose upstreaming stalled
around Linux 6.7), and the vendor's out-of-tree driver is only usable by freezing the board on
its 5.15 kernel. So on this class of machine, **encoding pixels on C is not an option**, and
"ship commands, not pixels" is not merely preferred — it is the only design that works.

This is why §9's large-buffer warning exists, and why §12 forbids this path from quietly
becoming the main road.

## 4. Approach: mirror the pool on S

**C keeps the fd and maps it locally. S creates its own, separate memfd. The two mappings are
kept in step by copying bytes.** No file descriptor is sent anywhere.

Concretely:

1. C intercepts `wl_shm.create_pool`, `mmap`s the app's fd, and forwards the request to S with
   the fd argument **replaced by the pool's size**.
2. S allocates its own memfd of that size, maps it, and creates a real `wl_shm_pool` against
   *its* compositor.
3. Every other shm request carries no fd and forwards unchanged through the existing object
   mapping.
4. At `wl_surface.commit`, C copies the attached buffer's bytes to S, which writes them into
   its mapping.

### Alternatives considered and rejected

**Ship each buffer as a standalone blob**, letting S build a throwaway pool and buffer per
frame. Rejected: it allocates per frame on S, and it fights Wayland's buffer-reuse model, in
which a client keeps a `wl_buffer` and waits for `wl_buffer.release` before redrawing. It also
discards the natural place to later skip unchanged content.

**Route shm content through the GPU resource path**, uploading it as a virglrenderer resource
so S re-exports it as a dma-buf and the existing attach path is untouched. Rejected on two
counts. It requires synthesising GPU commands on the machine whose defining property is having
no usable GPU, and it would entangle compositor furniture with the application's own Vulkan
command ring — the one stream the project is most careful to keep pristine. It also brushes
against the rule that `rayland-c` must never link a GPU stack
(`crates/rayland-c/tests/no_gpu_linkage.rs`).

## 5. Protocol changes

### 5.1 Pool creation reuses the existing substitution pattern

`wl_shm.create_pool(new_id pool, fd, int size)` is the **only** shm request carrying an fd, so
it gets the treatment `zwp_linux_buffer_params_v1.add` already gets. A new `WaylandArg` variant
replaces the `Fd` argument, exactly as `WaylandArg::Buffer(BufferToken)` does today:

```rust
/// Replaces the `fd` argument of `wl_shm.create_pool`. The descriptor stays on C, which maps
/// it; S learns only how large a pool to allocate for its own, separate memfd.
ShmPool { size: u32 },
```

No new top-level message is needed for pool creation, and the existing object mapping handles
the `new_id`.

### 5.2 One new message carries content

```rust
/// Contents of a region of an application shm pool, copied from C's mapping into S's.
/// Sent immediately BEFORE the `wl_surface.commit` that depends on it — see §7.
C2S::ShmPoolData {
    /// The application's `wl_shm_pool` object id, as C sees it. S maps this to its own pool.
    app_pool_id: u32,
    /// Byte offset into the pool at which `bytes` begins.
    offset: u32,
    /// The pixel bytes themselves.
    bytes: Vec<u8>,
},
```

### 5.3 Everything else is unchanged

`wl_shm_pool.create_buffer`, `wl_shm_pool.destroy`, and `wl_buffer.destroy` carry no fd and
forward as ordinary mapped requests. `wl_buffer.release` returns from S's compositor through
the existing `S2C::WaylandEvent` path — also fd-free — so buffer reuse works with no new code.

## 6. The two components

Both dispatch files are already large (`wayland_proxy.rs` 1391 lines, `wayland_client.rs` 1690).
This feature is self-contained state plus a handful of interception points, so it goes in its own
module on each side, leaving the large files doing routing only.

### 6.1 C side — `crates/rayland-c/src/wayland_proxy/shm.rs`

A `ShmTracker` owning three maps:

- **pool id → mapping**: the `mmap`, its length, and the owning object id.
- **buffer id → geometry**: pool id, offset, stride, height, width, format, recorded from
  `create_buffer`.
- **surface id → attached buffer id**, recorded from `wl_surface.attach`.

`wayland_proxy.rs` gains one global — `create_global::<WlShm>(&handle, 1)` — and five
interception points. Only `create_pool`, `resize` and `commit` do real work; `create_buffer` and
`attach` merely record state so that `commit` knows what to copy:

| Request | Action |
|---|---|
| `wl_shm.create_pool` | `mmap` the fd, record the pool, substitute `ShmPool { size }`, forward |
| `wl_shm_pool.create_buffer` | record geometry, forward unchanged |
| `wl_shm_pool.resize` | re-`mmap` locally, forward |
| `wl_surface.attach` | record the attached buffer, forward |
| `wl_surface.commit` | if the attached buffer is shm: copy its range, send `ShmPoolData`, **then** forward |

**Advertise `wl_shm` at version 1.** Version 2 adds only `wl_shm.release`, which nothing here
needs, and a lower advertised version is the conservative choice: clients bind the minimum of
what they want and what is offered.

On bind, the proxy emits `wl_shm.format` events for `ARGB8888` (0) and `XRGB8888` (1). These two
are mandatory for every Wayland compositor, so advertising exactly them is always truthful
without plumbing S's real format list back across the network. Any other format a client asks
for is refused per §9.

### 6.2 S side — `crates/rayland-s/src/shm_mirror.rs`

A `ShmMirror` holding, per pool: S's memfd, its mapping, and the real `wl_shm_pool` object.

On a `create_pool` whose fd argument arrived as `ShmPool { size }`, S binds `wl_shm` from the
globals it already collects by interface name (see `wayland_client.rs`'s registry handling),
creates a memfd, `ftruncate`s it to `size`, maps it, and creates the real pool. On
`C2S::ShmPoolData` it writes the bytes at the given offset. On `resize` it `ftruncate`s and
remaps **before** forwarding the resize. Destroy unmaps, closes, and destroys the pool object.

**The pitfall to keep in mind while reading this code:** S's memfd is a *different file* from
the application's. They are kept the same size deliberately, by hand, not by sharing. Anything
that changes one size must change the other.

## 7. Ordering: the load-bearing detail

`ShmPoolData` must be sent **before** the `wl_surface.commit` that depends on it.

Both travel the same ordered relay stream, so sending the bytes first is sufficient to guarantee
S's pool is current before its compositor is told to look at the surface. Reversing the order
would present a stale or blank frame — and, worse, would do so intermittently, because whether
it looks wrong depends on what happened to be in the pool from the previous frame. This is the
single easiest thing to get wrong in this design, and §11 tests it explicitly.

`wl_surface.commit` is the correct sync point because it is the moment the Wayland protocol
guarantees the client has finished drawing. Copying at `attach` would be too early (the client
may draw after attaching); copying lazily on S's demand is not possible, because S cannot ask.

## 8. Scope of v1: what this deliberately does *not* do

v1 is **correct first, optimised on evidence**. It does **not** implement:

- **Content hashing** to skip re-sending an unchanged buffer.
- **Damage-rectangle intersection** (`wl_surface.damage_buffer`) to send only changed regions.
- **Compression** (lz4 or otherwise).

All three are known, sound optimisations, and the `ranges` module in `rayland-relay` already
provides the dirty-range merging that damage tracking would build on. They are omitted because
we do not yet know whether they are needed: if `solarsim` pushes a 64×64 cursor a few times a
second, building a cache and a damage intersection is work spent on a guess, and the cache
invalidation logic is exactly where subtle staleness bugs live. §10's instrumentation exists to
answer that question with numbers.

## 9. Error handling

Five refusal cases, reported with the proxy's existing `drop:` log vocabulary so they read like
the failures already present:

1. **Pool shorter than its claimed size.** `create_pool` takes a size from the client; if the
   backing file is smaller, reading the mapping raises **SIGBUS** — a crash with no error path
   and a thoroughly baffling diagnosis. `fstat` the fd and refuse the pool. Cheap guard, nasty
   failure prevented.
2. **Buffer outside pool bounds** (`offset + stride × height > pool size`). Refuse the buffer
   rather than copy out of range — the same class of fault, caught by arithmetic instead.
3. **Unknown pool or buffer at commit.** Drop and log, matching existing `drop:unknown-object`
   behaviour.
4. **Stride smaller than `width × bytes_per_pixel`.** Refuse: proceeding would misread the
   layout and present garbage, which is harder to diagnose than presenting nothing.
5. **Pool resized smaller than a live buffer.** Refuse the resize.

**The deliberate non-refusal:** a large, full-window shm buffer is **carried, not refused**,
with a loud `shm:large-buffer` warning naming the byte size and the surface. Refusing would make
a software-rendered application show a blank window, which is a worse failure to debug than a
slow one. Policy on this is decided later, on the evidence the warning produces.

## 10. Instrumentation — the real v1 deliverable

Because v1 defers optimisation to evidence, the logging **is** that evidence and must be good
enough to carry the decision.

- **Per commit:** pool id, buffer dimensions and format, and bytes synced.
- **Per session, on teardown:** total bytes synced, commit count, and the largest single buffer
  observed — in the style of the summaries `rayland-s` already prints.

The question this must answer unambiguously is: *does a real application push a cursor through
this path, or a window?* If it is a cursor, §8's optimisations are never built.

## 11. Testing

Tests come first, and most need no hardware.

**Unit tests** (`ShmTracker` is pure arithmetic — requests in, byte ranges out — so all of these
run with a synthetic memfd, no compositor, no Mesa, no GPU, no network):

- Range computation from offset/stride/height, including a non-zero offset.
- Each of the five refusals in §9.
- Pool resize, growing and shrinking.
- Attach, then detach (attaching a null buffer).
- Commit with nothing attached.
- Several buffers carved from one pool.
- Pool destroyed while a buffer is still alive.
- The SIGBUS guard, with a memfd deliberately smaller than the declared pool size.

**Ordering test:** assert that the emitted message sequence places `ShmPoolData` before the
`WaylandRequest` carrying the commit (§7). This is worth its own test because reversing them
fails intermittently rather than cleanly.

**Integration test**, in the style of the existing `wayland_proxy_*` suite: a real
`wayland-client` binds `wl_shm`, creates a pool and a buffer, attaches and commits; the test
asserts a `ShmPoolData` with the correct range was produced. No S and no network required.

**End-to-end acceptance:** `solarsim` running on milkv, rendering on dop561. The rig for this
exists and is documented in `/mnt/build/README` on the board.

**Regression:** `vkcube` and the offscreen `rayland-refapp` must be unaffected. Advertising a new
global changes what clients choose, and `libdecor` in particular may now take an shm path for
decorations — which is the very traffic §10 is measuring.

## 12. Risks and dependencies

> **UPDATE 2026-09-01 — this dependency is CLEARED.** The keymap defect described below is **fixed**:
> S now sends the descriptor's *contents* as `WaylandArg::KeymapContent` and C mints a sealed `memfd`
> holding them, the same substitution shape this spec uses for the pool fd. Verified end to end
> (35,581 bytes relayed against 35,581 advertised, zero event drops on either side). So the risk this
> section is built around no longer blocks the shm work, and the early acceptance run it recommends is
> worth doing for other reasons but is no longer the gate. See `docs/data/2026-09-01-keymap-fix/`.
>
> One correction to the paragraph below while it is being read: the keymap drop does **not** always
> segfault the application. `vkgears` was observed **hanging** on it — an application that creates a
> `wl_keyboard` waits for its keymap — and a first, buggy version of the fix produced a genuine
> **SIGBUS** by handing over a short `memfd`. "Segfaults" was one observed symptom, not the mechanism.

**The keymap defect may make this necessary but not sufficient.** `winit` binds `wl_seat` and
creates a `wl_keyboard`. S relays `wl_seat.capabilities`, the application asks for the keyboard,
and S then drops `wl_keyboard.keymap` because it carries an fd — while continuing to deliver
that keyboard's other events. This segfaults `vkgears`
(`docs/data/2026-08-30-wp0-frame-time/keymap-drop-crashes-applications.md`). `solarsim` may
clear the `wl_shm` hurdle and hit the keymap one immediately afterwards.

`winit` is a more defensive library than `vkgears` and may tolerate a keymap that never arrives,
but that is **untested**. Two consequences: the keymap mitigation may need to land alongside
this work, and the end-to-end acceptance test should be attempted **early** rather than at the
end. Checking this is cheap and entirely independent of the shm work — run `solarsim` against
the current proxy and observe whether it survives seat setup. Do that before writing shm code.

**The coalescing safety condition must be stated, not inferred (folded in 2026-09-01).** When §8's
optimisations are eventually built, `rayland_relay::ranges::coalesce_ranges` is the shared primitive —
and it carries an obligation. Re-shipping an unchanged byte is safe **only where the sender's baseline
is a faithful model of the receiver's copy**. That holds here because C is the only writer of the pool
and S's memfd is written exclusively from C's copies. **State this explicitly when the optimisation is
built: the identical-looking condition on the presented-blob path is FALSE — S renders into those and
never reports it — and conflating the two is a pixel-corruption bug.**

**Instrumentation must not perturb what it measures (folded in 2026-09-01).** §10 says what to record
but not how, and "per commit: pool, dimensions, bytes" invites a per-commit `eprintln`, which is
exactly the mistake this project has made **five** times — most recently a link trace whose two
`eprintln`s per message halved the frame rate. The shape that works: a **log2-bucketed histogram
behind an env gate**, one `CLOCK_MONOTONIC` read and one relaxed atomic per sample, reported on a
timer. `crates/rayland-s/src/lockstat.rs` and `rayland_relay::stagelog` are working instances to copy.
No per-message `eprintln` on any path that runs per frame.

**The furniture assumption is an assumption.** §3 predicts cursor-and-decoration traffic. If
`libdecor` turns out to repaint large decoration surfaces frequently, the numbers change and
§8's optimisations move from "probably unnecessary" to "required". §10 exists to detect this.

**This path must never become the main road.** If a full-window shm surface becomes routine
rather than exceptional, that is a signal to reconsider the design, not to optimise the shm
path until it is fast enough. On hardware that cannot encode video (§3), there is no version of
shipping full frames from C that ends well.

## 13. Acceptance criteria

1. `solarsim` starts over Rayland without `WaylandError(Bind(NotPresent))`.
2. `solarsim` renders on dop561's display, driven from milkv.
3. `vkcube` and the offscreen `rayland-refapp` are unchanged (bit-identical output for the
   latter).
4. The session summary reports shm traffic, and that number is small enough to confirm — or
   refute — the furniture assumption in §3.
