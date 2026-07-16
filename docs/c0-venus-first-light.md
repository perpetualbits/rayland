# C0 — Venus First Light (how to run it)

C0 is the moment Rayland stopped drawing its own triangle and started running **somebody else's
program**.

Every slice before it (SP0–SP3) proved a real piece of plumbing — a wire protocol, a GPU render, a
QUIC transport, a zero-copy window — but they all shared one embarrassing property: **the triangle
was ours.** `rayland-client` hand-built a fixed command stream, `rayland-server` knew exactly what
to do with it, and the two agreed because we wrote both sides. That is a fine way to build a
skeleton, and it is not a way to run an application.

C0 replaces that with the real thing:

- **The application is real, unmodified, and does not know Rayland exists.** `rayland-refapp` is an
  ordinary off-screen Vulkan triangle program. It has **zero `rayland-*` dependencies**. It never
  mentions Venus, vtest, virglrenderer, sockets, or remoting. It cannot tell whether it is being
  remoted, and *that is the result* — an application you had to modify would prove nothing about
  real applications.
- **The capture is real, and it is not ours.** Mesa's **Venus** driver captures the app's Vulkan
  calls. We wrote none of it.
- **The replay is real, and it happens on the GPU.** Our host FFI-embeds **virglrenderer**, which
  replays the stream on this machine's actual Intel GPU.

**The result: the PNG the app produces through Rayland is bit-identical to the PNG the same binary
produces natively — 0 of 16384 bytes differ, 15/15 runs.**

> **Read this before you believe too much of it.** C0 proves the command stream **replays
> faithfully on one machine, over shared memory.** It proves **nothing about remoting.** The proof
> works *because* the client and the host share memory — and that sharing is exactly what a network
> takes away. Where that line falls, and what it costs, is
> [the Venus ring findings](design/2026-07-15-venus-ring-findings.md), which is the required
> reading for anyone touching (c)1.

---

## What the pieces are (for a reader who does not know this domain)

Four words do all the work here. If you already know Venus, skip to "How to run it".

**Vulkan** is the low-level API an application uses to draw. Normally its calls go into a driver
that talks straight to the GPU in the same machine.

**Venus** is a Vulkan driver **with no hardware behind it**. It is a real, conformant Mesa driver
(shipped as `libvulkan_virtio.so`) that, instead of programming a GPU, **serializes every Vulkan
call into bytes and sends them somewhere else to be executed.** It was built for virtual machines —
an app inside a VM draws, the host outside does the drawing. Rayland uses it *outside* any VM, as
the **capture** half of the problem: already written, already in every Mesa install, already
hardened. This is CLAUDE.md's locked decision 1a — *reuse, do not reinvent* — cashed in.

**virglrenderer** is the **replayer**: it takes that serialized stream and executes it against a
real GPU. It also comes from the VM world, and it is hardened against precisely our threat model —
**an untrusted party driving the host's GPU**. That hardening is not incidental; it is a large part
of why borrowing this engine is a better idea than writing one.

**vtest** is how they talk without a VM. Inside a VM, Venus reaches the host through a kernel device
(`virtgpu`). Outside, no such device exists — so Mesa has a second backend that speaks a simple
protocol over a **Unix socket** instead. That backend is why Venus works with no VM at all, and
C0's host implements it (`crates/rayland-vtest/src/vtest.rs` — it lived in `rayland-engine` during
C0; (c)1 Task 1 moved it into its own GPU-free crate).

### The architecture

```
      C side (the app — unmodified)              S side (the GPU machine)
 ┌───────────────────────────────┐        ┌────────────────────────────────────────────┐
 │ rayland-refapp                │        │ examples/vtest_serve (the host harness)    │
 │  a plain Vulkan triangle;     │ vtest  │  ├ vtest-protocol server (accepts socket)  │
 │  zero Rayland dependencies    │ ─────► │  ├ VirglEngine: FFI → libvirglrenderer     │
 │                               │  Unix  │  └ replays on the real GPU (Intel Iris Xe) │
 │ Mesa VENUS ICD                │ socket │                                            │
 │  serializes Vulkan → …        │ ◄───── │  passes back a shared-memory fd            │
 └───────────────────────────────┘SCM_RIGHTS└──────────────────────────────────────────┘
                │                                            ▲
                │        ┌────────────────────────────┐      │
                └───────►│  the shared-memory RING    │──────┘
                         │  (128 KiB + control words) │
                         │  where the commands ACTUALLY go
                         └────────────────────────────┘
```

**The dashed box is the important one, and it is the thing C0 discovered.** The Unix socket in the
diagram looks like the data path. It is not. It carries ring *management* only — *"here is a ring"*,
then a few doorbells saying *"come and look"*. **100% of the application's Vulkan commands travel
through the shared-memory ring**, which the host allocates and the client `mmap`s after we hand it a
file descriptor over `SCM_RIGHTS`. There is no protocol message when the app draws.

That is the single most consequential thing C0 learned, and it is why
[the ring findings](design/2026-07-15-venus-ring-findings.md) exists as a separate document. It is
also why (c)1's original one-line scope — *"swap the local socket for QUIC"* — **is disproved:
there is no socket carrying commands to swap.**

---

## How to run it

Two terminals. Every command below was run on this host, exactly as written, while writing this doc.

Terminal A — the host (binds the socket, serves **one** connection, exits):

    cargo run -p rayland-engine --example vtest_serve -- /tmp/rl-c0.sock

It reports:

    listening on /tmp/rl-c0.sock (one connection, then exit)
    engine up on /dev/dri/renderD128
    client connected; serving vtest
    session ended cleanly: VtestOutcome { rendered_resource_id: Some(6), context_id: Some(1), submitted_batches: 8 }

**`submitted_batches` will very likely differ on your machine — 8 and 10 have both been seen on
this one, and neither is wrong.** The other two fields are deterministic and should match:
`context_id: Some(1)` is the one context per connection, and `rendered_resource_id: Some(6)` is the
application's readback buffer (64×64×4 = 16384 bytes — the blob it maps to read its own pixels).

The batch count varies because **it is timing-dependent, not a property of the workload**. Mesa
only rings the doorbell when our ring thread has actually parked (see
[the ring findings](design/2026-07-15-venus-ring-findings.md)), so the number of batches counts *how
often the host happened to be asleep*, not how much work the application did — byte-identical ring
traffic has produced anywhere from 1 to 4 notifications. **Never read this number as a measure of
anything.** It is printed because it proves the session carried real traffic, not because its value
means something.

Terminal B — the unmodified application, pointed at Mesa's Venus ICD:

    env -u VK_LOADER_DRIVERS_SELECT \
        VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json \
        VN_DEBUG=vtest \
        VTEST_SOCKET_NAME=/tmp/rl-c0.sock \
        ./target/debug/rayland-refapp /tmp/venus.png

(Build it first with `cargo build -p rayland-refapp`.) The same binary, run with **no** environment
at all, renders on the local GPU instead:

    ./target/debug/rayland-refapp /tmp/native.png

`cmp /tmp/venus.png /tmp/native.png` reports no difference. **Nothing about the binary changed
between those two runs — only the environment did.** That is the entire claim, and it is why the
app is forbidden from knowing anything about Rayland.

### The environment pitfalls — every one of these cost real debugging time

These are not incantations to copy blindly; each one has a mechanism, and three of the four **fail
silently**, which is what made them expensive.

- **`VN_DEBUG=vtest` is REQUIRED, and its absence fails silently.** This is the big one. Mesa's
  Venus ICD **prefers its `virtgpu` backend** and only tries vtest when explicitly told to. Setting
  `VTEST_SOCKET_NAME` is **not** sufficient to select vtest — that variable is read inside
  `vtest_init`, which runs **after** the ICD has already chosen a backend. So the socket name
  cannot *cause* vtest to be selected; it only configures vtest once vtest has already won. Without
  `VN_DEBUG=vtest`, the client fails with `ERROR_INITIALIZATION_FAILED` and **never connects to the
  socket at all** — so the host logs no incoming connection, and the failure looks like a
  Rayland-side hang rather than a client that never tried to reach us.
- **`env -u VK_LOADER_DRIVERS_SELECT`.** If the host has that set to a driver filter (e.g.
  `*intel*` — this machine does), the Vulkan loader **silently hides the Venus ICD** and the client
  never connects. The failure presents as "no Vulkan devices", which looks nothing like the actual
  cause.
- **No validation layers.** Validation and Venus do not mix; enabling one produces failures that
  have nothing whatsoever to do with the code under test.
- **The socket path must be short.** `sockaddr_un::sun_path` is **108 bytes**. A long path (e.g.
  nested under a scratch or temp-session directory) overflows it and `UnixListener::bind` fails with
  *"path must be shorter than SUN_LEN"*. Use something like `/tmp/rl-c0.sock`. This one at least
  fails **loudly and immediately** at `bind`, unlike the three above.

### The automated proof

    cargo test -p rayland-engine --test refapp_venus_e2e -- --nocapture

which prints:

    OK: the venus-rendered image is BIT-IDENTICAL to the native one (16384 bytes)
    OK: an unmodified Vulkan application rendered a red triangle on blue by shipping its COMMAND
        STREAM to Rayland's engine, which replayed it on /dev/dri/renderD128

The test runs the *same binary* twice — once natively, once through Venus into our engine — and
compares. It **skips cleanly** (no GPU, no Venus) so CI stays green. The app's native correctness is
separately pinned by `cargo test -p rayland-refapp`, so an end-to-end failure localizes to the
engine path rather than to the app.

**The test is deliberately not vacuous**, which matters because "bit-identical" is also what a
*silently native* second run would produce. Three things make the Venus path load-bearing:
`VK_ICD_FILENAMES` restricts the loader to Venus with **no native fallback**; the test **panics** if
the app exits without ever connecting to the socket; and it asserts `context_id == Some(1)` — a real
Venus context really was created on our engine by a real client speaking the real protocol.

The test **enforces** the pixel assertions but only **reports** bit-equality. That is deliberate: an
exact match is *stronger* than C0's claim needs, so a future legitimate divergence (a different GPU,
a different Mesa) should be surfaced loudly to a human rather than turned into a red test.

---

## Why bit-identical, and why that is less impressive than it sounds

It is worth being honest about this, because "bit-identical" invites over-reading.

It is **expected**, on reflection. Both runs execute on the **same physical GPU** with the **same
Mesa stack underneath**. Venus does not re-render or re-interpret anything — it serializes the
commands, and virglrenderer replays them onto that same driver and that same hardware. Identical
commands, identical hardware, identical rasterisation. **A cross-machine or cross-vendor run — (c)1's
job, since this is the Venus/engine path, not SP2's already-complete and unrelated `postcard`
transport — is the first place bit-equality could legitimately break**, and that is exactly the kind
of thing the test is built to report rather than fail on.

What the result *does* prove is real and worth having: **the command stream survives the round trip
through Venus, our vtest server, our FFI boundary, and virglrenderer without a single byte of the
picture changing.** No dropped state, no mis-framed command, no lost precision.

---

## Who writes the PNG — and why it is the app, not the host

This is a deviation from the C0 spec worth understanding, because it looks like a shortcut and is
not. **The spec said the *host* would extract the rendered image and write the PNG. It does not.
The application does its own readback and writes its own PNG — which is what any normal Vulkan
program does.**

The reason is not convenience. The host-side readback the spec described **had nothing to read**:

- The app's `VkImage` is created by **Venus commands inside the ring**. It never appears in our
  engine's resource table, so `RenderEngine::read_back` could not see it even in principle.
- Task 4b confirmed this independently, from the other direction: a **`DEVICE_LOCAL` image produces
  no blob at all**. There is no host-visible resource corresponding to the picture.
- The blobs a live client *does* create are its command ring and its staging/reply pools — not
  rendered images. And blob resources have no queryable pixel format, so "read back the last blob"
  is not a shortcut to a frame.

So the app calls `vkMapMemory` on a `HOST_VISIBLE` buffer and copies the image into it, exactly as
an ordinary offscreen Vulkan program would. That readback **creates its own shared blob** — Task 4b
caught it in the act: `res=6`, **16384 bytes = 64 × 64 × 4 exactly**, holding the blue clear colour.
**That blob is the pixel return path.**

**The consequence, which is not a dead end but must not be forgotten:**
`RenderEngine::read_back`/`EngineFrame` (built in Task 3) are **off C0's critical path — but they
are not dead code.** SP1 and (c)1 must put a frame **on a screen**, and that needs **host-side
pixels**. C0's app-side readback deliberately sidesteps that question; it does not answer it.
Answering it was Task 4c, **deferred by the owner**, and it is an open question that (c)1/SP1 must
close. The most promising lead on record: `blob_id != 0` cleanly discriminates the app's own memory
(the vertex buffer, the readback buffer) from Venus's internal plumbing (ring, reply arena, staging
pool). See the [ring findings](design/2026-07-15-venus-ring-findings.md) §8.5.

---

## The reliability result — this was the actual gate

C0's real risk was never the triangle. It was this:

The (c)0 feasibility spike proved Venus-without-a-VM works, but found the debug host
(`virgl_test_server`) **flaky** — repeated `vkEnumeratePhysicalDevices → INITIALIZATION_FAILED`,
with native Vulkan perfectly healthy. That left one question that gated the entire sub-project, and
arguably the entire architecture: **is the flakiness in the *library*, or only in the throwaway test
harness?** If `libvirglrenderer` itself were flaky, C0 would have had to fix it — and if it proved
unfixable, that was the trigger to reconsider the whole engine choice (GFXReconstruct, crosvm-gpu).

**Answer: the library is not flaky. The harness was.** Repeated init → context → replay → teardown
is reliable:

    cargo test -p rayland-engine --test reliability

25× `new → context → drop`, plus 25 simultaneous contexts, plus the singleton guard — all passing on
the real GPU, skipping cleanly without one. End to end, the full app-through-Venus drive ran
**15/15** clean.

Two things had to be right to get there, and both are the kind of detail that is invisible until it
bites:

- **`VIRGL_RENDERER_RENDER_SERVER` (`1 << 9`) is required** for a Venus context — without it,
  `virgl_renderer_init` returns `EINVAL`. It forks a render-server subprocess that **also sandboxes
  the client**, which is a threat-model bonus we get for free.
- **`get_drm_fd` must open a *fresh* fd per call**, because virglrenderer takes ownership and closes
  it. Handing it the same fd twice is a use-after-close.

---

## Scope — stated honestly

C0 is **same machine, local Unix socket, offscreen**. That is the whole of it.

**What C0 proved:** a real, unmodified Vulkan application's command stream, captured by Mesa's
Venus, replays faithfully on a real GPU through a Rayland-owned host that FFI-embeds virglrenderer —
reliably, and with a bit-identical picture.

**What C0 did NOT prove — and this is the part to carry forward:**

- **Nothing about remoting.** The proof works **because** the client and host **share memory**.
  The commands reach the host by being written into pages both processes have mapped, using a file
  descriptor passed over `SCM_RIGHTS`. **Neither the shared page nor the fd survives a network.**
  QUIC has no fd-passing, and two machines cannot share a page. **(c)1 is a protocol design task,
  not a transport substitution.**
- **Nothing about real applications.** The out-of-line command path
  (`vkExecuteCommandStreamsMESA`) triggers when a single submission exceeds **8192 bytes**, at which
  point the ring stops containing commands and starts containing *pointers into other memory*.
  C0's largest input is a **1008-byte SPIR-V**, so this **never triggered once**. "The ring is the
  whole stream" is true for a triangle and **will break on the first real app**.
- **Nothing about sustained rendering.** Ring wrap never happened (peak **7.58%** of the buffer).
  Multi-threaded clients were never tested (`ring_idx == 0` is confirmed for a single thread only,
  and our fence path hardcodes ring 0). The 5-second fence timeout was **never exercised** — it
  lives on `read_back`, which C0's path does not reach by design.
- **Nothing on a screen.** Offscreen only; a PNG, exactly like SP0.
- **No coherence protocol of our own**, no content-addressed assets, no sandboxing beyond what
  virglrenderer's render server gives us for free, no GL, no complex app.

And underneath all of it sits the problem no engine choice can dodge, documented in full in the
[ring findings](design/2026-07-15-venus-ring-findings.md) §6: **`vkMapMemory` has no API call to
intercept.** Applications write vertices and textures **straight into mapped memory, with no Vulkan
call at all**. There is nothing on any wire to forward. That is modern Vulkan's shape, not Venus's
failing, and **any** remote Vulkan must answer it.

---

## Where this sits in the arc

SP0–SP3 built Rayland's own hand-rolled `postcard` protocol end to end: a triangle over TCP, then a
window, then QUIC across a real network, then zero-copy dmabuf presentation. **Arc (c) replaces that
protocol with the real Venus engine.** The two paths coexist today — the SP0-era code is untouched
and its tests still pass — until arc (c) fully supersedes it.

| slice | what it adds | status |
|---|---|---|
| **C0** | The real engine: unmodified app → Venus → our virglrenderer host → real GPU → PNG. Same machine. | **this document** |
| **(c)1** | The network. **Rescoped by C0's findings** — not "swap the socket for QUIC" (there is no socket carrying commands) but *"design the protocol that ships the ring"*. Also owes SP1 host-side pixels for the screen. | next |
| **(c)2** | Mapped-memory coherence — the `vkMapMemory` problem, in full. | later |
| **(c)3** | Content-addressed assets. | later |
| **(c)4** | Real/complex applications; GL via Zink. | later |

**Required reading before (c)1:** [the Venus ring findings](design/2026-07-15-venus-ring-findings.md).
(c)1's design is meant to be built directly on it. The C0 design spec
([`design/2026-07-14-c0-venus-first-light.md`](design/2026-07-14-c0-venus-first-light.md)) records
what C0 was *planned* to be, with a dated amendment recording where reality overruled the plan.
