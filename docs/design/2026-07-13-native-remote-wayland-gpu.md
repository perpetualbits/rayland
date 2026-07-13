# What Wayland Would Need to Grow Native Remote GPU Rendering

**Date:** 2026-07-13
**Status:** Design / position paper (P3 — "what the ecosystem needs," not a build spec)

---

## 1. The problem, in one topology

We deliberately adopt X11-era vocabulary, which is the *inverse* of common cloud usage:

- **S ("server" side)** — where the user physically sits: keyboard, mouse, **display, GPU**,
  Wayland compositor, working Vulkan/GL drivers. The beefy laptop/workstation.
- **C ("client" side)** — where the application executable runs. Possibly foreign-arch
  (RISC-V rv64), possibly weak (a Milk-V Mars SBC), possibly a headless 96-core/768 GB
  hypervisor. May have accelerators (NPU/AI-GPU) but **no good graphics/display path**.
- **The wire (C→S)** should carry a **command/API stream** — the *language* of rendering —
  and S's GPU does the actual drawing. A pixel/video stream is the fallback, not the goal.

The target is, in one line: **resurrected _indirect GLX_ — but for Vulkan and modern GL,
carried over a Wayland transport.**

## 2. Why remoteness was *excluded*, not merely omitted

X11 was a wire protocol from birth: a client sends drawing ops to an X **server** that owns
display and input, so "remote X" is just a TCP socket. Modern toolkits stopped using X
drawing primitives and render client-side, which is why X forwarding ships bitmaps and feels
slow while local X flies (DRI direct rendering + MIT-SHM, neither of which crosses a network).

Wayland made the opposite bet: it is a **buffer-handle-passing** protocol. The client renders
into a GPU buffer and hands the compositor a **dmabuf file descriptor** over a Unix socket.
You cannot pass an FD over a network, and that is deliberate — Wayland assumes client and
compositor share memory and a GPU. Remoteness is therefore an *excluded assumption*. Any
bridge must ship buffer **contents** (or the **commands** that produce them) where Wayland
only ever ships **handles**.

## 3. The two regimes (scope)

The problem cleanly partitions into two regimes with **disjoint hardware assumptions**, so a
real system need not be brilliant at both:

- **Command-stream regime (the target).** C weak / foreign-arch / driver-challenged, S
  strong. Workloads: GPU desktop apps (Blender, CAD, GIS, WebGL), the user's own GL/Vulkan
  tools, 2D-GL/compositor-grade surfaces. Command-streaming wins because it moves GPU work to
  S (where the GPU and drivers are) and asks nothing of C's encode hardware.
- **Video regime (the escape hatch).** C strong with working encoders, S strong, fat network,
  frame-tight AAA workloads. Here you encode on C and decode on S — and that is fine precisely
  because C can afford it. In the target topology, video is *doubly wrong*: the pixels
  originate at C, the weakest box, so the encode burden lands on exactly the machine least
  able to carry it.

A native system command-streams by default and, when it detects the video regime's
preconditions, degrades to it per-surface (see §7).

**Non-goals:** low-latency live audio monitoring for a C-side DAW over WAN (§8 explains why
this is a physics limit); AAA gaming over the command-stream path; a clean-sheet graphics-API
remoting protocol (we reuse Venus/virgl/gfxstream).

## 4. Thesis: "native remote Wayland" is barely a Wayland change

When you trace where the work lands, **Wayland-proper needs only three small extensions.**
The heavy lifting belongs in **Mesa** and in a **new sibling protocol** that sits *beside*
Wayland — exactly as GL already sits beside Wayland today. Wayland never carried GL; it
carried buffers. Native remote rendering keeps that boundary and merely lets the buffer be
produced by a renderer the compositor co-owns.

## 5. The six growth areas

**① Mesa: a transport-abstracted remoting driver — `[grow existing]`**
Venus (Vulkan-on-virtio) and gfxstream already serialize a Vulkan command stream and replay
it on a host GPU. Native support = promote their transport from a virtio ring to a pluggable
backend with a **network** option (vsock/QUIC). Biggest chunk of real code, but *grow a
driver*, not invent one.

**② The Zink simplifier — `[exists, lean on it]`**
Do **not** remote GL and Vulkan separately. **Zink** turns GL/GLES into Vulkan, so the
ecosystem need only remote *one* API — Vulkan — and every GL app rides it for free.

**③ Wayland protocol extensions — `[new, but small]`** — the only Wayland-proper changes:
- **Buffer-by-token** — a `linux-dmabuf-v1` sibling where a buffer is named by an opaque
  handle valid on the *compositor's* GPU, not by an FD passed from the client. The crux move:
  C references a dmabuf that physically lives on S and that C can never hold an FD to.
- **Remotable sync** — `linux-drm-syncobj-v1` (already landed) barely changes, because the
  fence is *S-local*: the renderer on S signals it, the compositor on S waits on it. Only the
  *trigger* to advance the timeline rides in from C's command stream.
- **Network display addressing** — the analog of `DISPLAY`: a way for an app on C to find and
  attach to S's compositor (see §8; in practice this lives in the proxy, not libwayland).

**④ The sibling protocol — `[genuinely new]`** — memory-coherence + content-addressed asset
residence + adaptive L3 policy. Lives beside Wayland. See §6–§7.

**⑤ Compositor plumbing — `[grow existing]`**
wlroots/Mutter/KWin grow a "render backend that hosts a remoting renderer": accept
token-buffers, share a GPU allocator namespace with the S-side renderer service, wait on the
remotable syncobj. wlroots is the natural reference vehicle.

**⑥ Security / session — `[new, but conventional]`** — see §8.

**Payoff:** what people call "remote Wayland" is ~90% a Mesa problem and a new-sibling-protocol
problem. Wayland itself contributes three modest extensions, and two of the three heavy pieces
(Venus, Zink) already ship. The closest existing *complete* instance of this architecture is
**ChromeOS Crostini** — Sommelier (Wayland proxy) + gfxstream (API remoting) + virtio-gpu
(transport) — i.e. our design with exactly one substitution: **transport = network instead of
virtio.**

## 6. The sibling protocol — core

### Transport: one QUIC connection, many streams

QUIC is load-bearing. It gives independent, individually-flow-controlled streams over one
connection (a 40 MB texture upload cannot head-of-line-block a 64-byte uniform update),
0-RTT reconnect, **connection migration** (laptop ethernet→wifi re-tunes instead of dropping),
and **datagrams** (unreliable, for media — see §8). Logical streams by priority:

- **Control/sync** — tiny, highest priority: fence signals, present-complete, readback-ready.
- **Command** — the Venus Vulkan stream. Reliable, ordered, high rate, one-directional C→S.
- **Memory** — mapped-memory deltas. Bidirectional (uploads C→S, readbacks S→C).
- **Asset** — content-addressed bulk blobs. Reliable, low priority, preemptible.
- **Media (reserved)** — QUIC datagrams for audio; unreliable, preempts bulk (§8).

### Piece 1 — Memory coherence (the hard core)

The difficulty is Vulkan host-visible memory: the app maps a pointer, writes, and expects the
GPU to see it. The lever is **which memory types the remoting driver on C advertises**:

- **Do not advertise `HOST_COHERENT`.** Coherent memory has no flush call, so there is no
  signal telling you when to ship. Expose only **non-coherent** `HOST_VISIBLE` memory; the app
  is then spec-required to call `vkFlushMappedMemoryRanges` / `vkInvalidateMappedMemoryRanges`,
  and **those calls become the explicit ship/fetch protocol events.**
- **Upload:** `vkFlushMappedMemoryRanges(range)` → ship exactly those bytes, tagged to the
  S-side allocation.
- **Readback (latency-critical reverse):** `vkInvalidateMappedMemoryRanges` → S ships the
  region S→C. Map-for-read right after a GPU write is an unavoidable round-trip — the thing
  WAN mode must fight.
- **Dirty-track fallback** for apps demanding coherent memory: write-protect mapped pages
  (userfaultfd), catch first-write-per-page, ship dirty pages at submit. Slower; the escape
  hatch that keeps everything working.
- **Staging-copy idiom is free:** "write staging buffer → `vkCmdCopyBuffer` to device-local"
  ships bytes once on flush; the copy is just a command. Upload-once, draw-many results.

### Piece 2 — Content-addressed asset residence (the lever)

Every C-authored payload is named by a **content hash** (BLAKE3). Before any bytes cross:

1. C sends `HAVE? <hash> <size> <format>` on the asset stream.
2. S checks its **persistent content-addressed cache**. `HIT` → the renderer binds the
   already-resident GPU resource. **Zero bytes cross.**
3. On `MISS`, the **residence oracle**: C's `HAVE?` carries provenance hints ("these bytes are
   `/nas/tex/foo.ktx2` on filesystem-UUID X" / "object-store key K"). If S can reach that store
   directly, **S fetches the payload over its own fast local path — nothing crosses the thin
   C→S link.**
4. Only if all else fails does C ship the bytes (GPU-compressed BCn/ASTC passed through).

Consequences:
- **The protocol self-partitions.** *S-born* data (render targets, GPU-generated textures) is
  born and consumed on S's GPU and **never crosses.** Only *C-born* data enters the residence
  protocol, and crosses once — or zero times if resident elsewhere.
- **The weak-SBC case collapses to nothing.** A Milk-V loading a 2 GB scene from the same NAS
  the laptop mounts touches *none* of the texture bytes — it emits `bind asset <hash>` and S
  self-fetches. C's job shrinks to logic + command emission, matched to a weak/foreign-arch
  client. Skinnier than X11 ever managed.

## 7. The sibling protocol — adaptive L3 policy (LAN↔WAN)

The connection continuously estimates RTT/bandwidth/loss and tunes knobs; it is a dial, not a
cliff. The enemy on WAN is **round-trips, not bandwidth**:

- **LAN (sub-ms):** synchronous readbacks on demand, fine-grained flushes, 1-frame buffering.
  Feels local.
- **WAN (20–50 ms):** one-directional bias — C never blocks on S if avoidable; pipeline 2–3
  frames; **coalesce flushes** into one burst per submit; **predict/prefetch readbacks**
  speculatively; **prefetch assets** ahead of need (and lean on the residence oracle so
  bandwidth is saved entirely); compress only when the link is thin.
- **Per-surface regime flip.** If a surface proves readback- or dynamic-upload-heavy on a bad
  link and C has a working encoder, the policy flips *that surface* to the video regime
  (encode-on-C). The regime becomes an adaptive, per-surface decision, not a static choice.

### Frame lifecycle (steady state)

1. App on C records a command buffer; references a texture → `HAVE? hash` → S `HIT`, binds
   handle. *(0 bytes)*
2. App writes uniforms to a mapped staging buffer → `vkFlushMappedMemoryRanges` → bytes ship
   on the memory stream. *(tiny)*
3. App submits → draws, state, staging→device copy ship on the command stream.
4. S-side renderer replays on the real GPU → **S-local dmabuf**, signals an **S-local
   drm-syncobj** timeline point.
5. Wayland proxy on S attaches the dmabuf **by token**, commits, points the compositor at the
   timeline point.
6. Compositor **waits on the fence locally (no network)**, composites, presents on S's display.
7. Input flows S→C on the Wayland channel.

**Steady state — assets resident, no readbacks — only commands + tiny uniform deltas cross
C→S, and only input crosses S→C.** The modern indirect-GLX dream, lighter than X11.

## 8. Security, session, and network-display addressing

**The app stays blind.** The app on C connects to a bog-standard `WAYLAND_DISPLAY` Unix socket
— the **C-side proxy** (Sommelier/waypipe half). All addressing, auth, and transport live in
the proxy, never in the app or libwayland. Network-display addressing is thus "how does the
proxy find and authenticate to S," a service-endpoint + credential problem. Keep secrets out of
env vars (`/proc/PID/environ` leaks).

**Transport security — "mosh for GPU."** Bootstrap with SSH, run over QUIC: `ssh S` proves
both identities using existing keys, and hands back a short-lived QUIC session key + UDP
endpoint; the bulk render protocol then runs over native QUIC. This is mosh's proven model and
gives SSH's trust ecosystem *and* QUIC's transport quality. For a fleet with existing SSH
config, **the existing SSH setup is the auth layer — zero new credential management.** A pure
SSH-tunnel mode remains available for restrictive networks (at the cost of TCP head-of-line
blocking).

**Reattachable sessions fall out.** App on C keeps running, GPU state lives on S, transport is
migratable → detach on ethernet, reattach on wifi, app still there. tmux/mosh semantics for
GPU apps.

**Authorization — beat X11's flat trust.** X11's sin was that any connected client could
keylog/screenshot/inject into every other client. Here:
- A remote client is just an **isolated Wayland client on S** — the proxy connects as an
  ordinary client, so all Wayland isolation guarantees hold automatically.
- Remote clients get **fewer** privileges than local ones: screencopy, input injection,
  layer-shell denied by default; clipboard is an explicit, revocable grant.
- **The Wayland-proper hook already exists:** `wp_security_context_v1` (built for Flatpak) lets
  the proxy tag the connection "remote + capability set X"; the compositor's existing policy
  engine does the rest.

**The one genuinely dangerous thing: remote GPU command execution.** You are letting a remote
machine drive your real GPU driver by replaying a command stream. GPU drivers are a notorious
exploit surface (malformed streams, OOB descriptors, malicious SPIR-V, hangs that freeze the
display). Defend it like a hypervisor defends against guest escape:
1. **Treat the command stream as hostile input** — full validation, bounds-checked descriptors,
   SPIR-V validation/sanitization before anything reaches the real driver.
2. **Inherit the threat model** — Venus/virglrenderer exist *because* a VM guest driving the
   host GPU is the same problem; their host-side hardening is directly reusable. Strongest
   argument for building on Venus rather than clean-sheet.
3. **Sandbox the replay** — seccomp'd separate process, own GPU context / render node, so a
   compromise is contained and a hang can be reset without killing the compositor. The
   compositor only ever touches the dmabuf token.
4. **Quota and watchdog** — per-client VRAM quotas, command-rate limits, GPU-reset watchdog.
5. **Feature restriction as security** — the S∩C capability negotiation also *withholds* the
   sketchiest extensions from remote clients.

**GPU capability negotiation.** At attach, C's driver and S's renderer take the **intersection**
of Vulkan version/extensions/limits/formats/memory-types. Because the C-side driver answers the
app's `vkGetPhysicalDeviceFeatures`, it transparently presents only the S-deliverable,
S-permitted set. The intersection is both a compat filter and a security filter.

**Wayland-proper needs almost nothing here:** a "remote client" security-context tag, which
`wp_security_context_v1` already provides. Auth, transport, addressing, and GPU sandboxing
correctly live outside the display server.

### Audio forward-compatibility (reservations only; full design deferred)

Full audio is deferred to a later session, but three cheap reservations avoid double-paying,
and one latency limit must be stated honestly:

- **Transport already accommodates it.** Pro audio needs an *unreliable, low-jitter,
  droppable* path — a late sample must be dropped, not retransmitted. **QUIC datagrams are
  exactly that.** The transport chosen for graphics reasons is already audio-friendly; an
  SSH-TCP-only design would have forced a later transport rebuild.
- **Reserve a shared session clock / timestamp base** (lean on Wayland `presentation-time`) so
  future A/V sync has a common timeline. Cheap to reserve, painful to retrofit.
- **Reserve a media-preempts-bulk priority tier** (QUIC prioritization already exists) and name
  **PipeWire as the S-side sink** and video-regime carrier, so audio later is "add a
  PipeWire-remote link," not a graphics-protocol change.
- **Honest latency limit (physics, not engineering).** *Playback* (hearing a mix) is fine on
  LAN and WAN via an S-side jitter buffer, at the cost of buffering latency. *Live monitoring*
  (MIDI played on S → Bitwig engine on C → audio back to S) puts a full network round-trip
  inside the real-time path: borderline on a fast LAN, unusable on WAN. The two failure modes
  are separable — a C-side DAW's own xruns are RT-scheduling starvation on C (not our problem);
  wire-induced dropouts are new and are mitigated, never eliminated, by S-side buffering that
  trades latency for safety. No real-time guarantee is promised across the wire.

## 9. Honest ledger: invented vs assembled

- Venus-over-network transport — **grow existing.**
- Zink for GL-via-Vulkan — **exists.**
- Content-addressed asset cache + residence oracle — **new**, but conceptually a CAS +
  provenance hints. Highest-value novel idea here.
- Memory-coherence protocol (non-coherent-only + flush-shipping + dirty-track fallback) —
  **new integration** of known techniques.
- Adaptive L3 policy with per-surface regime flip — **new**; makes it survive real networks.
- Wayland extensions (buffer-by-token; remotable syncobj already exists; security-context
  already exists) — **small.**
- Security model — **conventional**, reusing SSH + VM-GPU-escape hardening.

## 10. Prior art and reference points

- **Crostini / Sommelier + gfxstream + virtio-gpu** — the near-complete existing instance;
  swap virtio for network.
- **Venus** (Mesa Vulkan-on-virtio) + **VirGLrenderer** host — the command-remoting protocol.
- **gfxstream** — second, transport-abstracted API-forwarding implementation.
- **Zink** — GL/GLES → Vulkan.
- **waypipe** — Wayland protocol forwarding (ships buffer contents today).
- **Indirect GLX** — the historical precedent this resurrects for Vulkan.
- **VirtualGL** — the *opposite* topology (GPU co-located with the app, ships pixels to a thin
  client); architecturally instructive as the anti-pattern for this design.
- **mosh** — SSH-bootstrap + UDP-datagram data plane + roaming; the session/transport model.
- **PipeWire** — the designated S-side media sink and video-regime carrier.
- **`linux-drm-syncobj-v1`, `linux-dmabuf-v1`, `wp_security_context_v1`, `presentation-time`**
  — existing Wayland protocol pieces this leans on.

## 11. Deferred / open questions

- **Audio (full design)** — the `iv` follow-up: PipeWire-remote integration, A/V sync clock,
  monitoring-latency envelope, xrun accounting.
- **Multi-GPU on S** — renderer placement, cross-GPU dmabuf, output/GPU affinity.
- **Video-regime protocol integration** — how per-surface encode-on-C coexists with
  command-stream surfaces on the same connection.
- **Residence-oracle trust model** — how S safely self-fetches without C lying about a hash
  (content-verify on fetch; provenance authorization).
- **Readback latency math** — quantifying WAN prediction/prefetch effectiveness per workload
  class.
