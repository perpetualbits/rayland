# The return path needs a completion protocol, not a better guess

**Status:** design note, written 2026-07-17 after (c)1 Task 9's stale-frame race defeated three
attempted fixes. It records *why* they failed and what shape the real fix has. The **instrumentation**
§7 asks for is now implemented and has confirmed the `T2 < T4` prediction — see §8. The **fix** (§6's
completion protocol) is not yet implemented.

The analysis in §2–§6 is **Roland's**, and it is the first account of this defect that explains all
the evidence rather than part of it. It is recorded here rather than left in a chat log because
every wrong turn it forecloses cost hours to find.

---

## 1. The defect, in one paragraph

An unmodified Vulkan application on C renders offscreen on S's GPU and reads its pixels back. Across
the relay, some frames arrive as **the previous frame, whole and intact**, and others arrive **torn**.
The application exits 0 and is never told. Measured on `rayland-icosa-cpu` (120 frames, frame N a
pure function of N): 3–39 frames wrong per run, wildly variable. Full evidence:
[`docs/c1-the-network.md`](../c1-the-network.md) §3.1.

**The readback path exists because the *application* asks for it** — `vkCmdCopyImageToBuffer` into its
own mapped buffer — not because Rayland chose to ship pixels. A Wayland application presenting on S's
screen never asks for those bytes and never meets this bug. This is the "applications map GPU-written
resources and read them back" case, which is a **distributed Vulkan memory model** problem, not a
window-remoting one.

## 2. It is a distributed synchronization problem, not a dirty-memory problem

There are four distinct events, and today's code conflates them:

1. the application submits GPU work;
2. S's GPU finishes the commands that write the buffer;
3. those writes become **coherent and readable** by S's CPU or transfer engine;
4. the resulting buffer version **reaches C and becomes visible** in C's memory.

A local Vulkan application gets a synchronization object whose meaning ends around **2 or 3**. On one
machine that is sufficient, because the app's memory *is* the memory the GPU wrote. Rayland must
extend that meaning through **4**, and nothing in Vulkan does that for us.

**The required invariant:**

> When C observes fence value N as signalled, every byte belonging to buffer version N must already
> be visible in C's buffer.

**Anything based on periodic comparison cannot establish that invariant.** That sentence is the
whole finding. (c)1's return path is a 200 µs poll that diffs S's memory against a shadow — it is a
*sampled property of a polling system*, and no amount of polling manufactures a happens-before edge.

## 3. Why CPU page-fault tracking is the wrong instrument

Recorded because it is an attractive idea and it does not work here:

- `mprotect`, `userfaultfd` write-protect, and soft-dirty bits act on **CPU page-table accesses**.
  GPU writes are DMA-like device accesses through GPU virtual memory and possibly an IOMMU; they do
  not walk the process's CPU page tables and do not trip read-only protection.
- GPU-side write tracking exists in places (access counters, migration dirty tracking) but is
  driver- and hardware-specific, not portable userspace, page-granular where we need bytes, and
  write-faulting every GPU page would serialize rendering.
- **Even a working GPU write trap answers the wrong question.** It says *"the GPU has started
  modifying this page"*; we need *"all GPU writes belonging to submission N are complete and
  visible"*.

(Page-fault tracking may still be right for (c)2's *separate* problem — detecting **C's own CPU
writes** into mapped memory. That is a different question, on a different side of the link.)

## 4. Why the virglrenderer fence experiment failed

Task 9 exposed `RenderEngine::wait_for_work_retired` and called it before diffing. It waits — 684
calls averaging **1.1 ms** in a 120-frame run — and changes nothing.

`virgl_renderer_context_create_fence(ctx, 0, ring_idx, id)` on a Venus context fences the **vkr
ring**: the CPU-side command ring. Its fences retire when the ring thread *reaches* them — after
decode and submission — **not** when the host GPU operation completes. So the barrier waits for the
ring to drain while the GPU's copy is still in flight.

`VirglEngine::read_back` uses the same primitive correctly only because it fences a resource created
by `create_resource` on C0's offscreen path, where **our own code did the submit**. For the
application's Venus queue work, that fence does not dominate the memory update being observed.

Other candidates in the same family, any of which may also hold:
the resource copy/download is scheduled separately; the fence belongs to the wrong context or ring;
the diffed buffer is a shadow updated after retirement; the GPU is done but the **cache-coherency
transition** has not happened. Linux deliberately separates GPU synchronization from cache coherency
— `DMA_BUF_IOCTL_SYNC` does not wait for GPU work; a client must wait on the fence separately.

**So the experiment does not prove fences are useless. It proves that fence does not dominate this
write.**

## 5. Why "wait until nothing remains to send" was *worse*

Measured: release immediately → 25/13/26 frames wrong; hold one poll → 4/1/1; wait for an empty diff
→ 16/8. Non-monotonic in delay, which looked paradoxical and is not:

- **False quiescence.** The poller checks *before* the GPU-visible update lands, sees no differences,
  declares empty, and releases. Waiting for an empty queue can therefore release *earlier* than
  waiting one poll after a detected change.
- **Incoherent snapshots.** If the source changes while the diff scans it, a pass can mix old and new
  cache lines. "Empty" then means only that the scanner agrees with its own previous snapshot.
- **Scheduling phase changes.** Added waits move the poller, renderer, network and application
  threads into different interleavings. A race probability is non-monotonic under added delay because
  delay changes interleaving rather than extending a deadline.
- **Version confusion.** With no buffer generations, frames N and N+1 coalesce into one stream;
  "nothing left" never establishes *which submission* the bytes belong to.
- **Unobserved writes.** A diff detects final byte *inequality*, not writes. A value written and
  restored between samples vanishes from the detector entirely.

The extra 200 µs reduced the probability of the bad interleaving. **It could not create the missing
happens-before**, which is why it plateaued at "wrong 2% of the time" instead of zero — and why it is
not in the tree.

## 6. The shape of the real fix

**Immutable buffer versions plus timeline fences.** C's application-visible fence must be a
**synthetic Rayland fence**, which must *not* signal merely because S's GPU fence retired:

```text
rayland_fence[N] = gpu_complete_on_S[N]
               AND bytes_transmitted[N]
               AND bytes_installed_on_C[N]
```

Per GPU-visible write epoch, carry: `buffer_id`, `generation`, `producer_timeline`,
`producer_fence_value`, and damage regions (or a full-buffer marker).

**On S:** associate the write-producing submission with an exact host GPU timeline value; do not
inspect or transfer the buffer before that value retires; perform the driver-required resource
synchronization/readback; snapshot generation N; send it with chunk numbers; send
`GENERATION_COMMITTED(N)` only after every chunk is emitted.

**On C:** receive into storage that is **not yet application-visible**; verify every chunk for
generation N arrived; atomically publish generation N; **only then** advance the application-visible
fence.

That establishes what polling cannot:

```text
publish(C, buffer, N) happens-before signal(C, app_fence, N) happens-before app_read(C, buffer, N)
```

**Venus fence feedback is the right semantic seam.** The network did not invalidate it; it changed
its implementation from a shared status word into a message:

```text
FENCE_COMPLETE(context_id, timeline_id, fence_value, buffer_versions[])
```

Spec §6's crutch table disables it (`VN_PERF=no_fence_feedback`) because a network cannot carry a
shared page, and calls buying it back "the first thing (c)2 should buy back". This note is the
argument for why that is not an optimisation but **the correctness fix**.

## 7. The next concrete step: instrument the stages separately

Do not attempt another fix until the ordering graph is known. Timestamp:

```text
T0 guest/API submission accepted      T5 first changed byte observed in mapped host memory
T1 host Vulkan command submitted      T6 transfer packet emitted
T2 host Vulkan fence/timeline signal  T7 packet installed on C
T3 resource sync/readback initiated   T8 application-visible fence signalled
T4 resource sync/readback completed   T9 application CPU read begins
```

The bug is wherever the graph permits `T9 < T7`, or `T2 < T4` while the implementation assumes
`T2 >= T4`. Today's code assumes the latter and Task 9's evidence says it is false.

## 8. That instrumentation was done, and it is `T2 < T4` (2026-07-17)

§7 has been carried out. `rayland_relay::trace` (env-gated by `RAYLAND_C1_TRACE`) stamps T0/T2/T5/T6
on S and T7/T8 on C against the shared `CLOCK_MONOTONIC`, and **Probe A** re-fingerprints each
readback blob after S ships it, catching the GPU still writing a frame already declared complete. The
full evidence and method are in [`../c1-the-network.md`](../c1-the-network.md) §3.1, *The mechanism,
established*. In one sentence, across three runs:

> **`T2 < T4` is real and large.** S's GPU keeps writing a readback blob for **0.33 ms to ~20 ms
> after** the return path shipped it — every observed late write beyond both the 200 µs poll and the
> 1.1 ms fence barrier, which is why neither §4's nor §5's experiment reached zero. And the C side is
> a faithful courier: no `T7` ever preceded its `T6`, so `T9 < T7` is **not** in play. The violation
> is entirely S shipping bytes the GPU has not finished.

That closes the "do not fix until the ordering graph is known" gate this section opened. The graph is
known, it is the one §2–§6 predicted, and the next step is the completion protocol of §6 — not another
guess at the return path's timing.
