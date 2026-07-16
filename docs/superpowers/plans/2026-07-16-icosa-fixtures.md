# The icosahedron fixtures — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build two unmodified Vulkan applications — one computing a zooming Mandelbrot on the CPU into persistently-mapped memory, one computing it in a fragment shader — that texture a spinning icosahedron, so that Rayland finally has a workload which exercises the `vkMapMemory` problem (c)2 exists to solve.

**Architecture:** Three crates. `rayland-icosa-core` is a GPU-free library holding everything the two fixtures must agree on: the icosahedron geometry, the frame-indexed animation schedule, the Mandelbrot math, and bit-exact `log2`/`sin`/`cos`. `rayland-icosa-cpu` and `rayland-icosa-gpu` are ordinary offscreen Vulkan binaries built on it, differing *only* in where the fractal is evaluated and therefore in how many bytes per frame cross mapped memory (~1 MiB versus ~128 B).

**Tech Stack:** Rust 2024, `ash` 0.38 (Vulkan), `image` 0.25 (PNG), `anyhow` 1. GLSL compiled to SPIR-V with `glslangValidator` and committed, per `shaders/README.md`.

**The spec is [`docs/design/2026-07-16-icosa-fixtures.md`](../../design/2026-07-16-icosa-fixtures.md). Read it before starting.** It explains *why* each constant and constraint below exists; this plan only says *what* to build. The findings document it rests on is [`docs/design/2026-07-15-venus-ring-findings.md`](../../design/2026-07-15-venus-ring-findings.md) §6.

## Global Constraints

Every task's requirements implicitly include this section.

- **The fixtures must not know they are being remoted.** Zero `rayland-*` dependencies in any of the three crates, including their tests. No mention of Venus, vtest, virglrenderer, sockets, rings, blobs, or remoting in code, comments, docs, or metadata. No environment probing, no conditional rendering paths, no command-line rendering options. (Spec §2.)
- **No new flags, ever.** If a variant is needed it is a new binary with its own constants, not an option on an existing one. (Spec §2.)
- **Rust edition 2024, `rust-version = "1.85"`**, matching the workspace.
- **Licensing:** libraries LGPL-3.0-or-later, binaries GPL-3.0-or-later. All three crates `publish = false`, `version = "0.0.1"`.
- **`repository = "https://github.com/perpetualbits/rayland"`** in every manifest.
- **Comment discipline (`CLAUDE.md`):** a doc-comment block on every function, type, trait and module describing what it does, its inputs, outputs, failure modes and domain pitfalls; an intent comment on every non-trivial line explaining *why*, never restating syntax. Code and comments must always agree.
- **No Claude/AI attribution** anywhere in code, comments, docs, or commit messages.
- **Determinism is the product.** No `Instant::now()`, no wall-clock, no `powi`, no libm transcendental (`log`, `ln`, `sin`, `cos`, `powf`, `exp`) in any code path that affects a pixel. Only IEEE `+ - * /`, comparisons, `round()`, and bit manipulation. This is not stylistic; see spec §5.4 and the note in Task 2.
- **Fixed constants**, shared from `rayland-icosa-core`, identical in both fixtures:
  - `FRAME_COUNT = 120`
  - `IMAGE_SIZE = 256` (render target, square)
  - `TEXTURE_SIZE = 512` (fractal texture, square, RGBA8)
  - `MAX_ITER = 512`
  - `ZOOM_PER_FRAME = 0.97`, `INITIAL_HALF_WIDTH = 1.5`
  - `CENTER = (-0.743643887037151, 0.13182590420533)`

## A note on code completeness in this plan

Tasks 1–4 give complete code: the math is exactly specifiable and its exactness is the point. Tasks 5–7 give complete code for everything *novel* (depth attachment, persistent mapping, texture upload, the frame loop) and, for Vulkan boilerplate that already exists in this repository, direct the implementer to `crates/rayland-refapp/src/{context,pipeline,render}.rs` as the pattern to copy and adapt. That is deliberate: reproducing 600 lines of instance/device/queue bring-up here would be a worse instruction than "do it exactly like the file next door does it", and the refapp version is already reviewed and working.

## File Structure

| File | Responsibility |
|---|---|
| `crates/rayland-icosa-core/Cargo.toml` | Manifest. No dependencies at all. |
| `crates/rayland-icosa-core/src/lib.rs` | Crate docs; re-exports; the shared constants. |
| `crates/rayland-icosa-core/src/exact_math.rs` | Bit-exact `log2`, `sin_cos`. Nothing else. |
| `crates/rayland-icosa-core/src/geometry.rs` | The 60-vertex icosahedron table and its `Vertex` type. |
| `crates/rayland-icosa-core/src/schedule.rs` | `frame_orientation`, `frame_zoom`, the MVP matrix. |
| `crates/rayland-icosa-core/src/fractal.rs` | Mandelbrot smooth-iteration + HSV→RGB; `render_fractal_into`. |
| `crates/rayland-icosa-core/tests/log2_table.rs` | The committed bit-exactness contract for `log2`. |
| `crates/rayland-icosa-core/tests/sin_cos_table.rs` | The committed bit-exactness contract for `sin_cos`. |
| `crates/rayland-icosa-cpu/Cargo.toml` | Fixture A manifest. |
| `crates/rayland-icosa-cpu/src/main.rs` | Fixture A: constants, arg parsing, the frame loop, CSV. |
| `crates/rayland-icosa-cpu/src/context.rs` | Vulkan bring-up + memory allocation (adapted from refapp). |
| `crates/rayland-icosa-cpu/src/pipeline.rs` | Render pass with depth, graphics pipeline, descriptor layout. |
| `crates/rayland-icosa-cpu/src/render.rs` | Targets, host buffers, the persistent map, texture upload, draw. |
| `crates/rayland-icosa-cpu/tests/native_render.rs` | Fixture A's baseline on the host's own driver. |
| `crates/rayland-icosa-gpu/…` | Fixture B, same shape as A. |
| `shaders/icosa.vert`, `icosa_textured.frag`, `icosa_fractal.frag` (+ `.spv`) | GLSL sources and committed SPIR-V. |
| `crates/rayland-engine/tests/icosa_cpu_venus_e2e.rs` | Fixture A's end-to-end proof. |
| `crates/rayland-engine/tests/icosa_gpu_venus_e2e.rs` | Fixture B's end-to-end proof. |

---

### Task 1: `rayland-icosa-core` skeleton and bit-exact `log2`

**Files:**
- Create: `crates/rayland-icosa-core/Cargo.toml`
- Create: `crates/rayland-icosa-core/src/lib.rs`
- Create: `crates/rayland-icosa-core/src/exact_math.rs`
- Test: `crates/rayland-icosa-core/tests/log2_table.rs`
- Modify: `Cargo.toml` (workspace `members`)
- Modify: `CLAUDE.md` (crate count and crate list)

**Interfaces:**
- Consumes: nothing.
- Produces: `rayland_icosa_core::exact_math::log2(x: f64) -> f64`.

**Why this exists, for the implementer:** `log2` here is *not* about accuracy. Rust's `f64::log2` is more accurate than what you are about to write. It is about **reproducibility across machines**: IEEE-754 exactly specifies `+ - * /` and square root, but says nothing about transcendentals, and implementations legitimately differ in the last bits. The application runs on machine C, which may be x86 or RISC-V; the native baseline runs it on x86. If the fractal depended on the host libm, the two runs would differ in a handful of pixels and the failure would look exactly like a Rayland bug while being nothing of the sort. Rust never auto-contracts expressions into FMA, so an expression built only from IEEE basic operations evaluates bit-identically on any IEEE host. That is the whole trick.

- [ ] **Step 1: Write the failing test**

Create `crates/rayland-icosa-core/tests/log2_table.rs`:

```rust
//! The bit-exactness contract for [`rayland_icosa_core::exact_math::log2`].
//!
//! # What this test is really asserting
//! Not that `log2` is *accurate* — it is deliberately less accurate than the standard library's.
//! That it is **reproducible**: that this function, built only from IEEE-754 basic operations,
//! returns the identical bit pattern on every host it is ever compiled for. The table below is
//! the contract. It was generated once, on one machine, and committed. If a refactor changes a
//! single bit of a single entry, this test fails — which is the intent, because a single changed
//! bit is exactly what would later show up as an inexplicable one-pixel diff in an end-to-end
//! render test and cost someone a day.
//!
//! # Regenerating the table
//! Don't, unless the polynomial itself is deliberately being changed. If it is: print
//! `log2(x).to_bits()` for each input below and paste the results in. Then be aware that every
//! committed reference image downstream shifts too.

use rayland_icosa_core::exact_math::log2;

/// Inputs and their required exact results, as raw `f64` bit patterns.
///
/// The inputs are chosen to cover the shape of the function rather than to be pretty: exact powers
/// of two (where the mantissa polynomial contributes nothing and only the exponent path runs),
/// values just above and just below them (where the exponent decomposition switches over), and a
/// spread of ordinary values across the range the fractal actually feeds it — `log2(|z|)` for `|z|`
/// just past the escape radius of 2, up to very large escapees.
const CASES: &[(f64, u64)] = &[
    (1.0, 0x0000000000000000),
    (2.0, 0x3ff0000000000000),
    (4.0, 0x4000000000000000),
    (0.5, 0xbff0000000000000),
    // Placeholder bit patterns for the non-exact cases; see Step 3 for how these are filled.
    (3.0, 0x0000000000000000),
    (1.5, 0x0000000000000000),
    (10.0, 0x0000000000000000),
    (1e300, 0x0000000000000000),
];

/// Every case must reproduce its committed bit pattern exactly.
#[test]
fn log2_matches_the_committed_bit_patterns() {
    for &(input, expected_bits) in CASES {
        let got = log2(input);
        assert_eq!(
            got.to_bits(),
            expected_bits,
            "log2({input}) must be bit-exact: got {got} ({:#018x}), want {:#018x}",
            got.to_bits(),
            expected_bits
        );
    }
}

/// The exact powers of two must be exactly right, not merely reproducible.
///
/// This is a separate check because it is the one place where the function has a *correct* answer
/// that is representable, and getting it wrong would mean the exponent decomposition is broken —
/// a bug the table alone could not distinguish from a deliberate polynomial change.
#[test]
fn log2_of_exact_powers_of_two_is_exact() {
    for exponent in -60i32..=60 {
        let x = f64::powi(2.0, exponent);
        assert_eq!(
            log2(x),
            exponent as f64,
            "log2(2^{exponent}) must be exactly {exponent}"
        );
    }
}

/// Non-positive inputs are outside the domain and must be reported, not silently wrong.
///
/// The fractal never calls this with such a value (it feeds `|z|` after escape, always > 2), but a
/// silent NaN propagating into a pixel would be far harder to diagnose than a loud one.
#[test]
fn log2_of_non_positive_is_nan() {
    assert!(log2(0.0).is_nan(), "log2(0) must be NaN");
    assert!(log2(-1.0).is_nan(), "log2(-1) must be NaN");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rayland-icosa-core`
Expected: FAIL — the package does not exist yet (`error: package ID specification ... did not match any packages`).

- [ ] **Step 3: Write the crate and the implementation**

Create `crates/rayland-icosa-core/Cargo.toml`:

```toml
# Everything the two icosahedron fixtures must agree on, and nothing that touches a GPU.
#
# The two fixtures exist to be compared against each other, and that comparison is only meaningful
# if they are identical in every respect except the one under study. Two copies of this code would
# drift — someone would fix a rounding detail in one and not the other — and the moment they drift
# the comparison stops measuring what it claims to. Sharing the code is what makes the pair an
# instrument rather than two programs.
#
# This crate has NO dependencies, deliberately. Its correctness is mathematical, and it should be
# testable on a machine with no GPU, no driver and no display.
#
# A LIBRARY crate, so per the repository license policy it is LGPL-3.0-or-later.
[package]
name = "rayland-icosa-core"
version = "0.0.1"
edition = "2024"
rust-version = "1.85"
description = "Shared geometry, animation schedule and reproducible fractal math for the icosahedron fixtures."
license = "LGPL-3.0-or-later"
repository = "https://github.com/perpetualbits/rayland"
publish = false                # a test fixture's support library; nothing here belongs on crates.io

[dependencies]
# None, and none may be added without a very good reason. See the crate note above.
```

Create `crates/rayland-icosa-core/src/lib.rs`:

```rust
//! Shared foundations for a pair of ordinary Vulkan programs that draw a spinning, fractal-textured
//! icosahedron.
//!
//! # What lives here and why it is shared
//! Two programs draw this scene. One computes the fractal on the CPU; one computes it in a fragment
//! shader. Their whole purpose is to be compared, and a comparison between two programs is only
//! informative if they are identical in everything but the single property being studied. So every
//! decision they must agree on — the geometry, the animation schedule, the fractal's arithmetic —
//! lives here, in one place, and neither program is permitted its own copy.
//!
//! # This crate never touches a GPU
//! No `ash`, no Vulkan, no image encoding, no I/O. Its correctness is arithmetic, and it is
//! testable on a machine with no graphics stack installed at all. Keep it that way.
//!
//! # Determinism is the product
//! Every function here that can affect a pixel is built exclusively from IEEE-754 basic operations
//! (`+ - * /`, comparison, `round`) and bit manipulation. None of them calls a libm transcendental.
//! See [`exact_math`] for why that constraint exists; it is the single most surprising thing about
//! this crate and the easiest to accidentally undo.

// Reproducible replacements for the libm functions the rest of the crate would otherwise need.
pub mod exact_math;

/// The number of frames a fixture run renders.
///
/// Fixed, and not configurable. A fixture with a `--frames` option is a fixture with an opinion
/// about how it is run, and the comparison between the two fixtures silently stops being
/// controlled the first time someone runs them with different values.
pub const FRAME_COUNT: u32 = 120;

/// The rendered image's edge length in pixels; the images are square.
///
/// Large enough that a shaded solid has an unambiguous interior and silhouette for a test to
/// check, small enough that 120 readbacks and 120 exact image comparisons stay cheap.
pub const IMAGE_SIZE: u32 = 256;

/// The fractal texture's edge length in pixels; the texture is square, RGBA8.
///
/// 512×512×4 bytes is exactly 1 MiB per frame. That is the number this whole exercise is about:
/// enough traffic to be honest about what per-frame texture upload costs, while staying under the
/// 8 MiB ceiling the surrounding system imposes on a single buffer, so that the fixture stresses
/// the interesting thing rather than tripping over an unrelated limit.
pub const TEXTURE_SIZE: u32 = 512;

/// The Mandelbrot escape-iteration ceiling.
///
/// 512, not the 2000 a GPU-only interactive program can afford. The CPU fixture evaluates this
/// loop for every one of 512×512 pixels every frame, on a machine that may be a modest
/// single-board computer, and 512 keeps that worst case near 134 million iterations per frame:
/// heavy enough to be honest about a weak CPU, light enough not to swamp the measurement. It is
/// also ample detail at the zoom depth this run reaches.
///
/// **Both fixtures use this same value.** Giving the GPU one a different ceiling because it can
/// afford one would destroy the only thing the pair is for.
pub const MAX_ITER: u32 = 512;
```

Create `crates/rayland-icosa-core/src/exact_math.rs`:

```rust
//! Reproducible replacements for the libm functions this crate would otherwise call.
//!
//! # Why this module exists at all
//! IEEE-754 exactly specifies the results of `+`, `-`, `*`, `/` and square root: given the same
//! inputs and the same rounding mode, every conforming machine produces the identical bit pattern.
//! It specifies **nothing** about transcendentals. `log`, `sin` and `cos` are library code, and
//! different libm implementations on different architectures legitimately differ in the last bits
//! of their results.
//!
//! That is normally a non-issue, and normally you should just call `f64::log2`. It is an issue here
//! because of what these numbers are for. The application that uses them runs on one machine; the
//! baseline it is compared against runs the same program on a *different* machine, possibly of a
//! different architecture. Every pixel of both runs must match exactly, because the entire point of
//! that comparison is that any difference at all indicates a defect elsewhere in the system. A
//! handful of last-bit libm differences would masquerade as such a defect and send someone hunting
//! for a bug that does not exist.
//!
//! So: these functions are built exclusively from IEEE basic operations and bit manipulation. Rust
//! does not automatically contract expressions into fused multiply-add, so an expression written
//! that way evaluates bit-identically everywhere.
//!
//! # These are less accurate than the standard library, on purpose
//! Each is a truncated series good to roughly 1e-9, against a standard library that is
//! correctly-rounded to the last bit. That trade is correct here: the results drive an 8-bit colour
//! channel and a rotation matrix, where 1e-9 is invisible, while reproducibility is load-bearing.
//! **A visually fine approximation that is bit-exact everywhere is strictly better here than a
//! perfect one that is not.**
//!
//! # Pitfall
//! Do not "improve" anything here by reaching for a standard-library transcendental, and do not
//! rewrite an expression into a form a compiler might fuse. Both changes look like cleanups and
//! both silently reintroduce exactly the cross-machine divergence this module exists to prevent.
//! The tests in `tests/log2_table.rs` and `tests/sin_cos_table.rs` are what stand in the way.

/// `2 / ln(2)`, the constant that converts the natural-log series below into a base-2 logarithm.
///
/// Written as a literal rather than computed, so that it is one exactly-specified `f64` on every
/// machine rather than the output of a library call.
const TWO_OVER_LN2: f64 = 2.885390081777926814;

/// Base-2 logarithm, reproducible bit-for-bit on any IEEE-754 host.
///
/// # How it works
/// Any positive `f64` is `m * 2^e` with `m` in `[1, 2)`, and that split is *exact* — it is just
/// reading the exponent and mantissa fields out of the bit pattern. So
/// `log2(x) = e + log2(m)`, and only `log2(m)` over the narrow range `[1, 2)` needs approximating.
///
/// For that, substitute `t = (m - 1) / (m + 1)`, which maps `[1, 2)` onto `[0, 1/3)`, and use the
/// classical odd series `ln(m) = 2 * (t + t³/3 + t⁵/5 + …)`. Because `t` never exceeds 1/3 the
/// series converges quickly: truncating after the `t¹⁵` term leaves an error near 1e-9, which is
/// far below the resolution of anything downstream.
///
/// # Inputs and outputs
/// `x` must be positive and finite. Returns `log2(x)`.
///
/// # Failure modes
/// Returns NaN for zero, negative inputs and NaN. Returns positive infinity for positive infinity.
/// Subnormal inputs are **not** handled correctly — their exponent field is zero and the
/// decomposition below would misread them. This is acceptable because the only caller feeds `|z|`
/// after Mandelbrot escape, which always exceeds 2; if that ever changes, this must too.
pub fn log2(x: f64) -> f64 {
    // The domain guard. Zero and negatives have no real logarithm, and a silent NaN produced deep
    // inside a pixel loop is far harder to trace than one produced here at the boundary.
    if !(x > 0.0) {
        return f64::NAN;
    }
    // Infinity has no finite decomposition, and the bit manipulation below would produce garbage
    // from its exponent field rather than the mathematically right answer.
    if x.is_infinite() {
        return f64::INFINITY;
    }

    let bits = x.to_bits();
    // The stored exponent is biased by 1023; subtracting the bias recovers the true power of two.
    // This is exact — no rounding is involved in reading a field out of a bit pattern.
    let exponent = (((bits >> 52) & 0x7ff) as i64 - 1023) as f64;
    // Replace the exponent field with the bias itself, which forces the value into `[1, 2)` while
    // preserving every mantissa bit: this is the `m` of `x = m * 2^e`, recovered exactly.
    let mantissa = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000);

    // The substitution that shrinks the approximation's range from [1, 2) to [0, 1/3), which is
    // what makes so few series terms sufficient.
    let t = (mantissa - 1.0) / (mantissa + 1.0);
    let t2 = t * t;

    // The odd series in Horner form, from the smallest term inward. Horner is used because it is
    // both accurate and, more importantly here, a fixed sequence of multiplies and adds with no
    // opportunity for a compiler to reassociate into something machine-dependent.
    let mut poly = 1.0 / 15.0;
    poly = poly * t2 + 1.0 / 13.0;
    poly = poly * t2 + 1.0 / 11.0;
    poly = poly * t2 + 1.0 / 9.0;
    poly = poly * t2 + 1.0 / 7.0;
    poly = poly * t2 + 1.0 / 5.0;
    poly = poly * t2 + 1.0 / 3.0;
    poly = poly * t2 + 1.0;

    // `exponent` contributes the integer part exactly; the series supplies the fraction. For exact
    // powers of two the mantissa is exactly 1, so `t` is exactly 0 and this whole term vanishes —
    // which is why `log2(2^n)` comes out exactly `n`.
    exponent + TWO_OVER_LN2 * t * poly
}
```

Add to `crates/rayland-icosa-core/src/lib.rs` — already done above via `pub mod exact_math;`.

Add the crate to the workspace `members` list in the root `Cargo.toml`, immediately after the `rayland-refapp` line:

```toml
    "crates/rayland-icosa-core",     # shared geometry/schedule/fractal math for the icosahedron fixtures
```

- [ ] **Step 4: Fill in the table's real bit patterns**

The four non-exact cases in `CASES` were committed as placeholder zeros. Print the real values and paste them in — this is the one time the table is allowed to be written from the implementation rather than the other way round, because the table's job is to pin *reproducibility*, and there is nothing to pin until the polynomial exists.

Run:

```bash
cd /home/roland/git/rayland && cat > /tmp/gen_log2_table.rs <<'EOF'
fn main() {
    for x in [3.0f64, 1.5, 10.0, 1e300] {
        println!("({x:e}, {:#018x}),", rayland_icosa_core::exact_math::log2(x).to_bits());
    }
}
EOF
cargo run -q -p rayland-icosa-core --example gen_log2_table 2>/dev/null \
  || echo "Use a temporary #[test] that prints instead; delete it afterwards."
```

Simpler and preferred: add a temporary `#[test] fn print() { … }` in `exact_math.rs` that prints the four bit patterns, run `cargo test -p rayland-icosa-core print -- --nocapture`, paste the output into `CASES`, and **delete the temporary test**.

Sanity-check before pasting: `log2(3.0)` must be ≈ 1.585, `log2(1.5)` ≈ 0.585, `log2(10.0)` ≈ 3.322, `log2(1e300)` ≈ 996.578. If any is wildly off, the polynomial is wrong and the table would enshrine the bug.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p rayland-icosa-core`
Expected: PASS — `log2_matches_the_committed_bit_patterns`, `log2_of_exact_powers_of_two_is_exact`, `log2_of_non_positive_is_nan`.

- [ ] **Step 6: Update `CLAUDE.md`**

`CLAUDE.md` says "A Cargo workspace of twelve crates." That is now false, and `CLAUDE.md`'s own rule says a change that falsifies it must fix it in the same change. Change "twelve" to "thirteen" and add to the crate list, after the `rayland-refapp` entry:

```markdown
- **`crates/rayland-icosa-core`** — shared foundations for the icosahedron fixtures: the geometry,
  the frame-indexed animation schedule, the Mandelbrot math, and the bit-exact `log2`/`sin`/`cos`
  those rest on. **No dependencies at all, and never touches a GPU** — its correctness is
  arithmetic. Its reason for existing is that the two fixtures must be identical in everything but
  the property under study, and two copies of this code would drift. LGPL, `publish = false`.
```

(Later tasks bring the count to fourteen and fifteen; each fixes it in its own change.)

- [ ] **Step 7: Commit**

```bash
git add crates/rayland-icosa-core Cargo.toml CLAUDE.md
git commit -m "icosa Task 1: icosa-core skeleton + bit-exact log2

The fixture's fractal must produce identical pixels on x86 and RISC-V alike:
its native baseline runs the app on one machine and the remoted run on
another, and every pixel of the two must match, because a difference is
supposed to mean a defect. libm transcendentals are not IEEE-specified and
would differ in the last bits, so log2 is rebuilt from IEEE basic operations
only — exponent decomposition plus a truncated odd series. Less accurate than
the standard library, and reproducible, which is the trade that matters here.

The committed bit-pattern table is the contract."
```

---

### Task 2: Bit-exact `sin`/`cos`

**Files:**
- Modify: `crates/rayland-icosa-core/src/exact_math.rs`
- Test: `crates/rayland-icosa-core/tests/sin_cos_table.rs`

**Interfaces:**
- Consumes: nothing from Task 1 (same module, independent function).
- Produces: `rayland_icosa_core::exact_math::sin_cos(x: f64) -> (f64, f64)` returning `(sin(x), cos(x))`.

**Why this exists:** the same argument as `log2`, applied to the rotation matrix. The spec's §5.4 named `log` only; that was an oversight. The animation's orientation is computed on machine C and fed to the GPU as a matrix, so if `sin`/`cos` came from libm the matrix would differ between an x86 C and a RISC-V C, every pixel of the spinning solid would shift, and the spec's §5.5 claim — that any pixel difference is a Rayland defect — would simply be false. Both must be exact for that claim to hold.

- [ ] **Step 1: Write the failing test**

Create `crates/rayland-icosa-core/tests/sin_cos_table.rs`:

```rust
//! The bit-exactness contract for [`rayland_icosa_core::exact_math::sin_cos`].
//!
//! # What this test is really asserting
//! The same thing `log2_table.rs` asserts, for the same reason: not accuracy, but that this
//! function returns the identical bit pattern on every host. The rotation matrix of the spinning
//! solid is built from these values on the machine the application runs on. Two runs of the same
//! program on two different architectures must produce the same picture down to the last bit, or
//! the comparison the whole fixture exists to support is worthless.
//!
//! # Regenerating the table
//! Don't, unless the kernels are deliberately being changed — every committed reference image
//! downstream shifts with it.

use rayland_icosa_core::exact_math::sin_cos;

/// How far a result may drift from the standard library's and still be considered *correct*.
///
/// This is a separate concept from bit-exactness and must not be confused with it. The table below
/// pins reproducibility to the last bit; this tolerance checks that the reproducible answer is also
/// the *right* answer to within the truncated series' error. Both matter: a function could be
/// perfectly reproducible and perfectly wrong.
const ACCURACY_TOLERANCE: f64 = 1e-9;

/// Inputs spanning every quadrant, plus the reduction's edges.
///
/// Chosen for the shape of the algorithm, not prettiness: zero (where the kernels' leading terms
/// are all that survive), values inside each of the four quadrants the range reduction selects,
/// values very near the quadrant boundaries (where an off-by-one in the quadrant index shows up as
/// a swapped sine and cosine), negatives, and a large multiple of pi where the reduction has to do
/// real work and cancellation is worst.
const INPUTS: &[f64] = &[
    0.0, 0.5, 1.0, 1.5707963267948966, 2.0, 3.0, 3.141592653589793, 4.0, 5.0, 6.283185307179586,
    -0.5, -2.0, -7.0, 50.0, 119.0,
];

/// Every input must reproduce its committed bit patterns exactly.
///
/// The table is filled in at implementation time (see the plan's Step 4) and is the contract from
/// then on.
#[test]
fn sin_cos_matches_the_committed_bit_patterns() {
    // (input, sin bits, cos bits) — filled in from the implementation, then frozen.
    const CASES: &[(f64, u64, u64)] = &[];
    assert!(
        !CASES.is_empty(),
        "the bit-pattern table must be filled in; see the plan's Task 2 Step 4"
    );
    for &(input, want_sin, want_cos) in CASES {
        let (got_sin, got_cos) = sin_cos(input);
        assert_eq!(
            got_sin.to_bits(),
            want_sin,
            "sin({input}) must be bit-exact: got {got_sin}"
        );
        assert_eq!(
            got_cos.to_bits(),
            want_cos,
            "cos({input}) must be bit-exact: got {got_cos}"
        );
    }
}

/// The reproducible answer must also be the right answer.
///
/// Compared against the standard library, which is correctly-rounded and therefore a fair oracle
/// for accuracy even though it is unusable as an implementation here.
#[test]
fn sin_cos_is_accurate_enough() {
    for &x in INPUTS {
        let (s, c) = sin_cos(x);
        assert!(
            (s - x.sin()).abs() < ACCURACY_TOLERANCE,
            "sin({x}): got {s}, libm says {}",
            x.sin()
        );
        assert!(
            (c - x.cos()).abs() < ACCURACY_TOLERANCE,
            "cos({x}): got {c}, libm says {}",
            x.cos()
        );
    }
}

/// The Pythagorean identity must hold, which catches a broken quadrant selection that the accuracy
/// check could miss if both kernels drifted together.
#[test]
fn sin_cos_satisfies_the_pythagorean_identity() {
    for &x in INPUTS {
        let (s, c) = sin_cos(x);
        assert!(
            (s * s + c * c - 1.0).abs() < ACCURACY_TOLERANCE,
            "sin²+cos² must be 1 at {x}; got {}",
            s * s + c * c
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rayland-icosa-core --test sin_cos_table`
Expected: FAIL to compile — `sin_cos` is not defined in `exact_math`.

- [ ] **Step 3: Write the implementation**

Append to `crates/rayland-icosa-core/src/exact_math.rs`:

```rust
/// `2 / π`, used to find how many quarter-turns to subtract during range reduction.
const TWO_OVER_PI: f64 = 0.6366197723675814;

/// The high half of `π / 2`, exact in `f64` with its low mantissa bits deliberately zeroed.
///
/// Splitting `π/2` across two constants is the classical Cody-Waite trick: `k * PI_OVER_2_HI` is
/// then computed *exactly* (no rounding, because `HI` has enough trailing zero bits to absorb the
/// multiplication for the small `k` this crate ever uses), so the subtraction below loses no
/// precision to cancellation. `PI_OVER_2_LO` then supplies the bits `HI` omitted.
const PI_OVER_2_HI: f64 = 1.5707963267341256;

/// The low half of `π / 2` — the part [`PI_OVER_2_HI`] left out.
const PI_OVER_2_LO: f64 = 6.077100506506192e-11;

/// Sine and cosine together, reproducible bit-for-bit on any IEEE-754 host.
///
/// # How it works
/// Both functions are only cheaply approximable near zero, so the argument is first *range
/// reduced*: find the nearest multiple `k` of `π/2`, subtract it to leave a remainder `r` in
/// roughly `[-π/4, π/4]`, and remember `k mod 4` — the quadrant. Sine and cosine of the original
/// argument are then sine and cosine of `r`, possibly swapped and possibly negated, according to
/// the quadrant. Over `[-π/4, π/4]` the Taylor series for both converge fast enough that
/// truncating leaves an error near 1e-11.
///
/// The subtraction is done in two steps against a split `π/2` (Cody-Waite) rather than one, because
/// a single-constant subtraction would lose most of its significant bits to cancellation for larger
/// arguments and the resulting angle error would be plainly visible as a wobble in the animation.
///
/// # Inputs and outputs
/// `x` in radians. Returns `(sin(x), cos(x))`. Both are returned together because the caller always
/// wants both and the expensive part — the range reduction — is shared.
///
/// # Failure modes
/// Accurate for `|x|` up to a few hundred radians, which is all this crate needs (the largest angle
/// any frame produces is well under 100). For very large arguments the two-constant reduction is
/// insufficient and the result degrades; it does not signal this. NaN and infinite inputs produce
/// NaN.
pub fn sin_cos(x: f64) -> (f64, f64) {
    // A non-finite angle has no meaningful sine, and the reduction below would produce nonsense
    // rather than propagate the NaN cleanly.
    if !x.is_finite() {
        return (f64::NAN, f64::NAN);
    }

    // How many quarter-turns away from zero the argument is. `round` is `roundToIntegralTiesAway`,
    // which IEEE-754 *does* exactly specify — unlike the transcendentals — so this is reproducible.
    let k = (x * TWO_OVER_PI).round();
    // The two-step Cody-Waite subtraction. Written as two separate subtractions on purpose: fusing
    // them into one expression would let the intermediate be rounded once instead of twice and
    // change the result.
    let r = (x - k * PI_OVER_2_HI) - k * PI_OVER_2_LO;

    // Which quadrant the original angle fell in. `rem_euclid` gives a non-negative result for
    // negative `k`, unlike `%`, so the match below needs no sign special-casing.
    let quadrant = (k as i64).rem_euclid(4);

    let r2 = r * r;

    // sin(r) = r - r³/3! + r⁵/5! - r⁷/7! + r⁹/9! - r¹¹/11!, in Horner form over r².
    let mut sin_poly = -1.0 / 39916800.0; // -1/11!
    sin_poly = sin_poly * r2 + 1.0 / 362880.0; // +1/9!
    sin_poly = sin_poly * r2 - 1.0 / 5040.0; // -1/7!
    sin_poly = sin_poly * r2 + 1.0 / 120.0; // +1/5!
    sin_poly = sin_poly * r2 - 1.0 / 6.0; // -1/3!
    sin_poly = sin_poly * r2 + 1.0; // +1
    let sin_r = r * sin_poly;

    // cos(r) = 1 - r²/2! + r⁴/4! - r⁶/6! + r⁸/8! - r¹⁰/10! + r¹²/12!, in Horner form over r².
    let mut cos_r = 1.0 / 479001600.0; // +1/12!
    cos_r = cos_r * r2 - 1.0 / 3628800.0; // -1/10!
    cos_r = cos_r * r2 + 1.0 / 40320.0; // +1/8!
    cos_r = cos_r * r2 - 1.0 / 720.0; // -1/6!
    cos_r = cos_r * r2 + 1.0 / 24.0; // +1/4!
    cos_r = cos_r * r2 - 1.0 / 2.0; // -1/2!
    cos_r = cos_r * r2 + 1.0; // +1

    // Rotate the (sin, cos) pair into the quadrant the reduction took us out of. Each arm is the
    // standard identity for adding a multiple of π/2; getting one wrong swaps or flips the
    // animation in a way the identity test alone would not catch, which is why the table test
    // covers every quadrant explicitly.
    match quadrant {
        0 => (sin_r, cos_r),
        1 => (cos_r, -sin_r),
        2 => (-sin_r, -cos_r),
        _ => (-cos_r, sin_r),
    }
}
```

- [ ] **Step 4: Fill in the table's real bit patterns**

Add a temporary `#[test]` in `exact_math.rs` that prints `(input, sin_bits, cos_bits)` for every value in the test's `INPUTS`, run it with `-- --nocapture`, paste the result into `CASES` in `tests/sin_cos_table.rs`, and **delete the temporary test**.

Sanity-check before pasting: `sin(0)` must be exactly `0.0` (bits `0x0`), `cos(0)` exactly `1.0` (bits `0x3ff0000000000000`), and `sin(π/2)` ≈ 1.0. If `sin` and `cos` look swapped at any input, the quadrant arms are wrong — fix that before freezing the table, because the table would otherwise enshrine the bug forever.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p rayland-icosa-core`
Expected: PASS — all six tests across both table files.

- [ ] **Step 6: Commit**

```bash
git add crates/rayland-icosa-core
git commit -m "icosa Task 2: bit-exact sin/cos

The spec made log2 reproducible and left the rotation matrix on libm's sin
and cos, which is the identical trap: the matrix is computed on the machine
the app runs on, so an x86 C and a RISC-V C would place the solid at
minutely different angles and every pixel would differ. The claim that any
pixel difference indicates a defect elsewhere only holds if both are exact.

Cody-Waite range reduction against a split pi/2, then truncated Taylor
kernels — IEEE basic operations and round() only, no libm."
```

---

### Task 3: The icosahedron geometry

**Files:**
- Create: `crates/rayland-icosa-core/src/geometry.rs`
- Modify: `crates/rayland-icosa-core/src/lib.rs` (add `pub mod geometry;`)
- Test: inline `#[cfg(test)]` module in `geometry.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `rayland_icosa_core::geometry::Vertex { position: [f32; 3], normal: [f32; 3], uv: [f32; 2] }` — `#[repr(C)]`, `Copy`.
  - `rayland_icosa_core::geometry::icosahedron() -> [Vertex; 60]`.

The tests live inline rather than in `tests/` because they assert invariants of a private construction (the golden-ratio table and its face list), and an integration test would only be able to check the public output — which is what the *public* invariants below do anyway. Both are here; keeping them together is what `CLAUDE.md`'s "files that change together live together" asks for.

- [ ] **Step 1: Write the failing test**

Create `crates/rayland-icosa-core/src/geometry.rs` containing *only* the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The solid must have exactly the 20 faces of an icosahedron, unshared.
    ///
    /// 60 = 20 faces × 3 vertices. The vertices are deliberately *not* shared between faces even
    /// though only 12 distinct positions exist, because each face needs its own flat normal — see
    /// the module documentation.
    #[test]
    fn has_twenty_faces_of_three_unshared_vertices() {
        assert_eq!(icosahedron().len(), 60, "20 triangular faces × 3 vertices");
    }

    /// Those 60 vertices must collapse to exactly 12 distinct positions.
    ///
    /// This is what makes it an icosahedron rather than 20 unrelated triangles: the faces really do
    /// share corners geometrically, even though the vertex buffer repeats them. Compared exactly
    /// (via bit patterns) rather than with a tolerance, because the generator emits each position
    /// from the identical expression every time — any drift would mean the table is not doing what
    /// it claims.
    #[test]
    fn has_twelve_distinct_positions() {
        let mut distinct: Vec<[u32; 3]> = icosahedron()
            .iter()
            .map(|v| [v.position[0].to_bits(), v.position[1].to_bits(), v.position[2].to_bits()])
            .collect();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 12, "an icosahedron has 12 corners");
    }

    /// Every edge must have the same length — the definition of *regular*.
    ///
    /// A tolerance is used here, unlike elsewhere in this crate: the positions come from a
    /// normalisation involving a square root, and the three edges of a face are computed from
    /// different coordinate pairs, so they agree mathematically but not bit-for-bit. This is a
    /// statement about geometry, not about reproducibility, and 1e-6 is far tighter than any
    /// construction error would be while being loose enough to survive that.
    #[test]
    fn all_edges_have_equal_length() {
        let verts = icosahedron();
        let length = |a: [f32; 3], b: [f32; 3]| {
            let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
        };
        let reference = length(verts[0].position, verts[1].position);
        for face in verts.chunks_exact(3) {
            for (a, b) in [(0, 1), (1, 2), (2, 0)] {
                let edge = length(face[a].position, face[b].position);
                assert!(
                    (edge - reference).abs() < 1e-6,
                    "every edge must be {reference}; found {edge}"
                );
            }
        }
    }

    /// Every face normal must point away from the centre.
    ///
    /// The solid is centred on the origin, so a face's centroid *is* its outward direction. A
    /// normal pointing inward means the face's winding order is reversed, which the GPU would
    /// silently back-face cull — the face would simply vanish from the render, and diagnosing one
    /// missing triangle out of 20 from a picture is far more work than this test.
    #[test]
    fn all_normals_point_outward() {
        for (index, face) in icosahedron().chunks_exact(3).enumerate() {
            let centroid = [
                (face[0].position[0] + face[1].position[0] + face[2].position[0]) / 3.0,
                (face[0].position[1] + face[1].position[1] + face[2].position[1]) / 3.0,
                (face[0].position[2] + face[1].position[2] + face[2].position[2]) / 3.0,
            ];
            let n = face[0].normal;
            let dot = n[0] * centroid[0] + n[1] * centroid[1] + n[2] * centroid[2];
            assert!(dot > 0.0, "face {index}'s normal points inward (dot = {dot})");
        }
    }

    /// All three vertices of a face must share one normal — that is what makes the face flat.
    #[test]
    fn each_face_has_one_flat_normal() {
        for (index, face) in icosahedron().chunks_exact(3).enumerate() {
            assert_eq!(face[0].normal, face[1].normal, "face {index} is not flat");
            assert_eq!(face[1].normal, face[2].normal, "face {index} is not flat");
        }
    }

    /// Every face must carry the same inscribed-triangle UVs, so every face shows the same image.
    #[test]
    fn every_face_carries_the_same_uv_triangle() {
        for (index, face) in icosahedron().chunks_exact(3).enumerate() {
            assert_eq!(face[0].uv, FACE_UVS[0], "face {index} corner 0");
            assert_eq!(face[1].uv, FACE_UVS[1], "face {index} corner 1");
            assert_eq!(face[2].uv, FACE_UVS[2], "face {index} corner 2");
        }
    }

    /// The triangle's own corners and centroid must be inside it, and the square's corners outside.
    ///
    /// The square's corners are the specific case that matters: they are the region the fractal must
    /// *not* iterate, and they are the bulk of it.
    #[test]
    fn uv_inside_test_accepts_the_triangle_and_rejects_the_corners() {
        for corner in FACE_UVS {
            assert!(uv_is_inside_face(corner), "{corner:?} is a corner of the triangle");
        }
        let centroid = [
            (FACE_UVS[0][0] + FACE_UVS[1][0] + FACE_UVS[2][0]) / 3.0,
            (FACE_UVS[0][1] + FACE_UVS[1][1] + FACE_UVS[2][1]) / 3.0,
        ];
        assert!(uv_is_inside_face(centroid), "the centroid is inside");
        for corner in [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]] {
            assert!(
                !uv_is_inside_face(corner),
                "{corner:?} is a corner of the texture, outside the inscribed triangle"
            );
        }
    }

    /// The triangle must cover roughly a third of the texture.
    ///
    /// This pins the number the fractal's cost rests on. An equilateral triangle of side 0.866 and
    /// height 0.75 has area 0.325 — so about two thirds of the texture is padding that must never be
    /// iterated. If a future edit to `FACE_UVS` changed this materially, the CPU fixture's timings
    /// would shift for a reason nobody would think to look for; this test makes that loud.
    #[test]
    fn the_triangle_covers_about_a_third_of_the_texture() {
        let steps = 200;
        let mut inside = 0;
        for y in 0..steps {
            for x in 0..steps {
                let uv = [
                    (x as f32 + 0.5) / steps as f32,
                    (y as f32 + 0.5) / steps as f32,
                ];
                if uv_is_inside_face(uv) {
                    inside += 1;
                }
            }
        }
        let coverage = inside as f32 / (steps * steps) as f32;
        assert!(
            (0.31..0.34).contains(&coverage),
            "the inscribed equilateral triangle must cover ~32.5% of the texture; got {coverage}"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rayland-icosa-core geometry`
Expected: FAIL to compile — `icosahedron`, `Vertex` and `FACE_UVS` are not defined.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/rayland-icosa-core/src/geometry.rs` (above the test module):

```rust
//! The icosahedron: 20 equilateral triangular faces, 12 corners, 30 edges.
//!
//! # Why the vertices are not shared
//! Only 12 distinct positions exist, so an indexed mesh could describe this solid in 12 vertices
//! and 60 indices. This module emits **60 vertices instead**, three per face, repeating each
//! position five times.
//!
//! That is deliberate. A vertex shared between faces can carry only one normal, so sharing forces
//! the normal at each corner to be an average of the five faces meeting there, and the GPU then
//! interpolates smoothly across every face — turning a Platonic solid into a faceted ball with soft
//! edges. Giving each face its own three vertices lets each carry its own true face normal, so the
//! faces shade flatly and the edges stay hard, which is what makes the shape read as an
//! icosahedron. At 60 vertices, an index buffer would save under a kilobyte and add Vulkan surface
//! for nothing.
//!
//! # The construction
//! The 12 corners of a regular icosahedron are the cyclic permutations of `(0, ±1, ±φ)`, where `φ`
//! is the golden ratio. This is a classical result and is why the golden ratio shows up in a file
//! about a solid; there is no numerology involved. The points are then normalised onto the unit
//! sphere so the solid has a predictable size regardless of `φ`'s magnitude.

/// The golden ratio, `(1 + √5) / 2`.
///
/// `sqrt` is one of the few operations IEEE-754 *does* specify exactly, so this is reproducible
/// across hosts in the same way the rest of this crate is — unlike a transcendental, it needs no
/// replacement from [`crate::exact_math`].
const PHI: f32 = 1.618_034;

/// A single vertex, laid out exactly as the vertex shader expects to read it.
///
/// `#[repr(C)]` is load-bearing, not decoration: this struct is copied byte-for-byte into a GPU
/// buffer and interpreted according to a hand-written attribute description. Rust's default layout
/// is deliberately unspecified and may reorder fields, which would silently feed positions into the
/// normal attribute and produce a plausible-looking but wrong picture.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vertex {
    /// Position in model space, on the unit sphere.
    pub position: [f32; 3],
    /// The face's outward normal. Identical for all three vertices of a face — see the module docs.
    pub normal: [f32; 3],
    /// Texture coordinate into the fractal image, in `0.0..=1.0`, origin top-left.
    pub uv: [f32; 2],
}

/// The UV triangle every face samples: an equilateral triangle inscribed in the texture.
///
/// All 20 faces use these same three coordinates, so every face displays the same fractal image and
/// the zoom is visible on all of them at once. Equilateral to match the equilateral faces, so the
/// image arrives unsheared; the alternative — atlasing 20 distinct sub-regions — would buy nothing
/// diagnostic while adding a per-face layout that can be subtly wrong in ways a test would struggle
/// to catch.
///
/// The values: side 0.866 and height 0.75, centred in the unit square.
///
/// # Why this is public
/// [`crate::fractal`] reads it, because this triangle covers only 32.5% of the texture and the
/// fractal must not iterate the other 67.5% — no face can ever sample it. That is not merely
/// thrift: the GPU fixture evaluates the fractal per fragment and so is restricted to the visible
/// region automatically, by the rasteriser. A CPU fixture that iterated the whole square would do
/// three times the arithmetic of its counterpart for a reason having nothing to do with the
/// property the two are built to compare, and the comparison between them would be quietly wrong by
/// that factor.
pub const FACE_UVS: [[f32; 2]; 3] = [[0.5, 0.125], [0.067, 0.875], [0.933, 0.875]];

/// Whether a texture coordinate falls inside [`FACE_UVS`], and is therefore ever visible.
///
/// # Inputs and outputs
/// `uv` in texture space, `0.0..=1.0` on each axis. Returns true if the point is inside the
/// triangle or on its edge.
///
/// # How it works
/// The standard edge-sign test: for each of the triangle's three edges, compute the cross product
/// of the edge with the vector from its start to the point. The point is inside exactly when all
/// three have the same sign — that is, when it is on the same side of every edge. Points exactly on
/// an edge produce a zero and are counted inside, which is the right choice at a texture boundary:
/// a texel the sampler may read must not be black.
///
/// # Failure modes
/// None. A degenerate triangle would report everything inside, but [`FACE_UVS`] is a compile-time
/// constant and is not degenerate.
pub fn uv_is_inside_face(uv: [f32; 2]) -> bool {
    // The signed area of the triangle formed by edge (a → b) and the point. Its sign says which
    // side of that edge the point is on.
    let edge_sign = |a: [f32; 2], b: [f32; 2]| {
        (b[0] - a[0]) * (uv[1] - a[1]) - (b[1] - a[1]) * (uv[0] - a[0])
    };
    let d0 = edge_sign(FACE_UVS[0], FACE_UVS[1]);
    let d1 = edge_sign(FACE_UVS[1], FACE_UVS[2]);
    let d2 = edge_sign(FACE_UVS[2], FACE_UVS[0]);
    // All non-negative or all non-positive: the point is on the same side of every edge. Written to
    // accept both signs rather than assuming a winding, so a future reordering of FACE_UVS cannot
    // silently invert the test into "outside".
    let all_non_negative = d0 >= 0.0 && d1 >= 0.0 && d2 >= 0.0;
    let all_non_positive = d0 <= 0.0 && d1 <= 0.0 && d2 <= 0.0;
    all_non_negative || all_non_positive
}

/// The 12 corners, before normalisation: the cyclic permutations of `(0, ±1, ±φ)`.
const CORNERS: [[f32; 3]; 12] = [
    [-1.0, PHI, 0.0],  // 0
    [1.0, PHI, 0.0],   // 1
    [-1.0, -PHI, 0.0], // 2
    [1.0, -PHI, 0.0],  // 3
    [0.0, -1.0, PHI],  // 4
    [0.0, 1.0, PHI],   // 5
    [0.0, -1.0, -PHI], // 6
    [0.0, 1.0, -PHI],  // 7
    [PHI, 0.0, -1.0],  // 8
    [PHI, 0.0, 1.0],   // 9
    [-PHI, 0.0, -1.0], // 10
    [-PHI, 0.0, 1.0],  // 11
];

/// The 20 faces, as triples of indices into [`CORNERS`].
///
/// Wound counter-clockwise as seen from *outside* the solid, which is what makes the outward normal
/// come out of the cross product below with the right sign. The `all_normals_point_outward` test is
/// what stands between this table and a silently back-face-culled triangle.
const FACES: [[usize; 3]; 20] = [
    [0, 11, 5],
    [0, 5, 1],
    [0, 1, 7],
    [0, 7, 10],
    [0, 10, 11],
    [1, 5, 9],
    [5, 11, 4],
    [11, 10, 2],
    [10, 7, 6],
    [7, 1, 8],
    [3, 9, 4],
    [3, 4, 2],
    [3, 2, 6],
    [3, 6, 8],
    [3, 8, 9],
    [4, 9, 5],
    [2, 4, 11],
    [6, 2, 10],
    [8, 6, 7],
    [9, 8, 1],
];

/// Scale a vector onto the unit sphere.
///
/// The raw golden-ratio corners sit at radius `√(1 + φ²) ≈ 1.902`; normalising gives the solid a
/// radius of exactly 1, so the camera distance in [`crate::schedule`] can be chosen once and stay
/// meaningful.
fn normalize(v: [f32; 3]) -> [f32; 3] {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    [v[0] / length, v[1] / length, v[2] / length]
}

/// The solid's vertex buffer: 60 vertices, three per face, ready to copy to the GPU.
///
/// # Outputs
/// An array of 60 [`Vertex`], in face order: elements `3n..3n+3` are face `n`'s three corners,
/// wound counter-clockwise seen from outside, all three carrying face `n`'s outward normal and the
/// shared [`FACE_UVS`].
///
/// # Failure modes
/// None; it is a pure function of compile-time constants and cannot fail. It recomputes the table
/// on every call rather than caching, because it is called once per program run and a `static`
/// would need either `unsafe` or a lazy-initialisation dependency, for no gain.
pub fn icosahedron() -> [Vertex; 60] {
    // A placeholder to fill; every element is overwritten below, since FACES covers all 20 faces.
    let mut vertices = [Vertex {
        position: [0.0; 3],
        normal: [0.0; 3],
        uv: [0.0; 2],
    }; 60];

    for (face_index, face) in FACES.iter().enumerate() {
        let p = [
            normalize(CORNERS[face[0]]),
            normalize(CORNERS[face[1]]),
            normalize(CORNERS[face[2]]),
        ];

        // The face's plane, as two edges sharing corner 0.
        let edge1 = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
        let edge2 = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
        // The cross product of two counter-clockwise-wound edges points out of the face. This is
        // where FACES' winding order turns into a normal direction, and why a mis-wound entry in
        // that table produces an inward normal rather than a merely cosmetic problem.
        let cross = [
            edge1[1] * edge2[2] - edge1[2] * edge2[1],
            edge1[2] * edge2[0] - edge1[0] * edge2[2],
            edge1[0] * edge2[1] - edge1[1] * edge2[0],
        ];
        // The lighting in the fragment shader assumes a unit normal; an unnormalised one would make
        // faces brighter or darker purely according to their triangle's area.
        let normal = normalize(cross);

        for corner in 0..3 {
            vertices[face_index * 3 + corner] = Vertex {
                position: p[corner],
                normal,
                uv: FACE_UVS[corner],
            };
        }
    }

    vertices
}
```

Add to `crates/rayland-icosa-core/src/lib.rs`, after the `exact_math` module declaration:

```rust
// The solid itself: its 60 vertices, their flat normals and their texture coordinates.
pub mod geometry;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rayland-icosa-core`
Expected: PASS — all six geometry tests plus the earlier math tests.

If `all_normals_point_outward` fails for some faces but not others, the `FACES` table has mis-wound entries; swap the last two indices of each failing face. Do not "fix" it by taking the absolute value of the dot product or by disabling back-face culling later — that hides a real defect in the table.

- [ ] **Step 5: Commit**

```bash
git add crates/rayland-icosa-core
git commit -m "icosa Task 3: the icosahedron, 60 unshared vertices with flat normals

Golden-ratio construction, normalised to the unit sphere. Vertices are not
shared between faces even though only 12 distinct positions exist: a shared
vertex can carry only one normal, so sharing would average the normals at the
corners and smooth the solid into a faceted ball. Each face gets its own three
vertices and its own true face normal, so the faces shade flat and the edges
stay hard.

All 20 faces carry the same inscribed equilateral UV triangle, so every face
shows the same fractal and the zoom is visible on all of them at once."
```

---

### Task 4: The frame schedule and the fractal

**Files:**
- Create: `crates/rayland-icosa-core/src/schedule.rs`
- Create: `crates/rayland-icosa-core/src/fractal.rs`
- Modify: `crates/rayland-icosa-core/src/lib.rs`
- Test: inline `#[cfg(test)]` modules in both files

**Interfaces:**
- Consumes: `exact_math::{log2, sin_cos}` (Tasks 1–2), `MAX_ITER`, `TEXTURE_SIZE`, `FRAME_COUNT` (Task 1).
- Produces:
  - `schedule::frame_zoom(frame: u32) -> f64` — the complex-plane half-width at this frame.
  - `schedule::frame_mvp(frame: u32) -> [[f32; 4]; 4]` — the model-view-projection matrix, column-major, ready to memcpy into a uniform buffer.
  - `schedule::CENTER: (f64, f64)`.
  - `fractal::render_into(pixels: &mut [u8], half_width: f64)` — fills `TEXTURE_SIZE * TEXTURE_SIZE * 4` bytes of RGBA8.

- [ ] **Step 1: Write the failing tests**

Create `crates/rayland-icosa-core/src/schedule.rs` with only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The schedule must be a pure function of the frame index.
    ///
    /// This is the property the entire fixture rests on. If any of these functions consulted a
    /// clock, two runs of the same binary would produce different images and "native versus
    /// remoted" would be a race between two timelines rather than a comparison of two known
    /// quantities. Calling twice and demanding identical bits is a cheap, direct check of that.
    #[test]
    fn the_schedule_is_pure() {
        for frame in 0..crate::FRAME_COUNT {
            assert_eq!(frame_zoom(frame).to_bits(), frame_zoom(frame).to_bits());
            assert_eq!(frame_mvp(frame), frame_mvp(frame));
        }
    }

    /// Calling out of order must not change the answers — no hidden accumulator.
    ///
    /// `frame_zoom` is a repeated multiplication, and the obvious wrong implementation caches the
    /// running product across calls. That would pass `the_schedule_is_pure` (which calls in order)
    /// while being catastrophically order-dependent.
    #[test]
    fn the_schedule_does_not_depend_on_call_order() {
        let forward: Vec<u64> = (0..crate::FRAME_COUNT).map(|f| frame_zoom(f).to_bits()).collect();
        let backward: Vec<u64> = (0..crate::FRAME_COUNT)
            .rev()
            .map(|f| frame_zoom(f).to_bits())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert_eq!(forward, backward, "frame_zoom must not accumulate across calls");
    }

    /// The zoom must actually zoom in, and reach the documented depth.
    #[test]
    fn the_zoom_narrows_to_the_documented_depth() {
        assert_eq!(frame_zoom(0), INITIAL_HALF_WIDTH, "frame 0 is the starting view");
        let last = frame_zoom(crate::FRAME_COUNT - 1);
        assert!(last < INITIAL_HALF_WIDTH, "the view must narrow");
        // 0.97^119 ≈ 0.0268; the spec quotes ~0.026 of the starting half-width.
        let ratio = last / INITIAL_HALF_WIDTH;
        assert!(
            (0.025..0.028).contains(&ratio),
            "the final view must be ~2.6% of the first; got {ratio}"
        );
    }

    /// The zoom must stay well inside f64's precision, so no frame is degenerate.
    ///
    /// The spec chose the schedule specifically so that mantissa exhaustion never becomes a design
    /// problem; this test is what keeps that true if someone later tunes the constants.
    #[test]
    fn the_zoom_never_approaches_the_precision_floor() {
        let smallest = frame_zoom(crate::FRAME_COUNT - 1);
        assert!(
            smallest > 1e-12,
            "a half-width near f64's resolution would pixelate into blocks; got {smallest}"
        );
    }

    /// The solid must actually rotate — frame 0 and a later frame must differ.
    #[test]
    fn the_solid_rotates() {
        assert_ne!(frame_mvp(0), frame_mvp(30), "the orientation must change over time");
    }
}
```

Create `crates/rayland-icosa-core/src/fractal.rs` with only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The number of bytes a full fractal texture occupies.
    fn texture_bytes() -> usize {
        (crate::TEXTURE_SIZE as usize) * (crate::TEXTURE_SIZE as usize) * 4
    }

    /// The renderer must fill exactly the buffer it was given, and every pixel must be opaque.
    ///
    /// A stray transparent pixel would blend the solid's face into the background in a way that is
    /// hard to see and harder to attribute.
    #[test]
    fn fills_every_pixel_opaquely() {
        let mut pixels = vec![0u8; texture_bytes()];
        render_into(&mut pixels, 1.5);
        for (index, chunk) in pixels.chunks_exact(4).enumerate() {
            assert_eq!(chunk[3], 255, "pixel {index} must be fully opaque");
        }
    }

    /// The fractal must be deterministic: same half-width in, same bytes out.
    #[test]
    fn is_deterministic() {
        let mut first = vec![0u8; texture_bytes()];
        let mut second = vec![0u8; texture_bytes()];
        render_into(&mut first, 1.5);
        render_into(&mut second, 1.5);
        assert_eq!(first, second, "the fractal must be a pure function of its half-width");
    }

    /// Zooming must change the picture — otherwise the animation is a still image.
    #[test]
    fn zooming_changes_the_picture() {
        let mut wide = vec![0u8; texture_bytes()];
        let mut narrow = vec![0u8; texture_bytes()];
        render_into(&mut wide, 1.5);
        render_into(&mut narrow, 0.05);
        assert_ne!(wide, narrow, "a different half-width must produce a different image");
    }

    /// Points inside the set must be black, and the set must be present in the view.
    ///
    /// The centre of the starting view is a known interior point of the Mandelbrot set, so it must
    /// come out black. If the whole image were black, or none of it, the iteration is broken in a
    /// way that a "the images differ" test would happily pass.
    #[test]
    fn interior_points_are_black() {
        let mut pixels = vec![0u8; texture_bytes()];
        // A view centred on the origin, which is deep inside the set's main cardioid.
        render_into_at(&mut pixels, (0.0, 0.0), 0.1);
        let centre = (crate::TEXTURE_SIZE as usize / 2) * (crate::TEXTURE_SIZE as usize) * 4
            + (crate::TEXTURE_SIZE as usize / 2) * 4;
        assert_eq!(
            &pixels[centre..centre + 3],
            &[0, 0, 0],
            "the origin is inside the set and must be black"
        );
    }

    /// Points far outside the set must escape immediately and not be black.
    #[test]
    fn exterior_points_are_not_black() {
        let mut pixels = vec![0u8; texture_bytes()];
        // Centred far outside the set, where every point escapes on the first iteration or two.
        render_into_at(&mut pixels, (4.0, 4.0), 0.1);
        let centre = (crate::TEXTURE_SIZE as usize / 2) * (crate::TEXTURE_SIZE as usize) * 4
            + (crate::TEXTURE_SIZE as usize / 2) * 4;
        assert_ne!(
            &pixels[centre..centre + 3],
            &[0, 0, 0],
            "a point far outside the set must not be coloured as interior"
        );
    }

    /// A buffer of the wrong size must be refused loudly rather than partially filled.
    #[test]
    #[should_panic(expected = "must be exactly")]
    fn refuses_a_wrongly_sized_buffer() {
        let mut pixels = vec![0u8; 16];
        render_into(&mut pixels, 1.5);
    }

    /// The texture's corners lie outside the sampled triangle and must be black padding.
    ///
    /// Aimed at a view where every point escapes and is therefore brightly coloured, so a corner
    /// that came out black could only have come from the triangle test rather than from the fractal
    /// happening to be dark there.
    #[test]
    fn the_padding_outside_the_triangle_is_black() {
        let mut pixels = vec![0u8; texture_bytes()];
        render_into_at(&mut pixels, (4.0, 4.0), 0.1);
        let size = crate::TEXTURE_SIZE as usize;
        for (x, y, label) in [
            (0, 0, "top-left"),
            (size - 1, 0, "top-right"),
            (0, size - 1, "bottom-left"),
            (size - 1, size - 1, "bottom-right"),
        ] {
            let offset = (y * size + x) * 4;
            assert_eq!(
                &pixels[offset..offset + 4],
                &[0, 0, 0, 255],
                "the {label} corner is outside the sampled triangle and must be black padding"
            );
        }
    }

    /// Restricting the iteration to the triangle must not change what the triangle shows.
    ///
    /// The point of the restriction is to skip *invisible* work. If a texel inside the triangle came
    /// out differently because of it, the restriction would be changing the picture rather than just
    /// its cost — a much worse bug than the waste it set out to fix.
    #[test]
    fn the_restriction_does_not_alter_the_visible_region() {
        let mut pixels = vec![0u8; texture_bytes()];
        render_into(&mut pixels, 1.5);
        let size = crate::TEXTURE_SIZE as usize;
        // The triangle's centroid, which is comfortably inside it.
        let centroid_uv = [
            (crate::geometry::FACE_UVS[0][0]
                + crate::geometry::FACE_UVS[1][0]
                + crate::geometry::FACE_UVS[2][0])
                / 3.0,
            (crate::geometry::FACE_UVS[0][1]
                + crate::geometry::FACE_UVS[1][1]
                + crate::geometry::FACE_UVS[2][1])
                / 3.0,
        ];
        let x = (centroid_uv[0] * size as f32) as usize;
        let y = (centroid_uv[1] * size as f32) as usize;
        let offset = (y * size + x) * 4;

        // Recompute that one texel the long way, with no triangle test in the path at all, and
        // demand the identical bytes.
        let u = (x as f64 + 0.5) / size as f64 - 0.5;
        let v = (y as f64 + 0.5) / size as f64 - 0.5;
        let expected = point_color(
            crate::schedule::CENTER.0 + u * 2.0 * 1.5,
            crate::schedule::CENTER.1 + v * 2.0 * 1.5,
        );
        for channel in 0..3 {
            let want = (expected[channel].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            assert_eq!(
                pixels[offset + channel],
                want,
                "a texel inside the triangle must be unaffected by the restriction"
            );
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rayland-icosa-core`
Expected: FAIL to compile — `frame_zoom`, `frame_mvp`, `INITIAL_HALF_WIDTH`, `render_into`, `render_into_at` are not defined.

- [ ] **Step 3: Write the schedule**

Prepend to `crates/rayland-icosa-core/src/schedule.rs`:

```rust
//! Where the solid is pointing and how deep the fractal is zoomed, at each frame.
//!
//! # The one rule: no clock
//! Every function here is a pure function of the **frame index**. Nothing consults `Instant::now`,
//! nothing measures elapsed time, nothing skips or repeats a frame to keep up. This is what makes
//! an animated program testable at all: the same binary run twice produces the same 120 images, so
//! comparing two runs is comparing two known quantities rather than watching two races.
//!
//! A program that animated on wall-clock time could not be asserted against, only squinted at.
//!
//! # And no libm
//! The rotation matrix needs sine and cosine, and the zoom needs a power. Both come from
//! [`crate::exact_math`] or from plain multiplication rather than from the standard library, for
//! the reason that module explains at length: this arithmetic happens on the machine the program
//! runs on, and the same program must produce the same picture on a different machine.
//!
//! In particular `f64::powi` is **not** used. It is not IEEE-specified, it lowers to an LLVM
//! intrinsic whose expansion is a quality-of-implementation matter, and it could legitimately
//! differ between targets. The loop below is longer and is exactly reproducible.

use crate::exact_math::sin_cos;

/// The complex-plane half-width of the very first frame's view.
///
/// 1.5 frames the whole Mandelbrot set comfortably, so the animation opens on something
/// recognisable before diving in.
pub const INITIAL_HALF_WIDTH: f64 = 1.5;

/// How much the view narrows each frame.
///
/// Geometric, so the zoom feels linear to the eye. Over the 120 frames of a run this reaches
/// `0.97^119 ≈ 0.027` of the starting width — visibly deep, and nowhere near `f64`'s 52-bit
/// mantissa, so precision exhaustion never becomes something this fixture has to design around.
pub const ZOOM_PER_FRAME: f64 = 0.97;

/// The point in the complex plane the view zooms toward.
///
/// A classical deep-zoom coordinate on the boundary of the set, chosen because the boundary is
/// where the structure is: a point in the interior would zoom into featureless black and a point
/// well outside into featureless colour, and either would make the animation's later frames
/// useless as evidence that anything is being drawn.
pub const CENTER: (f64, f64) = (-0.743643887037151, 0.13182590420533);

/// How far the solid turns about the vertical axis each frame, in radians.
///
/// Chosen with [`PITCH_PER_FRAME`] so that the two are incommensurate: the solid never returns to a
/// previous orientation during a run, so all 120 frames are genuinely distinct and a defect that
/// affects only some orientations cannot hide behind a repeat.
const YAW_PER_FRAME: f64 = 0.031;

/// How far the solid tips about the horizontal axis each frame, in radians.
const PITCH_PER_FRAME: f64 = 0.017;

/// How far the camera sits from the solid, in model-space units.
///
/// The solid has radius 1 (its corners are normalised onto the unit sphere), so 3.2 frames it with
/// a comfortable margin at the field of view below — near enough to fill the image, far enough that
/// no face ever crosses the near plane.
const CAMERA_DISTANCE: f64 = 3.2;

/// The perspective projection's near plane.
const NEAR_PLANE: f64 = 0.1;

/// The perspective projection's far plane.
const FAR_PLANE: f64 = 10.0;

/// `1 / tan(fov/2)` for a 45-degree vertical field of view.
///
/// Precomputed as a literal rather than derived with a tangent, for the same reproducibility reason
/// as everything else here — and because it is a constant, so computing it at runtime would be
/// pointless as well as risky.
const FOCAL_LENGTH: f64 = 2.414_213_562_373_095;

/// The complex-plane half-width of the view at `frame`.
///
/// # Inputs and outputs
/// `frame` in `0..FRAME_COUNT`. Returns the half-width, shrinking geometrically from
/// [`INITIAL_HALF_WIDTH`].
///
/// # Failure modes
/// None. Frames beyond `FRAME_COUNT` simply keep zooming; nothing here enforces the range, because
/// the loop that calls it does.
///
/// # Pitfall
/// The repeated multiplication is deliberate and must not be replaced with `powi` or `powf` — see
/// the module documentation. It must also stay a *local* loop with no cached state between calls:
/// an accumulator held across calls would make the result depend on call order, which
/// `the_schedule_does_not_depend_on_call_order` exists to catch.
pub fn frame_zoom(frame: u32) -> f64 {
    let mut half_width = INITIAL_HALF_WIDTH;
    // Multiply once per elapsed frame. Exactly reproducible: every step is a single IEEE multiply.
    for _ in 0..frame {
        half_width *= ZOOM_PER_FRAME;
    }
    half_width
}

/// The model-view-projection matrix at `frame`, column-major, as the shader will read it.
///
/// # Inputs and outputs
/// `frame` in `0..FRAME_COUNT`. Returns a 4×4 matrix in **column-major** order — `m[c][r]` is
/// column `c`, row `r` — which is the layout GLSL's `mat4` expects when read straight out of a
/// uniform buffer.
///
/// # What it composes
/// A yaw about Y and a pitch about X (the animation), then a translation away from the camera, then
/// a perspective projection with Vulkan's depth convention.
///
/// # Pitfall — Vulkan is not OpenGL here
/// Vulkan's clip space has **Y pointing down** and depth in `0..1`, where OpenGL has Y up and depth
/// in `-1..1`. The Y flip and the depth remap below are what account for that. Omitting the flip
/// renders the solid upside down — which on a shape this symmetric is genuinely easy to miss, and
/// is exactly why the vertex shader's convention is spelled out rather than assumed.
///
/// # Failure modes
/// None; pure arithmetic over the frame index.
pub fn frame_mvp(frame: u32) -> [[f32; 4]; 4] {
    let t = frame as f64;
    // Angles grow linearly with the frame index — the entire animation, in two lines.
    let (sin_yaw, cos_yaw) = sin_cos(t * YAW_PER_FRAME);
    let (sin_pitch, cos_pitch) = sin_cos(t * PITCH_PER_FRAME);

    // The rotation, as yaw about Y composed with pitch about X, written out rather than built from
    // a matrix-multiply helper: three lines of explicit products are easier for a reviewer to check
    // than a general routine, and this is the only place a rotation is ever needed.
    let r00 = cos_yaw;
    let r01 = sin_yaw * sin_pitch;
    let r02 = sin_yaw * cos_pitch;
    let r10 = 0.0;
    let r11 = cos_pitch;
    let r12 = -sin_pitch;
    let r20 = -sin_yaw;
    let r21 = cos_yaw * sin_pitch;
    let r22 = cos_yaw * cos_pitch;

    // The projection's two scale factors. The images are square, so the horizontal and vertical
    // focal lengths are equal and no aspect-ratio term is needed.
    let sx = FOCAL_LENGTH;
    // Negated: this is the Y flip that converts the maths convention (Y up) to Vulkan's (Y down).
    let sy = -FOCAL_LENGTH;
    // Vulkan maps the visible depth range onto 0..1, not -1..1. These two terms are that mapping.
    let sz = FAR_PLANE / (NEAR_PLANE - FAR_PLANE);
    let tz = (FAR_PLANE * NEAR_PLANE) / (NEAR_PLANE - FAR_PLANE);

    // Project(Translate(Rotate(v))), multiplied out by hand. The camera translation only touches z,
    // so most of the projection's product with the translation collapses to nothing.
    [
        [(sx * r00) as f32, (sy * r10) as f32, (sz * r20) as f32, (-r20) as f32],
        [(sx * r01) as f32, (sy * r11) as f32, (sz * r21) as f32, (-r21) as f32],
        [(sx * r02) as f32, (sy * r12) as f32, (sz * r22) as f32, (-r22) as f32],
        [
            0.0,
            0.0,
            (sz * -CAMERA_DISTANCE + tz) as f32,
            CAMERA_DISTANCE as f32,
        ],
    ]
}
```

Note on the `f64 → f32` narrowing in the last lines: that conversion is IEEE-specified round-to-nearest and therefore exactly reproducible, so it does not undo anything Tasks 1–2 established.

- [ ] **Step 4: Write the fractal**

Prepend to `crates/rayland-icosa-core/src/fractal.rs`:

```rust
//! The zooming Mandelbrot image the solid's faces are textured with.
//!
//! # Where this came from
//! The algorithm — escape-time iteration, a smooth (non-integer) iteration count to kill the colour
//! banding, and an HSV sweep for the palette — is the one from the author's `mandelsmooth` program.
//! What has changed: that program is an interactive GLSL shader driven by the mouse wheel, and this
//! is a fixed, frame-indexed, CPU-side `f64` computation. The arithmetic in the middle is the same.
//!
//! # Why the smooth iteration count needs care
//! The formula is `i + 1 - log2(log2(|z|))`. That inner logarithm is the reason
//! [`crate::exact_math`] exists: it is the only transcendental in the whole picture, and if it came
//! from libm this image would differ in its last bits between an x86 machine and a RISC-V one.
//!
//! # Only the visible third is iterated
//! Every face samples the same equilateral triangle inscribed in this square texture, and that
//! triangle covers 32.5% of it. The Mandelbrot iteration therefore runs **only inside the
//! triangle**; the rest is written black without being iterated.
//!
//! That is not a mere optimisation, and it must not be removed as "simplification". The GPU fixture
//! evaluates this same fractal per fragment, so its rasteriser confines the work to the visible
//! region for free. If this module iterated the whole square, the CPU fixture would perform roughly
//! three times the fractal arithmetic of its counterpart — for a reason having nothing to do with
//! where the fractal is computed, which is the one property the two fixtures exist to compare. The
//! resulting measurement would be wrong by that factor and would look perfectly reasonable.
//!
//! The padding is still written every frame, though. The byte traffic through mapped memory is the
//! thing the CPU fixture is built to create; only the expensive arithmetic is skipped.

use crate::exact_math::log2;

/// The escape radius, squared.
///
/// Escape is tested against `|z|² > 4` rather than `|z| > 2` so that the loop needs no square root.
/// The smooth-iteration formula is derived assuming a generous escape radius, and 2 is the standard
/// choice; a larger radius smooths marginally better at the cost of more iterations.
const ESCAPE_RADIUS_SQUARED: f64 = 4.0;

/// Convert an HSV colour to RGB, each channel in `0.0..=1.0`.
///
/// # Inputs and outputs
/// `hue` in `0.0..=1.0` (wrapping), `saturation` and `value` in `0.0..=1.0`. Returns linear RGB.
///
/// # Why HSV at all
/// Sweeping the hue turns a scalar — the smooth iteration count — into a colour that varies
/// continuously and never repeats a shade at a nearby value, which is what makes the set's banding
/// structure legible. A grayscale ramp would be simpler and would show far less.
///
/// # Failure modes
/// None; out-of-range inputs are clamped by the arithmetic rather than rejected.
fn hsv_to_rgb(hue: f64, saturation: f64, value: f64) -> [f64; 3] {
    // The classical piecewise-linear hue ramp, expressed as three phase-shifted triangle waves.
    // Written with explicit `rem_euclid` rather than `%` so that a negative hue wraps rather than
    // reflecting, which would produce a visible seam at hue zero.
    let ramp = |offset: f64| {
        let phase = (hue * 6.0 + offset).rem_euclid(6.0);
        // A triangle wave: rises, plateaus, falls, in `0.0..=1.0`.
        let raw = (phase - 3.0).abs() - 1.0;
        let clamped = raw.clamp(0.0, 1.0);
        // Smoothstep, which softens the ramp's corners; `mandelsmooth` does this and the palette
        // looks visibly harsher without it.
        clamped * clamped * (3.0 - 2.0 * clamped)
    };
    let rgb = [ramp(0.0), ramp(4.0), ramp(2.0)];
    // Mix toward white by the inverse of saturation, then scale by value.
    [
        value * (1.0 + saturation * (rgb[0] - 1.0)),
        value * (1.0 + saturation * (rgb[1] - 1.0)),
        value * (1.0 + saturation * (rgb[2] - 1.0)),
    ]
}

/// The colour of a single point of the complex plane, as RGB in `0.0..=1.0`.
///
/// # Inputs and outputs
/// `cx`, `cy` — the point. Returns black for points that never escape (i.e. points of the set), and
/// a hue-swept colour for points that do.
///
/// # Failure modes
/// None. The `log2` of a `|z|` that has just escaped is always well-defined: escape guarantees
/// `|z| > 2`, so the inner `log2` is positive and the outer one is real.
fn point_color(cx: f64, cy: f64) -> [f64; 3] {
    let mut zx = 0.0f64;
    let mut zy = 0.0f64;
    let mut iteration = 0u32;

    // The Mandelbrot recurrence, z = z² + c, iterated until it escapes or we give up. Every
    // operation is an IEEE multiply or add, so this loop is bit-reproducible on any host.
    while iteration < crate::MAX_ITER {
        let zx2 = zx * zx;
        let zy2 = zy * zy;
        // Once |z|² passes 4 the orbit provably diverges, so there is no reason to keep iterating.
        if zx2 + zy2 > ESCAPE_RADIUS_SQUARED {
            break;
        }
        // The complex square, expanded: (zx + i·zy)² = (zx² - zy²) + i·(2·zx·zy).
        let next_zy = 2.0 * zx * zy + cy;
        zx = zx2 - zy2 + cx;
        zy = next_zy;
        iteration += 1;
    }

    // Points that used the whole iteration budget are treated as inside the set and painted black.
    // This is the standard lie: they may merely be very slow to escape, but at 512 iterations the
    // distinction is invisible.
    if iteration >= crate::MAX_ITER {
        return [0.0, 0.0, 0.0];
    }

    // The smooth iteration count. The integer count alone produces visible concentric bands, because
    // it jumps by a whole unit at each escape contour; subtracting log2(log2(|z|)) interpolates
    // across the band and removes them. This is `mandelsmooth`'s whole reason for its name.
    let modulus_squared = zx * zx + zy * zy;
    // log2(|z|) = log2(|z|²) / 2, which saves a square root and is exact — a division by two only
    // decrements the exponent.
    let log_modulus = log2(modulus_squared) / 2.0;
    let smooth_iteration = iteration as f64 + 1.0 - log2(log_modulus);

    // Normalise into a hue. The scale factor is not `MAX_ITER`: almost every escaping point does so
    // in the first few dozen iterations, so dividing by the full budget would compress the entire
    // visible palette into a sliver of the hue circle and the image would be nearly monochrome.
    let hue = smooth_iteration / 64.0;
    hsv_to_rgb(hue, 0.85, 1.0)
}

/// Render the fractal into `pixels` as RGBA8, at the run's fixed centre.
///
/// # Inputs and outputs
/// `pixels` must be exactly `TEXTURE_SIZE * TEXTURE_SIZE * 4` bytes and is fully overwritten.
/// `half_width` is the view's half-width in the complex plane — see
/// [`crate::schedule::frame_zoom`].
///
/// # Failure modes
/// Panics if `pixels` is the wrong length. That is deliberate: the caller is writing into
/// GPU-visible memory it mapped itself, a wrong length there means the caller's allocation
/// disagrees with the texture's declared size, and quietly filling part of it would produce a
/// half-drawn texture that looks like a Rayland transport bug.
pub fn render_into(pixels: &mut [u8], half_width: f64) {
    render_into_at(pixels, crate::schedule::CENTER, half_width);
}

/// Render the fractal into `pixels` as RGBA8, centred anywhere.
///
/// Identical to [`render_into`] but with an explicit centre. Exists so the unit tests can aim the
/// view at a known interior point and a known exterior point; the fixtures themselves always use
/// [`render_into`] and the run's fixed centre.
///
/// # Failure modes
/// Panics if `pixels` is the wrong length; see [`render_into`].
pub fn render_into_at(pixels: &mut [u8], center: (f64, f64), half_width: f64) {
    let size = crate::TEXTURE_SIZE as usize;
    let expected = size * size * 4;
    assert_eq!(
        pixels.len(),
        expected,
        "the pixel buffer must be exactly {expected} bytes for a {size}×{size} RGBA8 texture"
    );

    for y in 0..size {
        for x in 0..size {
            let offset = (y * size + x) * 4;

            // The texture coordinate this texel sits at, which is what decides whether any face can
            // ever sample it.
            let uv = [
                (x as f32 + 0.5) / size as f32,
                (y as f32 + 0.5) / size as f32,
            ];

            // Only 32.5% of the texture lies under the triangle every face samples; the rest is
            // never read by anything. Skipping the iteration there is not just thrift — the GPU
            // fixture evaluates this same fractal per fragment and so is confined to the visible
            // region automatically, by the rasteriser. Iterating the whole square here would make
            // this program do three times its counterpart's arithmetic for a reason unrelated to
            // the one property the two exist to compare, and would corrupt that comparison by that
            // factor. See `crate::geometry::FACE_UVS`.
            //
            // The padding is still *written*, every frame, like every other texel: the byte traffic
            // through mapped memory is what this program exists to create, and shrinking it to the
            // triangle would quietly cut the very number the workload is built around. It is only
            // the expensive part — up to MAX_ITER iterations — that is skipped.
            if !crate::geometry::uv_is_inside_face(uv) {
                pixels[offset] = 0;
                pixels[offset + 1] = 0;
                pixels[offset + 2] = 0;
                pixels[offset + 3] = 255;
                continue;
            }

            // Map the pixel onto the complex plane. The `+ 0.5` samples the pixel's centre rather
            // than its top-left corner, which keeps the image symmetric about the view's centre.
            let u = (x as f64 + 0.5) / size as f64 - 0.5;
            let v = (y as f64 + 0.5) / size as f64 - 0.5;
            // The texture is square, so the same half-width applies to both axes.
            let cx = center.0 + u * 2.0 * half_width;
            let cy = center.1 + v * 2.0 * half_width;

            let rgb = point_color(cx, cy);
            // Scale to 8-bit. The `+ 0.5` before truncation is round-to-nearest; without it the
            // whole image would be biased half a level dark. Clamping guards the channel against a
            // palette value marginally outside 0..1 producing a wrapped byte — a bright pixel in a
            // dark region, which looks exactly like a memory-corruption artefact.
            for channel in 0..3 {
                pixels[offset + channel] = (rgb[channel].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
            // Opaque: the texture is a surface colour, and any transparency here would blend the
            // solid's faces into the background.
            pixels[offset + 3] = 255;
        }
    }
}
```

Add to `crates/rayland-icosa-core/src/lib.rs`:

```rust
// Where the solid points and how deep the fractal is zoomed, at each frame.
pub mod schedule;
// The zooming Mandelbrot image the faces are textured with.
pub mod fractal;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p rayland-icosa-core`
Expected: PASS — all schedule and fractal tests, plus the earlier math and geometry tests.

- [ ] **Step 6: Check the fractal is worth looking at**

The unit tests prove the fractal is deterministic and that its interior is black; they do not prove it is *legible*. Before building a GPU fixture on top of it, look at one:

Add a temporary test that calls `render_into(&mut pixels, 1.5)` and writes the bytes to a PNG in `/tmp` with a scratch `image` dependency, run it, open the file. Then **delete the temporary test and the scratch dependency** — this crate has no dependencies and must keep none.

What you should see: an upward-pointing **triangle** of fractal on a black square. Inside the triangle, the Mandelbrot set in black on a hue-swept background, with no harsh banding. The black surround is the padding outside the sampled UV triangle and is correct — it is never displayed, because no face samples it.

If the image is nearly all one colour, the hue scale factor is wrong. If it shows concentric rings, the smooth iteration count is not being applied and `log2` should be suspected. If the triangle points *down*, `FACE_UVS` has been reordered relative to the geometry and the fractal will appear rotated on every face.

- [ ] **Step 7: Commit**

```bash
git add crates/rayland-icosa-core
git commit -m "icosa Task 4: the frame schedule and the zooming fractal

The schedule is a pure function of the frame index — no clock, anywhere. That
is what makes an animated fixture testable: the same binary run twice produces
the same 120 images, so native-versus-remoted compares two known quantities
instead of racing two timelines. powi is avoided along with libm; it is not
IEEE-specified and could differ between targets.

The fractal is mandelsmooth's algorithm — escape-time iteration, smooth
iteration count to kill the banding, HSV palette — moved from an interactive
GLSL shader to a fixed f64 CPU computation. The hue scale divides by 64
rather than the iteration ceiling: almost every point escapes in the first few
dozen iterations, so dividing by the full budget would squeeze the palette into
a sliver of the hue circle."
```

---

### Task 5: `rayland-icosa-cpu` — the solid, shaded, one frame

**Files:**
- Create: `crates/rayland-icosa-cpu/Cargo.toml`
- Create: `crates/rayland-icosa-cpu/src/main.rs`
- Create: `crates/rayland-icosa-cpu/src/context.rs`
- Create: `crates/rayland-icosa-cpu/src/pipeline.rs`
- Create: `crates/rayland-icosa-cpu/src/render.rs`
- Create: `shaders/icosa.vert`, `shaders/icosa_textured.frag` (+ committed `.spv`)
- Test: `crates/rayland-icosa-cpu/tests/native_render.rs`
- Modify: `Cargo.toml`, `CLAUDE.md`

**Interfaces:**
- Consumes: `rayland_icosa_core::{IMAGE_SIZE, geometry::{Vertex, icosahedron}, schedule::frame_mvp}`.
- Produces: a binary `rayland-icosa-cpu <output-directory>` which, at this task's end, writes `frame_0000.png` — the solid, flat-shaded in a solid colour, no texture yet.

**Why this task stops short of the texture:** the texture is the entire point of the fixture, and it is also the part most likely to fail against Rayland. Getting a shaded, depth-tested solid onto a PNG *first* means that when the textured version breaks, the depth buffer and the geometry are already known-good and the search space is one thing wide. This is the same reason `native_render.rs` exists at all.

- [ ] **Step 1: Write the failing test**

Create `crates/rayland-icosa-cpu/tests/native_render.rs`:

```rust
//! The CPU fixture's **baseline**: it draws the right picture on this host's own GPU, with this
//! host's own Vulkan driver, and nothing else involved.
//!
//! # Why this must pass before the end-to-end test is even attempted
//! The end-to-end test spans an enormous amount of machinery. When something that long fails, the
//! expensive question is *which link broke*. This test removes every link but the first: the app
//! against the host's ordinary driver. If this passes and the end-to-end test fails, the app is
//! provably not the problem. That localisation is the whole reason it is written and run first.
//!
//! # Skip, don't fail, without a GPU
//! Following this repository's convention for GPU tests, absence of a render node is reported as a
//! SKIP rather than a failure, so CI without one stays green and stays light.

use image::GenericImageView;
use std::path::Path;
use std::process::Command;

/// The DRM render node this repository's GPU tests gate on.
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// The image edge length the app renders at. Asserted against the decoded PNG rather than assumed.
const IMAGE_SIZE: u32 = 256;

/// The app must render a shaded solid: lit centre, empty corners.
///
/// The checks are chosen to distinguish the failures that actually happen. A centre that matches the
/// background means the draw did not land — no geometry, wrong viewport, or every face culled. A
/// corner that is *not* background means the solid is the wrong size or the projection is wrong.
/// Together they pin "a solid of roughly the right size in the right place" without asserting a full
/// hash, which is what the end-to-end test is for.
#[test]
fn icosa_cpu_natively_renders_a_shaded_solid() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!("SKIP icosa_cpu_natively_renders_a_shaded_solid: no render node at {RENDER_NODE}");
        return;
    }

    let output_dir = std::env::temp_dir().join("rayland-icosa-cpu-native");
    // A stale directory from an earlier run would let a silently-failing binary "pass" on last
    // run's artefacts. Removing it makes the files' existence real evidence of this run.
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir).expect("the output directory must be creatable");

    let status = Command::new(env!("CARGO_BIN_EXE_rayland-icosa-cpu"))
        .arg(&output_dir)
        .status()
        .expect("the fixture binary must be launchable");
    assert!(status.success(), "the fixture must exit successfully on a host with a GPU; got {status}");

    let frame = image::open(output_dir.join("frame_0000.png"))
        .expect("the app must have written a decodable PNG for frame 0");
    assert_eq!(
        frame.dimensions(),
        (IMAGE_SIZE, IMAGE_SIZE),
        "the app must render at its documented size"
    );

    // The camera looks straight at the solid, so the image centre is always covered by a face.
    let centre = frame.get_pixel(IMAGE_SIZE / 2, IMAGE_SIZE / 2);
    assert_ne!(
        centre,
        image::Rgba([0, 0, 0, 255]),
        "the centre must be covered by a lit face, not the cleared background"
    );

    // All four corners lie outside the solid's silhouette. All four are checked because a single
    // corner cannot distinguish "the clear worked" from "the image is flipped".
    let last = IMAGE_SIZE - 1;
    for (x, y, label) in [
        (0, 0, "top-left"),
        (last, 0, "top-right"),
        (0, last, "bottom-left"),
        (last, last, "bottom-right"),
    ] {
        assert_eq!(
            frame.get_pixel(x, y),
            image::Rgba([0, 0, 0, 255]),
            "the {label} corner must still show the cleared background"
        );
    }

    eprintln!("OK: the CPU fixture renders a shaded solid natively, with no remoting involved");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rayland-icosa-cpu`
Expected: FAIL — the package does not exist yet.

- [ ] **Step 3: Write the shaders**

Create `shaders/icosa.vert`:

```glsl
#version 450

// The vertex attributes, matching rayland_icosa_core::geometry::Vertex field for field. The
// locations here and the VkVertexInputAttributeDescription offsets in pipeline.rs are two halves of
// one contract; changing either alone feeds positions into the normal slot and produces a
// plausible-looking but wrong picture.
layout(location = 0) in vec3 in_position;
layout(location = 1) in vec3 in_normal;
layout(location = 2) in vec2 in_uv;

// The model-view-projection matrix, rebuilt on the CPU every frame. This is the *only* thing that
// changes between frames in the GPU fixture, and one of two in the CPU fixture.
layout(binding = 0) uniform Uniforms {
    mat4 mvp;
    // The fractal view's half-width; unused by this shader but present so both fixtures share one
    // uniform block layout. See the fragment shaders.
    float half_width;
    vec2 center;
} u;

layout(location = 0) out vec3 frag_normal;
layout(location = 1) out vec2 frag_uv;

void main() {
    // The normal is passed through in model space, and the light direction below is given in model
    // space too, so the solid's lighting rotates with it — the faces catch the light as they turn,
    // which is what makes the rotation legible at all. Lighting in view space would leave every
    // face's brightness constant and the solid would look like it was not moving.
    frag_normal = in_normal;
    frag_uv = in_uv;
    gl_Position = u.mvp * vec4(in_position, 1.0);
}
```

Create `shaders/icosa_textured.frag`:

```glsl
#version 450

layout(location = 0) in vec3 frag_normal;
layout(location = 1) in vec2 frag_uv;

layout(location = 0) out vec4 out_color;

// The fractal, computed on the CPU and uploaded every frame. In the GPU fixture this binding does
// not exist and the fractal is evaluated here instead; that difference is the entire experiment.
layout(binding = 1) uniform sampler2D fractal;

// The light's direction, in model space, normalised. Fixed — not animated, not configurable — so
// that exactly one thing in the scene moves.
const vec3 LIGHT_DIRECTION = normalize(vec3(0.4, 0.7, 0.6));

// How much light a face receives when facing fully away. Without this, back-facing-but-visible
// faces go pure black and the silhouette dissolves into the background.
const float AMBIENT = 0.25;

void main() {
    // Lambert: brightness falls off with the cosine of the angle to the light. `max` clamps faces
    // turned away from the light to zero rather than letting them go negative and wrap.
    float diffuse = max(dot(normalize(frag_normal), LIGHT_DIRECTION), 0.0);
    float light = AMBIENT + (1.0 - AMBIENT) * diffuse;
    out_color = vec4(texture(fractal, frag_uv).rgb * light, 1.0);
}
```

For this task, the texture does not exist yet. Create a temporary `shaders/icosa_flat.frag` — identical but with `out_color = vec4(vec3(0.9, 0.5, 0.2) * light, 1.0);` and no sampler — use it for this task, and **delete it in Task 6** when the real texture arrives. This is what lets the depth-and-geometry half be proven before the texture half is written.

Compile all three, per `shaders/README.md`:

```bash
cd /home/roland/git/rayland
glslangValidator -V shaders/icosa.vert -o shaders/icosa.vert.spv
glslangValidator -V shaders/icosa_flat.frag -o shaders/icosa_flat.frag.spv
glslangValidator -V shaders/icosa_textured.frag -o shaders/icosa_textured.frag.spv
```

Update `shaders/README.md` to list the new sources and their regeneration commands, alongside the existing triangle ones.

- [ ] **Step 4: Write the manifest and the Vulkan bring-up**

Create `crates/rayland-icosa-cpu/Cargo.toml`:

```toml
# An ordinary off-screen Vulkan program that draws a spinning, fractal-textured icosahedron, with
# the fractal computed on this machine's CPU and uploaded to the GPU every frame.
#
# This crate is deliberately unaware of everything around it. It depends on no `rayland-*` crate
# except `rayland-icosa-core`, which is pure mathematics and knows nothing either. It contains no
# mention of remoting, and it cannot tell whether its rendering is happening locally. That ignorance
# is the whole point: an application that knew it was being remoted would prove nothing, and the
# only way to be sure the interception is invisible is for the program to have no way to see it.
#
# A BINARY crate, so per the repository license policy it is GPL-3.0-or-later.
[package]
name = "rayland-icosa-cpu"
version = "0.0.1"
edition = "2024"
rust-version = "1.85"
description = "An ordinary off-screen Vulkan program drawing a spinning icosahedron textured with a CPU-computed fractal."
license = "GPL-3.0-or-later"
repository = "https://github.com/perpetualbits/rayland"
publish = false

[[bin]]
name = "rayland-icosa-cpu"
path = "src/main.rs"

[dependencies]
rayland-icosa-core = { path = "../rayland-icosa-core" }  # geometry, schedule, fractal — no GPU, no remoting
ash = { workspace = true }                               # thin Vulkan bindings; the only way this program talks to a GPU
image = { workspace = true }                             # PNG encoding of the pixels read back from the GPU
anyhow = { workspace = true }                            # top-level error handling, per the repo's binary-crate convention

[dev-dependencies]
# The native test re-reads the PNGs the binary wrote and inspects individual pixels. Deliberately
# no `rayland-*` crate beyond the fixture's own: "renders correctly on its own" is exactly the
# baseline these tests establish, and a baseline that linked the system under test would not be one.
image = { workspace = true }
```

Create `crates/rayland-icosa-cpu/src/context.rs` by copying `crates/rayland-refapp/src/context.rs` and adapting it. That file already does exactly what is needed — instance creation, physical-device selection, queue-family selection, logical device, command pool, and an `allocate` helper that finds a memory type by property flags — and it is already reviewed and working. Changes required:

1. Rename the crate references in the module docs; keep the same structure and comment density.
2. `VulkanContext::new` needs no new extensions; the fixture is offscreen, exactly as refapp is.

Add the crate to the workspace `members` in the root `Cargo.toml`:

```toml
    "crates/rayland-icosa-cpu",      # fixture A: fractal on the CPU, uploaded per frame
```

- [ ] **Step 5: Write the pipeline, with depth**

Create `crates/rayland-icosa-cpu/src/pipeline.rs`, modelled on `crates/rayland-refapp/src/pipeline.rs`. Everything there carries over except that this pipeline has a **depth attachment**, a **vertex layout with three attributes**, and a **descriptor set**. The novel parts, in full:

```rust
/// The format of the colour attachment, and of the PNG written from it.
///
/// `R8G8B8A8_UNORM` is universally supported as a colour attachment, so no format negotiation is
/// needed, and it maps one-to-one onto the PNG's bytes with no conversion that could introduce a
/// rounding difference between hosts.
pub const COLOR_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// The format of the depth attachment.
///
/// `D32_SFLOAT` is chosen because the Vulkan specification *requires* every implementation to
/// support it as a depth-stencil attachment. That matters more here than usual: this is the first
/// depth buffer in this repository, and picking a format that needs `vkGetPhysicalDeviceFormatProperties`
/// negotiation would add a failure mode on the very path being brought up. A stencil-bearing format
/// would also be fine on most hardware and is not guaranteed; there is no stencil in this scene.
pub const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;
```

The render pass gains a second attachment:

```rust
// The depth attachment. `CLEAR` on load because every frame starts with nothing drawn, and
// `DONT_CARE` on store because — unlike the colour attachment — nothing ever reads the depth buffer
// after the render pass ends. Saying so explicitly lets a tiler discard it instead of writing a
// megabyte back to memory for nobody.
let depth_attachment = vk::AttachmentDescription::default()
    .format(DEPTH_FORMAT)
    .samples(vk::SampleCountFlags::TYPE_1)
    .load_op(vk::AttachmentLoadOp::CLEAR)
    .store_op(vk::AttachmentStoreOp::DONT_CARE)
    .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
    .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
    .initial_layout(vk::ImageLayout::UNDEFINED)
    .final_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL);
```

and the depth-stencil state must be enabled, which refapp's pipeline omits entirely:

```rust
// Without this, the pipeline defaults to no depth testing and the 20 faces paint over each other in
// submission order — the back of the solid drawn on top of the front, which looks like a scrambled
// mess rather than an obviously wrong picture. `LESS` with `depth_write_enable` is the ordinary
// opaque-geometry configuration: a fragment survives only if it is nearer than what is already
// there, and if it survives it becomes the new nearest.
let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
    .depth_test_enable(true)
    .depth_write_enable(true)
    .depth_compare_op(vk::CompareOp::LESS)
    .depth_bounds_test_enable(false)
    .stencil_test_enable(false);
```

The vertex input describes `rayland_icosa_core::geometry::Vertex`:

```rust
// One interleaved buffer holding position, normal and UV per vertex. The stride is the Rust
// struct's size, and the offsets are its field offsets: this description and `Vertex`'s `#[repr(C)]`
// layout are two halves of one contract, and a mismatch feeds one attribute's bytes into another's
// slot — producing a picture that renders happily and is wrong.
let binding = vk::VertexInputBindingDescription::default()
    .binding(0)
    .stride(std::mem::size_of::<Vertex>() as u32)
    .input_rate(vk::VertexInputRate::VERTEX);

let attributes = [
    // position: vec3, at offset 0
    vk::VertexInputAttributeDescription::default()
        .location(0)
        .binding(0)
        .format(vk::Format::R32G32B32_SFLOAT)
        .offset(0),
    // normal: vec3, after the position
    vk::VertexInputAttributeDescription::default()
        .location(1)
        .binding(0)
        .format(vk::Format::R32G32B32_SFLOAT)
        .offset(12),
    // uv: vec2, after the normal
    vk::VertexInputAttributeDescription::default()
        .location(2)
        .binding(0)
        .format(vk::Format::R32G32_SFLOAT)
        .offset(24),
];
```

Back-face culling is enabled (`cull_mode(vk::CullModeFlags::BACK)`, `front_face(vk::FrontFace::COUNTER_CLOCKWISE)`), matching the winding the geometry table produces. Do not disable culling to "fix" a missing face — that would mask a mis-wound face in the table, which Task 3's `all_normals_point_outward` test is the right place to catch.

The descriptor set layout has two bindings: binding 0 a `UNIFORM_BUFFER` visible to both stages, binding 1 a `COMBINED_IMAGE_SAMPLER` visible to the fragment stage. For this task the fragment shader is `icosa_flat.frag` and does not use binding 1, but declare both now so Task 6 changes only the shader.

- [ ] **Step 6: Write the render module and main, for one frame**

Create `crates/rayland-icosa-cpu/src/render.rs`, modelled on `crates/rayland-refapp/src/render.rs`'s `ColorTarget` and `HostBuffer`. Add a `DepthTarget` alongside `ColorTarget` — a `D32_SFLOAT` image with `DEVICE_LOCAL` memory and a `DEPTH` aspect image view, no readback path, since nothing reads it.

Create `crates/rayland-icosa-cpu/src/main.rs`. For this task it renders frame 0 only:

```rust
//! **An ordinary off-screen Vulkan program.** It draws a spinning icosahedron, textured with a
//! zooming Mandelbrot fractal that it computes on this machine's CPU, and writes each frame to a
//! PNG. That is all it does.
//!
//! # Usage
//! ```text
//! rayland-icosa-cpu <output-directory>
//! ```
//! Writes `frame_0000.png` … `frame_0119.png` into the directory, and prints per-frame timings to
//! stdout as CSV.
//!
//! # Pitfall for anyone changing this
//! Resist the temptation to make it "better" in ways that make it special. No command-line
//! rendering options, no environment probing, no conditional paths. Its value is precisely that it
//! is boring and typical.
```

Parse exactly one argument, the output directory. Bring up the context, build the pipeline, allocate the targets, upload the vertex buffer once, write `frame_mvp(0)` into the uniform buffer, draw, read back, write `frame_0000.png`.

- [ ] **Step 7: Run the test to verify it passes**

Run: `cargo test -p rayland-icosa-cpu`
Expected: PASS — `icosa_cpu_natively_renders_a_shaded_solid`. On a host with no render node: SKIP.

Then look at `/tmp/rayland-icosa-cpu-native/frame_0000.png`. You should see a clearly faceted icosahedron, lit from the upper right, in orange, on black — with **hard edges between faces**. If the faces blend smoothly into one another, the normals are being shared or interpolated and Task 3's construction should be suspected. If the solid looks scrambled, with faces visibly punching through one another, depth testing is not enabled.

- [ ] **Step 8: Update `CLAUDE.md` and commit**

Change the crate count from "thirteen" to "fourteen" and add the crate entry, after `rayland-icosa-core`:

```markdown
- **`crates/rayland-icosa-cpu`** — fixture A: an ordinary offscreen Vulkan program drawing a
  spinning icosahedron textured with a fractal it computes on **its own CPU** and writes into
  persistently-mapped memory every frame. Depends only on `rayland-icosa-core` (pure mathematics)
  and knows nothing about remoting. GPL, `publish = false`.
```

```bash
git add crates/rayland-icosa-cpu Cargo.toml CLAUDE.md shaders/
git commit -m "icosa Task 5: the CPU fixture draws a shaded, depth-tested solid

Frame 0 only, flat-shaded in a solid colour — the texture comes next. Stopping
here is deliberate: the texture is the part most likely to break against the
remoting path, and proving the geometry and the depth buffer first means that
when it does break, the search space is one thing wide.

This is the repository's first depth attachment. D32_SFLOAT because the spec
requires every implementation to support it, so no format negotiation is
needed on the very path being brought up."
```

---

### Task 6: `rayland-icosa-cpu` — the mapped fractal, all 120 frames

**Files:**
- Modify: `crates/rayland-icosa-cpu/src/render.rs`, `src/main.rs`, `src/pipeline.rs`
- Delete: `shaders/icosa_flat.frag`, `shaders/icosa_flat.frag.spv`
- Modify: `crates/rayland-icosa-cpu/tests/native_render.rs`

**Interfaces:**
- Consumes: everything from Task 5, plus `rayland_icosa_core::{TEXTURE_SIZE, FRAME_COUNT, fractal::render_into, schedule::frame_zoom}`.
- Produces: the finished fixture A — `frame_0000.png` … `frame_0119.png` plus a CSV timing report on stdout.

**This is the task the whole fixture exists for.** Everything before it was scaffolding.

- [ ] **Step 1: Extend the test**

Add to `crates/rayland-icosa-cpu/tests/native_render.rs`:

```rust
/// The app must render all 120 frames, and they must differ from one another.
///
/// "All 120 exist" catches a loop that stops early. "Frame 0 and frame 119 differ" catches the
/// far more insidious failure: a loop that runs but re-uploads the same texture every time, or
/// never updates the matrix — which would produce 120 identical files that pass every other check
/// here and would make the end-to-end comparison meaningless, since a static picture is trivially
/// easy to transport correctly.
#[test]
fn icosa_cpu_renders_all_frames_and_they_differ() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!("SKIP icosa_cpu_renders_all_frames_and_they_differ: no render node at {RENDER_NODE}");
        return;
    }

    let output_dir = std::env::temp_dir().join("rayland-icosa-cpu-frames");
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir).expect("the output directory must be creatable");

    let status = Command::new(env!("CARGO_BIN_EXE_rayland-icosa-cpu"))
        .arg(&output_dir)
        .status()
        .expect("the fixture binary must be launchable");
    assert!(status.success(), "the fixture must exit successfully; got {status}");

    for frame in 0..120u32 {
        let path = output_dir.join(format!("frame_{frame:04}.png"));
        assert!(path.exists(), "frame {frame} must have been written to {path:?}");
    }

    let first = std::fs::read(output_dir.join("frame_0000.png")).expect("frame 0 must be readable");
    let last = std::fs::read(output_dir.join("frame_0119.png")).expect("frame 119 must be readable");
    assert_ne!(
        first, last,
        "the solid must rotate and the fractal must zoom — 120 identical frames would mean neither is happening"
    );
}

/// The app must print one CSV line per frame, with a header.
///
/// The timing report is the fixture's other output, and the reason a *pair* of fixtures exists at
/// all: the comparison between them is a comparison of numbers. A run that silently stopped
/// printing them would still pass every image check above.
#[test]
fn icosa_cpu_prints_a_timing_report() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!("SKIP icosa_cpu_prints_a_timing_report: no render node at {RENDER_NODE}");
        return;
    }

    let output_dir = std::env::temp_dir().join("rayland-icosa-cpu-timing");
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir).expect("the output directory must be creatable");

    let output = Command::new(env!("CARGO_BIN_EXE_rayland-icosa-cpu"))
        .arg(&output_dir)
        .output()
        .expect("the fixture binary must be launchable");
    let stdout = String::from_utf8(output.stdout).expect("the timing report must be valid UTF-8");
    let lines: Vec<&str> = stdout.lines().collect();

    assert_eq!(
        lines[0], "frame,fractal_us,upload_us,draw_readback_us",
        "the report must start with its header"
    );
    assert_eq!(lines.len(), 121, "one header line plus one line per frame");
    // Spot-check the shape of a data line rather than its values, which are timings and vary.
    let fields: Vec<&str> = lines[1].split(',').collect();
    assert_eq!(fields.len(), 4, "each data line must have four fields");
    assert_eq!(fields[0], "0", "the first data line must be frame 0");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rayland-icosa-cpu`
Expected: FAIL — `icosa_cpu_renders_all_frames_and_they_differ` fails on the missing `frame_0001.png`; `icosa_cpu_prints_a_timing_report` fails on empty stdout.

- [ ] **Step 3: Add the persistently-mapped staging buffer**

In `crates/rayland-icosa-cpu/src/render.rs`, add:

```rust
/// The fractal's staging buffer: host-visible memory the CPU writes and the GPU copies from.
///
/// # The pointer is held for the program's whole life
/// `vkMapMemory` is called **once**, in [`FractalStaging::new`], and the raw pointer it returns is
/// kept and written through on every frame thereafter. This is not an optimisation; it is what
/// ordinary Vulkan programs do, and doing anything else here would make this fixture unrepresentative
/// of the applications it stands in for. Mapping and unmapping around each write would be slower and
/// would also be a lie.
///
/// # The memory is HOST_COHERENT, so there is no flush
/// Coherent memory needs no `vkFlushMappedMemoryRanges`: the specification guarantees the write is
/// visible to the device without one. So this program makes **no Vulkan call whatsoever** between
/// writing a megabyte of pixels and issuing the copy that reads them. That is the ordinary, correct,
/// idiomatic thing to do, and it is chosen here deliberately rather than incidentally.
pub struct FractalStaging {
    /// The buffer the copy command reads from.
    pub buffer: vk::Buffer,
    /// The memory backing it. Freed on drop, after the mapping is torn down.
    memory: vk::DeviceMemory,
    /// The live mapping. Valid from construction until drop.
    mapped: *mut u8,
    /// How many bytes the mapping covers, so the slice handed out is bounded.
    size: usize,
}

impl FractalStaging {
    /// Allocate the staging buffer and map it, once.
    ///
    /// # Failure modes
    /// Returns an error if the allocation fails or if no memory type is both `HOST_VISIBLE` and
    /// `HOST_COHERENT`. The latter cannot happen on a conformant implementation — Vulkan requires at
    /// least one such type to exist — but is reported rather than assumed.
    pub unsafe fn new(context: &VulkanContext, size: usize) -> anyhow::Result<FractalStaging> {
        let buffer_info = vk::BufferCreateInfo::default()
            .size(size as u64)
            // TRANSFER_SRC: this buffer is only ever the source of a copy into the texture image.
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = context.device.create_buffer(&buffer_info, None)?;

        let requirements = context.device.get_buffer_memory_requirements(buffer);
        // HOST_VISIBLE so the CPU can map it at all; HOST_COHERENT so no explicit flush is needed.
        // See the type's documentation for why coherent is the deliberate choice and not a shortcut.
        let memory = allocate(
            context,
            requirements,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        context.device.bind_buffer_memory(buffer, memory, 0)?;

        // The one and only map. From here until drop, the CPU writes through this pointer.
        let mapped = context
            .device
            .map_memory(memory, 0, size as u64, vk::MemoryMapFlags::empty())?
            as *mut u8;

        Ok(FractalStaging { buffer, memory, mapped, size })
    }

    /// The mapped bytes, as a slice the fractal renderer can fill.
    ///
    /// # Failure modes
    /// None, but the returned slice aliases GPU-visible memory: writing to it while the device is
    /// reading from it is a data race that Vulkan does not protect against. The frame loop's fence
    /// wait is what makes this safe, and it is the caller's job to have done it.
    pub fn pixels(&mut self) -> &mut [u8] {
        // Safe because `mapped` is valid for `size` bytes from construction until drop, and `&mut
        // self` guarantees no other Rust reference to it exists.
        unsafe { std::slice::from_raw_parts_mut(self.mapped, self.size) }
    }
}
```

`Drop` must `unmap_memory` before `free_memory` and destroy the buffer, in that order.

Add a `FractalTexture` beside it: a `DEVICE_LOCAL` `R8G8B8A8_UNORM` image of `TEXTURE_SIZE` square with `TRANSFER_DST | SAMPLED` usage, its image view, and a `vk::Sampler` with `LINEAR` filtering and `CLAMP_TO_EDGE` addressing.

- [ ] **Step 4: Write the frame loop**

Replace the single-frame body of `main.rs` with:

```rust
// The timing report's header. Ordinary profiling output: it measures this program's own work with
// this program's own clock. Note carefully that the clock only ever *measures* and never *decides* —
// nothing about what is drawn depends on a timing value. If that ever changes, every frame stops
// being reproducible and this program stops being useful.
println!("frame,fractal_us,upload_us,draw_readback_us");

for frame in 0..rayland_icosa_core::FRAME_COUNT {
    // Wait for the previous frame to finish before touching the mapped memory it was reading from.
    // Without this the CPU would overwrite the staging buffer while the GPU's copy was still in
    // flight, and the texture would be a torn mixture of two frames — which looks exactly like a
    // corrupted transport and would be a miserable thing to debug.
    wait_for_previous_frame(&context, fence)?;

    // 1. The fractal, straight into mapped host-visible memory. No Vulkan call is involved in this
    //    step at all: it is a plain memory write through a pointer obtained once at startup.
    let fractal_start = Instant::now();
    rayland_icosa_core::fractal::render_into(
        staging.pixels(),
        rayland_icosa_core::schedule::frame_zoom(frame),
    );
    let fractal_us = fractal_start.elapsed().as_micros();

    // 2. The matrix, likewise straight into mapped memory.
    write_uniforms(&mut uniforms, rayland_icosa_core::schedule::frame_mvp(frame));

    // 3. The upload: the copy command that reads what step 1 wrote.
    let upload_start = Instant::now();
    record_and_submit_upload(&context, &staging, &texture)?;
    let upload_us = upload_start.elapsed().as_micros();

    // 4. Draw and read back.
    let draw_start = Instant::now();
    record_and_submit_draw(&context, &pipeline, &targets, fence)?;
    let pixels = read_back(&context, &targets)?;
    let draw_readback_us = draw_start.elapsed().as_micros();

    // 5. The artefact. Written by the application itself, from pixels it read back — not by anything
    //    else on its behalf.
    let path = output_dir.join(format!("frame_{frame:04}.png"));
    image::save_buffer(
        &path,
        &pixels,
        rayland_icosa_core::IMAGE_SIZE,
        rayland_icosa_core::IMAGE_SIZE,
        image::ColorType::Rgba8,
    )?;

    println!("{frame},{fractal_us},{upload_us},{draw_readback_us}");
}
```

Switch the pipeline to `icosa_textured.frag.spv`, write the descriptor set's binding 1 to the texture's view and sampler, and delete `shaders/icosa_flat.frag` and its `.spv`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p rayland-icosa-cpu`
Expected: PASS — all four native tests.

- [ ] **Step 6: Look at the result**

Run the fixture and inspect a spread of frames:

```bash
cargo run -q -p rayland-icosa-cpu -- /tmp/icosa-cpu
```

Open `frame_0000.png`, `frame_0060.png`, `frame_0119.png`. Each face should carry a visibly identical, recognisable Mandelbrot image, shaded by the light; the solid should be at a different angle in each; the fractal should be visibly deeper in each. If the fractal is upside down relative to what a fractal viewer shows, that is expected and correct — Vulkan's texture origin is top-left — and must not be "fixed" by flipping the UVs, because it is not wrong.

Also check the timing report's shape: `fractal_us` should dominate `upload_us` and `draw_readback_us` by a wide margin on any normal desktop. That is the number Task 7's fixture exists to contrast with.

- [ ] **Step 7: Commit**

```bash
git add crates/rayland-icosa-cpu shaders/
git rm shaders/icosa_flat.frag shaders/icosa_flat.frag.spv
git commit -m "icosa Task 6: the mapped fractal, all 120 frames

This is the task the fixture exists for. vkMapMemory is called once at
startup; every frame thereafter writes a megabyte of freshly computed
Mandelbrot pixels straight through that pointer and then issues a copy. The
memory is HOST_COHERENT, so no flush is required and none is made — there is
no Vulkan call anywhere between the write and the copy that reads it.

That is the ordinary, idiomatic thing for a Vulkan program to do, and it is
also precisely the case with nothing on the wire to intercept. Both facts at
once are the problem this workload is here to state in executable form.

Also adds the CSV timing report. The clock measures and never decides:
nothing about what is drawn depends on a timing value."
```

---

### Task 7: `rayland-icosa-gpu` — the control

**Files:**
- Create: `crates/rayland-icosa-gpu/` (manifest, `main.rs`, `context.rs`, `pipeline.rs`, `render.rs`, `tests/native_render.rs`)
- Create: `shaders/icosa_fractal.frag` (+ `.spv`)
- Modify: `Cargo.toml`, `CLAUDE.md`, `shaders/README.md`

**Interfaces:**
- Consumes: `rayland_icosa_core::{IMAGE_SIZE, FRAME_COUNT, MAX_ITER, geometry::*, schedule::{frame_mvp, frame_zoom, CENTER}}`.
- Produces: a binary `rayland-icosa-gpu <output-directory>` writing 120 PNGs and the same CSV.

Fixture B is fixture A with the texture path removed: no staging buffer, no texture image, no sampler, no copy. The fragment shader evaluates the fractal instead. Everything else — geometry, schedule, lighting, resolution, frame count, iteration ceiling — is identical, because that identity is the only reason the pair is worth having.

- [ ] **Step 1: Write the failing test**

Create `crates/rayland-icosa-gpu/tests/native_render.rs` as a copy of fixture A's, with `rayland-icosa-cpu` replaced by `rayland-icosa-gpu` throughout and the temp directories renamed to match. The assertions are deliberately identical: both fixtures draw the same scene, so both baselines check the same things. (Repeated rather than shared: a test helper crate would be a `rayland-*` dependency, and these tests must not have one.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rayland-icosa-gpu`
Expected: FAIL — the package does not exist.

- [ ] **Step 3: Write the fragment shader**

Create `shaders/icosa_fractal.frag`:

```glsl
#version 450

layout(location = 0) in vec3 frag_normal;
layout(location = 1) in vec2 frag_uv;

layout(location = 0) out vec4 out_color;

// The same uniform block the vertex shader reads, with the fractal's view parameters live in it.
// In the CPU fixture these two fields are ignored and the fractal arrives as a texture; here they
// are all the fractal needs. That difference — one megabyte per frame versus these twelve bytes —
// is the entire experiment.
layout(binding = 0) uniform Uniforms {
    mat4 mvp;
    float half_width;
    vec2 center;
} u;

const vec3 LIGHT_DIRECTION = normalize(vec3(0.4, 0.7, 0.6));
const float AMBIENT = 0.25;

// Must match rayland_icosa_core::MAX_ITER. Not a uniform: it is a fixed property of the workload,
// and making it settable would let the two fixtures be run with different ceilings, which would
// quietly destroy the only thing the pair is for.
const int MAX_ITER = 512;

// A base-2 logarithm built from the exponent decomposition and the same truncated odd series as
// rayland_icosa_core::exact_math::log2, rather than GLSL's built-in log2.
//
// The reproducibility argument that forces this on the CPU side does not, strictly, apply here:
// this shader runs on one GPU and produces the same answer every time it does. It is transcribed
// anyway so that the two fixtures compute the same function of the same inputs, which is what lets
// their outputs be compared to each other as well as each to its own baseline.
float exact_log2(float x) {
    // frexp splits x into a mantissa in [0.5, 1) and an exponent, exactly — the same field
    // extraction the Rust version does by hand, which GLSL exposes directly.
    int exponent;
    float mantissa = frexp(x, exponent);
    // Shift the mantissa into [1, 2) to match the Rust version's range, adjusting the exponent.
    mantissa *= 2.0;
    exponent -= 1;

    float t = (mantissa - 1.0) / (mantissa + 1.0);
    float t2 = t * t;
    float poly = 1.0 / 15.0;
    poly = poly * t2 + 1.0 / 13.0;
    poly = poly * t2 + 1.0 / 11.0;
    poly = poly * t2 + 1.0 / 9.0;
    poly = poly * t2 + 1.0 / 7.0;
    poly = poly * t2 + 1.0 / 5.0;
    poly = poly * t2 + 1.0 / 3.0;
    poly = poly * t2 + 1.0;
    return float(exponent) + 2.885390081777926814 * t * poly;
}

// The same HSV ramp as the Rust version's hsv_to_rgb, with the same smoothstep.
vec3 hsv2rgb(vec3 c) {
    vec3 rgb = clamp(abs(mod(c.x * 6.0 + vec3(0.0, 4.0, 2.0), 6.0) - 3.0) - 1.0, 0.0, 1.0);
    rgb = rgb * rgb * (3.0 - 2.0 * rgb);
    return c.z * mix(vec3(1.0), rgb, c.y);
}

void main() {
    // The face's UV, which spans an equilateral triangle inscribed in the unit square, is mapped
    // onto the complex plane exactly as the CPU fixture maps its texture's pixel grid — so the two
    // fixtures show the same region of the fractal on the same face.
    vec2 offset = frag_uv - vec2(0.5);
    vec2 c = u.center + offset * 2.0 * u.half_width;

    vec2 z = vec2(0.0);
    int i;
    for (i = 0; i < MAX_ITER; i++) {
        if (dot(z, z) > 4.0) break;
        z = vec2(z.x * z.x - z.y * z.y, 2.0 * z.x * z.y) + c;
    }

    vec3 fractal;
    if (i >= MAX_ITER) {
        // Inside the set.
        fractal = vec3(0.0);
    } else {
        // The smooth iteration count, matching the Rust version term for term: log2(|z|) is
        // log2(|z|²)/2, which avoids a square root.
        float log_modulus = exact_log2(dot(z, z)) / 2.0;
        float smooth_iter = float(i) + 1.0 - exact_log2(log_modulus);
        // Divided by 64, not MAX_ITER, for the reason the Rust version explains: almost every point
        // escapes early, so dividing by the full budget would compress the palette to a sliver.
        fractal = hsv2rgb(vec3(smooth_iter / 64.0, 0.85, 1.0));
    }

    float diffuse = max(dot(normalize(frag_normal), LIGHT_DIRECTION), 0.0);
    float light = AMBIENT + (1.0 - AMBIENT) * diffuse;
    out_color = vec4(fractal * light, 1.0);
}
```

Compile it and add it to `shaders/README.md`:

```bash
glslangValidator -V shaders/icosa_fractal.frag -o shaders/icosa_fractal.frag.spv
```

- [ ] **Step 4: Write the crate**

Copy `crates/rayland-icosa-cpu` to `crates/rayland-icosa-gpu` and remove the texture path: no `FractalStaging`, no `FractalTexture`, no sampler, no `COMBINED_IMAGE_SAMPLER` descriptor binding, no upload submission, no call to `fractal::render_into`. The uniform buffer gains the `half_width` and `center` fields the shader reads — and note these are still written through a persistent mapping, exactly as fixture A's are. The difference between the fixtures is the *volume* of mapped writes, not their presence; both write through a pointer with no interceptable call, and stating that clearly in the crate documentation is important, because a reader will otherwise assume the GPU fixture "avoids" mapped memory. It does not.

The CSV keeps all four columns for comparability. `fractal_us` is measured around the (now trivial) uniform write, and `upload_us` around nothing at all — record it as `0`. Keeping the columns identical means the two runs' reports can be diffed directly, which is the point.

Add to the workspace `members`:

```toml
    "crates/rayland-icosa-gpu",      # fixture B: fractal in a fragment shader; the volume control
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p rayland-icosa-gpu`
Expected: PASS — all four native tests.

- [ ] **Step 6: Compare the two fixtures**

This is the moment the pair first produces its number:

```bash
cargo run -q -p rayland-icosa-cpu -- /tmp/icosa-cpu > /tmp/cpu.csv
cargo run -q -p rayland-icosa-gpu -- /tmp/icosa-gpu > /tmp/gpu.csv
```

Compare the reports. Expect `fractal_us` to be several orders of magnitude apart. Compare `frame_0060.png` from each by eye: they must show *recognisably the same picture*, with small differences from `f32` versus `f64` and from per-pixel versus per-texel sampling. They will **not** be bit-identical, and must not be expected to be — each fixture is compared against its own baseline, never against the other.

Both fixtures now evaluate the fractal only where a face can see it: A because of its explicit triangle test, B because its rasteriser gives it that for free. That parity is what makes the `fractal_us` ratio mean "CPU versus GPU" rather than "CPU doing three times the work". If someone later removes A's triangle test as a simplification, this comparison silently gains a factor of three — which is why `the_triangle_covers_about_a_third_of_the_texture` and the module documentation both exist to object.

If the two look like different fractals rather than the same one at different quality, the shader's UV-to-complex-plane mapping disagrees with the Rust version's pixel-to-complex-plane mapping, and that must be fixed before either becomes a reference.

- [ ] **Step 7: Update `CLAUDE.md` and commit**

Change the crate count from "fourteen" to "fifteen" and add:

```markdown
- **`crates/rayland-icosa-gpu`** — fixture B: the same spinning icosahedron, same geometry, same
  schedule, same fractal arithmetic — but evaluated in a fragment shader, so roughly 128 bytes per
  frame cross mapped memory instead of a megabyte. It is the **volume control** for
  `rayland-icosa-cpu`, not an alternative to it: note that it still writes its uniforms through a
  persistent mapping with no interceptable call, so the pair isolates how cost scales with
  mapped-write volume, not the presence of mapped writes. GPL, `publish = false`.
```

```bash
git add crates/rayland-icosa-gpu Cargo.toml CLAUDE.md shaders/
git commit -m "icosa Task 7: the GPU fixture — the volume control

Same geometry, same schedule, same fractal arithmetic, same lighting, same
resolution, same iteration ceiling. The only difference is where the fractal
is evaluated, and therefore how many bytes per frame cross mapped memory:
roughly 128 against roughly a megabyte.

It does not 'avoid' mapped memory — it still writes its uniforms through a
persistent mapping with no interceptable call, exactly as the CPU fixture
does. What the pair isolates is how cost scales with mapped-write volume,
across four orders of magnitude of it, which is the question that actually
needs answering."
```

---

### Task 8: The end-to-end proofs

**Files:**
- Create: `crates/rayland-engine/tests/icosa_cpu_venus_e2e.rs`
- Create: `crates/rayland-engine/tests/icosa_gpu_venus_e2e.rs`
- Modify: `crates/rayland-engine/Cargo.toml` (dev-dependencies)

**Interfaces:**
- Consumes: the two fixture binaries; `rayland-engine`'s existing test harness.
- Produces: the fixtures' end-to-end verdict.

**Read `crates/rayland-engine/tests/refapp_venus_e2e.rs` first and follow it exactly.** It already solves every environment problem these tests have — launching a fixture binary with `VK_ICD_FILENAMES` pointed at Mesa's Venus ICD and `VTEST_SOCKET_NAME` pointed at the engine's socket, gating on `virgl_available`, and tearing the server down. `docs/c0-venus-first-light.md` §"The environment pitfalls" lists the traps; every one of them cost real debugging time already and none needs to be rediscovered.

**Expect these to fail.** See Task 9. That is the finding, not a blocker.

- [ ] **Step 1: Write the CPU fixture's end-to-end test**

Create `crates/rayland-engine/tests/icosa_cpu_venus_e2e.rs`:

```rust
//! The CPU fixture's end-to-end proof: the same unmodified binary, rendered through the remoting
//! path, must produce **bit-identical** PNGs to its native run.
//!
//! # Why bit-identical is a fair demand and not a harsh one
//! Both runs draw on the same GPU with the same driver — the native baseline uses it directly, the
//! remoted run reaches it through the engine. The fractal is computed on the CPU in both runs, and
//! bit-exactly (see `rayland-icosa-core`'s `exact_math`), so it contributes nothing to a
//! difference. The *only* thing that changes between the two runs is how the commands reached the
//! GPU. Any pixel difference is therefore a defect in that path, which is exactly the assertion
//! this test wants to make.
//!
//! # Why every frame is compared, not just the last
//! A defect that corrupts one intermediate frame and then self-corrects — a delta applied late, an
//! upload racing a draw — is invisible to a final-frame check, and is exactly the kind of thing the
//! relay and the coherence work can produce. At 256×256 the full comparison is roughly 24 MiB, so
//! comparing all 120 costs nothing worth saving.
//!
//! # A tolerance would be worse than useless here
//! The bugs this path produces are usually *small* before they are large: a dropped mapped write, a
//! stale texture, a delta applied out of order. A tolerance is precisely where those would live.
```

The test body:
1. Skip unless `virgl_available()` and the render node exists.
2. Run `CARGO_BIN_EXE_rayland-icosa-cpu` natively into one directory, asserting success.
3. Start the engine's vtest server, run the same binary with the Venus environment into a second directory, asserting success.
4. For each of the 120 frames, read both files and `assert_eq!` their **bytes**. Report the first mismatching frame by number and stop, rather than dumping 120 failures — the first divergence is the diagnostic and the rest are noise.

`CARGO_BIN_EXE_rayland-icosa-cpu` is only defined for `rayland-icosa-cpu`'s own tests. From `rayland-engine`, follow whatever mechanism `refapp_venus_e2e.rs` already uses to locate the refapp binary and use the same one; if it resolves a path under `target/`, add `rayland-icosa-cpu` and `rayland-icosa-gpu` as `dev-dependencies` of `rayland-engine` so Cargo builds them first.

**Note the one place this crosses the fixtures' isolation rule and why it does not violate it:** `rayland-engine` depends on the fixtures, not the other way round. The fixtures still know nothing. The arrow's direction is the whole distinction.

- [ ] **Step 2: Run it**

Run: `cargo test -p rayland-engine --test icosa_cpu_venus_e2e -- --nocapture`
Expected: **FAIL**, most likely at frame 0 or at startup, and most likely on depth-attachment support or on the per-frame mapped texture never arriving. Capture the exact failure — that is this task's deliverable.

Do **not** adjust the fixture to make this pass. See Task 9.

- [ ] **Step 3: Write the GPU fixture's end-to-end test**

Create `crates/rayland-engine/tests/icosa_gpu_venus_e2e.rs` — the same test against `rayland-icosa-gpu`. Repeat the code rather than sharing it; the two tests will diverge as their findings differ, and a shared helper would couple them.

- [ ] **Step 4: Run it**

Run: `cargo test -p rayland-engine --test icosa_gpu_venus_e2e -- --nocapture`
Expected: **FAIL**, but plausibly *later* and for different reasons than the CPU one — it has no texture upload, so if it fails it is the depth buffer or the geometry rather than mapped-memory volume. The difference between the two failures is itself a finding.

- [ ] **Step 5: Commit**

```bash
git add crates/rayland-engine
git commit -m "icosa Task 8: the end-to-end proofs (expected to fail)

Both fixtures, run through the remoting path, must produce bit-identical PNGs
to their native runs. Both runs draw on the same GPU with the same driver and
compute the fractal bit-exactly on the CPU, so the only thing that differs is
how the commands reached the GPU — which makes any pixel difference a defect
in that path, and makes a tolerance the wrong tool. Small dropped writes and
stale textures are exactly what a tolerance would hide.

Every frame is compared, not just the last: a defect that corrupts one
intermediate frame and self-corrects is invisible to a final-frame check and
is exactly what a relay can produce.

These fail today. That is the deliverable — see the plan's Task 9."
```

---

### Task 9: Record what the fixtures found

**Files:**
- Create: `docs/icosa-fixtures.md`
- Modify: `docs/design/2026-07-16-icosa-fixtures.md` (the `sin`/`cos` amendment)

**Interfaces:**
- Consumes: the failures from Task 8 and the timing reports from Tasks 6–7.
- Produces: the document the (c)2 work starts from.

The fixtures' purpose is to produce findings, and a finding nobody wrote down is not one. This task turns Task 8's failures into the thing (c)2 reads first, in the register of `docs/c0-venus-first-light.md`: written for someone who does not know the domain, complete rather than brief, and honest about scope.

- [ ] **Step 1: Amend the spec**

The spec's §5.4 names `log` as the only libm trap and §5.5 claims any pixel difference is a Rayland defect. Task 2 established that `sin`/`cos` are the same trap and that §5.5's claim is false without them being exact too. Update both sections to say so, and note the amendment date. A spec that no longer matches the code is a bug by `CLAUDE.md`'s rule.

- [ ] **Step 2: Write the findings document**

Create `docs/icosa-fixtures.md` covering:
- **What the fixtures are and how to run them** — the exact commands, including the Venus environment, cross-referencing `docs/c0-venus-first-light.md` rather than repeating its pitfalls.
- **What the two timing reports actually said.** Real numbers from Tasks 6 and 7, natively. State the machine they were measured on; a timing without its machine is not a measurement.
- **What broke, in order, and what each failure means.** For each: the exact error, whether it is a coverage gap (something Rayland does not implement yet) or a design limit (something the current design cannot do, by construction), and which sub-project owns it.
- **What is still unknown.** Explicitly: anything the fixtures could not reach because something earlier failed first. This section is the one most likely to be skipped and the most valuable to (c)2.

- [ ] **Step 3: Commit**

```bash
git add docs/icosa-fixtures.md docs/design/2026-07-16-icosa-fixtures.md
git commit -m "icosa Task 9: what the fixtures found

The fixtures exist to produce findings and a finding nobody wrote down is not
one. Records the native timing reports with the machine they came from, each
end-to-end failure with whether it is a coverage gap or a design limit, and —
most importantly for whoever picks up (c)2 — what the fixtures could not reach
because something earlier failed first.

Also amends the spec: it named log as the only libm trap, and sin/cos are the
same trap. Its claim that any pixel difference indicates a defect is false
unless both are bit-exact."
```

---

## Notes for whoever executes this

**The fixtures are supposed to fail against Rayland.** Task 8's tests are red on delivery, and Task 9 writes down *why*. Do not weaken a fixture to make a test green. If `icosa-cpu` fails because per-frame mapped texture writes do not survive the relay, that is the design working exactly as currently specified — (c)1 never claimed to solve mapped memory — and the fixture's job is to say so precisely and reproducibly rather than to pass. A fixture that passes because it was made easier has destroyed its own reason for existing.

**The determinism constraints are not style.** Every `log`, `sin`, `cos`, `powi` or `Instant::now` that creeps into a pixel path silently converts a sharp test into a flaky one, and the flakiness will show up months later as a phantom Rayland bug. The table tests in Tasks 1–2 are the guard rail; do not relax them.

**The isolation rule is not style either.** The moment a fixture gains a flag, an environment check, or a `rayland-*` dependency beyond `icosa-core`, it stops being evidence about how real applications behave and becomes evidence about how this fixture behaves. That is a much less interesting thing to know.

**The two fixtures must stay identical in everything but one property.** This is easier to break than it looks, and the triangle restriction in `fractal.rs` is the standing example: the GPU fixture is confined to the visible region by its rasteriser, so the CPU fixture has to be confined explicitly, or it silently does three times the arithmetic and the whole comparison is wrong by that factor while looking entirely plausible. Before changing either fixture, ask what the change does to the *other* one. If the answer is "nothing", that is usually the bug.
