/*
 * project-map.js — the DATA for the Rayland repository.
 *
 * This is the single source of truth the renderer (project-map.html) reads. It is loaded
 * with a plain <script> tag (NOT fetch) so the page works when opened directly from disk
 * over file://. Everything here is derived from the repository's own record — CLAUDE.md,
 * docs/DIARY.md, the design specs under docs/design/, and the crates that actually exist —
 * not invented. Status reflects the roadmap and merged reality, not aspiration.
 *
 * Keep this in sync with the roadmap whenever the project's status changes.
 */
window.PROJECT_MAP = {
  project: {
    name: "Rayland",
    tagline: "Native remote GPU rendering for Wayland — ship the commands, not the pixels.",
    repo: "rayland",
    updated: "2026-07-25"
  },

  // Each status is a colour band the renderer keys on. The hint is the plain-language meaning.
  statuses: {
    done:    { label: "Shipped",   hint: "Built, tested, merged to main." },
    active:  { label: "In flight", hint: "Under construction now, on a live branch." },
    planned: { label: "Planned",   hint: "Designed or scoped — not yet built." },
    seam:    { label: "Open seam", hint: "An unsolved boundary — where the hard research still lives." }
  },

  // Architectural bands, drawn top → bottom. The app sits at the top; its commands descend
  // through C, cross the wire, and are executed on S's GPU at the bottom.
  layers: [
    { id: "app",         label: "Applications & fixtures", hint: "Unmodified programs and the fixtures that probe the hard cases. They know nothing about remoting." },
    { id: "cside",       label: "C · the application machine", hint: "The weak, possibly headless or RISC-V machine where the program actually runs. Links no GPU code." },
    { id: "wire",        label: "Wire & transport", hint: "The messages that cross the network, their framing, and the QUIC that carries them." },
    { id: "sside",       label: "S · the GPU machine", hint: "The strong machine: real GPU, real display, the compositor the user is looking at." },
    { id: "engine",      label: "GPU engine", hint: "The reused Venus / virglrenderer replay engine, driven on S's real hardware." },
    { id: "foundations", label: "Foundations & legacy", hint: "Shared geometry/maths, the SP0-era hand-rolled arc, and the crates.io facade." }
  ],

  nodes: [
    /* ---------------------------------------------------------------- app layer */
    {
      id: "refapp", label: "rayland-refapp", layer: "app", status: "done",
      tags: ["C0", "fixture"],
      desc: "An ordinary offscreen Vulkan triangle program with zero rayland-* dependencies and no knowledge of remoting. Its whole value is that it is boring and typical — the captured workload C0 proved bit-identical to native through the real engine.",
      files: ["crates/rayland-refapp"],
      specs: [
        { label: "C0 · Venus first light", href: "docs/c0-venus-first-light.md" },
        { label: "C0 design spec", href: "docs/design/2026-07-14-c0-venus-first-light.md" }
      ],
      parts: [
        { label: "Red-on-blue triangle", status: "done", desc: "64×64, matching SP0, drawn and read back via vkMapMemory." },
        { label: "Native render test", status: "done", desc: "Proves the app renders correctly with no engine in the loop." }
      ],
      deps: []
    },
    {
      id: "icosa-cpu", label: "rayland-icosa-cpu", layer: "app", status: "done",
      tags: ["(c)2", "fixture"],
      desc: "Fixture A for the mapped-memory problem: a spinning icosahedron textured with a fractal it computes on its own CPU and writes into persistently-mapped HOST_COHERENT memory every frame — with no flush, so nothing on the wire says a megabyte changed. Exactly the case with nothing to intercept.",
      files: ["crates/rayland-icosa-cpu"],
      specs: [{ label: "Icosahedron fixtures", href: "docs/icosa-fixtures.md" }],
      parts: [
        { label: "Per-frame CPU fractal", status: "done", desc: "A megabyte of mapped writes per frame, uninterceptable." },
        { label: "Loopback bit-identical", status: "done", desc: "0/120 frames differ across the (c)1 loopback relay." }
      ],
      deps: ["icosa-vk", "icosa-core"]
    },
    {
      id: "icosa-gpu", label: "rayland-icosa-gpu", layer: "app", status: "done",
      tags: ["(c)2", "fixture"],
      desc: "Fixture B: the same spinning icosahedron, same geometry, same schedule, same maths — but the fractal is evaluated in a fragment shader, so 80 bytes per frame cross mapped memory instead of a megabyte. The volume control for fixture A, isolating how cost scales with mapped-write volume.",
      files: ["crates/rayland-icosa-gpu"],
      specs: [{ label: "Icosahedron fixtures", href: "docs/icosa-fixtures.md" }],
      parts: [
        { label: "GPU fractal (80 B/frame)", status: "done", desc: "Uniforms through a persistent mapping, still no interceptable call." }
      ],
      deps: ["icosa-vk", "icosa-core"]
    },
    {
      id: "icosa-window", label: "rayland-icosa-window", layer: "app", status: "done",
      tags: ["demo"],
      desc: "A demo, not a fixture: opens a live Wayland window and shows the icosa solid actually spinning for a human to look at. Exempt from the fixtures' rules — it may depend on rayland-* crates and has a compositor-paced redraw loop, both of which the fixtures forbid.",
      files: ["crates/rayland-icosa-window"],
      specs: [{ label: "Icosahedron fixtures", href: "docs/icosa-fixtures.md" }],
      parts: [
        { label: "Persistent xdg_toplevel", status: "done", desc: "One window, redrawn on every wl_surface::frame callback." }
      ],
      deps: ["icosa-vk", "icosa-core", "present"]
    },
    {
      id: "zink-gl", label: "Real apps · GL via Zink", layer: "app", status: "planned",
      tags: ["(c)4"],
      desc: "The far end of the roadmap: real, complex applications, and OpenGL support by routing it through Zink (GL-on-Vulkan). Depends on everything below it being solid first.",
      files: [],
      specs: [{ label: "Architecture", href: "docs/design/2026-07-13-native-remote-wayland-gpu.md" }],
      parts: [],
      deps: ["c"]
    },

    /* -------------------------------------------------------------- C-side layer */
    {
      id: "c", label: "rayland-c", layer: "cside", status: "done",
      tags: ["(c)1", "daemon", "GPL"],
      desc: "C's daemon: a local vtest server a stock, unmodified Mesa Venus ICD connects to. It hands the app plain local memfds for its ring and blobs, watches the ring where 100% of the app's Vulkan commands actually live, and relays the bytes to S. No Mesa fork and no patch — Rayland is simply the party that allocates the ring.",
      files: ["crates/rayland-c/src/main.rs", "crates/rayland-c/src/ring.rs", "crates/rayland-c/src/relay_engine.rs"],
      specs: [
        { label: "(c)1 · The network", href: "docs/c1-the-network.md" },
        { label: "Venus ring findings", href: "docs/design/2026-07-15-venus-ring-findings.md" },
        { label: "Incremental blob sync", href: "docs/design/2026-07-25-c1-incremental-blob-sync.md" }
      ],
      parts: [
        { label: "vtest host", status: "done", desc: "Speaks the protocol Mesa's Venus ICD expects; hands out local memfds." },
        { label: "Ring watcher", status: "done", desc: "Polls the ring's tail and relays deltas — the loop the whole project exists for." },
        { label: "Reader thread", status: "done", desc: "Owns recv on the link; routes S's replies, blob data, and progress." },
        { label: "Stall detector", status: "done", desc: "Distinguishes 'S is slow' from 'S has stopped' where Mesa's watchdog cannot." },
        { label: "Incremental blob sync", status: "done", desc: "C keeps a baseline of what S holds per application blob and ships only the byte-runs that changed — an unchanged blob crosses nothing. Replaces the v1 whole-blob resend that measured 16.5 MB of resends against 23 KB of actual commands on vkcube. Does not solve remote vkMapMemory ((c)2) or dedup ((c)3)." }
      ],
      deps: ["vtest", "relay", "transport"]
    },
    {
      id: "wp0-proxy", label: "Wayland proxy · WP0", layer: "cside", status: "active",
      tags: ["WP0", "Task 4"],
      desc: "The presentation piece: a real app like vkcube presents through Wayland, but its swapchain wl_buffer is an invalid virtio-gpu dma-buf. WP0 puts a Wayland proxy on C — the app connects to it, not a real compositor — forwards the protocol to S, and replaces the one thing that cannot cross a network (the swapchain fd) with a buffer-by-token naming the S-side resource the command relay already rendered. No pixels cross the network.",
      files: ["crates/rayland-c/src/wayland_proxy.rs", "crates/rayland-c/src/proxy_link.rs", "crates/rayland-s/src/wayland_client.rs"],
      specs: [
        { label: "WP0 spec", href: "docs/design/2026-07-22-wp0-wayland-proxy-first-light.md" },
        { label: "WP0 plan", href: "docs/design/2026-07-22-wp0-wayland-proxy-first-light-plan.md" },
        { label: "Task 4 handoff", href: "docs/design/2026-07-24-wp0-task4-next-session-prompt.md" }
      ],
      parts: [
        { label: "Forward tunnel (4.1–4.2)", status: "done", desc: "App → C proxy → link → S's real compositor, binds and requests replayed." },
        { label: "fd → token intercept", status: "done", desc: "The swapchain dma-buf fd is correlated to a resource id and never crosses." },
        { label: "Event return path (4.4)", status: "done", desc: "Local dmabuf-format synthesis + a relay tunnel (eventfd + send_event, S→app id translation) — vkcube now receives and acks xdg configure." },
        { label: "Token → wl_buffer (4.3)", status: "planned", desc: "Resolve the swapchain token to S's retained HOST3D dma-buf and present zero-copy. Not yet reachable: vkcube never calls create_params, so the Buffer-token arm is never entered." },
        { label: "Ring-relay starvation", status: "done", desc: "FIXED. Never a deadlock and never Mesa: take_venus_blob_writes byte-diffed every Venus-internal blob at gap 0 — ring shadow, 1 MiB reply arena, 8 MiB staging pool — a byte at a time, on a 200 us poll, holding the applier lock. Measured up to 637 ms per call, 71 slow sections; it ran back-to-back, starved the message thread, and the delta that releases the app was never applied, so Mesa's ~3.5 s stall abort fired. Fix: HostBlob::changed_byte_ranges compares 64-byte chunks with slice equality (lowers to memcmp) and descends to the byte loop only inside a differing chunk — detection grain still the byte, ranges byte-identical. Verified: 71 -> 15 slow sections with take_venus_blob_writes gone; deltas applied 90/91 -> 102/102; loopback e2e still bit-identical (refapp + 120-frame icosa). A wrong fix was caught first: filtering by s_written would have broken the return path, since s_written is populated BY the diff." },
        { label: "Token → wl_buffer (4.3) — now reachable", status: "planned", desc: "With the starvation fixed, vkcube reaches swapchain buffer creation for the first time: three create_immed intercepts binding buffers 21/23/25 to resources 8/9/10 at 500x500 XR24, via the fd->token correlation. The WaylandArg::Buffer(_) arm — unreachable all week — is now genuinely reached. 4.3 is no longer blocked on an unreachable path; it is blocked behind the submit decode below." },
        { label: "vkQueueSubmit fails to decode on S (the new wall)", status: "active", desc: "virglrenderer refuses the app's submit and says so: 'vkr: vkQueueSubmit resulted in CS error' -> 'vn_dispatch_command failed' -> 'early bail due to fatal decoder state' -> context destroyed. A CS error is a DECODE failure, not a GPU failure, so this is relay fidelity: bytes S needed were absent or wrong. The fence therefore never signals, the app polls vkGetFenceStatus (type 38) forever, and the vn_wsi[0,0] thread spins into Venus's silent abort (abort frames byte-identical to the previous wall's, reached from a new caller). All 102 deltas were read AND applied, so it is not delivery. HYPOTHESIS (unverified): Venus places the submit's command-buffer contents in the staging pool, which blob_sync deliberately never ships (blob_id==0 = Venus-internal) — which would also explain why refapp/icosa never hit it. Against it: scan_for_out_of_line_stream never fired. NEXT: dump the failing submit's bytes as S sees them and compare with what the app wrote." }
      ],
      deps: ["c", "relay", "s", "present"]
    },

    /* ----------------------------------------------------------------- wire layer */
    {
      id: "vtest", label: "rayland-vtest", layer: "wire", status: "done",
      tags: ["C0", "protocol", "LGPL"],
      desc: "The vtest wire protocol Mesa's Venus ICD speaks, the RenderEngine / VtestTransport traits, and the repository's knowledge of Mesa's command ring. Has no GPU dependencies by construction — only libc and thiserror — and a test asserts the engine never leaks into it, because C must never link a GPU stack.",
      files: ["crates/rayland-vtest/src/vtest.rs", "crates/rayland-vtest/src/venus_ring"],
      specs: [{ label: "Venus ring findings", href: "docs/design/2026-07-15-venus-ring-findings.md" }],
      parts: [
        { label: "vtest framing", status: "done", desc: "Source-verified against real captured bytes." },
        { label: "venus_ring decoder", status: "done", desc: "The ring is the real wire format; a CI fixture pins the captured bytes." },
        { label: "no-GPU linkage guard", status: "done", desc: "Mechanically enforced: cargo tree is libc + thiserror only." }
      ],
      deps: []
    },
    {
      id: "relay", label: "rayland-relay", layer: "wire", status: "done",
      tags: ["(c)1", "wire", "LGPL"],
      desc: "The (c)1 relay wire protocol: the C2S / S2C messages that cross the network — ring deltas, blob syncs, replies, and the WP0 Wayland messages. Pure data, no GPU, no sockets, no async runtime, because both rayland-c and rayland-s depend on it and C must stay GPU-free.",
      files: ["crates/rayland-relay/src/message.rs"],
      specs: [{ label: "(c)1 · The network", href: "docs/c1-the-network.md" }],
      parts: [
        { label: "C2S / S2C message set", status: "done", desc: "Ring deltas, inline submits, blob data both ways, ring progress." },
        { label: "WaylandMessage / BufferToken", status: "done", desc: "The structured Wayland tunnel and the fd-replacement token." },
        { label: "Stage tracer", status: "done", desc: "Env-gated timestamps on a shared clock, used by both daemons." }
      ],
      deps: []
    },
    {
      id: "transport", label: "rayland-transport", layer: "wire", status: "done",
      tags: ["SP2", "QUIC", "LGPL"],
      desc: "The QUIC transport: synchronous stream adapters over a quinn connection. Latency, not bandwidth, is what hurts a command relay, so QUIC's endpoint and congestion control are here — though v1 still shares one stream.",
      files: ["crates/rayland-transport"],
      specs: [{ label: "SP2 · Real transport", href: "docs/sp2-real-transport.md" }],
      parts: [
        { label: "Bi-directional streams", status: "done", desc: "S owes C real answers, so the split gives each thread its own half." },
        { label: "Per-path streams", status: "seam", desc: "Everything still shares one stream today — head-of-line blocking waits for a future slice." }
      ],
      deps: []
    },
    {
      id: "mapped-mem", label: "Mapped-memory coherence", layer: "wire", status: "seam",
      tags: ["(c)2", "open"],
      desc: "The genuinely hard problem: an app writes vertices and textures straight into mapped memory with no API call to intercept, and there is no seam to hook — both Mesa backends make flush/invalidate no-ops. On loopback the shared page is real so it simply works; over a true network it cannot exist. The readback return path is solved; the forward mapped-write path over a real network is still the frontier.",
      files: ["crates/rayland-relay/src/message.rs"],
      specs: [
        { label: "True-remote mapped sync", href: "docs/design/2026-07-19-c2-true-remote-mapped-sync.md" },
        { label: "Icosahedron fixtures", href: "docs/icosa-fixtures.md" }
      ],
      parts: [
        { label: "Readback return path", status: "done", desc: "Solved by the vkGetFenceStatus completion signal — 0 stale over 20 real-network runs." },
        { label: "Forward mapped writes", status: "seam", desc: "Uninterceptable writes reach S on loopback but cannot cross a true network unaided." }
      ],
      deps: ["relay", "c"]
    },
    {
      id: "assets", label: "Content-addressed assets", layer: "wire", status: "planned",
      tags: ["(c)3"],
      desc: "A planned arc: address large assets by content so the same texture or buffer is never shipped twice, cutting the return-path fragmentation the current whole-blob sync produces.",
      files: [],
      specs: [{ label: "Architecture", href: "docs/design/2026-07-13-native-remote-wayland-gpu.md" }],
      parts: [],
      deps: ["relay"]
    },

    /* ----------------------------------------------------------------- S-side layer */
    {
      id: "s", label: "rayland-s", layer: "sside", status: "done",
      tags: ["(c)1", "daemon", "GPL"],
      desc: "S's daemon: the other end of rayland-c. It does not 'receive commands and execute them' — a relayed ring delta is written into the ring blob's own memory, because that is where virglrenderer's ring thread polls for it. It applies the relay to a real libvirglrenderer and, since (c)2, retires each frame through an engine actor so the readback fence and the ring doorbell cooperate on one thread.",
      files: ["crates/rayland-s/src/main.rs", "crates/rayland-s/src/apply.rs", "crates/rayland-s/src/delivery.rs"],
      specs: [
        { label: "(c)1 · The network", href: "docs/c1-the-network.md" },
        { label: "Engine actor", href: "docs/design/2026-07-18-c2-engine-actor.md" },
        { label: "GetFenceStatus completion", href: "docs/design/2026-07-21-c2-getfencestatus-completion.md" }
      ],
      parts: [
        { label: "Relay applier", status: "done", desc: "Writes deltas into S's ring mirror and publishes tail; re-wraps the ring." },
        { label: "Readback completion gate", status: "done", desc: "Ships a frame only once its readback blob has actually advanced." },
        { label: "Wayland replay", status: "active", desc: "Rebuilds the app's object graph on S's real compositor — WP0." },
        { label: "Multi-queue", status: "seam", desc: "ring_idx decode is single-queue today; a second ring stays latent." }
      ],
      deps: ["relay", "engine", "present"]
    },
    {
      id: "present", label: "rayland-present", layer: "sside", status: "done",
      tags: ["SP1 · SP3", "LGPL"],
      desc: "On-screen presentation: takes finished pixels and shows them in a real xdg_toplevel window, via wl_shm or zero-copy zwp_linux_dmabuf. Shared by the SP-era rayland-server and rayland-s, so it lives in its own crate rather than being duplicated.",
      files: ["crates/rayland-present/src/window.rs", "crates/rayland-present/src/dmabuf.rs"],
      specs: [
        { label: "SP1 · Onto the screen", href: "docs/sp1-onto-the-screen.md" },
        { label: "SP3 · Zero-copy present", href: "docs/sp3-zero-copy-presentation.md" }
      ],
      parts: [
        { label: "wl_shm path", status: "done", desc: "The copy path — what (c)1 uses, presenting the app's readback blob." },
        { label: "Zero-copy dmabuf", status: "done", desc: "Export to the compositor with a wl_shm fallback." }
      ],
      deps: []
    },

    /* ---------------------------------------------------------------- engine layer */
    {
      id: "engine", label: "rayland-engine", layer: "engine", status: "done",
      tags: ["arc (c)", "GPU", "FFI", "LGPL"],
      desc: "The real engine: FFI-embeds libvirglrenderer behind rayland-vtest's RenderEngine trait, driving a Venus context on S's GPU. Since (c)1 this crate is only the GPU — the ffi declarations and the VirglEngine that drives them — with the engine actor making one thread own virglrenderer.",
      files: ["crates/rayland-engine/src/virgl.rs", "crates/rayland-engine/src/actor.rs", "crates/rayland-engine/src/ffi.rs"],
      specs: [
        { label: "Engine actor", href: "docs/design/2026-07-18-c2-engine-actor.md" },
        { label: "ring_idx decode", href: "docs/design/2026-07-19-c2-ringidx-decode.md" }
      ],
      parts: [
        { label: "virglrenderer FFI", status: "done", desc: "Venus capset, callbacks ABI verified field-by-field against the header." },
        { label: "Engine actor", status: "done", desc: "One thread owns virglrenderer; clients message it — fence and doorbell cooperate." },
        { label: "dma-buf export", status: "done", desc: "HOST3D resources export as real dma-bufs — the basis of WP0 zero-copy." }
      ],
      deps: ["vtest"]
    },

    /* ------------------------------------------------------------ foundations layer */
    {
      id: "icosa-core", label: "rayland-icosa-core", layer: "foundations", status: "done",
      tags: ["(c)2", "shared", "LGPL"],
      desc: "Shared foundations for the icosahedron fixtures: the geometry, the frame-indexed animation schedule, the Mandelbrot maths, and the bit-exact log2/sin/cos those rest on. No dependencies at all, and never touches a GPU — its correctness is arithmetic. It exists so the two fixtures cannot drift.",
      files: ["crates/rayland-icosa-core"],
      specs: [{ label: "Icosahedron fixtures", href: "docs/icosa-fixtures.md" }],
      parts: [],
      deps: []
    },
    {
      id: "icosa-vk", label: "rayland-icosa-vk", layer: "foundations", status: "done",
      tags: ["(c)2", "shared", "LGPL"],
      desc: "The Vulkan scaffolding both icosahedron fixtures share: bring-up, the depth-tested render pass and pipeline, the targets, the persistent host mapping, and the readback. It exists so the two fixtures cannot drift in the parts that must be identical for their comparison to mean anything.",
      files: ["crates/rayland-icosa-vk"],
      specs: [{ label: "Icosahedron fixtures", href: "docs/icosa-fixtures.md" }],
      parts: [],
      deps: ["icosa-core"]
    },
    {
      id: "wire-sp0", label: "rayland-wire", layer: "foundations", status: "done",
      tags: ["SP0", "legacy", "LGPL"],
      desc: "The SP0-era hand-rolled command messages and their postcard framing. Arc (s)'s own protocol, complete and merged; it coexists with the real-engine arc until that fully supersedes it.",
      files: ["crates/rayland-wire"],
      specs: [{ label: "SP0 · First light", href: "docs/sp0-first-light.md" }],
      parts: [],
      deps: []
    },
    {
      id: "client-sp0", label: "rayland-client", layer: "foundations", status: "done",
      tags: ["SP0", "legacy", "GPL"],
      desc: "SP0-era C side: hand-builds the triangle command stream and sends it. Superseded in intent by the real-engine arc, but its code is untouched and its tests still pass.",
      files: ["crates/rayland-client"],
      specs: [{ label: "SP0 · First light", href: "docs/sp0-first-light.md" }],
      parts: [],
      deps: ["wire-sp0"]
    },
    {
      id: "server-sp0", label: "rayland-server", layer: "foundations", status: "done",
      tags: ["SP0 · SP3", "legacy", "GPL"],
      desc: "SP0-era S side: replays the hand-rolled stream on a real GPU and presents it (PNG, wl_shm window, or zero-copy dmabuf). The origin of the presentation code that became rayland-present.",
      files: ["crates/rayland-server"],
      specs: [
        { label: "SP0 · First light", href: "docs/sp0-first-light.md" },
        { label: "SP3 · Zero-copy present", href: "docs/sp3-zero-copy-presentation.md" }
      ],
      parts: [],
      deps: ["wire-sp0", "present"]
    },
    {
      id: "facade", label: "rayland", layer: "foundations", status: "planned",
      tags: ["facade", "GPL"],
      desc: "The published placeholder that reserves the crates.io name — the future facade that will present Rayland as one coherent crate. It exists to hold the name; the real facade is still ahead.",
      files: ["crates/rayland"],
      specs: [{ label: "Architecture", href: "docs/design/2026-07-13-native-remote-wayland-gpu.md" }],
      parts: [],
      deps: []
    }
  ],

  // The sequenced story: phases are the walking-skeleton milestones, harden items are the
  // cross-cutting toughening that follows. Status matches CLAUDE.md's own account.
  roadmap: [
    { id: "sp0", kind: "phase",  label: "SP0 · First light",             status: "done" },
    { id: "sp1", kind: "phase",  label: "SP1 · Onto the screen",         status: "done" },
    { id: "sp2", kind: "phase",  label: "SP2 · Real transport (QUIC)",   status: "done" },
    { id: "sp3", kind: "phase",  label: "SP3 · Zero-copy presentation",  status: "done" },
    { id: "c0",  kind: "phase",  label: "C0 · Venus first light",        status: "done" },
    { id: "c1",  kind: "phase",  label: "(c)1 · The network",            status: "done" },
    { id: "c2",  kind: "phase",  label: "(c)2 · Mapped memory & readback", status: "done" },
    { id: "wp0", kind: "phase",  label: "WP0 · Wayland proxy first light", status: "active" },
    { id: "c3",  kind: "phase",  label: "(c)3 · Content-addressed assets", status: "planned" },
    { id: "c4",  kind: "phase",  label: "(c)4 · Real apps · GL via Zink", status: "planned" },
    { id: "sp4", kind: "harden", label: "SP4 · Adaptive L3 · session & security", status: "planned" },
    { id: "sp5", kind: "harden", label: "SP5 · Proxy completeness (Sommelier/waypipe-grade)", status: "planned" },
    { id: "audio", kind: "harden", label: "Audio track", status: "planned" }
  ]
};
