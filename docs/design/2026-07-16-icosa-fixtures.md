# The icosahedron fixtures — a workload that makes `vkMapMemory` bite

**Status:** design, ratified 2026-07-16. Not yet implemented.
**Serves:** (c)2 (mapped-memory coherence) primarily; (c)3 (content-addressed assets) and
on-screen work secondarily.
**Required reading first:**
[`2026-07-15-venus-ring-findings.md`](2026-07-15-venus-ring-findings.md), especially
§6 "Finding 5 — the genuinely hard problem is `vkMapMemory`, not the ring".

---

## 1. Why this exists

Rayland has exactly one unmodified reference application: `rayland-refapp`. It draws one
red triangle on a blue background at 64×64, reads the pixels back, and writes a PNG. It
was the right fixture for C0, whose claim was narrow — *a real, unmodified Vulkan program
can render on S's GPU at all*. It proved that, and it is still the right program for that
proof, so it is not being changed.

But it is a **static, single-frame** program. It writes its vertex buffer once, at
startup, and never touches host-visible memory again. That makes it nearly silent on the
problem the findings document names as the hard one:

> `vkMapMemory` has no API call to intercept — apps write vertices and textures straight
> into mapped memory, and the only lever is Vulkan's coherence rules.

An application that writes mapped memory *once* does not stress a design that has to cope
with applications writing mapped memory *continuously*. (c)2 needs a workload that does
the latter, on purpose, at a volume that hurts. Nothing in the repository currently
produces one, and nothing in the repository currently produces a number for what mapped
writes cost across a network.

These fixtures are that workload. They are test instruments first and pretty pictures
second. If a choice ever arises between "more impressive" and "more diagnostic", take
diagnostic.

### What they are *not*

They are not a demo, not a benchmark suite, and not a general-purpose renderer. They are
also not a replacement for `rayland-refapp` — refapp's value is that it is the simplest
thing that can possibly be remoted, which makes it the right first thing to check when
anything breaks. These fixtures are the *next* thing to check, when refapp passes and
something harder does not.

---

## 2. The inherited rule: the fixture must not know

`rayland-refapp`'s crate documentation states the rule these fixtures inherit verbatim:

> The only way to be certain an application has not been adapted to the remoting is for it
> to have no knowledge of the remoting to adapt to.

Therefore, for every crate specified here:

- **No dependency on any `rayland-*` crate except `rayland-icosa-core` and
  `rayland-icosa-vk`** — the two libraries specified in §3, which are themselves bound by
  every rule in this section and know nothing about remoting either. Not for logging, not
  for utilities, not for tests.

  The `rayland-` prefix on those two is a workspace naming convention, not a statement that
  they are part of Rayland's remoting machinery; they are the fixtures' own support code
  and nothing else depends on them. What the rule actually protects is that no code a
  fixture links can *see* the remoting — and none can. The test to apply when adding any
  dependency is not "is it in this workspace" but **"could this let the program tell it is
  being remoted?"**
- **No mention** of Venus, vtest, virglrenderer, sockets, rings, blobs, or remoting —
  in code, comments, documentation, or crate metadata.
- **No environment probing**, no conditional rendering paths, no command-line rendering
  options. The binary must not be able to discover whether it is being remoted, because a
  binary that cannot discover it cannot accidentally be written to accommodate it.
- Everything that makes the remote path happen lives in the **environment** the binary is
  launched with (`VK_ICD_FILENAMES`, `VN_DEBUG`, `VTEST_SOCKET_NAME`), exactly as with
  refapp.

There is one deliberate, narrow exception, discussed in §7: the fixtures print per-frame
timings to stdout. This is ordinary profiling output of the kind any graphics program
might have. It probes nothing and reveals nothing about the environment. It stays inside
the rule.

**Pitfall for anyone extending these later.** The temptation will be to add a flag — a
`--frames` option, a `--no-texture` switch, a `--headless` toggle. Resist it. Each such
flag is a place where the fixture starts to have opinions about how it is being run, and
a fixture with opinions about how it is being run is no longer evidence about how *real*
applications behave. If a variant is genuinely needed, it is a new binary with its own
constants, not a flag on this one.

---

## 3. Structure: four crates

### 3.1 `rayland-icosa-core` — the shared, GPU-free library

Contains everything both fixtures must agree on, and nothing that touches a GPU:

- The icosahedron geometry table (§4).
- The frame schedule: `frame_orientation(i)` and `frame_zoom(i)` (§5).
- The Mandelbrot smooth-iteration and HSV→RGB math, in scalar Rust (§6).
- The bit-exact `log2` the smooth-iteration formula needs (§6.2).

Dependencies: none beyond `core`/`std`. No `ash`. No `image`. This crate must be unit
testable on a machine with no GPU, no driver, and no display, because its correctness is
mathematical and should not be hostage to a graphics stack being installed.

License LGPL-3.0-or-later (a library), `publish = false` (a test fixture; nothing here
belongs on crates.io).

**Why a shared crate rather than duplicating the math.** The entire diagnostic value of
having two fixtures (§3.5) rests on them being identical in every respect except the one
under study. Two copies of the fractal math would drift — someone would fix a rounding
detail in one and not the other — and the moment they drift, the comparison between the
two fixtures stops measuring what it claims to measure and starts measuring the drift.
Sharing the code is what makes the pair an instrument instead of two programs.

### 3.2 `rayland-icosa-vk` — the shared Vulkan scaffolding

Contains everything both fixtures must agree on *graphically*, exactly as §3.1 holds
everything they must agree on arithmetically:

- Vulkan bring-up: instance, physical device, queue, logical device, command pool, and
  memory-type selection.
- The depth-tested render pass and graphics pipeline (§4), parameterised only by which
  fragment shader it uses and whether a sampler is bound.
- The colour target, the depth target, and the readback buffer.
- `MappedBuffer`: the persistent `HOST_VISIBLE | HOST_COHERENT` mapping **both** fixtures
  write through (§7.2).
- The draw-and-read-back, and the PNG write.

License LGPL-3.0-or-later (a library), `publish = false`.

**Why this is shared and not copied into each fixture.** The argument is §3.1's, applied
to the render loop rather than the arithmetic. If each fixture carried its own copy of
roughly 1200 lines of Vulkan, the two would drift — someone would fix a barrier or a
format in one and not the other — and the moment they drift, the comparison stops measuring
where the fractal is computed and starts measuring the drift. Sharing the code makes the
control a **fact** rather than a promise: the fixtures *cannot* differ in what this crate
holds.

**It does not make the fixtures less ordinary.** Real Vulkan applications lean on helper
libraries — VMA, vk-bootstrap, whole engines — constantly. A program with 1200 lines of
hand-rolled bring-up inline is the *less* typical one. What matters for §2's rule is that
nothing in this crate knows about remoting either, and nothing does.

**What deliberately stays out of it.** The frame loop, the timing report, and the texture
path. Those are where the fixtures differ, and a shared frame loop with an `if texture`
inside it would bury the experiment's independent variable in a library — the one place
nobody thinks to look for it.

### 3.3 `rayland-icosa-cpu` — fixture A, the torture test

Computes the fractal on **C's CPU** every frame, writes it into persistently-mapped
host-visible memory, uploads it, draws the spinning solid, reads back, writes PNGs, prints
timings. This is the (c)2 workload proper. Binary crate → GPL-3.0-or-later per repository
policy, `publish = false`.

### 3.4 `rayland-icosa-gpu` — fixture B, the control

Same geometry, same lighting, same schedule, same math — but the fractal is evaluated in a
fragment shader on S's GPU, so only a handful of scalars cross per frame instead of a
megabyte. Binary crate → GPL-3.0-or-later, `publish = false`.

### 3.5 What the pair measures, and what it does not

Fixture A minus fixture B approximates the cost attributable to **mapped-write volume and
texture upload**, because everything else — vertex count, draw calls, resolution, frame
count, lighting, the arithmetic itself — is held identical by construction.

"Held identical by construction" takes real care, and is easy to lose. §4's rule that
fixture A iterates only the sampled triangle exists entirely for this reason: B's
rasteriser restricts its fractal work to the visible region automatically, so an A that did
not restrict its own would be doing three times the arithmetic for a reason unrelated to
the experiment. Any future change to one fixture must be checked against this paragraph
before it is made. The pair is an instrument, and an instrument with an uncontrolled
variable in it is just two programs.

**Be precise about what the control controls.** Both fixtures write to mapped memory with
no interceptable API call; both are `HOST_COHERENT`; neither ever calls
`vkFlushMappedMemoryRanges`. They differ only in **volume**: roughly 1 MiB per frame
versus roughly 128 bytes per frame. So the pair does *not* isolate "mapped writes versus
no mapped writes" — that experiment does not exist here, and could not, since a moving
picture needs at least a matrix to change. What the pair isolates is **how the cost scales
with mapped-write volume**, across four orders of magnitude of it. That is the more useful
question, and it is the one (c)3's content-addressed assets exist to answer.

### 3.6 Workspace registration

Four entries are added to the workspace `members` list in the root `Cargo.toml`. This
takes the workspace from twelve crates to sixteen, which makes the sentence "A Cargo
workspace of twelve crates" in `CLAUDE.md` false — so `CLAUDE.md`'s crate count and crate
list are updated **in the same change** that adds each crate, per its own self-update rule.
This spec does not itself trigger that update, since a document adds no crates.

---

## 4. Geometry

A regular icosahedron: 20 equilateral triangular faces, 12 vertices, 30 edges. Generated
from the standard golden-ratio construction — the 12 points are the cyclic permutations of
`(0, ±1, ±φ)` where `φ = (1 + √5) / 2` — normalised to the unit sphere and scaled to fit
the view.

**Vertices are not shared between faces.** The table emits 60 vertices, three per face,
even though only 12 distinct positions exist. This is deliberate: each face then carries
its own flat normal, and the solid renders with hard edges and visibly distinct faces,
which is what makes it read as a Platonic solid rather than a low-polygon ball. Sharing
vertices would force normals to be averaged at the corners and smooth the edges away.
There is no index buffer; at 60 vertices it would save nothing worth the extra Vulkan
surface.

**A depth buffer is required.** A solid seen from outside has faces behind other faces,
and without depth testing the back ones paint over the front ones depending on submission
order. This is the first thing in Rayland that needs a depth attachment — refapp's single
triangle never did — so it will be the first exercise of depth-stencil format selection,
depth image allocation, and depth attachment setup through the remoted path. Expect this
to be where breakage appears first, and treat a depth-related failure as a finding about
Rayland rather than a bug in the fixture.

**UV mapping.** All 20 faces sample the *same* equilateral triangle, centred in the fractal
texture with a margin around it. Every face therefore shows the same image, and the zoom is
visible on all of them simultaneously. The alternative — atlasing 20 distinct sub-regions —
buys nothing diagnostic and adds a per-face layout that can be subtly wrong in ways a test
would struggle to catch.

Note it is **not** *inscribed*, and the distinction matters to the arithmetic below: a
genuinely inscribed (maximal) equilateral triangle in a square has side ≈1.035 and covers
≈46% of it. This one has side 0.866, covers 32.5%, and touches none of the texture's edges.
The margin is not slack — it is what leaves room for the two-texel dilation §7.2 requires.

**Only the sampled triangle is iterated.** The triangle covers 32.5% of the square texture,
so a naive full-texture fractal would spend roughly two thirds of its arithmetic on texels
no face ever samples. Fixture A therefore runs the Mandelbrot iteration **only for texels in
and immediately around the UV triangle**, and fills the rest with black.

This is not merely an efficiency point, and it is not optional. Fixture B evaluates the
fractal **per fragment**, so it only ever computes the visible region — it gets the
triangular restriction for free, from the rasteriser. A fixture A that iterated the whole
square would do about three times B's fractal arithmetic for reasons that have nothing to
do with where the fractal is computed, which is the single property §3.5 claims the pair
isolates. The comparison would be contaminated by a factor of three, silently.

**The full megabyte is still written and still uploaded.** Only the *iteration* is
restricted; the black padding is written into mapped memory every frame like everything
else. This is deliberate. The expensive thing (up to `MAX_ITER` iterations per texel) is
what must not be wasted; the byte traffic is what the fixture exists to create, and
shrinking it to fit the triangle would quietly reduce the headline number in §5.3 that the
whole workload is built around.

### 4.1 Unit tests (no GPU)

- The generated solid has exactly 20 faces and 60 vertices.
- All 30 distinct edges have equal length.
- Every face normal points outward (its dot product with the face centroid is positive).
- The 60 vertices collapse to exactly 12 distinct positions.

---

## 5. Animation and determinism

### 5.1 No wall-clock, ever

Orientation and zoom are **pure functions of the frame index**:

```
frame_orientation(i) -> rotation to apply at frame i
frame_zoom(i)        -> complex-plane half-width at frame i
```

No `Instant::now()`, no delta-time, no frame skipping, no vsync coupling. This is what
makes an animated fixture testable at all: the same binary run twice produces the same 120
images, so "native versus remoted" is a comparison of two known quantities rather than a
race between two timelines. A fixture that animated on wall-clock time could not be
asserted against, only squinted at.

A run is **120 frames**, fixed as a constant, not an option.

### 5.2 The zoom schedule

The zoom targets a fixed, known-interesting coordinate in the complex plane, with a
geometric schedule of 0.97× per frame. Over 120 frames that reaches `0.97^120 ≈ 0.026×`
the starting half-width — visibly deep, and comfortably inside `f64`'s 52-bit mantissa, so
there is no need to design around precision exhaustion. (Note that `mandelsmooth.c` uses
`float` and interactive mouse-driven zoom; both are dropped here — `f64` for headroom, a
fixed schedule for determinism.)

### 5.3 Sizes, and why these ones

| Thing | Size | Reason |
|---|---|---|
| Render target | 256×256 | Big enough for a shaded solid to have unambiguous interior and silhouette for a test to check; small enough that 120 readbacks stay cheap. |
| Fractal texture | 512×512 RGBA8 = 1 MiB | Real bandwidth: ~63 MiB/s at 60 fps. Sits well under the 8 MiB blob cap pinned by (c)1 Task 2, so it stresses the design without immediately hitting an unrelated ceiling. |

### 5.4 The `log()` trap

The smooth-iteration formula (inherited from `mandelsmooth.c`) is:

```
smooth_iter = i + 1 - log(log(|z|)) / log(2)
```

`log` is a libm function. **libm results are not identical across platforms.** IEEE-754
exactly specifies `+ - * /` and square root; it does *not* specify transcendentals, and
implementations legitimately differ in the last bits. C's target machines explicitly
include RISC-V, so a fixture whose CPU-computed texture depends on the host libm would
produce different pixels on different C machines — and the resulting test failure would
look exactly like a Rayland bug while being nothing of the sort. That is a very expensive
afternoon, and it is avoidable for about ten lines of code.

**Resolution:** `rayland-icosa-core` implements its **own** `log2` — a polynomial over the
float's exponent and mantissa bits, using only IEEE `+ - * /`. Rust does not
automatically contract expressions into FMA, so an expression built from those operations
evaluates bit-identically on any IEEE-754 host. The fragment shader in fixture B uses the
same polynomial, transcribed.

This is not about the fractal being *accurate*. It is about it being *reproducible*. A
visually fine approximation that is bit-exact everywhere is strictly better here than a
correctly-rounded one that is not.

**Unit test:** the custom `log2` is checked bit-for-bit against a stored table of inputs
and expected bit patterns, generated once and committed. The table is the contract; if a
refactor changes a single bit, the test fails, which is the intent.

### 5.5 Why bit-identical is a fair demand

In the remoted case, the drawing happens on **S's GPU**. In the native baseline, the
drawing also happens on S's GPU, via S's local driver. Same hardware, same driver, both
paths. The CPU-side fractal is computed on C in both cases, bit-exactly per §5.4. The only
thing that differs between the two runs is *how the commands reached the GPU*.

Therefore any pixel difference at all is a Rayland defect, and the tests demand exact
equality rather than a tolerance. Tolerances hide precisely the class of bug this project
is most likely to produce — a dropped mapped write, a stale texture, a delta applied out
of order — because those bugs are usually *small* before they are large. A tolerance is a
place for them to live.

---

## 6. The fractal math

Transcribed from `~/git/mandelsmooth`'s `mandelbrot.c`, which — worth knowing before
"porting" it — is **already a GLSL fragment shader**, with mouse-wheel zoom driven from
SDL. So fixture B is close to a direct transcription of that shader into SPIR-V, and
fixture A is the same fifteen lines of math as a scalar `f64` loop. What is dropped: SDL,
the interactive event loop, mouse-driven navigation, and `float` precision.

The algorithm, unchanged in substance:

1. Map the pixel to a point `c` in the complex plane from the frame's centre and zoom.
2. Iterate `z = z² + c` up to a maximum iteration count, escaping when `|z|² > 4`.
3. Points that never escape are black.
4. Points that escape get a smooth iteration count (§5.4), normalised, mapped to a hue,
   and converted HSV→RGB.

### 6.1 Iteration count

**512**, fixed as a constant in `icosa-core`, not an option. `mandelsmooth.c` uses 2000;
that was tuned for an interactive GPU shader and is far too expensive for a per-frame CPU
loop on a machine that may be a RISC-V single-board computer.

The worst case is **~45.0 million iterations per frame**, not the 512×512×512 ≈ 134 million
a full-texture fractal would cost: §4's restriction means only the dilated triangle is
iterated, about 33.5% of the texture. That is heavy enough to be honest about C being weak,
light enough not to dominate the measurement, and ample detail at the zoom depth §5.2
reaches.

Both fixtures use this same constant. B must not get a different iteration count, or the
pair stops being a controlled comparison.

### 6.2 The bit-exact `log2`

See §5.4. Lives in `icosa-core`, used by both fixtures.

---

## 7. Per-frame behaviour

### 7.1 Both fixtures

Lighting is a single fixed directional light with Lambert diffuse plus a small ambient
term, modulating the sampled fractal colour. Fixed — not animated, not configurable —
because a moving light adds a second thing that could be wrong without adding anything
that could be learned.

### 7.2 `rayland-icosa-cpu`, per frame

1. Compute the 512×512 fractal **directly into persistently-mapped host-visible memory**.
   `vkMapMemory` is called **once**, at startup; every frame thereafter writes through the
   raw pointer. Texels outside the sampled UV triangle are written black without being
   iterated (§4): the full megabyte crosses mapped memory, but none of the Mandelbrot
   arithmetic is spent where no face can see it.

   **The iterated region is the triangle dilated outward by two texels, not the bare
   triangle.** The texture is sampled with `LINEAR` filtering, so a bilinear fetch at a UV
   just *inside* the triangle reads a 2×2 neighbourhood reaching up to one texel *outside*
   it. Leaving those black bleeds a dark fringe into every face's edge — and only fixture A
   has a texture, so only fixture A would have the fringe, making it a visible divergence in
   exactly the place §3.5 requires the pair to be identical. Two texels rather than one: one
   is the strict bilinear footprint, the second is margin against UV-to-texel rounding at the
   boundary. The dilation must stay this small; it is filter correctness, not licence to
   iterate the square.
2. Write the model-view-projection matrix into mapped uniform memory the same way.
3. `vkCmdCopyBufferToImage` from the staging buffer to a device-local texture.
4. Draw 20 triangles with depth testing.
5. Read back the 256×256 result, write the PNG.

**The staging memory is `HOST_COHERENT` on purpose**, which means the program never calls
`vkFlushMappedMemoryRanges` — there is *no API call anywhere on the wire* announcing that
a megabyte of texture changed. This is the hardest case for Rayland, and it is also simply
what an ordinary Vulkan application does; those two facts together are the entire problem,
and the fixture's job is to state it in executable form rather than in prose.

### 7.3 `rayland-icosa-gpu`, per frame

1. Write the MVP matrix, zoom half-width, and centre into mapped uniform memory
   (~128 bytes).
2. Draw 20 triangles with depth testing; the fragment shader evaluates the fractal
   per-pixel from the interpolated UVs.
3. Read back, write the PNG.

### 7.4 The timing report

Both fixtures print one CSV line per frame to stdout:

```
frame,fractal_us,upload_us,draw_readback_us
```

This is ordinary profiling output. It measures the program's own work with the program's
own clock and mentions nothing about the environment; it is the kind of printout any
graphics program might carry. It is the narrow exception §2 reserves, and it earns its
place because the whole point of fixture A versus fixture B is to produce *numbers*, and a
fixture that produced only images would make someone add the timing later — probably as a
flag, which §2 forbids for good reason.

Note that the timing report uses a clock, while §5.1 forbids wall-clock. There is no
contradiction: the clock **measures** and never **decides**. No rendering behaviour, no
frame content, and no control flow may depend on a timing value. If timing ever feeds back
into what is drawn, determinism is gone and the fixtures are worthless.

---

## 8. Testing

### 8.1 `rayland-icosa-core` — unit tests, no GPU

- Geometry invariants (§4.1).
- `log2` bit-exactness against the committed table (§5.4).
- Schedule purity: `frame_orientation(i)` and `frame_zoom(i)` return equal values for
  equal `i`, across repeated calls and in any order.

### 8.2 Per fixture — `tests/native_render.rs`

Mirrors `rayland-refapp/tests/native_render.rs`: run the binary on the host's own driver
and assert known pixels on frame 0 and frame 119. Establishes that the picture is right
when nothing else is involved.

These two test files are near-identical, and are deliberately **not** factored into a
shared helper — the one place this design accepts duplication. A shared helper would have
to live in a crate both fixtures depend on, and a baseline test's entire job is to
establish "this program renders correctly with nothing else involved". A baseline that
imported shared machinery would be testing that machinery too, which is exactly what it
must not do.

### 8.3 Per fixture — e2e in `rayland-engine`

Mirrors `rayland-engine/tests/refapp_venus_e2e.rs`: run the same binary through Mesa's
Venus ICD into Rayland's engine onto S's GPU, and assert **all 120 PNGs are bit-identical**
to the native run. At 256×256 that is roughly 24 MiB of comparison — cheap enough that
comparing every frame beats sampling a few.

Comparing all 120 matters. A defect that corrupts one intermediate frame and then
self-corrects — a delta applied late, a texture upload racing a draw — is invisible to a
final-frame check and is exactly the sort of thing (c)1's relay and (c)2's coherence work
can produce.

---

## 9. Sequencing and expectations

These fixtures are expected to **fail** against Rayland when first run, and that is their
purpose. Likely order of breakage, most to least likely:

1. **Depth attachments** — the first ones in the project (§4).
2. **Per-frame mapped texture writes** — the (c)2 problem, unsolved by construction as of
   (c)1 (§7.2).
3. **Texture upload bandwidth** — the (c)3 problem; expected to be slow rather than wrong.

A failure in (1) is a gap in coverage. A failure in (2) is the design working as
currently specified — (c)1 does not claim to solve mapped memory — and the fixture's job
is to say so precisely and reproducibly rather than to pass.

Do not weaken the fixtures to make them pass. Their value is exactly that they do not.
