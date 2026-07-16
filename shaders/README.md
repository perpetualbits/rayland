# Shaders

`triangle.vert` / `triangle.frag` are the GLSL sources. The committed `.spv` files are
their SPIR-V compilations, embedded into `rayland-server` at build time so the build
needs no shader compiler.

`icosa.vert` / `icosa_flat.frag` / `icosa_textured.frag` are `rayland-icosa-vk`'s shaders,
shared by both icosahedron fixtures. `icosa.vert` and `icosa_flat.frag` are embedded into
`rayland-icosa-vk` itself (the vertex shader by the library, the flat fragment shader only
by the library's own test — see that shader's header for why it is permanent, not
scaffolding). `icosa_textured.frag` is compiled and committed here for the same
no-shader-compiler-required reason, but is embedded by the CPU fixture (a later task), not
by `rayland-icosa-vk`.

Regenerate after editing the GLSL with:

    glslangValidator -V shaders/triangle.vert -o shaders/triangle.vert.spv
    glslangValidator -V shaders/triangle.frag -o shaders/triangle.frag.spv
    glslangValidator -V shaders/icosa.vert -o shaders/icosa.vert.spv
    glslangValidator -V shaders/icosa_flat.frag -o shaders/icosa_flat.frag.spv
    glslangValidator -V shaders/icosa_textured.frag -o shaders/icosa_textured.frag.spv
