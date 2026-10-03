//! WGSL generation per [`ShaderKey`]: a fixed vertex stage plus a fragment
//! stage assembled from the generated combiner and blender functions.

use crate::frame::{ShaderKey, ShaderMode};
use crate::rdp::AlphaTest;

const PRELUDE: &str = r#"
struct Draw {
    screen: vec4<f32>,
    prim: vec4<f32>,
    env: vec4<f32>,
    fog: vec4<f32>,
    blend: vec4<f32>,
    fill: vec4<f32>,
    lod: vec4<f32>,
    tile0: vec4<f32>,
    size0: vec4<f32>,
    tile1: vec4<f32>,
    size1: vec4<f32>,
    lodp: vec4<f32>,
    filt: vec4<f32>,
};
@group(0) @binding(0) var<uniform> u: Draw;
@group(1) @binding(0) var tex0: texture_2d<f32>;
@group(1) @binding(1) var samp0: sampler;
@group(1) @binding(2) var tex1: texture_2d<f32>;
@group(1) @binding(3) var samp1: sampler;

struct VIn {
    @location(0) pos: vec4<f32>,
    @location(1) color: vec4<f32>,
    @location(2) st: vec2<f32>,
    @location(3) st_clamp: vec4<f32>,
};
struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) st: vec2<f32>,
    @location(2) @interpolate(flat) st_clamp: vec4<f32>,
};

@vertex
fn vs_main(v: VIn) -> VOut {
    var o: VOut;
    o.pos = vec4<f32>(v.pos.x * u.screen.x + v.pos.w * u.screen.y,
                      v.pos.y * u.screen.z + v.pos.w * u.screen.w,
                      v.pos.z, v.pos.w);
    o.color = v.color;
    o.st = v.st;
    o.st_clamp = v.st_clamp;
    return o;
}

fn noise(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(12.9898, 78.233))) * 43758.5453);
}

// OLED care dim: u.lod.y = 1 - HUD brightness (0 = off; x1.0 is exact, so
// the default options are bit-identical). Alpha is kept.
fn dim(c: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(c.rgb * (1.0 - u.lod.y), c.a);
}

// RDP LOD per pixel (u.lodp.x != 0, i.e. LodMode::Tile/Chain; max level
// u.lodp.y >= 1). L = the largest |dS|, |dT| to the next pixel in x and y,
// in texels before the tile shift (the RDP's max(|dS|,|dT|) of the pixel
// deltas; here per target pixel, so higher resolutions keep finer levels).
// Returns (TEXEL0 level, TEXEL1 level, LOD_FRACTION):
// - L < 1 (magnify): level 0 for both, fraction 0;
// - distant (floor(log2 L) >= max level, or L >= 256 = lod bits 0x6000):
//   the max level for both, fraction 1 (0xFF);
// - else level t = floor(log2 L), t + 1 and the fraction L / 2^t - 1 —
//   linear within the octave, as the RDP's ((lod << 3) >> t) & 0xFF.
// Detail/sharpen and prim min level are not modelled (PW64 never enables
// them, gfx.md GBI audit).
fn rdp_lod(dx: vec2<f32>, dy: vec2<f32>) -> vec3<f32> {
    if (u.lodp.x == 0.0) {
        return vec3<f32>(0.0, 1.0, 0.0);
    }
    let l = max(max(abs(dx.x), abs(dx.y)), max(abs(dy.x), abs(dy.y)));
    if (!(l >= 1.0)) {
        return vec3<f32>(0.0);
    }
    let t = floor(log2(l));
    if (t >= u.lodp.y || l >= 256.0) {
        return vec3<f32>(u.lodp.y, u.lodp.y, 1.0);
    }
    return vec3<f32>(t, t + 1.0, clamp(l / exp2(t) - 1.0, 0.0, 1.0));
}

// LodMode::Chain: level l's UV. Level l's texel centers sit at integer
// level-l coordinates (S/2^l), so bilinear sampling needs +½ texel of level
// l, not of level 0 (u.lodp.z = ½ when bilinear, 0 for point sampling).
fn chain_uv(uv0: vec2<f32>, l: f32) -> vec2<f32> {
    return uv0 + u.lodp.z * (exp2(l) - 1.0) * u.size0.xy;
}
"#;

/// N64 3-point filter helpers (only in modules for
/// `TexFilter::N64`; see `FILTER3`).
const FILTER3_FNS: &str = r#"
// Texel index `i` of an axis of `n` texels under a wrap code (0 clamp,
// 1 repeat, 2 mirror) — what the GPU samplers do for the same modes.
fn wrap_i(i: i32, n: i32, mode: i32) -> i32 {
    if (mode == 1) {
        return ((i % n) + n) % n;
    }
    if (mode == 2) {
        let p = 2 * n;
        let m = ((i % p) + p) % p;
        return select(m, p - 1 - m, m >= n);
    }
    return clamp(i, 0, n - 1);
}

fn wrap_code(c: f32) -> vec2<i32> {
    let v = i32(c);
    return vec2<i32>(v % 3, v / 3);
}

// RDP 3-point filter at mip level `lvl` (texel centers at +½ in UV, as
// for the GPU sampler): of the 2x2 texels around the sample, the triangle
// the fraction (fx, fy) falls in — upper left (fx + fy < 1: t00, t10,
// t01) or lower right (t11, t01, t10) — is interpolated.
fn texel3(t: texture_2d<f32>, uv: vec2<f32>, lvl: i32, wrap: vec2<i32>) -> vec4<f32> {
    let n = vec2<i32>(textureDimensions(t, lvl));
    let p = uv * vec2<f32>(n) - 0.5;
    let b = floor(p);
    let f = p - b;
    let i = vec2<i32>(b);
    let x0 = wrap_i(i.x, n.x, wrap.x);
    let x1 = wrap_i(i.x + 1, n.x, wrap.x);
    let y0 = wrap_i(i.y, n.y, wrap.y);
    let y1 = wrap_i(i.y + 1, n.y, wrap.y);
    let t01 = textureLoad(t, vec2<i32>(x0, y1), lvl);
    let t10 = textureLoad(t, vec2<i32>(x1, y0), lvl);
    if (f.x + f.y < 1.0) {
        let t00 = textureLoad(t, vec2<i32>(x0, y0), lvl);
        return t00 + f.x * (t10 - t00) + f.y * (t01 - t00);
    }
    let t11 = textureLoad(t, vec2<i32>(x1, y1), lvl);
    return t11 + (1.0 - f.x) * (t01 - t11) + (1.0 - f.y) * (t10 - t11);
}

// 3-point on level 0 while magnifying; the N64 has no mipmaps, so when
// minifying this fades (over one GPU LOD step, from the level-0 footprint
// `duv*` in UV per pixel) into the GPU's trilinear/anisotropic sample
// `gpu` instead of aliasing.
fn filter3(t: texture_2d<f32>, gpu: vec4<f32>, uv: vec2<f32>, duvx: vec2<f32>, duvy: vec2<f32>, wrap: vec2<i32>) -> vec4<f32> {
    let n = vec2<f32>(textureDimensions(t, 0));
    let lam = log2(max(length(duvx * n), length(duvy * n)));
    let w = clamp(lam, 0.0, 1.0);
    if (w >= 1.0) {
        return gpu;
    }
    return mix(texel3(t, uv, 0, wrap), gpu, w);
}
"#;

/// Fragment prologue: S/T (clamped to the texrect range), its per-pixel
/// derivatives (uniform control flow), both texels and the LOD fraction
/// `lf`. `LodMode::Chain` replaces the texels with the chain levels.
const SAMPLE: &str = "    let st = clamp(i.st, i.st_clamp.xy, i.st_clamp.zw);
    let dsx = dpdx(i.st);
    let dsy = dpdy(i.st);
    let uv0 = (st * u.tile0.xy - u.tile0.zw) * u.size0.xy;
    let uv1 = (st * u.tile1.xy - u.tile1.zw) * u.size1.xy;
    var t0 = textureSample(tex0, samp0, uv0);
    var t1 = textureSample(tex1, samp1, uv1);
    let lod = rdp_lod(dsx, dsy);
    let lf = lod.z;
";

/// Chain sampling (default filter).
const CHAIN: &str = "    if (u.lodp.x == 2.0) {
        t0 = textureSampleLevel(tex0, samp0, chain_uv(uv0, lod.x), lod.x);
        t1 = textureSampleLevel(tex0, samp0, chain_uv(uv0, lod.y), lod.y);
    }
";

/// Chain + 3-point: the chain levels are the N64's own, so 3-point on the
/// exact level (no GPU fade); other bilinear texels through `filter3`.
const FILTER3: &str = "    if (u.lodp.x == 2.0) {
        if (u.filt.x == 1.0) {
            let w = wrap_code(u.filt.z);
            t0 = texel3(tex0, chain_uv(uv0, lod.x), i32(lod.x), w);
            t1 = texel3(tex0, chain_uv(uv0, lod.y), i32(lod.y), w);
        } else {
            t0 = textureSampleLevel(tex0, samp0, chain_uv(uv0, lod.x), lod.x);
            t1 = textureSampleLevel(tex0, samp0, chain_uv(uv0, lod.y), lod.y);
        }
    } else {
        if (u.filt.x == 1.0) {
            t0 = filter3(tex0, t0, uv0, dsx * u.tile0.xy * u.size0.xy, dsy * u.tile0.xy * u.size0.xy, wrap_code(u.filt.z));
        }
        if (u.filt.y == 1.0) {
            t1 = filter3(tex1, t1, uv1, dsx * u.tile1.xy * u.size1.xy, dsy * u.tile1.xy * u.size1.xy, wrap_code(u.filt.w));
        }
    }
";

/// Full WGSL module for `key`; `n64_filter` = `TexFilter::N64` (3-point
/// for bilinear tiles, selected per texel by `u.filt`).
pub fn wgsl(key: &ShaderKey, n64_filter: bool) -> String {
    let mut s = String::from(PRELUDE);
    let sample = if n64_filter {
        s += FILTER3_FNS;
        format!("{SAMPLE}{FILTER3}")
    } else {
        format!("{SAMPLE}{CHAIN}")
    };
    s += &key.combiner.wgsl();
    s += &key.blend.wgsl(key.alpha_cvg_sel);
    s += "\n@fragment\nfn fs_main(i: VOut) -> @location(0) vec4<f32> {\n";
    s += &sample;
    match key.mode {
        ShaderMode::Fill => s += "    return dim(u.fill);\n",
        ShaderMode::DepthClear => s += "    return vec4<f32>(0.0);\n",
        ShaderMode::Copy => {
            // Copy mode only has the alpha-compare test.
            if key.blend.alpha_test == AlphaTest::Threshold {
                s += "    if (t0.a < max(u.blend.a, 0.5 / 255.0)) { discard; }\n";
            }
            s += "    return dim(t0);\n";
        }
        // DepthImage is not dimmed: it writes the z image, no visible color.
        ShaderMode::DepthImage => {
            s += &combined(key);
            s += "    return blend(c, shade);\n";
        }
        ShaderMode::Normal => {
            s += &combined(key);
            s += "    return dim(blend(c, shade));\n";
        }
    }
    s += "}\n";
    if key.mode == ShaderMode::DepthImage {
        // Second step: the blender input as a z-buffer word → depth.
        s += crate::rdp::WGSL_Z_WORD;
        s += "\n@fragment\nfn fs_depth(i: VOut) -> @builtin(frag_depth) f32 {\n";
        s += &sample;
        s += &combined(key);
        s += "    return zword_depth(c);\n}\n";
    }
    s
}

/// Fragment-body lines computing the combiner output `c` (+ alpha test).
fn combined(key: &ShaderKey) -> String {
    let mut s = String::from("    let shade = i.color;\n");
    s += "    let c = combine(t0, t1, shade, noise(i.pos.xy), lf);\n";
    s += match key.blend.alpha_test {
        AlphaTest::None => "",
        AlphaTest::Threshold => "    if (c.a < max(u.blend.a, 0.5 / 255.0)) { discard; }\n",
        AlphaTest::CoverageBlend => "    if (c.a < 1.0 / 8.0) { discard; }\n",
        AlphaTest::CoverageEdge => "    if (c.a < 0.5) { discard; }\n",
    };
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::combiner::CombinerKey;
    use crate::rdp::{BlendState, CycleType};

    /// Every generated module must parse and validate with naga.
    #[test]
    fn generated_modules_validate() {
        let combiners = [
            (0xFC26_A004, 0x1F10_93FF),
            (0xFC12_19FF, 0xFFFF_FE38),
            (0xFC11_1404, 0xFF13_FFFF),
            (0xFC40_FE0B, 0x34FD_FD38),
            (0xFCFF_FFFF, 0xFFFF_FFFF),
        ];
        let modes = [
            0x0C08_0000 | 0x0011_2078,
            0xC800_0000 | 0x0010_49D8,
            0x0055_0000,
            0x0000_1003,
        ];
        for (w0, w1) in combiners {
            for two in [false, true] {
                for &l in &modes {
                    for mode in [
                        ShaderMode::Normal,
                        ShaderMode::Fill,
                        ShaderMode::Copy,
                        ShaderMode::DepthImage,
                    ] {
                        let cycle = if two { CycleType::Two } else { CycleType::One };
                        let key = ShaderKey {
                            mode,
                            combiner: CombinerKey::new(w0, w1, two),
                            blend: BlendState::new(cycle, l),
                            alpha_cvg_sel: l & 0x2000 != 0,
                        };
                        for n64 in [false, true] {
                            let src = wgsl(&key, n64);
                            let module = naga::front::wgsl::parse_str(&src)
                                .unwrap_or_else(|e| panic!("{}\n{src}", e.emit_to_string(&src)));
                            naga::valid::Validator::new(
                                naga::valid::ValidationFlags::all(),
                                naga::valid::Capabilities::empty(),
                            )
                            .validate(&module)
                            .unwrap_or_else(|e| panic!("{e:?}\n{src}"));
                        }
                    }
                }
            }
        }
    }
}
