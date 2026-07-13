# Rayland

**Native remote GPU rendering for Wayland.** Run a graphical application on one
machine, but render and display it on *another* machine — the one with the powerful
GPU and the monitor you're actually looking at — by sending a **command stream**
across the network instead of a stream of pixels.

> **Status: early design + bootstrap.** This repository currently contains the
> architecture/design document and a placeholder crate that reserves the `rayland`
> name on crates.io. There is no working software yet. The name nods to Sun Ray
> (thin client, compute elsewhere, display here) and rhymes with Wayland.

## The idea, plainly

Rayland borrows X11-era vocabulary, which is the *reverse* of how "client" and
"server" are used in the cloud. Read this carefully:

| Term | Meaning in Rayland | Example |
|------|--------------------|---------|
| **S** — "server" side | Where **you sit**: keyboard, mouse, **display, GPU**, the Wayland compositor, working drivers. | Your capable laptop. |
| **C** — "client" side | Where the **application executable runs**. Possibly weak, or a different CPU architecture, or headless. | A RISC-V single-board computer, or a big CPU-only hypervisor. |

The application runs on **C**. To draw, it emits a *command stream* — the language of
rendering ("draw these triangles, with this shader, sampling this texture") — which
crosses the network to **S**. There, **S's GPU** does the real work and the result
appears on **S's** display.

The key bet: **ship commands, not pixels.** This is the modern heir to X11's
network-transparent graphics, rebuilt for Vulkan and modern OpenGL. A video stream of
already-rendered pixels *is* supported as a fallback — but only as a fallback, because
in the target setup the weak machine (C) is exactly the wrong place to run an expensive
video encoder.

## Why this is hard (and why it's not hopeless)

Wayland deliberately assumes the application and the compositor share memory and a GPU:
the app renders into a GPU buffer and passes a *file-descriptor handle* over a local
socket. You cannot send a file descriptor across a network, so remoteness isn't a
missing feature — it's an *excluded assumption*.

The encouraging part: the hardest component — serializing a Vulkan command stream and
replaying it on a remote GPU — already exists and is battle-tested in the virtual-machine
world (Venus, virglrenderer, gfxstream; and the whole stack ships in ChromeOS Crostini).
Rayland's job is largely to swap that stack's *transport* from "shared memory inside one
computer" to "a real network," and to add the genuinely new pieces that a network needs.

Read the full architecture — including the honest list of what already exists versus
what must be invented — in
[`docs/design/2026-07-13-native-remote-wayland-gpu.md`](docs/design/2026-07-13-native-remote-wayland-gpu.md).

## Building

Nothing to build yet beyond a placeholder crate:

```sh
cargo build
```

## License

Rayland is an application and is licensed **GPL-3.0-or-later**. Individual library
crates that emerge from the project may be licensed LGPL-3.0-or-later; each crate
declares its own license in its manifest.
