# The Venus command ring: what actually crosses the boundary, and what that costs Rayland

**Date:** 2026-07-15
**Status:** Findings document — evidence, not decisions.
**Produced by:** sub-project C0 ("Venus First Light"), Task 4 spikes.
**Parent design:** [`2026-07-13-native-remote-wayland-gpu.md`](2026-07-13-native-remote-wayland-gpu.md)
**C0 spec:** [`2026-07-14-c0-venus-first-light.md`](2026-07-14-c0-venus-first-light.md)
**How to run C0:** [`../c0-venus-first-light.md`](../c0-venus-first-light.md)

---

## 0. Why this document exists, and who it is for

C0 set out to prove that a real, unmodified Vulkan application could be captured on one machine and
replayed on another machine's GPU. It proved that — see the C0 run doc. Along the way it discovered
something more important, and more inconvenient, than the proof itself: **the thing we assumed was
our data path is not the data path.**

Sub-project (c)1's original one-line scope was *"swap the local socket for QUIC."* **That scope is
disproved.** There is no socket carrying commands to swap. This document is the evidence base from
which (c)1 must be redesigned, and it is written to stand alone: a reader who knows nothing about
Venus, virglrenderer, vtest, or command rings should be able to finish it and design from it.

Everything here is either **observed** (we captured the bytes on this host and they are reproduced
below) or **read from source** (`file:line` against a pinned tag). Where the two disagree, this
document says so explicitly and says which won. Where neither answers a question, it says
**open question** rather than guessing. Do not treat inference here as fact; the confidence table in
§9 is the summary.

**Provenance of every number below:**

| Component | Version | Role |
|---|---|---|
| Mesa (the Venus ICD — the **client**, machine C) | `mesa-26.0.3` (installed: `libgl1-mesa-dri 26.0.3-1ubuntu1`) | Captures and serializes the app's Vulkan |
| virglrenderer (`vkr` — the **host**, machine S) | `virglrenderer-1.2.0` (installed: `libvirglrenderer1 1.2.0-2ubuntu2`) | Replays the stream on the real GPU |
| GPU | Intel Iris Xe (RPL-P), `/dev/dri/renderD128` | Where the drawing actually happens |
| Clients driven | `vnprobe` (init-only, 3 runs), `rayland-refapp` (a full render) | The workloads captured |

---

## 1. Background: the five words you need before the findings make sense

Skip this section if you already know Venus. It is here because the findings are meaningless
without it, and CLAUDE.md's documentation rule says a reader who does not know this domain must be
able to follow along.

**Vulkan** is the low-level GPU API an application uses to draw. Normally an app's Vulkan calls go
into a driver that talks straight to the graphics hardware in the same machine.

**Venus** is a Vulkan driver that *has no hardware*. It is a real, conformant Vulkan driver
(an "ICD" — Installable Client Driver — the file Mesa ships as `libvulkan_virtio.so`, selected via
`VK_ICD_FILENAMES=…/virtio_icd.json`). Instead of programming a GPU, **it serializes every Vulkan
call into bytes and sends them somewhere else to be executed.** It was built for virtual machines:
an app inside a VM draws, and the *host* outside the VM does the drawing on the real GPU. From
Rayland's point of view Venus is the *capture* half of the problem, already written, already
shipping in every Mesa install, and already hardened. This is CLAUDE.md's locked decision 1a —
reuse, do not reinvent.

**virglrenderer** is the other half: the **replayer**. It takes the serialized command stream and
executes it against a real GPU. Its Venus-specific part is called `vkr`. It, too, comes from the VM
world, and it is hardened against exactly our threat model — *an untrusted party driving the host's
GPU*. Rayland FFI-embeds it as `libvirglrenderer` (C0 Task 1).

**vtest** is a Unix-socket wire protocol. In a VM, Venus talks to the host through a kernel device
(`virtgpu`). Outside a VM there is no such device, so Mesa has a second, developer-oriented backend
that talks over a plain Unix socket instead. That backend is called **vtest**, and it is the reason
Rayland can use Venus *with no VM at all*. C0's host implements the vtest protocol
(`crates/rayland-vtest/src/vtest.rs` — it lived in `rayland-engine` when this document was written;
(c)1 Task 1 moved it into its own crate, which by construction links no GPU code).

**The ring** is the thing this document is about. It is a fixed-size block of memory that *both*
the client and the host can read and write, with a producer writing commands into one end and a
consumer reading them out of the other — a classic single-producer/single-consumer circular buffer.
Mesa calls its implementation `vn_ring`. **The whole finding of this document is that the ring, not
the socket, is where the application's Vulkan commands live.**

---

## 2. Finding 1 — Venus does not send commands over the socket

**This is the finding that invalidates (c)1's original scope.**

We built C0 expecting the vtest socket to carry the application's Vulkan command stream, because
that is what a "wire protocol" implies and it is what the C0 spec assumed. It does not.

The socket carries exactly two kinds of thing:

1. **One `vkCreateRingMESA`** (command opcode **188** = `0xbc`) — *"here is a ring; here is its
   size and the offset of every one of its fields."*
2. **A handful of `vkNotifyRingMESA`** (opcode **190** = `0xbe`) — a **doorbell**, carrying no
   commands at all. It says only *"my write frontier has moved to X, come and look."*

That is all. Measured on this host, across three independent live captures of an init-only client:

- **100%** of the socket's command dwords are ring *management*.
- **0%** of the socket's command dwords are application Vulkan commands.
- **100%** of the application's Vulkan commands (`vkCreateInstance`, `vkEnumerateInstanceVersion`,
  and everything after) are in the ring, in shared memory, having crossed no socket at all.

**~4 KiB of ring traffic is a *complete* Vulkan initialization** (4024 bytes, identical in all three
runs). Against that, the socket carried **140–236 bytes**, all of it bookkeeping.

The consequence is stated bluntly in the ring module's own docs
(`crates/rayland-vtest/src/venus_ring/mod.rs`): **`RenderEngine::submit` — the path C0 built, and
the only path the socket feeds — never sees a single application Vulkan command.** It sees the
ring's *address*, and then a series of pokes. The real work is done by virglrenderer's `vkr_ring`
thread reading shared memory that our process never routes.

This is not a defect in C0's implementation. It is C0 having built a correct implementation of the
wrong channel — and finding out, which was the point of a walking skeleton.

### 2.1 How the commands reach the host without a socket

The mechanism is worth understanding precisely, because it is the reason remoting is hard.

Mesa **hardcodes** its shared-memory type to `HOST3D` on the vtest backend
(`vn_renderer_vtest.c:1055`):

```c
/* see virtgpu_init_shmem_blob_mem */
assert(vtest->capset.data.supports_blob_id_0);
vtest->shmem_blob_mem = VCMD_BLOB_TYPE_HOST3D;
```

`HOST3D` means: **the host's 3D driver allocates the memory, and the client maps the host's own
pages.** So the sequence is:

1. The client asks (over the socket) for a blob of 131268 bytes.
2. *Our host* allocates it through virglrenderer, and exports it as a file descriptor.
   Measured on this host, `virgl_renderer_resource_export_blob` returns `fd_type = 3` =
   `VIRGL_RENDERER_BLOB_FD_TYPE_SHM` — plain shared memory, not a DMABUF.
3. Our host passes that descriptor back over the socket using **`SCM_RIGHTS`**, the Unix mechanism
   for sending an open file descriptor to another process (C0 Task 4a built this).
4. The client `mmap`s it. **From this moment both processes are writing the same physical pages.**
5. The client writes commands into those pages with a bare `memcpy`, and the host reads them.
   **No protocol message is involved, and none is required.**

This is why an init-only run emitted only 2–5 `SUBMIT_CMD2` messages while executing dozens of
Vulkan calls, a puzzle that stood unexplained for two tasks. The commands were never on the socket.

> **The load-bearing observation for (c)1:** step 3 is the crux. **`SCM_RIGHTS` is a Unix-domain
> socket feature. It cannot cross a network.** QUIC has no fd-passing, and there is no such thing
> as sharing a memory page between two machines. So the shared-memory side-channel that carries
> 100% of the command stream *does not survive the move to a network at all.* This is not a
> transport substitution. It is a protocol design task.
>
> C0 encoded this at the type level rather than leaving it as a note: `serve_vtest` is generic over
> a `VtestTransport` trait in which `send_fd` is a **required** method, precisely so that a future
> QUIC transport confronts the gap **at compile time** instead of silently inheriting a broken
> assumption. (This supersedes the C0 plan's Self-Review §3, which claimed a plain
> `Read + Write` bound made the server "generic so (c)1 swaps in QUIC unchanged". It does not; a
> live client cannot work without fd-passing.)

---

## 3. Finding 2 — the ring carries the identical command language (this is the good news)

**This is the headline, and it is the reason remote Wayland is not dead.**

When the ring was discovered, the obvious fear was that the ring might contain something private,
opaque, or hardware-specific — a format we would have to reverse-engineer, or worse, one that
embeds host pointers and is meaningless on another machine. **It does not.** The ring's bytes are
*the same command language* the socket's inline path uses: the same `vn_cs_encoder` output, the
same `[VkCommandTypeEXT][VkCommandFlagsEXT]` prologue, the same `vn_sizeof_*` encodings.

**Remoting Venus is therefore a plumbing problem, not an encoding problem.** The stream is
*language*, and it is legible to us today — with no VM, no virtgpu, and no reverse-engineering —
because we hold the descriptor for the memory it lives in and its layout is declared to us in-band.

This was proven **twice, independently**, by two methods that share no assumptions.

### 3.1 Proof from source: the host decodes both channels with the same code

virglrenderer has three places that consume a Venus command stream, and all three are the *same
two-line idiom* — point a decoder at a byte range, then loop until it is exhausted:

**(a) the ring path** — `vkr_ring.c:220-223`:
```c
vkr_cs_decoder_set_buffer_stream(dec, buffer, size);

while (vkr_cs_decoder_has_command(dec)) {
   vn_dispatch_command(&ring->dispatch);
```

**(b) the inline `SUBMIT_CMD2` path** — `vkr_context.c:170-173` (this is what
`virgl_renderer_submit_cmd`, i.e. C0's `RenderEngine::submit`, actually reaches):
```c
vkr_cs_decoder_set_buffer_stream(&ctx->decoder, buffer, size);

while (vkr_cs_decoder_has_command(&ctx->decoder)) {
   vn_dispatch_command(&ctx->dispatch);
```

Same decoder type, same dispatch entry point, same generated table. **The two paths are
line-for-line identical, differing only in which decoder instance they use** — the ring owns a
private encoder/decoder pair (`vkr_ring.c:132-138`), the inline path uses the context's. The
*language* is byte-for-byte the same.

The producer side confirms it independently: `vn_ring_submit_internal` copies the encoder's
committed bytes **verbatim** into the ring (`vn_ring.c:451-455`), via a plain `memcpy`
(`vn_ring.c:127-142`). No re-encoding, no envelope, no per-command framing is added. *What the
encoder produced is what the ring holds.*

### 3.2 Proof from live capture: we decoded real commands out of the shared pages

Because our host allocates the blob and holds its descriptor, we can `mmap` the very same physical
pages the client writes into. Doing so and sampling them every 1 ms produced real commands. Decoded
at the ring's buffer base (**abridged** — every line's offset is given, so the omitted rows are the
gaps; the `}` braces mark the halves of a 64-bit field):

```
0xc0  000000b2   vkSetReplyCommandStreamMESA (178)
0xc4  00000000     cmd_flags = 0
0xc8  00000001 } pStream != NULL
0xd0  00000002     .resourceId = 2      <-- the 1 MiB blob
0xdc  00000014 } .size = 20                            (36 bytes total)

0xe4  00000089   vkEnumerateInstanceVersion (137)
0xe8  00000001     cmd_flags = 1  (generate reply)
0xec  00000001 } pApiVersion != NULL                   (16 bytes total)

0xf4  000000b2   vkSetReplyCommandStreamMESA (178)
0x108 00000014 } .offset = 20           <-- chains off the previous size
0x110 00000018 } .size = 24                            (36 bytes total)

0x118 00000000   vkCreateInstance (0)
```

Five confirmations, **none of which was fitted to the data**:

1. **Sizes match Mesa's `vn_sizeof_*` byte-for-byte.** `vkSetReplyCommandStreamMESA` sums to
   4+4+8+4+8+8 = **36**; observed `0xc0 → 0xe4` = **36**. `vkEnumerateInstanceVersion` sums to
   4+4+8 = **16**; observed = **16**. The sizes come from Mesa's headers; the bytes come from the
   live client. Neither was derived from the other.
2. **The reply-offset chain is self-consistent:** `offset=0, size=20`, then `offset=20, size=24`.
3. **`head` lands exactly on a command boundary:** head = 88 at the first sample, and
   36 + 16 + 36 = **88**. The host had consumed exactly three whole commands.
4. **`resourceId = 2` is the second blob** — so the replies go to a *different* shmem (§6).
5. **The replies confirm it from the other side.** The reply arena's first bytes echo
   `0x89` = `vkEnumerateInstanceVersion`, and `0x00404155` decodes as **Vulkan 1.4.341**. The
   second reply, at exactly offset 20 where `SetReply` #2 pointed, is `cmd_type = 0` =
   `vkCreateInstance` — matching ring command #4. The arena also carries the ASCII string
   `Intel(R) Iris(R) Xe Graphics (RPL-P)`.

**Cross-check from a capture that predates the spike.** Task 4a captured a `SUBMIT_CMD2` payload
before anyone was looking for a ring. Its `dword[9] = 0x000000bc` = opcode **188** =
`vkCreateRingMESA`. The socket's one real command is *"create the ring"* — independently
corroborating §2 from evidence gathered for a different purpose.

### 3.3 This finding is now in the repository, not just in this document

The captured bytes are a **CI fixture test** (`crates/rayland-vtest/src/venus_ring/captured.rs`),
running with no GPU and no Mesa install. It is **mutation-verified, not decorative**: changing
`encoded_size(178)` from 36 to 40 independently fails **5 of the 6** fixture tests.

---

## 4. Finding 3 — the ring's layout

The client's first blob is **131268** bytes. That number is what made the ring findable: a 128 KiB
power-of-two buffer plus a 196-byte remainder, and *a non-power-of-two remainder next to a
power-of-two buffer is what a header looks like.*

The decomposition closes exactly:

```
192 (control) + 131072 (128 KiB command buffer) + 4 (extra) = 131268
```

| offset | field | size | written by | read by |
|---|---:|---:|---|---|
| `0x00` | **`head`** — bytes *consumed* | 4 (64 reserved) | **host (S)** | client |
| `0x40` | **`tail`** — bytes *produced* | 4 (64 reserved) | **client (C)** | host |
| `0x80` | **`status`** — a bitmask | 4 (64 reserved) | **host (S)** | client |
| `0xc0` | **buffer** — the command stream | **131072** | **client (C)** | host |
| `0x200c0` | `extra` | 4 | host (S) | *nobody* — see below |

Each control word sits on **its own 64-byte cache line** (Mesa declares them `alignas(64)`,
`vn_ring.c:254-275`). This is deliberate false-sharing avoidance: `head` and `tail` are written by
different threads on different sides of the mapping, and packing them into one cache line would
make every doorbell a cache-coherence storm. **A reader that assumes three adjacent dwords will
read garbage.**

**The layout is not guessed and must not be hardcoded.** Every one of these offsets is *declared to
us in-band*, by the client, in the `vkCreateRingMESA` command's `VkRingCreateInfoMESA` — precisely
so the host need not know them a priori. We verified this in both directions: the declared values
were read off the socket, and independently, every observed change in the shared pages fell at
exactly the declared offset and nowhere else. The engine's constants
(`venus_ring::RING_HEAD_OFFSET` and friends) exist so the fixture test has something to assert
against, and their docs say plainly that a production reader must parse them from the message.

### 4.1 Two pitfalls in the control words that will bite a naive reader

**`head` and `tail` are free-running byte counters, not offsets.** They are masked into the buffer
only at access time (`ring->cur & ring->buffer_mask`, `vn_ring.c:132`), and the buffer size is
asserted to be a power of two so the mask is cheap. Wrap is by **unsigned 32-bit overflow**, and
the arithmetic is deliberately wrap-safe: occupancy is computed as a *difference*
(`vn_ring.c:215`, `vkr_ring.c:289`), which stays correct across the 2^32 wrap. A reader that treats
them as indices into the buffer works fine right up until the moment it does not.

**The strongest single piece of evidence in the whole capture** is that `tail` predicts the future.
`tail` is an offset from the buffer base, so `0xc0 + tail` must be exactly where the *next*
sample's writes begin. Three consecutive samples, no fitting:

| sample | observed `tail` | predicted `0xc0 + tail` | next sample's first changed range |
|---|---|---|---|
| +2.01 ms | `0xd8` (216) | **`0x198`** | `0x198..0x19c` ✅ |
| +10.67 ms | `0x120` (288) | **`0x1e0`** | `0x1e0..0x1e4` ✅ |
| +22.94 ms | `0xd48` (3400) | **`0xe08`** | `0xe08..0xe0c` ✅ |

**`status` is a bitmask, and its polarity is the opposite of what "status = 1" suggests.** From
Mesa's own generated headers (`vn_protocol_renderer_defines.h:473-478`, and identically in the
driver-side copy):

```c
VK_RING_STATUS_NONE_MESA  = 0,
VK_RING_STATUS_IDLE_BIT_MESA  = 0x00000001,
VK_RING_STATUS_FATAL_BIT_MESA = 0x00000002,
VK_RING_STATUS_ALIVE_BIT_MESA = 0x00000004,
```

So **`status & 1` set means the host's ring thread is IDLE (parked)**, and `status == 0` means it
is *actively polling*. This is worth stating loudly because it is counter-intuitive — "1" reads
like "busy" — and because the C0 spike report and the engine's own module docs initially recorded
this polarity **inverted**. The observations were never affected (the raw values are what they
are, and the code uses observed offsets, not the polarity), but the mechanism in §5.2 only makes
sense the right way round: *the client kicks the host when it sees the IDLE bit, i.e. when
`status == 1`.* The inverted claims have been corrected in the same change that created this
document.

### 4.2 The `extra` region is vestigial in this Mesa version

It is declared to the host (`vn_ring.c:362-363`), mapped by the client (`vn_ring.c:315`), and the
host can write it via `vkWriteRingExtraMESA`. Its documented purpose is "32-bit seqno for renderer
roundtrips". **But nothing on the client ever reads it** — a grep of all 48 `.c`/`.h` files in
Mesa's `src/virtio/vulkan/` finds `shared.extra` only at its assignment, and
`vn_async_vkWriteRingExtraMESA` is never called from any driver source. Roundtrips in 26.0.3 go
through `vkSubmitVirtqueueSeqnoMESA`/`vkWaitVirtqueueSeqnoMESA` instead, which use a host-side
condition variable. **[INFERENCE]** Rayland can allocate it to keep the layout honest and otherwise
ignore it — but must not *rely* on it being unused, since the host will faithfully write it if a
`vkWriteRingExtraMESA` ever arrives.

---

## 5. Finding 4 — the four obstacles

None of these is opacity. That is the point: the stream is legible (§3), so every obstacle below is
about *plumbing and timing*, which is the kind of problem engineering can solve. They are listed in
ascending order of nastiness, and they are the most valuable things C0 learned. **They are the
point of this document, not an appendix.**

### 5.1 Obstacle 1 — there is no seam to hook

*Why this is hard:* to ship the ring's bytes across a network, something must notice that the bytes
changed. In a well-layered system there would be an obvious place to hook that — a `flush` callback
the driver calls when it has finished writing. **There is no such place, and the abstraction has no
entry point that could be made into one.**

Venus's renderer abstraction (`vn_renderer`) does have coherence hooks — `bo_flush` and
`bo_invalidate` — and the initial assumption (which I had stated to the owner as fact) was that
these were an existing seam we could implement. **That was refuted, and this is the single most
important correction the spikes produced.** Both hooks are **nops in *both* backends**. Not just in
the vtest backend — in the real virtgpu backend too (`vn_renderer_virtgpu.c:1090-1105`):

```c
static void
virtgpu_bo_flush(struct vn_renderer *renderer, struct vn_renderer_bo *bo,
                 VkDeviceSize offset, VkDeviceSize size)
{
   /* nop because kernel makes every mapping coherent */
}
```

A grep of the entire 1838-line file for any `TRANSFER_TO_HOST`/`TRANSFER_FROM_HOST` ioctl or
`msync` finds **none**. **Both existing backends assume fully coherent shared memory. There is no
in-tree model for a non-coherent backend. Rayland would be authoring the first one.**

And it is worse than "flush is a nop", structurally:

- `bo_flush`/`bo_invalidate` operate on a `vn_renderer_bo` — *device memory*. They are the only
  coherence hooks in the entire vtable.
- **The ring is not a `bo`.** It lives in a `vn_renderer_shmem` (`vn_ring.c:294-295`).
- **`vn_renderer_shmem_ops` has exactly two members: `create` and `destroy`**
  (`vn_renderer.h:141-146`). There is no flush, no invalidate, no notify, no barrier.

> There is **no vtable seam at which a Rayland backend could hook "the ring's bytes changed, ship
> them"**. The client writes the ring with a bare `memcpy` and a bare `atomic_store` straight
> through `shmem->mmap_ptr`. The `vn_renderer` abstraction is never consulted, and has no entry
> point that *could* be consulted.

**Unverified mitigations, recorded as leads, not decisions:** the ring lives in the *application's
own process*, so a Rayland C-side component could poll `tail` locally — the missing notification
never needs to cross the network at all. Alternatively, patch `vn_ring` to add a notify hook (a few
lines, plausibly upstreamable). Neither has been tried.

### 5.2 Obstacle 2 — there is no steady-state notification to listen for

*Why this is hard:* even granting a place to hook, a transport wants an *event* — "new work, ship
it". In the steady state, **no event exists**. The ring's own header says so
(`vn_ring.h:16-18`):

> "An externally-defined mechanism is required for ring setup and notifications in both directions.
> **Notifications for new data from the producer are needed only when the consumer is not actively
> polling, which is indicated by the ring status.**"

The client kicks the host **only** if the host has declared itself idle *and* it has been ≥1 ms
since the last kick (`vn_ring.c:475-483`):

```c
if (status & VK_RING_STATUS_IDLE_BIT_MESA) {
   const int64_t now = os_time_get_nano();
   if (os_time_timeout(ring->last_notify, ring->next_notify, now)) {
      ring->last_notify = now;
      ring->next_notify = now + VN_RING_IDLE_TIMEOUT_NS;
      return true;   /* the only path that sends vkNotifyRingMESA */
   }
}
return false;
```

Meanwhile the host busy-polls (`thrd_yield()` for 16 iterations, then an exponentially growing
sleep from 10 µs) and only sets the IDLE bit after 1 ms of nothing (`vkr_ring.c:263-327`).

> **In the steady state — an actively rendering application, host thread never idle — the number
> of notification messages emitted is ZERO.** The client's entire action is a `memcpy` into shared
> memory and a `seq_cst` store to a `uint32_t`. No syscall, no ioctl, no vtest message.

**Two consequences for (c)1:**

1. **A network transport cannot be purely event-driven off the existing protocol.** It must poll
   C's local `tail` word itself. That is cheap (a local 4-byte read, no syscall), but it means the
   transport owns a polling loop and its own latency/coalescing policy. *The kick is a wakeup, not
   a work announcement*, and it fires only when there is **least** to ship — it cannot be the
   "ship now" trigger. **[SOURCE]** for the mechanism; **[INFERENCE]** for the design consequence.
2. **Never use doorbell counts as a metric.** Byte-identical ring traffic produced **1** notify in
   one run and **4** in another. The count measures the host scheduler, not the workload. (This
   number misled us for two tasks; it is written down so it does not mislead anyone again.)

*Silver lining, **[INFERENCE]**:* polling locally is arguably **better** than a kick, because the
transport can coalesce a burst of commands into one network packet — precisely what a WAN wants,
and precisely what the shared-memory design cannot express.

### 5.3 Obstacle 3 — the ring is not self-contained (and a toy workload hides it)

*Why this is hard:* the appealing plan is "ship `ring[old_tail..new_tail]` and you have shipped the
stream." **That is true only for toys, and it fails silently on real work.**

If a *single submission's* encoded length exceeds **`direct_size` = `buffer_size >> direct_order` =
`131072 >> 4` = 8192 bytes** (`vn_ring.c:319`), Mesa does not put the command in the ring. It puts
a small **`vkExecuteCommandStreamsMESA`** (opcode **180** = `0xb4`) in the ring instead, which
**points at other shmems by `res_id`** (`vn_ring.c:494-528`):

```c
descs[desc_count++] = (VkCommandStreamDescriptionMESA){
   .resourceId = buf->shmem->res_id,
   .offset     = buf->offset,
   .size       = buf->committed_size,
};
```

The host resolves that `res_id` to a **host pointer** (`vkr_cs.c:75-94`) and runs the same dispatch
loop over that *other* memory. So: **ship the ring alone and you have shipped a pointer to data the
host does not have.**

**Task 4b never triggered this path — and traced exactly why.** Opcode 180 occurs **zero** times,
in any of the six blobs, across every sample:

```
res1 (  131268 B, 8 samples): dwords ever == 180 -> 0     (the command ring)
res2 ( 1048576 B, 7 samples): dwords ever == 180 -> 0
res3 (      64 B, 5 samples): dwords ever == 180 -> 0
res4 ( 8388608 B, 4 samples): dwords ever == 180 -> 0
res5 (    4096 B, 3 samples): dwords ever == 180 -> 0
res6 (   16384 B, 2 samples): dwords ever == 180 -> 0
```

This is conclusive rather than a sampling artefact: `tail` is monotonic and never wrapped, so the
final sample's `buffer[0..9936]` contains **every byte the client ever wrote to the ring**, and 180
is not among them. The root cause is not "not observed" — it is pinned: **the largest input this
app has is a 1008-byte SPIR-V module**, nowhere near 8192.

> **So "the ring is the whole stream" is TRUE HERE AND WILL BREAK ON THE FIRST REAL APP.** The
> threshold is a per-submission byte count, not a property of the workload class. Any real
> application — non-trivial SPIR-V, large descriptor writes, a big `vkCmdUpdateBuffer`, or simply a
> longer command buffer — crosses 8192 routinely, and the ring becomes *pointers into other
> shmems*. **Anything in (c)1 that assumes otherwise is correct only for triangles.**

**The trap 4b found, which is exactly the shape of mistake this will cause.** Venus *does* stage
command-buffer recording into a **separate 8 MiB shmem** (`res=4`), and the agent found its app's
exact `VkViewport` sitting in it:

```
res=4 @0x0e8 as f32: [0.0, 0.0, 64.0, 64.0, 0.0, 1.0]
refapp VkViewport   : [0.0, 0.0, 64.0, 64.0, 0.0, 1.0]
```

That **looks** like proof the out-of-line path is in use. **It is not.** Because the recorded
stream is under 8192 bytes, Venus copies those staged bytes **inline into the ring** at submit —
proved directly by locating the same `0x42800000 0x42800000` (64.0f, 64.0f) at ring dword 2005. So
`res=4` is *staging*, not a second data path, for this workload. **A separate shmem containing your
app's data is not evidence of the out-of-line path.**

**Lever worth recording:** `direct_order` is a **client-side constant** (`vn_instance.c:152`), and
`direct_size = buffer_size >> direct_order`. At `direct_order = 0`, `direct_size == buffer_size` —
*every* command that fits in the ring stays inline. Combined with a larger ring
(`VKR_RING_BUFFER_MAX_SIZE` is 16 MiB, `vkr_ring.h:20`), this could plausibly make the ring
genuinely self-contained for early slices, deferring `res_id` replication entirely.
**[INFERENCE] — untested, and note `vn_ring_cs_upload_locked` (`vn_ring.c:595-618`) still exists as
a fallback for commands larger than the ring itself, so the indirect path can never be eliminated
outright.** This is the single highest-leverage open experiment for (c)1.

### 5.4 Obstacle 4 — the watchdog reports liveness it never checks

*Why this is hard:* a network host is slow, and slow hosts trip watchdogs. There **is** a watchdog,
it calls `abort()`, and — the nasty part — **it will happily tell you everything is fine while
nothing is happening.**

The host runs a **separate** monitor thread (`vkr-ringmon-%d`) that sets the ALIVE bit on every
monitored ring every ~3 s (period chosen by the client, `VN_WATCHDOG_REPORT_PERIOD_US` = 3 s). The
client checks it while spinning, and `abort()`s if the heartbeat expired (`vn_common.c:278-282`).
Budgets: first check at **~3.5 s** of spinning, hard abort at **~895 s**.

The budget itself is WAN-friendly — a 200 ms RTT link is nowhere near tripping it, and the
heartbeat is a trivially forwardable periodic 4-byte OR. **The problem is what it means**
(`vkr_context.c:536-539`):

```c
list_for_each_entry (struct vkr_ring, ring, &ctx->rings, head) {
   if (ring->monitor)
      vkr_ring_set_status_bits(ring, VK_RING_STATUS_ALIVE_BIT_MESA);
}
```

It sets ALIVE **unconditionally**, for every monitored ring, **without consulting the ring thread's
state at all**, from a thread that does nothing else. **It proves the host process is being
scheduled. It proves nothing whatsoever about the ring making progress.**

> **The footgun, stated plainly:** a Rayland transport that faithfully forwards the heartbeat while
> the ring is stalled converts a **fast, diagnosable 3.5-second abort** into an **895-second
> hang**. Forwarding it faithfully is the *obvious* implementation and it is the wrong one.
> A correct transport must gate the heartbeat on evidence of actual ring progress.

**What *does* assume low latency is not the watchdog — it is the polling.** `vn_ring_wait_seqno`
(`vn_ring.c:181-198`) busy-polls the `head` word, with this candid comment:

> "A renderer wait incurs several hops and the renderer might poll repeatedly anyway. **Let's just
> poll here.**"

Every *synchronous* command hits this: `vn_ring_submit_command` waits whenever `reply_size` is
non-zero. **[INFERENCE]** This is the real latency sensitivity, and it is inherent to Venus's
request/reply design, not to the ring. Venus's own mitigation is that the overwhelming majority of
commands are `vn_async_*` (no reply, no wait); only genuinely synchronous Vulkan entry points pay
the round trip. See §7.

---

## 6. Finding 5 — the genuinely hard problem is `vkMapMemory`, not the ring

**If you remember one thing from this document after the headline, remember this one.**

Everything above is about a command stream, and a command stream can be intercepted because
commands are *calls*. But modern Vulkan's whole performance model is that the hot path is **not**
calls. An application:

1. calls `vkMapMemory` **once**, getting a raw pointer into GPU-visible memory;
2. then writes vertices, uniforms, and texture data **straight into that pointer** — for the rest
   of the program's life;
3. with **no API call at all** for any of those writes.

> **There is no command to intercept. There is no event. There is nothing on any wire.** The app
> writes to memory, and on a single machine the GPU simply sees it.

This is **modern Vulkan's shape, not Venus's failing.** Any remote Vulkan — Rayland's or anyone
else's — must answer it. It cannot be dodged by choosing a different capture engine, because it is
not the engine's doing; it is the API's contract.

**The evidence that this is real and not theoretical (Task 4b).** A rendering app created six
blobs, all `HOST3D`, all `MAPPABLE`. Two of them are the app's own data, crossing the boundary
through mapped memory with no command in sight:

| res | size | blob_id | what it is | evidence |
|---|---:|---:|---|---|
| 1 | 131268 | 0 | the Venus command ring | 192 + 131072 + 4; `head`/`tail` behaviour |
| 2 | 1048576 | 0 | the **reply arena** | named by `vkSetReplyCommandStreamMESA` as `resourceId=2`; holds decoded replies |
| **3** | **64** | **16** | **the app's vertex buffer** | decodes float-for-float, below |
| 4 | 8388608 | 0 | command-buffer staging pool | holds the app's `VkViewport`; §5.3 |
| 5 | 4096 | 23 | allocated, **never written** (zero at every sample) | likely a `vn_feedback` pool; not chased |
| **6** | **16384** | **18** | **the app's readback buffer** | 64×64×4 exactly; caught holding the clear colour |

**`res=3` is the app's vertex buffer, float-for-float:**

```
res=3 decoded : [0.0, -0.5, 1.0, 0.0, 0.0,  0.5, 0.5, 1.0, 0.0, 0.0,  -0.5, 0.5, 1.0, 0.0, 0.0]
refapp VERTICES: [0.0, -0.5, 1.0, 0.0, 0.0,  0.5, 0.5, 1.0, 0.0, 0.0,  -0.5, 0.5, 1.0, 0.0, 0.0]
MATCH: True
```

Three vertices of `(vec2 position, vec3 color)` — the exact triangle geometry — crossing into a blob
we allocated and exported. **That is `vkMapMemory` on `HOST_VISIBLE` memory, seen from the host
side.** On a network, those 64 bytes have to be *shipped*, and nothing in the protocol says when.

**`res=6` is the pixel return path**, and it demonstrably carries the picture: 16384 bytes =
64 × 64 × 4 exactly, and the sampler caught it holding `ffff0000` repeated — little-endian bytes
`00 00 ff ff` = RGBA `(0, 0, 255, 255)` = **the blue clear colour**, i.e. the top rows of the
rendered image. This blob is how the pixels get back to the app, and it is exactly what (c)2's
coherence work must solve.

**A usable signal, recorded for (c)1/SP1:** `blob_id` discriminates cleanly. `blob_id == 0` for
Venus's *internal* shmems (ring, reply arena, staging pool); `blob_id != 0` (16, 18, 23) for
allocations corresponding to a client `VkDeviceMemory`. That is a real handle on telling "the app's
memory" from "the transport's plumbing".

### 6.1 The one lever, and why it is unproven ground

Vulkan does **not** actually require the app's writes to be magically visible. It requires it only
for memory the driver advertises as `HOST_COHERENT`. For memory *not* so advertised, **the
application is obliged to call `vkFlushMappedMemoryRanges`** — which is a real, interceptable API
call, at exactly the moment the data is ready.

So the lever is: **do not advertise `HOST_COHERENT`, and the app must tell us when to ship.**

**This is unproven ground and must not be treated as a plan.** Both existing backends nop their
flush hooks (§5.1), so there is no working example in the tree of a backend that relies on this.
Venus's own comment is candid about how much the current design assumes:

> *"We wrongly assume that mmap(dma_buf) and vkMapMemory(VkDeviceMemory) are equivalent when the
> blob type is VCMD_BLOB_TYPE_HOST3D. While we check for VCMD_PARAM_HOST_COHERENT_DMABUF_BLOB, we
> know vtest can lie."*

Whether real applications behave correctly against a non-coherent Venus, whether Mesa's Venus can
even be made to advertise non-coherent memory, and what it costs — all open. **[UNKNOWN]**, and
this is the spike (c)2 exists to run.

---

## 7. Finding 6 — the economics: the return path is the bulk, and latency is the enemy

For an identical, deterministic init-only workload:

| channel | bytes |
|---|---:|
| ring buffer (res=1, final tail) | **4024** |
| **reply arena** (res=2, high-water) | **48820** |
| vtest socket, inline command bytes | 140–236 |

**The reply arena is ~12× the command traffic.** The intuition that a command stream is a
one-directional firehose from C to S is **wrong**: the *return* path is the bulk.

That matters far more than it first appears, because **the replies are not a stream — they are
round-trips.** Every reply-bearing command makes the client thread spin on `head` until the answer
comes back (§5.4). On a LAN that is invisible. On a WAN, each one costs a full C→S→C RTT during
which the application is *stopped*.

> **This is the classic X11-over-network latency trap, arriving in a new costume.** X11's reputation
> for being slow over a network was never mostly about bandwidth; it was about applications making
> synchronous round-trips and blocking on each one. Venus has the same shape and the same
> mitigation available (the overwhelming majority of Venus commands are `vn_async_*`, which never
> wait). **Bandwidth is not the thing that will hurt Rayland. Latency is.** Any (c)1 design that
> optimizes bytes-on-the-wire while ignoring round-trip count is optimizing the wrong axis.

**A second, structural latency ceiling.** `head` must flow back S→C promptly, because it gates
three things on the client's critical path: **flow control** (C cannot write more than 128 KiB
ahead of the last `head` it has *seen*), **every seqno wait**, and **shmem retirement** (without
head updates, references leak and encoder pools grow). This makes the ring size a hard
**bandwidth-delay-product ceiling**: at 100 ms RTT, ≤128 KiB per RTT ≈ **1.3 MB/s** of command
stream. The host advances `head` after **every single command** (`vkr_ring.c:231-234`); naively
mirroring that is one network message per command, which would be catastrophic. Rayland must
coalesce head updates *while preserving the ≤128 KiB-in-flight invariant*. **[INFERENCE]** —
`buf_size` is a client-side constant (`vn_instance.c:149`) and the host caps rings at 16 MiB, so a
16 MiB ring would lift the ceiling to ~160 MB/s at the same RTT.

**An ordering constraint that is invisible in the source and will produce once-an-hour heisenbugs.**
Replies land in a *different* shmem (§3.2, §6), and the only signal that a reply is ready is `head`
reaching the submission's seqno. Therefore a transport **must ship the reply-shmem contents
*before* it ships the head update that releases the client's wait.** Ship them in the other order
and the client reads stale bytes — rarely, non-deterministically, and nowhere near the cause.
**[INFERENCE]**, and worth designing against explicitly.

---

## 8. What C0 did *not* establish (read this before designing (c)1)

Stated as open questions, because writing them down as facts is exactly how a toy result becomes a
production bug.

1. **Nothing about remoting.** C0's proof works *because* client and host share memory. It is a
   same-machine result. **The transport question is entirely open**, and everything in §5 is why.
2. **The out-of-line path (§5.3) has never been reached.** Trigger pinned (a submission > 8192 B),
   never triggered. Cheapest way to force it deliberately: a large SPIR-V module (> 8 KiB), a big
   `vkCmdUpdateBuffer`, or a large descriptor-set write. **Do this early in (c)1.**
3. **Ring wrap has never happened.** Peak `tail` was **9936 bytes of 131072 = 7.58%** for a full
   render (2.5× the init-only 4024, still nowhere near). The ring drains as fast as it fills. Wrap
   handling is **untested code**, in Mesa and in our decoder (which takes a *linear* slice and has
   no modulo arithmetic at all). Reaching a wrap needs a client that outruns the host — sustained
   multi-frame submission. It should be reached *deliberately*, not waited for.
4. **`ring_idx == 0` is confirmed for a single-threaded client only.** This is the weakest of C0's
   answers and must not be banked. Mesa has a **TLS ring** path (`vn_common.c:329`) that creates an
   *additional* ring per thread and plausibly a nonzero `ring_idx`; our fence path hardcodes ring 0.
   Note that path also sets `direct_order = 0`, so a TLS ring never goes indirect at all — a second
   reason its traffic would look different. **Test with threads before relying on the hardcode.**
5. **Host-side frame extraction is unanswered** (C0 Task 4c, deferred by the owner). C0's app does
   its own readback, which sidesteps the question — but **SP1/(c)1 must put a frame on a screen, and
   that requires host-side pixels.** The `DEVICE_LOCAL` colour image produced **no blob at all**, so
   the app's `VkImage` never enters our resource table; extraction will have to work through the
   ring's object graph, not the resource table. `blob_id != 0` (§6) is the most promising lead.
6. **Which 1 MiB allocation `res=2` corresponds to in Mesa's source is ambiguous.** There are three
   independent 1 MiB allocations (`reply_shmem_pool`, `ring->upload`, `cs_shmem_pool`) and size
   alone cannot distinguish them. **What it *does* is not ambiguous** — the live capture shows
   `vkSetReplyCommandStreamMESA` naming `resourceId = 2` and real replies landing in it, so it is
   the reply arena *functionally*. Settle the source attribution by logging call sites; do not
   assume. **[UNKNOWN]**
7. **`vn_renderer_bo`/`VkDeviceMemory` mapping and timeline-sync paths were never audited** for
   further shared-address-space assumptions. Host-visible memory mapping in particular is very
   likely to hide more. **This deserves its own spike before (c)2.**

---

## 9. Confidence summary

| # | Finding | Confidence | Basis |
|---|---|---|---|
| 1 | The socket carries 0% application commands; the ring carries 100% | **High** | 3 live captures + a 4th predating the spike; `vn_renderer_vtest.c:1055` |
| 2 | The ring is the identical command language | **High** | Two independent proofs: `vkr_ring.c:220-223` ≡ `vkr_context.c:170-173`; live decode with sizes from Mesa's headers |
| 3 | Layout: 192 + 131072 + 4 = 131268; head@0x00, tail@0x40, status@0x80, buffer@0xc0 | **High** | Declared in-band by `vkCreateRingMESA`; every observed change at the declared offset; layout struct compiled, not hand-derived |
| 4 | `status & 1` = **IDLE** (host parked) | **High** | `vn_protocol_renderer_defines.h:475` + driver-side copy, read directly on this host |
| 5 | No coherence seam exists (both backends nop) | **High** | `vn_renderer_virtgpu.c:1090-1105`; `vn_renderer.h:141-146` |
| 6 | No steady-state notification | **High** | `vn_ring.h:16-18` states it; `vn_ring.c:475-483` + `vkr_ring.c:263-327` implement it |
| 7 | Out-of-line path triggers > 8192 B/submission | **High** (mechanism) / **untriggered** (empirically) | `vn_ring.c:319`, single call site `vn_ring.c:522`; 0 occurrences across 6 blobs |
| 8 | The watchdog does not check ring progress | **High** (code) / **[INFERENCE]** (the 895 s consequence) | `vkr_context.c:536-539`, `vn_common.c:278-287` |
| 9 | `vkMapMemory` is the deeper problem | **High** | res=3 decodes as the exact vertex data; res=6 holds the clear colour |
| 10 | `vkFlushMappedMemoryRanges` is the lever | **[INFERENCE], unproven** | Vulkan's contract; but both backends nop their flush — no working example exists |
| 11 | Return path ~12× command traffic; latency dominates | **High** (the numbers) / **[INFERENCE]** (the WAN conclusion) | 48820 B vs 4024 B measured; `vn_ring.c:181-198` |
| 12 | `direct_order = 0` would keep traffic inline | **[INFERENCE], untested** | `vn_instance.c:152`, `vn_ring.c:319` |

---

## 10. The one-paragraph version, for whoever designs (c)1

Venus's application command stream **does not travel over the vtest socket** — it is written
straight into shared memory that our host allocates and the client `mmap`s via an fd we pass over
`SCM_RIGHTS`. **Neither the shared page nor the fd survives a network**, so (c)1 is not a transport
substitution; it is a protocol design task. The good news is decisive: **the ring's bytes are the
same legible Venus command language**, proven twice independently, so this is plumbing, not
reverse-engineering. The bad news is four-fold: **there is no seam** to hook (both backends nop
their flush; the ring is not even a `bo`, so Rayland would author the first non-coherent backend),
**no steady-state notification** to trigger on (the transport must poll `tail` itself), **the ring
is not self-contained** above 8 KiB per submission (it becomes pointers into other shmems — true
for every real app, and never once triggered by C0's triangle), and **the watchdog lies** (forward
it faithfully while the ring stalls and a 3.5 s abort becomes an 895 s hang). And underneath all of
it sits the problem no engine choice can dodge: **`vkMapMemory` has no API call to intercept** —
apps write vertices and textures straight into mapped memory, and the only lever is Vulkan's
non-`HOST_COHERENT` flush obligation, which is unproven ground. Finally, aim at the right axis: the
reply arena was **~12× the command traffic** and waited-on replies are **round-trips**. Bandwidth is
not what will hurt. Latency is.
