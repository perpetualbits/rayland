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
