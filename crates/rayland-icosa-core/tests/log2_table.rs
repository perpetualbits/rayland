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
/// The inputs are chosen to cover the *shape* of the function, not to be pretty. Recall how `log2`
/// works internally: it splits `x` into a mantissa `m` in `[1, 2)` and an integer exponent `e`, then
/// approximates only `log2(m)` via `t = (m - 1) / (m + 1)`, which maps `m`'s range onto `t` in
/// `[0, 1/3)` before running the truncated polynomial. So the property that actually needs covering
/// is where each case lands on that `t` axis, not how the inputs look in decimal:
///
/// - `1.0`, `2.0`, `4.0`, `0.5` are exact powers of two: `m` is exactly `1.0`, so `t` is exactly `0`
///   and the polynomial contributes nothing — only the exponent path is exercised.
/// - `1.5` and `3.0` share the identical mantissa (`1.5`, so `t = 0.2` for both) but different
///   exponents (`0` and `1`). Pinning the same polynomial output against two different exponents is
///   what isolates the `exponent +` term from the series term — a real bug in the exponent bias or
///   the mantissa bit-mask would show up here even if the polynomial itself were flawless.
/// - `2.0001` is just above a power of two: `m ≈ 1.00005`, so `t ≈ 2.5e-5`, pinning the `t → 0` edge
///   where the exponent decomposition switches over and where the polynomial is evaluated nearest
///   its most common real input. It also doubles as a realistic value of `|z|` just past the
///   fractal's escape radius of 2, which is what this function is actually fed in production.
/// - `1.999` is just below a power of two: `m ≈ 1.999`, so `t ≈ 0.333`, pinning the `t → 1/3` edge —
///   the far end of the polynomial's domain, where the series' truncation error is largest (~1e-9,
///   versus ~1.5e-13 at `t = 0.2`) and its shape is most distinctive.
/// - `10.0` (`t ≈ 0.111`) and `1e300` (`t ≈ 0.198`) round out the middle of the domain and exercise
///   large positive exponents.
const CASES: &[(f64, u64)] = &[
    (1.0, 0x0000000000000000),
    (2.0, 0x3ff0000000000000),
    (4.0, 0x4000000000000000),
    (0.5, 0xbff0000000000000),
    // Generated once from this implementation via a temporary #[test] (see Task 1 Step 4) and
    // sanity-checked against known approximations before being committed:
    // log2(3) ≈ 1.585, log2(1.5) ≈ 0.585, log2(10) ≈ 3.322, log2(1e300) ≈ 996.578.
    (3.0, 0x3ff95c01a39fb95a),
    (1.5, 0x3fe2b803473f72b3),
    (10.0, 0x400a934f0979a371),
    (1e300, 0x408f24a09f1a8b87),
    // Added in a review follow-up to Task 1: the original eight inputs pinned the polynomial at
    // only three distinct t values (0, 0.2, 0.111/0.198), all in the lower 60% of its domain,
    // leaving the t -> 0 and t -> 1/3 edges — and the doc comment's claims about covering them —
    // untested. Generated the same way as the block above and sanity-checked against
    // log2(1.999) ≈ 0.99928 and log2(2.0001) ≈ 1.0000721 before being committed.
    (1.999, 0x3feffa16d7dfcfd3),
    (2.0001, 0x3ff0004ba30a7e18),
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
