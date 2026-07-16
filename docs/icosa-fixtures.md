# The icosahedron fixtures — what they found

This is the document (c)2 (mapped-memory coherence) reads first. It records what actually
happened when `rayland-icosa-cpu` and `rayland-icosa-gpu` — the two fixtures built to make
`vkMapMemory` bite (design spec:
[`design/2026-07-16-icosa-fixtures.md`](design/2026-07-16-icosa-fixtures.md)) — were run
through Venus, and it is more useful for what it corrects than for what it confirms.

> **Read this before you believe too much of it.** The spec that commissioned these
> fixtures predicted they would fail against Rayland. They did not — both passed, 120 of
> 120 frames bit-identical, for both fixtures. That is not good news about (c)2 being
> solved. It is a finding about *where* the hard problem lives, and the rest of this
> document explains why the prediction was wrong and what it would take to actually see
> the failure it was looking for.

---

## 1. What the fixtures are, briefly

Full detail — geometry, the frame schedule, the fractal math, why a depth attachment is
carried, why two crates of shared scaffolding exist — is in the design spec. The one
paragraph needed to follow the rest of this document:

Both fixtures draw the same spinning, textured icosahedron for 120 frames, using the same
shared geometry and animation code (`rayland-icosa-core`) and the same shared Vulkan
render loop (`rayland-icosa-vk`), so that the two differ in exactly one respect. **Fixture
A** (`rayland-icosa-cpu`) computes a 512×512 Mandelbrot texture on **C's CPU** every frame
and writes the whole megabyte into persistently-mapped `HOST_COHERENT` memory, with no
`vkFlushMappedMemoryRanges` call anywhere — because an ordinary Vulkan program does not
make that call, and the whole point of the fixture is to behave like one. **Fixture B**
(`rayland-icosa-gpu`) evaluates the identical fractal in a fragment shader instead, so only
80 bytes of uniform data cross mapped memory each frame rather than a megabyte. Neither
fixture knows Rayland exists; both are ordinary, unmodified Vulkan programs, per the
inherited rule in the design spec's §2.

This is C0's shape of workload — see
[`c0-venus-first-light.md`](c0-venus-first-light.md) for what Venus, vtest, and
virglrenderer are, and for the C0 architecture diagram these fixtures reuse unmodified —
made to write, not just once, but every single frame, at a volume that goes from
essentially nothing (fixture B, 80 bytes) to something that actually hurts (fixture A, 1
MiB) at 60 fps. That is the axis these fixtures exist to measure: not "does mapped memory
work", but "how does the cost of mapped-memory writes scale with how much gets written."

---

## 2. How to run them

Both fixtures take exactly one argument: an output directory.

    cargo build -p rayland-icosa-cpu -p rayland-icosa-gpu
    mkdir -p /tmp/icosa-cpu /tmp/icosa-gpu
    ./target/debug/rayland-icosa-cpu /tmp/icosa-cpu
    ./target/debug/rayland-icosa-gpu /tmp/icosa-gpu

Each writes `frame_0000.png` through `frame_0119.png` into that directory and prints one
CSV line per frame to stdout, headed `frame,fractal_us,upload_us,draw_readback_us` (§4
below explains why that header does not mean the same thing in both files).

**The output directory must already exist, and this is not checked or reported clearly.**
Neither fixture calls `std::fs::create_dir_all` on its argument. If the directory is
missing, `write_png` (in `rayland-icosa-vk`, via the `image` crate's `save_buffer`) fails
on the very first frame with a plain

    rayland-icosa-cpu: No such file or directory (os error 2)

and the process exits 1. **This cost real confusion during development**: a run pointed at
a directory that had not been created failed on frame 0, before any of the expensive
per-frame work ran, and 120 frames therefore appeared to complete in a fraction of a
second — a runtime that looked like a suspiciously fast success far more than it looked
like an error, unless stderr was actually read. Read stderr. There is no other symptom.

### Running them through Venus

Exactly the pattern in [`c0-venus-first-light.md`](c0-venus-first-light.md)'s "How to run
it" — that document's environment-pitfalls section (`VN_DEBUG=vtest` being required and
failing silently without it, `VK_LOADER_DRIVERS_SELECT` needing to be unset, the socket
path needing to stay short) applies verbatim here and is not repeated. The one difference
is which binary is launched:

Terminal A — the host:

    cargo run -p rayland-engine --example vtest_serve -- /tmp/rl-icosa.sock

Terminal B — the fixture, pointed at Mesa's Venus ICD:

    env -u VK_LOADER_DRIVERS_SELECT \
        VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json \
        VN_DEBUG=vtest \
        VTEST_SOCKET_NAME=/tmp/rl-icosa.sock \
        ./target/debug/rayland-icosa-cpu /tmp/icosa-cpu-venus

(substitute `rayland-icosa-gpu` for the other fixture). The automated versions of this —
the ones actually used to produce §3's numbers and §5's headline result — are:

    cargo test -p rayland-engine --test icosa_cpu_venus_e2e -- --nocapture
    cargo test -p rayland-engine --test icosa_gpu_venus_e2e -- --nocapture

Each of these builds its fixture, runs it natively into a temp directory, runs it again
through a `VirglEngine`-backed vtest server into a second temp directory, and asserts all
120 PNG pairs are byte-identical. Both skip cleanly (printing `SKIP <test name>: ...`) on a
host with no `/dev/dri/renderD128` or no Venus ICD manifest, so CI stays green without a
GPU.

### The validation-layer recipe — load-bearing and easy to get wrong

Some of the findings below (§6) depend on running under Khronos's validation layer. Simply
forcing it to load is **not enough** and will silently prove nothing:

    VK_LOADER_LAYERS_ENABLE="*validation" ./target/debug/rayland-icosa-cpu /tmp/out

This loads the layer, and it will print **nothing at all** — not because nothing was
found, but because neither fixture creates a `VK_EXT_debug_utils` messenger (an ordinary
Vulkan program has no reason to, and these fixtures must stay ordinary per the design
spec's §2). Without a messenger, and without a settings file telling the layer where to
send what it finds, every check still runs, and the results simply go nowhere. **A run
without the settings file below reports success whether the code is right or wrong** —
this exact mistake was made once already in this project (see §7), and the run that made
it looked, from the outside, identical to a clean pass.

The recipe that actually reports something: write a settings file —

    khronos_validation.debug_action = VK_DBG_LAYER_ACTION_LOG_MSG
    khronos_validation.log_filename = stdout
    khronos_validation.validate_core = true
    khronos_validation.validate_sync = true

— and point `VK_LAYER_SETTINGS_PATH` at it:

    VK_LOADER_LAYERS_ENABLE="*validation" \
        VK_LAYER_SETTINGS_PATH=/path/to/that/file \
        ./target/debug/rayland-icosa-cpu /tmp/out

Now `Validation Error: [ VUID-... ]` lines, if any, appear on stdout. Both fixtures'
`tests/native_render.rs` carry this recipe already wired into a standing test
(`validation_layer_reports_no_errors_across_a_full_run`), so it does not need to be
reconstructed by hand for routine use — it is documented here because a future run *off*
that test harness (for instance, pointed at a different engine or a different driver) will
need to reproduce it, and "the layer printed nothing" is not evidence of anything unless
this file was in place.

---

## 3. What the two timing reports actually said

**Machine:** `dop561`, a 13th-generation Intel Core i7-1360P, using its integrated **Intel
Iris Xe Graphics (RPL-P)** GPU via `/dev/dri/renderD128`, driven by **Mesa ANV** (Mesa
26.0.3, reported Vulkan API version 1.4.335). Every number below was measured on this one
machine; a timing without its machine is not a measurement, and none of what follows
should be assumed to hold on different silicon, a different driver, or a discrete GPU.

Both fixtures were built in **release** mode and run natively (no Venus, no Rayland
anywhere) for the full 120-frame schedule. Means over all 120 frames:

| µs/frame | fixture A (CPU) | fixture B (GPU) |
|---|---:|---:|
| `fractal_us` | 49,439 | 0.3 |
| `upload_us` | 628 | 0 |
| `draw_readback_us` | 743 | 1,506 |
| **total** | **50,810** | **1,507** |

That is a **33.7× wall-clock speedup**, fixture B over fixture A — moving the fractal off
C's CPU and onto S's GPU is worth over an order of magnitude even before any question of
remoting enters the picture.

**Isolating the fractal itself**, rather than the whole frame: fixture B's `draw_readback_us`
minus fixture A's `draw_readback_us` (1,506 − 743 = 763 µs) is the *only* thing fixture B
adds to the shared render loop, and it is where the fractal's cost landed once it moved
into the fragment shader. Comparing that to fixture A's `fractal_us` (49,439 µs, the same
fractal on C's CPU) gives **49,439 µs on the CPU versus 763 µs on the GPU — a 65× speedup**
for the arithmetic alone, independent of everything else the frame does.

**The megabyte — the number that actually matters to Rayland.** Fixture A's `upload_us`
(628 µs) is the cost of moving that texture's bytes across local PCIe, on this machine,
measured directly. It is **1.2% of fixture A's total frame time** — genuinely negligible
here. But the whole design premise (§3.5 of the design spec) is that fixture A's texture
write is a *volume* that would need to cross a real network in (c)1's target scenario, and
628 µs of local PCIe bandwidth does not project to 628 µs over a slower link. **Projected**
(not measured — these figures scale the measured 628 µs by the ratio of PCIe bandwidth to
each link's bandwidth, and are marked as projections deliberately, because they were never
run):

| link | figure | % of fixture A's frame |
|---|---:|---:|
| local PCIe | **628 µs (measured)** | 1.2% |
| 1 GbE | 8,389 µs (projected, 13.4× local) | 14.3% |
| 100 Mbit | 83,886 µs (projected) | 83.9 ms — **more than the fractal itself** (49 ms) |

And the steady-state bandwidth this texture alone demands at 60 fps — one 512×512 RGBA8
texture on one solid — is **63 MB/s = 503 Mbit/s sustained**. That is not a spike; it is
what every second of smooth rendering needs, forever, for a single texture on a single
low-polygon object.

**The reading that matters, stated plainly:** mapped-write volume is *noise* on the machine
these fixtures were built and tested on, and would be *ruinous* on the network (c)1
actually targets. That asymmetry — invisible on a developer's desk, dominant on the wire —
is exactly the trap a design can fall into by testing only on one machine, and it is why
this pair of fixtures exists at four orders of magnitude of volume rather than one.

---

## 4. The CSVs mislead if read naively

Both fixtures print a CSV with the identical header,
`frame,fractal_us,upload_us,draw_readback_us`, and it is tempting to diff the two files
column by column. **Do not.** The columns mean different things in the two files, on
purpose, and the purpose is correct — but a reader who does not know that will draw a wrong
conclusion.

- `fractal_us` in fixture A really is the cost of the CPU Mandelbrot computation: ~49,439 µs
  on average, because that is literally what runs between the two `Instant::now()` calls
  bracketing it — `fractal::render_into`, the loop that iterates up to 512 times per texel
  for every texel in and around the sampled UV triangle.
- `fractal_us` in fixture B is **0.3 µs on average**, and it is *not* measuring a cheaper
  fractal. It is measuring the construction of a `Uniforms` value — a few floating-point
  multiplies via `frame_mvp`/`frame_zoom`. Fixture B's actual fractal evaluation happens
  **per fragment, inside the shader**, which this fixture's frame loop has no way to time
  in isolation because that work is submitted to and executed on the GPU as part of the
  shared `Scene::draw` call. It shows up instead as the difference between fixture B's
  `draw_readback_us` (1,506 µs) and fixture A's (743 µs) — see the isolation arithmetic in
  §3.
- `upload_us` in fixture B is not merely small — it is a hardcoded `0`, on every frame,
  because fixture B has no separate upload step for the frame loop to time at all: its
  entire per-frame mapped write is the 80-byte uniform block, written inside the shared
  `Scene::draw` call, with no distinct "copy the texture in" step the way fixture A has.
  The column is kept in fixture B's CSV, at a constant zero, purely so the two files have
  the same shape and can be loaded with the same tooling — not because there is anything
  being measured there.

This is *correct*, not a bug: the fractal genuinely left the CPU when it moved into the
shader, so it *should* vanish from a column that only ever measured CPU-side work. But
"vanish from the column" and "vanish from the cost" are different claims, and only the
first one is true. Anyone who sees `fractal_us` drop from 49,439 µs to 0.3 µs and concludes
the fractal got 165,000× cheaper has mistaken a bookkeeping artifact of where the clock
started and stopped for a real result. It got roughly 65× cheaper (§3), and the other
100,000× is an accounting shift, not a discovery.

---

## 5. The headline: both fixtures passed, and that is not what the spec expected

This is the finding that matters most, and it should not be softened.

The design spec's §9 predicted, in order of likelihood, that these fixtures would fail
against Rayland — first on the depth attachment, then on per-frame mapped texture writes
(the (c)2 problem, "unsolved by construction"), then on texture upload bandwidth. **Neither
prediction held.** Run through Rayland's C0 path — `cargo test -p rayland-engine --test
icosa_cpu_venus_e2e` and `--test icosa_gpu_venus_e2e` — both fixtures produced **120 of 120
frames bit-identical** to their native runs. The depth prediction was independently wrong
too, for an unrelated reason (§8 below).

**These tests are not vacuous.** Each requires the fixture to genuinely connect to the
vtest socket — a fixture that fails to connect (the classic missing-`VN_DEBUG=vtest`
failure documented in the C0 doc) makes the test panic with a specific diagnosis rather
than silently passing — and each asserts `context_id == Some(1)`, meaning a real Venus
context was actually created by a real client speaking the real protocol. The remoted runs
also took real, measurably longer wall time than the native ones: **24 seconds (fixture A)
and 6 seconds (fixture B), against 13 seconds and 2 seconds natively** — consistent with a
real command stream and a real megabyte crossing a real (if local) engine on every frame,
not with a test that silently fell back to rendering natively and comparing a file to
itself.

### Why the prediction was wrong

It conflated **"through Venus"** with **"across a network"**. Those sound like the same
claim and are not, and the difference between them is the entire content of (c)2.

The path these end-to-end tests exercise is C0's path: one machine, a local Unix socket,
and a Venus ICD that hands the ring and every blob — including fixture A's staging buffer,
the one carrying a megabyte of freshly-computed fractal every frame — to Rayland's engine
as **memfds passed over `SCM_RIGHTS`**. The application's "mapped memory" *is*, physically,
the same pages virglrenderer reads to replay the draw. Nothing is copied across a wire,
nothing is serialized, nothing is transported at all in the sense that matters here — so
nothing can be dropped, torn, delayed, or applied out of order. Per-frame mapped writes
therefore work **perfectly** on this path. Not because Rayland has solved mapped-memory
coherence — it has not attempted to, since (c)2 does not yet exist — but because **on one
machine sharing physical memory, there is nothing to solve.**

[The Venus ring findings document](design/2026-07-15-venus-ring-findings.md) already stated
the half of this that mattered to (c)1: *neither a shared page nor a file descriptor
survives a network.* The corollary went unstated there, and it is exactly what the design
spec's §9 missed: **both survive a Unix-domain socket perfectly well.** A prediction built
on "Venus hands the app's writes to the engine with no interceptable API call" is true and
was the entire premise of these fixtures — but on a single machine, "no interceptable API
call" and "no problem" are the same sentence, because the pages never needed to move.

### What Task 8 actually is

Given that, these tests are not the (c)2 proof the spec set out to produce. They are the
**control** for it. They establish, concretely and by running, that these fixtures render
bit-identically when the memory really is shared — which is exactly the fact that has to be
true before anyone runs them across an actual relay, because it is what makes any future
divergence there **provably the relay's fault and not the fixture's**. A fixture that had
never been shown to work on the easy path would leave every future failure ambiguous
between "the fixture is subtly wrong" and "the relay is subtly wrong." That ambiguity is
now closed. As a baseline, that is worth more than the predicted failure would have been.

### Where the fixtures will actually bite

Through **`rayland-c` → `rayland-s`** — (c)1's QUIC relay — where the pages genuinely
cannot be shared (two machines cannot share physical memory) and the file descriptor
genuinely cannot cross a network (`SCM_RIGHTS` is a Unix-domain-socket-only mechanism).
**That run has never happened, and this spec never commissioned it.** Neither `rayland-c`
nor `rayland-s` currently knows how to launch either fixture — wiring that up is the
obvious, and currently entirely open, next piece of work, and it is where the design
spec's original §9 expectations (depth first, then mapped writes, then bandwidth) properly
belong, unrefuted, waiting for the run that would actually test them.

---

## 6. Vulkan's own sync validation cannot see the mapped write

This is a separate, and in some ways sharper, statement of the same underlying problem, and
it did not require Venus at all to demonstrate — it shows up on the native path, on this
one machine, with no remoting involved.

Fixture A's texture upload path (`texture.rs`) does two things the Vulkan specification
requires and that took real engineering to get right: `FractalTexture::upload` waits on its
own fence before returning (so the next frame's CPU write into the staging buffer cannot
race the GPU's read of the previous frame's contents), and it records a layout-transition
barrier after the copy (`TRANSFER_DST_OPTIMAL → SHADER_READ_ONLY_OPTIMAL`) before the draw
samples the texture.

**Removing the fence wait and running the full 120-frame suite under
`khronos_validation.validate_sync = true` (with the settings file from §2, so the layer
actually has somewhere to report) produces zero errors.** This is not a gap that a future
version of the validation layer might close. It is structural: fixture A's per-frame
fractal write is a bare host memory store through a raw pointer obtained once, at startup,
into `HOST_VISIBLE | HOST_COHERENT` memory, with no `vkFlushMappedMemoryRanges` call
anywhere in the program. **There is no Vulkan API call at the moment that write happens for
any layer to hook.** Sync validation works by tracking what each *API call* touches and
when; a plain memory write through a pointer is invisible to it by construction, not by
oversight. (The fence wait itself remains required and was never weakened in the shipped
code — nothing in the Vulkan specification licenses skipping it merely because one tool
cannot currently observe its absence; removing it was a mutation, run to observe this
result, and reverted immediately afterward.)

Contrast this with the layout-transition barrier: removing *that* mutation, under the same
settings, fires `Validation Error: [ VUID-vkCmdDraw-None-09600 ]` — ten times across the
120-frame run — every time. That barrier crosses a real API call (`vkCmdPipelineBarrier`),
so the validation layer can see it, and does. The difference between the two mutations is
the whole point: one lives on a wire the layer watches, the other does not exist as a wire
event at all.

**This is (c)2's problem, stated by the ecosystem's own tooling rather than by this
project.** Khronos's own validation layer sees every Vulkan API call an application makes —
that is its entire mechanism. If it is structurally blind to a host write through a
persistently-mapped `HOST_COHERENT` buffer, then **any** relay that works by watching API
calls is blind to the identical write, for the identical reason. This is not a limitation
Rayland introduced and not one Rayland can fix by watching more carefully; it is a property
of what mapped memory *is* in Vulkan's own model, confirmed independently by the tool the
ecosystem trusts most to catch exactly this class of mistake.

---

## 7. Depth testing is unobservable in this scene

The design spec originally justified carrying a depth attachment on the grounds that
"without depth testing the back faces paint over the front ones depending on submission
order." **That claim is false, and it was disproved by direct experiment** — six
orientations (`frame_mvp(0, 15, 37, 60, 90, 111)`) were rendered with the production
pipeline configuration (`cull_mode(BACK)`) once with depth testing enabled and once with it
disabled, and the raw pixel bytes were compared: **byte-for-byte identical, in all six
rotations.** Disabling culling *as well* makes the two diverge (first byte difference at
offset 47413), which shows the mechanism rather than merely confirming the symptom: the
icosahedron is a **convex** solid, so a ray from the camera through any given pixel enters
through exactly one front-facing triangle and exits through exactly one back-facing
triangle. Back-face culling discards the exit triangle before the rasterizer ever produces
a fragment for it, so with culling on, at most **one** fragment ever competes for a given
pixel — and `CompareOp::LESS` has nothing left to reject.

**This is a correction to the design spec, not a defect in the fixtures.** §4 of the design
spec has already been updated in place to say so.

**The attachment is kept anyway, and the reason is Rayland's, not the picture's.** This is
the first depth attachment anywhere in this repository — `rayland-refapp`'s single triangle
never carried one — so it is the first exercise, through any part of the remoted path, of
depth-stencil **format selection, depth image allocation, and attachment setup**. Those
mechanics run on every frame regardless of whether any fragment is ever actually rejected
by the depth test, and Task 8's bit-identical result over 120 frames is real coverage of
them, even though the picture itself would look identical without the attachment at all.
**Do not "simplify" the depth attachment away** on the strength of this finding — a future,
non-convex workload would need it working, and removing it now would silently retire the
only coverage this project has of the depth path, with no visible symptom to catch the
removal.

---

## 8. What the fixtures could not reach

This is the section most likely to be skipped, and — because everything above it describes
paths that worked — it is the most valuable one for whoever picks up (c)2. Nothing here
should be read as a defect in these fixtures; each is a boundary the fixtures were built up
to but not past, because something else has to exist first.

**The relay path was never exercised, and this is where the hard problem actually lives.**
Every result above ran over C0's path — one machine, a local socket, shared physical
memory. `rayland-c` → `rayland-s`, (c)1's QUIC relay, is where a shared page genuinely
cannot exist and a file descriptor genuinely cannot cross, and neither daemon currently
knows how to launch either icosa fixture. No test anywhere in this project has pointed a
fixture at that path. Building that wiring is not optional groundwork for (c)2 — it is
where (c)2's actual subject matter begins.

**Fixture A's fence wait and post-copy barrier are spec-correct, and one of the two is not
observable through the local test suite at all.** §6 above is the detailed version; the
summary is that mutation-testing showed removing the fence wait changes nothing observable
(sync validation is structurally blind to it, and separately, `Scene::draw`'s own
intervening fence wait closes the actual data race at any fractal cost, including zero —
so even a plausible "the fractal is slow enough that the race never opens" explanation
turned out to be the wrong mechanism when checked). Intel's ANV driver also tolerates the
*missing* layout-transition barrier for this particular simple, uncompressed image copy —
sampling a `TRANSFER_DST_OPTIMAL` image where `SHADER_READ_ONLY_OPTIMAL` was expected
produced no visible pixel difference on this driver, though the Vulkan specification calls
that undefined behavior and a different driver (a tiled or compression-capable one) could
turn it into visible corruption or a lost device with zero change to this code. The barrier
*is* pinned, by a different, sharper instrument: the validation-layer test (§6) fires
`VUID-vkCmdDraw-None-09600` at every one of the 120 draws when it is removed. Both pieces
of code are correct, required by the specification, and left exactly as built.

**Cross-architecture bit-exactness is designed for and has never been tested.**
`rayland-icosa-core`'s `log2` and `sin_cos` were rebuilt from IEEE basic operations
specifically so that an x86 C and a RISC-V C would compute the identical fractal texture
and the identical rotation matrix, bit for bit — see the design spec's §5.4 (amended by
this same task to cover `sin`/`cos`, which is the same trap `log` already was). That design
work is real and the frozen bit-pattern tables pin it against regression. But no RISC-V run
of either fixture has ever happened. The bit-exactness argument that makes §5.5's "any pixel
difference is a Rayland defect" claim fair is, for now, a property proven on paper and by
unit test, not by a cross-architecture render.

**No on-screen presentation.** Both fixtures write PNGs to a directory and exit; neither has
ever produced a frame on a live compositor. `rayland-present` exists and SP1/(c)1 have put a
frame from a *different* workload on a real screen, but these fixtures have not been routed
through that path.

---

## 9. A theme worth naming: the instrument that could not have seen it

Several of the findings above share one shape, and it is worth naming once rather than
leaving it implicit in each: **an instrument reported a result it was not actually in a
position to produce**, and the absence of a signal got read as evidence of correctness when
the instrument itself was, in effect, switched off.

- The validation layer, run with no `VK_EXT_debug_utils` messenger, printed nothing —
  on a correct build and a broken one alike (§2, §6) — and an early pass over fixture A
  read that silence as "no errors" rather than as "nowhere for errors to go."
- A test's own `eprintln!` announced "the scaffolding renders a shaded, depth-tested solid"
  while disabling depth testing entirely changed not one byte of output (§7); the program
  was truthfully reporting what it *intended*, not what it had actually demonstrated.
- The frozen bit-pattern table for `log2` (`rayland-icosa-core`, Task 1) carried a doc
  comment claiming coverage of values "just above and below" every power of two across
  eight inputs; decomposing those same eight inputs by hand showed the polynomial was
  actually pinned at only three distinct points in its domain, missing both of the edges
  the comment claimed to cover.
- Elsewhere in this project's history (the (c)1 network work), `ssh-add -l` reported a key
  as available and usable — because it was *listed* by a locked keyring agent — right up
  until every authentication attempt against it failed, because a locked key can be listed
  but cannot sign.

None of these are the same bug, and none of them are really about carelessness. They are
about a check that *looks* like it covers a case continuing to look that way right up until
someone asks it to actually fire — and a design (or a test, or a comment) that has never
been made to fail on purpose gives no evidence at all about whether it would catch a real
failure. The corrective in every instance above was the same: run the mutation, watch for
the alarm, and only then trust the silence. That is worth carrying into (c)2 directly,
because a relay that watches Vulkan API calls will, by §6's finding, be exactly this kind of
instrument with respect to mapped memory — correctly silent about everything it can see,
and telling you nothing whatsoever about the thing it cannot.

---

## 10. Where this leaves (c)2

The fixtures did their job: they produced a real, reproducible, four-orders-of-magnitude
number for how the cost of mapped-memory writes scales with volume (§3), they showed that
Vulkan's own tooling cannot see the write that (c)2 exists to solve (§6), and they proved —
by actually running, not by argument — that the easy path works, so that the relay path's
first real test will mean something the moment someone builds it (§5, §8). What they did
not do, and could not have done without (c)1's relay existing to run them over, is show the
failure the original spec predicted. That failure is still out there, unrefuted, waiting at
the far end of `rayland-c` → `rayland-s` — and per §6, it will not announce itself through
any Vulkan API call, which is the one fact about it that is not in question.

---

## 11. For (c)1's Task 9: how to point these at the relay

§10 says the failure is waiting at the far end of `rayland-c` → `rayland-s`. This section says
concretely what to do about it, because (c)1's Task 9 — the netem sweep — is the run that can.

### Why the sweep wants fixture A

Task 9's workload today is `rayland-refapp`: one static triangle, one vertex buffer written once at
startup. That is the right thing for Task 8's bring-up and it is **silent** on the questions Task 9
asks, because it never touches mapped memory again and never sends a meaningful byte.

| | `rayland-refapp` | `rayland-icosa-cpu` |
|---|---|---|
| mapped writes after startup | none | **1 MiB every frame** |
| frames | 1 | 120 |
| per-frame CPU work | none | ~49 ms of Mandelbrot |

**Keep refapp in the sweep.** It is the trivial baseline that says whether a failure is
workload-specific or general. Fixture A is the stress case, not a replacement.

### What the control buys you

Both fixtures render **bit-identically through Venus on the C0 path** (§5). That is not a proof about
mapped memory — it is a *control*, and its value is entirely for this run: it means **any divergence
across the relay is provably the relay's, not the fixture's.** Without it, a failure over the network
would leave you asking whether the workload was ever sound. That question is now closed.

### The comparison rule (do not get this backwards)

Compare the remoted run against the fixture run **natively on S**, never on C. `c1-two-machine.sh`
already states why: C's GPU is a different rasteriser, so a C-side baseline compares renderers
instead of transports. `crates/rayland-engine/tests/icosa_{cpu,gpu}_venus_e2e.rs` already obeys this
rule (they remove `VK_ICD_FILENAMES`/`VN_DEBUG`/`VTEST_SOCKET_NAME` for the native leg) and are a
working model.

One thing in the run's favour: with C and S both x86_64, the CPU-computed fractal is bit-exact across
them for free. `rayland-icosa-core` rebuilds `log2`/`sin_cos` from IEEE basic operations so this
survives a RISC-V C as well — but **that has never been tested** (§8). If a RISC-V C ever joins,
fixture A is ready and that is where the bit-exactness earns its keep.

### The open question these fixtures exist to ask

**Does (c)1's blob sync actually ship the megabyte every frame, and what does it cost?**

Task 5's conservative blob sync must move it somehow: 1 MiB × 120 frames = **120 MiB** of texture
for a three-second animation. The fractal changes **every texel of every frame** — it is a zooming
view, not a static image with a moving overlay — so the byte-granular diff of Task 5b should find
**nothing to elide**. That makes this close to the worst case the design can be handed, which is
exactly what a feasibility verdict should be tested against.

If the diff *does* help here, that is a surprising result and worth understanding before trusting it
elsewhere.

For scale, measured locally (dop561, Intel Iris Xe / Mesa ANV, release, means over 120 frames): the
megabyte costs **628 µs** to move on-machine — **1.2% of fixture A's frame**. Projected on bandwidth
alone: **8.4 ms at 1 GbE**, **84 ms at 100 Mbit** — the latter *more than the fractal itself*. Those
projections are what the sweep replaces with fact.

### Practicalities that cost real time here

- **The output directory must already exist.** Both fixtures exit 1 with
  `No such file or directory (os error 2)` otherwise. This made 120 frames appear to take 0.48 s
  once — a failed run read as a measurement.
- **Budget the wall-clock and distinguish a timeout from a rendering failure** — they are different
  findings. Native: fixture A ≈ 13.4 s for 120 frames, fixture B ≈ 2 s. Through Venus locally: 24 s
  and 6 s.
- **The validation layer prints nothing without a settings file** (§2). The fixtures register no
  `VK_EXT_debug_utils` messenger, so a run without `VK_LAYER_SETTINGS_PATH` reports success whether
  the code is right or wrong. `crates/rayland-icosa-cpu/tests/native_render.rs` has a working example.
- **Do not add `VN_DEBUG=no_abort`** — `c1-two-machine.sh` explains why at length, and fixture A's
  120-frame run gives a ring stall far more chances to occur than refapp's single frame ever did.

### If the fixtures need to change for the sweep

They are deliberately option-free (design spec §2): no `--frames`, no `--no-texture`, no size knob,
because a fixture with opinions about how it is run stops being evidence about how real applications
behave. **"The sweep needs it" is a real reason** to revisit that. A shorter run, a different texture
size, or a fixed subset of frames can be added properly — as constants, or as a second binary — in
preference to patching around the fixture or, worse, weakening it until it passes.
