#version 450

layout(location = 0) in vec3 frag_normal;
layout(location = 1) in vec2 frag_uv;

layout(location = 0) out vec4 out_color;

// The fractal, computed on the CPU and uploaded every frame. In the GPU fixture this binding does
// not exist and the fractal is evaluated here instead; that difference is the entire experiment.
layout(binding = 1) uniform sampler2D fractal;

// The light's direction, in model space, normalised. Fixed — not animated, not configurable — so
// that exactly one thing in the scene moves.
const vec3 LIGHT_DIRECTION = normalize(vec3(0.4, 0.7, 0.6));

// How much light a face receives when facing fully away. Without this, back-facing-but-visible
// faces go pure black and the silhouette dissolves into the background.
const float AMBIENT = 0.25;

void main() {
    // Lambert: brightness falls off with the cosine of the angle to the light. `max` clamps faces
    // turned away from the light to zero rather than letting them go negative and wrap.
    float diffuse = max(dot(normalize(frag_normal), LIGHT_DIRECTION), 0.0);
    float light = AMBIENT + (1.0 - AMBIENT) * diffuse;
    out_color = vec4(texture(fractal, frag_uv).rgb * light, 1.0);
}
