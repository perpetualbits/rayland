# (c)1 — The Network (what it costs, and what broke)

(c)1 is the moment Rayland's command stream crossed a **real network** between two **real machines**,
and the moment we stopped guessing what that costs.

C0 proved the stream replays faithfully on one machine over shared memory, and was explicit that it
proved **nothing about remoting**: on one machine the memfd *is* the memory, so nothing is
transported and nothing can be lost. (c)1 transports it. This document is the measurement, and the
measurement's job is to be the difference between evidence and a demo.

> **The short version, if you read nothing else.**
>
> - An **unmodified** Vulkan application on a machine with **no GPU in use** renders on another
>   machine's GPU across a real network, and the pixels are **bit-identical** to running the same
>   binary natively on that machine. That works.
> - **The commands are free. The memory is not.** Fixture A ships **1,706 bytes of commands per
>   frame** and **5.21 MiB of mapped memory per frame** — a ratio of about **3,200×**. Rayland's
>   founding intuition ("language is cheaper than pixels") is confirmed, and it is not the thing that
>   hurts.
> - **The relay is neither bandwidth-bound nor RTT-bound. It is message-rate-bound**, and that was
>   predicted by nobody. Fixture A takes **94 s against 13.4 s native at *zero* added latency**.
> - **The relay silently delivers stale frames.** Fixture A gets **frame N−1** instead of frame N,
>   3 times in 120 at 0 ms and 5 times in 120 at 20 ms. Every occurrence is stale by *exactly one*.
>   The application exits 0. Nothing warns anybody. **This is a correctness bug in (c)1, and it is
>   the most important thing in this document.**

---

## 1. What was actually run

Two machines, and the vocabulary is X11's, not the cloud's (see `CLAUDE.md`):

- **C = apollo** — where the **application** runs. x86_64, Ubuntu 26.04. **Its AMD GPU is unused.**
  No Rust toolchain, no GPU stack: it needs only stock Mesa's Venus ICD and two copied binaries.
- **S = dop561** — where the **GPU and the user** are. Intel Raptor Lake-P Iris Xe (`8086:a7a0`),
  Mesa 26.0.3, `/dev/dri/renderD128`. Also the build host.

The application runs on C under an **unmodified** Mesa Venus ICD, which talks to `rayland-c`'s local
vtest server. `rayland-c` relays over **QUIC** to `rayland-s`, which drives a real
`libvirglrenderer` on S's GPU. The application is not aware of any of this and has zero `rayland-*`
dependencies.

### 1.1 The link — read this before believing any number here

**dop561 has two addresses and they are not equivalent.** This cost a day's worth of wrong belief
and is worth stating plainly:

| path | link | RTT avg | RTT max | jitter (mdev) |
|---|---:|---:|---:|---:|
| `192.168.1.192` | dop561's **WiFi** | 11.8 ms | 91.2 ms | 26.5 ms |
| `192.168.1.150` | dop561's `br0`, wired | **0.65 ms** | 0.97 ms | **0.18 ms** |

apollo's `enp1s0` is wired at 1000 Mb/s and routes to both.

**Task 8 used the WiFi address**, and its result — 7/7 bit-identical runs — was described in prose,
by more than one author, as "across real Ethernet". **That was false.** Nothing in the repository
ever claimed it; `scripts/c1-two-machine.sh` simply defaults `S_IP` to the WiFi address without
characterising it. The correction makes the result **stronger**, not weaker: an unmodified Vulkan
application rendered bit-identically across a link with **26 ms of jitter and a 91 ms worst case**,
7 times out of 7. The relay did not care.

**But it would have silently ruined this sweep.** A netem "+20 ms" cell on a link whose own jitter is
±26 ms measures the access point, not the protocol. The sweep therefore runs on the **wired** path,
where netem is the only variable. This is the class of error that is invisible afterwards, because
the numbers come out plausible.

### 1.2 The workloads, and why there are three

| | `rayland-refapp` | `rayland-icosa-cpu` (A) | `rayland-icosa-gpu` (B) |
|---|---|---|---|
| what it draws | one static triangle | spinning icosahedron, Mandelbrot texture | same scene, fractal in a fragment shader |
| frames | 1 | 120 | 120 |
| mapped writes after startup | **none** | **1 MiB every frame** | 80 B every frame |
| per-frame CPU | none | ~49 ms of Mandelbrot | ~0 |

`refapp` is the trivial baseline: it says whether a failure is workload-specific or general. It is
also **structurally silent** on both questions this document asks, because it never touches mapped
memory again after startup and never sends a meaningful byte.

The two icosahedron fixtures exist to make `vkMapMemory` bite: they write through a persistently
mapped `HOST_COHERENT` buffer **with no flush**, and therefore with **no API call anywhere between
the write and the copy that reads it** — nothing to intercept. See
[`docs/icosa-fixtures.md`](icosa-fixtures.md). Fixture B is the **volume control**, not an
alternative: same geometry, same schedule, same arithmetic, same render loop, differing only in
where the fractal is computed.

### 1.3 The comparison rule (do not get this backwards)

Every remoted run is compared against the same binary run **natively on S** — never on C. C's GPU is
an AMD part; comparing against it would compare **rasterisers** instead of **transports**. The whole
claim is that only the transport changed.

Baselines were generated **in-run**, on S, in the same session as the cells they judge: a baseline
from another day is a baseline from another driver state.

**They were then cross-checked against an independently produced set.** The icosa sub-project
published per-frame SHA256s (`docs/icosa-baseline-dop561.txt`), generated 20 minutes earlier in a
separate run. Every hash matched, both fixtures, all 120 frames each:

| | independent baseline | this sweep's baseline |
|---|---|---|
| fixture A, `frame_0000` | `bd793229a3b3ca4f…` | `bd793229a3b3ca4f…` |
| fixture A, run rollup | `f49a9314b908c5db…` | `f49a9314b908c5db…` |
| fixture B, run rollup | `86f0d51641b2011b…` | `86f0d51641b2011b…` |

**This is load-bearing.** It is why every finding below is a statement about the relay rather than a
statement about the workload. The fixtures are deterministic and bit-stable run-to-run (120/120
across two runs, both fixtures), so any divergence across the relay is provably the relay's.

---

## 2. The measurement

`scripts/c1-sweep.sh` runs each workload at four round-trip times and records, per cell: bytes each
way **split by channel**, round trips and the time actually spent blocked in them, time to first
frame, wall-clock, and whether the pixels are still bit-identical.

The instrument is `crates/rayland-c/src/metrics.rs`, behind `RAYLAND_C1_METRICS=1`. Classification
happens at exactly one seam per direction (`link.rs`), which the daemon's own design guarantees
every message crosses — the reader thread owns `recv` exclusively and the send half sits behind one
mutex. That exclusivity is what makes these **totals** rather than samples.

**Latency is added with `tc netem` on C's egress**, filtered by a `prio` band to **UDP toward S
only**. An unfiltered qdisc would also delay the harness's own ssh — apollo routes both over
`enp1s0` — which would slow orchestration and make a working machine look broken. The delay is
one-way, so it adds N ms to the RTT, not 2N. netem's packet counter is asserted non-zero per cell,
because a typo'd filter installs perfectly and delays nothing, and the cell would then report
"100 ms RTT" over an undelayed link.

**`VN_DEBUG=no_abort` is deliberately absent.** Mesa aborts the application ~3.5 s after a ring
stalls, and that abort is the stall detector. A hang here is a finding, not a nuisance.

### 2.1 What is deliberately not counted

**Doorbells.** `C2S::NotifyRing`'s own documentation forbids a metric on it, and it is right:
ring-findings §5.2 measured **1 notification in one run and 4 in another for byte-identical ring
traffic**, because Mesa rings the doorbell only when it observes the consumer's IDLE bit and a 1 ms
throttle has elapsed. A doorbell count measures **scheduling**, not work. They are counted as bytes
— they are real bytes — and never as an event count.

---

## 3. Results

Bit-identity is against native-on-S. `status=ok` means all frames matched.

| workload | +RTT | status | wall (s) | first frame (ms) | round trips | C→S total | S→C total |
|---|---:|---|---:|---:|---:|---:|---:|
| refapp | 0 | **ok** | 3.47 | 18 | 6 | 60,431 | 40,322 |
| refapp | 20 | **ok** | 4.08 | 101 | 6 | 27,409 | 40,330 |
| refapp | 50 | **ok** | 5.16 | 216 | 6 | 27,401 | 40,330 |
| refapp | 100 | **ok** | 6.78 | 417 | 6 | 27,401 | 40,354 |
| fixture A | 0 | **DIFFERS** | 93.81 | 18 | 8 | **656,347,158** | 18,402,906 |
| fixture A | 20 | **DIFFERS** | 134.65 | 100 | 8 | 661,862,644 | 18,153,877 |
| fixture A | 50 | **DIFFERS** | 164.52 | 213 | 8 | 663,175,291 | 16,238,893 |
| fixture A | 100 | **DIFFERS** | 199.98 | 417 | 8 | 664,488,187 | 13,431,896 |
| fixture B | 0 | `app_exit_134` | 8.74 | 18 | 3 | 3,174 | 18,178 |
| fixture B | 20 | `app_exit_134` | 9.06 | 97 | 3 | 4,272 | 19,298 |
| fixture B | 50 | `app_exit_134` | 9.63 | 218 | 3 | 4,272 | 19,306 |
| fixture B | 100 | `app_exit_134` | 10.56 | 414 | 3 | 4,272 | 19,290 |

Every cell of the matrix ran; none is missing or estimated. `netem`'s packet counter was non-zero in
every cell with added delay, so each RTT label is one the traffic actually experienced.

The full per-channel figures are committed at
[`docs/data/c1-sweep-2026-07-17.csv`](data/c1-sweep-2026-07-17.csv), exactly as the harness produced
them. The tables in this document are readings of that file and nothing else; if a number here
disagrees with it, the file is right.

**The fixture commit is pinned.** These cells measured the icosa fixtures as of the merge at
`1d6a717` (fixtures at `d12fc36`). `main` moved during the sweep and was deliberately **not** merged
mid-run: a fixture that changes between cells would still build, still run, and still print numbers,
while silently making the columns incomparable.

### 3.1 🔴 The relay silently delivers stale frames

**Fixture A renders 117 of 120 frames correctly and gets 3 wrong. Every wrong frame is
byte-for-byte the *previous* native frame.**

| +RTT | frames wrong | which | stale by exactly one? |
|---:|---:|---|---|
| 0 ms | **3** | 13, 77, 100 | **3 of 3** |
| 20 ms | **5** | 8, 48, 75, 106, 115 | **5 of 5** |

> ⚠️ **This section originally said "eight out of eight are `N−1`; never torn, never novel pixels",
> and drew structural conclusions from it. That was wrong**, and it is corrected below rather than
> quietly rewritten. It was inferred from a sample of **eight** frames across two runs. A later run
> of the loopback reproducer produced **38** bad frames, and **16 of them (42%) match no native frame
> at all** — i.e. **tearing is real and common**. The conclusion "it is not tearing" was a confident
> generalisation from a small sample: true of those eight, false about the system.

**There are two distinct failure modes**, measured on one loopback run of 120 frames (38 bad):

| mode | count | what the application got |
|---|---:|---|
| **stale by exactly one** — byte-for-byte the previous native frame | **22** | it read before *any* of its pixels had arrived |
| **torn** — matches no native frame that ever existed | **16** | it read while its pixels were *partially* applied |

The torn frames arrive in runs (0069–0070, 0073–0076, 0101–0105), which is what falling behind and
staying behind looks like.

What the full set does still establish:

- **It is not an off-by-one in the code.** The affected frames are **different in every run**. Code
  is not sporadic. This is a **race**.
- **Both modes share one cause.** If nothing couples *"your pixels have arrived"* to *"you may
  read"*, then reading early yields the previous frame **intact**, and reading mid-delivery yields a
  **torn** mix. One missing dependency, two costumes. The existence of tearing is what rules out a
  pure mis-ordering, which could only ever produce a clean `N−1`.
- **The rate varies enormously run to run** — 3, 5, 8, 20, 29, 36, 38, 39 of 120 have all been
  observed, on the same code and the same link. Any single run is a sample, not a measurement. This
  document said "3 of 120" as though it characterised the defect; it characterised one run.

**Why this matters more than any timing number in this document:** the application **exits 0**. It
is not told. With an ordinary application, frame N−1 *is a plausible frame of animation* — nobody
watching would ever see it. It took a fixture where **frame N is a pure function of N**, with no
wall-clock anywhere in its math, to turn "looks fine" into "here are the three, here is the proof".

That property was designed into `rayland-icosa-core` for a different reason, and it is the only
reason this bug is visible.

#### An attempted fix, and why it failed (read before trying the obvious thing)

**The obvious repair has been tried and does not work. It is left in the tree, wired up and
documented, so the next attempt starts after it rather than at it.**

The reasoning was: `poll_progress` infers "S's GPU wrote" from a `memcmp`, and a `memcmp` answers
*"did these bytes change?"*, never *"has the GPU finished?"*. `rayland-engine` has always been able
to answer the real question — `VirglEngine::wait_for_context_fence`, which `read_back` has used since
C0 Task 3 — so it was exposed on the trait as `RenderEngine::wait_for_work_retired` and called by the
progress loop *between* reading the ring frontier and diffing the blobs (with the applier lock
dropped across it, since a 5 s fence timeout would otherwise starve the ring and trip Mesa's 3.5 s
abort). A second change ordered the application's blobs ahead of Venus's internal ones, so the reply
arena — whose bytes release the application's wait — cannot cross the wire before the pixels.

**Measured, three runs: 25, 13, 26 of 120 frames wrong.** The unfixed range is 3–39. It is not an
improvement; it is indistinguishable from doing nothing.

**And the barrier is not idle while failing**, which is the useful part:

```
C1BARRIER calls=684 total_us=740621 mean_us=1082
```

684 fence waits per run, averaging **1.1 ms of real waiting**. It waits for something. That something
is **not** "the application's readback copy has landed in the blob".

**The conclusion, and the next investigator's starting point: a virglrenderer context fence does not
order against the work Venus's own ring thread dispatches.** `read_back` gets a correct frame from
the identical primitive because it fences resources created by `create_resource` — C0's offscreen
path — never the application's Venus queue. So the engine answers a real question, truthfully, and it
is *still the wrong question*.

That is the fourth time in this document's history that an instrument has answered a different
question than the one being asked, and the first time it happened to a fix rather than a measurement.

#### The mechanism is NOT established

Stated plainly because a plausible mechanism is worse than none:

- ❌ **Refuted: "the return path is polled, not fenced — `RingProgress` releases the application's
  wait before the pixels arrive."** This was drafted as the finding and it is **wrong**.
  `Applier::poll_progress` (`crates/rayland-s/src/apply.rs`) pushes `BlobData` **first** and appends
  `RingProgress` **last, always**, with a comment stating exactly why: *"this is what releases the
  application's wait, and it must not do so before the bytes it is waiting for are on their way."*
  The ordering is deliberate and correct.
- ❓ **Suspected, unproven: S has no GPU-completion signal.** `take_bytes_s_wrote()` discovers S's
  writes by **comparing bytes in memory** — sampling a buffer the GPU may still be writing into.
  Ring retirement means commands were **dispatched**, not that the GPU **finished** the copy. If the
  copy lands after the diff samples it, those bytes ship on the *next* poll, arriving after the
  application has already read. This is consistent with every observation above, including
  "worse with latency" and "never by two". **It is not proved.**

If that hypothesis holds, the defect is that **v1 infers "S wrote" from memory contents rather than
from the GPU saying so**, and a diff-based return path is structurally racy against an engine with
no completion signal. That is a deeper problem than a mis-ordering and it is **(c)1's, not (c)2's**.

### 3.2 The commands are free. The memory is not.

Fixture A, per frame:

| channel | per frame | over 120 frames |
|---|---:|---:|
| **ring** (the application's actual Vulkan commands) | **1,706 B** | 204,724 B |
| **blob sync** (its mapped memory) | **5.21 MiB** | **656,125,883 B** |

**A ratio of about 3,200×.**

### 3.2.1 The megabyte is shipped **5.2 times per frame**

The icosa sub-project registered a prediction *before* this sweep ran
(`docs/icosa-fixtures.md` §11): fixture A's C→S blob sync should be **~120 MiB** over the run if the
whole buffer ships every frame, or **~6.7 MiB** if only the 5.6% of bytes that actually change do.

**Measured: 626 MiB — 5.2× more than the "nothing elides" worst case.** Per frame:

| +RTT | blob sync / frame | **k** = whole 1 MiB buffers shipped per frame | wall (s) |
|---:|---:|---:|---:|
| 0 | 5,467,716 B | **5.214** | 93.8 |
| 20 | 5,513,678 B | 5.258 | 134.7 |
| 50 | 5,524,618 B | 5.269 | 164.5 |
| 100 | 5,535,558 B | **5.279** | 200.0 |

**k is structural, not a timing artefact**, and this was settled by a sub-prediction registered in
advance: *if k came from the ring watcher happening to fire mid-write, it should rise with
wall-clock.* **k moves +1.2% while wall-clock moves +113%.** It is flat. Whatever ships the buffer
5.2 times does so for a structural reason, not because it caught the application mid-write.

This is consistent with the message counts: **~16.4 blob-sync messages per frame** at a mean of
332 KB — roughly five whole-texture ships plus a tail of tiny ones (the 80-byte uniform blob, also
shipped repeatedly).

So the answer to the question the fixtures were built to ask — *does (c)1's blob sync actually ship
the megabyte every frame?* — is: **it ships it 5.2 times every frame**, for a buffer that changed by
5.6%. Neither the "ships it once" nor the "diffs it down" model was right, and the sweep can say so
because the prediction was on record before the run.

**Note what this is not.** It is not the C→S diff failing: there **is** no C→S diff. Spec §7 rules
out dirty tracking on C's side deliberately (Venus gives no API-level signal for which bytes
changed), so C ships whole blobs by design. The finding is not that the shipping is undiffed — that
was known — but that it happens **five times over**.

This is the founding intuition of the whole project, confirmed and then immediately qualified.
"Language is cheaper than pixels" is **true**, emphatically: 1.7 KB per frame carries everything the
application asked the GPU to do. C0 measured ~4 KiB of ring traffic for a complete Vulkan
initialisation; that scale holds.

**And it does not matter,** because the commands refer to memory, and the memory is 3,200× larger
than the commands. No cleverness in command encoding touches this. The application wrote those bytes
by hand, through a persistent mapping, with **no API call to intercept** — which is (c)2's problem,
now with a number instead of an argument.

### 3.3 The relay is message-rate-bound — which nobody predicted

**Fixture A takes 94 seconds at *zero* added latency, against 13.4 s natively. A 7× slowdown with no
network delay at all.**

The link is not the problem:

- 626 MiB at 1 GbE is **~5 s** of wire time.
- The fractal is **5.9 s** of CPU.
- The wall-clock is **94 s**.

Fixture A's wall-clock across the whole range — **94 → 135 → 165 → 200 s** at 0/20/50/100 ms — rises
by roughly **1.06 s per added millisecond of RTT**. Over 120 frames that implies the workload pays
the RTT about **8–9 times per frame**.

> **And the `round_trips` column says 8 — for the entire run.** Both numbers are correct, and the
> gap between them is a **limitation of the instrument, stated here rather than left for a reader to
> trip over.** `metrics.rs` counts a round trip only where a thread blocks in `ChannelLink::recv` —
> the capset and blob creation, which happen at startup. The per-frame stalls are **not** those:
> they are the application waiting for its fence, served by `RingProgress` arriving from S, and no
> thread in `rayland-c` blocks on them in a way this instrument can see. So **the sweep can measure
> that ~8 RTTs per frame are being paid, but cannot yet point at where.** Finding out is the first
> job of whoever picks up §6.2.

But the **94 s floor at zero RTT** is the headline: with the network removed entirely, the relay is
still 7× slower than native. The RTT scaling is a second, separate problem stacked on top of it.

The missing ~80 seconds is **per-message overhead**:

- **C→S: 1,973 blob-sync messages for 120 frames** — about **16 whole-blob syncs per frame**, mean
  332 KB each. (c)1's conservative blob sync does not ship the megabyte once per frame; it ships
  large blobs sixteen times per frame. That is the **5.2× overshoot** against "resend everything
  every frame", which was supposed to be the worst case.
- **S→C: 1,553,266 messages carrying 18,396,291 B — a mean of 11.8 bytes per message.** The framing
  overhead exceeds the payload.

That second figure is the **return-path fragmentation** an earlier (c)1 session predicted and never
got to test. It is now measured. Note carefully what it is **not**: the byte-granular diff **works**
— 18.4 MB moved instead of the ~30 MB a whole-readback-per-frame would cost. It found the right
bytes. A zooming fractal's boundary produces thousands of small scattered runs, and each run becomes
its own message with its own length prefix, envelope, and write. **The diff succeeded and the
transport made the success cost more than the failure would have.**

### 3.4 Fixture B is out of scope — by design, and it says so

`app_exit_134` is SIGABRT. It is **not a bug**, and it is not a ring stall (which is what this
document first claimed, wrongly).

`rayland-c` **refuses to relay**, deliberately, and explains itself in full:

> *refusing to relay the ring delta ending at tail 12440: the command stream carries a dword equal to
> 180 … which is `vkExecuteCommandStreamsMESA` — Venus's out-of-line command path, which (c)1 v1 does
> not relay. Mesa produces this when a single submission exceeds `direct_size` (`buffer_size >> 4` =
> 8192 bytes for the 128 KiB instance ring), and the real commands then live in *other* shmems that
> this version never ships; S would execute whatever its copy of those blobs happens to contain.*

The daemon closes the link rather than relay a stream that would misbehave on S's GPU **with no
trace of the cause**. Mesa then aborts because its vtest server vanished. **The abort is downstream
of (c)1 correctly declining work it cannot do** — a guard firing as designed, leaving a precise
explanation for whoever finds it.

**Why B and not A** is the interesting part: B evaluates the fractal in a **fragment shader**, so its
SPIR-V is large, and a single submission carrying it exceeds `direct_size`. **The fixture that pushes
*fewer* bytes through mapped memory is the one that needs a command path (c)1 does not have.** The
volume control found a scope limit rather than a volume effect.

The lever, when the time comes, is named in the refusal: Mesa's client-side `direct_order` constant
(`vn_instance.c:152`); `0` makes `direct_size == buffer_size`.

### 3.5 Startup is RTT-bound and one-off — as predicted

Time from the application connecting to its first frame:

| +RTT | 0 ms | 20 ms | 50 ms | 100 ms |
|---|---:|---:|---:|---:|
| **first frame** | **18 ms** | **101 ms** | **216 ms** | **417 ms** |

Linear in RTT, ≈ 4 round trips, and **one-off** — refapp's total wall-clock rises only 3.47 → 6.78 s
across the same range. This is spec §8.1's second prediction, and it holds cleanly.

---

## 4. Scoring the spec's predictions

§8.1 recorded its expectations in advance, *"which is the difference between measuring and
rationalizing"*, and asked that a failed prediction be reported loudly. Accordingly:

| §8.1 predicted | verdict |
|---|---|
| **Bandwidth should be small; "language is cheaper than pixels"** | ✅ **Confirmed, emphatically.** 1,706 B/frame of commands. |
| **Startup is RTT-bound but one-off** | ✅ **Confirmed.** 18 → 417 ms, linear, ~4 RTTs, one-off. |
| **Steady state should be bandwidth-bound, not RTT-bound** | ❌ **Refuted, and not in the direction anyone feared.** It is *neither*. Fixture A is **message-rate-bound**: 7× slower than native at 0 ms RTT, with only ~5 s of wire time in a 94 s run. |
| **The return path is ~12× the command path** | ⚠️ **Confirmed and understated — and measuring the wrong thing.** S→C blob data (18.4 MB) is **~90×** the ring (204 KB). But **C→S blob sync (626 MiB) dwarfs both** — a third path C0 could not see, because refapp never writes mapped memory after startup. *"The return path is where the bytes are"* was true of C0's workload and false about the world. |

The last row is the one worth sitting with. The ratio was not wrong; it was **an honest measurement
of a workload that could not exhibit the phenomenon that dominates everything**. It took a fixture
built specifically to write mapped memory to make the real cost visible at all.

---

## 5. What (c)1 did NOT prove

Per spec §13, and stated plainly because the temptation to over-read a working demo is the whole
reason this section exists:

- **Not arbitrary applications.** Three workloads, two of them written by us for this purpose. One of
  the three (fixture B) is **outside v1's scope entirely** — the out-of-line command path is not
  implemented.
- **Not a real Wayland application.** Everything here renders **offscreen** and reads back. There is
  no swapchain, no compositor on C, no `wl_surface`. On-screen presentation exists
  (`rayland-present`, Task 7) but is not in this measurement, and it has a known limitation: it
  draws **one static frame per call**, so an animated on-screen source is a shape it does not
  currently support.
- **Not that performance is acceptable.** It is not. A 7× slowdown at zero latency is not a viable
  product, and this document's central timing finding is a **defect report**, not a benchmark.
- **Not correctness.** (c)1 **delivers wrong frames**, silently, on the one workload built to check.
  This is the single most important sentence in this document.
- **Not the WiFi path at scale.** Task 8's 7/7 across a 26 ms-jitter link is real and encouraging;
  this sweep deliberately used the wired path to make netem the only variable.

---

## 6. What this hands to (c)2 and beyond

1. **Fix the stale-frame race first.** Nothing else matters while the relay can deliver frame N−1
   without telling anybody. Establish the mechanism before fixing it — the first plausible
   explanation was wrong (§3.1).
2. **The message rate is the cost, not the byte count.** 1.55 million ~12-byte messages and 16
   whole-blob syncs per frame. Coalescing runs and batching syncs is where the 80 seconds are.
   Compression and cleverer diffs are not: **the diff already works.**
3. **The mapped-memory problem is now quantified, not argued:** 3,200× more mapped bytes than
   command bytes, written with no API call to intercept. That is (c)2's whole brief.
4. **Vulkan's own sync validation cannot see these writes either** — structurally, because there is
   no API call to hook (`docs/icosa-fixtures.md` §6). If Khronos's layer, which sees every API call
   an application makes, is blind to them, so is any relay built on watching API calls. That is
   (c)2's problem stated by the ecosystem's own tooling rather than by us.

---

## 7. How to reproduce

```bash
# Full sweep: 3 workloads × 4 RTTs. Requires apollo reachable and passwordless sudo there for tc.
scripts/c1-sweep.sh

# A subset, for a smoke test:
RTTS="0 100" WORKLOADS=refapp scripts/c1-sweep.sh

# The uncontrolled WiFi row (Task 8's link):
S_IP=192.168.1.192 RTTS=0 scripts/c1-sweep.sh
```

Results land in `/tmp/c1-sweep/sweep.csv`. The harness always removes its netem qdisc, on success,
on failure, and on Ctrl-C: a qdisc left behind would silently delay every future run on that machine
and would look like a network fault rather than our residue.

### 7.1 Traps that cost real time here

- **The fixtures' output directory must already exist.** They exit 1 with `No such file or directory
  (os error 2)` otherwise — which once made 120 frames appear to take 0.48 s, a failed run wearing a
  measurement's clothes.
- **`pgrep -f /tmp/rayland-c` matches the argv of the shell running the `pgrep`.** It reports "still
  running" forever. Use `pgrep -x`. A detector that always fires is not a detector.
- **The application exiting does not mean the session is over.** `rayland-c` is still relaying when
  the app's process is gone. Reading its log at app-exit samples a session in progress; the faster
  the cell, the more traffic is missed. This produced a **6× spread** on identical refapp runs that
  looked like a property of Venus. The daemon now prints `final=1` at clean session end and the
  harness waits for it.
- **The ssh key must be named explicitly** (`-i … -o IdentitiesOnly=yes`). The gnome-keyring agent
  lists locked keys via `ssh-add -l` and then cannot sign with them, burning `MaxAuthTries` before
  ssh reaches the right key. It presents as "the key is rejected" and recurs after every reboot or
  screen-lock.

---

## 8. A note on how these numbers were arrived at

Four separate findings in this document were, at some point, **wrong in a draft of this document**:

1. A "3.03 s time to first frame" that was the harness's own `sleep 3`.
2. A "6× run-to-run variance in Venus's C→S volume" that was the harness reading a log mid-session.
3. A "readback ordering race" that the code explicitly prevents, with a comment explaining why.
4. A "Mesa ring-stall abort" that was (c)1 deliberately refusing an out-of-scope command path.

None was a lie and none was carelessness; each was an instrument answering a **different question
than the one being reported**. That is the same shape as the WiFi/Ethernet error (§1.1), as the icosa
sub-project's own 18× error about how much of the fractal changes per frame, and as
`ssh-add -l` listing a key it cannot sign with.

The mechanisms that caught them are worth more than the findings: **an independently generated
baseline** (§1.3), **a prediction written down before the run** — which caught an 18× error in the
predictor's own model *before* it could contaminate anything — and, twice, **a rate limit that
forced a ten-minute pause between believing something and publishing it.**

The instrument is rarely broken. It is usually answering a different question.
