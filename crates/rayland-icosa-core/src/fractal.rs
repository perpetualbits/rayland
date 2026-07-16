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
//! # Only the visible third (plus a filtering margin) is iterated
//! Every face samples the same equilateral triangle centred in this square texture with a margin
//! ([`crate::geometry::FACE_UVS`]), and that triangle covers 32.5% of it. The expensive part of this
//! module — the Mandelbrot iteration, up to [`crate::MAX_ITER`] steps per texel — therefore runs
//! only near that triangle; the rest is written black without being iterated.
//!
//! That restriction is not a mere optimisation, and it must not be removed as "simplification". The
//! GPU fixture evaluates this same fractal per fragment, so its rasteriser confines the work to the
//! visible region for free. If this module iterated the whole square, the CPU fixture would perform
//! roughly three times the fractal arithmetic of its counterpart — for a reason having nothing to do
//! with where the fractal is computed, which is the one property the two fixtures exist to compare.
//! The resulting measurement would be wrong by that factor and would look perfectly reasonable.
//!
//! The padding is still written every frame, though. The byte traffic through mapped memory is the
//! thing the CPU fixture is built to create; only the expensive arithmetic is skipped.
//!
//! # Why "near that triangle" and not "inside that triangle"
//! [`crate::geometry::uv_is_inside_face`]'s doc comment carries a pitfall note aimed squarely at this
//! module: the fractal texture is sampled with **linear** (bilinear) filtering — a later task's
//! sampler configuration mandates it (see `docs/design/2026-07-16-icosa-fixtures.md` §7.2) — and a
//! bilinear fetch at a UV *inside* the triangle can read a 2×2 texel
//! neighbourhood reaching up to *one texel outside* it. If this module iterated exactly the bare
//! triangle and painted everything else black, those just-outside texels would be black, and the
//! linear filter would blend that black into every sample near a face's edge — a dark fringe, and
//! **only in the CPU fixture**, because its sibling evaluates the fractal per fragment and has no
//! texture, and therefore no filter, at all. That would be a visible divergence between the two
//! fixtures in exactly the place they must be identical, and it would look like a data-transport bug
//! rather than what it actually is: a filtering artefact from too small a sampled region.
//!
//! The fix belongs here, not in [`crate::geometry::uv_is_inside_face`] — that predicate must keep
//! meaning exactly "inside the triangle", because other callers (or a future one) may need that
//! exact meaning without a filtering opinion baked in. So `render_into_at` below iterates a region
//! dilated 2 texels outward from the triangle's edges, via `uv_is_inside_dilated_face`. Two texels,
//! not one: one texel is the strict bilinear footprint the filter can reach, and the second is free
//! margin against ordinary UV-to-texel rounding at the boundary — the fetch position a face's vertex
//! shader interpolates to is not obliged to land exactly where this module's own `(x + 0.5) / size`
//! texel-centre sampling would put it, and a single texel of slack absorbs that without needing to
//! reason precisely about where the two could disagree.
//!
//! [`crate::geometry::FACE_UVS`]'s own doc comment records that its ~0.067 gap from the texture's
//! edges (about 34 texels at [`crate::TEXTURE_SIZE`] = 512) exists to leave room for exactly this
//! dilation. Checked directly: offsetting each of the triangle's three edges outward by 2 texels
//! (2/512 ≈ 0.0039 in UV units) and intersecting the offset lines moves the triangle's three corners
//! outward by at most ~4 texels, landing the dilated triangle's bounding box at roughly
//! `x ∈ [0.060, 0.940]`, `y ∈ [0.117, 0.879]` — comfortably inside the `[0, 1]` texture on every
//! side, nowhere near clipping against the square's edges.

use crate::exact_math::log2;
use crate::geometry::FACE_UVS;

/// The escape radius, squared.
///
/// Escape is tested against `|z|² > 4` rather than `|z| > 2` so that the loop needs no square root.
/// The smooth-iteration formula is derived assuming a generous escape radius, and 2 is the standard
/// choice; a larger radius smooths marginally better at the cost of more iterations.
const ESCAPE_RADIUS_SQUARED: f64 = 4.0;

/// How many texels beyond [`FACE_UVS`]'s triangle the fractal is iterated, in the direction
/// perpendicular to each edge.
///
/// See this module's doc comment ("Why 'near that triangle' and not 'inside that triangle'") for the
/// full reasoning: 1 texel is the bilinear filter's own footprint, and the second texel is slack
/// against UV-to-texel rounding at the boundary. Expressed in texels (rather than baked directly
/// into a UV constant) so its relationship to the texture's resolution is explicit at the call site.
const DILATION_TEXELS: f32 = 2.0;

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
    // in the first few dozen iterations at frame 0, so even dividing by 64 already produces a narrow
    // band, not a wide sweep (measured: with this divisor, hue's 10th/50th/90th percentiles across
    // the triangle are 0.043/0.068/0.215 of a cycle — 90% of the palette packed into a fifth of the
    // hue circle). Dividing by the full 512-iteration budget instead would make that band 8x narrower
    // still, and the image would be nearly monochrome. 64 is a compromise across the whole run rather
    // than a fit to either end: at frame 0 it keeps the band narrow but legible instead of crushing it
    // further, and by frame 119 — deep enough into the zoom that escape has slowed markedly — the same
    // divisor produces a full multi-cycle sweep (median hue 0.375, with 13.5% of the triangle past one
    // cycle). Not derived from a formula; chosen by rendering both ends of the zoom and checking that
    // 64 keeps the palette usable at both, not just one.
    let hue = smooth_iteration / 64.0;
    hsv_to_rgb(hue, 0.85, 1.0)
}

/// Whether `uv` lies inside [`FACE_UVS`]'s triangle, or within [`DILATION_TEXELS`] texels of it.
///
/// # Inputs and outputs
/// `uv` in texture space, `0.0..=1.0` on each axis. Returns true inside the triangle, on its edge, or
/// within the dilation margin outside it.
///
/// # How it works
/// The same edge-sign test [`crate::geometry::uv_is_inside_face`] uses, generalised: instead of
/// asking whether `uv` is on the same side of every edge as the interior (distance ≥ 0), this asks
/// whether it is within `margin` of that — distance ≥ `-margin` — for whichever winding this
/// triangle turns out to have. The raw edge-sign value from that predicate is twice the signed area
/// of the triangle `(edge, uv)`, not a distance; dividing by the edge's own length converts it into
/// an actual perpendicular distance in UV units, which is what a texel-count margin can be compared
/// against. At `margin = 0.0` this reduces to exactly [`crate::geometry::uv_is_inside_face`]'s
/// condition (dividing by a positive length never changes a comparison's sign against zero).
///
/// Offsetting each of a convex polygon's edges outward by a fixed distance and re-intersecting them
/// (which is what three independent per-edge comparisons effectively do) produces the correctly
/// dilated convex polygon — a slightly larger triangle, not a rounded blob — which is exactly the
/// region a caller doing per-edge distance checks wants.
///
/// # Why this lives here and not next to [`crate::geometry::uv_is_inside_face`]
/// That predicate's own doc comment is explicit that its fix belongs to "whoever writes the
/// texture" — this module — and that the predicate itself must keep meaning exactly "inside the
/// triangle". Giving it a margin parameter would blur that meaning for every other caller.
///
/// # Failure modes
/// None for [`FACE_UVS`], the only triangle this is ever called with: a genuine, non-degenerate
/// equilateral triangle, so no edge has zero length and the division below never divides by zero.
fn uv_is_inside_dilated_face(uv: [f32; 2], margin_texels: f32) -> bool {
    // The margin, converted from texels into the same UV units the edge distances below come out
    // in, so the two are directly comparable.
    let margin = margin_texels / crate::TEXTURE_SIZE as f32;

    // The perpendicular signed distance from `uv` to the infinite line through `a -> b`: the
    // edge-sign cross product (twice the signed triangle area) divided by the edge's own length.
    // Positive or negative according to which side of the edge `uv` is on; which sign means
    // "interior" depends on FACE_UVS's winding and is not assumed here, mirroring
    // `uv_is_inside_face`'s own even-handedness about winding direction.
    let signed_distance = |a: [f32; 2], b: [f32; 2]| -> f32 {
        let edge_x = b[0] - a[0];
        let edge_y = b[1] - a[1];
        let cross = edge_x * (uv[1] - a[1]) - edge_y * (uv[0] - a[0]);
        let edge_len = (edge_x * edge_x + edge_y * edge_y).sqrt();
        cross / edge_len
    };
    let d0 = signed_distance(FACE_UVS[0], FACE_UVS[1]);
    let d1 = signed_distance(FACE_UVS[1], FACE_UVS[2]);
    let d2 = signed_distance(FACE_UVS[2], FACE_UVS[0]);
    // "Within margin of all three edges' interior side", for either possible winding — the same
    // either/or structure as `uv_is_inside_face`, but with the threshold moved from 0 out to
    // `margin` on whichever side is interior.
    let all_non_negative = d0 >= -margin && d1 >= -margin && d2 >= -margin;
    let all_non_positive = d0 <= margin && d1 <= margin && d2 <= margin;
    all_non_negative || all_non_positive
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

            // The texture coordinate this texel sits at, which is what decides whether it is ever
            // reachable by a bilinear fetch anywhere a face samples.
            let uv = [
                (x as f32 + 0.5) / size as f32,
                (y as f32 + 0.5) / size as f32,
            ];

            // Only the triangle plus a 2-texel filtering margin is iterated; see this module's doc
            // comment ("Why 'near that triangle' and not 'inside that triangle'") for why the bare
            // triangle alone is not enough once linear filtering is in the picture, and why 2 texels
            // and not the whole square.
            //
            // The padding is still *written*, every frame, like every other texel: the byte traffic
            // through mapped memory is what this program exists to create, and shrinking it to the
            // triangle would quietly cut the very number the workload is built around. It is only
            // the expensive part — up to MAX_ITER iterations — that is skipped.
            if !uv_is_inside_dilated_face(uv, DILATION_TEXELS) {
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
            // whole image would be biased half a level dark. `as u8` on a float has been a
            // *saturating* cast since Rust 1.45 (`1.2f64 as u8 == 255`, `(-0.3f64) as u8 == 0`,
            // `f64::NAN as u8 == 0`), so an out-of-range value can never wrap into a bogus byte — that
            // failure mode does not exist in this language. The `clamp` is a defensive statement of
            // the intended `0..=1` range rather than a guard against a real hazard: `hsv_to_rgb`'s
            // output is provably within `[0.15, 1.0]` for the fixed `saturation = 0.85`, `value = 1.0`
            // this module always calls it with (its result is `1 + 0.85·(ramp − 1)` with
            // `ramp ∈ [0, 1]`), so the clamp is currently unreachable. It stays so that a future
            // palette change — a different saturation, or a value channel that varies — cannot
            // silently produce an out-of-range byte instead of an explicit clamp to a known range.
            for channel in 0..3 {
                pixels[offset + channel] = (rgb[channel].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
            // Opaque: the texture is a surface colour, and any transparency here would blend the
            // solid's faces into the background.
            pixels[offset + 3] = 255;
        }
    }
}

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
        assert_eq!(
            first, second,
            "the fractal must be a pure function of its half-width"
        );
    }

    /// Zooming must change the picture — otherwise the animation is a still image.
    #[test]
    fn zooming_changes_the_picture() {
        let mut wide = vec![0u8; texture_bytes()];
        let mut narrow = vec![0u8; texture_bytes()];
        render_into(&mut wide, 1.5);
        render_into(&mut narrow, 0.05);
        assert_ne!(
            wide, narrow,
            "a different half-width must produce a different image"
        );
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

    /// The texture's corners lie outside the sampled triangle, well beyond the dilation margin, and
    /// must be black padding.
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

    /// Restricting the iteration to the (dilated) triangle must not change what the triangle shows.
    ///
    /// The point of the restriction is to skip *invisible* work. If a texel deep inside the triangle
    /// came out differently because of it, the restriction would be changing the picture rather than
    /// just its cost — a much worse bug than the waste it set out to fix. The centroid used here is
    /// far enough from every edge that the 2-texel dilation cannot be a factor either way; this test
    /// is about the restriction's correctness, not the dilation's — that is
    /// `margin_texels_are_colored_but_texels_far_outside_stay_black`, below.
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

    /// Texels just beyond the bare triangle, but within the 2-texel dilation margin, must be
    /// coloured — not black — and texels well beyond that margin must still be black padding.
    ///
    /// This is the test for the one deliberate deviation from a plain "iterate the triangle"
    /// restriction: `render_into_at` iterates a region dilated 2 texels beyond `FACE_UVS`'s
    /// triangle, so that the bilinear sampler the fixture's sibling task mandates never reads a
    /// black padding texel when fetching a UV just inside the triangle's edge (see this module's
    /// doc comment for the full reasoning). If the dilation were missing, the "near" point checked
    /// here — 1 texel outside the bare triangle — would wrongly come out black. If the dilation were
    /// unbounded (i.e. the restriction was removed rather than widened), the "far" point — 6 texels
    /// out — would wrongly come out coloured.
    ///
    /// The two probe points are placed along the true outward normal of `FACE_UVS`'s first edge,
    /// computed independently here rather than reusing `uv_is_inside_dilated_face`, so this does not
    /// just check the implementation against itself. Both points are confirmed (`assert!`, below) to
    /// be outside the *bare* triangle before the real assertions run, so a future change to
    /// `FACE_UVS` that moved the edge would fail loudly here rather than silently passing a test that
    /// no longer probes what it claims to.
    #[test]
    fn margin_texels_are_colored_but_texels_far_outside_stay_black() {
        let mut pixels = vec![0u8; texture_bytes()];
        // Every point escapes almost immediately in this view, so any texel that got iterated at all
        // comes out visibly non-black; only texels the loop skipped entirely stay at their
        // initialised black.
        render_into_at(&mut pixels, (4.0, 4.0), 0.1);
        let size = crate::TEXTURE_SIZE as usize;

        let a = crate::geometry::FACE_UVS[0];
        let b = crate::geometry::FACE_UVS[1];
        let edge = [(b[0] - a[0]) as f64, (b[1] - a[1]) as f64];
        let edge_len = (edge[0] * edge[0] + edge[1] * edge[1]).sqrt();
        // The edge vector rotated -90 degrees and normalised. Which rotation direction is "outward"
        // depends on FACE_UVS's winding; the sanity assertions below (both probe points must be
        // outside the bare triangle) catch it if this guess is backwards.
        let normal = [-edge[1] / edge_len, edge[0] / edge_len];
        let midpoint = [((a[0] + b[0]) as f64) / 2.0, ((a[1] + b[1]) as f64) / 2.0];
        let texel = 1.0 / size as f64;

        let point_at = |texels_out: f64| -> [f32; 2] {
            [
                (midpoint[0] + normal[0] * texel * texels_out) as f32,
                (midpoint[1] + normal[1] * texel * texels_out) as f32,
            ]
        };
        // 1 texel out: outside the bare triangle, comfortably inside the 2-texel margin.
        let near = point_at(1.0);
        // 6 texels out: well beyond the 2-texel margin.
        let far = point_at(6.0);
        assert!(
            !crate::geometry::uv_is_inside_face(near),
            "test setup: the near probe must be outside the bare triangle"
        );
        assert!(
            !crate::geometry::uv_is_inside_face(far),
            "test setup: the far probe must be outside the bare triangle"
        );

        // Matches the pixel-index convention `the_restriction_does_not_alter_the_visible_region`
        // uses: floor(uv * size), not the texel-centre-sampling `render_into_at` itself does — close
        // enough here because both probe points sit well clear of the 2-texel boundary they are
        // meant to test, so a sub-texel rounding difference cannot flip the result.
        let pixel_offset_of = |uv: [f32; 2]| {
            let x = (uv[0] * size as f32) as usize;
            let y = (uv[1] * size as f32) as usize;
            (y * size + x) * 4
        };

        let near_offset = pixel_offset_of(near);
        let far_offset = pixel_offset_of(far);
        assert_ne!(
            &pixels[near_offset..near_offset + 3],
            &[0, 0, 0],
            "a texel within the dilation margin must be coloured, not left as black padding"
        );
        assert_eq!(
            &pixels[far_offset..far_offset + 4],
            &[0, 0, 0, 255],
            "a texel beyond the dilation margin must still be black padding"
        );
    }
}
