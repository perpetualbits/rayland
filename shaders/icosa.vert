#version 450

// The vertex attributes, matching rayland_icosa_core::geometry::Vertex field for field. The
// locations here and the VkVertexInputAttributeDescription offsets in pipeline.rs are two halves of
// one contract; changing either alone feeds positions into the normal slot and produces a
// plausible-looking but wrong picture.
layout(location = 0) in vec3 in_position;
layout(location = 1) in vec3 in_normal;
layout(location = 2) in vec2 in_uv;

// The model-view-projection matrix, rebuilt on the CPU every frame. This is the *only* thing that
// changes between frames in the GPU fixture, and one of two in the CPU fixture.
layout(binding = 0) uniform Uniforms {
    mat4 mvp;
    // The fractal view's half-width; unused by this shader but present so both fixtures share one
    // uniform block layout. See the fragment shaders.
    float half_width;
    vec2 center;
} u;

layout(location = 0) out vec3 frag_normal;
layout(location = 1) out vec2 frag_uv;

void main() {
    // The normal is passed through in model space, and the light direction below is given in model
    // space too, so the solid's lighting rotates with it — the faces catch the light as they turn,
    // which is what makes the rotation legible at all. Lighting in view space would leave every
    // face's brightness constant and the solid would look like it was not moving.
    frag_normal = in_normal;
    frag_uv = in_uv;
    gl_Position = u.mvp * vec4(in_position, 1.0);
}
