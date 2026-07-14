# SP1 — Onto the Screen (how to run it)

SP1 shows the streamed triangle in a **live Wayland window** on S, instead of writing a PNG.
The client emits the same command stream as SP0; the server replays it on the GPU, copies the
result into a `wl_shm` buffer, and displays it in an `xdg_toplevel` window.

## Run it (on a machine with a Wayland session)

In one terminal, start the server (it waits for one connection):

    cargo run -p rayland-server            # listens on 127.0.0.1:9000

In another terminal, run the client:

    cargo run -p rayland-client            # connects to 127.0.0.1:9000

A window titled "Rayland — SP1" appears showing a **red triangle on a blue background**.

- Close the window → the client exits (the server closed its liveness connection).
- Or press Ctrl-C in the client → the window closes (the server saw the client disconnect).

Either side ending tears down both — the window and the client always stop together.

## Headless / PNG fallback

Without a Wayland session (or to reproduce the SP0 PNG), ask the server to write a file and
exit instead of opening a window:

    cargo run -p rayland-server -- --png out.png
    cargo run -p rayland-client

Open `out.png`: the same red-triangle-on-blue image.

## Tests

    cargo test                             # unit tests: the pixel swizzle + the liveness wait

The on-screen window itself is verified by eye (above); CI stays compositor-free. The Wayland
crates use the dlopen backend, so building needs no `libwayland` package. See the
[SP1 design spec](design/2026-07-14-sp1-onto-the-screen.md) for why.

## Known SP1 limitations (deferred by design)

- Fixed-size window (the frame's size); no resize re-render — SP2+/SP3.
- CPU round-trip through `wl_shm`; zero-copy dmabuf is SP3.
- One frame per run; a live frame stream is a later sub-project.
