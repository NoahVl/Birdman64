//! Renderer output identity: the synthetic frames rendered with default
//! options must keep their pixel hashes (recorded under mesa lavapipe
//! before the LOD-fraction / 3-point-filter work, 2026-09-29). Other
//! adapters rasterize differently, so the constants are only checked on
//! llvmpipe; elsewhere the test just renders. Skips without an adapter.

mod synth;

use pw64_gfx::{Interpreter, RenderOptions, Renderer, TexFilter};

const SEED1_MSAA4: u64 = 0x3cb4_707d_993b_5cd5;
const SEED2_MSAA1: u64 = 0x9862_e31a_a46d_7635;

fn fnv(b: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &x in b {
        h = (h ^ x as u64).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn gpu() -> Option<Gpu> {
    let instance = wgpu::Instance::default();
    let adapter =
        pw64_gfx::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .ok()?;
    let name = adapter.get_info().name;
    let (d, q) =
        pw64_gfx::block_on(adapter.request_device(&pw64_gfx::device_descriptor(&adapter))).ok()?;
    Some((d, q, name))
}

/// With the per-pixel LOD fraction (the synthetic frames use the mip-lerp
/// combiner with max level 1), default options.
const LOD_SEED1_MSAA4: u64 = 0x48cf_e45e_a239_5aae;
const LOD_SEED2_MSAA1: u64 = 0x8be1_b4bb_3fdb_3d4d;

fn render(seed: u64, msaa: u32, lod: bool, filter: TexFilter, gpu: &Gpu) -> u64 {
    let w = synth::build(seed);
    let mut frame = Interpreter::new().run(&w.mem, w.dl);
    if !lod {
        // What the renderer drew before LOD fractions existed: every draw
        // as a non-mipmapped one.
        for d in &mut frame.draws {
            d.uniforms.lodp = [0.0; 4];
        }
    }
    let mut r = Renderer::new(
        &gpu.0,
        &gpu.1,
        wgpu::TextureFormat::Rgba8Unorm,
        RenderOptions {
            msaa,
            filter,
            fill_view: false,
            ..RenderOptions::default()
        },
    );
    fnv(&r.render_to_rgba(&frame, (640, 480)))
}

type Gpu = (wgpu::Device, wgpu::Queue, String);

/// Non-mipmapped draws with the default (bilinear) filter render exactly as
/// before the LOD/3-point work; mip-mapped ones change (LOD fraction), and
/// the N64 filter changes the image.
#[test]
fn default_render_is_unchanged() {
    let Some(g) = gpu() else {
        eprintln!("no GPU adapter; skipped");
        return;
    };
    let bl = TexFilter::Bilinear;
    let h1 = render(1, 4, false, bl, &g);
    let h2 = render(2, 1, false, bl, &g);
    let l1 = render(1, 4, true, bl, &g);
    let l2 = render(2, 1, true, bl, &g);
    let n1 = render(1, 4, true, TexFilter::N64, &g);
    eprintln!(
        "{}: no-LOD {h1:#018x} {h2:#018x}, LOD {l1:#018x} {l2:#018x}, n64 {n1:#018x}",
        g.2
    );
    if g.2.contains("llvmpipe") {
        assert_eq!((h1, h2), (SEED1_MSAA4, SEED2_MSAA1));
        assert_eq!((l1, l2), (LOD_SEED1_MSAA4, LOD_SEED2_MSAA1));
    }
    assert_ne!(h1, l1, "LOD fraction changes the mip-lerp draws");
    assert_ne!(l1, n1, "3-point differs from bilinear");
}
