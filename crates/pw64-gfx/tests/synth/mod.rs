//! Synthetic Fast3D workload shaped like an in-flight Pilotwings 64 frame
//! (renderer.md "144 Hz profiling": ~9 k commands, ~2.7 k triangles,
//! ~600 `G_VTX`, ~180 texture loads, heavy SETTILE/SETTILESIZE/othermode
//! churn), plus a [`Frame`] hash for output-identity checks. Shared by
//! `tests/interp_identity.rs` and `examples/bench_interp.rs`; all data is
//! generated (no ROM content).

#![allow(dead_code)]

use pw64_gfx::frame::Frame;
use pw64_gfx::matrix::{self, Mat4};
use pw64_gfx::memory::VecMemory;

/// Deterministic xorshift64*.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u32) -> u32 {
        (self.next() >> 33) as u32 % n
    }
    pub fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + self.below((hi - lo) as u32) as i32
    }
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// A texture stored in the synthetic RDRAM.
#[derive(Clone, Copy)]
pub struct Tex {
    pub addr: u32,
    /// `G_IM_FMT_*`, `G_IM_SIZ_*`.
    pub fmt: u32,
    pub siz: u32,
    pub w: u32,
    pub h: u32,
    /// Load with LOADTILE instead of LOADBLOCK.
    pub tile_load: bool,
    /// CI: TLUT address (16 entries).
    pub tlut: Option<u32>,
}

impl Tex {
    fn bits(&self) -> u32 {
        4 << self.siz
    }
    fn bytes(&self) -> u32 {
        self.w * self.h * self.bits() / 8
    }
    /// TMEM line in 64-bit words.
    fn line(&self) -> u32 {
        (self.w * self.bits()).div_ceil(64)
    }
}

pub struct Workload {
    pub mem: VecMemory,
    pub dl: u32,
    pub textures: Vec<Tex>,
}

fn cmd(op: u32, lo: u32) -> u32 {
    (op << 24) | (lo & 0x00FF_FFFF)
}

fn perspective() -> Mat4 {
    let (n, f) = (1.0f32, 5000.0f32);
    let fy = 1.0 / (30f32.to_radians()).tan();
    [
        [fy * 0.75, 0.0, 0.0, 0.0],
        [0.0, fy, 0.0, 0.0],
        [0.0, 0.0, (n + f) / (n - f), -1.0],
        [0.0, 0.0, 2.0 * n * f / (n - f), 0.0],
    ]
}

fn model(r: &mut Rng) -> Mat4 {
    let a = r.below(628) as f32 / 100.0;
    let (s, c) = a.sin_cos();
    let k = 0.5 + r.below(100) as f32 / 100.0;
    [
        [c * k, 0.0, -s * k, 0.0],
        [0.0, k, 0.0, 0.0],
        [s * k, 0.0, c * k, 0.0],
        [
            r.range(-300, 300) as f32,
            r.range(-100, 100) as f32,
            -(r.range(80, 1500) as f32),
            1.0,
        ],
    ]
}

/// SETTILE words.
#[allow(clippy::too_many_arguments)]
fn settile(
    fmt: u32,
    siz: u32,
    line: u32,
    tmem: u32,
    tile: u32,
    pal: u32,
    cm: u32,
    mask: u32,
) -> (u32, u32) {
    (
        cmd(0xF5, (fmt << 21) | (siz << 19) | (line << 9) | tmem),
        (tile << 24) | (pal << 20) | (cm << 18) | (mask << 14) | (cm << 8) | (mask << 4),
    )
}

fn tilesize(tile: u32, w: u32, h: u32) -> (u32, u32) {
    (
        cmd(0xF2, 0),
        (tile << 24) | ((w - 1) << 14) | ((h - 1) << 2),
    )
}

fn log2(v: u32) -> u32 {
    31 - v.leading_zeros()
}

/// Emits the load sequence for `t` at TMEM word `tmem` and the render tile
/// `tile` (mirrors `gDPLoadTextureBlock` / `gDPLoadTextureTile`).
fn load(out: &mut Vec<(u32, u32)>, t: &Tex, tmem: u32, tile: u32) {
    if let Some(tlut) = t.tlut {
        out.push((cmd(0xFD, 2 << 19), tlut)); // RGBA16 image
        out.push((cmd(0xF5, 256), 7 << 24)); // tile 7 at TMEM 256
        out.push((cmd(0xE6, 0), 0));
        out.push((cmd(0xF0, 0), (7 << 24) | (15 << 14)));
        out.push((cmd(0xE7, 0), 0));
    }
    let (w, h) = (t.w, t.h);
    if t.tile_load {
        out.push((cmd(0xFD, (t.fmt << 21) | (t.siz << 19) | (w - 1)), t.addr));
        out.push(settile(t.fmt, t.siz, t.line(), tmem, 7, 0, 0, 0));
        out.push((cmd(0xE6, 0), 0));
        out.push((cmd(0xF4, 0), (7 << 24) | ((w - 1) << 14) | ((h - 1) << 2)));
    } else {
        // LoadBlock of 16-bit units (4/8-bit loaded as 16-bit, like the macro).
        let (lsiz, texels) = if t.siz < 2 {
            (2, t.bytes() / 2)
        } else {
            (t.siz, w * h)
        };
        let words_per_line = t.line().max(1);
        let dxt = 2048u32.div_ceil(words_per_line);
        out.push((cmd(0xFD, (t.fmt << 21) | (lsiz << 19)), t.addr));
        out.push(settile(t.fmt, lsiz, 0, tmem, 7, 0, 0, 0));
        out.push((cmd(0xE6, 0), 0));
        out.push((
            cmd(0xF3, 0),
            (7 << 24) | ((texels.min(2048) - 1) << 12) | dxt,
        ));
    }
    out.push((cmd(0xE7, 0), 0));
    out.push(settile(
        t.fmt,
        t.siz,
        t.line(),
        tmem,
        tile,
        0,
        0,
        log2(w).min(log2(h)),
    ));
    out.push(tilesize(tile, w, h));
}

/// The frame. `seed` varies content (textures, vertices, state).
pub fn build(seed: u64) -> Workload {
    let mut r = Rng::new(seed);
    let mut mem = VecMemory::default();
    mem.push(&[0; 64], 8); // keep address 0 unused

    // Textures (~48, 1–4 KiB each, several formats and both load kinds).
    let kinds: [(u32, u32, u32, u32, bool, bool); 7] = [
        (0, 2, 32, 32, false, false), // RGBA16 32x32 block
        (2, 0, 64, 64, false, true),  // CI4 64x64 + TLUT
        (3, 1, 32, 32, false, false), // IA8
        (4, 0, 64, 32, false, false), // I4
        (0, 2, 32, 32, true, false),  // RGBA16 via LoadTile
        (0, 3, 16, 16, false, false), // RGBA32
        (3, 2, 32, 16, true, false),  // IA16 via LoadTile
    ];
    let mut textures = Vec::new();
    for i in 0..48 {
        let (fmt, siz, w, h, tile_load, ci) = kinds[i % kinds.len()];
        let mut t = Tex {
            addr: 0,
            fmt,
            siz,
            w,
            h,
            tile_load,
            tlut: None,
        };
        t.addr = mem.push(&r.bytes(t.bytes() as usize), 8);
        if ci {
            t.tlut = Some(mem.push(&r.bytes(32), 8));
        }
        textures.push(t);
    }
    let font = Tex {
        addr: mem.push(&r.bytes(64 * 32 / 2), 8),
        fmt: 4,
        siz: 0,
        w: 64,
        h: 32,
        tile_load: false,
        tlut: None,
    };

    // Matrices, lights, viewport, vertex buffers.
    let proj = mem.push(&matrix::to_fixed(&perspective()), 8);
    let ortho = {
        let mut m = matrix::IDENTITY;
        m[0][0] = 2.0 / 320.0;
        m[1][1] = 2.0 / 240.0;
        m[3] = [-1.0, -1.0, 0.0, 1.0];
        mem.push(&matrix::to_fixed(&m), 8)
    };
    let models: Vec<u32> = (0..96)
        .map(|_| {
            let m = model(&mut r);
            mem.push(&matrix::to_fixed(&m), 8)
        })
        .collect();
    let mut vp = Vec::new();
    for v in [640i16, -480, 511, 0, 640, 480, 511, 0] {
        vp.extend_from_slice(&v.to_be_bytes());
    }
    let vp = mem.push(&vp, 8);
    // Light_t: col, pad, colc, pad, dir[3], pad (+ 8 pad); ambient after.
    let light = mem.push(
        &[
            230, 220, 200, 0, 230, 220, 200, 0, 40, 90, 30, 0, 0, 0, 0, 0,
        ],
        8,
    );
    let ambient = mem.push(&[60, 60, 70, 0, 60, 60, 70, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8);
    let vbufs: Vec<u32> = (0..160)
        .map(|_| {
            let mut b = Vec::with_capacity(256);
            for _ in 0..16 {
                for v in [r.range(-60, 60), r.range(-60, 60), r.range(-60, 60)] {
                    b.extend_from_slice(&(v as i16).to_be_bytes());
                }
                b.extend_from_slice(&0i16.to_be_bytes());
                b.extend_from_slice(&(r.range(0, 2048) as i16).to_be_bytes());
                b.extend_from_slice(&(r.range(0, 2048) as i16).to_be_bytes());
                b.extend_from_slice(&r.bytes(4));
            }
            mem.push(&b, 8)
        })
        .collect();

    // Combiners: shade, texture × shade, 2-cycle mip lerp, decal.
    let combiners = [
        (0xFCFF_FFFF, 0xFFFE_793C),
        (0xFC12_1824, 0xFF33_FFFF),
        (0xFC26_A004, 0x1F10_93FF),
        (0xFC12_7E24, 0xFFFF_F3F9),
    ];
    let render_modes = [0x0055_2078, 0x0055_2D58, 0x0C18_4DD8, 0x0050_4240];

    // One sub-list per material (like the engine's model DLs).
    let mut materials = Vec::new();
    for m in 0..300u32 {
        let mut c: Vec<(u32, u32)> = Vec::new();
        let two = r.below(3) == 0;
        c.push((cmd(0xE7, 0), 0));
        c.push((cmd(0xBA, (20 << 8) | 2), if two { 1 << 20 } else { 0 }));
        c.push((
            cmd(0xBA, (12 << 8) | 2),
            if r.below(4) == 0 { 0 } else { 2 << 12 },
        ));
        c.push((cmd(0xB9, (3 << 8) | 29), render_modes[r.below(4) as usize]));
        if m % 4 == 0 {
            c.push((cmd(0xB9, 2), 0)); // alpha compare
        }
        let comb = if two {
            combiners[2]
        } else {
            combiners[r.below(4) as usize]
        };
        c.push(comb);
        c.push((cmd(0xB6, 0), 0xFFFF_FFFF));
        let mut gm = 0x0000_2205; // zbuffer, shade, smooth, cull back
        if r.below(3) == 0 {
            gm |= 0x0002_0000; // lighting
        }
        if r.below(2) == 0 {
            gm |= 0x0001_0000; // fog
        }
        c.push((cmd(0xB7, 0), gm));
        c.push((cmd(0xBB, (1 << 11) | 1), 0xFFFF_FFFF));
        let t = textures[r.below(textures.len() as u32) as usize];
        let loads = m % 17 < 10; // ~59 % of materials load (≈ 177 of 300)
        c.push((
            cmd(0xBA, (14 << 8) | 2),
            if t.tlut.is_some() { 2 << 14 } else { 0 },
        ));
        if loads {
            load(&mut c, &t, 0, 0);
            if two {
                let t2 = textures[r.below(textures.len() as u32) as usize];
                if t2.tlut.is_none() && t2.bytes() <= 2048 {
                    load(&mut c, &t2, 256, 1);
                } else {
                    c.push(settile(t.fmt, t.siz, t.line(), 0, 1, 0, 0, 4));
                    c.push(tilesize(1, t.w / 2, t.h / 2));
                }
            }
        } else {
            // State re-set without a load (same TMEM contents).
            c.push(settile(t.fmt, t.siz, t.line(), 0, 0, 0, 0, 5));
            c.push(tilesize(0, t.w, t.h));
            c.push(settile(t.fmt, t.siz, t.line(), 0, 1, 0, 0, 4));
            c.push(tilesize(1, t.w / 2, t.h / 2));
        }
        if r.below(3) == 0 {
            c.push((cmd(0xFA, 0), r.next() as u32));
        }
        if r.below(4) == 0 {
            c.push((cmd(0xFB, 0), r.next() as u32));
        }
        c.push((
            cmd(0x01, (0x04 << 16) | 64),
            models[r.below(models.len() as u32) as usize],
        ));
        for _ in 0..2 {
            let n = 8 + r.below(9);
            let vb = vbufs[r.below(vbufs.len() as u32) as usize];
            c.push((cmd(0x04, ((n - 1) << 20) | (n * 16)), vb));
            for _ in 0..(4 + r.below(2)) {
                let v = [r.below(n), r.below(n), r.below(n)];
                c.push((
                    cmd(0xBF, 0),
                    ((v[0] * 10) << 16) | ((v[1] * 10) << 8) | (v[2] * 10),
                ));
            }
        }
        c.push((cmd(0xBD, 0), 0));
        c.push((cmd(0xB8, 0), 0));
        let words: Vec<u32> = c.iter().flat_map(|&(a, b)| [a, b]).collect();
        materials.push(mem.push_words(&words));
    }

    // Main list: setup, clears, world, HUD.
    let (cimg, zimg) = (0x0010_0000, 0x0020_0000);
    let mut c: Vec<(u32, u32)> = vec![
        (cmd(0xBC, 0x0006), 0),
        (cmd(0xE9, 0), 0),
        (cmd(0xED, 0), (320 << 14) | (240 << 2)),
        // Depth clear: fill mode into the z image.
        (cmd(0xBA, (20 << 8) | 2), 3 << 20),
        (cmd(0xFF, 0x10_013F), zimg),
        (cmd(0xF7, 0), 0xFFFC_FFFC),
        (cmd(0xF6, (319 << 14) | (239 << 2)), 0),
        (cmd(0xE7, 0), 0),
        (cmd(0xFF, 0x10_013F), cimg),
        (cmd(0xFE, 0), zimg),
        (cmd(0xF7, 0), 0x0001_0001),
        (cmd(0xF6, (319 << 14) | (239 << 2)), 0),
        (cmd(0xE7, 0), 0),
        (cmd(0x03, (0x80 << 16) | 16), vp),
        (cmd(0x01, (0x03 << 16) | 64), proj),
        (cmd(0x01, (0x02 << 16) | 64), models[0]),
        (cmd(0xBC, 0x0002), 0x8000_0040), // one light
        (cmd(0x03, (0x86 << 16) | 16), light),
        (cmd(0x03, (0x88 << 16) | 16), ambient),
        (cmd(0xBC, 0x0008), 0x1900_E000), // fog mul/off
        (cmd(0xF8, 0), 0x8090_A0FF),
        (cmd(0xF9, 0), 0x0000_0080),
        (cmd(0xC0, 0), 0x5057_5731), // world-view bracket
    ];
    for &m in &materials {
        c.push((cmd(0x06, 0), m));
    }
    c.push((cmd(0xC0, 0), 0x5057_5730));

    // HUD: 1-cycle texrects from a font sheet, a few fill rects.
    c.push((cmd(0x01, (0x03 << 16) | 64), ortho));
    c.push((cmd(0xE7, 0), 0));
    c.push((cmd(0xBA, (20 << 8) | 2), 0));
    c.push((cmd(0xBA, (12 << 8) | 2), 0));
    c.push((cmd(0xB9, (3 << 8) | 29), 0x0050_4240));
    c.push(combiners[1]);
    load(&mut c, &font, 0, 0);
    for i in 0..24u32 {
        let (x, y) = (8 + (i % 12) * 24, 200 + (i / 12) * 16);
        c.push((
            cmd(0xE4, ((x + 8) << 14) | ((y + 8) << 2)),
            (x << 14) | (y << 2),
        ));
        c.push((cmd(0xB4, 0), ((i % 8) * 8 * 32) << 16 | ((i / 8) * 8 * 32)));
        c.push((cmd(0xB3, 0), 0x0400_0400));
        if i % 6 == 5 {
            c.push((cmd(0xE7, 0), 0));
            c.push((cmd(0xFA, 0), r.next() as u32));
        }
    }
    c.push((cmd(0xBA, (20 << 8) | 2), 3 << 20));
    for i in 0..4u32 {
        c.push((cmd(0xF7, 0), r.next() as u32));
        c.push((
            cmd(0xF6, ((20 + i * 70) << 14) | (30 << 2)),
            ((10 + i * 70) << 14) | (10 << 2),
        ));
    }
    c.push((cmd(0xE9, 0), 0));
    c.push((cmd(0xB8, 0), 0));
    let words: Vec<u32> = c.iter().flat_map(|&(a, b)| [a, b]).collect();
    let dl = mem.push_words(&words);
    Workload { mem, dl, textures }
}

struct Fnv(u64);

impl Fnv {
    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 = (self.0 ^ x as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

/// Hash of everything the renderer consumes: vertices (bit-exact), draws
/// (`Debug`, which prints floats round-trip exactly), textures by key.
pub fn frame_hash(f: &Frame) -> u64 {
    frame_hash_with(f, |s| s)
}

/// [`frame_hash`] as the pre-LOD interpreter would give it: the draw
/// `Debug` text without the `lodp`/`filt` uniforms added for the per-pixel
/// LOD fraction and the 3-point filter (everything else must be as before).
pub fn frame_hash_pre_lod(f: &Frame) -> u64 {
    frame_hash_with(f, |mut s| {
        for field in [", lodp: [", ", filt: ["] {
            if let Some(a) = s.find(field) {
                let b = a + s[a..].find(']').unwrap() + 1;
                s.replace_range(a..b, "");
            }
        }
        s
    })
}

/// Strips the fill view's placement fields (`stretch_y`, default `vanchor`)
/// and the OLED-care flag (default `hud`) from a draw's `Debug` text: like
/// `lodp`/`filt` they are renderer-side placement inputs, not interpreter
/// output, so the identity constants stay.
fn strip_placement(mut s: String) -> String {
    if let Some(a) = s.find(", stretch_y: [") {
        let b = a + s[a..].find(']').unwrap() + 1;
        s.replace_range(a..b, "");
    }
    if let Some(a) = s.find(", vanchor: Middle") {
        s.replace_range(a..a + ", vanchor: Middle".len(), "");
    }
    if let Some(a) = s.find(", hud: false") {
        s.replace_range(a..a + ", hud: false".len(), "");
    }
    s
}

fn frame_hash_with(f: &Frame, draw_text: impl Fn(String) -> String) -> u64 {
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    h.bytes(bytemuck::cast_slice(&f.vertices));
    for d in &f.draws {
        h.bytes(draw_text(strip_placement(format!("{d:?}"))).as_bytes());
    }
    let mut keys: Vec<_> = f.textures.keys().copied().collect();
    keys.sort_unstable();
    for k in keys {
        let img = &f.textures[&k];
        h.bytes(&k.to_le_bytes());
        h.bytes(&img.width.to_le_bytes());
        h.bytes(&img.height.to_le_bytes());
        h.bytes(&img.rgba);
    }
    h.0
}
