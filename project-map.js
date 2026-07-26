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
    updated: "2026-07-26"
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
      files: ["crates/rayland-c/src/main.rs", "crates/rayland-c/src/ring.rs", "crates/rayland-c/src/relay_engine.rs", "crates/rayland-c/src/ring_dump.rs"],
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
        { label: "S cannot resolve the submit's VkQueue (located)", status: "active", desc: "LOCATED by the stream decoder's first run. 'CS error' never meant truncation: the generated dispatcher reports it whenever the decoder's fatal flag is set after dispatch, and vn_dispatch_vkQueueSubmit sets that same flag on `if (!args.queue)` — a HANDLE LOOKUP FAILURE, not a read past the end. So S cannot resolve the VkQueue the app submits to. Everything fits: bytes byte-exact, framing agreed (virglrenderer's head lands on offset 748, exactly our decoder's boundary for type 18), our decoder succeeds because it stubs lookups and never dispatches. REFUTED in the same run: a tail falling mid-command (105/105 deltas walk to ReachedEnd) and an out-of-line stream (zero type-180 anywhere). Queue lifecycle now readable: exactly ONE vkGetDeviceQueue2 in the whole stream, with a vkDestroyDevice between it and the failing submit. NEXT: dump the queue handle id the submit names against the ids S has registered — was it never created, destroyed, or never delivered?" },
        { label: "Superseded: 'the submit does not complete on S'", status: "done", desc: "RING RELAY PROVEN BYTE-EXACT: both sides independently FNV-1a every delta and join on tail — 253 deltas, 253 identical digests, zero mismatches. So corruption, truncation, reordering and loss are all eliminated, and the fault is in what the submit REFERENCES, not the submit itself. Two failure modes observed, and it is INTERMITTENT: (a) 'vkr: vkQueueSubmit resulted in CS error' -> fatal decoder state -> context destroyed -> vn_wsi[0,0] spins into Venus's silent abort; (b) no error at all, the submit simply never completes and the app polls vkGetFenceStatus 142 times until timeout (exit 124). Intermittency argues against a structurally-missing staging pool and for a race — something the submit needs is sometimes present. Candidates: the token-built swapchain images (res 8/9/10, from the WP0 buffer path, never reconciled with the (c)1 blob path), or part of the recorded stream living in the staging pool C declines to publish. BLOBS NOW FINGERPRINTED TOO: all four application blobs are byte-identical on both sides (so the incremental blob sync is measured correct under a real app, not just e2e-green on fixtures); ring and arena differ by 1 and 16 bytes, which is sampling skew on channels both sides write. THE ONE STRUCTURAL DIVERGENCE is the 8 MiB staging pool res=3 — C holds content (28 non-zero bytes), S's copy is entirely zeros, because blob_sync declines to publish blob_id==0 by design. Not proven to be the cause (28 bytes is a small footprint), but after eliminating the ring and the app blobs it is the only candidate left standing, and it is empty on the failing side. STAGING POOL NOW EXONERATED: shipping it (RAYLAND_C1_SHIP_BLOB, a named diagnostic, not a fix) makes C and S agree on res=3 byte-for-byte and the app gets NO further — 36 proxy lines with it on, 52 with it off, in two runs each. So the submit does not reference raw blob content C holds and S lacks. Ring byte-exact, app blobs identical, staging pool closed and irrelevant. TOKEN-BUILT RESOURCES ALSO AGREE: res 8/9/10 fingerprint identically on both sides (both empty — the submit that would fill them is what fails). So the elimination is COMPLETE: ring byte-exact, every blob's content agrees, staging pool closed experimentally without effect. THE FAILING SUBMIT IS NOT SHORT OF ANY BYTES. What S lacks is the OBJECT STATE behind those resources — the VkImage, its memory binding, its layout — built through the WP0 token path, which nothing has yet been made responsible for establishing. WHAT 'CS ERROR' MEANS (from the generated dispatcher): the decoder's fatal flag is set while dispatching vkQueueSubmit, and that flag has one trigger — reading past the end of the stream. So S's decoder believes the submit is TRUNCATED, while every byte provably matches C's and all 102 deltas were applied. Two readings, neither yet tested: (1) a relayed tail can fall mid-command, or (2) part of the submit lives outside the ring (vkExecuteCommandStreamsMESA, type 180). STAGING POOL NOW PROPERLY EXONERATED (the earlier attempt was invalid — the app stalled before the submit, so it tested nothing; shipping the pool incrementally fixed that, the pool is byte-identical on both sides, and the CS error still fires). AT FAILURE head is 180 bytes behind tail, so the decoder had 180 bytes for the command at that offset and overran: THE COMMAND NEEDS MORE BYTES THAN THE PUBLISHED TAIL PROVIDES. Every byte of every resource now demonstrably agrees, so what is wrong is EXTENT, not content. BOTH REMAINING READINGS ARE BLOCKED ON THE SAME GAP: venus_ring::decode's encoded_size can only express FIXED encodings — its own docs list vkCreateInstance/vkCreateRingMESA/vkExecuteCommandStreamsMESA as nameable but unsizeable — so no table can walk a stream of variable-length commands; that needs per-command decoders (the thousands of generated lines Mesa ships), which is a subsystem decision, not an afternoon. Also measured: the relay path is so latency-sensitive that a periodic ~11 MiB hash stopped the app reaching its buffers at all (36 proxy lines vs 52, 5 runs to 3) — three instruments distorted this system today. **CORRECTIONS, 2026-07-26 — two claims in this entry are now MEASURED FALSE.** (i) \"that flag has one trigger — reading past the end of the stream\": wrong, vn_cs_decoder_set_fatal is overloaded (handle validation sets the same flag from a different file with no distinguishing message). (ii) \"the command needs more bytes than the published tail provides\": wrong, at failure virglrenderer's extent is [26748, 27536) = 788 bytes and the submit at 27416 is exactly 120 bytes, ending exactly at 27536 — the extent covers it precisely, and virglrenderer's head lands on a boundary our decoder independently agrees with. Both readings that motivated building the decoder are retired by the decoder. See DIARY.md 2026-07-26." },
        { label: "vkQueueSubmit CS error — ROOT-CAUSED: VK_ERROR_DEVICE_LOST", status: "done", desc: "SOLVED 2026-07-26. The refused submit returns VkResult=-4 (VK_ERROR_DEVICE_LOST) from the real vkQueueSubmit on S's GPU; the four accepted submits in the same run return 0. vn_dispatch_vkQueueSubmit ends with `else if (args.ret == VK_ERROR_DEVICE_LOST) vn_cs_decoder_set_fatal(...)`, and every submit in this workload has flags=0x0 (measured), so that IS the live path — device loss poisons the command stream SILENTLY and surfaces only as the generic '%s resulted in CS error'. THIS WAS NEVER A PROTOCOL BUG: the relay was correct throughout, which is exactly why twelve protocol theories were all refuted. Proved by building a patched virgl_render_server (source from the Ubuntu archive pool over plain HTTP — no root, no deb-src; meson/ninja/pyyaml in a scratchpad venv; the distro's own fix-c23.patch for the glibc C11-threads clash) and spawning it via RENDER_SERVER_EXEC_PATH, which the SYSTEM library honours, leaving /usr/libexec untouched. THE REAL QUESTION IS NOW DIFFERENT AND IT IS A RESOURCE-SETUP BUG, NOT A RELAY ONE: why does the device die? ANSWERED SAME DAY, and the leading suspect was WRONG. The handleType hypothesis (VUID-VkBindImageMemoryInfo-memory-02728 / -02726: dma-buf external memory bound to images and buffers created with handleTypes=0) was nominated and then REFUTED by correlation: all three VKR_DEBUG=validate runs carry the same 3 bind violations and issue the same 5 submits, yet only one loses the device — a condition present when the submit succeeds cannot be what makes it fail. Those violations are real spec breaches worth fixing on their own account, but they are not this bug. THE ACTUAL ANSWER IS THE GPU. vkcube defaults to GPU 1, the discrete NVIDIA RTX A500 — not the Intel node rayland-s opens. Forcing --gpu_number 0 (Intel Iris Xe): NVIDIA 14 runs / 7 device losses (50%) reaching 5-6 submits; Intel 10 runs / 0 device losses reaching 8 submits EVERY run. p~0.001 at the measured NVIDIA rate, plus a rate-independent signal (Intel gets strictly further). SCOPE, STATED CAREFULLY: both configurations still exit 124 — this is NOT 'vkcube works on Intel', that timeout is the known (c)1 synchronous-reply latency wall and a separate problem. The claim is that the device loss is a property of the NVIDIA driver executing what Venus hands it, NOT of Rayland's relay — which closes the loop on the byte-level refutations by showing the same relayed stream executes without fault on a different driver, same machine, same run configuration. HISTORICAL DETAIL OF THE CHARACTERISATION FOLLOWS. THE FAILING SUBMIT IS NOW FULLY CHARACTERISED. vkcube issues five vkQueueSubmits; four are accepted and the fifth is refused. Decoded field-by-field from dumped bytes: accepted #2 = {fence 0x1d, wait sem 0x1e, cmdbuf 0x18, signal sem 0x2e}; refused #5 = {fence 0x1f, wait sem 0x20, cmdbuf 0x19, signal sem 0x2f} — the SECOND SWAPCHAIN IMAGE's resource set, structurally identical to the accepted one, encoding to exactly 120 bytes with nothing left over. REFUTED THIS SESSION, each by measurement: queue id wrong (all five name 0x6); queue id zero (the silent lookup_object early-out VK_NULL_HANDLE fences require); ring wrap (128 KiB buffer, failure at 27416); frame desync (head on an agreed boundary); truncation/bounds overrun (extent covers the command exactly); bad sType or unknown pNext (our decoder runs the SAME generated code on the same bytes and sets no fatal); the WSI semaphore import (vkImportSemaphoreResourceMESA names resourceId=0 in BOTH the accepted and refused frames — it was the day's best lead and it is not the differentiator); and 'the handles were never created on S' (every vkCreateFence, vkCreateSemaphore and vkAllocateCommandBuffers id that crossed the ring was enumerated and cross-referenced — all 14 objects exist, command buffer 0x19 included). NOT CLOSED, and deliberately not listed as refuted: 'handle lookup failure (!args.queue)'. It rests on the absence of an 'invalid object id' log, which rests in turn on an UNVERIFIED inference from string adjacency in the render server binary; disassembly around that string shows a calloc under a mutex, which reads like an object-table INSERT (duplicate id on create), not a lookup. So a failed lookup may be silent. WHERE THE WALL IS: the generated dispatcher calls through to virglrenderer's own vkr_dispatch_vkQueueSubmit, which is not in the vendored headers and can set fatal for reasons we cannot enumerate. Every condition we CAN enumerate is refuted. BLOCKED ON: virglrenderer 1.2.0 src/venus/vkr_queue.c — gitlab.freedesktop.org is behind an anti-bot wall, there is no GitHub mirror of the canonical repo, and apt-get source fails with no deb-src configured (enabling one needs root). ALSO ESTABLISHED: the two failure modes this map recorded separately (exit 134 CS error vs exit 124 timeout) are ONE failure at two speeds — the submit is only reached when the run is fast enough, which is why every added diagnostic suppresses it." }
      ],
      deps: ["c", "relay", "s", "present"]
    },

    /* ----------------------------------------------------------------- wire layer */
    {
      id: "venus-proto", label: "rayland-venus-proto", layer: "wire", status: "done",
      tags: ["WP0", "decoder"],
      desc: "DONE (Tasks 1-5 landed). Rayland can relay Venus streams byte-exactly but, before this crate, could not read them: encoded_size can only express FIXED encodings, so the decoder used to halt at the application's first real command and 'where does this command end' could not be asked — which is what blocked the open vkQueueSubmit framing question. This crate borrows Mesa's own generated venus-protocol (73k lines, 43 headers) behind a C shim, rather than reimplementing it, for the same reason CLAUDE.md reuses the rendering engine: a second source of truth for a format Rayland does not own will diverge, and the symptom is a decoder confidently reporting the wrong thing. Feasible because byte consumption is independent of handle lookups, so stub lookups give identical framing with no object table, no validation and no GPU. Entire public surface is one question: how long is the command at the start of this slice. BINDING CONSTRAINT, NOW DELIBERATELY BREACHED IN ONE PLACE (2026-07-26): the constraint was diagnostic and structural only — it may never make a correctness decision, because (c)1 spec §7 relays the ring as opaque bytes precisely so a decode bug cannot become a corruption bug. That still holds for rayland-c (mechanically, via decoder_is_not_load_bearing). It no longer holds for rayland-s: find_destroy_device, which apply.rs:755 uses to decide when to retire the readback gate, was measured firing on payload bytes (offsets 6236 then 6300 in consecutive runs, in a delta with no vkDestroyDevice in it) and now validates its hit against a decoded command boundary. The rule was written so a decode bug could not become a corruption bug; here it was blocking the replacement of a heuristic PROVEN to corrupt. Recorded as a trade, not a loophole — the rayland-c guard is deliberately left un-widened so the discrepancy stays visible rather than being papered over by a green test. See DIARY.md 2026-07-26. Task 4 wired the safe Rust wrapper (command_len(), Command, DecodeFault) into rayland-vtest::venus_ring::decode as a fallback behind the fixed-size table, and Task 5 closed the loop: a test (rayland-c's decoder_is_not_load_bearing) mechanically asserts the relay path never names this crate, rayland-c's no_gpu_linkage guard was re-run and re-read (still passes — it guards rayland-engine's absence, not this crate's presence, and that assertion is unaffected), and the crate docs / CLAUDE.md were corrected where this crate's arrival made them false. Diagnostic-only for what gets RELAYED (that is what rayland-c's guard pins, and it holds); no longer diagnostic-only for what rayland-s does with what it received — see the breach noted above.",
      files: ["crates/rayland-venus-proto/src/lib.rs", "crates/rayland-venus-proto/csrc/vkr_cs.h", "crates/rayland-venus-proto/csrc/decode_switch.inc", "crates/rayland-venus-proto/csrc/shim.c", "crates/rayland-venus-proto/vendor", "crates/rayland-venus-proto/tools/gen_switch.py"],
      specs: [
        { label: "Decoder design", href: "docs/superpowers/specs/2026-07-26-venus-stream-decoder-design.md" },
        { label: "Implementation plan", href: "docs/superpowers/plans/2026-07-26-venus-stream-decoder.md" }
      ],
      parts: [
        { label: "Task 1: crate + vendored headers + compiling shim", status: "done", desc: "52 vendored venus-protocol headers (virglrenderer 1.2.0, 974 KiB) plus a from-scratch csrc/vkr_cs.h satisfying the contract vn_protocol_renderer_cs.h declares. FINDING: that header's own 'these types/functions are expected' comment is stale against its code — six symbols (blob-storage get/put, alloc_temp_array, the three handle-id helpers, and a struct vkr_object) are referenced in the file body but absent from the comment. alloc_temp_array and the handle-id helpers are implemented for real (mechanical extensions of already-blessed primitives, verified against two call sites in the vendored tree); the blob-storage pair is a documented NULL stub — safe because Task 1 calls no decoder at all, but Task 2 must give it a real body before decoding any command with a blob array (vkCmdPushConstants2, pipeline specialization data). Shim compiles, links, and rayland_venus_proto_selftest() reports 38 (VK_COMMAND_TYPE_vkGetFenceStatus_EXT). rayland-c's no_gpu_linkage guard re-run and still green." },
        { label: "Task 2: generated decode switch + command_len shim entry point", status: "done", desc: "tools/gen_switch.py scans the vendored headers for vn_decode_<name>_args_temp symbols and VK_COMMAND_TYPE_* enumerators and emits csrc/decode_switch.inc (312 cases, case 38 = vkGetFenceStatus present, no duplicates). Closed Task 1's carried finding: vkr_cs_decoder_get_blob_storage's NULL path used to be silently swallowed by Mesa's generated call sites (an early return that skips the cursor-advancing decode without setting fatal) — it now sets fatal itself before returning NULL, and matches the reference's dec->cur-aliasing return on success, which needed a dec->cur != val guard added to vkr_cs_decoder_read/peek. SURPRISE FINDING: building the switch revealed six commands (vkGetQueryPoolResults, vkGetPipelineCacheData, vkCopyImageToMemoryMESA, vkWriteAccelerationStructuresPropertiesKHR, vkGetRayTracingShaderGroupHandlesKHR, vkGetRayTracingCaptureReplayShaderGroupHandlesKHR) have decoders that also take a struct vn_cs_encoder* — contradicting the brief's 'encoder side is never driven' premise — because Mesa pre-sizes each command's reply arena during decode; vkr_cs_encoder_get_blob_storage now returns a safe non-null sentinel instead of the abort() first draft, since all six call sites only store the pointer, never dereference it. Verified with hand-built byte streams (not committed): a real vkCreatePipelineCache blob decodes to its exact hand-computed length and faults rather than under-reporting when truncated mid-blob. rayland_venus_command_len exists in csrc/shim.c; the Rust wrapper is later work." },
        { label: "Task 3: safe Rust wrapper (command_len, Command, DecodeFault)", status: "done", desc: "unsafe confined to one extern \"C\" call in command_len(); Command{command_type,len} and DecodeFault{Truncated,UnknownCommand{command_type},BadArgs} are the entire safe surface. Six tests, all green: the brief's four (24-byte vkGetFenceStatus; mid-command truncation; too-short-for-prologue; unknown command type reporting its own type back), plus two carried over from Task 2's review as committed load-bearing evidence (that review's own verification used a throwaway harness that no longer exists) — a vkCreatePipelineCache stream with a 5-byte pInitialData blob (padded to 8 on the wire, proving vkr_cs_decoder_get_blob_storage's NULL-return fix), and a vkGetPipelineCacheData stream that reaches the 3-argument decoder path and its vn_cs_encoder_get_blob_storage sentinel. Both carried tests' byte counts (88 and 48) were hand-derived field-by-field from vn_protocol_renderer_pipeline_cache.h before running, not pinned from output, and both passed first try. TEETH-CHECK FINDING, worth recording straight: the brief's literal instruction (flip vkr_cs_decoder_read's bounds check from < to >) does NOT fail the truncation test in isolation — size_t being unsigned means the flip fires fatal MORE often (breaking two other tests) rather than disabling the check, and for this specific truncated input it coincidentally still reports Truncated, for the wrong reason (an early false-positive on the prologue read, not the real truncation site). A test that cannot fail is not evidence, so a second mutation (the bounds check body replaced with `if (0)`, genuinely disabling it) was used instead: with it, the truncated stream decodes to a plausible-but-wrong Ok(Command{command_type:38,len:24}) instead of erroring — exactly the 'confidently wrong length' hazard this crate exists to prevent. Reverted; git diff on vkr_cs.h is empty. Still nothing in the workspace calls this crate — DIAGNOSTIC ONLY holds. REVIEW FIX ROUND: substance approved (reviewer independently re-derived both byte counts and re-traced the teeth-check finding, both held); one real finding — csrc/shim.c's rayland_venus_proto_selftest still claimed to be linked from src/lib.rs::selftest_command_type, which this task had deleted, making the function orphaned and the comment doubly stale. Deleted the function (its one job is now covered more thoroughly by command_len's six tests) rather than correcting the comment in place. Also cited RAYLAND_VENUS_FAULT_* names next to command_len's fault-code branches, and corrected the SAFETY comment's 'keeps no state between calls' — the scratch pool's bytes are _Thread_local static and persist; only its bump-allocator cursor resets per call." },
        { label: "Task 4: wired into venus_ring::decode as the fallback past the fixed-size table", status: "done", desc: "rayland-vtest's decode_commands now tries encoded_size's table first (pure Rust, the independent cross-check) and, for anything the table cannot express, calls command_len — this crate's first real caller. Agreement test confirms both give the identical 24 bytes for vkNotifyRingMESA, the one command both can size. FIXTURE FINDING: the brief's anchor test assumed a captured multi-command stream long enough to reach DecodeStop::ReachedEnd; no such fixture exists in this repo (the 2026-07-15 ring capture preserves only 100 of 216 produced bytes — enough for the three table-known commands and one byte into vkCreateInstance, no further), and one was not fabricated to fit, since a captured fixture's whole value is that its bytes are an observation, never synthesized. Real, honest effect on the existing fixture: the stop at vkCreateInstance changed from UnknownCommandSize (this crate's own ignorance) to Truncated{offset:88} (the borrowed decoder CAN size it, the 100-byte window just doesn't hold it) — more precise, not weaker. Positive ReachedEnd evidence instead came from a different real capture already on hand: the 2026-07-19 vkGetDeviceQueue2 bytes (a command deliberately excluded from the table), which the borrowed decoder sizes at exactly 80 bytes with no remainder. Open gap, recorded rather than worked around: no fixture here yet proves the walker crossing several variable-length commands in one real stream." },
        { label: "Task 5: guards and documentation", status: "done", desc: "Closed the loop the whole design rests on. New test rayland-c/tests/decoder_is_not_load_bearing.rs greps the relay path's own source files for the decoder crate's name and fails loudly, with a message naming (c)1 spec §7, if any of them ever names it. REVIEW FINDING (Critical): the first version had a live hole — main.rs (not ring.rs) is where relay decisions actually happen (messages_for_delta, scan_for_out_of_line_stream, link.send all live there), main.rs was missing from the checked file list, and main.rs already held a reachable call to decode_commands (the RAYLAND_RING_DUMP diagnostic) that named no crate the guard grepped for. Fixed by extracting that diagnostic into its own module, src/ring_dump.rs — main.rs now contains no decoder call of any kind — adding main.rs to the checked file list, and checking two needles (the crate name and decode_commands) instead of one. Re-teeth-checked: planting a real, compiling decode_commands call back in main.rs's relay path failed the guard, naming main.rs and the right needle; reverted, green again. rayland-c's no_gpu_linkage guard was re-run and re-read rather than assumed: it still asserts only that rayland-engine is absent from rayland-c's dependency tree, unaffected by this crate's arrival — its doc comment never claimed 'only libc and thiserror' (that phrasing lived in rayland-vtest's own docs and CLAUDE.md), so no self-description correction was needed there. rayland-vtest's crate docs and CLAUDE.md, which did carry that now-false phrase, were corrected to name the new dependency and explain why the crate is still GPU-free despite it." },
        { label: "Final whole-change review: fix wave (5 Important + minors)", status: "done", desc: "A review of the whole five-task change-set (no Critical issues found) flagged five Important findings and several minors, all fixed in one pass. (1) shim.c's temp-pool comment contradicted its own next line (called the _Thread_local static pool 'a local'); rewritten to say what src/lib.rs already knew: bytes persist per-thread, temp_used resets every call, thread-local because two threads may decode concurrently. (2) decoder_is_not_load_bearing.rs's allowlist covered only 5 of 10 src/ files (missing shm.rs, metrics.rs, proxy_link.rs, wayland_proxy.rs, and any future file) and its own disclosure named a live hole: ring_dump.rs's dump_if_enabled was excluded from the check on a prose-only claim that it returns nothing. Fixed by inverting the list (enumerate src/*.rs, subtract a commented EXCLUDED set) and adding a second test, ring_dump_exposes_only_the_returnless_diagnostic_entry_point, that mechanically pins dump_if_enabled's exact returnless signature — teeth-checked by giving it `-> bool` (with a trailing `true`, so it still compiles): the new test failed with the intended message, the substring guard correctly stayed green. (3) The temp pool's 1 MiB exhaustion (vkr_cs_decoder_alloc_temp/_alloc_temp_array's fatal) was indistinguishable from a genuinely truncated stream — the one diagnosis this crate exists to get right, since virglrenderer's own cap is 1000x larger. Added struct vkr_cs_decoder::pool_exhausted, fault code 4 (RAYLAND_VENUS_FAULT_TEMP_POOL_EXHAUSTED), DecodeFault::TempPoolExhausted and a matching DecodeStop::TempPoolExhausted in venus_ring::decode — both with tests using a cheaply-constructed vkResetFences stream (fenceCount=200,000 overruns the pool before any handle bytes are read, so the whole test stream is 28 bytes). (4) The SAFETY comment on command_len (and vkr_cs_decoder_get_blob_storage's own doc) rested on an unrecorded audit — no generated _temp decoder writes through the blob-storage pointer at 1.2.0 — now written down at both sites and folded into the vendor-update checklist. (5) A missing diary entry for Task 4's own review-round fix (325e9f0, which retracted stale absolutist 'can never reach past command two' claims) is now recorded. Minors: doc comments added to five previously-undocumented vkr_cs.h functions; build.rs moved c99→c11 (shim.c already uses C11 _Thread_local/_Alignas); vkr_cs_handle_store_id's pointer-width vs upstream's vkr_object_id-width direct-branch write, and vkr_cs_decoder_read's silent clamp vs upstream's assert, both now documented as deliberate, harmless-on-64-bit deviations; rayland-vtest/Cargo.toml gained an explicit publish = false (it depends on the already-unpublishable rayland-venus-proto); vendor/MESA_VERSION gained a five-step re-vendor checklist and vendor/LICENSE.mesa now attributes the vendored tree's actual two-license split (MIT for Mesa's own generated headers, Apache-2.0 for the Khronos Vulkan headers pulled in transitively — the review's 'MIT' framing was itself imprecise); the design spec's 'drift risk: none' gained the qualifier that it holds only while C, S and the vendored copy agree on protocol version." }
      ],
      deps: ["vtest"]
    },
    {
      id: "vtest", label: "rayland-vtest", layer: "wire", status: "done",
      tags: ["C0", "protocol", "LGPL"],
      desc: "The vtest wire protocol Mesa's Venus ICD speaks, the RenderEngine / VtestTransport traits, and the repository's knowledge of Mesa's command ring. Has no GPU dependencies by construction — it links libc, thiserror, and (since rayland-venus-proto Task 4) rayland-venus-proto, which compiles Mesa's generated protocol headers with no driver, no device and no libvirglrenderer behind them — and rayland-c's no_gpu_linkage test asserts the real engine never leaks into its dependency tree, because C must never link a GPU stack.",
      files: ["crates/rayland-vtest/src/vtest.rs", "crates/rayland-vtest/src/venus_ring"],
      specs: [{ label: "Venus ring findings", href: "docs/design/2026-07-15-venus-ring-findings.md" }],
      parts: [
        { label: "vtest framing", status: "done", desc: "Source-verified against real captured bytes." },
        { label: "venus_ring decoder", status: "done", desc: "The ring is the real wire format; CI fixtures pin captured bytes. Since rayland-venus-proto Task 4, decode_commands falls back to that crate's borrowed Mesa decoder for anything its own fixed-size table cannot express, so the walk reaches far past the three command types the table knows. Since 2026-07-26 it is no longer purely diagnostic: find_destroy_device decodes before believing a signature match, because the bare scan was measured false-positiving on payload bytes and retiring rayland-s's readback gate on a device destruction that never happened. It still never informs a RELAY decision — rayland-c's guard pins that — only rayland-s's gate." },
        { label: "no-GPU linkage guard", status: "done", desc: "Mechanically enforced: cargo tree for rayland-c (which links rayland-vtest) must not contain rayland-engine. rayland-vtest's own tree now includes rayland-venus-proto (a build-time C compile of protocol headers, no GPU), which the guard correctly does not object to — it only ever named rayland-engine as its needle." }
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
      files: ["crates/rayland-s/src/main.rs", "crates/rayland-s/src/apply.rs"],
      specs: [
        { label: "(c)1 · The network", href: "docs/c1-the-network.md" },
        { label: "Engine actor", href: "docs/design/2026-07-18-c2-engine-actor.md" },
        { label: "GetFenceStatus completion", href: "docs/design/2026-07-21-c2-getfencestatus-completion.md" }
      ],
      parts: [
        { label: "Relay applier", status: "done", desc: "Writes deltas into S's ring mirror and publishes tail; re-wraps the ring." },
        { label: "Readback completion gate", status: "done", desc: "Ships a frame only once its readback blob has actually advanced. Lives inside progress_thread in main.rs — the src/delivery.rs the earlier design named was DELETED by the 2026-07-21 G' fix (commit 0a21513); docs pointing at that path were corrected 2026-07-26. Re-verified over the real network 2026-07-26: scripts/c2-icosa-two-machine.sh, 10/10 runs, 0 stale frames, no wedge." },
        { label: "Queue-latch apparatus (DEAD on the shipping path)", status: "seam", desc: "OPEN SEAM found 2026-07-26. apply.rs latches the app's queue from vkGetDeviceQueue2, then scans every delta for find_destroy_device / find_queue_submit to feed three methods: retirement_ring_idx, queue_ring_drained, latest_submit_pos. NONE of the three has a production caller — main.rs calls exactly six session methods (lock, apply, reply_arena_fence_signaled, take_app_blob_writes, take_ring_progress, take_venus_blob_writes) and none of them is these. The 2026-07-21 G' fix replaced the S-issued fence with the reply-arena scan and left this machinery running on every delta, scanning every byte, feeding nothing. Consequence for 2026-07-26's find_destroy_device false-positive fix: that defect was REAL but INERT — it retired a gate nothing reads, so it never explained any icosa staleness, and the (c)1 §7 exception taken for it is load-bearing on a path that is not itself load-bearing. Not deleted, because this is where multi-queue support naturally lands and it now behaves correctly; recorded rather than silently removed. See DIARY.md 2026-07-26." },
        { label: "Wayland replay", status: "active", desc: "Rebuilds the app's object graph on S's real compositor — WP0." },
        { label: "Live remote presentation (THE DEMO)", status: "done", desc: "DELIVERED 2026-07-26: rayland-icosa-cpu runs on apollo, its commands cross a real network, dop561's GPU draws them, and the icosahedron animates live in a window on dop561's screen — 120/120 frames, via scripts/icosa-remote-demo.sh. THE BLACK WINDOW seen earlier was never a presentation defect: FrameCapture snapshots a blob at BlobCreated, which holds the finished frame for a one-frame app (Mesa creates the readback blob lazily at vkMapMemory, AFTER refapp's single vkCmdCopyImageToBuffer) but holds NOTHING for a 120-frame app whose blob is born before any of them. Two fixes failed first and both are recorded in the code: re-reading the blob per-apply stalled the ring 30 s (a 256 KiB read of GPU-shared memory under the session lock — the sixth instrument this week to become a participant in what it measured), and re-reading after the session reads all zeroes because the app has freed the buffer. LiveFrame instead accumulates the frame from the readback runs progress_thread already extracts and ships, gated on the fence proving the submit and its copy complete: no second read, bytes known whole. Presentation also moved onto its own thread BESIDE the relay — it used to run only after serve() returned, which is right for one still frame and useless for a live one. HONEST PERFORMANCE NOTE: ~2 fps at 256x256, and the slowness is accounted for rather than mysterious — this fixture ships 1 MiB up per frame (CPU-computed fractal through mapped memory, the worst case it was BUILT to be; its sibling icosa-gpu draws the same picture in 80 bytes/frame) and 256 KiB down fragmenting into ~5000 one-byte messages. That is ~2.6 MB/s: round-trip and message-rate bound, NOT bandwidth bound. Resolution is separately cheap in principle — in a command-streaming design it is the GPU's problem and the GPU is next to the display — but costs bandwidth TODAY only because S presents the app's readback buffer, which is what WP0's token -> wl_buffer path exists to end." },
        { label: "Multi-queue", status: "seam", desc: "ring_idx decode is single-queue today; a second ring stays latent." }
      ],
      deps: ["relay", "engine", "present"]
    },
    {
      id: "present", label: "rayland-present", layer: "sside", status: "done",
      tags: ["SP1 · SP3", "LGPL"],
      desc: "On-screen presentation: takes finished pixels and shows them in a real xdg_toplevel window, via wl_shm or zero-copy zwp_linux_dmabuf. Shared by the SP-era rayland-server and rayland-s, so it lives in its own crate rather than being duplicated. SINCE 2026-07-26 IT CAN ALSO FOLLOW A LIVE RENDER: present_live() takes an optional boxed 'static closure supplying subsequent frames, and the window re-arms a wl_surface.frame callback on every commit and redraws from it. With None it is present() exactly — no callbacks requested, no behaviour change for rayland-server — which is what made the change safe to land in a crate two binaries depend on. A boxed closure rather than a borrow of the FrameSource, deliberately: holding the source would put a lifetime parameter on RaylandWindow and therefore on all six delegate_* macros and nine impls, for no gain. Pacing is the COMPOSITOR's, not the relay's — the window shows whichever frame S last completed, so a slower remote render repeats frames. That keeps presentation from ever blocking the relay, at the cost of not being frame-accurate.",
      files: ["crates/rayland-present/src/window.rs", "crates/rayland-present/src/frame.rs"],
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
