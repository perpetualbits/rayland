# Rayland

**Native remote GPU rendering for Wayland.** Run a graphical application on one machine, but render
and display it on *another* machine — the one with the powerful GPU and the monitor you are actually
looking at — by sending a **command stream** across the network instead of a stream of pixels.

It is the modern heir to X11's network-transparent graphics, rebuilt for Vulkan. The name nods to
Sun Ray (thin client, compute elsewhere, display here) and rhymes with Wayland.

> **Status: the core thesis is proven and running.** An **unmodified** Vulkan application executes on
> one machine, is rendered by a second machine's GPU across a real network, and animates live in a
> window on that second machine's screen. The application is not patched, not relinked, and not aware
> that any of this is happening. Frames come back **bit-identical** to running the same binary
> natively, and the shipping configuration measured **0 failures in 480 real-network runs**.
>
> It is **not** yet a general-purpose tool. It runs Vulkan applications, not OpenGL; it has been
> exercised on a handful of workloads, not on a desktop full of them; and the presentation path is
> mid-rebuild (see [Where it stands](#where-it-stands)). Treat it as working research infrastructure,
> not a product.

## The idea, plainly

Rayland borrows X11-era vocabulary, which is the *reverse* of how "client" and "server" are used in
the cloud. Read this carefully, because every document and identifier in the project depends on it:

| Term | Meaning in Rayland | Example |
|------|--------------------|---------|
| **S** — "server" side | Where **you sit**: keyboard, mouse, **display, GPU**, the Wayland compositor, working drivers. | Your capable desktop. |
| **C** — "client" side | Where the **application executable runs**. Possibly weak, a different CPU architecture, or headless. | A RISC-V single-board computer, or a big CPU-only hypervisor. |

The application runs on **C**. To draw, it emits a *command stream* — the language of rendering
("draw these triangles, with this shader, sampling this texture") — which crosses the network to
**S**. There, **S's GPU** does the real work and the result appears on **S's** display.

The key bet: **ship commands, not pixels.** A video stream of already-rendered pixels *is* supported
as a fallback — but only as a fallback, because in the target setup the weak machine (C) is exactly
the wrong place to run an expensive video encoder.

## Why this is hard

Wayland deliberately assumes the application and the compositor share memory and a GPU: the app
renders into a GPU buffer and passes a *file-descriptor handle* over a local socket. You cannot send
a file descriptor across a network, so remoteness is not a missing feature — it is an **excluded
assumption**.

And the difficulty runs deeper than the file descriptor, in a way that is easy to miss. The obvious
plan — "find the socket the graphics driver talks over, and put a network under it" — **does not
work at all**, because that socket carries none of the drawing:

> Mesa's Venus driver does not send Vulkan commands over its socket. It writes them into a
> **shared-memory ring** whose file descriptor was passed over `SCM_RIGHTS`; the socket carries only
> a *doorbell* saying "the ring has advanced". **0% of the application's commands cross the socket.**

Neither a shared memory page nor a file descriptor survives a network, so remoting is a protocol
design problem rather than a transport swap. That discovery reshaped the project and is written up in
[`docs/design/2026-07-15-venus-ring-findings.md`](docs/design/2026-07-15-venus-ring-findings.md).

## Why it is not hopeless

The hardest component — serializing a Vulkan command stream and replaying it on a remote GPU —
already exists and is battle-tested in the virtual-machine world (Venus, virglrenderer, gfxstream;
the whole stack ships in ChromeOS Crostini). Better still, it is hardened against *exactly* the
threat model remoting has: an untrusted party driving the host GPU.

So Rayland does not reinvent it. Rayland's own code is **100% Rust**; the borrowed engine is an
external library linked behind a clean Rust trait boundary, kept clean enough that the engine could
later be replaced without touching the rest.

## How it works

The trick that makes it work with a **completely unmodified** application and a **stock, unpatched**
Mesa driver is this: the driver's protocol has a notion of a "host", and the host is simply *whoever
allocates the ring*. Rayland can be that party.

`rayland-c` is a local server the application's own Mesa driver connects to. It hands the application
ordinary local shared-memory for its command ring — memory that Rayland owns. The application then
writes its Vulkan commands into Rayland's memory without knowing it, and Rayland relays those bytes
across the network. No Mesa fork, no patch, no driver shim.

```
  C — the application machine                network            S — the GPU machine
  ┌─────────────────────────────┐                               ┌─────────────────────────────┐
  │ unmodified Vulkan app       │                               │ rayland-s                   │
  │   ↓ stock Mesa Venus driver │                               │   writes the delta INTO its  │
  │ writes commands into a ring │   ring deltas + blob syncs    │   own mirror of that ring —  │
  │ rayland-c watches that ring │ ────────── QUIC ────────────► │   which is where the engine  │
  │   and relays the raw bytes  │                               │   already polls for work     │
  │                             │ ◄───────── QUIC ───────────── │   ↓ real GPU draws it        │
  │ results land back in the    │   readback, replies, progress │   ↓ into a window on S        │
  │ app's own memory            │                               │                             │
  └─────────────────────────────┘                               └─────────────────────────────┘
```

On S, the relayed bytes are **written into memory**, not "executed": the borrowed engine's own thread
is already polling those pages for work. That inversion is the single most counter-intuitive thing
about the implementation.

## Where it stands

| Phase | |
|---|---|
| **SP0–SP3** — first light, on-screen, QUIC transport, zero-copy presentation | ✅ complete |
| **C0** — an unmodified app captured by Mesa's Venus driver, replayed on a real GPU, PNG bit-identical to native | ✅ complete |
| **(c)1** — the network: commands across two real machines, and what that costs | ✅ complete |
| **(c)2** — mapped memory and the readback return path | ✅ complete |
| **WP0** — Wayland proxy: the app's own window, instead of S re-presenting a readback buffer | 🔨 **in progress** |
| **(c)3** — content-addressed assets | planned |
| **(c)4** — real/complex applications; OpenGL via Zink | planned |
| **SP4 / SP5** — adaptive policy, session and security; full Wayland proxy coverage | planned |
| **Audio** | planned, separate track |

**What is measured** (not merely believed — this project's habit is to distrust demos):

- The application's pixels come back **bit-identical** to a native run, over a real network.
- **0 failures in 480 real-network runs** of the shipping configuration (<0.62% at 95% confidence).
- **0 stale frames in 20 real-network runs** after the return-path completion barrier landed.
- Commands really are nearly free: one fixture ships **1,706 bytes of commands per frame** against
  **5.21 MiB of mapped memory per frame** — a ratio of ~3,200×. The founding intuition holds, and the
  cost lives somewhere else entirely.
- The remaining per-frame cost is the **synchronous round trip** (the application polling a fence
  through the network), not bandwidth and not message count.

**What is honestly still open:** the mapped-memory forward path over a true network, multi-queue
support, the round trip itself, OpenGL, and everything a real desktop application needs beyond the
Wayland surface basics.

## Trying it

Rayland needs **two machines** for anything end-to-end: one running the application, one with the GPU
and the display. The GPU-free parts build and test anywhere:

```sh
cargo test -p rayland-vtest -p rayland-relay -p rayland-venus-proto
```

The demo — an application on C, drawn by S's GPU, animating on S's screen — is
[`scripts/icosa-remote-demo.sh`](scripts/icosa-remote-demo.sh). Every script in
[`scripts/`](scripts) carries a long header explaining exactly what it runs and what it measured.

Two gotchas that cost real time to find, so you do not have to:

- Run `vkcube` with `--gpu_number 0`. It defaults to the discrete GPU, which on at least one NVIDIA
  part returns `VK_ERROR_DEVICE_LOST` — a failure that is **not** Rayland's, and took three days to
  prove so.
- Never set `VN_DEBUG=no_abort`. Mesa's stall abort *is* the stall detector.

## Documentation

Rayland is documented far past the usual standard, deliberately. Start wherever fits:

| If you want… | Read |
|---|---|
| The whole project in one file, current | [`docs/OVERVIEW.md`](docs/OVERVIEW.md) |
| The full architecture, and what exists vs. must be invented | [`docs/design/2026-07-13-native-remote-wayland-gpu.md`](docs/design/2026-07-13-native-remote-wayland-gpu.md) |
| **The story of how it was built — including every wrong turn** | [`docs/DIARY.md`](docs/DIARY.md) |
| What is shipped, in flight, or an open seam — visually | `project-map.html`, opened from disk |
| Why the socket carries nothing | [`docs/design/2026-07-15-venus-ring-findings.md`](docs/design/2026-07-15-venus-ring-findings.md) |
| What remoting actually costs, measured | [`docs/c1-the-network.md`](docs/c1-the-network.md) |
| The conventions this repository is built to | [`CLAUDE.md`](CLAUDE.md) |

**A word about the diary.** [`docs/DIARY.md`](docs/DIARY.md) is not a changelog — git has that. It is
the reasoning as it actually unfolded, including the beliefs that turned out to be wrong, which are
left in place and marked corrected rather than quietly edited away. It exists for two reasons: so
that whoever tries this idea again does not have to re-walk the dead ends, and because this software
was written by an AI under human supervision, and trust in that cannot be asserted — only earned by
showing the work honestly, mistakes included.

## Building

```sh
cargo build              # the workspace: eighteen crates
cargo test               # GPU-backed tests need a real GPU and a Wayland session
```

## License

Rayland is an application and is licensed **GPL-3.0-or-later**. Individual library crates that emerge
from the project may be licensed LGPL-3.0-or-later; each crate declares its own license in its
manifest.
