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
//! (`+ - * /`, comparison, square root, `round`) and bit manipulation. None of them calls a libm
//! transcendental. Square root is in that list deliberately, not by oversight: IEEE-754 specifies it
//! exactly, the same as the four arithmetic operators, so — unlike `log`, `sin` or `cos` — it needs
//! no reproducible replacement from [`exact_math`] and does not weaken this guarantee.
//! `geometry`'s private `normalize` function is the example: it calls `sqrt` directly to build
//! vertex positions and normals. See [`exact_math`] for why the transcendental restriction exists;
//! it is the single most surprising thing about this crate and the easiest to accidentally undo.

// Reproducible replacements for the libm functions the rest of the crate would otherwise need.
pub mod exact_math;

// The solid itself: its 60 vertices, their flat normals and their texture coordinates.
pub mod geometry;

// Where the solid points and how deep the fractal is zoomed, at each frame.
pub mod schedule;
// The zooming Mandelbrot image the faces are textured with.
pub mod fractal;

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
/// 512, not the 2000 a GPU-only interactive program can afford. The CPU fixture does *not* evaluate
/// this loop for all 512×512 pixels: [`fractal`] restricts iteration to the sampled triangle plus a
/// small filtering margin (see that module's doc comment), which together cover ~33.5% of the
/// texture. So the true worst case is `262144 × 0.335 × 512 ≈ 45.0 million` iterations per frame, on
/// a machine that may be a modest single-board computer — heavy enough to be honest about a weak
/// CPU, light enough not to swamp the measurement. It is also ample detail at the zoom depth this run
/// reaches.
///
/// **Both fixtures use this same value.** Giving the GPU one a different ceiling because it can
/// afford one would destroy the only thing the pair is for.
pub const MAX_ITER: u32 = 512;
