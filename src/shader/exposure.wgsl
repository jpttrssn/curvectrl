// SPDX-License-Identifier: MPL-2.0

// Canonical fullscreen triangle: three NDC vertices whose triangle
// clips to the entire viewport rectangle. The (-1,-1) corner is
// shared; the other two overshoot to x=3 or y=3 so the diagonal
// across (-1,3)→(3,-1) covers every pixel when the render-pass
// scissor clips to the [-1,1]² viewport.
fn vertex_position(vertex_index: u32) -> vec2<f32> {
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(-1.0,  3.0),
        vec2<f32>( 3.0, -1.0),
    );
    return positions[vertex_index];
}

struct Uniforms {
    exposure: f32,   // 2^EV gain (linear-light multiplier)
    vp_width: f32,   // viewport width in physical pixels
    vp_height: f32,  // viewport height in physical pixels
    sc_x: f32,       // scissor rect origin x (physical pixels)
    sc_y: f32,       // scissor rect origin y (physical pixels)
    sc_w: f32,       // scissor rect width
    sc_h: f32,       // scissor rect height
    _pad: f32,
};

@group(0) @binding(0) var t_mono: texture_2d<f32>;
@group(0) @binding(1) var s_mono: sampler;
@group(0) @binding(2) var<uniform> uniforms: Uniforms;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var out: VertexOutput;
    // Positions are already NDC; render-pass viewport clips the
    // oversize corners to the actual framebuffer rect.
    out.position = vec4<f32>(vertex_position(vertex_index), 0.0, 1.0);
    return out;
}

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 {
        return c * 12.92;
    }
    return 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    // Fragment position in physical pixels (top-left origin in WGSL).
    let frag = input.position.xy;

    // Map fragment position to UV [0, 1] over the scissor region.
    let uv = vec2<f32>(
        (frag.x - uniforms.sc_x) / uniforms.sc_w,
        (frag.y - uniforms.sc_y) / uniforms.sc_h,
    );

    // Sample the linear mono texture.
    let mono_linear = textureSample(t_mono, s_mono, uv).r;

    // Apply exposure in linear light. `exposure` is the linear-light
    // gain (2^EV), computed once on the Rust side per slider change.
    let exposed = mono_linear * uniforms.exposure;
    let clamped = clamp(exposed, 0.0, 1.0);

    // Linear → sRGB transfer function (scalar, single-channel).
    let srgb = linear_to_srgb(clamped);

    return vec4<f32>(srgb, srgb, srgb, 1.0);
}
