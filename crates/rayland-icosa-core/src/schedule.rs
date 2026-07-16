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
//! In particular `f64::powi` and `f64::powf` are **not** used. Neither is IEEE-specified; both lower
//! to library or intrinsic code whose expansion is a quality-of-implementation matter, and either
//! could legitimately differ between targets. The loop below is longer and is exactly reproducible.

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
pub const CENTER: (f64, f64) = (-0.743_643_887_037_151, 0.131_825_904_205_33);

/// How far the solid turns about the vertical axis each frame, in radians.
///
/// The solid never returns to a previous orientation during a run, so all 120 frames are genuinely
/// distinct and a defect that affects only some orientations cannot hide behind a repeat. The reason
/// is simpler than it might look: at this rate, yaw's own period is `2π / 0.031 ≈ 202.7` frames, well
/// past [`crate::FRAME_COUNT`] = 120, so across one run yaw itself never completes a full turn and is
/// therefore injective over `0..120` — no repeat is possible regardless of what pitch does. (0.031
/// and [`PITCH_PER_FRAME`] are *not* incommensurate, for what that is worth: `0.031 / 0.017 = 31/17`
/// exactly, a rational ratio, which is the definition of commensurate — but that fact plays no role
/// in the no-repeat argument above.)
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
        [
            (sx * r00) as f32,
            (sy * r10) as f32,
            (sz * r20) as f32,
            (-r20) as f32,
        ],
        [
            (sx * r01) as f32,
            (sy * r11) as f32,
            (sz * r21) as f32,
            (-r21) as f32,
        ],
        [
            (sx * r02) as f32,
            (sy * r12) as f32,
            (sz * r22) as f32,
            (-r22) as f32,
        ],
        [
            0.0,
            0.0,
            (sz * -CAMERA_DISTANCE + tz) as f32,
            CAMERA_DISTANCE as f32,
        ],
    ]
}

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
        let forward: Vec<u64> = (0..crate::FRAME_COUNT)
            .map(|f| frame_zoom(f).to_bits())
            .collect();
        let backward: Vec<u64> = (0..crate::FRAME_COUNT)
            .rev()
            .map(|f| frame_zoom(f).to_bits())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert_eq!(
            forward, backward,
            "frame_zoom must not accumulate across calls"
        );
    }

    /// The zoom must actually zoom in, and reach the documented depth.
    #[test]
    fn the_zoom_narrows_to_the_documented_depth() {
        assert_eq!(
            frame_zoom(0),
            INITIAL_HALF_WIDTH,
            "frame 0 is the starting view"
        );
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
        // The real cliff is where the *per-texel step* — `2 * half_width / TEXTURE_SIZE`, the gap
        // in the complex plane between adjacent texel centres — approaches an f64 ulp at [`CENTER`]:
        // once a step is that small, neighbouring texels round to the same `c` and the image
        // pixelates into blocks. At `CENTER.0 ≈ -0.744`, the ulp is `2^-53 ≈ 1.1e-16` (f64's mantissa
        // is 52 bits, and `0.5 <= |CENTER.0| < 1.0` puts the binade exponent at -1), so the cliff sits
        // at `half_width ≈ ulp * TEXTURE_SIZE / 2 ≈ 2.8e-14` — about four orders of magnitude below
        // this `1e-12` threshold. `1e-12` is deliberately not tightened to sit right at that cliff:
        // it is a coarse guard comfortably above it, tuned to catch a schedule that got tuned into the
        // danger zone without having to track the exact ulp math above every time. At frame 119 the
        // half-width is ~0.04 and the per-texel step is ~1.56e-4 — about 1.4e12 ulps clear of
        // degenerate, nowhere near this floor or the real one.
        let smallest = frame_zoom(crate::FRAME_COUNT - 1);
        assert!(
            smallest > 1e-12,
            "a half-width within a few orders of magnitude of f64's per-texel resolution would \
             pixelate into blocks; got {smallest}"
        );
    }

    /// The solid must actually rotate — frame 0 and a later frame must differ.
    #[test]
    fn the_solid_rotates() {
        assert_ne!(
            frame_mvp(0),
            frame_mvp(30),
            "the orientation must change over time"
        );
    }
}
