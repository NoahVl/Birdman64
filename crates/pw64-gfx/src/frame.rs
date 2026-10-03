//! The interpreter's output: a flat vertex array plus draw batches, ready
//! for [`crate::Renderer`]. Nothing here depends on the output resolution.

use crate::combiner::CombinerKey;
use crate::rdp::{BlendState, DepthState};
use crate::texture::TileBinding;
use bytemuck::{Pod, Zeroable};
use pw64_formats::Image;
use std::collections::HashMap;
use std::sync::Arc;

/// N64 framebuffer size all screen coordinates refer to.
pub const N64_WIDTH: f32 = 320.0;
pub const N64_HEIGHT: f32 = 240.0;

/// One GPU vertex.
///
/// `pos` is in "N64 screen clip space": `pos.xy / pos.w` are N64 pixel
/// coordinates (0..320, 0..240, y down) and `pos.z / pos.w` is reversed
/// depth (1 = near, 0 = far). The renderer maps this to the target.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct Vertex {
    pub pos: [f32; 4],
    /// Shade RGBA 0..1 (alpha = fog factor when `G_FOG` is on).
    pub color: [f32; 4],
    /// Texel coordinates after `gSPTexture` scaling (before tile shift).
    pub st: [f32; 2],
    /// S/T clamp range (min s, min t, max s, max t), same space as `st`.
    /// Texture rectangles only sample their own texels on the RDP; at
    /// higher resolutions bilinear filtering would reach past the rect's
    /// edge texels, so they are clamped. [`Vertex::NO_CLAMP`] otherwise.
    pub st_clamp: [f32; 4],
}

impl Vertex {
    pub const NO_CLAMP: [f32; 4] = [-1.0e9, -1.0e9, 1.0e9, 1.0e9];
}

/// Per-draw uniforms (WGSL `struct Draw`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct DrawUniforms {
    /// Screen transform, filled in by the renderer: `x' = x*k.x + w*k.y`,
    /// `y' = y*k.z + w*k.w`.
    pub screen: [f32; 4],
    pub prim: [f32; 4],
    pub env: [f32; 4],
    pub fog: [f32; 4],
    pub blend: [f32; 4],
    pub fill: [f32; 4],
    /// x = prim LOD fraction, y = unused (LOD_FRACTION is computed per
    /// pixel from `lodp`), z = K4, w = K5.
    pub lod: [f32; 4],
    /// Per tile: (shift S, shift T, origin S, origin T), (1/w, 1/h, 0, 0).
    pub tile0: [f32; 4],
    pub size0: [f32; 4],
    pub tile1: [f32; 4],
    pub size1: [f32; 4],
    /// Per-pixel RDP LOD (see `shader.rs` `rdp_lod`): x = [`LodMode`] as
    /// f32, y = max level (`gSPTexture` level), z = ½ if the chain is
    /// sampled bilinearly (per-level texel-center offset, `LodMode::Chain`),
    /// w = 0. All zero = LOD_FRACTION 0 (non-mipmapped draws).
    pub lodp: [f32; 4],
    /// Filled in by the renderer (like `screen`): N64 3-point filter per
    /// texel (x = TEXEL0, y = TEXEL1: 1 = on) and their wrap codes
    /// (z, w: S mode + 3 × T mode; 0 clamp, 1 repeat, 2 mirror).
    pub filt: [f32; 4],
}

/// How a draw gets its LOD fraction / mip tiles (`DrawUniforms::lodp.x`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LodMode {
    /// LOD_FRACTION = 0, TEXEL0/1 = the bound tiles (rects, max level 0,
    /// combiners that don't read LOD_FRACTION).
    Off = 0,
    /// `G_TL_TILE`: TEXEL0/1 = render tile / +1, LOD_FRACTION per pixel.
    Tile = 1,
    /// `G_TL_LOD` with the tiles forming a proper mip chain, bound as one
    /// texture with those levels ([`Frame::chains`]): the shader picks the
    /// levels per pixel.
    Chain = 2,
}

/// How the fragment shader produces its color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShaderMode {
    /// Combiner + blender.
    Normal,
    /// `G_CYC_FILL`: the fill color.
    Fill,
    /// `G_CYC_COPY`: TEXEL0 as-is.
    Copy,
    /// Fill rectangle into the depth buffer: writes the fill word's depth
    /// (vertex z), no color.
    DepthClear,
    /// Combiner/blender draw with the color image pointed at the z image
    /// (`uvGfxStateDrawDL` shadow volumes): the RDP depth-tests the triangle
    /// against the z-buffer and writes the blended "color" as the new z.
    /// The renderer does this in two steps (stencil mark, then depth write).
    DepthImage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShaderKey {
    pub mode: ShaderMode,
    pub combiner: CombinerKey,
    pub blend: BlendState,
    /// `ALPHA_CVG_SEL` without `CVG_X_ALPHA`: blender alpha = coverage (1).
    pub alpha_cvg_sel: bool,
}

/// Everything that selects a GPU pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PipelineKey {
    pub shader: ShaderKey,
    pub depth: DepthState,
    /// `G_CULL_FRONT` = 1, `G_CULL_BACK` = 2 (both = nothing drawn).
    pub cull: u8,
}

/// A run of triangles sharing all state.
#[derive(Debug, Clone, PartialEq)]
pub struct DrawCall {
    pub pipeline: PipelineKey,
    pub first_vertex: u32,
    pub vertex_count: u32,
    pub uniforms: DrawUniforms,
    pub textures: [Option<TileBinding>; 2],
    /// Scissor in N64 pixels: x0, y0, x1, y1.
    pub scissor: [f32; 4],
    /// 3D geometry (clipped to the viewport × clip ratio) vs 2D rectangles.
    pub is_3d: bool,
    /// Widescreen behaviour (only used with `RenderOptions::widescreen`).
    pub wide: Wide,
    /// Widescreen HUD edge anchor (only moves `Wide::Fixed` draws).
    pub anchor: Anchor,
    /// Fill-view HUD vertical anchor (only moves `Wide::Fixed` draws).
    pub vanchor: VAnchor,
    /// For [`Wide::Stretch`]: the vertices' min/max row in N64 pixels
    /// (a scissor must span the main view vertically for the fill view to
    /// stretch it; see [`Wide::MAIN_VIEW_Y`]). `[0, 0]` otherwise.
    pub stretch_y: [f32; 2],
    /// Flight-HUD draw (the native game's `G_NOOP` "PWH" tags,
    /// `interp::HUD_TAG_ON`, `pw64_hud_tag`): the only draws the OLED care
    /// options (drift + dim) apply to. Ignored when OLED care is off
    /// (default options are bit-identical).
    pub hud: bool,
}

/// Horizontal HUD anchor in widescreen: a `Wide::Fixed` draw is shifted by
/// the side margin (output width beyond 4:3, halved) towards its edge, its
/// scissor with it. Set by the native game's `G_NOOP` anchor markers
/// (`interp::ANCHOR_TAG`, `pw64_widescreen.c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    Left,
    #[default]
    Centre,
    Right,
}

impl Anchor {
    /// Multiple of the side margin a draw is moved by.
    pub fn sign(self) -> f32 {
        match self {
            Anchor::Left => -1.0,
            Anchor::Centre => 0.0,
            Anchor::Right => 1.0,
        }
    }
}

/// Vertical HUD anchor for the fill view (S17): a `Wide::Fixed` draw moves
/// from its old letterboxed position to the true screen edge, scissor with
/// it. Set by the native game's `G_NOOP` marker (`interp::VANCHOR_TAG`,
/// "PWV"); only applies when `RenderOptions::fill_view` is on (with the
/// letterbox bars there is nothing to gain).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VAnchor {
    Top,
    #[default]
    Middle,
    Bottom,
}

impl VAnchor {
    /// Rows a draw moves by (N64 top-down rows): Top up to the true top
    /// edge, Bottom down to the true bottom edge. The timer keeps roughly
    /// its old gap to the edge; the bottom gauges end ~8 rows from the edge
    /// instead of 20 (tune visually).
    pub fn rows(self) -> f32 {
        match self {
            VAnchor::Top => -8.0,
            VAnchor::Middle => 0.0,
            VAnchor::Bottom => 12.0,
        }
    }
}

/// How a draw is placed when the output is wider than 4:3.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Wide {
    /// HUD/2D: stays in the centred 4:3 area.
    Fixed,
    /// Perspective 3D: same mapping as 4:3, but not cropped at the 4:3
    /// edges when its scissor is the main view (Hor+).
    Extend,
    /// Untextured and spanning the main view (clears, fades, letterbox
    /// bars): N64 x range `[x0, x1]` is stretched over the full output width.
    Stretch([f32; 2]),
}

impl Wide {
    /// The main view's x range (N64 pixels): a scissor or a stretched draw
    /// must reach at least this far both ways. Covers the full width and
    /// the flight camera's overscan-inset subscreen (x 10..310).
    pub const MAIN_VIEW: [f32; 2] = [16.0, N64_WIDTH - 16.0];
    /// The main view's row range for the fill view (S17): a scissor or a
    /// stretched fill must reach at least this far up/down to be given the
    /// output area's full height / stretched onto it. Covers the world
    /// view's rows (8..222), the replay fades (7..223) and the sky clip box
    /// (3..227).
    pub const MAIN_VIEW_Y: [f32; 2] = [10.0, 220.0];
}

/// One processed display list.
#[derive(Default, Clone)]
pub struct Frame {
    pub vertices: Vec<Vertex>,
    pub draws: Vec<DrawCall>,
    /// Every texture the draws reference, by content key.
    pub textures: HashMap<u64, Arc<Image>>,
    /// `LodMode::Chain` textures: chain key → the level textures' content
    /// keys (level 0 first; each in `textures`, each half the previous).
    pub chains: HashMap<u64, Arc<[u64]>>,
}

impl Frame {
    pub fn triangle_count(&self) -> usize {
        self.vertices.len() / 3
    }
}
