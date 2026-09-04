// SPDX-License-Identifier: GPL-3.0-or-later

//! GPU detail shader — renders mono image data with live EV adjustment.
//!
//! The mono `Vec<f32>` is uploaded to the GPU once as an `R16Float` texture.
//! Exposure and the tone curve are applied as shader uniforms (`2^EV` gain and
//! a pivoted `ratio * p^exp` remap) — zero CPU re-encoding, zero new `Handle`
//! per frame.

use cosmic::iced::core::{Length, Rectangle};
use cosmic::iced::wgpu::util::DeviceExt;
use cosmic::iced::widget::shader::{Pipeline, Primitive, Program, Shader, Viewport};

use crate::edit_manifest::CropMargins;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// GPU-backed detail view image with live exposure adjustment.
pub struct DetailProgram {
    mono: Vec<f32>,
    width: u32,
    height: u32,
    /// The full-resolution (post-masked-border, pre-downscale) display-oriented
    /// source dimensions the crop margins are authored against, restored from
    /// the downscaled texture + the sensor's true long edge at construction
    /// (both share the same aspect, so the scale per axis is recovered). The
    /// texture is a downscale of this; the shader maps source-pixel crop
    /// margins onto the texture by `tex/src` per axis. At the native level-up
    /// (texture == source) the scale collapses to 1 and the crop is exact.
    src_w: u32,
    src_h: u32,
    /// Raw EV value in stops; converted to `2^EV` once on the GPU side per
    /// slider change so the WGSL shader sees a linear-light gain.
    exposure: f32,
    /// Detail-view zoom in `log2` units: 1.0 = contain fit, each +1 doubles
    /// the rendered scale (see [`DetailPrimitive::prepare`]).
    zoom: f32,
    /// Pan offset of the image center from the widget center, in logical
    /// points; converted to physical pixels on the GPU side.
    pan: (f32, f32),
    /// Median of the uploaded positive: the mid-gray the contrast curve
    /// pivots around. Measured once at construction from `mono`.
    mid: f32,
    /// 98th percentile of the uploaded positive: the white point the
    /// highlight-rolloff curve pivots around.
    white: f32,
    /// 10th percentile of the uploaded positive: the shadow anchor the
    /// shadows curve pivots around. Measured once at construction from `mono`.
    shadow: f32,
    /// Contrast power `kc`: the live tone curve pivots at `mid`,
    /// `C(p) = mid^(1-kc) * p^kc`. `1.0` = identity (no contrast change).
    contrast: f32,
    /// Highlight-rolloff power `kr`: the live tone curve pivots at `white`,
    /// `R(p) = white^(1-kr) * p^kr`. `1.0` = identity.
    rolloff: f32,
    /// Shadows power `ks`: the live tone curve pivots at `shadow`,
    /// `S(p) = shadow^(1-ks) * p^ks`. `1.0` = identity.
    shadows: f32,
    /// Live source-pixel crop margins removed from each edge of the
    /// full-resolution display-oriented frame. Applied as a uniform UV-remap
    /// (no texture re-upload): at render time [`crop_uv_geometry`] scales them
    /// onto the downscaled texture by `tex_axis/src_axis`, so the live trim and
    /// the CPU thumbnail bake (which crops the same source-pixel margins onto
    /// the print) stay in the same reference frame. A keyboard trim is instant.
    /// `CropMargins::default()` (all zero) shows the full frame.
    crop: CropMargins,
    /// Whether the "view dimmed crop area" overlay is shown: the full
    /// uncropped frame drawn on top of the zoomed crop with everything outside
    /// the crop rectangle dimmed. View-only — never persisted or baked.
    show_mask: bool,
    /// Monotonic id bumped by the app model on each new detail decode. Used
    /// to detect image changes and rebuild the GPU texture/bind group.
    image_id: u64,
}

impl DetailProgram {
    /// Create a new program for the given mono image.
    ///
    /// `mono` is linear, inverted-positive pre-sRGB data (one `f32` per pixel,
    /// row-major, top-to-bottom). `width`/`height` are the **texture** dims
    /// (the downscale of the source, display-oriented). `src_long_edge` is the
    /// sensor's true long edge AFTER masked-border cropping and BEFORE the
    /// downscale — it restores the full-resolution display-oriented source dims
    /// (`source_dimensions`) that the crop margins are authored against.
    /// `exposure` is the raw EV value; the gain sent to the GPU is `2^EV`.
    /// `crop` is the frame's stored source-pixel crop (all-zero for a fresh
    /// frame). The view starts at contain fit (zoom 1.0, no pan) with an
    /// identity tone curve. The shadow/mid-gray/white-point pivots are measured
    /// from `mono` once here.
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(
        mono: Vec<f32>,
        width: u32,
        height: u32,
        exposure: f32,
        crop: CropMargins,
        image_id: u64,
        src_long_edge: u32,
    ) -> Self {
        let (shadow, mid, white) = tone_anchors(&mono);
        // The texture shares the display source's aspect (resize_area +
        // orientation preserve it within floor-rounding), so the two source
        // axes are recovered from the long edge: the long-edged axis equals
        // `src_long_edge`, the short axis scales its texture counterpart.
        let (src_w, src_h) = if width >= height {
            (
                src_long_edge.max(1),
                (((u64::from(height) * u64::from(src_long_edge)) + u64::from(width) / 2)
                    / u64::from(width.max(1))) as u32,
            )
        } else {
            (
                (((u64::from(width) * u64::from(src_long_edge)) + u64::from(height) / 2)
                    / u64::from(height.max(1))) as u32,
                src_long_edge.max(1),
            )
        };
        Self {
            mono,
            width,
            height,
            src_w,
            src_h,
            exposure,
            zoom: 1.0,
            pan: (0.0, 0.0),
            mid,
            white,
            shadow,
            contrast: 1.0,
            rolloff: 1.0,
            shadows: 1.0,
            crop,
            show_mask: false,
            image_id,
        }
    }

    /// Update the exposure value (called on slider drag).
    pub fn set_exposure(&mut self, ev: f32) {
        self.exposure = ev;
    }

    /// Update the zoom/pan transform (called on detail-view wheel/drag).
    ///
    /// `zoom` is in `log2` units (1.0 = contain fit). `pan` is in logical
    /// points relative to the widget center.
    pub fn set_view(&mut self, zoom: f32, pan: (f32, f32)) {
        self.zoom = zoom;
        self.pan = pan;
    }

    /// Update the contrast/rolloff/shadows tone curve (called on editing
    /// drawer sliders). Only the two remap uniforms change — the uploaded
    /// texture stays the fixed render, re-curved per pixel in WGSL.
    ///
    /// `contrast` pivots at the image's measured mid-gray, `rolloff` at the
    /// measured white point, `shadows` at the measured 10th-percentile shadow
    /// anchor (`1.0` = identity for each); all three compose into the single
    /// `ratio * p^exp` remap the shader applies.
    pub fn set_curve(&mut self, contrast: f32, rolloff: f32, shadows: f32) {
        self.contrast = contrast;
        self.rolloff = rolloff;
        self.shadows = shadows;
    }

    /// Update the live crop margins (called on each keyboard trim).
    ///
    /// Only the four crop uniforms change — the uploaded texture stays intact.
    /// The WGSL zoom-to-fits the CROPPED region into the widget; the overlay
    /// full-frame layer (see [`Self::set_show_mask`]) dims outside the crop.
    pub fn set_crop(&mut self, crop: CropMargins) {
        self.crop = crop;
    }

    /// Show or hide the "view dimmed crop area" overlay (called on toggle).
    ///
    /// When on, the shader draws the full uncropped frame on top of the zoomed
    /// crop (same zoom/pan) and dims everything outside the crop rectangle,
    /// letting the user see where the crop lands over the whole negative.
    /// View-only state: never persisted or applied to grid/cover bakes.
    pub fn set_show_mask(&mut self, on: bool) {
        self.show_mask = on;
    }

    /// The full-resolution display-oriented source dimensions the persisted
    /// crop margins are authored against (the sensor's post-masked-border dims,
    /// oriented, before any downscale). The crop keyboard math must bound its
    /// trims against THIS frame — the downscaled texture dims would store a
    /// texture-pixel crop the thumbnail bake mis-scales.
    #[must_use]
    pub fn source_dimensions(&self) -> (u32, u32) {
        (self.src_w, self.src_h)
    }

    /// Wrap in a `Shader` widget sized to fill the parent.
    pub fn view<M>(&self) -> Shader<M, Self> {
        Shader::new(self.clone())
            .width(Length::Fill)
            .height(Length::Fill)
    }
}

impl Clone for DetailProgram {
    fn clone(&self) -> Self {
        Self {
            mono: self.mono.clone(),
            width: self.width,
            height: self.height,
            src_w: self.src_w,
            src_h: self.src_h,
            exposure: self.exposure,
            zoom: self.zoom,
            pan: self.pan,
            mid: self.mid,
            white: self.white,
            shadow: self.shadow,
            contrast: self.contrast,
            rolloff: self.rolloff,
            shadows: self.shadows,
            crop: self.crop,
            show_mask: self.show_mask,
            image_id: self.image_id,
        }
    }
}

impl std::fmt::Debug for DetailProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DetailProgram")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("src_w", &self.src_w)
            .field("src_h", &self.src_h)
            .field("exposure", &self.exposure)
            .field("zoom", &self.zoom)
            .field("pan", &self.pan)
            .field("mid", &self.mid)
            .field("white", &self.white)
            .field("shadow", &self.shadow)
            .field("contrast", &self.contrast)
            .field("rolloff", &self.rolloff)
            .field("shadows", &self.shadows)
            .field("crop", &self.crop)
            .field("show_mask", &self.show_mask)
            .field("mono_len", &self.mono.len())
            .field("image_id", &self.image_id)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// iced::widget::shader::Program implementation
// ---------------------------------------------------------------------------

impl<M> Program<M> for DetailProgram {
    type State = ();
    type Primitive = DetailPrimitive;

    fn draw(
        &self,
        _state: &(),
        _cursor: cosmic::iced::mouse::Cursor,
        _bounds: Rectangle,
    ) -> Self::Primitive {
        DetailPrimitive {
            mono: self.mono.clone(),
            exposure: self.exposure,
            zoom: self.zoom,
            pan: self.pan,
            mid: self.mid,
            white: self.white,
            shadow: self.shadow,
            contrast: self.contrast,
            rolloff: self.rolloff,
            shadows: self.shadows,
            crop: self.crop,
            show_mask: self.show_mask,
            width: self.width,
            height: self.height,
            src_w: self.src_w,
            src_h: self.src_h,
            image_id: self.image_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Crop-geometry + tone-curve pure helpers
// ---------------------------------------------------------------------------

/// The live-crop sub-rectangle in **texture space** as `(origin, size)`, both
/// `(x, y)` pairs, given the source-pixel crop margins, the downscaled texture
/// dims (`tex_w`/`tex_h`) and the full-resolution display-oriented source dims
/// (`src_w`/`src_h`) the margins are authored against. Each margin axis scales
/// by `tex_axis/src_axis`, so a stored source-pixel crop removes the same
/// *fraction of the frame* at any texture resolution (overview 2048 or native)
/// and matches what the CPU thumbnail bake does. An all-zero crop yields origin
/// `(0, 0)` and size equal to the texture — the identity no-op the WGSL remap
/// leaves untouched.
#[allow(clippy::cast_precision_loss)]
#[must_use]
fn crop_uv_geometry(
    crop: CropMargins,
    tex_w: u32,
    tex_h: u32,
    src_w: u32,
    src_h: u32,
) -> ((f32, f32), (f32, f32)) {
    let sx = tex_w as f32 / src_w.max(1) as f32;
    let sy = tex_h as f32 / src_h.max(1) as f32;
    (
        (crop.left as f32 * sx, crop.top as f32 * sy),
        (
            crop.cropped_width(src_w) as f32 * sx,
            crop.cropped_height(src_h) as f32 * sy,
        ),
    )
}

/// Bins per axis for the anchor histogram. 4096 bins over [0,1] resolve the
/// median and the 98th-percentile white point to ~2.4e-4 absolute — far finer
/// than the f16 texture's ~2^-11 and more than any preview needs.
const ANCHOR_BINS: usize = 4096;

/// Smallest anchor accepted. Guards `pow(0, negative)` → NaN for degenerate
/// all-black frames, where the 50th/98th percentile of noise can land at 0.
pub(crate) const MIN_ANCHOR: f32 = 1e-3;

/// Measure the tone anchors a pivoted curve needs, from the uploaded positive
/// (`p` in [0,1]): the 10th percentile (the shadow anchor, robust against a
/// few pure-black pixels), the median (a stable mid-gray), and the 98th
/// percentile (a robust white point, insensitive to a few hot specular
/// pixels). Single pass over the mono, O(n) — no sort of a 2048² buffer.
///
/// `pub(crate)` so the CPU thumbnail bake reuses the same anchor machinery the
/// detail shader does, keeping grid and detail measurements aligned.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub(crate) fn tone_anchors(mono: &[f32]) -> (f32, f32, f32) {
    // Heap-allocated (4096 × u64 ≈ 32 KB would trip `large_stack_arrays`).
    let mut bins = vec![0_u64; ANCHOR_BINS];
    for &v in mono {
        let v = v.clamp(0.0, 1.0);
        // `(v * (BINS-1))` maps 0..1 to bin 0..BINS-1; truncation is fine
        // because binning is deliberately approximate.
        let idx = (v * (ANCHOR_BINS - 1) as f32) as usize;
        bins[idx.min(ANCHOR_BINS - 1)] += 1;
    }

    if mono.is_empty() {
        return (MIN_ANCHOR, MIN_ANCHOR, MIN_ANCHOR);
    }

    let total = mono.len();
    let shadow = percentile(&bins, total, 0.10);
    let mid = percentile(&bins, total, 0.5);
    let white = percentile(&bins, total, 0.98);
    (
        shadow.max(MIN_ANCHOR),
        mid.max(MIN_ANCHOR),
        white.max(MIN_ANCHOR),
    )
}

/// The value of the `q`-quantile (0..=1) of the bin counts, as a coordinate
/// in [0,1]. Walks the cumulative distribution until it reaches the
/// `q·total`-th element; for a non-trivial `total` this lands just past the
/// low-population boundary bins that a strict `>` test would skip.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn percentile(bins: &[u64], total: usize, q: f32) -> f32 {
    let target = (total as f32 * q) as u64;
    let mut cumulative = 0_u64;
    for (i, &count) in bins.iter().enumerate() {
        cumulative += count;
        if cumulative >= target {
            return i as f32 / (ANCHOR_BINS - 1) as f32;
        }
    }
    1.0
}

/// Precompute the per-frame tone remap `T(p) = clamp(ratio * p^exp, 0, 1)`.
///
/// Contrast, rolloff, and shadows are power curves pivoted at the image's
/// measured mid-gray (`mid`), white point (`white`), and shadow anchor
/// (`shadow`):
///
/// `C(p) = mid^(1-kc) · p^kc`          (pivot at `mid`: `C(mid) = mid`)
/// `R(p) = white^(1-kr) · p^kr`         (pivot at `white`: `R(white) = white`)
/// `S(p) = shadow^(1-ks) · p^ks`        (pivot at `shadow`: `S(shadow) =
///                                                    shadow`)
///
/// Three monotone powers compose exactly into one power, so all three fit a
/// single `(ratio, exp)` pair the WGSL shader applies as `ratio·p^exp`:
/// `S∘R∘C(p) = (shadow^(1-ks) · white^(ks(1-kr)) · mid^(ks·kr(1-kc))) · p^(kc·kr·ks)`.
/// At `kc = kr = ks = 1` the remap is the identity (`ratio = 1`, `exp = 1`),
/// so untouched renders stay byte-identical.
///
/// `pub(crate)` — the shared helper the CPU thumbnail bake reuses so grid and
/// detail agree, alongside the GPU `prepare()` fold.
#[allow(clippy::too_many_arguments)]
pub(crate) fn curve_remap(
    contrast: f32,
    rolloff: f32,
    shadows: f32,
    shadow: f32,
    mid: f32,
    white: f32,
) -> (f32, f32) {
    let ratio = shadow.powf(1.0 - shadows)
        * white.powf(shadows * (1.0 - rolloff))
        * mid.powf(shadows * rolloff * (1.0 - contrast));
    let exponent = contrast * rolloff * shadows;
    (ratio, exponent)
}

/// Apply the composed tone remap `T(p) = clamp(ratio · p^exp, 0, 1)` to a mono
/// positive in place.
///
/// This is the CPU twin of the WGSL's `clamp(curve_ratio * pow(p, curve_exp),
/// 0, 1)` — the exact expression the grid thumbnail bake must match so a
/// baked tile and the detail shader produce identical tones. Identity at the
/// `(1.0, 1.0, 1.0)` defaults; `shadow`/`mid`/`white` are the same anchors
/// [`tone_anchors`] measures. A separate op from exposure (which the shader
/// applies after), so callers must apply this *before* the `2^EV` gain to
/// mirror the shader.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_curve(
    mono: &mut [f32],
    contrast: f32,
    rolloff: f32,
    shadows: f32,
    shadow: f32,
    mid: f32,
    white: f32,
) {
    let (ratio, exponent) = curve_remap(contrast, rolloff, shadows, shadow, mid, white);
    for value in mono.iter_mut() {
        *value = (ratio * value.powf(exponent)).clamp(0.0, 1.0);
    }
}

// ---------------------------------------------------------------------------
// iced::wgpu::primitive::Primitive implementation
// ---------------------------------------------------------------------------

/// Per-frame data sent from `Program::draw()` to the GPU pipeline.
///
/// Carries a clone of the mono data on the first frame so the pipeline can
/// create the GPU texture.  `image_id` lets the pipeline detect when a new
/// image is selected (iceD caches pipelines per type, so we cannot rely on
/// `initialized` alone).
#[derive(Debug, Clone)]
pub struct DetailPrimitive {
    mono: Vec<f32>,
    exposure: f32,
    /// Detail-view zoom in `log2` units; 1.0 = contain fit.
    zoom: f32,
    /// Pan offset of the image center from the widget center (logical points).
    pan: (f32, f32),
    /// Mid-gray pivot (median of the positive), measured once at decode.
    mid: f32,
    /// White-point pivot (98th percentile of the positive).
    white: f32,
    /// Shadow anchor (10th percentile of the positive).
    shadow: f32,
    /// Contrast power (pivots at `mid`; 1.0 = identity).
    contrast: f32,
    /// Highlight-rolloff power (pivots at `white`; 1.0 = identity).
    rolloff: f32,
    /// Shadows power (pivots at `shadow`; 1.0 = identity).
    shadows: f32,
    /// Live source-pixel crop margins removed from each edge (uniform UV-remap).
    crop: CropMargins,
    /// Whether the "view dimmed crop area" overlay is shown (full frame on top
    /// of the zoomed crop, dimmed outside the crop rect).
    show_mask: bool,
    width: u32,
    height: u32,
    /// Full-resolution display-oriented source dims the crop margins are
    /// authored against (see [`crop_uv_geometry`]).
    src_w: u32,
    src_h: u32,
    image_id: u64,
}

impl Primitive for DetailPrimitive {
    type Pipeline = DetailPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &cosmic::iced::wgpu::Device,
        queue: &cosmic::iced::wgpu::Queue,
        bounds: &Rectangle,
        viewport: &Viewport,
    ) {
        // --- Rebuild texture + bind group if image identity changed ---
        // `iced` caches the `DetailPipeline` across image selections because
        // both selections share the same pipeline type. `initialized` only
        // catches the very first frame; tracking `image_id` catches every
        // subsequent image swap too.
        let needs_new_texture =
            !pipeline.initialized || pipeline.current_image_id != Some(self.image_id);

        if needs_new_texture {
            let tex = device.create_texture(&cosmic::iced::wgpu::TextureDescriptor {
                label: Some("exposure mono"),
                size: cosmic::iced::wgpu::Extent3d {
                    width: self.width,
                    height: self.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: cosmic::iced::wgpu::TextureDimension::D2,
                format: cosmic::iced::wgpu::TextureFormat::R16Float,
                usage: cosmic::iced::wgpu::TextureUsages::TEXTURE_BINDING
                    | cosmic::iced::wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            queue.write_texture(
                cosmic::iced::wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: 0,
                    origin: cosmic::iced::wgpu::Origin3d::ZERO,
                    aspect: cosmic::iced::wgpu::TextureAspect::All,
                },
                &mono_to_half_bytes(&self.mono),
                cosmic::iced::wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.width * 2),
                    rows_per_image: Some(self.height),
                },
                cosmic::iced::wgpu::Extent3d {
                    width: self.width,
                    height: self.height,
                    depth_or_array_layers: 1,
                },
            );

            let texture_view =
                tex.create_view(&cosmic::iced::wgpu::TextureViewDescriptor::default());

            pipeline.texture = Some(texture_view);
            pipeline.bind_group = Some(device.create_bind_group(
                &cosmic::iced::wgpu::BindGroupDescriptor {
                    label: Some("exposure bg"),
                    layout: &pipeline.bind_group_layout,
                    entries: &[
                        cosmic::iced::wgpu::BindGroupEntry {
                            binding: 0,
                            resource: cosmic::iced::wgpu::BindingResource::TextureView(
                                pipeline.texture.as_ref().unwrap(),
                            ),
                        },
                        cosmic::iced::wgpu::BindGroupEntry {
                            binding: 1,
                            resource: cosmic::iced::wgpu::BindingResource::Sampler(
                                &pipeline.sampler,
                            ),
                        },
                        cosmic::iced::wgpu::BindGroupEntry {
                            binding: 2,
                            resource: pipeline.uniform_buf.as_entire_binding(),
                        },
                    ],
                },
            ));
            pipeline.current_image_id = Some(self.image_id);
            pipeline.initialized = true;
        }

        // --- Per-frame: update uniform buffer ---
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let sf = viewport.scale_factor() as f32;
        #[allow(clippy::cast_precision_loss)]
        let tex_w = self.width as f32;
        #[allow(clippy::cast_precision_loss)]
        let tex_h = self.height as f32;
        // Live crop: the texture sub-rectangle to show. Origin is the removed
        // left/top margins and size the kept region, mapped from the stored
        // source-pixel crop onto the DOWNSCALED texture by `tex/src` per axis
        // (see [`crop_uv_geometry`]); at the native level-up the texture IS the
        // source and the map is 1:1. The WGSL contain-fits the crop into the
        // widget (base view) and, when the dim overlay is on, also lays out the
        // full texture to show the crop in context. An all-zero crop → origin 0
        // and size == texture.
        let (crop_origin, (crop_w, crop_h)) =
            crop_uv_geometry(self.crop, self.width, self.height, self.src_w, self.src_h);
        let (crop_l, crop_t) = crop_origin;
        // Live tone remap: contrast pivots at the measured mid-gray, highlight
        // rolloff at the measured white point, shadows at the measured shadow
        // anchor. Composed on the CPU into one `ratio · p^exp`; identity at
        // the 1.0 defaults, so untouched renders stay byte-identical to the
        // pre-curve pass.
        let (curve_ratio, curve_exp) = curve_remap(
            self.contrast,
            self.rolloff,
            self.shadows,
            self.shadow,
            self.mid,
            self.white,
        );
        let uniforms = Uniforms {
            // Convert raw EV (slider value) to linear-light gain once per
            // frame; the WGSL shader reads this as a direct multiplier.
            exposure: self.exposure.exp2(),
            // Texture dimensions feed the WGSL's contained-fit math so the
            // shader mirrors `widget::image.content_fit(ContentFit::Contain)`.
            tex_w,
            tex_h,
            sc_x: bounds.x * sf,
            sc_y: bounds.y * sf,
            sc_w: bounds.width * sf,
            sc_h: bounds.height * sf,
            // Detail-view transform: zoom in log2 units (1.0 = contain fit);
            // pan in logical points converted to physical pixels, same
            // convention as the scissor rect above.
            zoom: self.zoom,
            pan_x: self.pan.0 * sf,
            pan_y: self.pan.1 * sf,
            crop_l,
            crop_t,
            crop_w,
            crop_h,
            // Whether to draw the full-frame dim overlay (see set_show_mask).
            show_mask: if self.show_mask { 1.0 } else { 0.0 },
            // Tone curve: `clamp(ratio * p^exp, 0, 1)`, applied before the
            // exposure multiply.
            curve_ratio,
            curve_exp,
        };
        queue.write_buffer(&pipeline.uniform_buf, 0, bytemuck::bytes_of(&uniforms));
    }

    fn render(
        &self,
        pipeline: &Self::Pipeline,
        encoder: &mut cosmic::iced::wgpu::CommandEncoder,
        target: &cosmic::iced::wgpu::TextureView,
        clip_bounds: &Rectangle<u32>,
    ) {
        let (_tex_view, Some(pipeline_obj), Some(bind_group)) = (
            &pipeline.texture,
            &pipeline.render_pipeline,
            &pipeline.bind_group,
        ) else {
            return;
        };

        {
            let mut pass = encoder.begin_render_pass(&cosmic::iced::wgpu::RenderPassDescriptor {
                label: Some("exposure render"),
                color_attachments: &[Some(cosmic::iced::wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: cosmic::iced::wgpu::Operations {
                        load: cosmic::iced::wgpu::LoadOp::Load,
                        store: cosmic::iced::wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            #[allow(clippy::cast_precision_loss)]
            pass.set_viewport(
                clip_bounds.x as f32,
                clip_bounds.y as f32,
                clip_bounds.width as f32,
                clip_bounds.height as f32,
                0.0,
                1.0,
            );
            pass.set_pipeline(pipeline_obj);
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
    }
}

// ---------------------------------------------------------------------------
// GPU pipeline (created once per image, stored in iced's primitive storage)
// ---------------------------------------------------------------------------

/// GPU resources shared across all `DetailPrimitive` instances of the same
/// image.  Created lazily on the first `prepare()` call (needs the mono data
/// to build the texture).
pub struct DetailPipeline {
    texture: Option<cosmic::iced::wgpu::TextureView>,
    uniform_buf: cosmic::iced::wgpu::Buffer,
    sampler: cosmic::iced::wgpu::Sampler,
    bind_group_layout: cosmic::iced::wgpu::BindGroupLayout,
    bind_group: Option<cosmic::iced::wgpu::BindGroup>,
    render_pipeline: Option<cosmic::iced::wgpu::RenderPipeline>,
    initialized: bool,
    /// Identity of the image currently installed in the GPU texture. Compared
    /// against the primitive's `image_id` on every `prepare()` call to detect
    /// that the user has selected a different photo.
    current_image_id: Option<u64>,
}

impl std::fmt::Debug for DetailPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DetailPipeline")
            .field("initialized", &self.initialized)
            .finish_non_exhaustive()
    }
}

impl Pipeline for DetailPipeline {
    fn new(
        device: &cosmic::iced::wgpu::Device,
        _queue: &cosmic::iced::wgpu::Queue,
        _format: cosmic::iced::wgpu::TextureFormat,
    ) -> Self {
        let shader = load_shader(device);
        let bind_group_layout = build_bind_group_layout(device);
        let render_pipeline = build_render_pipeline(device, &shader, &bind_group_layout);

        let uniform_buf =
            device.create_buffer_init(&cosmic::iced::wgpu::util::BufferInitDescriptor {
                label: Some("exposure uniforms"),
                contents: bytemuck::bytes_of(&Uniforms::default()),
                usage: cosmic::iced::wgpu::BufferUsages::UNIFORM
                    | cosmic::iced::wgpu::BufferUsages::COPY_DST,
            });

        let sampler = device.create_sampler(&cosmic::iced::wgpu::SamplerDescriptor {
            address_mode_u: cosmic::iced::wgpu::AddressMode::ClampToEdge,
            address_mode_v: cosmic::iced::wgpu::AddressMode::ClampToEdge,
            mag_filter: cosmic::iced::wgpu::FilterMode::Linear,
            min_filter: cosmic::iced::wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Bind group and texture are created lazily in `prepare()` on the
        // first frame (they require the mono data to build the texture).

        Self {
            texture: None,
            uniform_buf,
            sampler,
            bind_group_layout,
            bind_group: None,
            render_pipeline: Some(render_pipeline),
            initialized: false,
            current_image_id: None,
        }
    }

    fn trim(&mut self) {
        // Nothing to trim — the texture lives for the image's lifetime.
    }
}

/// Load the WGSL source from disk and create a shader module.
fn load_shader(device: &cosmic::iced::wgpu::Device) -> cosmic::iced::wgpu::ShaderModule {
    device.create_shader_module(cosmic::iced::wgpu::ShaderModuleDescriptor {
        label: Some("exposure shader"),
        source: cosmic::iced::wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
            "shader/exposure.wgsl"
        ))),
    })
}

fn build_bind_group_layout(
    device: &cosmic::iced::wgpu::Device,
) -> cosmic::iced::wgpu::BindGroupLayout {
    device.create_bind_group_layout(&cosmic::iced::wgpu::BindGroupLayoutDescriptor {
        label: Some("exposure bgl"),
        entries: &[
            cosmic::iced::wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: cosmic::iced::wgpu::ShaderStages::FRAGMENT,
                ty: cosmic::iced::wgpu::BindingType::Texture {
                    sample_type: cosmic::iced::wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: cosmic::iced::wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            cosmic::iced::wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: cosmic::iced::wgpu::ShaderStages::FRAGMENT,
                ty: cosmic::iced::wgpu::BindingType::Sampler(
                    cosmic::iced::wgpu::SamplerBindingType::Filtering,
                ),
                count: None,
            },
            cosmic::iced::wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: cosmic::iced::wgpu::ShaderStages::VERTEX
                    | cosmic::iced::wgpu::ShaderStages::FRAGMENT,
                ty: cosmic::iced::wgpu::BindingType::Buffer {
                    ty: cosmic::iced::wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    })
}

fn build_render_pipeline(
    device: &cosmic::iced::wgpu::Device,
    shader: &cosmic::iced::wgpu::ShaderModule,
    bind_group_layout: &cosmic::iced::wgpu::BindGroupLayout,
) -> cosmic::iced::wgpu::RenderPipeline {
    let pipeline_layout =
        device.create_pipeline_layout(&cosmic::iced::wgpu::PipelineLayoutDescriptor {
            label: Some("exposure pl"),
            bind_group_layouts: &[bind_group_layout],
            immediate_size: 0,
        });

    device.create_render_pipeline(&cosmic::iced::wgpu::RenderPipelineDescriptor {
        label: Some("exposure rp"),
        layout: Some(&pipeline_layout),
        vertex: cosmic::iced::wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: cosmic::iced::wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(cosmic::iced::wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            targets: &[Some(cosmic::iced::wgpu::ColorTargetState {
                format: cosmic::iced::wgpu::TextureFormat::Bgra8Unorm,
                // Alpha-blend so the WGSL's `alpha=0` letterbox bars let the
                // COSMIC panel background show through. Image pixels have
                // `alpha=1`, so they composite the same as `REPLACE` would.
                blend: Some(cosmic::iced::wgpu::BlendState::ALPHA_BLENDING),
                write_mask: cosmic::iced::wgpu::ColorWrites::ALL,
            })],
            compilation_options: cosmic::iced::wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: cosmic::iced::wgpu::PrimitiveState {
            topology: cosmic::iced::wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: cosmic::iced::wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

// ---------------------------------------------------------------------------
// Uniform buffer layout (must match the WGSL `Uniforms` struct)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Uniforms {
    exposure: f32,
    /// Texture width in pixels — drives the WGSL contained-fit math.
    tex_w: f32,
    /// Texture height in pixels.
    tex_h: f32,
    sc_x: f32,
    sc_y: f32,
    sc_w: f32,
    sc_h: f32,
    /// Detail-view zoom: 1.0 = contain fit, each +1 doubles scale.
    zoom: f32,
    /// Pan offset in physical pixels (logical points × scale factor).
    pan_x: f32,
    pan_y: f32,
    /// Live crop, in source pixels: the sub-rectangle origin (left/top margins
    /// removed) and its size (source − removed margins). All zero/no-op when
    /// `crop_w == tex_w` and `crop_l == 0`.
    crop_l: f32,
    crop_t: f32,
    crop_w: f32,
    crop_h: f32,
    /// Non-zero when the "view dimmed crop area" overlay is shown: the shader
    /// draws the full uncropped frame on top of the zoomed crop and dims
    /// everything outside the crop rectangle.
    show_mask: f32,
    /// Tone-curve ratio: contrast/rolloff/shadows power curves (pivoted at the
    /// image's measured mid-gray, white point, and shadow anchor) composed
    /// into a single `ratio·p^exp` pair. `(1.0, 1.0)` is the identity —
    /// untouched renders are unchanged.
    curve_ratio: f32,
    /// Tone-curve exponent; `exp = contrast · rolloff · shadows`.
    curve_exp: f32,
}

// SAFETY: Uniforms is repr(C) with all f32 fields.
unsafe impl bytemuck::Pod for Uniforms {}
unsafe impl bytemuck::Zeroable for Uniforms {}

// ---------------------------------------------------------------------------
// f32 → f16 (IEEE 754 binary16) bit conversion
// ---------------------------------------------------------------------------
//
// `R16Float` is filterable on WebGPU; `R32Float` is not. Half-float has
// 1 sign + 5 exponent + 10 mantissa — plenty of precision for our
// post-inversion linear-light tonal data (mostly 0..1.5, occasionally
// higher after a stop of exposure). The conversion runs once per decode
// (~16 MB f32 → 8 MB u16 at 2048²).
//
// Subnormal f32 inputs flush to zero and overflows flush to infinity;
// round-to-nearest-even is used for mantissa truncation. No half-float
// subnormals are produced (input range is large enough that the small
// loss of precision below ~6e-8 linear is inaudible).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn f32_to_half(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp_field = ((bits >> 23) & 0xff).cast_signed();
    let mant = bits & 0x007f_ffff;

    if exp_field == 0xff {
        return sign | if mant == 0 { 0x7c00 } else { 0x7e00 };
    }
    if exp_field == 0 {
        return sign;
    }

    let unbiased = exp_field - 127;

    if unbiased > 15 {
        return sign | 0x7c00;
    }
    if unbiased < -14 {
        return sign;
    }

    let half_exp = (unbiased + 15) as u32;
    let mut half_mant = mant >> 13;
    let round_part = mant & 0x1fff;

    // Round-to-nearest-even.
    if round_part > 0x1000 || (round_part == 0x1000 && (half_mant & 1) != 0) {
        half_mant += 1;
        if half_mant >= 0x400 {
            // Mantissa overflow → bump exponent (mantissa bits implicitly 0).
            let new_exp = half_exp + 1;
            if new_exp >= 31 {
                return sign | 0x7c00;
            }
            return sign | ((new_exp as u16) << 10);
        }
    }

    sign | ((half_exp as u16) << 10) | (half_mant as u16)
}

/// Convert a `Vec<f32>` of linear mono values into an `R16Float`-compatible
/// byte buffer for `queue.write_texture`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn mono_to_half_bytes(mono: &[f32]) -> Vec<u8> {
    let mut half = Vec::with_capacity(mono.len() * 2);
    for &v in mono {
        half.extend_from_slice(&f32_to_half(v).to_le_bytes());
    }
    half
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_uv_geometry_zero_crop_is_identity() {
        // Same source and texture dims → the margin mapping is a no-op.
        let (origin, size) = crop_uv_geometry(CropMargins::default(), 400, 300, 400, 300);
        assert_eq!(origin, (0.0, 0.0));
        assert_eq!(size, (400.0, 300.0));
    }

    #[test]
    fn crop_uv_geometry_offsets_origin_by_left_top_and_shrinks_size() {
        // Source == texture (native): margins map 1:1 onto the texture.
        let crop = CropMargins {
            top: 10,
            right: 5,
            bottom: 30,
            left: 15,
        };
        let (origin, size) = crop_uv_geometry(crop, 400, 300, 400, 300);
        assert_eq!(origin, (15.0, 10.0));
        assert_eq!(size, (380.0, 260.0));
    }

    #[test]
    fn crop_uv_geometry_scales_source_px_onto_the_downscaled_texture() {
        // A 6000x4000 source shown at a 0.1× 600x400 texture: a stored
        // source-pixel crop (100 left, 50 top) must remove 10 and 5 texture
        // pixels respectively so the live view and the thumbnail bake stay in
        // the same reference frame. Before the src-aware fix each axis was
        // treated as texture pixels, so the downscaled view truncated ~10× more
        // than the bake revealed.
        let crop = CropMargins {
            top: 50,
            right: 100,
            bottom: 150,
            left: 100,
        };
        let (origin, size) = crop_uv_geometry(crop, 600, 400, 6000, 4000);
        assert_eq!(origin, (10.0, 5.0));
        assert_eq!(size, (580.0, 380.0));
    }

    #[test]
    fn crop_uv_geometry_scales_a_portrait_crop_onto_the_rotated_texture() {
        // Rotate90 display source (4000x6000) downscaled to a 256x384 portrait
        // texture (~0.064×, the THUMB-long-edge ratio is coincidental here).
        let crop = CropMargins {
            top: 300,
            right: 0,
            bottom: 300,
            left: 0,
        };
        let (origin, size) = crop_uv_geometry(crop, 256, 384, 4000, 6000);
        assert_eq!(origin, (0.0, 19.2));
        assert_eq!(size, (256.0, 384.0 - 38.4));
    }

    #[test]
    fn new_restores_display_source_dims_from_the_long_edge() {
        // Landscape source: texture 600x400 (downscale of 6000x4000) → source
        // long edge 6000 lands on the width axis.
        let program = DetailProgram::new(
            vec![0.0; 600 * 400],
            600,
            400,
            0.0,
            CropMargins::default(),
            1,
            6000,
        );
        assert_eq!(program.source_dimensions(), (6000, 4000));

        // Portrait (rotated) source: texture 400x600 → long edge lands height.
        let program = DetailProgram::new(
            vec![0.0; 400 * 600],
            400,
            600,
            0.0,
            CropMargins::default(),
            1,
            6000,
        );
        assert_eq!(program.source_dimensions(), (4000, 6000));

        // Native decode (texture == source): the long edge restores exactly.
        let program = DetailProgram::new(
            vec![0.0; 400 * 600],
            400,
            600,
            0.0,
            CropMargins::default(),
            1,
            600,
        );
        assert_eq!(program.source_dimensions(), (400, 600));
    }

    #[test]
    fn f32_to_half_matches_canonical_values() {
        assert_eq!(f32_to_half(0.0), 0x0000, "+0");
        assert_eq!(f32_to_half(-0.0), 0x8000, "-0");
        assert_eq!(f32_to_half(1.0), 0x3c00, "1.0");
        assert_eq!(f32_to_half(-1.0), 0xbc00, "-1.0");
        assert_eq!(f32_to_half(0.5), 0x3800, "0.5");
        assert_eq!(f32_to_half(2.0), 0x4000, "2.0");
        assert_eq!(f32_to_half(65504.0), 0x7bff, "largest finite f16");
        assert_eq!(f32_to_half(65536.0), 0x7c00, "overflow → +∞");
        assert_eq!(f32_to_half(-65536.0), 0xfc00, "overflow → -∞");
        assert_eq!(f32_to_half(0.1), 0x2e66, "0.1");
        assert_eq!(f32_to_half(-0.1), 0xae66, "-0.1");
        assert_eq!(f32_to_half(0.000_488_281_25), 0x1000, "2^-11");
        assert_eq!(
            f32_to_half(0.000_061_035_156_25),
            0x0400,
            "smallest f16 normal ≈ 2^-14"
        );
        // Values smaller than the smallest f16 normal range flush to zero;
        // we don't produce half-float subnormals.
        assert_eq!(
            f32_to_half(0.000_030_517_578_125),
            0x0000,
            "below smallest normal flushes to 0 (no subnormal handling)"
        );
    }

    #[test]
    fn f32_to_half_underflow_flushes_to_zero() {
        // Anything smaller than the smallest f16 normal (≈ 6.1e-5) flushes
        // to zero; we do not produce half-float subnormals.
        assert_eq!(f32_to_half(1.0e-7), 0x0000, "tiny positive → +0");
        assert_eq!(f32_to_half(-1.0e-7), 0x8000, "tiny negative → -0");
    }

    #[test]
    fn f32_to_half_handles_infinity_and_nan() {
        assert_eq!(f32_to_half(f32::INFINITY), 0x7c00, "+∞");
        assert_eq!(f32_to_half(f32::NEG_INFINITY), 0xfc00, "-∞");
        assert_eq!(f32_to_half(f32::NAN) & 0x7c00, 0x7c00, "NaN keeps exp");
        assert_eq!(f32_to_half(f32::NAN) & 0x0200, 0x0200, "NaN keeps mant bit");
    }

    #[test]
    fn mono_to_half_bytes_lays_out_u16_le() {
        // For 1.0 we expect two bytes: 0x00 0x3C (little-endian u16 0x3C00).
        let bytes = mono_to_half_bytes(&[1.0]);
        assert_eq!(bytes, [0x00, 0x3c]);
    }

    #[test]
    fn curve_remap_is_identity_at_defaults() {
        let (ratio, exponent) = curve_remap(1.0, 1.0, 1.0, 0.2, 0.4, 0.92);
        assert!((ratio - 1.0).abs() < 1e-6);
        assert!((exponent - 1.0).abs() < 1e-6);
    }

    #[test]
    fn contrast_pivots_around_the_image_midgray() {
        let (shadow, mid, white) = (0.15_f32, 0.4_f32, 0.92_f32);
        for kc in [0.6_f32, 1.3] {
            let (ratio, exponent) = curve_remap(kc, 1.0, 1.0, shadow, mid, white);
            let t_mid = (ratio * mid.powf(exponent)).clamp(0.0, 1.0);
            assert!(
                (t_mid - mid).abs() < 1e-5,
                "contrast {kc}: T(mid) = {t_mid}"
            );
        }
    }

    #[test]
    fn rolloff_pivots_around_the_image_white_point() {
        let (shadow, mid, white) = (0.15_f32, 0.4_f32, 0.92_f32);
        for kr in [0.6_f32, 1.3] {
            let (ratio, exponent) = curve_remap(1.0, kr, 1.0, shadow, mid, white);
            let t_white = (ratio * white.powf(exponent)).clamp(0.0, 1.0);
            assert!(
                (t_white - white).abs() < 1e-5,
                "rolloff {kr}: T(white) = {t_white}"
            );
        }
    }

    #[test]
    fn shadows_pivots_around_the_image_shadow_anchor() {
        let (shadow, mid, white) = (0.15_f32, 0.4_f32, 0.92_f32);
        for ks in [0.6_f32, 1.3] {
            let (ratio, exponent) = curve_remap(1.0, 1.0, ks, shadow, mid, white);
            let t_shadow = (ratio * shadow.powf(exponent)).clamp(0.0, 1.0);
            assert!(
                (t_shadow - shadow).abs() < 1e-5,
                "shadows {ks}: T(shadow) = {t_shadow}"
            );
        }
    }

    #[test]
    fn curve_remap_composes_the_three_pivots_exactly() {
        // Applying the rolloff and shadows powers after the contrast power must
        // equal the single (ratio, exp) the shader applies — the composition is
        // exact for the underlying power functions. The shader applies one
        // final clamp (never an intermediate one), so compare raw then
        // both-clamped.
        let (shadow, mid, white) = (0.15_f32, 0.4_f32, 0.92_f32);
        let (kc, kr, ks) = (1.25_f32, 0.75_f32, 1.4_f32);
        let (ratio, exponent) = curve_remap(kc, kr, ks, shadow, mid, white);
        for p in [0.0_f32, 0.05, 0.15, 0.4, 0.6, 0.92, 1.0] {
            let c = mid.powf(1.0 - kc) * p.powf(kc);
            let r = white.powf(1.0 - kr) * c.powf(kr);
            let sequential = shadow.powf(1.0 - ks) * r.powf(ks);
            let composed = ratio * p.powf(exponent);
            assert!(
                (sequential - composed).abs() < 1e-4,
                "p {p}: sequential {sequential} vs composed {composed}"
            );
            assert!(
                (sequential.clamp(0.0, 1.0) - composed.clamp(0.0, 1.0)).abs() < 1e-4,
                "p {p}: clamped sequential vs clamped composed"
            );
        }
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn tone_anchors_find_shadow_mid_and_white_on_a_known_ramp() {
        let mono: Vec<f32> = (0..=2000).map(|v| v as f32 / 2000.0).collect();
        let (shadow, mid, white) = tone_anchors(&mono);
        assert!((shadow - 0.1).abs() < 0.02, "shadow {shadow}");
        assert!((mid - 0.5).abs() < 0.01, "median {mid}");
        assert!((white - 0.98).abs() < 0.02, "white {white}");
    }

    #[test]
    fn tone_anchors_guard_degenerate_black_frames() {
        let (shadow, mid, white) = tone_anchors(&[0.0; 256]);
        assert_eq!(shadow, MIN_ANCHOR);
        assert_eq!(mid, MIN_ANCHOR);
        assert_eq!(white, MIN_ANCHOR);
    }

    #[test]
    fn apply_curve_is_identity_at_defaults() {
        // The grid thumbnail bake calls `apply_curve` with the identity curve,
        // so untouched renders must be byte-identical (no phantom tone shift).
        let mut values = vec![0.0_f32, 0.13, 0.5, 0.84, 1.0];
        let original = values.clone();
        apply_curve(&mut values, 1.0, 1.0, 1.0, 0.15, 0.45, 0.92);
        assert!(
            values
                .iter()
                .zip(&original)
                .all(|(a, b)| (a - b).abs() < 1e-6),
            "identity curve changed values: {values:?}"
        );
    }

    #[test]
    fn apply_curve_matches_the_wgsl_expression() {
        // `apply_curve` is the CPU twin of the WGSL's
        // `clamp(curve_ratio * pow(p, curve_exp), 0, 1)` — re-derive the same
        // expression independently and confirm they agree on a range of inputs.
        let (shadow, mid, white) = (0.18_f32, 0.42_f32, 0.93_f32);
        let (contrast, rolloff, shadows) = (1.25_f32, 0.7_f32, 1.3_f32);
        let (ratio, exponent) = curve_remap(contrast, rolloff, shadows, shadow, mid, white);

        for p in [0.0_f32, 0.05, 0.18, 0.25, 0.42, 0.7, 0.93, 1.0, 3.0] {
            let expected = (ratio * p.powf(exponent)).clamp(0.0, 1.0);
            let mut v = [p];
            apply_curve(&mut v, contrast, rolloff, shadows, shadow, mid, white);
            assert!((v[0] - expected).abs() < 1e-6, "p {p}: {v:?} vs {expected}");
        }
    }

    #[test]
    fn wgsl_source_parses_and_validates() {
        // The shader is compiled by wgpu at first launch, so a parse error
        // would only surface at runtime. Validate the source we embed every
        // test run instead.
        let source = include_str!("shader/exposure.wgsl");
        let module = naga::front::wgsl::parse_str(source)
            .unwrap_or_else(|err| panic!("WGSL failed to parse: {err}"));

        let stages: Vec<_> = module.entry_points.iter().map(|ep| ep.stage).collect();
        assert!(stages.contains(&naga::ShaderStage::Vertex));
        assert!(stages.contains(&naga::ShaderStage::Fragment));
    }
}
