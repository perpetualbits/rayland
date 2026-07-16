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
///
/// Set to `1e-10`. The kernels' true worst-case error, scanned over the full reduced range
/// `[-π/4, π/4]` in exact arithmetic, is 6.93e-12 (sine kernel, at `|r| = π/4`; the cosine kernel's
/// is smaller, 3.9e-13) — see `exact_math.rs`'s `sin_cos` doc comment. `1e-10` leaves roughly 14x
/// slack over that true error: enough that the check is not brittle against, say, a future term
/// count change, but tight enough to still be *binding*. A materially looser value is not merely
/// generous — dropping the cosine kernel's `+1/12!` term entirely still passes both this check and
/// the Pythagorean-identity check below at the old `1e-9` tolerance; only the frozen table below
/// catches that regression. The oracle itself stays sound at this tightness: libm's own error is
/// ~1e-16, far below `1e-10`.
const ACCURACY_TOLERANCE: f64 = 1e-10;

/// Inputs spanning every quadrant, plus the reduction's tie points and edges.
///
/// Chosen for the shape of the algorithm, not prettiness. Recall how `sin_cos` works: it computes
/// `k = round(x * 2/π)` (`round` is ties-away-from-zero), reduces to `r = x - k*π/2` which always
/// lands in `[-π/4, π/4]`, and picks sine/cosine identities by `k mod 4` (the quadrant). So the
/// properties that need covering are the quadrant reached, how large `|r|` gets, and — separately —
/// whether `k`'s rounding at an exact tie is handled correctly, since only the tie rule decides the
/// quadrant there. Working through every input below against that model:
///
/// - `0.0`: `k = 0`, quadrant 0, `r = 0` — the kernels' leading terms are all that survive.
/// - `0.5`: `k = 0`, quadrant 0, `r = 0.5`.
/// - `1.0`: `k = 1`, quadrant 1, `r ≈ -0.571`.
/// - `1.5707963267948966` (π/2): `k = 1`, quadrant 1, `r ≈ 0` — an ordinary boundary, not a tie
///   (`x * 2/π` rounds to exactly `1.0`, not to a `…5`), included as a sanity anchor since
///   `sin(π/2)` must come out ≈ 1.0.
/// - `2.0`: `k = 1`, quadrant 1, `r ≈ 0.429`.
/// - `3.0`: `k = 2`, quadrant 2, `r ≈ -0.142`.
/// - `3.141592653589793` (π): `k = 2`, quadrant 2, `r ≈ 0`.
/// - `4.0`: `k = 3`, quadrant 3, `r ≈ -0.712` — the closest the original list came to the reduced
///   range's maximum, and still short of it.
/// - `5.0`: `k = 3`, quadrant 3, `r ≈ 0.288`.
/// - `6.283185307179586` (2π): `k = 4`, quadrant 0, `r ≈ 0`.
/// - `-0.5`: `k = 0`, quadrant 0, `r = -0.5`.
/// - `-2.0`: `k = -1`, quadrant 3 (`rem_euclid(-1, 4) = 3`), `r ≈ -0.429`.
/// - `-7.0`: `k = -4`, quadrant 0, `r ≈ -0.717`.
/// - `50.0`: `k = 32`, quadrant 0, `r ≈ -0.265`.
/// - `119.0`: `k = 76`, quadrant 0, `r ≈ -0.381` — a large multiple of π/2 where the reduction has
///   to do real work and cancellation is worst.
///
/// That covers all four quadrants, but every one of the above has `|r|` well short of the reduced
/// range's true maximum of π/4 ≈ 0.785, and — more importantly — none of them is a *tie*: a point
/// where `x * 2/π` lands exactly on `n + 0.5`, so the ties-away rounding rule alone decides which
/// way `k` (and therefore the quadrant) goes. A single off-by-one in that tie-break silently swaps
/// sine and cosine, and nothing above would catch it. The three points below close both gaps at
/// once, because the tie points *are* where `|r|` is largest:
///
/// - `0.7853981633974483` (π/4): `x * 2/π = 0.5` exactly, a tie; ties-away-from-zero rounds it to
///   `k = 1`, quadrant 1, `r = π/4 - π/2 = -π/4` — the reduced range's most negative extreme.
/// - `-0.7853981633974483` (−π/4): `x * 2/π = -0.5` exactly, a tie; ties-away rounds it to `k = -1`,
///   quadrant 3, `r = -π/4 + π/2 = π/4` — the reduced range's most positive extreme.
/// - `2.356194490192345` (3π/4): `x * 2/π = 1.5` exactly, a tie; ties-away rounds it to `k = 2`,
///   quadrant 2, `r = 3π/4 - π = -π/4`.
///
/// So the list below genuinely does what its predecessor's comment only claimed: it reaches every
/// quadrant, it pins both the zero case and the reduced range's true worst-case `|r|` (≈ π/4, the
/// maximum error case for both truncated Taylor kernels), it exercises the tie rule that the
/// ordinary boundary near π/2 does not, and it still keeps the large-argument, negative and
/// cancellation-heavy cases from the original list.
///
/// - `-0.0`: `k = -0.0`, quadrant 0, `r = 0.0`. Included not for the quadrant model above (it is
///   arithmetically identical to `0.0`) but because `sin_cos` has a documented, deliberate
///   divergence from libm at exactly this input — `sin_cos(-0.0)` returns `+0.0` for the sine
///   component, where libm returns `-0.0` (see `exact_math.rs`'s `sin_cos` doc comment for why).
///   Adding it here pins that divergence in the frozen table below, rather than leaving it as an
///   assertion in a comment that nothing actually checks.
///
/// The four values that coincide with a named `std::f64::consts` constant (π/2, π, 2π, π/4) are
/// written using that constant rather than as hand-typed decimals — clippy's `approx_constant`
/// lint flags a literal that merely approximates a well-known constant, since a transcription slip
/// there would be easy to miss. Each was checked bit-identical to the decimal it replaces before
/// the substitution (`to_bits()` matches in every case); see the implementation's commit for the
/// values. `3π/4` has no corresponding named constant, so it stays a literal.
const INPUTS: &[f64] = &[
    0.0,
    0.5,
    1.0,
    std::f64::consts::FRAC_PI_2,
    2.0,
    3.0,
    std::f64::consts::PI,
    4.0,
    5.0,
    std::f64::consts::TAU,
    -0.5,
    -2.0,
    -7.0,
    50.0,
    119.0,
    std::f64::consts::FRAC_PI_4,
    -std::f64::consts::FRAC_PI_4,
    2.356194490192345,
    -0.0,
];

/// Every input must reproduce its committed bit patterns exactly.
///
/// The table is filled in at implementation time (see the plan's Step 4) and is the contract from
/// then on.
#[test]
fn sin_cos_matches_the_committed_bit_patterns() {
    // (input, sin bits, cos bits) — printed by a temporary #[test] in exact_math.rs against this
    // exact implementation, sanity-checked (sin(0) = 0.0 exactly, cos(0) = 1.0 exactly, sin(π/2) ≈
    // 1.0, and the π/4 / 3π/4 pair cross-checked against the identities sin(3π/4) = sin(π/4) and
    // cos(3π/4) = -cos(π/4) — both hold bit-for-bit, the sign flip on cos being the only
    // difference), then frozen here. The temporary test has been deleted; see this file's own
    // "# Regenerating the table" section (top of file) for what regenerating this table would
    // require.
    const CASES: &[(f64, u64, u64)] = &[
        (0.0, 0x0000000000000000, 0x3ff0000000000000),
        (0.5, 0x3fdeaee8744b048f, 0x3fec1528065b7d56),
        (1.0, 0x3feaed548f090d16, 0x3fe14a280fb502b2),
        (
            std::f64::consts::FRAC_PI_2,
            0x3ff0000000000000,
            0x3c91a62633100000,
        ),
        (2.0, 0x3fed18f6ead1b447, 0xbfdaa226575371d4),
        (3.0, 0x3fc210386db6d55b, 0xbfefae04be85e5d2),
        (std::f64::consts::PI, 0x3ca1a62633100000, 0xbff0000000000000),
        (4.0, 0xbfe837b9dddc222c, 0xbfe4eaa606dae026),
        (5.0, 0xbfeeaf81f5e09933, 0x3fd22785706b4ad9),
        (
            std::f64::consts::TAU,
            0xbcb1a62633100000,
            0x3ff0000000000000,
        ),
        (-0.5, 0xbfdeaee8744b048f, 0x3fec1528065b7d56),
        (-2.0, 0xbfed18f6ead1b447, 0xbfdaa226575371d4),
        (-7.0, 0xbfe50608c26cbfae, 0x3fe81ff79ed923e6),
        (50.0, 0xbfd0cabfe5fcdfc8, 0x3feee1006fc3fcfa),
        (119.0, 0xbfd7c515b551b81a, 0x3fedb6097cbb24f1),
        (
            std::f64::consts::FRAC_PI_4,
            0x3fe6a09e667f497a,
            0x3fe6a09e667e480b,
        ),
        (
            -std::f64::consts::FRAC_PI_4,
            0xbfe6a09e667f497a,
            0x3fe6a09e667e480b,
        ),
        (2.356194490192345, 0x3fe6a09e667e480b, 0xbfe6a09e667f497a),
        // Pins the documented libm divergence at -0.0: sin comes out +0.0 (bits identical to
        // sin_cos(0.0)'s), not the -0.0 libm's sin(-0.0) would give. See exact_math.rs's sin_cos
        // doc comment.
        (-0.0, 0x0000000000000000, 0x3ff0000000000000),
    ];
    // If the table is ever emptied (e.g. by a bad merge or an over-eager refactor), the loop below
    // would iterate zero times and this test would report success without checking anything — the
    // exact silent-pass failure mode this file exists to prevent. Guard against that explicitly.
    assert!(!CASES.is_empty(), "the bit-pattern table must not be empty");
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

/// The Pythagorean identity must hold, which catches a wrong-magnitude kernel term that the
/// accuracy check alone could miss.
///
/// This is **not** a quadrant-selection check, and must not be described as one: every quadrant arm
/// in `sin_cos` is a signed permutation of `(sin_r, cos_r)` — a swap, a sign flip, or both — and
/// `s² + c²` is invariant under all four of them (squaring erases both the swap and every sign).
/// So no quadrant bug, however wrong, can move this sum away from 1. Verified directly: mutating the
/// quadrant `match` (swapping arms 1 and 3, and separately corrupting arm 2) is caught by both the
/// bit-pattern table above and `sin_cos_is_accurate_enough`, but this test passes unchanged against
/// both mutations. `tests/sin_cos_table.rs`'s frozen table is what actually covers the quadrants;
/// see `exact_math.rs`'s `sin_cos` doc comment for why.
///
/// What this test *does* catch, and was verified to catch: a term dropped from a Taylor kernel —
/// specifically, the review confirmed it catches a dropped sine-kernel term. Because the two
/// kernels are evaluated independently, a magnitude error in one is not cancelled by the other the
/// way a quadrant permutation would be, so `s² + c²` moves measurably away from 1. That makes this
/// a kernel-magnitude check, cheap to run and complementary to the per-value accuracy check above.
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
