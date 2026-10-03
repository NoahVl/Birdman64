//! GPU checks of the per-pixel RDP LOD fraction / mip-chain selection and
//! the N64 3-point filter against CPU-computed expectations. Frames are
//! built by hand (exact S/T per pixel); rendered at 320×240 so one target
//! pixel = one N64 pixel. Skips without an adapter (lavapipe in the cloud
//! container: `apt-get install mesa-vulkan-drivers`).

use pw64_formats::Image;
use pw64_gfx::combiner::{CombinerKey, Cycle};
use pw64_gfx::frame::{DrawCall, DrawUniforms, Frame, LodMode, PipelineKey, ShaderKey, ShaderMode};
use pw64_gfx::rdp::{BlendState, CycleType, DepthState, FinalBlend};
use pw64_gfx::texture::{SamplerKey, TileBinding, Wrap};
use pw64_gfx::{Anchor, RenderOptions, Renderer, TexFilter, Wide, frame::Vertex};
use std::collections::HashMap;
use std::sync::Arc;

const W: u32 = 320;
const H: u32 = 240;
/// Blender word: (IN, 0, IN, 1) in both cycles = opaque pass-through.
const OPAQUE: u32 = 0x0C08_0000 | 0x0302_0000;

fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::default();
    let adapter =
        pw64_gfx::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .ok()?;
    pw64_gfx::block_on(adapter.request_device(&pw64_gfx::device_descriptor(&adapter))).ok()
}

fn render(frame: &Frame, filter: TexFilter) -> Option<Vec<u8>> {
    let (d, q) = gpu()?;
    let mut r = Renderer::new(
        &d,
        &q,
        wgpu::TextureFormat::Rgba8Unorm,
        RenderOptions {
            msaa: 1,
            filter,
            fill_view: false,
            ..RenderOptions::default()
        },
    );
    Some(r.render_to_rgba(frame, (W, H)))
}

fn solid(w: u32, h: u32, c: [u8; 4]) -> Arc<Image> {
    Arc::new(Image {
        width: w,
        height: h,
        rgba: c.repeat((w * h) as usize),
    })
}

fn binding(key: u64, w: u32, h: u32, shift: f32, wrap: Wrap) -> TileBinding {
    TileBinding {
        key,
        sampler: SamplerKey {
            wrap: [wrap; 2],
            linear: true,
        },
        width: w,
        height: h,
        shift: [shift; 2],
        // uls 0, minus half a texel (bilinear).
        origin: [-0.5; 2],
    }
}

fn tile_uniform(b: &TileBinding) -> ([f32; 4], [f32; 4]) {
    (
        [b.shift[0], b.shift[1], b.origin[0], b.origin[1]],
        [1.0 / b.width as f32, 1.0 / b.height as f32, 0.0, 0.0],
    )
}

/// One draw of `verts` (triangle list) with the given combiner/textures.
fn frame(
    verts: Vec<Vertex>,
    combiner: CombinerKey,
    cycle: CycleType,
    tex: [TileBinding; 2],
    lodp: [f32; 4],
    images: Vec<(u64, Arc<Image>)>,
) -> Frame {
    let blend = BlendState::new(cycle, OPAQUE);
    assert_eq!(blend.kind, FinalBlend::Opaque);
    let (tile0, size0) = tile_uniform(&tex[0]);
    let (tile1, size1) = tile_uniform(&tex[1]);
    let draw = DrawCall {
        pipeline: PipelineKey {
            shader: ShaderKey {
                mode: ShaderMode::Normal,
                combiner,
                blend,
                alpha_cvg_sel: false,
            },
            depth: DepthState {
                test: false,
                write: false,
                decal: false,
            },
            cull: 0,
        },
        first_vertex: 0,
        vertex_count: verts.len() as u32,
        uniforms: DrawUniforms {
            screen: [0.0; 4],
            prim: [0.0; 4],
            env: [0.0; 4],
            fog: [0.0; 4],
            blend: [0.0; 4],
            fill: [0.0; 4],
            lod: [0.0; 4],
            tile0,
            size0,
            tile1,
            size1,
            lodp,
            filt: [0.0; 4],
        },
        textures: [Some(tex[0]), Some(tex[1])],
        scissor: [0.0, 0.0, W as f32, H as f32],
        is_3d: true,
        wide: Wide::Fixed,
        anchor: Anchor::Centre,
        vanchor: pw64_gfx::frame::VAnchor::Middle,
        stretch_y: [0.0; 2],
        hud: false,
    };
    Frame {
        vertices: verts,
        draws: vec![draw],
        textures: images.into_iter().collect(),
        chains: HashMap::new(),
    }
}

/// Full-screen quad; `st(x, y, w)` gives each corner's S/T.
fn screen_quad(w_of_y: impl Fn(f32) -> f32, st: impl Fn(f32, f32) -> [f32; 2]) -> Vec<Vertex> {
    let v = |x: f32, y: f32| {
        let w = w_of_y(y);
        Vertex {
            pos: [x * w, y * w, 0.5 * w, w],
            color: [1.0; 4],
            st: st(x, y),
            st_clamp: Vertex::NO_CLAMP,
        }
    };
    let (w, h) = (W as f32, H as f32);
    vec![
        v(0.0, 0.0),
        v(w, 0.0),
        v(w, h),
        v(0.0, 0.0),
        v(w, h),
        v(0.0, h),
    ]
}

/// The receding plane: w = 16 at the top row, 1 at the bottom; s/w = x/2,
/// t/w = (240 - y)/2 (affine in screen space, so both triangles agree).
fn inv_w(y: f32) -> f32 {
    1.0 / 16.0 + (15.0 / 16.0) * (y / 240.0)
}
fn plane_st(x: f32, y: f32) -> [f32; 2] {
    [0.5 * x / inv_w(y), 0.5 * (240.0 - y) / inv_w(y)]
}
fn receding_plane() -> Vec<Vertex> {
    screen_quad(|y| 1.0 / inv_w(y), plane_st)
}

/// CPU twin of the shader's `rdp_lod` at pixel (px, py) (px even): the
/// GPU derivatives are differences inside a 2×2 quad (fine or coarse agree
/// for the quad's even column).
fn cpu_lod(px: u32, py: u32, max_level: f32) -> (f32, f32, f32) {
    let (x, y0) = (px as f32 + 0.5, (py & !1) as f32 + 0.5);
    let a = plane_st(x, y0);
    let dx = plane_st(x + 1.0, y0);
    let dy = plane_st(x, y0 + 1.0);
    let l = [dx[0] - a[0], dx[1] - a[1], dy[0] - a[0], dy[1] - a[1]]
        .iter()
        .fold(0.0f32, |m, v| m.max(v.abs()));
    if l < 1.0 {
        return (0.0, 0.0, 0.0);
    }
    let t = l.log2().floor();
    if t >= max_level || l >= 256.0 {
        return (max_level, max_level, 1.0);
    }
    (t, t + 1.0, l / t.exp2() - 1.0)
}

/// `(T1 - T0) * LOD_FRACTION + T0` (RGB and alpha), then `COMBINED * SHADE`:
/// PW64's most common UVTX combiner.
fn mip_lerp() -> CombinerKey {
    CombinerKey::new(0xFC26_A004, 0x1F10_93FF, true)
}

fn px(img: &[u8], x: u32, y: u32) -> [f32; 3] {
    let i = ((y * W + x) * 4) as usize;
    [0, 1, 2].map(|c| img[i + c] as f32)
}

fn mix(a: [u8; 4], b: [u8; 4], f: f32) -> [f32; 3] {
    [0, 1, 2].map(|c| a[c] as f32 + (b[c] as f32 - a[c] as f32) * f)
}

/// Pixels spread over the plane whose LOD fraction isn't next to an
/// octave edge (where a derivative rounding difference flips the level).
fn sample_pixels(max_level: f32) -> Vec<(u32, u32, (f32, f32, f32))> {
    let mut out = Vec::new();
    for py in (1..H).step_by(7) {
        for px in [40u32, 160, 300] {
            let lod = cpu_lod(px, py, max_level);
            let edge = lod.2 != 0.0 && lod.2 != 1.0 && (lod.2 < 0.05 || lod.2 > 0.95);
            if !edge {
                out.push((px, py, lod));
            }
        }
    }
    out
}

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

/// `G_TL_TILE`: TEXEL0 = tile (red), TEXEL1 = tile + 1 (blue); the
/// fraction blends them per pixel along the receding plane — 0 while
/// magnifying near the bottom, the RDP's in-octave ramp further up, 1 once
/// distant (octave ≥ max level).
#[test]
fn tile_mode_lod_fraction_blends_levels() {
    let t0 = binding(1, 32, 32, 1.0, Wrap::Repeat);
    let t1 = binding(2, 16, 16, 0.5, Wrap::Repeat);
    let f = frame(
        receding_plane(),
        mip_lerp(),
        CycleType::Two,
        [t0, t1],
        [LodMode::Tile as u8 as f32, 2.0, 0.0, 0.0],
        vec![(1, solid(32, 32, RED)), (2, solid(16, 16, BLUE))],
    );
    let Some(img) = render(&f, TexFilter::Bilinear) else {
        eprintln!("no GPU adapter; skipped");
        return;
    };
    let pixels = sample_pixels(2.0);
    let mut seen = [false; 3]; // magnified, blended, distant
    for &(x, y, (_, _, lf)) in &pixels {
        let want = mix(RED, BLUE, lf);
        let got = px(&img, x, y);
        for c in 0..3 {
            assert!(
                (got[c] - want[c]).abs() <= 10.0,
                "({x},{y}) lf {lf}: got {got:?} want {want:?}"
            );
        }
        seen[if lf == 0.0 {
            0
        } else if lf == 1.0 {
            2
        } else {
            1
        }] = true;
    }
    assert_eq!(seen, [true; 3], "plane covers magnify, ramp and distant");

    // The same draw without LOD (non-mipmapped): TEXEL0 only, as before.
    let mut off = f.clone();
    off.draws[0].uniforms.lodp = [0.0; 4];
    let img = render(&off, TexFilter::Bilinear).unwrap();
    for &(x, y, _) in &pixels {
        assert_eq!(px(&img, x, y), [255.0, 0.0, 0.0], "({x},{y})");
    }
}

/// `G_TL_LOD` with a chain: TEXEL0/TEXEL1 = levels floor(log2 L) and +1 of
/// a 4-level chain (one colour per level), blended by the fraction; the
/// max level (3) once distant.
#[test]
fn chain_mode_selects_levels_per_pixel() {
    let colors = [RED, GREEN, BLUE, WHITE];
    let chain = binding(100, 32, 32, 1.0, Wrap::Repeat);
    let mut f = frame(
        receding_plane(),
        mip_lerp(),
        CycleType::Two,
        [chain, chain],
        [LodMode::Chain as u8 as f32, 3.0, 0.5, 0.0],
        (0..4)
            .map(|l| (10 + l as u64, solid(32 >> l, 32 >> l, colors[l])))
            .collect(),
    );
    f.chains.insert(100, [10, 11, 12, 13].into());
    let Some(img) = render(&f, TexFilter::Bilinear) else {
        eprintln!("no GPU adapter; skipped");
        return;
    };
    let mut levels = [false; 4];
    for (x, y, (l0, l1, lf)) in sample_pixels(3.0) {
        let want = mix(colors[l0 as usize], colors[l1 as usize], lf);
        let got = px(&img, x, y);
        for c in 0..3 {
            assert!(
                (got[c] - want[c]).abs() <= 10.0,
                "({x},{y}) levels {l0}/{l1} lf {lf}: got {got:?} want {want:?}"
            );
        }
        levels[l0 as usize] = true;
    }
    assert_eq!(levels, [true; 4], "all chain levels reached");
}

/// 1-cycle `TEXEL0` over a magnified 2×2 texture: every pixel is the RDP's
/// 3-point interpolation (upper-left or lower-right triangle of the 2×2
/// texel square), with clamp and repeat addressing. The default filter
/// stays bilinear.
#[test]
fn n64_filter_is_three_point() {
    let texels: [[u8; 4]; 4] = [
        [200, 10, 30, 255],
        [20, 220, 60, 255],
        [40, 90, 250, 255],
        [250, 240, 5, 255],
    ];
    let img = Arc::new(Image {
        width: 2,
        height: 2,
        rgba: texels.concat(),
    });
    let one = Cycle {
        rgb: [15, 15, 31, 1],
        alpha: [7, 7, 7, 1],
    };
    let (w0, w1) = CombinerKey::encode([one, one]);
    let combiner = CombinerKey::new(w0, w1, false);
    for (wrap, span) in [(Wrap::Clamp, 2.0f32), (Wrap::Repeat, 4.0)] {
        let b = binding(7, 2, 2, 1.0, wrap);
        // S/T = texel coordinates, texel k's center at k (RDP): -0.5 at the
        // left/top edge to span - 0.5 at the right/bottom.
        let st = |x: f32, y: f32| [x / W as f32 * span - 0.5, y / H as f32 * span - 0.5];
        let f = frame(
            screen_quad(|_| 1.0, st),
            combiner,
            CycleType::One,
            [b, b],
            [0.0; 4],
            vec![(7, img.clone())],
        );
        let Some(out) = render(&f, TexFilter::N64) else {
            eprintln!("no GPU adapter; skipped");
            return;
        };
        let texel = |i: i32, j: i32| {
            let c = |v: i32| match wrap {
                Wrap::Clamp => v.clamp(0, 1),
                _ => v.rem_euclid(2),
            };
            texels[(c(j) * 2 + c(i)) as usize].map(|v| v as f32)
        };
        for y in (3..H).step_by(11) {
            for x in (5..W).step_by(13) {
                let [s, t] = st(x as f32 + 0.5, y as f32 + 0.5);
                let (i, j) = (s.floor() as i32, t.floor() as i32);
                let (fx, fy) = (s - s.floor(), t - t.floor());
                if (fx + fy - 1.0).abs() < 0.02 {
                    continue; // on the triangle diagonal
                }
                let (t00, t10, t01, t11) = (
                    texel(i, j),
                    texel(i + 1, j),
                    texel(i, j + 1),
                    texel(i + 1, j + 1),
                );
                let want: [f32; 3] = std::array::from_fn(|c| {
                    if fx + fy < 1.0 {
                        t00[c] + fx * (t10[c] - t00[c]) + fy * (t01[c] - t00[c])
                    } else {
                        t11[c] + (1.0 - fx) * (t01[c] - t11[c]) + (1.0 - fy) * (t10[c] - t11[c])
                    }
                });
                let got = px(&out, x, y);
                for c in 0..3 {
                    assert!(
                        (got[c] - want[c]).abs() <= 2.5,
                        "{wrap:?} ({x},{y}) s {s} t {t}: got {got:?} want {want:?}"
                    );
                }
            }
        }
        // Bilinear (default) differs from 3-point inside the texel squares.
        let bl = render(&f, TexFilter::Bilinear).unwrap();
        assert_ne!(bl, out, "{wrap:?}");
    }
}
