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
    // Generated once from this implementation via a temporary #[test] (see Task 1 Step 4) and
    // sanity-checked against known approximations before being committed:
    // log2(3) ≈ 1.585, log2(1.5) ≈ 0.585, log2(10) ≈ 3.322, log2(1e300) ≈ 996.578.
    (3.0, 0x3ff95c01a39fb95a),
    (1.5, 0x3fe2b803473f72b3),
    (10.0, 0x400a934f0979a371),
    (1e300, 0x408f24a09f1a8b87),
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
