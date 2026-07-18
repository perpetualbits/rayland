# (c)2 — how S can read the app's readback: what virglrenderer 1.2.0 does and does not allow

**Status:** investigation note, 2026-07-18. Opens (c)2 (the readback return path handed over by (c)1).
It answers one question — *can S obtain the application's GPU-written readback buffer through a
coherent, fenced, engine-side read?* — from the actual virglrenderer source, and reframes (c)2's
options accordingly. Full trace with per-claim citations:
`.superpowers/sdd/` scratch is git-ignored, so the durable evidence is
`scratchpad/c2-transfer-reachability.md` (kept alongside this note's session) and the citations below.

## The verdict: the engine-side transfer path is a *hardcoded stub*, not a soft limitation

(c)1's handoff (and this repo's CLAUDE.md, now corrected) pointed at "make S a fenced engine-side
consumer via `virgl_renderer_transfer_read_iov`, as C0's `read_back` does." **That path does not
exist for the application's blob, and it is not close.** Read against `virglrenderer-1.2.0`
(commit `500b41d5c8`, the installed `libvirglrenderer1 1.2.0-2ubuntu2`) and cross-checked against
`virglrenderer-1.3.0`:

- **`transfer_read_iov` on the Venus context is a permanent stub.** Rayland creates its context with
  `VIRGL_RENDERER_CAPSET_VENUS` + `VIRGL_RENDERER_RENDER_SERVER`, which routes every transfer to
  `proxy_context_transfer_3d` (`src/proxy/proxy_context.c:412-421`) — a function whose entire body is
  `proxy_log("no transfer support..."); return -1;`. It never looks at the resource. It is unchanged
  byte-for-byte in 1.3.0. This *is* the log line Rayland already recorded empirically
  (`crates/rayland-engine/src/virgl.rs:858-869`).
- **The `ctx_id = 0` bypass fails for blobs too.** It requires `res->pipe_resource`
  (`src/virglrenderer.c:378-379`), which a blob created via `virgl_resource_create_from_fd` never sets
  (`src/virgl_resource.c` — only the classic vrend/GL path sets it). Re-importing the fd produces the
  same `pipe_resource`-less resource.
- **There is no resource-to-resource copy or blit anywhere in the API** (whole header enumerated). The
  only way to fill a fresh classic resource is `transfer_write_iov` from a CPU `iovec` you already
  hold — i.e. you must have `resource_map`'d the blob first, so it composes *from* the mmap, it does
  not avoid it.
- **`virgl_renderer_resource_map` is a bare `mmap(MAP_SHARED, fd)`** (`src/virglrenderer.c:1248-1253`)
  with **no flush/invalidate anywhere** — the host's `vkFlush/InvalidateMappedMemoryRanges` dispatch
  is `NULL` (`src/venus/vkr_device_memory.c:479-480`), and the tree contains no `DMA_BUF_IOCTL_SYNC`
  or `msync` on this path. So going through the engine's own accessor buys correct fd/size lookup and
  nothing about coherence.

**C0's `read_back` works only because it transfers a *classic* resource S itself created via
`resource_create` (which has a `pipe_resource`) through the `ctx_id = 0` path — never the app's blob.**
That is why it was never a template for the app's readback.

## What this leaves: split timing from coherence, and keep both off the engine lock

The investigation also clarified *why* (c)1's Phase 1 fence contended, and points at a path that
avoids it:

- **The contention was Rayland's own engine `Mutex`, not virglrenderer's internals.** virglrenderer's
  classic API is not thread-safe, so Rayland serialises every engine call behind one lock; a
  `wait_for_context_fence` on the progress thread therefore blocks the message thread's per-delta
  doorbell (`engine.submit`). virglrenderer's *own* ring thread does not touch the resource table in
  steady state and is asleep ~1 ms after the app's last ring write (`src/venus/vkr_ring.c:242-335`,
  guest `VN_RING_IDLE_TIMEOUT_NS = 1 ms`). So the hazard is Rayland's lock, not the library.
- **Therefore the readback should use signals and primitives that do *not* take the engine lock:**
  - **Timing** — the app is released by the **fence-feedback word**, which is itself a GPU
    `vkCmdFillBuffer` into a blob S can watch by plain `mmap` (no engine call). Watching it tells S
    "the GPU is done" without a `wait_for_context_fence` and without the lock.
  - **Coherence** — the one primitive virglrenderer does *not* provide, `DMA_BUF_IOCTL_SYNC_START/_END`,
    is a **kernel dma-buf ioctl** S can issue directly on the fd it already holds, with no engine
    call and no lock. Per the source, this is the *only* avenue left for coherence, and it is exactly
    the avenue virglrenderer leaves unaddressed.

If that composes, S reads the readback with **no engine-lock involvement at all** — removing the
doorbell contention that sank Phase 1 — and gets coherent bytes from the kernel sync that virglrenderer
never issues.

## The gating unknowns (a small empirical spike answers all three)

Source cannot settle these; they depend on the host GPU/driver:

1. **Is the readback blob's fd a `DMABUF`** (needed for `DMA_BUF_IOCTL_SYNC`) or a plain `SHM` memfd
   (on which the ioctl is meaningless)? Check the export type for the actual fixture resource.
2. **What does `virgl_renderer_resource_get_map_info` report** for it — `CACHED` or `WC`? `WC` is
   independent evidence the memory is not naturally CPU-coherent (so a fence alone is insufficient and
   the sync is genuinely needed).
3. **Does `DMA_BUF_IOCTL_SYNC_START/_END` around the read actually make it coherent** — i.e. does
   `feedback-word-timing + dma-buf-sync + read` converge to correct pixels where the un-synced mmap
   tore? This is the decisive test.

**If yes:** (c)2 has a real, contention-free fix (no engine lock, no virglrenderer change). **If the
fd is `SHM`, or the sync does not help:** (c)2's remaining options are to patch virglrenderer (add a
real blob transfer or a coherent map) or the kernel export path — a heavier, upstream track.

## Corrected pointer

CLAUDE.md's (c)2 bullet previously named `transfer_read_iov` as the path forward. That is retracted
here: it is a hardcoded stub. The path forward under investigation is the `mmap` +
`DMA_BUF_IOCTL_SYNC` + fence-feedback-timing composition above, pending the spike.
