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
