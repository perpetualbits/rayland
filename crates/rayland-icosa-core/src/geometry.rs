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
//! icosahedron. At 60 vertices (1,920 bytes) versus 12 shared vertices plus a 60-entry `u16` index
//! buffer (504 bytes), an index buffer would save about 1.4 KB — trivial next to the 1 MiB fractal
//! texture this solid is wrapped in — while adding Vulkan surface (an index buffer, its binding,
//! and the smooth-normal-vs-flat-normal tradeoff above) for a saving nobody would notice.
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
/// None in this crate: [`FACE_UVS`] is a compile-time constant forming a genuine, non-degenerate
/// triangle, so the degenerate case below never actually arises here. For a caller who passed a
/// different, degenerate triangle: if all three corners coincide, every point reports as inside
/// (every edge sign is exactly zero, and zero satisfies both the non-negative and non-positive
/// branches); if the corners are merely collinear without coinciding, the three edge signs are not
/// guaranteed to agree, so the result is not "everything inside" in general — it is unspecified
/// behaviour this function was never designed to promise anything about.
pub fn uv_is_inside_face(uv: [f32; 2]) -> bool {
    // The signed area of the triangle formed by edge (a → b) and the point. Its sign says which
    // side of that edge the point is on.
    let edge_sign =
        |a: [f32; 2], b: [f32; 2]| (b[0] - a[0]) * (uv[1] - a[1]) - (b[1] - a[1]) * (uv[0] - a[0]);
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
            .map(|v| {
                [
                    v.position[0].to_bits(),
                    v.position[1].to_bits(),
                    v.position[2].to_bits(),
                ]
            })
            .collect();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 12, "an icosahedron has 12 corners");
    }

    /// Every edge must have the same length — the definition of *regular*.
    ///
    /// A tolerance is used here, unlike elsewhere in this crate, because this test asserts a
    /// *geometric* property (that the solid is regular) rather than the bit-for-bit
    /// *reproducibility* that `exact_math`'s frozen tables pin. Those are different kinds of claim:
    /// a geometric property should still hold, and this test should still express "a regular
    /// icosahedron", even if the construction below changed to one that reached equal edge lengths
    /// by a different, less symmetric route. Measured against this exact construction the deviation
    /// happens to be exactly zero — every edge here is computed from the same pair of magnitudes by
    /// the solid's symmetry, and `sqrt` is one of the few operations IEEE-754 specifies exactly — but
    /// the test does not rely on that; 1e-6 is loose enough to survive a construction that does not
    /// share this coincidence while still being far tighter than any real regularity defect.
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
            assert!(
                dot > 0.0,
                "face {index}'s normal points inward (dot = {dot})"
            );
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
            assert!(
                uv_is_inside_face(corner),
                "{corner:?} is a corner of the triangle"
            );
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
