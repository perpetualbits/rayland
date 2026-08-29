# Two more applications through WP0 — 2026-08-30

vkcube was the only application WP0 had ever been driven by. Two others were tried, at the
repository owner's suggestion, and both were informative — one by crashing.

## `vkgears` — a real second Vulkan WSI client, and it **crashes `rayland-s`**

Same path, same environment, `/usr/bin/vkgears` on apollo. Result: **`rayland-s` segfaults**, zero
buffers built, zero frames. Two distinct defects, in a chain:

**A. A bind capped on S is not propagated to the objects created from it.**

```
[wp-proxy] bound global xdg_wm_base v6 -> object 12          (C advertises the descriptor's max)
rayland-s: WP0 replay bound `xdg_wm_base` v5 (app obj 12)     (S's weston offers only v5, so capped)
panicked: Error when sending request xdg_wm_base@5.get_xdg_surface: expected version 5 but got 6
```

`handle_bind` already caps the bind at what S advertises (`version.min(g.version)`) — correctly, since
binding above a global's maximum is a protocol error. But the *child* objects created from that global
still carry the version **C** stamped on the `NewId`, which is the application's version, not the
capped one. A Wayland child inherits its parent's version and `wayland-backend` enforces it, so the
first `get_xdg_surface` panics.

vkcube never exposed this because the proxy advertises `xdg_wm_base` at the descriptor max and COSMIC
happens to offer the same version, so nothing was ever capped. **This is the third instance of the
version-inheritance rule biting this project** (after `create_immed`'s `wl_buffer` child, and the
params object) and the first where the mismatch is created by S's own capping.

**B. `catch_unwind` does not save the session, and the log says it does.**

```
rayland-s: WP0 replay: send_request (obj 12 opcode 2) panicked — likely a translation bug;
                       request dropped, session continues
panicked: called `Result::unwrap()` on an `Err` value: PoisonError { .. }
<segfault>
```

The panic happens while the `maps` mutex is held, which **poisons** it. The `catch_unwind` duly catches
the first panic and logs that the session continues — and then the next
`.lock().expect("the WP0 id maps lock is never poisoned")` finds it poisoned and takes the process
down. The comment asserting the lock is never poisoned is now demonstrably false, and the
reassuring log line is worse than no log line.

**Not fixed here** (this session is a measurement session). Both are recorded as the next task.

## `rayland-icosa-window` — refuses cleanly, and the refusal is correct

```
rayland-icosa-window: wl_shm unavailable: the requested global was not found in the registry
exit=0
```

`rayland-s` is untouched: zero panics. The demo presents via **`wl_shm`**, not a Vulkan dmabuf
swapchain — it renders offscreen and copies pixels into shared memory — and the C proxy deliberately
advertises no `wl_shm`.

That refusal is the right behaviour rather than a gap to plug casually. `wl_shm.create_pool` passes a
**file descriptor**, which cannot cross a network, and WP0's buffer-by-token substitution exists only
for the dmabuf path. And if it *were* proxied, the pool's contents are literally pixels: ~1 MB per
frame across the wire, which is exactly the traffic the presented-buffer exclusion removed the day
before. `rayland-icosa-window` is a `wl_shm` client by construction; WP0 is a dmabuf mechanism.
