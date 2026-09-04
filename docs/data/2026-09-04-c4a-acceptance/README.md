# (c)4a Task 12 — acceptance PASSED, 2026-09-04

**`solarsim`, unmodified wgpu/winit, running on the milkv riscv64 board, rendered by dop561's Intel
GPU, in its own window on the owner's live COSMIC session.** First time this application has ever run
on that board (cross-compiled for `riscv64gc-unknown-linux-gnu` the same day).

```
GPU: Virtio-GPU Venus (Intel(R) Iris(R) Xe Graphics (RPL-P)) [IntegratedGpu, Vulkan]
t=90s  frames presented on dop561: 121     panics in S: 0
```

## The acceptance criteria, which are visual by design

The spec is explicit that **"resolves" is not "works"** and that acceptance is *not* a list of
supported interfaces. Checked by the repository owner looking at the screen:

| | interface responsible | verdict |
|---|---|---|
| window at the correct scale | `wl_output` + `zxdg_output_manager_v1` (Task 5) | **pass** |
| decorations present | `zxdg_decoration_manager_v1` (Task 6) | **pass** |
| cursor visible over the window | `wp_cursor_shape_manager_v1` (Task 7) | **pass** |

Owner's words: *"It looks completely normal."*

## What acceptance found that every test missed

Getting here required fixing a defect that **all 109 unit tests passed over**: `wl_fixes` cannot be
relayed. Its only request, `destroy_registry`, names a `wl_registry` — the one object the proxy does
not mirror — so the relayed request reached S naming an unknown object, `wayland-backend` panicked,
and the poisoned connection killed the entire Wayland replay. **1 frame and a dead replay → 603
frames and zero panics** once refused. This is the whole justification for the criterion being a human
looking at a screen.

## Two observations from the owner, NOT yet explained

1. **Frame rate is poor** — ~1.3 fps, consistent with the 169 frames in ~120 s recorded on
   2026-09-01, so nothing has regressed. But there is **no baseline**: nobody has measured what
   `solarsim` does on the board's own display, and the board is X11-only and slow, so that baseline
   may not be obtainable in a comparable form.
2. **Resizing stalls output for ~10 s, and then the frame rate is largely unchanged — at roughly four
   times the window area** (doubled in both directions). The stall corroborates the known unfixed
   resize cost (5.1 s and 4.7 s measured on milkv). **The invariance does not corroborate anything —
   it contradicts the obvious explanation.** A fill-rate- or GPU-bound workload scales with pixel
   count; one bound by a fixed per-frame cost does not. If frame rate really is insensitive to area,
   then "the board cannot render faster" is the wrong story and the synchronous round trip is the
   right one — which is what this project has already measured as the dominant term for every other
   workload.

**Neither is a finding yet.** (2) is a single informal observation made by eye during a demo whose
window was being dragged about, on the owner's live session where an unfocused window is throttled by
the compositor. It is written down because it is *testable*: frames per second at two window areas,
interleaved, is a cheap experiment and the answer changes where effort should go.
