# Report to planning — what a real toolkit asks the WP0 proxy for

**Session:** 2026-08-30, dop561 only (see §5). **Branch:** `wp0-wayland-proxy`.
**Evidence:** `docs/data/2026-08-30-wp0-toolkit-scouting/`. **Reproducer:** `scripts/wp0-probe.sh`.

> **The measured list has one entry: `wl_shm`.** Both applications die at **winit's event-loop
> creation** — earlier than predicted, before any window exists — having bound **no global at all**.
> Nothing about window creation, `wgpu`, or solarsim's own code has been tested, so the *second* item
> on the list is genuinely unknown rather than inferable.

---

## 1. The four runs

| Run | What | Outcome |
|---|---|---|
| **A** control | solarsim natively | **works** — `Intel(R) Iris(R) Xe Graphics [IntegratedGpu, Vulkan]` |
| **B** | solarsim through the proxy | **dies at winit event-loop creation** |
| **C** control | the probe natively | **works** — 60 frames, exit 0 |
| **D** | the probe through the proxy | **dies identically to B** |

```
Error: Os(OsError { line: 99,
  file: ".../winit-0.30.13/src/platform_impl/linux/wayland/event_loop/mod.rs",
  error: WaylandError(Bind(NotPresent)) })
```

Same file, same line, same error, from two independent applications.

**The proxy's own log is what makes it unambiguous:** `application connected: client …` and then
**nothing**. Neither application binds a single global. The toolkit enumerates the registry, finds a
required global absent, and aborts before binding anything.

## 2. The measured list — one entry

**1. `wl_shm`.** `winit` 0.30 binds in this order; only `?` is fatal (`.../wayland/state.rs`):

| line | global | fatal? | proxy |
|---|---|---|---|
| 128 | `wl_compositor` | **yes** | ✅ |
| 129 | `wl_subcompositor` | no — soft `match` | ❌ irrelevant |
| 153 | viewporter | no — `.ok()` | ❌ irrelevant |
| **158** | **`wl_shm`** | **yes** | **❌ ← the failure** |
| 170 | `xdg_wm_base` | **yes** | ✅ |
| 171+ | xdg_activation, kwin_blur, text_input, relative_pointer | no — `.ok()` | ❌ irrelevant |

The proxy advertises four (`wayland_proxy.rs:995-998`) and supplies two of winit's three hard
requirements. **`wl_shm` is the only missing one.**

The list stops at one because **nothing can get past it without a code change**, which decision 5
forbade. That is a complete result by the brief's own standard, not a truncated one.

## 3. This refutes the prediction, and the refutation is the point

Decision 4 predicted: dies at **window creation** for want of `wl_shm`, then `wl_output`, then
`wl_subcompositor`.

- **`wl_shm` first — correct.**
- **"At window creation" — wrong, and it matters.** It dies at *event-loop* creation. Nothing about
  window creation has been exercised, so this session says nothing about what fails second.
- **`wl_subcompositor` next — not supported.** It is a *soft* bind in winit 0.30 that logs and
  continues. `wl_output` is not bound at init either.

**Honest statement about entry two: unknown.** On the source reading, winit's init should complete
with the current four plus `wl_shm`, and the next failure would lie past init. I would rather report
that than extend the list from the same kind of source-reading that produced the guess.

## 4. Toolkit-wide vs solarsim-specific

**Every failure observed is toolkit-wide; none is solarsim's alone** — solarsim never reaches its own
code. The probe reproduces the failure exactly, which **validates it as the stand-in it was built to
be**: the small crate can replace the whole application from here on, and it is re-runnable in one
command.

## 5. Deviations — one of them substantial

1. **`apollo` is down.** Both addresses unreachable, no route. The runs were done on **loopback on
   dop561**, and the controls ran on dop561 rather than apollo.

   *Why the result still stands:* the question is which services the toolkit asks **`rayland-c`** for,
   and that is decided by what the proxy advertises — identical on loopback. *What is genuinely
   weakened:* the controls no longer prove the binaries work on apollo, only that they work somewhere.
   For this question that is enough; for a presentation or performance question it would not be.

2. **solarsim is at `~/git/solsim`**, not `~/git/solarsim`.

3. **The design note is not filed**, so there is no addendum to add. The brief anticipated this.

4. **`WAYLAND_DEBUG=1` yields nothing** for either application — both reach Wayland through the
   pure-Rust `wayland-client`, not libwayland. `RAYLAND_WP_LOG` was the witness that worked, and its
   most informative output was an absence.

5. **`scripts/wp0-probe.sh`'s split-machine path is guarded off** with an explicit error rather than
   shipped untested, since it has never been run.

6. **A peer session reports `milkv` is now a working C-side machine** — riscv64, Debian sid chroot,
   Mesa 26.1.6, solarsim built and verified there against lavapipe. Nothing has crossed the network on
   it yet. It is the obvious way to redo these runs genuinely split, and arguably a better C machine
   than apollo for this project — a weak board is the case Rayland exists for.

## 6. The smallest next step

**Advertise `wl_shm` on the proxy and re-run `scripts/wp0-probe.sh`.** That is one global, and the
probe will immediately say whether the toolkit gets further and where it stops next — which is the
only way to learn entry two.

That decision is the owner's, and it is not small in its implications: `wl_shm.create_pool` passes a
**file descriptor**, which cannot cross a network, and its contents are **pixels**. Whatever shape the
answer takes, it will not be "forward the fd", and it should not quietly reintroduce the per-frame
pixel traffic removed the day before. The evidence this session was asked for now exists to inform it.

**Also worth scheduling, unrelated to this front:** the two `vkgears` defects from the rate session
remain open, and one of them segfaults `rayland-s`.
