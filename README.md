# Rayland

**Native remote GPU rendering for Wayland.** Run a graphical application on one
machine, but render and display it on *another* machine — the one with the powerful
GPU and the monitor you're actually looking at — by sending a **command stream**
across the network instead of a stream of pixels.

> **Status: working prototype.** An **unmodified** Vulkan application — using the
> stock Mesa Venus driver, no fork, no patch, no recompile — renders on a remote
> GPU across a **real network** and reads its pixels back **frame-perfect**
> (bit-identical to running natively, zero stale frames across repeated
> multi-machine runs). This is proven on deliberately small workloads; making it
> hold for arbitrary applications is the work in progress. See
> [Where the project stands](#where-the-project-stands). The name nods to Sun Ray
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
world (Venus, virglrenderer, gfxstream; the whole stack ships in ChromeOS Crostini).
Rayland **reuses that command-stream machinery, but replaces its locality, trust,
lifetime, and failure assumptions** — which turned out to be a protocol-design problem
in its own right, not a transport swap. The project's pivotal early finding: the socket
everyone assumed carried the commands carries almost none of them. The application's
Vulkan calls live in a **shared-memory ring** whose file descriptor crosses a Unix
socket exactly once — and *neither a shared page nor a file descriptor survives a
network*. Rayland's daemons therefore watch that ring on C, relay its deltas and the
memory the commands reference, and faithfully reconstruct on S the shared memory the
application never knew it was sharing. (The write-up of that discovery:
[`docs/design/2026-07-15-venus-ring-findings.md`](docs/design/2026-07-15-venus-ring-findings.md).)

Read the full architecture — including the honest list of what already exists versus
what must be invented — in
[`docs/design/2026-07-13-native-remote-wayland-gpu.md`](docs/design/2026-07-13-native-remote-wayland-gpu.md).

## Where the project stands

The work is organized as a walking skeleton: get something rendering end-to-end first,
then harden. Two arcs so far.

**Arc (s) — proof of the loop (complete).** A hand-rolled command protocol pushed end
to end in four steps: a triangle serialized on C and replayed on S's real GPU into a
bit-identical PNG (SP0); into a live Wayland window (SP1); over QUIC (SP2); presented
zero-copy via dmabuf with a `wl_shm` fallback (SP3). This arc's protocol could never
speak for arbitrary applications — that was never its job. Its code and tests remain.

**Arc (c) — unmodified applications (current).** The real product: retire the
hand-rolled protocol and capture *stock* applications through Mesa's Venus driver.

- **C0 (done):** an ordinary, unmodified Vulkan program, captured by the unpatched
  Venus ICD and replayed through Rayland's embedded virglrenderer — PNG bit-identical
  to native. Same machine, local socket. No Mesa fork is needed, ever: the vtest
  protocol's "host" is whoever allocates the ring, and Rayland's C-side daemon simply
  is that host.
- **(c)1 (done):** the network. C's daemon relays ring deltas and memory blobs to S's
  daemon over a real link; commands execute on S's GPU; results present in a Wayland
  window on S. Forward path bit-identical on trivial workloads.
- **(c)2 (readback: done; mapped memory: in progress):** the return path — an
  application that renders remotely and *reads its pixels back* — is frame-perfect
  over a real network (zero stale frames across twenty two-machine runs),
  after a chain of instructive dead ends recorded in
  [`docs/DIARY.md`](docs/DIARY.md). Still open: per-frame round-trip latency,
  multi-queue applications, and the genuinely hard problem this sub-project owns —
  memory the application writes through `vkMapMemory` with **no API call to
  intercept**, over a link where no shared page can exist.
- **(c)3 — content-addressed assets** and **(c)4 — real/complex applications, GL via
  Zink** follow.

Current honest limits: small single-queue workloads, Vulkan only, Linux only, and the
return path is latency-bound rather than bandwidth-bound. The full current state, in
detail, lives in [`CLAUDE.md`](CLAUDE.md); the story of how it got here — including
what went wrong and why — is [`docs/DIARY.md`](docs/DIARY.md).

## Companion project: Parhelion

[Parhelion](https://github.com/perpetualbits/parhelion) is a sibling project: a
Wayland compositor with microkernel discipline, built to be — among other things —
Rayland's **reference S-side host**. Its scene graph treats "a buffer rendered by a
sandboxed replay service on behalf of a machine across the network" as a first-class
texture source, which gives Rayland's buffer-by-token and sandboxed-replay designs a
native home to land in first, before they are proposed to the wider ecosystem.
Rayland does not depend on Parhelion: presenting into any ordinary Wayland compositor
remains supported and is the portable baseline.

## Building

A Cargo workspace of seventeen crates:

```sh
cargo build
```

Building the full workspace requires the system `libvirglrenderer` development
package (located via `pkg-config`) for the S-side engine crate. The C-side crates
deliberately have **no** GPU dependencies — C may be a weak or headless machine —
and enforce that with tests. `cargo test` is safe on a GPU-less machine: tests that
need a real Venus-capable render node or a live Wayland compositor detect their
absence and skip cleanly.

## License

Rayland is an application and is licensed **GPL-3.0-or-later**. Individual library
crates that emerge from the project may be licensed LGPL-3.0-or-later; each crate
declares its own license in its manifest.
