# What a real toolkit asks the WP0 proxy for — measured, 2026-08-30

Four runs. The evidence for `docs/reports/2026-08-30-wp0-toolkit-scouting-report.md`.

| Run | What | Outcome |
|---|---|---|
| **A** control | `solarsim` natively | **works** — `GPU: Intel(R) Iris(R) Xe Graphics [IntegratedGpu, Vulkan]`, ran until the harness timer |
| **B** | `solarsim` through the WP0 proxy | **dies at winit event-loop creation** |
| **C** control | the probe natively | **works** — 60 frames presented, exit 0 |
| **D** | the probe through the WP0 proxy | **dies identically to B** |

## The failure, identical in B and D

```
Error: Os(OsError { line: 99,
  file: ".../winit-0.30.13/src/platform_impl/linux/wayland/event_loop/mod.rs",
  error: WaylandError(Bind(NotPresent)) })
```

Same file, same line, same error, from two independent applications. And the proxy's own log shows
why that matters: **neither application binds a single global.**

```
[wp-proxy] application connected: client InnerClientId { id: 0, serial: 1 }
                              (... and nothing further)
```

The toolkit enumerates the registry, finds a required global absent, and aborts **before binding
anything at all**. It never reaches window creation, never reaches `wgpu`, never reaches solarsim's
own code.

## The measured list — it has exactly one entry

**1. `wl_shm`.**

`winit` 0.30's Wayland event loop binds in this order, and only the `?` lines are fatal
(`winit-0.30.13/src/platform_impl/linux/wayland/state.rs`):

| line | global | required? | proxy has it? |
|---|---|---|---|
| 128 | `wl_compositor` | **yes** (`?`) | ✅ |
| 129 | `wl_subcompositor` | no — soft `match`, logs and continues | ❌ (does not matter) |
| 153 | viewporter | no — `.ok()` | ❌ (does not matter) |
| **158** | **`wl_shm`** | **yes (`?`)** | **❌ ← the failure** |
| 170 | `xdg_wm_base` | **yes** (`?`) | ✅ |
| 171+ | xdg_activation, kwin_blur, text_input, relative_pointer | no — all `.ok()` | ❌ (do not matter) |

The proxy advertises four globals (`wayland_proxy.rs:995-998`): `wl_compositor`, `xdg_wm_base`,
`zwp_linux_dmabuf_v1`, `wl_seat`. Of winit's three hard requirements it supplies two. **`wl_shm` is
the only missing one**, and it is the whole reason both applications die.

**The list stops at one entry because nothing can get past it without a code change**, which this
session was forbidden. Adding `wl_shm` is the owner's decision and needs this evidence first.

## This partly refutes the prediction, and the refutation is the useful part

The design note predicted death at **window creation** for want of `wl_shm`, then `wl_output`, then
`wl_subcompositor`.

- **`wl_shm` first: correct.**
- **"At window creation": wrong.** It dies at *event-loop creation*, before any window exists — which
  matters, because it means nothing about window creation has been tested at all.
- **`wl_subcompositor` as a subsequent blocker: not supported.** In winit 0.30 it is a soft bind that
  logs and continues. `wl_output` is not bound at init either.

So the honest statement about what comes *after* `wl_shm` is: **unknown, and unmeasurable from here.**
On the source reading, winit's init should complete with the proxy's existing four plus `wl_shm`; the
next failure would be somewhere past init, and no evidence in this session touches it.

## Toolkit-wide vs solarsim-specific

**Every failure observed is toolkit-wide. None is solarsim's alone** — because solarsim never reaches
its own code. The probe reproduces the failure exactly, which validates it as the stand-in it was
built to be: from here on, the small crate can be used instead of the whole application.
