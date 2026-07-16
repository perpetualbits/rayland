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
//! Each is a truncated series, against a standard library that is correctly-rounded to the last
//! bit: `log2` to roughly 1e-9 and `sin_cos` to roughly 1e-11 (see each function's own doc comment
//! for the measured worst case). That trade is correct here: the results drive an 8-bit colour
//! channel and a rotation matrix, where errors of this size are invisible, while reproducibility is
//! load-bearing. **A visually fine approximation that is bit-exact everywhere is strictly better
//! here than a perfect one that is not.**
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
const TWO_OVER_LN2: f64 = 2.885_390_081_777_926_8;

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
    // The domain guard. Zero, negatives and NaN have no real logarithm, and a silent NaN produced
    // deep inside a pixel loop is far harder to trace than one produced here at the boundary.
    // Written as two ordinary comparisons (rather than `!(x > 0.0)`) because `f64` is only
    // partially ordered: NaN compares false to everything, so a negated `>` reads as "not
    // greater", which is easy to misjudge at a glance, versus this, which states the excluded
    // NaN case explicitly.
    if x.is_nan() || x <= 0.0 {
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

/// `2 / π`, used to find how many quarter-turns to subtract during range reduction.
///
/// Taken from `std::f64::consts` rather than written as a literal, because clippy's
/// `approx_constant` lint (correctly) flags a hand-typed decimal that happens to approximate a
/// named constant — the risk is a transcription slip nobody would notice. Verified bit-identical
/// to the literal `0.6366197723675814` this was originally written as (`to_bits()` on both sides
/// gives `0x3fe45f306dc9c883`), so using the named constant changes nothing about the arithmetic
/// below, only where the bit pattern comes from.
const TWO_OVER_PI: f64 = std::f64::consts::FRAC_2_PI;

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
/// truncating leaves a maximum error — scanned over the full reduced range in exact arithmetic —
/// of **6.93e-12** for the sine kernel (attained at the domain's edge, `|r| = π/4`) and a smaller
/// **3.9e-13** for the cosine kernel. Both are many orders below the resolution of anything
/// downstream (an 8-bit colour channel, a rotation matrix), which is the basis for calling the
/// term counts above adequate.
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
///
/// `sin_cos(-0.0)` returns `(+0.0, 1.0)` for the sine component, where libm returns `-0.0` —
/// libm preserves the sign of zero through sine (an odd function) as an IEEE-754 convention, but
/// the reduction here does not: `k` rounds to `-0.0`, and the two Cody-Waite subtractions that
/// follow (`-0.0 - (-0.0)`) land on positive zero before the kernel ever runs, so `sin_r` comes out
/// `0.0 * positive = +0.0`. This is a **deliberate non-issue, not a bug**: the divergence is itself
/// bit-exact and deterministic across every host, so it cannot reintroduce the cross-machine drift
/// this module exists to prevent, and `|(+0.0) - (-0.0)| = 0.0` keeps it well inside any accuracy
/// tolerance the tests enforce. It is documented here, and pinned in `tests/sin_cos_table.rs`'s
/// frozen table, purely so a future reader comparing this function's output to libm's does not
/// mistake the sign-bit difference at this one input for a real defect.
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
