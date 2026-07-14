# SP2 — Real Transport (how to run it)

SP2 carries the triangle command stream over **QUIC** instead of TCP, so the client can run on
a different machine and CPU architecture from the server. The server still renders on its GPU
and shows the result in a window (SP1).

## Local run (one machine)

Terminal A — the server (binds a QUIC/UDP port, waits for one connection):

    cargo run -p rayland-server            # listens on 127.0.0.1:9000 (QUIC)

Terminal B — the client:

    cargo run -p rayland-client            # connects to 127.0.0.1:9000 over QUIC

A window shows a red triangle on blue. Close it → the client exits; Ctrl-C the client → the
window closes (SP1 teardown, now over QUIC).

Headless / PNG fallback:

    cargo run -p rayland-server -- --png out.png
    cargo run -p rayland-client

## Cross-machine run (the SP2 milestone)

The server (S) runs on the machine with the GPU and display (the laptop, `dop561`); the client
(C) runs on another machine over the LAN. The client does no GPU work, so it builds on a weak
or foreign-architecture host.

1. On S, start the server bound to the LAN interface (or `0.0.0.0`) so C can reach it:

       cargo run -p rayland-server -- 0.0.0.0:9000

   Note S's LAN IP (e.g. `192.168.x.y`). QUIC is UDP — ensure UDP/9000 is allowed.

2. On C, build and run the client **natively** (no cross-compilation needed):

   - rv64 SBC:  `ssh milkv.localdomain`, then in a checkout of this repo:

         cargo build -p rayland-client
         cargo run -p rayland-client -- 192.168.x.y:9000

   - apollo:    `ssh apollo.localdomain -i ~/.ssh/keys.d/stationoost/id_ed25519`, then:

         cargo run -p rayland-client -- 192.168.x.y:9000

3. The triangle — emitted on C — appears in the window on S, rendered by S's GPU. This is the
   remote-app-on-your-screen milestone: a program on the rv64 board drawing on the laptop.

## Tests

    cargo test                             # unit + the QUIC loopback e2e (asserts pixels)

The transport's TLS crypto provider is `ring` (see Task 1's decision: the pure-Rust
`rustls-rustcrypto` provider was spiked first but cannot drive quinn's QUIC packet crypto, so
`rustls` falls back to `ring`, which is C+assembly compiled at build time via the `cc` crate).
This means every machine that *builds* this workspace (including the rv64 SBC, since the
client builds and runs natively there — no cross-compilation) needs a working C toolchain
(`gcc`/`clang`); it does not need any system TLS/crypto *library*, since `ring` vendors and
statically links its own crypto code. CI (`ubuntu-latest`) already has a C compiler, so no CI
changes are needed. The channel is **encrypted but not authenticated** in SP2 (a loudly-named
skip-verify); real authentication is SP4. See the
[SP2 design spec](design/2026-07-14-sp2-real-transport.md).

## Known SP2 limitations (deferred by design)

- One bidirectional stream; the multi-stream sibling protocol is SP3.
- Encrypted but unauthenticated (skip-verify); SSH-bootstrap + real trust is SP4.
- CPU round-trip through `wl_shm`; zero-copy dmabuf is SP3.
