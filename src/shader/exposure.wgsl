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
    tex_w: f32,      // texture width in pixels
    tex_h: f32,      // texture height in pixels
    sc_x: f32,       // scissor rect origin x (physical pixels)
    sc_y: f32,       // scissor rect origin y (physical pixels)
    sc_w: f32,       // scissor rect width
    sc_h: f32,       // scissor rect height
    zoom: f32,       // detail-view zoom, 1.0 = contain fit, each +1 doubles scale
    pan_x: f32,      // pan offset of the image center, physical pixels
    pan_y: f32,
    // Live tone curve applied to the sampled positive: `clamp(ratio * p^exp, 0, 1)`.
    // The CPU folds the contrast/rolloff power curves (pivoted at the image's
    // measured mid-gray and white point) into this single pair; both are 1.0 at
    // the defaults, making the remap the identity.
    curve_ratio: f32,
    curve_exp: f32,
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

fn in_bounds(uv: vec2<f32>) -> bool {
    return uv.x >= 0.0 && uv.x <= 1.0 && uv.y >= 0.0 && uv.y <= 1.0;
}

/// Maps a fragment position (physical pixels) to the texture UV to sample,
/// applying the detail view's zoom/pan transform.
///
/// The rendered image is scaled relative to the contain-fit base:
/// `scale = min(sc_w/tex_w, sc_h/tex_h) * 2^(zoom - 1)`. At `zoom == 1` this
/// reproduces `ContentFit::Contain` exactly (whole frame visible, letterbox
/// bars). Each +1 zoom unit doubles the scale, so the frame crosses cover
/// (image fills the widget, bars gone) at `1 + log2(cover/contain)` and
/// overflows beyond that — zooming is never confined to the letterbox band.
/// The image stays centered in the widget, displaced by `pan` physical px.
fn view_uv(frag: vec2<f32>) -> vec2<f32> {
    let contain = min(uniforms.sc_w / uniforms.tex_w, uniforms.sc_h / uniforms.tex_h);
    let scale = contain * exp2(uniforms.zoom - 1.0);
    let rw = uniforms.tex_w * scale;
    let rh = uniforms.tex_h * scale;
    let box_x = uniforms.sc_x + (uniforms.sc_w - rw) * 0.5 + uniforms.pan_x;
    let box_y = uniforms.sc_y + (uniforms.sc_h - rh) * 0.5 + uniforms.pan_y;
    return vec2<f32>(
        (frag.x - box_x) / rw,
        (frag.y - box_y) / rh,
    );
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let frag = input.position.xy;
    let uv = view_uv(frag);

    // Sample within [0, 1] (clamped to avoid sampler wrap reads at sub-rect
    // edges when rasterizing across the contained boundary).
    let mono_linear = textureSample(t_mono, s_mono, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0))).r;

    // Live tone curve re-shapes the baked positive's values: the CPU folds
    // the contrast power (pivot at the image's measured mid-gray) and the
    // highlight-rolloff power (pivot at the measured white point) into one
    // `ratio * p^exp`. Identity at the defaults (byte-identical render).
    let remapped = clamp(uniforms.curve_ratio * pow(mono_linear, uniforms.curve_exp), 0.0, 1.0);

    // Linear-light exposure via the Rust-computed 2^EV gain.
    let exposed = remapped * uniforms.exposure;
    let clamped = clamp(exposed, 0.0, 1.0);
    let srgb = linear_to_srgb(clamped);

    // Outside the image rect (letterbox/pillarbox bars at low zoom, or the
    // edges revealed while panning past the frame at high zoom), output
    // transparent (alpha=0) so the COSMIC panel background shows through.
    // The render pipeline is `BlendState::ALPHA_BLENDING`, so alpha=0 lets
    // the prior framebuffer contents pass through.
    let inside = in_bounds(uv);
    let rgb = select(vec3<f32>(0.0), vec3<f32>(srgb), inside);
    let alpha = select(0.0, 1.0, inside);
    return vec4<f32>(rgb, alpha);
}