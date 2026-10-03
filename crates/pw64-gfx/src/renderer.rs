//! wgpu backend: draws a [`Frame`] into any color target at any size.
//!
//! - Resolution independence: vertices are in N64 screen space (320×240)
//!   and mapped to the target by a per-draw screen transform, so the image
//!   is rendered natively at the target resolution.
//! - Aspect: the 4:3 image is pillar/letterboxed; with
//!   [`RenderOptions::widescreen`] the output area is wider and each draw is
//!   placed by its [`Wide`] class: world geometry extends past the 4:3 edges
//!   (Hor+), full-width fills stretch, HUD/2D stays centred 4:3.
//! - Depth is reversed-Z float (+ stencil for z-image draws; N64 z values
//!   are only reproduced for words written into the z image).
//! - Pipelines are cached per [`PipelineKey`], shader modules per
//!   [`ShaderKey`], textures per content key (with generated mipmaps).

use crate::frame::{
    DrawCall, DrawUniforms, Frame, N64_HEIGHT, N64_WIDTH, PipelineKey, ShaderKey, ShaderMode,
    Vertex, Wide,
};
use crate::rdp::FinalBlend;
use crate::shader;
use crate::texture::{SamplerKey, TileBinding, Wrap};
use pw64_formats::Image;
use std::collections::HashMap;
use wgpu::util::DeviceExt;

mod fb;
pub use fb::FbOp;

#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// MSAA sample count (1 or 4).
    pub msaa: u32,
    /// Output aspect; wider than 4:3 extends perspective 3D (Hor+) and
    /// stretches full-width fills, 2D stays in the centred 4:3 area.
    pub widescreen: Widescreen,
    /// Filtering of bilinear (`G_TF_BILERP`) tiles; point tiles are always
    /// point-sampled. Can change between frames (shaders are cached per
    /// filter).
    pub filter: TexFilter,
    /// Fill view (S17, the game's letterbox removed around world views):
    /// world draws spanning the main view vertically get the output area's
    /// full height as their scissor, full-view fills are stretched onto it,
    /// and anchored HUD moves to the true edges. Fill off renders exactly
    /// like before.
    pub fill_view: bool,
    /// OLED care: the flight HUD (`DrawCall::hud`) drifts and dims. The
    /// default (brightness 1, no drift) is output-bit-identical.
    pub oled: Oled,
}

/// OLED care options (see [`RenderOptions::oled`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oled {
    /// HUD color multiplier (1 = no dimming; alpha is kept).
    pub brightness: f32,
    /// HUD shift in N64 pixels (a slow circle, [`oled_drift`]).
    pub drift: [f32; 2],
}

impl Default for Oled {
    fn default() -> Self {
        Self {
            brightness: 1.0,
            drift: [0.0; 2],
        }
    }
}

/// The drift circle for [`Oled::drift`]: radius 2 N64 px, period 240 s
/// (slow enough to never read as motion, fast enough to spread the wear).
pub fn oled_drift(t_secs: f64) -> [f32; 2] {
    let a = t_secs * std::f64::consts::TAU / 240.0;
    [2.0 * a.cos() as f32, 2.0 * a.sin() as f32]
}

/// Texture filter for `G_TF_BILERP` tiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TexFilter {
    /// GPU bilinear (+ trilinear/anisotropic mipmaps when minifying).
    #[default]
    Bilinear,
    /// The RDP's 3-point triangle filter while magnifying, fading into the
    /// GPU mip filtering when minifying (shader.rs `filter3`).
    N64,
}

/// Output area shape (fitted into the target, letter/pillarboxed).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Widescreen {
    /// 4:3.
    Off,
    /// The target's own aspect (at least 4:3).
    Fill,
    /// A fixed aspect ratio (w/h; values ≤ 4:3 mean 4:3).
    Aspect(f32),
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            msaa: 4,
            widescreen: Widescreen::Off,
            filter: TexFilter::Bilinear,
            fill_view: false,
            oled: Oled::default(),
        }
    }
}

const UNIFORM_STRIDE: u64 = 256;

/// Depth + stencil format: stencil marks the pixels a [`ShaderMode::DepthImage`]
/// draw passed the depth test on. Float depth when the device has it (see
/// [`crate::device_descriptor`]).
fn depth_format(device: &wgpu::Device) -> wgpu::TextureFormat {
    if device
        .features()
        .contains(wgpu::Features::DEPTH32FLOAT_STENCIL8)
    {
        wgpu::TextureFormat::Depth32FloatStencil8
    } else {
        wgpu::TextureFormat::Depth24PlusStencil8
    }
}

/// Which GPU pass of a draw a pipeline is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Step {
    /// Everything but z-image draws.
    Draw,
    /// Z-image draw, step 1: depth-test the triangle (no writes), stencil = 1
    /// where it passes.
    ZMark,
    /// Z-image draw, step 2: where stencil = 1, write the blender color as
    /// the new depth (`fs_depth`) and reset the stencil.
    ZWrite,
}

type BindKey = (Option<(u64, SamplerKey)>, Option<(u64, SamplerKey)>);

struct Targets {
    size: (u32, u32),
    depth: wgpu::TextureView,
    msaa: Option<wgpu::TextureView>,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    format: wgpu::TextureFormat,
    pub options: RenderOptions,
    uniform_bgl: wgpu::BindGroupLayout,
    texture_bgl: wgpu::BindGroupLayout,
    layout: wgpu::PipelineLayout,
    shaders: HashMap<(ShaderKey, TexFilter), wgpu::ShaderModule>,
    pipelines: HashMap<(PipelineKey, Step, TexFilter), wgpu::RenderPipeline>,
    depth_format: wgpu::TextureFormat,
    textures: HashMap<u64, wgpu::TextureView>,
    samplers: HashMap<SamplerKey, wgpu::Sampler>,
    bind_groups: HashMap<BindKey, wgpu::BindGroup>,
    white: wgpu::TextureView,
    /// `blit` (PW64_SCALE): fullscreen-triangle pipelines per (source
    /// filter, box downscale), samplers per filter. See `blit`.
    blit_layout: wgpu::BindGroupLayout,
    blit_shader: wgpu::ShaderModule,
    blit_pipelines: HashMap<(wgpu::FilterMode, bool), wgpu::RenderPipeline>,
    blit_samplers: HashMap<wgpu::FilterMode, wgpu::Sampler>,
    targets: Option<Targets>,
    vbuf: Option<(wgpu::Buffer, u64)>,
    ubuf: Option<(wgpu::Buffer, u64, wgpu::BindGroup)>,
    /// Persistent N64 framebuffer targets (`fb.rs`).
    fbs: fb::FbStore,
}

/// Gives fully transparent texels the color of their opaque neighbours, so
/// bilinear filtering and mipmaps don't bleed their (often white or black)
/// RGB into visible edges. Opaque texels are unchanged.
fn dilate_transparent(w: u32, h: u32, px: &mut [u8]) {
    // One ring is enough for bilinear filtering (alpha stays 0).
    let src = px.to_vec();
    let (w, h) = (w as i64, h as i64);
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            if src[i + 3] != 0 {
                continue;
            }
            let (mut sum, mut n) = ([0u32; 3], 0);
            for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                let (nx, ny) = ((x + dx).rem_euclid(w), (y + dy).rem_euclid(h));
                let j = ((ny * w + nx) * 4) as usize;
                if src[j + 3] != 0 {
                    (0..3).for_each(|c| sum[c] += src[j + c] as u32);
                    n += 1;
                }
            }
            if n > 0 {
                (0..3).for_each(|c| px[i + c] = (sum[c] / n) as u8);
            }
        }
    }
}

/// `DrawUniforms::filt` for `TexFilter::N64`: 3-point for bilinear tiles,
/// with their wrap codes (S + 3 × T; clamp 0, repeat 1, mirror 2).
fn filter_uniform(tex: &[Option<TileBinding>; 2]) -> [f32; 4] {
    let code = |w: Wrap| match w {
        Wrap::Clamp => 0.0,
        Wrap::Repeat => 1.0,
        Wrap::Mirror => 2.0,
    };
    let mut f = [0.0; 4];
    for (i, b) in tex.iter().enumerate() {
        if let Some(b) = b.filter(|b| b.sampler.linear) {
            f[i] = 1.0;
            f[2 + i] = code(b.sampler.wrap[0]) + 3.0 * code(b.sampler.wrap[1]);
        }
    }
    f
}

fn mip_chain(img: &Image) -> Vec<(u32, u32, Vec<u8>)> {
    box_mips(img.width, img.height, dilated(img))
}

/// `img`'s texels as uploaded: alpha-0 RGB dilated (see `mip_chain`).
fn dilated(img: &Image) -> Vec<u8> {
    let mut base = img.rgba.clone();
    let px = base.as_chunks::<4>().0;
    // I textures (r = g = b = a) are skipped: their alpha-0 texels are
    // black *color* that combiners use (the UVEN sky ring lerps shade → white
    // by TEXEL0), and a clamped T edge stretches that row over a whole band —
    // dilated, it showed as pale streaks across the flight sky.
    let intensity = px
        .iter()
        .all(|p| p[0] == p[1] && p[1] == p[2] && p[2] == p[3]);
    if !intensity && px.iter().any(|p| p[3] == 0) && px.iter().any(|p| p[3] != 0) {
        dilate_transparent(img.width, img.height, &mut base);
    }
    base
}

/// Box-filtered mip chain down to 1×1 from level 0 `base` (`w`×`h` RGBA8).
fn box_mips(w: u32, h: u32, base: Vec<u8>) -> Vec<(u32, u32, Vec<u8>)> {
    let mut out = vec![(w, h, base)];
    while let Some((w, h, px)) = out.last() {
        if *w == 1 && *h == 1 {
            break;
        }
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        let mut next = vec![0u8; (nw * nh * 4) as usize];
        for y in 0..nh {
            for x in 0..nw {
                for c in 0..4 {
                    let mut sum = 0u32;
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        let sx = (x * 2 + dx).min(w - 1);
                        let sy = (y * 2 + dy).min(h - 1);
                        sum += px[((sy * w + sx) * 4 + c) as usize] as u32;
                    }
                    next[((y * nw + x) * 4 + c) as usize] = (sum / 4) as u8;
                }
            }
        }
        out.push((nw, nh, next));
    }
    out
}

impl Renderer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        options: RenderOptions,
    ) -> Self {
        let uniform_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pw64 draw uniforms"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(size_of::<DrawUniforms>() as u64),
                },
                count: None,
            }],
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let samp_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let texture_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pw64 textures"),
            entries: &[tex_entry(0), samp_entry(1), tex_entry(2), samp_entry(3)],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pw64"),
            bind_group_layouts: &[&uniform_bgl, &texture_bgl],
            push_constant_ranges: &[],
        });
        let white = device
            .create_texture_with_data(
                queue,
                &wgpu::TextureDescriptor {
                    label: Some("pw64 white"),
                    size: wgpu::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &[255; 4],
            )
            .create_view(&Default::default());
        Self {
            device: device.clone(),
            queue: queue.clone(),
            format,
            options,
            uniform_bgl,
            texture_bgl,
            layout,
            shaders: HashMap::new(),
            pipelines: HashMap::new(),
            depth_format: depth_format(device),
            textures: HashMap::new(),
            samplers: HashMap::new(),
            bind_groups: HashMap::new(),
            white,
            blit_layout: device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("pw64 blit"),
                entries: &[tex_entry(0), samp_entry(1)],
            }),
            blit_shader: device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("pw64 blit"),
                source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
            }),
            blit_pipelines: HashMap::new(),
            blit_samplers: HashMap::new(),
            targets: None,
            vbuf: None,
            ubuf: None,
            fbs: fb::FbStore::default(),
        }
    }

    /// Number of cached pipelines / shaders / textures (stats).
    pub fn cache_stats(&self) -> (usize, usize, usize) {
        (
            self.pipelines.len(),
            self.shaders.len(),
            self.textures.len(),
        )
    }

    /// GPU steps of one draw.
    fn steps(key: &PipelineKey) -> &'static [Step] {
        if key.shader.mode == ShaderMode::DepthImage {
            &[Step::ZMark, Step::ZWrite]
        } else {
            &[Step::Draw]
        }
    }

    fn pipeline(&mut self, key: &PipelineKey, step: Step) {
        let filter = self.options.filter;
        if !self.pipelines.contains_key(&(*key, step, filter)) {
            let module = self.shaders.entry((key.shader, filter)).or_insert_with(|| {
                let src = shader::wgsl(&key.shader, filter == TexFilter::N64);
                self.device
                    .create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some("pw64 combiner"),
                        source: wgpu::ShaderSource::Wgsl(src.into()),
                    })
            });
            let s = &key.shader;
            let no_color = matches!(s.mode, ShaderMode::DepthClear | ShaderMode::DepthImage)
                || s.blend.kind == FinalBlend::KeepDst;
            let blend = (s.mode == ShaderMode::Normal && s.blend.kind == FinalBlend::Alpha)
                .then_some(wgpu::BlendState::ALPHA_BLENDING);
            let d = key.depth;
            let stencil_face = |compare, pass_op| wgpu::StencilFaceState {
                compare,
                fail_op: wgpu::StencilOperation::Keep,
                depth_fail_op: wgpu::StencilOperation::Keep,
                pass_op,
            };
            let stencil = |face: wgpu::StencilFaceState| wgpu::StencilState {
                front: face,
                back: face,
                read_mask: 0xFF,
                write_mask: 0xFF,
            };
            let (depth_write, depth_compare, stencil) = match step {
                Step::Draw => (
                    d.write,
                    if d.test {
                        wgpu::CompareFunction::GreaterEqual
                    } else {
                        wgpu::CompareFunction::Always
                    },
                    wgpu::StencilState::default(),
                ),
                Step::ZMark => (
                    false,
                    if d.test {
                        wgpu::CompareFunction::GreaterEqual
                    } else {
                        wgpu::CompareFunction::Always
                    },
                    stencil(stencil_face(
                        wgpu::CompareFunction::Always,
                        wgpu::StencilOperation::Replace,
                    )),
                ),
                Step::ZWrite => (
                    true,
                    wgpu::CompareFunction::Always,
                    stencil(stencil_face(
                        wgpu::CompareFunction::Equal,
                        wgpu::StencilOperation::Zero,
                    )),
                ),
            };
            let pipeline = self
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("pw64"),
                    layout: Some(&self.layout),
                    vertex: wgpu::VertexState {
                        module,
                        entry_point: Some("vs_main"),
                        compilation_options: Default::default(),
                        buffers: &[wgpu::VertexBufferLayout {
                            array_stride: size_of::<Vertex>() as u64,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &wgpu::vertex_attr_array![
                                0 => Float32x4, 1 => Float32x4, 2 => Float32x2, 3 => Float32x4
                            ],
                        }],
                    },
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        front_face: wgpu::FrontFace::Ccw,
                        cull_mode: match key.cull {
                            1 => Some(wgpu::Face::Front),
                            2 => Some(wgpu::Face::Back),
                            _ => None,
                        },
                        ..Default::default()
                    },
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: self.depth_format,
                        depth_write_enabled: depth_write,
                        depth_compare,
                        stencil,
                        bias: if d.decal && step != Step::ZWrite {
                            wgpu::DepthBiasState {
                                constant: 8,
                                slope_scale: 1.0,
                                clamp: 0.0,
                            }
                        } else {
                            Default::default()
                        },
                    }),
                    multisample: wgpu::MultisampleState {
                        count: self.options.msaa,
                        ..Default::default()
                    },
                    fragment: Some(wgpu::FragmentState {
                        module,
                        entry_point: Some(if step == Step::ZWrite {
                            "fs_depth"
                        } else {
                            "fs_main"
                        }),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: self.format,
                            blend,
                            write_mask: if no_color {
                                wgpu::ColorWrites::empty()
                            } else {
                                wgpu::ColorWrites::ALL
                            },
                        })],
                    }),
                    multiview: None,
                    cache: None,
                });
            self.pipelines.insert((*key, step, filter), pipeline);
        }
    }

    fn upload_texture(&mut self, key: u64, img: &Image) {
        let mut mips = mip_chain(img);
        // Texture-pack replacements can be any size: drop the levels past the
        // device limit (creating them is a validation panic). The chain ends
        // at 1×1, so some level always fits.
        let max = self.device.limits().max_texture_dimension_2d;
        let fits = mips.iter().position(|&(w, h, _)| w <= max && h <= max);
        mips.drain(..fits.unwrap_or(0));
        self.upload_levels(key, &mips);
    }

    /// A `LodMode::Chain` texture: the N64 tiles as its mip levels (the
    /// interpreter checked they halve). A base past the device limit (pack
    /// replacement) falls back to level 0's box mips.
    fn upload_chain(&mut self, key: u64, levels: &[&Image]) {
        let max = self.device.limits().max_texture_dimension_2d;
        if levels[0].width > max || levels[0].height > max {
            self.upload_texture(key, levels[0]);
            return;
        }
        let mips: Vec<_> = levels
            .iter()
            .map(|m| (m.width, m.height, dilated(m)))
            .collect();
        self.upload_levels(key, &mips);
    }

    fn upload_levels(&mut self, key: u64, mips: &[(u32, u32, Vec<u8>)]) {
        let (width, height, _) = mips[0];
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pw64 tile"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: mips.len() as u32,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (level, (w, h, px)) in mips.iter().enumerate() {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: level as u32,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                px,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(*h),
                },
                wgpu::Extent3d {
                    width: *w,
                    height: *h,
                    depth_or_array_layers: 1,
                },
            );
        }
        self.textures
            .insert(key, tex.create_view(&Default::default()));
    }

    fn sampler(&mut self, key: SamplerKey) -> &wgpu::Sampler {
        self.samplers.entry(key).or_insert_with(|| {
            let mode = |w: Wrap| match w {
                Wrap::Clamp => wgpu::AddressMode::ClampToEdge,
                Wrap::Repeat => wgpu::AddressMode::Repeat,
                Wrap::Mirror => wgpu::AddressMode::MirrorRepeat,
            };
            let f = if key.linear {
                wgpu::FilterMode::Linear
            } else {
                wgpu::FilterMode::Nearest
            };
            self.device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("pw64"),
                address_mode_u: mode(key.wrap[0]),
                address_mode_v: mode(key.wrap[1]),
                mag_filter: f,
                min_filter: f,
                mipmap_filter: f,
                anisotropy_clamp: if key.linear { 8 } else { 1 },
                ..Default::default()
            })
        })
    }

    fn bind_group(&mut self, tex: &[Option<TileBinding>; 2]) {
        let key: BindKey = (
            tex[0].map(|b| (b.key, b.sampler)),
            tex[1].map(|b| (b.key, b.sampler)),
        );
        if self.bind_groups.contains_key(&key) {
            return;
        }
        let default_sampler = SamplerKey {
            wrap: [Wrap::Clamp; 2],
            linear: false,
        };
        let s0 = self.sampler(key.0.map_or(default_sampler, |k| k.1)).clone();
        let s1 = self.sampler(key.1.map_or(default_sampler, |k| k.1)).clone();
        let view = |k: Option<(u64, SamplerKey)>| {
            k.and_then(|k| self.textures.get(&k.0))
                .unwrap_or(&self.white)
        };
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pw64 textures"),
            layout: &self.texture_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view(key.0)),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&s0),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(view(key.1)),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&s1),
                },
            ],
        });
        self.bind_groups.insert(key, bg);
    }

    fn ensure_targets(&mut self, size: (u32, u32)) {
        if self.targets.as_ref().is_some_and(|t| t.size == size) {
            return;
        }
        let tex = |format, samples, label| {
            self.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: size.0,
                        height: size.1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: samples,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        let n = self.options.msaa;
        self.targets = Some(Targets {
            size,
            depth: tex(self.depth_format, n, "pw64 depth"),
            msaa: (n > 1).then(|| tex(self.format, n, "pw64 msaa color")),
        });
    }

    /// Output aspect for a target of `size` (4:3 when widescreen is off).
    fn aspect(widescreen: Widescreen, size: (u32, u32)) -> f32 {
        let a = match widescreen {
            Widescreen::Off => 0.0,
            Widescreen::Fill => size.0 as f32 / size.1.max(1) as f32,
            Widescreen::Aspect(a) => a,
        };
        a.max(4.0 / 3.0)
    }

    /// Maps N64 screen space into the target: returns the output area
    /// (aspect `aspect`, fitted and centred) and the 4:3 area centred in it
    /// (same height), each as (x0, y0, w, h) in target pixels.
    fn region(size: (u32, u32), aspect: f32) -> ([f32; 4], [f32; 4]) {
        let (w, h) = (size.0 as f32, size.1 as f32);
        let outer = if w / h > aspect {
            let ow = h * aspect;
            [(w - ow) / 2.0, 0.0, ow, h]
        } else {
            let oh = w / aspect;
            [0.0, (h - oh) / 2.0, w, oh]
        };
        let rw = outer[3] * 4.0 / 3.0;
        let inner = [
            outer[0] + (outer[2] - rw) / 2.0,
            outer[1],
            rw.min(outer[2]),
            outer[3],
        ];
        (outer, inner)
    }

    /// `placement`'s main-view gate: wider than 4:3 (Hor+) or a fill view,
    /// and the draw's scissor spans the main view horizontally.
    fn main_view(fill: bool, aspect: f32, d: &DrawCall) -> bool {
        (aspect > 4.0 / 3.0 + 1e-4 || fill)
            && d.scissor[0] <= Wide::MAIN_VIEW[0]
            && d.scissor[2] >= Wide::MAIN_VIEW[1]
    }

    /// Rows span the main view vertically (fill view only).
    fn spans_y(fill: bool, rows: [f32; 2]) -> bool {
        fill && rows[0] <= Wide::MAIN_VIEW_Y[0] && rows[1] >= Wide::MAIN_VIEW_Y[1]
    }

    /// Whether `placement` gives this draw the output area's full height as
    /// its scissor (fill view): world geometry whose clip box spans the
    /// main view vertically. Marks a framebuffer "filled": nothing is left
    /// for the VI overscan to hide there (fb.rs `vi_border`).
    fn extend_y(&self, d: &DrawCall, size: (u32, u32)) -> bool {
        d.wide == Wide::Extend
            && Self::main_view(
                self.options.fill_view,
                Self::aspect(self.options.widescreen, size),
                d,
            )
            && Self::spans_y(self.options.fill_view, [d.scissor[1], d.scissor[3]])
    }

    /// Screen transform + target scissor for one draw. `hud_shift` is the
    /// OLED drift in N64 px, applied (rounded to whole target px) to hud
    /// `Wide::Fixed` draws only.
    fn placement(
        widescreen: Widescreen,
        fill: bool,
        d: &DrawCall,
        size: (u32, u32),
        hud_shift: [f32; 2],
    ) -> ([f32; 4], [u32; 4]) {
        let (tw, th) = (size.0 as f32, size.1 as f32);
        let aspect = Self::aspect(widescreen, size);
        let ([ox, _, ow, _], [x0, y0, rw, rh]) = Self::region(size, aspect);
        let main_view = Self::main_view(fill, aspect, d);
        let (extend, stretch) = match d.wide {
            Wide::Extend if main_view => (true, None),
            Wide::Stretch([a, b]) if main_view && b > a => (true, Some((a, b))),
            _ => (false, None),
        };
        // Fill view: rows spanning the main view reach the output area's
        // full height: world geometry (`Extend`, e.g. the sky's ratio-1
        // clip box ending at rows 3/227) is un-cropped, full-view fills
        // (`Stretch`: env clear, crash/cloud/replay fades) are stretched
        // onto it like `stretch` does horizontally.
        let extend_y =
            d.wide == Wide::Extend && extend && Self::spans_y(fill, [d.scissor[1], d.scissor[3]]);
        let stretch_y = match d.wide {
            Wide::Stretch(_) if stretch.is_some() && Self::spans_y(fill, d.stretch_y) => {
                Some(d.stretch_y)
            }
            _ => None,
        };
        // HUD anchored to an edge: 4:3 mapping moved by the side margin
        // (target px), scissor included — the HUD scissor (0..320) would
        // otherwise crop it at the 4:3 edge, and a scissor left of N64 x 0
        // is not expressible in the display list.
        // OLED drift: whole target px (text would shimmer at the fractional
        // rest), scaled from N64 px per axis.
        let shift = if d.hud && d.wide == Wide::Fixed {
            [
                (hud_shift[0] * rw / N64_WIDTH).round(),
                (hud_shift[1] * rh / N64_HEIGHT).round(),
            ]
        } else {
            [0.0; 2]
        };
        let x0 = match d.wide {
            Wide::Fixed => x0 + d.anchor.sign() * (ow - rw) / 2.0 + shift[0],
            _ => x0,
        };
        // HUD anchored top/bottom (fill view): 4:3 mapping moved by the old
        // gap to the letterbox edge, scissor included, as the H anchor
        // does with x0. `Fixed` only: `Extend`/`Stretch` draws are
        // un-cropped/stretched over the full height instead, so their y0
        // stays unshifted (also for their full-height scissor).
        let y0 = match d.wide {
            Wide::Fixed if fill => y0 + d.vanchor.rows() * rh / N64_HEIGHT,
            _ => y0,
        } + shift[1];
        let (kx, bx) = match stretch {
            // N64 x a..b → ox..ox+ow.
            Some((a, b)) => {
                let k = ow / (b - a);
                (2.0 * k / tw, 2.0 * (ox - a * k) / tw - 1.0)
            }
            None => (2.0 * rw / (N64_WIDTH * tw), 2.0 * x0 / tw - 1.0),
        };
        // N64 rows a..b → y0..y0+rh (the output area, unshifted y0).
        let (ky, by) = match stretch_y {
            Some([a, b]) => {
                let k = rh / (b - a);
                (-2.0 * k / th, 1.0 - 2.0 * (y0 - a * k) / th)
            }
            None => (-2.0 * rh / (N64_HEIGHT * th), 1.0 - 2.0 * y0 / th),
        };
        let screen = [kx, bx, ky, by];
        let sx = |x: f32| x0 + x * rw / N64_WIDTH;
        let sy = |y: f32| y0 + y * rh / N64_HEIGHT;
        let (mut l, mut r) = (sx(d.scissor[0]), sx(d.scissor[2]));
        if extend {
            (l, r) = (ox, ox + ow);
        }
        // Either vertical fill: the scissor is the output area's full
        // height (unshifted y0).
        let (t, b) = if extend_y || stretch_y.is_some() {
            (y0, y0 + rh)
        } else {
            (sy(d.scissor[1]), sy(d.scissor[3]))
        };
        let c = |v: f32, max: f32| v.round().clamp(0.0, max) as u32;
        let (l, r, t, b) = (c(l, tw), c(r, tw), c(t, th), c(b, th));
        (screen, [l, t, r.saturating_sub(l), b.saturating_sub(t)])
    }

    /// Renders `frame` into `target` (of this renderer's format and `size`),
    /// cleared to black first. (The native game draws onto persistent
    /// framebuffer targets instead: [`Renderer::fb_draw`].)
    pub fn render(&mut self, frame: &Frame, target: &wgpu::TextureView, size: (u32, u32)) {
        self.draw_frame(frame, target, size, false);
    }

    /// `render`, or with `persist` over `target`'s previous contents (a
    /// framebuffer target, `fb.rs`). Depth is cleared either way.
    fn draw_frame(
        &mut self,
        frame: &Frame,
        target: &wgpu::TextureView,
        size: (u32, u32),
        persist: bool,
    ) {
        self.ensure_targets(size);
        for (key, img) in &frame.textures {
            if !self.textures.contains_key(key) {
                self.upload_texture(*key, img);
            }
        }
        for (key, levels) in &frame.chains {
            if !self.textures.contains_key(key) {
                let imgs: Option<Vec<&Image>> = levels
                    .iter()
                    .map(|k| frame.textures.get(k).map(|m| &**m))
                    .collect();
                if let Some(imgs) = imgs {
                    self.upload_chain(*key, &imgs);
                }
            }
        }
        // Per-draw uniforms at 256-byte strides.
        let mut ubytes = vec![0u8; frame.draws.len().max(1) * UNIFORM_STRIDE as usize];
        let mut scissors = Vec::with_capacity(frame.draws.len());
        let n64 = self.options.filter == TexFilter::N64;
        for (i, d) in frame.draws.iter().enumerate() {
            let (screen, sc) = Self::placement(
                self.options.widescreen,
                self.options.fill_view,
                d,
                size,
                self.options.oled.drift,
            );
            let mut u = d.uniforms;
            u.screen = screen;
            // OLED care dim: 1 - brightness in `lod.y` for HUD draws (0 =
            // off; ×1.0 is exact, so default options stay bit-identical).
            u.lod[1] = if d.hud {
                1.0 - self.options.oled.brightness
            } else {
                0.0
            };
            if n64 {
                u.filt = filter_uniform(&d.textures);
            }
            let o = i * UNIFORM_STRIDE as usize;
            ubytes[o..o + size_of::<DrawUniforms>()].copy_from_slice(bytemuck::bytes_of(&u));
            scissors.push(sc);
            self.bind_group(&d.textures);
            for &step in Self::steps(&d.pipeline) {
                self.pipeline(&d.pipeline, step);
            }
        }
        let need = ubytes.len() as u64;
        if self.ubuf.as_ref().is_none_or(|u| u.1 < need) {
            let cap = need.next_power_of_two();
            let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pw64 uniforms"),
                size: cap,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("pw64 uniforms"),
                layout: &self.uniform_bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &buf,
                        offset: 0,
                        size: wgpu::BufferSize::new(size_of::<DrawUniforms>() as u64),
                    }),
                }],
            });
            self.ubuf = Some((buf, cap, bg));
        }
        self.queue
            .write_buffer(&self.ubuf.as_ref().unwrap().0, 0, &ubytes);
        let vbytes: &[u8] = bytemuck::cast_slice(&frame.vertices);
        let vneed = (vbytes.len() as u64).max(64).next_multiple_of(4);
        if self.vbuf.as_ref().is_none_or(|v| v.1 < vneed) {
            let cap = vneed.next_power_of_two();
            let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pw64 vertices"),
                size: cap,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.vbuf = Some((buf, cap));
        }
        if !vbytes.is_empty() {
            self.queue
                .write_buffer(&self.vbuf.as_ref().unwrap().0, 0, vbytes);
        }

        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("pw64"),
            });
        let color_load = if persist {
            self.prepare_persistent(&mut enc, target, size, frame, &scissors)
        } else {
            wgpu::LoadOp::Clear(wgpu::Color::BLACK)
        };
        let ub = self.ubuf.as_ref().unwrap();
        {
            let t = self.targets.as_ref().unwrap();
            let (view, resolve) = match &t.msaa {
                Some(m) => (m, Some(target)),
                None => (target, None),
            };
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pw64 frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: resolve,
                    ops: wgpu::Operations {
                        load: color_load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &t.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0),
                        store: wgpu::StoreOp::Discard,
                    }),
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_vertex_buffer(0, self.vbuf.as_ref().unwrap().0.slice(..));
            pass.set_stencil_reference(1);
            for (i, d) in frame.draws.iter().enumerate() {
                let [x, y, w, h] = scissors[i];
                if w == 0 || h == 0 || d.vertex_count == 0 {
                    continue;
                }
                let bkey: BindKey = (
                    d.textures[0].map(|b| (b.key, b.sampler)),
                    d.textures[1].map(|b| (b.key, b.sampler)),
                );
                pass.set_bind_group(0, &ub.2, &[(i as u64 * UNIFORM_STRIDE) as u32]);
                pass.set_bind_group(1, &self.bind_groups[&bkey], &[]);
                pass.set_scissor_rect(x, y, w, h);
                for &step in Self::steps(&d.pipeline) {
                    pass.set_pipeline(&self.pipelines[&(d.pipeline, step, self.options.filter)]);
                    pass.draw(d.first_vertex..d.first_vertex + d.vertex_count, 0..1);
                }
            }
        }
        self.queue.submit([enc.finish()]);
    }

    /// Renders offscreen and reads the result back as tightly packed RGBA8
    /// (requires a renderer created with `Rgba8Unorm`).
    pub fn render_to_rgba(&mut self, frame: &Frame, size: (u32, u32)) -> Vec<u8> {
        assert_eq!(self.format, wgpu::TextureFormat::Rgba8Unorm);
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pw64 screenshot"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        self.render(frame, &tex.create_view(&Default::default()), size);
        read_rgba(&self.device, &self.queue, &tex)
    }

    /// Blits `source` over `target` (both of this renderer's format, sample
    /// count 1) with a fullscreen triangle — the downsample half of the
    /// window's `PW64_SCALE` supersampling. `filter` picks the sampling
    /// (linear default, nearest via `PW64_SCALE_FILTER=nearest`). Pipelines
    /// are cached per (filter, box), samplers per filter; the bind group is
    /// per call (the views change on resize).
    ///
    /// When both axes shrink by more than 2× a single linear sample aliases
    /// (a 1-texel checker survives linear 1:1), so the box variant
    /// (`fs_box_main`) averages an 8×8 tap grid per destination pixel
    /// instead. An explicit `nearest` is honored as point sampling — the
    /// caller picked it on purpose. The ratio comes from the views' parent
    /// textures: `blit` is always called with full mip-0 views
    /// (`create_view(&Default::default())`), so the parent size is the view
    /// size.
    pub fn blit(
        &mut self,
        source: &wgpu::TextureView,
        target: &wgpu::TextureView,
        filter: wgpu::FilterMode,
    ) {
        self.blit_into(source, target, filter, None);
    }

    /// `blit`, optionally into the `viewport` rect (x, y, w, h) of `target`
    /// only (the rest is cleared to black): framebuffer present.
    fn blit_into(
        &mut self,
        source: &wgpu::TextureView,
        target: &wgpu::TextureView,
        filter: wgpu::FilterMode,
        viewport: Option<[u32; 4]>,
    ) {
        let (src, dst) = (source.texture(), target.texture());
        let [vx, vy, vw, vh] = viewport.unwrap_or([0, 0, dst.width(), dst.height()]);
        let ratio =
            (src.width() as f32 / vw.max(1) as f32).min(src.height() as f32 / vh.max(1) as f32);
        let boxed = filter == wgpu::FilterMode::Linear && ratio > 2.0;
        if !self.blit_pipelines.contains_key(&(filter, boxed)) {
            let layout = self
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("pw64 blit"),
                    bind_group_layouts: &[&self.blit_layout],
                    push_constant_ranges: &[],
                });
            let pipeline = self
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("pw64 blit"),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &self.blit_shader,
                        entry_point: Some("vs_main"),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        ..Default::default()
                    },
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &self.blit_shader,
                        entry_point: Some(if boxed { "fs_box_main" } else { "fs_main" }),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: self.format,
                            blend: None,
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    multiview: None,
                    cache: None,
                });
            self.blit_pipelines.insert((filter, boxed), pipeline);
        }
        // Level 0 only (no mip filter): the source has one mip level;
        // `fs_box_main` averages with taps instead.
        let sampler = self.blit_samplers.entry(filter).or_insert_with(|| {
            self.device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("pw64 blit"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: filter,
                min_filter: filter,
                ..Default::default()
            })
        });
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pw64 blit"),
            layout: &self.blit_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(source),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("pw64 blit"),
            });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pw64 blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.blit_pipelines[&(filter, boxed)]);
            pass.set_bind_group(0, &bind, &[]);
            pass.set_viewport(vx as f32, vy as f32, vw as f32, vh as f32, 0.0, 1.0);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([enc.finish()]);
    }
}

/// Reads `tex` (a 4-byte-per-texel format, sample count 1, `COPY_SRC`) back
/// as tightly packed rows.
fn read_rgba(device: &wgpu::Device, queue: &wgpu::Queue, tex: &wgpu::Texture) -> Vec<u8> {
    let (w, h) = (tex.width(), tex.height());
    let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pw64 readback"),
        size: (row * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        tex.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(h),
            },
        },
        tex.size(),
    );
    queue.submit([enc.finish()]);
    buf.slice(..)
        .map_async(wgpu::MapMode::Read, |r| r.expect("map readback"));
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll");
    let data = buf.slice(..).get_mapped_range();
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let o = (y * row) as usize;
        out.extend_from_slice(&data[o..o + (w * 4) as usize]);
    }
    out
}

/// Fullscreen triangle: covers NDC (-1,-1)..(1,1) with uv in [0,1] (y
/// flipped — texture row 0 is the top, NDC row -1 the bottom).
const BLIT_WGSL: &str = r#"
struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VOut {
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    return VOut(vec4<f32>(x * 2.0 - 1.0, y * 2.0 - 1.0, 0.0, 1.0), vec2<f32>(x, 1.0 - y));
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs_main(v: VOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, v.uv);
}

// Framebuffer reload (renderer/fb.rs): the source texel under this pixel
// (same size as the target), written to every sample of an MSAA attachment.
@fragment
fn fs_load(v: VOut) -> @location(0) vec4<f32> {
    return textureLoad(tex, vec2<i32>(v.pos.xy), 0);
}

// Framebuffer margin clear (renderer/fb.rs, scissored).
@fragment
fn fs_black() -> @location(0) vec4<f32> {
    return vec4<f32>(0.0, 0.0, 0.0, 1.0);
}

// Box-downscale variant for deep (>2×) shrinks, where one linear sample
// aliases. Each destination pixel averages an 8×8 tap grid spread over its
// source footprint: a proper box average up to 8×, and still a 64-tap
// average (no single-tap aliasing) beyond that. Chosen over a mip chain
// because the supersample source texture (built in pw64/src/window.rs) has
// one mip level and no COPY_SRC — mip generation would need an extra
// per-frame copy/blit pass there.
@fragment
fn fs_box_main(v: VOut) -> @location(0) vec4<f32> {
    // Footprint of one destination pixel in source uv space: uv is linear in
    // screen space (u only along x, v only along y), so its per-pixel change
    // is exactly (1/dst_w, 1/dst_h) — no size uniform needed. uv is the
    // pixel's centre, so the footprint stays inside [0,1].
    let uv_step = fwidth(v.uv);
    let base = v.uv - uv_step * 0.5;
    var sum = vec4<f32>(0.0);
    for (var j = 0u; j < 8u; j = j + 1u) {
        for (var i = 0u; i < 8u; i = i + 1u) {
            let uv = base + (vec2<f32>(f32(i), f32(j)) + 0.5) / 8.0 * uv_step;
            sum += textureSampleLevel(tex, samp, uv, 0.0);
        }
    }
    return sum / 64.0;
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Interpreter;
    use crate::frame::{Anchor, VAnchor};
    use crate::matrix;
    use crate::memory::VecMemory;
    use pw64_formats::gbi::geom;

    pub(super) fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::default();
        let adapter =
            crate::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .ok()?;
        crate::block_on(adapter.request_device(&crate::device_descriptor(&adapter))).ok()
    }

    /// A quad of shade-colored vertices (x0..x1 × full height at depth z),
    /// object units of 1/100 screen half-extent.
    fn quad(mem: &mut VecMemory, x0: i16, x1: i16, z: i16, rgba: [u8; 4]) -> u32 {
        let mut v = Vec::new();
        for (x, y) in [(x0, -100i16), (x1, -100), (x1, 100), (x0, 100)] {
            for c in [x, y, z, 0, 0, 0] {
                v.extend_from_slice(&c.to_be_bytes());
            }
            v.extend_from_slice(&rgba);
        }
        mem.push(&v, 8)
    }

    /// Dilation fills alpha-0 RGBA texels but leaves I textures alone (their
    /// alpha-0 texels are black color data: see `mip_chain`).
    #[test]
    fn dilation_skips_intensity_textures() {
        let rgba = |px: [[u8; 4]; 2]| Image {
            width: 2,
            height: 1,
            rgba: px.concat(),
        };
        let i = mip_chain(&rgba([[0; 4], [0x33; 4]]));
        assert_eq!(i[0].2, [[0; 4], [0x33; 4]].concat());
        let c = mip_chain(&rgba([[255, 255, 255, 0], [10, 20, 30, 255]]));
        assert_eq!(c[0].2[..4], [10, 20, 30, 0]);
    }

    /// `uvGfxStateDrawDL`'s trick: a draw with the color image pointed at
    /// the z image writes its color as depth where it passes the z test.
    /// Here black (z word 0 = nearest) over the left half hides the later
    /// green quad there, so the red one stays visible.
    #[test]
    fn color_into_z_image_writes_depth() {
        let Some((device, queue)) = device() else {
            eprintln!("no GPU adapter; skipped");
            return;
        };
        let mut mem = VecMemory::default();
        let mut scale = matrix::IDENTITY;
        (0..3).for_each(|i| scale[i][i] = 0.01);
        let ident = mem.push(&matrix::to_fixed(&matrix::IDENTITY), 8);
        let mv = mem.push(&matrix::to_fixed(&scale), 8);
        let red = quad(&mut mem, -100, 100, 0, [255, 0, 0, 255]);
        let black = quad(&mut mem, -100, 0, -50, [0, 0, 0, 255]);
        let green = quad(&mut mem, -100, 100, -80, [0, 255, 0, 255]);
        let (fb, zb) = (0x0010_0000u32, 0x0020_0000u32);
        let tris = [(0xBF00_0000, 0x0000_0A14), (0xBF00_0000, 0x0000_141E)];
        let mut cmds = vec![
            (0x0103_0040, ident),       // projection
            (0x0102_0040, mv),          // modelview
            (0xFF10_013F, zb),          // SETCIMG = z image
            (0xFE00_0000, zb),          // SETZIMG
            (0xBA00_1402, 0x0030_0000), // cycle type fill
            (0xF700_0000, 0xFFFC_FFFC), // fill = max z
            (0xF64F_C3BC, 0),           // FILLRECT 0,0..319,239
            (0xBA00_1402, 0),           // 1-cycle
            (0xFCFF_FFFF, 0xFFFE_793C), // G_CC_SHADE
            (0xB900_031D, 0x30),        // Z_CMP | Z_UPD, opaque
            (
                0xB700_0000,
                geom::G_ZBUFFER | geom::G_SHADE | geom::G_SHADING_SMOOTH,
            ),
            (0xFF10_013F, fb), // color image = fb
            (0x0430_0040, red),
        ];
        cmds.extend(tris);
        cmds.extend([(0xFF10_013F, zb), (0x0430_0040, black)]);
        cmds.extend(tris);
        cmds.extend([(0xFF10_013F, fb), (0x0430_0040, green)]);
        cmds.extend(tris);
        cmds.push((0xB800_0000, 0));
        let words: Vec<u32> = cmds.iter().flat_map(|&(a, b)| [a, b]).collect();
        let dl = mem.push_words(&words);
        let frame = Interpreter::new().run(&mem, dl);
        let modes: Vec<_> = frame.draws.iter().map(|d| d.pipeline.shader.mode).collect();
        assert_eq!(
            modes,
            [
                ShaderMode::DepthClear,
                ShaderMode::Normal,
                ShaderMode::DepthImage,
                ShaderMode::Normal
            ]
        );
        let mut r = Renderer::new(
            &device,
            &queue,
            wgpu::TextureFormat::Rgba8Unorm,
            RenderOptions {
                msaa: 1,
                fill_view: false,
                ..Default::default()
            },
        );
        let (w, h) = (64u32, 48u32);
        let img = r.render_to_rgba(&frame, (w, h));
        let px = |x: u32, y: u32| {
            let o = ((y * w + x) * 4) as usize;
            [img[o], img[o + 1], img[o + 2]]
        };
        assert_eq!(px(16, 24), [255, 0, 0], "left: z image draw hides green");
        assert_eq!(px(48, 24), [0, 255, 0], "right: green over red");
    }

    /// A left-red / right-green frame at half-screen each, via two Fill
    /// draws (the simplest renderer path).
    pub(super) fn red_green_frame() -> Frame {
        use bytemuck::Zeroable;
        let pipeline = PipelineKey {
            shader: ShaderKey {
                mode: ShaderMode::Fill,
                combiner: crate::combiner::CombinerKey::new(0, 0, false),
                blend: crate::rdp::BlendState {
                    pre: None,
                    last: crate::rdp::BlendCycle::from_othermode_l(0, 0),
                    kind: crate::rdp::FinalBlend::Opaque,
                    alpha_test: crate::rdp::AlphaTest::None,
                },
                alpha_cvg_sel: false,
            },
            depth: crate::rdp::DepthState {
                test: false,
                write: false,
                decal: false,
            },
            cull: 0,
        };
        let verts = |x0: f32, x1: f32| {
            let v = |x: f32, y: f32| Vertex {
                pos: [x, y, 1.0, 1.0],
                color: [0.0; 4],
                st: [0.0; 2],
                st_clamp: Vertex::NO_CLAMP,
            };
            vec![
                v(x0, 0.0),
                v(x1, 0.0),
                v(x1, 240.0),
                v(x0, 0.0),
                v(x1, 240.0),
                v(x0, 240.0),
            ]
        };
        let draw = |first: u32, fill: [f32; 4], sc: [f32; 4]| DrawCall {
            pipeline,
            first_vertex: first,
            vertex_count: 6,
            uniforms: DrawUniforms {
                fill,
                ..Zeroable::zeroed()
            },
            textures: [None, None],
            scissor: sc,
            is_3d: false,
            wide: Wide::Fixed,
            anchor: Anchor::Centre,
            vanchor: VAnchor::Middle,
            stretch_y: [0.0; 2],
            hud: false,
        };
        Frame {
            vertices: verts(0.0, 320.0),
            draws: vec![
                draw(0, [1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 160.0, 240.0]),
                draw(0, [0.0, 1.0, 0.0, 1.0], [160.0, 0.0, 320.0, 240.0]),
            ],
            textures: HashMap::new(),
            chains: HashMap::new(),
        }
    }

    /// Widescreen placement (no GPU): world draws in the main view extend
    /// to the output edges with the 4:3 mapping (Hor+), full-width fills
    /// stretch, HUD and small viewports stay in the centred 4:3 area, and
    /// widescreen off is the plain 4:3 pillarbox.
    #[test]
    fn widescreen_placement() {
        let base = red_green_frame().draws[0].clone();
        let draw = |wide: Wide, scissor: [f32; 4]| DrawCall {
            wide,
            scissor,
            ..base.clone()
        };
        let full = [0.0, 0.0, 320.0, 240.0];
        let size = (1600, 900);
        let ws = Widescreen::Aspect(16.0 / 9.0);
        // Target px of N64 x (w = 1).
        let px = |s: [f32; 4], x: f32| ((x * s[0] + s[1]) + 1.0) / 2.0 * 1600.0;
        // Target px of N64 y (th = 1): N64 row 0 is the top (NDC y' = 1).
        let py = |s: [f32; 4], y: f32| (1.0 - (y * s[2] + s[3])) / 2.0 * 900.0;
        let place = |w, d: &DrawCall| Renderer::placement(w, false, d, size, [0.0; 2]);
        let place_fill = |w, d: &DrawCall| Renderer::placement(w, true, d, size, [0.0; 2]);

        let (s, sc) = place(Widescreen::Off, &draw(Wide::Extend, full));
        assert_eq!(sc, [200, 0, 1200, 900], "off: 4:3 pillarbox");
        assert!((px(s, 0.0) - 200.0).abs() < 0.01 && (px(s, 320.0) - 1400.0).abs() < 0.01);

        let (fixed, sc) = place(ws, &draw(Wide::Fixed, full));
        assert_eq!(sc, [200, 0, 1200, 900], "HUD stays 4:3");
        let (s, sc) = place(ws, &draw(Wide::Extend, full));
        assert_eq!(sc, [0, 0, 1600, 900], "world view reaches the edges");
        assert_eq!(s, fixed, "Hor+: same mapping, only the crop widens");
        // The flight subscreen (x 10..310) is the main view too.
        let (_, sc) = place(ws, &draw(Wide::Extend, [10.0, 8.0, 310.0, 222.0]));
        assert_eq!(sc, [0, 30, 1600, 803]);
        // A small 3D viewport is not.
        let (_, sc) = place(ws, &draw(Wide::Extend, [60.0, 40.0, 260.0, 200.0]));
        assert_eq!(sc, [425, 150, 750, 600]);

        let (s, sc) = place(ws, &draw(Wide::Stretch([0.5, 319.5]), full));
        assert_eq!(sc, [0, 0, 1600, 900], "full-width fill stretches");
        assert!(px(s, 0.5).abs() < 0.01 && (px(s, 319.5) - 1600.0).abs() < 0.01);

        // Fill view (S17): world draws spanning the view vertically are
        // un-cropped at the top/bottom (the sky's ratio-1 clip box rows
        // 3..227 and the full-screen world both get the output height), and
        // full-view fills are stretched onto it.
        let (_, sc) = place_fill(ws, &draw(Wide::Extend, full));
        assert_eq!(sc, [0, 0, 1600, 900], "fill: full screen stays full");
        let (s, sc) = place_fill(ws, &draw(Wide::Extend, [5.0, 3.0, 315.0, 227.0]));
        assert_eq!(
            sc,
            [0, 0, 1600, 900],
            "fill: sky clip box reaches the edges"
        );
        assert_eq!(
            s,
            place(ws, &draw(Wide::Extend, [5.0, 3.0, 315.0, 227.0])).0,
            "fill: Hor+ again, only the crop widens (no rescale)"
        );
        // The env clear's vertices span rows 8..222 (`stretch_y`).
        let mut env_clear = draw(Wide::Stretch([10.0, 310.0]), [10.0, 8.0, 310.0, 222.0]);
        env_clear.stretch_y = [8.0, 222.0];
        let (s, sc) = place_fill(ws, &env_clear);
        assert_eq!(
            sc,
            [0, 0, 1600, 900],
            "fill: env clear stretched onto the view"
        );
        assert!(
            (px(s, 10.0) - 0.0).abs() < 0.01 && (px(s, 310.0) - 1600.0).abs() < 0.01,
            "fill: x still stretched over the output"
        );
        assert!(
            (py(s, 8.0) - 0.0).abs() < 0.01 && (py(s, 222.0) - 900.0).abs() < 0.01,
            "fill: rows 8..222 stretched onto the output height"
        );
        // A letterbox bar (rows 1..8) is not stretched: same as fill off.
        let mut bar = draw(Wide::Stretch([10.0, 310.0]), [10.0, 1.0, 310.0, 8.0]);
        bar.stretch_y = [1.0, 8.0];
        assert_eq!(place_fill(ws, &bar), place(ws, &bar), "fill: bar untouched");
        // A HUD-sized scissor doesn't span the view vertically.
        let hud = draw(Wide::Extend, [5.0, 100.0, 315.0, 140.0]);
        assert_eq!(
            place_fill(ws, &hud),
            place(ws, &hud),
            "fill: small view untouched"
        );
        // 4:3 + fill: no side bars, world view over the full 4:3 output.
        let size43 = (1600, 1200);
        let (s, sc) = Renderer::placement(
            Widescreen::Off,
            true,
            &draw(Wide::Extend, [5.0, 3.0, 315.0, 227.0]),
            size43,
            [0.0; 2],
        );
        assert_eq!(sc, [0, 0, 1600, 1200], "4:3 fill: output area");
        assert_eq!(
            s,
            Renderer::placement(
                Widescreen::Off,
                false,
                &draw(Wide::Extend, [5.0, 3.0, 315.0, 227.0]),
                size43,
                [0.0; 2]
            )
            .0,
            "4:3 fill: mapping unchanged"
        );
    }

    /// HUD edge anchors: a `Fixed` draw moves by the side margin (200 px at
    /// 16:9 in 1600×900) with its scissor; nothing moves with widescreen
    /// off, and world/fill draws ignore the anchor.
    #[test]
    fn widescreen_hud_anchor() {
        let base = red_green_frame().draws[0].clone();
        let draw = |wide: Wide, anchor: Anchor| DrawCall {
            wide,
            anchor,
            scissor: [0.0, 0.0, 320.0, 240.0],
            ..base.clone()
        };
        let size = (1600, 900);
        let ws = Widescreen::Aspect(16.0 / 9.0);
        let px = |s: [f32; 4], x: f32| ((x * s[0] + s[1]) + 1.0) / 2.0 * 1600.0;
        let place = |w, d: &DrawCall| Renderer::placement(w, false, d, size, [0.0; 2]);

        let (s, sc) = place(ws, &draw(Wide::Fixed, Anchor::Left));
        assert_eq!(sc, [0, 0, 1200, 900], "left: scissor moves to the edge");
        assert!(px(s, 0.0).abs() < 0.01, "N64 x 0 at the left edge");
        let (s, sc) = place(ws, &draw(Wide::Fixed, Anchor::Right));
        assert_eq!(sc, [400, 0, 1200, 900]);
        assert!(
            (px(s, 320.0) - 1600.0).abs() < 0.01,
            "N64 x 320 at the right edge"
        );
        let centre = place(ws, &draw(Wide::Fixed, Anchor::Centre));
        assert_eq!(centre.1, [200, 0, 1200, 900]);

        for a in [Anchor::Left, Anchor::Right] {
            assert_eq!(
                place(Widescreen::Off, &draw(Wide::Fixed, a)),
                place(Widescreen::Off, &draw(Wide::Fixed, Anchor::Centre)),
                "off: unchanged"
            );
            assert_eq!(
                place(ws, &draw(Wide::Extend, a)),
                place(ws, &draw(Wide::Extend, Anchor::Centre))
            );
        }
    }

    /// HUD vertical anchors (fill view): a `Fixed` draw moves by
    /// `VAnchor::rows()` × rh/240 with its scissor; only with fill on, and
    /// `Extend`/`Stretch` draws ignore the anchor.
    #[test]
    fn fill_hud_vanchor() {
        let base = red_green_frame().draws[0].clone();
        let draw = |wide: Wide, vanchor: VAnchor| DrawCall {
            wide,
            vanchor,
            scissor: [0.0, 0.0, 320.0, 240.0],
            ..base.clone()
        };
        let size = (1600, 900);
        let ws = Widescreen::Aspect(16.0 / 9.0);
        let py = |s: [f32; 4], y: f32| (1.0 - (y * s[2] + s[3])) / 2.0 * 900.0;
        let place = |fill: bool, d: &DrawCall| Renderer::placement(ws, fill, d, size, [0.0; 2]);

        // Top: 8 rows up = 30 target px (N64 row 0 lands above the edge).
        let (s, sc) = place(true, &draw(Wide::Fixed, VAnchor::Top));
        assert!(
            (py(s, 0.0) + 30.0).abs() < 0.01,
            "top: row 0 above the edge"
        );
        assert_eq!(sc, [200, 0, 1200, 870]);
        // Bottom: 12 rows down = 45 target px.
        let (s, sc) = place(true, &draw(Wide::Fixed, VAnchor::Bottom));
        assert!(
            (py(s, 240.0) - 945.0).abs() < 0.01,
            "bottom: row 240 below the edge"
        );
        assert_eq!(sc, [200, 45, 1200, 855]);
        let centre = place(true, &draw(Wide::Fixed, VAnchor::Middle));
        assert_eq!(centre.1, [200, 0, 1200, 900]);

        // The scissor moves with the draw (a timer-like HUD box, rows
        // 10..18 top-down).
        let mut hud = draw(Wide::Fixed, VAnchor::Top);
        hud.scissor = [0.0, 10.0, 320.0, 18.0];
        let (_, sc) = place(true, &hud);
        assert_eq!(sc, [200, 8, 1200, 30], "top: scissor moved by 30 px");

        for v in [VAnchor::Top, VAnchor::Bottom] {
            // Fill off: exactly today.
            assert_eq!(
                place(false, &draw(Wide::Fixed, v)),
                place(false, &draw(Wide::Fixed, VAnchor::Middle)),
                "fill off: unchanged"
            );
            // Extend/Stretch ignore the anchor (world geometry, fades).
            assert_eq!(
                place(true, &draw(Wide::Extend, v)),
                place(true, &draw(Wide::Extend, VAnchor::Middle))
            );
        }
    }

    /// OLED care drift: a hud `Fixed` draw's transform + scissor move by
    /// the drift rounded to whole target px; non-hud draws and world/
    /// stretched draws never move (the HUD is the only OLED-worn image).
    #[test]
    fn oled_hud_drift() {
        let base = red_green_frame().draws[0].clone();
        let draw = |hud: bool, wide: Wide| DrawCall {
            wide,
            hud,
            scissor: [0.0, 0.0, 320.0, 240.0],
            ..base.clone()
        };
        let size = (1600, 900);
        let ws = Widescreen::Aspect(16.0 / 9.0);
        let shift = [2.0, -3.0];
        let place = |d: &DrawCall| Renderer::placement(ws, false, d, size, shift);
        let px = |s: [f32; 4], x: f32| ((x * s[0] + s[1]) + 1.0) / 2.0 * 1600.0;

        let (s, sc) = place(&draw(true, Wide::Fixed));
        assert_eq!(
            sc,
            [208, 0, 1200, 889],
            "2 N64 px = 7.5 target px rounds to 8, -3 rows = -11.25 to -11"
        );
        assert!(
            (px(s, 0.0) - 208.0).abs() < 0.01,
            "the transform moves with the scissor"
        );
        assert_eq!(
            place(&draw(false, Wide::Fixed)).1,
            [200, 0, 1200, 900],
            "non-hud: unchanged"
        );
        assert_eq!(
            place(&draw(true, Wide::Extend)).1,
            place(&draw(false, Wide::Extend)).1,
            "world draws never shift"
        );
        assert_eq!(
            place(&draw(true, Wide::Stretch([0.0, 320.0]))).1,
            place(&draw(false, Wide::Stretch([0.0, 320.0]))).1,
            "stretched fills never shift"
        );
    }

    /// A texture wider than the device limit (texture packs take any size)
    /// uploads from its first fitting mip level instead of panicking.
    #[test]
    fn oversized_texture_uploads_a_fitting_mip() {
        let Some((device, queue)) = device() else {
            eprintln!("no GPU adapter; skipped");
            return;
        };
        let mut r = Renderer::new(
            &device,
            &queue,
            wgpu::TextureFormat::Rgba8Unorm,
            RenderOptions::default(),
        );
        let width = device.limits().max_texture_dimension_2d + 1;
        let img = Image {
            width,
            height: 1,
            rgba: vec![255; width as usize * 4],
        };
        r.upload_texture(7, &img);
        assert!(r.textures.contains_key(&7));
    }

    /// Blits a solid green texture: isolates `blit` from the frame render.
    #[test]
    fn blit_solid_color() {
        let Some((device, queue)) = device() else {
            eprintln!("no GPU adapter; skipped");
            return;
        };
        let mut r = Renderer::new(
            &device,
            &queue,
            wgpu::TextureFormat::Rgba8Unorm,
            RenderOptions {
                msaa: 1,
                fill_view: false,
                ..Default::default()
            },
        );
        let src = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blit test src"),
            size: wgpu::Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &src,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[0, 255, 0, 255].repeat(16),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(16),
                rows_per_image: Some(4),
            },
            wgpu::Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
        );
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blit test target"),
            size: wgpu::Extent3d {
                width: 8,
                height: 8,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        r.blit(
            &src.create_view(&Default::default()),
            &target.create_view(&Default::default()),
            wgpu::FilterMode::Linear,
        );
        let img = read_rgba(&device, &queue, &target);
        assert_eq!(img[(4 * 8 + 4) * 4..(4 * 8 + 4) * 4 + 4], [0, 255, 0, 255]);
    }

    /// The `PW64_SCALE` path end to end: render a red/green shade image at
    /// 2× into an offscreen texture, `blit` it down, and compare with a
    /// direct render (interior exact, every pixel within AA tolerance).
    #[test]
    fn blit_downsamples_a_scaled_render() {
        let Some((device, queue)) = device() else {
            eprintln!("no GPU adapter; skipped");
            return;
        };
        let frame = red_green_frame();
        let mut r = Renderer::new(
            &device,
            &queue,
            wgpu::TextureFormat::Rgba8Unorm,
            RenderOptions {
                msaa: 1,
                fill_view: false,
                ..Default::default()
            },
        );
        let (w, h) = (64u32, 48u32);
        let direct = r.render_to_rgba(&frame, (w, h));
        // 2× offscreen render + blit down to a copy-source target.
        let off = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blit test offscreen"),
            size: wgpu::Extent3d {
                width: w * 2,
                height: h * 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        r.render(
            &frame,
            &off.create_view(&Default::default()),
            (w * 2, h * 2),
        );
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blit test target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        r.blit(
            &off.create_view(&Default::default()),
            &target.create_view(&Default::default()),
            wgpu::FilterMode::Linear,
        );
        let img = read_rgba(&device, &queue, &target);
        let px = |img: &[u8], x: u32, y: u32| {
            let o = ((y * w + x) * 4) as usize;
            [img[o], img[o + 1], img[o + 2]]
        };
        assert_eq!(px(&img, 16, 24), [255, 0, 0], "left: red");
        assert_eq!(px(&img, 48, 24), [0, 255, 0], "right: green");
        for (rb, dr) in img
            .chunks_exact(w as usize * 4)
            .zip(direct.chunks_exact(w as usize * 4))
        {
            for (a, b) in rb.as_chunks::<4>().0.iter().zip(dr.as_chunks::<4>().0) {
                assert!(
                    a[..3]
                        .iter()
                        .zip(b[..3].iter())
                        .all(|(x, y)| x.abs_diff(*y) <= 8),
                    "blit diverges from the direct render at some pixel"
                );
            }
        }
    }

    /// Deep downscale (128→30, 4.27×) takes the box path: a 2-texel-block
    /// checker, which linear minification would reproduce as hard black /
    /// white aliasing bands (each dest pixel samples one point), must
    /// average to uniform mid-grey everywhere.
    #[test]
    fn blit_box_downscale_averages_a_checker() {
        let Some((device, queue)) = device() else {
            eprintln!("no GPU adapter; skipped");
            return;
        };
        let mut r = Renderer::new(
            &device,
            &queue,
            wgpu::TextureFormat::Rgba8Unorm,
            RenderOptions {
                msaa: 1,
                fill_view: false,
                ..Default::default()
            },
        );
        let (n, d) = (128u32, 30u32);
        let src = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blit box test src"),
            size: wgpu::Extent3d {
                width: n,
                height: n,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut px = Vec::with_capacity((n * n * 4) as usize);
        for y in 0..n {
            for x in 0..n {
                let v = if ((x / 2) ^ (y / 2)) & 1 == 0 {
                    255u8
                } else {
                    0u8
                };
                px.extend_from_slice(&[v, v, v, 255]);
            }
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &src,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &px,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(n * 4),
                rows_per_image: Some(n),
            },
            wgpu::Extent3d {
                width: n,
                height: n,
                depth_or_array_layers: 1,
            },
        );
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blit box test target"),
            size: wgpu::Extent3d {
                width: d,
                height: d,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        r.blit(
            &src.create_view(&Default::default()),
            &target.create_view(&Default::default()),
            wgpu::FilterMode::Linear,
        );
        let img = read_rgba(&device, &queue, &target);
        // 8 taps per axis over a 4.27-texel footprint hit each 2-texel
        // block within one tap of proportionally, so the average stays
        // within a few counts of mid-grey (linear would hit 0/255).
        for (i, chunk) in img.as_chunks::<4>().0.iter().enumerate() {
            assert!(
                chunk[..3].iter().all(|&v| v.abs_diff(128) <= 16),
                "dest pixel {} not box-averaged: {:?}",
                i,
                &chunk[..3]
            );
        }
    }
}
