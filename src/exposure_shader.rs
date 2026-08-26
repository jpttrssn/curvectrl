// SPDX-License-Identifier: MPL-2.0

//! GPU exposure shader — renders mono image data with live EV adjustment.
//!
//! The mono `Vec<f32>` is uploaded to the GPU once as an `R32Float` texture.
//! Exposure is applied as a shader uniform (`2^EV` in linear light) — zero
//! CPU re-encoding, zero new `Handle` per frame.

use cosmic::iced::core::{Length, Rectangle};
use cosmic::iced::widget::shader::{Pipeline, Primitive, Program, Shader, Viewport};
use cosmic::iced::wgpu::util::DeviceExt;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// GPU-backed detail view image with live exposure adjustment.
pub struct ExposureProgram {
    mono: Vec<f32>,
    width: u32,
    height: u32,
    /// Raw EV value in stops; converted to `2^EV` once on the GPU side per
    /// slider change so the WGSL shader sees a linear-light gain.
    exposure: f32,
    /// Monotonic id bumped by the app model on each new detail decode. Used
    /// to detect image changes and rebuild the GPU texture/bind group.
    image_id: u64,
}

impl ExposureProgram {
    /// Create a new program for the given mono image.
    ///
    /// `mono` is linear, inverted-positive pre-sRGB data (one `f32` per pixel,
    /// row-major, top-to-bottom). `exposure` is the raw EV value; the gain
    /// sent to the GPU is `2^EV`.
    pub fn new(mono: Vec<f32>, width: u32, height: u32, exposure: f32, image_id: u64) -> Self {
        Self { mono, width, height, exposure, image_id }
    }

    /// Update the exposure value (called on slider drag).
    pub fn set_exposure(&mut self, ev: f32) {
        self.exposure = ev;
    }

    /// Aspect ratio (width / height) of the texture — used by callers that
    /// need to constrain layout to match (e.g. `aspect_ratio_container`).
    #[allow(clippy::cast_precision_loss)]
    pub fn aspect(&self) -> f32 {
        self.width as f32 / self.height as f32
    }

    /// Wrap in a `Shader` widget sized to fill the parent.
    pub fn view<M>(&self) -> Shader<M, Self> {
        Shader::new(self.clone())
            .width(Length::Fill)
            .height(Length::Fill)
    }
}

impl Clone for ExposureProgram {
    fn clone(&self) -> Self {
        Self {
            mono: self.mono.clone(),
            width: self.width,
            height: self.height,
            exposure: self.exposure,
            image_id: self.image_id,
        }
    }
}

impl std::fmt::Debug for ExposureProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExposureProgram")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("exposure", &self.exposure)
            .field("mono_len", &self.mono.len())
            .field("image_id", &self.image_id)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// iced::widget::shader::Program implementation
// ---------------------------------------------------------------------------

impl<M> Program<M> for ExposureProgram {
    type State = ();
    type Primitive = ExposurePrimitive;

    fn draw(
        &self,
        _state: &(),
        _cursor: cosmic::iced::mouse::Cursor,
        _bounds: Rectangle,
    ) -> Self::Primitive {
        ExposurePrimitive {
            mono: self.mono.clone(),
            exposure: self.exposure,
            width: self.width,
            height: self.height,
            image_id: self.image_id,
        }
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
pub struct ExposurePrimitive {
    mono: Vec<f32>,
    exposure: f32,
    width: u32,
    height: u32,
    image_id: u64,
}

impl Primitive for ExposurePrimitive {
    type Pipeline = ExposurePipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &cosmic::iced::wgpu::Device,
        queue: &cosmic::iced::wgpu::Queue,
        bounds: &Rectangle,
        viewport: &Viewport,
    ) {
        // --- Rebuild texture + bind group if image identity changed ---
        // `iced` caches the `ExposurePipeline` across image selections because
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
            pipeline.bind_group = Some(
                device.create_bind_group(&cosmic::iced::wgpu::BindGroupDescriptor {
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
                }),
            );
            pipeline.current_image_id = Some(self.image_id);
            pipeline.initialized = true;
        }

        // --- Per-frame: update uniform buffer ---
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let sf = viewport.scale_factor() as f32;
        #[allow(clippy::cast_precision_loss)]
        let uniforms = Uniforms {
            // Convert raw EV (slider value) to linear-light gain once per
            // frame; the WGSL shader reads this as a direct multiplier.
            exposure: self.exposure.exp2(),
            vp_w: viewport.physical_width() as f32,
            vp_h: viewport.physical_height() as f32,
            sc_x: bounds.x * sf,
            sc_y: bounds.y * sf,
            sc_w: bounds.width * sf,
            sc_h: bounds.height * sf,
            _pad: 0.0,
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
        let (_tex_view, Some(pipeline_obj), Some(bind_group)) =
            (&pipeline.texture, &pipeline.render_pipeline, &pipeline.bind_group)
        else {
            return;
        };

        {
            let mut pass =
                encoder.begin_render_pass(&cosmic::iced::wgpu::RenderPassDescriptor {
                    label: Some("exposure render"),
                    color_attachments: &[Some(
                        cosmic::iced::wgpu::RenderPassColorAttachment {
                            view: target,
                            depth_slice: None,
                            resolve_target: None,
                            ops: cosmic::iced::wgpu::Operations {
                                load: cosmic::iced::wgpu::LoadOp::Load,
                                store: cosmic::iced::wgpu::StoreOp::Store,
                            },
                        },
                    )],
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

/// GPU resources shared across all `ExposurePrimitive` instances of the same
/// image.  Created lazily on the first `prepare()` call (needs the mono data
/// to build the texture).
pub struct ExposurePipeline {
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

impl std::fmt::Debug for ExposurePipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExposurePipeline")
            .field("initialized", &self.initialized)
            .finish_non_exhaustive()
    }
}

impl Pipeline for ExposurePipeline {
    fn new(
        device: &cosmic::iced::wgpu::Device,
        _queue: &cosmic::iced::wgpu::Queue,
        _format: cosmic::iced::wgpu::TextureFormat,
    ) -> Self {
        let shader = load_shader(device);
        let bind_group_layout = build_bind_group_layout(device);
        let render_pipeline = build_render_pipeline(device, &shader, &bind_group_layout);

        let uniform_buf = device.create_buffer_init(&cosmic::iced::wgpu::util::BufferInitDescriptor {
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
        source: cosmic::iced::wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(
            include_str!("shader/exposure.wgsl"),
        )),
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
                    sample_type: cosmic::iced::wgpu::TextureSampleType::Float {
                        filterable: true,
                    },
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
                blend: Some(cosmic::iced::wgpu::BlendState::REPLACE),
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
    vp_w: f32,
    vp_h: f32,
    sc_x: f32,
    sc_y: f32,
    sc_w: f32,
    sc_h: f32,
    _pad: f32,
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
        assert_eq!(f32_to_half(0.000_061_035_156_25), 0x0400, "smallest f16 normal ≈ 2^-14");
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
}

