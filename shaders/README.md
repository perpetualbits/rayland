# Shaders

`triangle.vert` / `triangle.frag` are the GLSL sources. The committed `.spv` files are
their SPIR-V compilations, embedded into `rayland-server` at build time so the build
needs no shader compiler.

Regenerate after editing the GLSL with:

    glslangValidator -V shaders/triangle.vert -o shaders/triangle.vert.spv
    glslangValidator -V shaders/triangle.frag -o shaders/triangle.frag.spv
