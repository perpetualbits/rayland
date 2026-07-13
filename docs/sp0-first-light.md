# SP0 — First Light (how to run it)

SP0 is Rayland's **walking skeleton**: the narrowest slice that proves the central bet.
A program on the **C** ("client") side emits a stream of rendering *commands* describing a
single coloured triangle; that stream crosses a plain TCP socket to the **S** ("server")
side; **S replays it on a real Vulkan GPU** into an off-screen image; and the result is
written to a PNG. No pixels are sent over the wire — only commands and the triangle's
vertex data.

(If the S/C vocabulary looks backwards, that is deliberate — it is the X11-era convention,
the *reverse* of cloud usage. See the [README](../README.md): **S** is where you sit, with
the display and GPU; **C** is where the application runs.)

## Run it by hand

In one terminal, start the server. It binds `127.0.0.1:9000`, waits for exactly one
connection, renders it, writes `out.png`, and exits:

```sh
cargo run -p rayland-server            # listens on 127.0.0.1:9000, writes out.png
```

In a second terminal, run the client. It connects, sends the triangle command stream, and
disconnects:

```sh
cargo run -p rayland-client            # connects to 127.0.0.1:9000
```

Open `out.png`: a **red triangle on a blue background** — rendered on the server's GPU
purely from the client's command stream.

Both programs take optional arguments if you want to change the defaults:

```sh
cargo run -p rayland-server -- 127.0.0.1:9000 out.png   # <listen-addr> <output-png>
cargo run -p rayland-client -- 127.0.0.1:9000           # <server-addr>
```

## Run the tests

The pixel assertions are deterministic and need no human inspection — the tests check that
the centre pixel is the triangle's colour and the corners are the clear colour:

```sh
cargo test                             # includes the end-to-end TCP render test
```

## Running without a GPU (what CI does)

You do not need a physical GPU. Mesa's **lavapipe** is a CPU software implementation of
Vulkan; the entire pipeline runs on it unchanged. Install Mesa's Vulkan drivers, then point
the Vulkan loader at the lavapipe driver manifest:

```sh
sudo apt-get install -y mesa-vulkan-drivers libvulkan1 vulkan-tools

# The manifest's filename varies by distribution — lvp_icd.json or lvp_icd.x86_64.json —
# so discover it rather than hardcoding, exactly as CI does:
export VK_ICD_FILENAMES="$(ls /usr/share/vulkan/icd.d/lvp_icd*.json | head -n1)"

cargo test
```

### Pitfall: a machine with a real GPU may *hide* lavapipe

If your system already has a working GPU driver, the Vulkan loader may have a global
preference that filters lavapipe out. On such a host, forcing lavapipe for a single run
looks like:

```sh
VK_LOADER_DRIVERS_SELECT='*lvp*' cargo test
```

Conversely, on a machine whose loader is configured to *only* accept a hardware vendor's
driver, that same variable is how you re-enable lavapipe. CI runners have no GPU, so they
need none of this — pointing `VK_ICD_FILENAMES` at the discovered manifest is enough.

## What this does and does not prove

SP0 proves the **core loop**: commands produced on one machine replay correctly on
another machine's real GPU. It deliberately does **not** yet intercept a real application's
Vulkan calls, use QUIC, put anything on screen, or run across two machines — those arrive
in later sub-projects (SP1–SP5). See the
[SP0 design spec](design/2026-07-13-sp0-first-light.md) and the
[parent architecture](design/2026-07-13-native-remote-wayland-gpu.md).
