# The mouse stall, fixed — in `vkcube`, and demonstrated

## What the owner asked

*"I want that stall fixed. Why is glxgears apollo→dop561 running without any hiccups? If the fix has
to be in vkcube: fine. But I cannot have this kind of stalls when ordinary applications start working
with Rayland."*

Two questions, and they have different answers.

## Question 2 first: will ordinary applications stall? **No — vkcube is the outlier.**

This is settled from source, not opinion. Two Vulkan applications, same Wayland path:

**`vkcube`** (Vulkan-Tools, `cube/cube.c:3064`) — rebuilds unconditionally:

```c
static void handle_surface_configure(...) {
    xdg_surface_ack_configure(xdg_surface, serial);
    ...
    demo_resize(demo);          // every configure, no size check
}
```
It does not even look at what changed: `handle_toplevel_configure` marks `states UNUSED`.

**`vkgears`** (mesa-demos) — records, compares, and only then rebuilds:

```c
static void wsi_resize(int p_new_width, int p_new_height) {
   new_width = p_new_width;  new_height = p_new_height;      // record only
}
...
if (result == VK_SUBOPTIMAL_KHR || width != new_width || height != new_height)
   recreate_swapchain();                                      // compare first
}
```

`vkgears` already does the right thing, and it is the ordinary pattern: react to a real size change, or
to `VK_ERROR_OUT_OF_DATE_KHR` / `VK_SUBOPTIMAL_KHR` from the driver. A toolkit-based application (GTK,
Qt, winit, SDL) does the same. **`vkcube` is a demo with a naive handler, not a representative one.**

(The owner's `glxgears` comparison is a weak control for a different reason: it is OpenGL, and
Rayland's GL path via Zink is (c)4 and unbuilt — so that run went over something else entirely, with
no Vulkan swapchain to rebuild.)

## Question 1: the fix

`docs/patches/vkcube-only-resize-on-actual-size-change.patch` — guard the rebuild on an actual size
change. `!swapchain_ready` keeps the first configure creating the swapchain, and the driver's
out-of-date paths in the draw loop remain the safety net for anything that invalidates the swapchain
without changing its size.

### Validated natively first (dop561, COSMIC, pointer crossing only)

| arm | configures | distinct sizes | rebuilds |
|---|---|---|---|
| stock | 3 | 2 | **3** |
| patched | 4 | 2 | **1** |

Stock rebuilds once per *configure*; patched rebuilds once per *actual size change*.

### Then demonstrated over Rayland — milkv → dop561, 60 s each, pointer crossing only

| arm | frames in 60 s | configures | same-size configures | **rebuilds** | **stalls** | worst gap |
|---|---|---|---|---|---|---|
| **stock** | 252 | 178 | 177 | **92** | **9** | **1,117 ms** |
| **patched** | **1,079** | 392 | 391 | **1** | **0** | **104 ms** |

**391 pure focus-change configures produced one rebuild and zero stalls.** The worst gap fell from
1,117 ms to 104 ms, and the patched run drew **4.3× more frames in the same wall-clock time** because
it was not spending that time destroying and recreating swapchains.

> Read the median inter-frame gap with care here: stock's 40 ms looks *better* than patched's 53 ms,
> and it is an artefact — stock only produced 252 frames because it was stalled for much of the run,
> so its median is taken over the frames that did happen. Frames-per-run is the honest measure.

## Building it (the board has no cmake, and does not need one)

`cube.frag.inc` / `cube.vert.inc` are pre-generated in the repo, so `cube.c` compiles directly:

```
wayland-scanner client-header .../xdg-shell.xml                 xdg-shell-client-header.h
wayland-scanner private-code  .../xdg-shell.xml                 xdg-shell-code.c
wayland-scanner client-header .../xdg-decoration-unstable-v1.xml xdg-decoration-client-header.h
wayland-scanner private-code  .../xdg-decoration-unstable-v1.xml xdg-decoration-code.c
gcc -O2 -o vkcube cube.c xdg-shell-code.c xdg-decoration-code.c -I. \
    -DVK_USE_PLATFORM_WAYLAND_KHR -DVK_NO_PROTOTYPES -lvulkan -lwayland-client -ldl -lm
```

The `wayland_loader.h` shim expects the generated headers under `xdg-*-client-header.h`, and
`-lwayland-client` is required even though it `dlopen`s the library — the `wl_*_interface` symbols are
data. Built this way for riscv64 inside `/mnt/build/sid` on milkv.

## What this does NOT change about Rayland

Nothing was fixed in Rayland, because nothing in Rayland was broken. The relay was faithfully carrying
a configure the compositor really sent, to an application that really did choose to rebuild. What the
relay does is make a ~1 ms local cost into a ~1 s one, which is why this is worth an upstream patch
rather than a proxy workaround: **the proxy mitigation considered earlier (withholding a same-size
configure) is still the wrong fix**, since the `states` array genuinely changes and an application that
renders focus needs to know.
