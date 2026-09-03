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
    // Live keyboard crop, IN TEXTURE PIXELS (the source-pixel margins scaled by
    // tex/src per axis on the CPU): the sub-rectangle of the texture to show.
    // `crop_l`/`crop_t` is the region origin (left/top margins removed) and
    // `crop_w`/`crop_h` its size (texture minus the removed margins). All four
    // are zero/identity when there is no crop, reproducing the full frame.
    crop_l: f32,
    crop_t: f32,
    crop_w: f32,
    crop_h: f32,
    // When non-zero, draw the "view dimmed crop area" overlay: the full uncropped
    // frame at its own contain-fit on top of the zoomed crop, dimming everything
    // outside the crop rectangle so the user can see where the crop lands. The
    // overlay reuses the same zoom/pan; anything beyond the shader region is
    // clipped by the viewport. 0.0 = base zoom-crop view only.
    show_mask: f32,
    // Live tone curve applied to the sampled positive: `clamp(ratio * p^exp, 0, 1)`.
    // The CPU folds the contrast/rolloff/shadows power curves (pivoted at the
    // image's measured mid-gray, white point, and shadow anchor) into this
    // single pair; all are identity at the defaults, making the remap a no-op.
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

/// Maps a fragment position (physical pixels) to the texture UV to sample,
/// applying the detail view's zoom/pan transform and the live keyboard crop.
///
/// The image scale is anchored to the CROPPED frame's contain-fit base:
/// `scale = min(sc_w/crop_w, sc_h/crop_h) * 2^(zoom - 1)`. At `zoom == 1` the
/// remaining (uncropped-away) region contain-fits the widget, so trimming edges
/// re-fits/zooms the kept content to fill the box. Each +1 zoom unit doubles the
/// scale; the image stays centered, displaced by `pan` physical px.
///
/// The sample position maps `rel` into the cropped sub-rectangle
/// (`crop_l..crop_l+crop_w` × `crop_t..crop_t+crop_h`, in texture pixels), so the
/// UV spans only the kept content. A zero crop (`crop_w == tex_w`, `crop_l == 0`)
/// is exact identity.
///
/// When `show_mask != 0.0`, [`overlay_sample`] additionally lays out the full
/// uncropped frame on top (same zoom/pan, its own contain-fit on the full
/// texture) and flags whether each fragment lies outside the crop rectangle so
/// [`fs_main`] can dim it — the "view dimmed crop area" overlay.
struct ViewSample {
    uv: vec2<f32>,
    inside: bool,
}

fn in_bounds(uv: vec2<f32>) -> bool {
    return uv.x >= 0.0 && uv.x <= 1.0 && uv.y >= 0.0 && uv.y <= 1.0;
}

/// The full-frame "view dimmed crop area" overlay sample: `inside` is true when
/// the fragment lies on the full uncropped frame (its own contain-fit box), and
/// `dim` is 1.0 when it falls OUTSIDE the crop rectangle (in that full-frame
/// layout) so [`fs_main`] can darken it. Reuses the same zoom/pan as the base
/// view; the overlay never re-sizes or re-zooms the image. Any part of the full
/// frame beyond the shader region is clipped by the render-pass viewport.
struct OverlaySample {
    uv: vec2<f32>,
    inside: bool,
    dim: f32,
}

fn overlay_sample(frag: vec2<f32>) -> OverlaySample {
    // Contain both axes of the FULL source texture (independent of the base
    // zoom-crop layout) with the same zoom/pan transform.
    let contain = min(uniforms.sc_w / uniforms.tex_w, uniforms.sc_h / uniforms.tex_h);
    let scale = contain * exp2(uniforms.zoom - 1.0);
    let rw = uniforms.tex_w * scale;
    let rh = uniforms.tex_h * scale;
    let box_x = uniforms.sc_x + (uniforms.sc_w - rw) * 0.5 + uniforms.pan_x;
    let box_y = uniforms.sc_y + (uniforms.sc_h - rh) * 0.5 + uniforms.pan_y;
    let rel = vec2<f32>(
        (frag.x - box_x) / rw,
        (frag.y - box_y) / rh,
    );
    // The cropped sub-rectangle expressed in this full-frame screen layout.
    let cx0 = box_x + (uniforms.crop_l / uniforms.tex_w) * rw;
    let cx1 = box_x + ((uniforms.crop_l + uniforms.crop_w) / uniforms.tex_w) * rw;
    let cy0 = box_y + (uniforms.crop_t / uniforms.tex_h) * rh;
    let cy1 = box_y + ((uniforms.crop_t + uniforms.crop_h) / uniforms.tex_h) * rh;
    let in_crop_rect = frag.x >= cx0 && frag.x <= cx1 && frag.y >= cy0 && frag.y <= cy1;
    let inside = in_bounds(rel);
    let dim = select(1.0, 0.0, !inside || in_crop_rect);
    return OverlaySample(rel, inside, dim);
}

fn view_uv(frag: vec2<f32>) -> ViewSample {
    // Contain both axes of the CROPPED frame; trimming re-fits/zooms.
    let contain = min(uniforms.sc_w / uniforms.crop_w, uniforms.sc_h / uniforms.crop_h);
    let scale = contain * exp2(uniforms.zoom - 1.0);
    let rw = uniforms.crop_w * scale;
    let rh = uniforms.crop_h * scale;
    let box_x = uniforms.sc_x + (uniforms.sc_w - rw) * 0.5 + uniforms.pan_x;
    let box_y = uniforms.sc_y + (uniforms.sc_h - rh) * 0.5 + uniforms.pan_y;
    // Position over the crop box, then map into the cropped sub-rect UV.
    let rel = vec2<f32>(
        (frag.x - box_x) / rw,
        (frag.y - box_y) / rh,
    );
    let uv = vec2<f32>(
        uniforms.crop_l / uniforms.tex_w + rel.x * (uniforms.crop_w / uniforms.tex_w),
        uniforms.crop_t / uniforms.tex_h + rel.y * (uniforms.crop_h / uniforms.tex_h),
    );
    let inside = in_bounds(rel);
    return ViewSample(uv, inside);
}

/// Sample + tone-curve + exposure + sRGB-encode, shared by both the base
/// zoom-crop layer and the full-frame dim overlay so they match exactly.
fn shade(uv: vec2<f32>) -> f32 {
    // Sample within [0, 1] (clamped to avoid sampler wrap reads at sub-rect
    // edges when rasterizing across the contained boundary).
    let mono_linear = textureSample(t_mono, s_mono, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0))).r;

    // Live tone curve re-shapes the baked positive's values: the CPU folds
    // the contrast power (pivot at the image's measured mid-gray), the
    // highlight-rolloff power (pivot at the measured white point), and the
    // shadows power (pivot at the measured shadow anchor) into one
    // `ratio * p^exp`. Identity at the defaults (byte-identical render).
    let remapped = clamp(uniforms.curve_ratio * pow(mono_linear, uniforms.curve_exp), 0.0, 1.0);

    // Linear-light exposure via the Rust-computed 2^EV gain.
    let exposed = remapped * uniforms.exposure;
    let clamped = clamp(exposed, 0.0, 1.0);
    return linear_to_srgb(clamped);
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let frag = input.position.xy;
    let sample = view_uv(frag);
    let srgb = shade(sample.uv);

    // When the dim-overlay is on, take the fragment's color from the full-frame
    // overlay wherever it is on the uncropped frame, darkening outside the crop.
    var rgb = srgb;
    var alpha = select(0.0, 1.0, sample.inside);
    if uniforms.show_mask != 0.0 {
        let overlay = overlay_sample(frag);
        if overlay.inside {
            let overlay_srgb = shade(overlay.uv);
            // Dim (multiply down) the region outside the crop rectangle.
            let factor = 1.0 - 0.85 * overlay.dim;
            rgb = overlay_srgb * factor;
            alpha = 1.0;
        }
    }

    // Outside both the base image rect and any overlay region (letterbox/
    // pillarbox bars at low zoom, the edges revealed while panning, or the
    // trimmed margins), output transparent (alpha=0) so the COSMIC panel
    // background shows through via `BlendState::ALPHA_BLENDING`.
    return vec4<f32>(rgb, rgb, rgb, alpha);
}