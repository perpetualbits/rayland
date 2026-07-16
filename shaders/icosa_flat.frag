#version 450

// PERMANENT, not scaffolding to delete. No binary ever references this shader — only
// `rayland-icosa-vk`'s own `tests/renders_the_solid.rs` does, via the crate's compiled-in
// `icosa_flat.frag.spv`. Its purpose is to let that test prove the shared scaffolding (the
// geometry upload, the depth buffer, and the lighting) draws a correct, depth-tested, shaded solid
// *without* depending on either fixture's fractal — neither the CPU fixture's uploaded texture nor
// the GPU fixture's per-fragment Mandelbrot evaluation exists yet when this shader runs, and this
// shader is what lets the scaffolding be proven before either of them does. Do not delete this file
// as "unused": grep for `icosa_flat.frag.spv` in `rayland-icosa-vk/tests/` before ever removing it.

layout(location = 0) in vec3 frag_normal;
layout(location = 1) in vec2 frag_uv;

layout(location = 0) out vec4 out_color;

// The light's direction, in model space, normalised. Fixed — not animated, not configurable — so
// that exactly one thing in the scene moves. Identical to icosa_textured.frag's, so the two shaders
// light the solid identically and only the surface colour differs.
const vec3 LIGHT_DIRECTION = normalize(vec3(0.4, 0.7, 0.6));

// How much light a face receives when facing fully away. Without this, back-facing-but-visible
// faces go pure black and the silhouette dissolves into the background.
const float AMBIENT = 0.25;

void main() {
    // frag_uv is intentionally unread: this shader has no texture to sample, and declaring the
    // varying anyway keeps this shader's input interface identical to icosa_textured.frag's, which
    // is what makes it a fair stand-in for exercising the shared vertex stage and pipeline layout.
    // Lambert: brightness falls off with the cosine of the angle to the light. `max` clamps faces
    // turned away from the light to zero rather than letting them go negative and wrap.
    float diffuse = max(dot(normalize(frag_normal), LIGHT_DIRECTION), 0.0);
    float light = AMBIENT + (1.0 - AMBIENT) * diffuse;
    // A fixed orange, chosen only for visibility against the black clear colour; it carries no
    // meaning of its own the way the fractal's colours do.
    out_color = vec4(vec3(0.9, 0.5, 0.2) * light, 1.0);
}
