#version 450
// One vertex's inputs, matching the Vertex layout uploaded over the wire.
layout(location = 0) in vec2 inPosition;   // normalised-device-coordinate position
layout(location = 1) in vec3 inColor;      // linear RGB colour
// Colour passed through to the fragment shader, interpolated across the triangle.
layout(location = 0) out vec3 fragColor;
void main() {
    // Place the vertex; z = 0, w = 1 for a simple 2-D triangle.
    gl_Position = vec4(inPosition, 0.0, 1.0);
    // Forward the colour unchanged.
    fragColor = inColor;
}
