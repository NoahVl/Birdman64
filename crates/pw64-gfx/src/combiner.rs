//! RDP color combiner → WGSL.
//!
//! The combiner computes `(A - B) * C + D` per cycle, separately for RGB
//! and alpha. A [`CombinerKey`] is the raw 56-bit `G_SETCOMBINE` mux plus
//! the cycle type; [`CombinerKey::wgsl`] turns it into a WGSL function
//! `combine(t0, t1, shade, noise, lod_frac) -> vec4<f32>` that reads the per-draw
//! uniforms `u.prim`, `u.env` and `u.lod`. Shaders are cached per key by
//! the renderer.
//!
//! Hardware facts relied upon (N64 programming manual §12.7, and common HLE
//! practice):
//! - 1-cycle mode runs only the *second* cycle's settings (the standard
//!   `gsDPSetCombineMode(X, X)` idiom makes the choice moot).
//! - In the second cycle of 2-cycle mode, `TEXEL0`/`TEXEL1` swap (the texel
//!   pipeline is one stage ahead), and `COMBINED` is the first cycle's output.
//! - Each cycle's result is clamped to 0..1.

/// Raw `G_SETCOMBINE` mux + cycle type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CombinerKey {
    /// `(w0 & 0xFFFFFF) << 32 | w1`.
    pub mux: u64,
    pub two_cycle: bool,
}

/// One cycle's selectors: `rgb`/`alpha` = `[a, b, c, d]` raw field values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cycle {
    pub rgb: [u8; 4],
    pub alpha: [u8; 4],
}

fn bits(v: u32, shift: u32, len: u32) -> u8 {
    ((v >> shift) & ((1 << len) - 1)) as u8
}

impl CombinerKey {
    pub fn new(w0: u32, w1: u32, two_cycle: bool) -> Self {
        Self {
            mux: ((w0 as u64 & 0xFF_FFFF) << 32) | w1 as u64,
            two_cycle,
        }
    }

    /// The two cycles as stored (field layout of `GCCc0w0` ... `GCCc1w1`).
    pub fn cycles(&self) -> [Cycle; 2] {
        let w0 = (self.mux >> 32) as u32;
        let w1 = self.mux as u32;
        [
            Cycle {
                rgb: [
                    bits(w0, 20, 4),
                    bits(w1, 28, 4),
                    bits(w0, 15, 5),
                    bits(w1, 15, 3),
                ],
                alpha: [
                    bits(w0, 12, 3),
                    bits(w1, 12, 3),
                    bits(w0, 9, 3),
                    bits(w1, 9, 3),
                ],
            },
            Cycle {
                rgb: [
                    bits(w0, 5, 4),
                    bits(w1, 24, 4),
                    bits(w0, 0, 5),
                    bits(w1, 6, 3),
                ],
                alpha: [
                    bits(w1, 21, 3),
                    bits(w1, 3, 3),
                    bits(w1, 18, 3),
                    bits(w1, 0, 3),
                ],
            },
        ]
    }

    /// Inverse of [`Self::cycles`] (for tests and synthetic lists): the
    /// `G_SETCOMBINE` words.
    pub fn encode(c: [Cycle; 2]) -> (u32, u32) {
        let s = |v: u8, sh: u32| (v as u32) << sh;
        let w0 = (0xFC << 24)
            | s(c[0].rgb[0], 20)
            | s(c[0].rgb[2], 15)
            | s(c[0].alpha[0], 12)
            | s(c[0].alpha[2], 9)
            | s(c[1].rgb[0], 5)
            | s(c[1].rgb[2], 0);
        let w1 = s(c[0].rgb[1], 28)
            | s(c[1].rgb[1], 24)
            | s(c[1].alpha[0], 21)
            | s(c[1].alpha[2], 18)
            | s(c[0].rgb[3], 15)
            | s(c[0].alpha[1], 12)
            | s(c[0].alpha[3], 9)
            | s(c[1].rgb[3], 6)
            | s(c[1].alpha[1], 3)
            | s(c[1].alpha[3], 0);
        (w0, w1)
    }

    /// The cycles that run, with a flag for "second cycle" (texel swap).
    fn active(&self) -> Vec<(Cycle, bool)> {
        let [c0, c1] = self.cycles();
        if self.two_cycle {
            vec![(c0, false), (c1, true)]
        } else {
            vec![(c1, false)]
        }
    }

    /// Whether TEXEL0 / TEXEL1 are read (after the second-cycle swap).
    /// Allocation-free twin of scanning [`color_input`]/[`alpha_input`]
    /// for `t0`/`t1` (the interpreter calls it on every state change; the
    /// equivalence is tested exhaustively).
    pub fn uses_texel(&self) -> [bool; 2] {
        let [c0, c1] = self.cycles();
        let mut used = [false; 2];
        let mut run = |c: Cycle, second: bool| {
            // Selector reading texel `n` (0/1) before the second-cycle swap.
            let mut mark = |n: u8| used[(texel(n, second) == "t1") as usize] = true;
            for (slot, sel) in c.rgb.into_iter().enumerate() {
                match (slot, sel) {
                    (_, 1) | (2, 8) => mark(0),
                    (_, 2) | (2, 9) => mark(1),
                    _ => {}
                }
            }
            for (slot, sel) in c.alpha.into_iter().enumerate() {
                match (slot, sel) {
                    (2, 0) | (2, 6) => {}
                    (_, 1) => mark(0),
                    (_, 2) => mark(1),
                    _ => {}
                }
            }
        };
        if self.two_cycle {
            run(c0, false);
            run(c1, true);
        } else {
            run(c1, false);
        }
        used
    }

    /// Whether the generated code reads LOD_FRACTION (RGB C 13, alpha C 0 —
    /// unless `formula` drops the product because A and B are the same
    /// input, e.g. `(0, 0, 0, SHADE)` alphas). Allocation-free twin of
    /// scanning the WGSL for `lf` (called per template, like `uses_texel`).
    pub fn uses_lod_frac(&self) -> bool {
        let [c0, c1] = self.cycles();
        // Identity of the A / B RGB inputs (`color_input`): 0..=5 shared,
        // then A 6 = 1, A 7 = noise, B 7 = K4, everything else zero (255).
        let rgb_a = |s: u8| match s {
            0..=7 => s,
            _ => 255,
        };
        let rgb_b = |s: u8| match s {
            0..=5 => s,
            7 => 107,
            _ => 255,
        };
        let reads = |c: Cycle| {
            (c.rgb[2] == 13 && rgb_a(c.rgb[0]) != rgb_b(c.rgb[1]))
                || (c.alpha[2] == 0 && c.alpha[0] != c.alpha[1])
        };
        if self.two_cycle {
            reads(c0) || reads(c1)
        } else {
            reads(c1)
        }
    }

    /// Whether the shade color (vertex color / fog alpha) is read.
    pub fn uses_shade(&self) -> bool {
        self.wgsl().contains("shade")
    }

    /// WGSL source of `fn combine(t0, t1, shade, nz, lf) -> vec4<f32>` (`lf` =
    /// LOD_FRACTION, computed per pixel by the fragment prologue).
    pub fn wgsl(&self) -> String {
        let mut s = String::from(
            "fn combine(t0: vec4<f32>, t1: vec4<f32>, shade: vec4<f32>, nz: f32, lf: f32) -> vec4<f32> {\n    var comb = vec4<f32>(0.0);\n",
        );
        for (c, second) in self.active() {
            let rgb = formula(
                c.rgb.map_index(|i, v| color_input(i, v, second)),
                "vec3<f32>(0.0)",
            );
            let a = formula(c.alpha.map_index(|i, v| alpha_input(i, v, second)), "0.0");
            s += &format!(
                "    comb = clamp(vec4<f32>({rgb}, {a}), vec4<f32>(0.0), vec4<f32>(1.0));\n"
            );
        }
        s += "    return comb;\n}\n";
        s
    }
}

trait MapIndex {
    fn map_index(self, f: impl Fn(usize, u8) -> String) -> [String; 4];
}

impl MapIndex for [u8; 4] {
    fn map_index(self, f: impl Fn(usize, u8) -> String) -> [String; 4] {
        [0, 1, 2, 3].map(|i| f(i, self[i]))
    }
}

/// `(a - b) * c + d` with trivial simplifications; empty string = zero.
fn formula([a, b, c, d]: [String; 4], zero: &str) -> String {
    let mul = if a == b || c.is_empty() || (a.is_empty() && b.is_empty()) {
        String::new()
    } else {
        let diff = match (a.is_empty(), b.is_empty()) {
            (false, true) => a,
            (true, false) => format!("(-{b})"),
            _ => format!("({a} - {b})"),
        };
        if c == "1.0" || c == "vec3<f32>(1.0)" {
            diff
        } else {
            format!("{diff} * {c}")
        }
    };
    match (mul.is_empty(), d.is_empty()) {
        (true, true) => zero.to_string(),
        (true, false) => d,
        (false, true) => mul,
        (false, false) => format!("{mul} + {d}"),
    }
}

/// Texel names after the second-cycle swap.
fn texel(n: u8, second: bool) -> &'static str {
    match (n, second) {
        (0, false) | (1, true) => "t0",
        _ => "t1",
    }
}

/// RGB selector `sel` for slot `slot` (0 = A, 1 = B, 2 = C, 3 = D).
fn color_input(slot: usize, sel: u8, second: bool) -> String {
    let common = |sel: u8| -> Option<String> {
        Some(match sel {
            0 => "comb.rgb".into(),
            1 => format!("{}.rgb", texel(0, second)),
            2 => format!("{}.rgb", texel(1, second)),
            3 => "u.prim.rgb".into(),
            4 => "shade.rgb".into(),
            5 => "u.env.rgb".into(),
            _ => return None,
        })
    };
    if let Some(e) = common(sel) {
        return e;
    }
    let v3 = |s: &str| format!("vec3<f32>({s})");
    match (slot, sel) {
        (0, 6) | (3, 6) => v3("1.0"),
        (0, 7) => v3("nz"),
        // B: CENTER (chroma key) is unsupported → 0; K4.
        (1, 7) => v3("u.lod.z"),
        (2, 7) => v3("comb.a"),
        (2, 8) => v3(&format!("{}.a", texel(0, second))),
        (2, 9) => v3(&format!("{}.a", texel(1, second))),
        (2, 10) => v3("u.prim.a"),
        (2, 11) => v3("shade.a"),
        (2, 12) => v3("u.env.a"),
        (2, 13) => v3("lf"),
        (2, 14) => v3("u.lod.x"),
        (2, 15) => v3("u.lod.w"),
        // C: SCALE (chroma key) unsupported → 0. Everything else is 0.
        _ => String::new(),
    }
}

/// Alpha selector `sel` for slot `slot`.
fn alpha_input(slot: usize, sel: u8, second: bool) -> String {
    match (slot, sel) {
        (2, 0) => "lf".into(),
        (2, 6) => "u.lod.x".into(),
        (_, 0) => "comb.a".into(),
        (_, 1) => format!("{}.a", texel(0, second)),
        (_, 2) => format!("{}.a", texel(1, second)),
        (_, 3) => "u.prim.a".into(),
        (_, 4) => "shade.a".into(),
        (_, 5) => "u.env.a".into(),
        (_, 6) => "1.0".into(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Selector values from gbi.h (G_CCMUX_* / G_ACMUX_*).
    const COMBINED: u8 = 0;
    const TEXEL0: u8 = 1;
    const TEXEL1: u8 = 2;
    const SHADE: u8 = 4;
    const LOD_FRACTION: u8 = 13;
    const ZERO_A: u8 = 15;
    const ZERO_C: u8 = 31;
    const ZERO_D: u8 = 7;

    /// `G_CC_SHADE` for RGB and alpha.
    const CC_SHADE: Cycle = Cycle {
        rgb: [ZERO_A, ZERO_A, ZERO_C, SHADE],
        alpha: [7, 7, 7, SHADE],
    };

    #[test]
    fn decodes_mipmap_lerp_combiner() {
        // gsDPSetCombineLERP(TEXEL1, TEXEL0, LOD_FRACTION, TEXEL0, <same alpha>,
        //                    COMBINED, 0, SHADE, 0, <same alpha>): the most
        // common UVTX combiner (0xFC26A004 0x1F1093FF).
        let key = CombinerKey::new(0xFC26_A004, 0x1F10_93FF, true);
        let [c0, c1] = key.cycles();
        assert_eq!(c0.rgb, [TEXEL1, TEXEL0, LOD_FRACTION, TEXEL0]);
        assert_eq!(c0.alpha, [TEXEL1, TEXEL0, 0, TEXEL0]);
        assert_eq!(c1.rgb, [COMBINED, 15, SHADE, ZERO_D]);
        assert_eq!(c1.alpha, [COMBINED, 7, SHADE, 7]);
        assert_eq!(CombinerKey::encode([c0, c1]), (0xFC26_A004, 0x1F10_93FF));
        let src = key.wgsl();
        assert!(
            src.contains("(t1.rgb - t0.rgb) * vec3<f32>(lf) + t0.rgb"),
            "{src}"
        );
        assert!(src.contains("comb.rgb * shade.rgb"), "{src}");
        assert_eq!(key.uses_texel(), [true, true]);
        assert!(key.uses_shade());
    }

    #[test]
    fn one_cycle_uses_second_cycle_settings() {
        let tex = Cycle {
            rgb: [ZERO_A, ZERO_A, ZERO_C, TEXEL0],
            alpha: [7, 7, 7, TEXEL0],
        };
        let (w0, w1) = CombinerKey::encode([tex, CC_SHADE]);
        let key = CombinerKey::new(w0, w1, false);
        let src = key.wgsl();
        assert_eq!(src.matches("comb = clamp").count(), 1);
        assert!(src.contains("vec4<f32>(shade.rgb, shade.a)"), "{src}");
        assert_eq!(key.uses_texel(), [false, false]);
        // The same mux in 2-cycle mode reads TEXEL0 in the first cycle.
        assert_eq!(CombinerKey::new(w0, w1, true).uses_texel(), [true, false]);
    }

    #[test]
    fn second_cycle_swaps_texels() {
        let t0 = Cycle {
            rgb: [ZERO_A, ZERO_A, ZERO_C, TEXEL0],
            alpha: [7, 7, 7, TEXEL0],
        };
        let (w0, w1) = CombinerKey::encode([CC_SHADE, t0]);
        let key = CombinerKey::new(w0, w1, true);
        assert_eq!(key.uses_texel(), [false, true]);
        assert!(key.wgsl().contains("vec4<f32>(t1.rgb, t1.a)"));
    }

    /// `uses_texel` equals scanning the generated input expressions, for
    /// every selector value in every slot, cycle and cycle mode.
    #[test]
    fn uses_texel_matches_expressions() {
        let by_strings = |key: &CombinerKey| {
            let mut used = [false; 2];
            for (c, second) in key.active() {
                let exprs = (0..4)
                    .map(|i| color_input(i, c.rgb[i], second))
                    .chain((0..4).map(|i| alpha_input(i, c.alpha[i], second)));
                for e in exprs {
                    used[0] |= e.contains("t0");
                    used[1] |= e.contains("t1");
                }
            }
            used
        };
        let base = Cycle {
            rgb: [ZERO_A, ZERO_A, ZERO_C, ZERO_D],
            alpha: [7, 7, 7, 7],
        };
        for cycle in 0..2 {
            for slot in 0..8 {
                for sel in 0..32u8 {
                    let mut c = [base, base];
                    if slot < 4 {
                        c[cycle].rgb[slot] = sel & [15, 15, 31, 7][slot];
                    } else {
                        c[cycle].alpha[slot - 4] = sel & 7;
                    }
                    let (w0, w1) = CombinerKey::encode(c);
                    for two in [false, true] {
                        let k = CombinerKey::new(w0, w1, two);
                        assert_eq!(k.uses_texel(), by_strings(&k), "{c:?} two={two}");
                    }
                }
            }
        }
    }

    /// `uses_lod_frac` equals "the generated WGSL reads `lf`", for every
    /// (A, B) pair with C = LOD_FRACTION in both halves, cycles and modes.
    #[test]
    fn uses_lod_frac_matches_wgsl() {
        let base = Cycle {
            rgb: [ZERO_A, ZERO_A, ZERO_C, ZERO_D],
            alpha: [7, 7, 7, 7],
        };
        // Body only (the signature names `lf` too).
        let reads_lf = |k: &CombinerKey| {
            let src = k.wgsl();
            let body = &src[src.find('{').unwrap()..];
            body.contains("* lf") || body.contains("* vec3<f32>(lf)")
        };
        for cycle in 0..2 {
            for a in 0..16u8 {
                for b in 0..16u8 {
                    let mut rgb = [base, base];
                    rgb[cycle].rgb = [a, b, 13, ZERO_D];
                    let mut alpha = [base, base];
                    alpha[cycle].alpha = [a & 7, b & 7, 0, 7];
                    for c in [rgb, alpha] {
                        let (w0, w1) = CombinerKey::encode(c);
                        for two in [false, true] {
                            let k = CombinerKey::new(w0, w1, two);
                            assert_eq!(k.uses_lod_frac(), reads_lf(&k), "{c:?} two={two}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn all_zero_combiner_is_valid() {
        let z = Cycle {
            rgb: [ZERO_A, ZERO_A, ZERO_C, ZERO_D],
            alpha: [7, 7, 7, 7],
        };
        let (w0, w1) = CombinerKey::encode([z, z]);
        let src = CombinerKey::new(w0, w1, true).wgsl();
        assert!(src.contains("vec4<f32>(vec3<f32>(0.0), 0.0)"), "{src}");
    }
}
