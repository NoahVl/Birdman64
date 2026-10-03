//! RDP other-mode decoding: cycle type, texture filter, render mode
//! (blender, depth, coverage) → GPU state.
//!
//! The blender computes `(P * A + M * B)` per cycle. Cycles that do not read
//! the framebuffer (`CLR_MEM`) are emulated in the fragment shader (e.g.
//! `G_RM_FOG_SHADE_A` in cycle 0); the last cycle maps to fixed-function
//! GPU blending. Coverage is approximated: `CVG_X_ALPHA` (alpha-scaled
//! coverage) becomes alpha blending when the blender would blend on partial
//! coverage, or an alpha cutoff otherwise.

/// Other-mode H fields (`G_MDSFT_*` shifts from gbi.h).
pub mod omh {
    pub const CYCLETYPE_SHIFT: u32 = 20;
    pub const TEXTFILT_SHIFT: u32 = 12;
    pub const TEXTLUT_SHIFT: u32 = 14;
    /// `G_TL_LOD` (`G_MDSFT_TEXTLOD` = 16): tiles picked per pixel by LOD.
    pub const TEXTLOD: u32 = 1 << 16;
}

/// Other-mode L bits (render mode half, gbi.h).
pub mod oml {
    pub const ALPHA_COMPARE_MASK: u32 = 3;
    pub const G_AC_THRESHOLD: u32 = 1;
    pub const ZSRC_PRIM: u32 = 4;
    pub const AA_EN: u32 = 0x8;
    pub const Z_CMP: u32 = 0x10;
    pub const Z_UPD: u32 = 0x20;
    pub const IM_RD: u32 = 0x40;
    pub const CLR_ON_CVG: u32 = 0x80;
    pub const ZMODE_SHIFT: u32 = 10;
    pub const CVG_X_ALPHA: u32 = 0x1000;
    pub const ALPHA_CVG_SEL: u32 = 0x2000;
    pub const FORCE_BL: u32 = 0x4000;
}

/// `G_CYC_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CycleType {
    One,
    Two,
    Copy,
    Fill,
}

impl CycleType {
    pub fn from_othermode_h(h: u32) -> Self {
        match (h >> omh::CYCLETYPE_SHIFT) & 3 {
            0 => Self::One,
            1 => Self::Two,
            2 => Self::Copy,
            _ => Self::Fill,
        }
    }
}

/// `G_TF_POINT` (0) vs bilinear/average.
pub fn bilinear(h: u32) -> bool {
    (h >> omh::TEXTFILT_SHIFT) & 3 != 0
}

/// Blender inputs. P/M: 0 CLR_IN, 1 CLR_MEM, 2 CLR_BL, 3 CLR_FOG.
/// A: 0 A_IN, 1 A_FOG, 2 A_SHADE, 3 zero. B: 0 1-A, 1 A_MEM, 2 one, 3 zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlendCycle {
    pub p: u8,
    pub a: u8,
    pub m: u8,
    pub b: u8,
}

pub const CLR_MEM: u8 = 1;

impl BlendCycle {
    /// Cycle `n` (0 or 1) of the render-mode word.
    pub fn from_othermode_l(l: u32, n: usize) -> Self {
        let f = |sh: u32| ((l >> (sh - 2 * n as u32)) & 3) as u8;
        Self {
            p: f(30),
            a: f(26),
            m: f(22),
            b: f(18),
        }
    }

    fn reads_memory(&self) -> bool {
        self.p == CLR_MEM || self.m == CLR_MEM || self.b == 1
    }

    fn color(v: u8) -> &'static str {
        match v {
            0 | CLR_MEM => "px",
            2 => "u.blend.rgb",
            _ => "u.fog.rgb",
        }
    }

    fn alpha(v: u8) -> &'static str {
        match v {
            0 => "c.a",
            1 => "u.fog.a",
            2 => "shade.a",
            _ => "0.0",
        }
    }

    /// WGSL for a cycle evaluated entirely in the shader: `px` = input color.
    fn wgsl_full(&self) -> String {
        let (p, a, m) = (
            Self::color(self.p),
            Self::alpha(self.a),
            Self::color(self.m),
        );
        let b = match self.b {
            0 => format!("(1.0 - {a})"),
            1 | 2 => "1.0".into(),
            _ => "0.0".into(),
        };
        format!("    px = clamp({p} * {a} + {m} * {b}, vec3<f32>(0.0), vec3<f32>(1.0));\n")
    }
}

/// How the last blender cycle maps to GPU blending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FinalBlend {
    /// Write the shader color.
    Opaque,
    /// `src * a + dst * (1 - a)`.
    Alpha,
    /// Leave the color buffer alone (depth-only effects).
    KeepDst,
}

/// Fragment discard rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlphaTest {
    None,
    /// `G_AC_THRESHOLD`: alpha must reach the blend color's alpha.
    Threshold,
    /// Coverage × alpha reaches zero (blended `CVG_X_ALPHA`).
    CoverageBlend,
    /// Coverage × alpha below half a pixel (unblended `CVG_X_ALPHA`, "tex edge").
    CoverageEdge,
}

/// Decoded render state that feeds the shader and pipeline keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlendState {
    /// First cycle (2-cycle mode only), emulated in the shader.
    pub pre: Option<BlendCycle>,
    pub last: BlendCycle,
    pub kind: FinalBlend,
    pub alpha_test: AlphaTest,
}

impl BlendState {
    pub fn new(cycle: CycleType, l: u32) -> Self {
        let two = cycle == CycleType::Two;
        let pre = two.then(|| BlendCycle::from_othermode_l(l, 0));
        // 1-cycle mode uses the first blender cycle.
        let last = BlendCycle::from_othermode_l(l, two as usize);
        let force = l & oml::FORCE_BL != 0;
        let cvg_x_alpha = l & oml::CVG_X_ALPHA != 0;
        let aa = l & oml::AA_EN != 0;
        let kind = if last.p == CLR_MEM && last.m == CLR_MEM {
            FinalBlend::KeepDst
        } else if last.m == CLR_MEM && last.b == 0 && (force || (aa && cvg_x_alpha)) {
            FinalBlend::Alpha
        } else {
            FinalBlend::Opaque
        };
        let alpha_test = if l & oml::ALPHA_COMPARE_MASK == oml::G_AC_THRESHOLD {
            AlphaTest::Threshold
        } else if cvg_x_alpha {
            if kind == FinalBlend::Alpha {
                AlphaTest::CoverageBlend
            } else {
                AlphaTest::CoverageEdge
            }
        } else {
            AlphaTest::None
        };
        Self {
            pre,
            last,
            kind,
            alpha_test,
        }
    }

    /// WGSL of `fn blend(c, shade) -> vec4<f32>` (color written to the GPU
    /// blender; alpha is the blend factor for [`FinalBlend::Alpha`]).
    pub fn wgsl(&self, alpha_cvg_sel: bool) -> String {
        let mut s = String::from(
            "fn blend(c: vec4<f32>, shade: vec4<f32>) -> vec4<f32> {\n    var px = c.rgb;\n",
        );
        if let Some(pre) = self.pre {
            s += &pre.wgsl_full();
        }
        let l = &self.last;
        match self.kind {
            FinalBlend::Alpha => {
                // With ALPHA_CVG_SEL (and no CVG_X_ALPHA) A_IN is coverage.
                let a = if l.a == 0 && alpha_cvg_sel {
                    "1.0"
                } else {
                    BlendCycle::alpha(l.a)
                };
                s += &format!("    return vec4<f32>({}, {a});\n", BlendCycle::color(l.p));
            }
            FinalBlend::KeepDst => s += "    return vec4<f32>(px, 0.0);\n",
            FinalBlend::Opaque => {
                if l.reads_memory() {
                    // e.g. OPA_SURF: (IN, A_IN, MEM, A_MEM) at full coverage = IN.
                    s += &format!("    return vec4<f32>({}, 1.0);\n", BlendCycle::color(l.p));
                } else {
                    s += &l.wgsl_full();
                    s += "    return vec4<f32>(px, 1.0);\n";
                }
            }
        }
        s += "}\n";
        s
    }
}

/// N64 z-buffer word decompression (`z = mantissa << shift + offset` per
/// 3-bit exponent) → 18-bit z. Word = `z14 << 2 | dz`.
const Z_DECOMPRESS: [(u32, u32); 8] = [
    (6, 0x00000),
    (5, 0x20000),
    (4, 0x30000),
    (3, 0x38000),
    (2, 0x3C000),
    (1, 0x3E000),
    (0, 0x3F000),
    (0, 0x3F800),
];

/// A 16-bit value written into the z-buffer (a fill color or a "color" drawn
/// with the color image pointed at the z image) → the renderer's reversed
/// depth (1 = near, 0 = far). The game's viewport maps NDC z to the full
/// 18-bit range, so normalized N64 z = 1 - reversed depth.
pub fn zbuffer_word_to_depth(word: u16) -> f32 {
    let z14 = (word >> 2) as u32;
    let (shift, offset) = Z_DECOMPRESS[(z14 >> 11) as usize];
    let z18 = ((z14 & 0x7FF) << shift) + offset;
    1.0 - z18 as f32 / 0x3FFFF as f32
}

/// WGSL twin of [`zbuffer_word_to_depth`] taking the 16-bit framebuffer
/// color the blender would write (RGBA5551 of `c`).
pub const WGSL_Z_WORD: &str = r#"
fn zword_depth(c: vec4<f32>) -> f32 {
    // 8-bit color → 5 bits by truncation, like the RDP's 16-bit write (no dither).
    let q = vec3<u32>(round(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * 255.0)) >> vec3<u32>(3u);
    let word = (q.r << 11u) | (q.g << 6u) | (q.b << 1u) | select(0u, 1u, c.a >= 0.5);
    let z14 = word >> 2u;
    let e = z14 >> 11u;
    var shift = array<u32, 8>(6u, 5u, 4u, 3u, 2u, 1u, 0u, 0u);
    var offset = array<u32, 8>(0u, 0x20000u, 0x30000u, 0x38000u, 0x3C000u, 0x3E000u, 0x3F000u, 0x3F800u);
    let z18 = ((z14 & 0x7FFu) << shift[e]) + offset[e];
    return 1.0 - f32(z18) / f32(0x3FFFFu);
}
"#;

/// Depth state from the render mode + geometry mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DepthState {
    pub test: bool,
    pub write: bool,
    /// `ZMODE_DEC`: polygon offset toward the camera.
    pub decal: bool,
}

impl DepthState {
    pub fn new(l: u32, zbuffer_geom: bool) -> Self {
        Self {
            test: zbuffer_geom && l & oml::Z_CMP != 0,
            write: zbuffer_geom && l & oml::Z_UPD != 0,
            decal: (l >> oml::ZMODE_SHIFT) & 3 == 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // gbi.h render modes, (cycle-1 macro, cycle-2 macro) combined.
    const G_RM_FOG_SHADE_A: u32 = 0xC800_0000;
    const G_RM_PASS: u32 = 0x0C08_0000;
    const RM_AA_ZB_XLU_SURF2: u32 = 0x0010_49D8;
    const RM_AA_ZB_OPA_SURF2: u32 = 0x0011_2078;

    #[test]
    fn fog_then_translucent() {
        let s = BlendState::new(CycleType::Two, G_RM_FOG_SHADE_A | RM_AA_ZB_XLU_SURF2);
        assert_eq!(s.kind, FinalBlend::Alpha);
        let pre = s.pre.unwrap();
        assert_eq!((pre.p, pre.a, pre.m, pre.b), (3, 2, 0, 0));
        let src = s.wgsl(false);
        assert!(
            src.contains("u.fog.rgb * shade.a + px * (1.0 - shade.a)"),
            "{src}"
        );
        assert!(src.contains("return vec4<f32>(px, c.a)"), "{src}");
    }

    #[test]
    fn opaque_surface_and_pass() {
        let s = BlendState::new(CycleType::Two, G_RM_PASS | RM_AA_ZB_OPA_SURF2);
        assert_eq!(s.kind, FinalBlend::Opaque);
        // G_RM_PASS (IN, 0, IN, 1) is a no-op but still valid WGSL.
        assert!(s.wgsl(true).contains("return vec4<f32>(px, 1.0)"));
        let d = DepthState::new(RM_AA_ZB_OPA_SURF2, true);
        assert!(d.test && d.write && !d.decal);
        assert!(!DepthState::new(RM_AA_ZB_OPA_SURF2, false).test);
    }

    #[test]
    fn zbuffer_words() {
        // uvGfx_80222A98's clear value GPACK_RGBA5551(255,255,240,0) = far.
        assert_eq!(zbuffer_word_to_depth(0xFFFC), 0.0);
        // Black (the shadow-volume pass) = nearest.
        assert_eq!(zbuffer_word_to_depth(0x0001), 1.0);
        // Monotonic: larger words are farther.
        let d: Vec<f32> = (0..=0xFFFFu16)
            .step_by(4)
            .map(zbuffer_word_to_depth)
            .collect();
        assert!(d.windows(2).all(|w| w[1] <= w[0]));
    }

    #[test]
    fn tex_terr_blends_on_alpha_coverage() {
        // RM_AA_ZB_TEX_TERR(2): AA_EN|Z_CMP|Z_UPD|IM_RD|CVG_DST_CLAMP|
        // CVG_X_ALPHA|ALPHA_CVG_SEL|ZMODE_OPA|TEX_EDGE, (IN, A_IN, MEM, 1MA).
        let l = 0x8 | 0x10 | 0x20 | 0x40 | 0x1000 | 0x2000 | (1 << 20);
        let s = BlendState::new(CycleType::Two, G_RM_PASS | l);
        assert_eq!(s.kind, FinalBlend::Alpha);
        assert_eq!(s.alpha_test, AlphaTest::CoverageBlend);
    }
}
