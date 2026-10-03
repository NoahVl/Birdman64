//! HLE Fast3D (plain F3D, as used by Pilotwings 64) + RDP command
//! interpreter. Walks a display list in [`Memory`], runs the RSP geometry
//! work on the CPU (matrices, vertex transform, lighting, fog, texture
//! coordinates) and records RDP state into [`Frame`] draw batches.
//!
//! Command encodings follow the non-F3DEX branches of
//! `decomp/include/libultra/PR/gbi.h`; see `docs/notes/gfx.md`.

use crate::combiner::CombinerKey;
use crate::frame::{
    Anchor, DrawCall, DrawUniforms, Frame, LodMode, N64_HEIGHT, N64_WIDTH, PipelineKey, ShaderKey,
    ShaderMode, VAnchor, Vertex, Wide,
};
use crate::matrix::{self, MatrixStack};
use crate::memory::Memory;
use crate::rdp::{self, BlendState, CycleType, DepthState, oml};
use crate::texture::{self, TextureCache, TileBinding};
use pw64_formats::gbi::{Gfx, ImSiz, geom, op};
use pw64_formats::tmem::{Tile, TlutMode, Tmem};
use std::collections::BTreeSet;

/// F3D display-list call depth.
const DL_STACK_DEPTH: usize = 10;
/// Safety net against runaway lists (bad pointers, cycles).
const MAX_COMMANDS: usize = 4_000_000;

/// F3D `G_MOVEMEM` indices.
mod mv {
    pub const VIEWPORT: u8 = 0x80;
    pub const LOOKATY: u8 = 0x82;
    pub const LOOKATX: u8 = 0x84;
    pub const L0: u8 = 0x86;
    pub const L7: u8 = 0x94;
}

/// `G_MOVEWORD` indices.
mod mw {
    pub const NUMLIGHT: u8 = 0x02;
    pub const CLIP: u8 = 0x04;
    pub const SEGMENT: u8 = 0x06;
    pub const FOG: u8 = 0x08;
    pub const LIGHTCOL: u8 = 0x0A;
    pub const PERSPNORM: u8 = 0x0E;
}

#[derive(Debug, Clone, Copy, Default)]
struct Light {
    color: [f32; 3],
    /// Unit direction (F3D stores s8 ×3, pointing toward the light).
    dir: [f32; 3],
}

#[derive(Debug, Clone, Copy, Default)]
struct ProcVertex {
    pos: [f32; 4],
    color: [f32; 4],
    st: [f32; 2],
}

#[derive(Debug, Clone, Copy)]
struct TextureState {
    on: bool,
    tile: u8,
    /// Max mip level (`gSPTexture` level = levels - 1): the RDP's LOD clamp.
    level: u8,
    scale: [f32; 2],
}

#[derive(Debug, Clone, Copy)]
struct TImg {
    siz: ImSiz,
    width: u32,
    addr: u32,
}

/// Cached state template for the next triangles (rebuilt when dirty).
#[derive(Clone)]
struct Template {
    pipeline: PipelineKey,
    uniforms: DrawUniforms,
    textures: [Option<TileBinding>; 2],
    /// Unique per build within a run: a draw made from the same template
    /// has equal pipeline/uniforms/textures without comparing them.
    serial: u64,
}

/// Last [`texture::bind_tile`] result per tile. Valid while TMEM is
/// unchanged (`tmem_gen`) and the inputs the binding is a function of —
/// tile descriptor + size, TLUT mode, filter — are equal.
#[derive(Clone, Copy)]
struct BindMemo {
    tmem_gen: u64,
    tile: Tile,
    tlut: TlutMode,
    linear: bool,
    binding: TileBinding,
}

/// The interpreter. Keep one alive across frames: it owns the texture
/// cache. Each [`Interpreter::run`] starts from a reset RSP/RDP state.
pub struct Interpreter {
    pub textures: TextureCache,
    /// Debug: when `Some`, [`Interpreter::run`] appends one line per command
    /// (plus processed vertices and emitted triangles) — see `take_trace`.
    pub trace: Option<String>,
    unknown: BTreeSet<u8>,
    // RSP state.
    segments: [u32; 16],
    mtx: MatrixStack,
    vtx: [ProcVertex; 16],
    geometry_mode: u32,
    viewport: ([f32; 3], [f32; 3]),
    /// `gSPClipRatio` (F3D default 2): triangles are clipped to this many
    /// viewports; the RDP scissor crops the rest.
    clip_ratio: f32,
    lights: [Light; 8],
    num_lights: usize,
    fog_mul: f32,
    fog_off: f32,
    texture: TextureState,
    // RDP state.
    omh: u32,
    oml: u32,
    combine: (u32, u32),
    prim: [f32; 4],
    prim_lod: f32,
    prim_depth: f32,
    env: [f32; 4],
    fog: [f32; 4],
    blend: [f32; 4],
    fill: [f32; 4],
    /// Raw fill word (a z value when filling the z image).
    fill_word: u16,
    scissor: [f32; 4],
    cimg: u32,
    zimg: u32,
    timg: Option<TImg>,
    tmem: Tmem,
    template: Option<Box<Template>>,
    frame: Frame,
    /// Only perspective draws inside `WIDE_TAG_ON/OFF` brackets are world
    /// view (widescreen `Wide::Extend`); otherwise all of them are. Set for
    /// the native game, whose C side emits the tags. Kept across runs.
    pub wide_tags: bool,
    /// Inside a world-view bracket.
    wide_view: bool,
    /// Current HUD edge anchor (`ANCHOR_TAG` markers); reset per run.
    anchor: Anchor,
    /// Current HUD vertical anchor (`VANCHOR_TAG` markers); reset per run.
    vanchor: VAnchor,
    /// Inside the flight HUD (`HUD_TAG_ON/OFF` brackets); reset per run.
    hud: bool,
    /// Commands executed by the last run (TEXRECT + its halves count once).
    commands: usize,
    /// Bumped by every TMEM load (see [`BindMemo`]).
    tmem_gen: u64,
    bind_memo: [Option<BindMemo>; 8],
    /// Set while building the template of a texrect whose every pixel
    /// corner lands on a texel center (see `tex_rect`): the RDP bilinear
    /// never blends there, so the tile binds point-sampled.
    rect_point: bool,
    /// Last template serial handed out, and the one the last draw used.
    template_serial: u64,
    last_serial: u64,
    /// Reused DRAM read buffer for texture loads (kept across runs).
    load_buf: Vec<u8>,
    /// Sizes of the last frame (vertices, draws, textures): the next
    /// frame's initial capacity. Kept across runs.
    cap_hint: [usize; 3],
}

/// `G_NOOP` w1 markers around the native game's world view (Hor+
/// widescreen; `crates/pw64-game/native/src/pw64_widescreen.c`).
pub const WIDE_TAG_ON: u32 = 0x5057_5731;
pub const WIDE_TAG_OFF: u32 = 0x5057_5730;
/// `G_NOOP` w1 HUD anchor marker: "PWA" + `L`/`C`/`R` in the low byte;
/// applies to the following draws (see [`Anchor`]).
pub const ANCHOR_TAG: u32 = 0x5057_4100;
/// `G_NOOP` w1 HUD vertical anchor marker (fill view): "PWV" + `T`/`M`/`B`
/// in the low byte; applies to the following draws (see [`VAnchor`]).
pub const VANCHOR_TAG: u32 = 0x5057_5600;
/// `G_NOOP` w1 HUD marker on/off ("PWH1"/"PWH0", `pw64_hud_tag` in
/// `pw64_widescreen.c`): the following draws are flight HUD, the only ones
/// the renderer's OLED care options (drift + dim) apply to. Unlike the
/// anchor tags it is emitted unconditionally; the renderer ignores it
/// unless OLED care is on.
pub const HUD_TAG_ON: u32 = 0x5057_4831;
pub const HUD_TAG_OFF: u32 = 0x5057_4830;

/// The anchor an `ANCHOR_TAG` marker word selects (`None`: not a marker).
pub fn anchor_tag(w1: u32) -> Option<Anchor> {
    if w1 & 0xFFFF_FF00 != ANCHOR_TAG {
        return None;
    }
    match w1 as u8 {
        b'L' => Some(Anchor::Left),
        b'C' => Some(Anchor::Centre),
        b'R' => Some(Anchor::Right),
        _ => None,
    }
}

/// The anchor a `VANCHOR_TAG` marker word selects (`None`: not a marker).
pub fn vanchor_tag(w1: u32) -> Option<VAnchor> {
    if w1 & 0xFFFF_FF00 != VANCHOR_TAG {
        return None;
    }
    match w1 as u8 {
        b'T' => Some(VAnchor::Top),
        b'M' => Some(VAnchor::Middle),
        b'B' => Some(VAnchor::Bottom),
        _ => None,
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

fn rgba8(w: u32) -> [f32; 4] {
    w.to_be_bytes().map(|b| b as f32 / 255.0)
}

fn rgba5551(v: u16) -> [f32; 4] {
    let c = pw64_formats::tmem::rgba16(v);
    c.map(|b| b as f32 / 255.0)
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if l == 0.0 { v } else { v.map(|c| c / l) }
}

impl Interpreter {
    pub fn new() -> Self {
        Self {
            textures: TextureCache::default(),
            trace: None,
            unknown: BTreeSet::new(),
            segments: [0; 16],
            mtx: MatrixStack::default(),
            vtx: [ProcVertex::default(); 16],
            geometry_mode: 0,
            viewport: ([160.0, 120.0, 0.5], [160.0, 120.0, 0.5]),
            clip_ratio: 2.0,
            lights: [Light::default(); 8],
            num_lights: 0,
            fog_mul: 0.0,
            fog_off: 0.0,
            texture: TextureState {
                on: false,
                tile: 0,
                level: 0,
                scale: [1.0; 2],
            },
            omh: 0,
            oml: 0,
            combine: (0, 0),
            prim: [0.0; 4],
            prim_lod: 0.0,
            prim_depth: 0.0,
            env: [0.0; 4],
            fog: [0.0; 4],
            blend: [0.0; 4],
            fill: [0.0; 4],
            fill_word: 0,
            scissor: [0.0, 0.0, N64_WIDTH, N64_HEIGHT],
            cimg: 0,
            zimg: u32::MAX,
            timg: None,
            tmem: Tmem::new(),
            template: None,
            frame: Frame::default(),
            wide_tags: false,
            wide_view: false,
            anchor: Anchor::Centre,
            vanchor: VAnchor::Middle,
            hud: false,
            commands: 0,
            tmem_gen: 0,
            bind_memo: [None; 8],
            rect_point: false,
            template_serial: 0,
            last_serial: 0,
            load_buf: Vec::new(),
            cap_hint: [0; 3],
        }
    }

    /// Opcodes seen that are not implemented (each is logged once).
    pub fn unknown_opcodes(&self) -> &BTreeSet<u8> {
        &self.unknown
    }

    /// Number of commands the last [`Interpreter::run`] executed.
    pub fn command_count(&self) -> usize {
        self.commands
    }

    /// Runs the display list at segmented address `dl` and returns the
    /// recorded frame. RSP/RDP state is reset first, like a new gfx task.
    pub fn run(&mut self, mem: &dyn Memory, dl: u32) -> Frame {
        let [v, d, t] = self.cap_hint;
        let mut frame = Frame {
            vertices: Vec::with_capacity(v),
            draws: Vec::with_capacity(d),
            textures: std::collections::HashMap::with_capacity(t),
            chains: Default::default(),
        };
        self.run_into(mem, dl, &mut frame);
        frame
    }

    /// [`Interpreter::run`] into `frame`, reusing its allocations (it is
    /// cleared first).
    pub fn run_into(&mut self, mem: &dyn Memory, dl: u32, frame: &mut Frame) {
        let textures = std::mem::take(&mut self.textures);
        let unknown = std::mem::take(&mut self.unknown);
        let trace = self.trace.take().map(|mut s| {
            s.clear();
            s
        });
        let wide_tags = self.wide_tags;
        let load_buf = std::mem::take(&mut self.load_buf);
        *self = Self::new();
        self.wide_tags = wide_tags;
        self.textures = textures;
        self.unknown = unknown;
        self.trace = trace;
        self.load_buf = load_buf;
        frame.vertices.clear();
        frame.draws.clear();
        frame.textures.clear();
        frame.chains.clear();
        std::mem::swap(&mut self.frame, frame);
        self.execute(mem, dl);
        std::mem::swap(&mut self.frame, frame);
        self.cap_hint = [
            frame.vertices.len(),
            frame.draws.len(),
            frame.textures.len(),
        ];
    }

    fn execute(&mut self, mem: &dyn Memory, dl: u32) {
        let mut pc = self.resolve(mem, dl);
        let mut stack: Vec<u32> = Vec::new();
        for n in 0..MAX_COMMANDS {
            self.commands = n + 1;
            let w0 = mem.read_u32(pc);
            let w1 = mem.read_u32(pc.wrapping_add(4));
            let opc = (w0 >> 24) as u8;
            if let Some(t) = self.trace.as_mut() {
                use std::fmt::Write;
                let _ = writeln!(
                    t,
                    "{:indent$}{pc:08X}: {w0:08X} {w1:08X} {}",
                    "",
                    op::name(opc).unwrap_or("?"),
                    indent = stack.len() * 2
                );
            }
            pc = pc.wrapping_add(8);
            match opc {
                op::G_DL => {
                    let target = self.resolve(mem, w1);
                    if (w0 >> 16) & 0xFF == 0 {
                        if stack.len() >= DL_STACK_DEPTH {
                            log::warn!("G_DL nesting beyond {DL_STACK_DEPTH}; ignored");
                            continue;
                        }
                        stack.push(pc);
                    }
                    pc = target;
                }
                op::G_ENDDL => match stack.pop() {
                    Some(ret) => pc = ret,
                    None => return,
                },
                op::G_TEXRECT | op::G_TEXRECTFLIP => {
                    // F3D: the next two commands are RDPHALF_1 (s, t) and
                    // RDPHALF_2 (dsdx, dtdy), whatever their opcode byte
                    // (sprite.c's old gbi.h numbers them 0xB3/0xB2).
                    let st = mem.read_u32(pc.wrapping_add(4));
                    let d = mem.read_u32(pc.wrapping_add(12));
                    pc = pc.wrapping_add(16);
                    if let Some(t) = self.trace.as_mut() {
                        use std::fmt::Write;
                        let _ = writeln!(t, "    st {st:08X} dsdt {d:08X}");
                    }
                    self.tex_rect(w0, w1, st, d, opc == op::G_TEXRECTFLIP);
                }
                _ => self.command(mem, w0, w1),
            }
        }
        log::warn!("display list exceeded {MAX_COMMANDS} commands; stopped");
    }

    /// Segmented → physical address (`RSP_SEGMENT`). A [`Memory::map`]
    /// implementation can override this per raw word.
    fn resolve(&self, mem: &dyn Memory, addr: u32) -> u32 {
        if let Some(mapped) = mem.map(addr) {
            return mapped;
        }
        (self.segments[((addr >> 24) & 0xF) as usize].wrapping_add(addr & 0x00FF_FFFF))
            & 0x1FFF_FFFF
    }

    fn dirty(&mut self) {
        self.template = None;
    }

    fn command(&mut self, mem: &dyn Memory, w0: u32, w1: u32) {
        let opc = (w0 >> 24) as u8;
        match opc {
            // Widescreen world-view markers (native game, pw64_widescreen.c).
            op::G_NOOP if w1 == WIDE_TAG_ON || w1 == WIDE_TAG_OFF => {
                self.wide_view = w1 == WIDE_TAG_ON;
            }
            op::G_NOOP if anchor_tag(w1).is_some() => {
                self.anchor = anchor_tag(w1).unwrap_or_default();
            }
            op::G_NOOP if vanchor_tag(w1).is_some() => {
                self.vanchor = vanchor_tag(w1).unwrap_or_default();
            }
            // Flight-HUD on/off (pw64_hud_tag): brackets the following
            // draws so the renderer can drift + dim them (OLED care).
            op::G_NOOP if w1 == HUD_TAG_ON || w1 == HUD_TAG_OFF => {
                self.hud = w1 == HUD_TAG_ON;
            }
            op::G_SPNOOP
            | op::G_NOOP
            | op::G_RDPLOADSYNC
            | op::G_RDPPIPESYNC
            | op::G_RDPTILESYNC
            | op::G_RDPFULLSYNC
            | op::G_CULLDL
            | op::G_RDPHALF_1
            | op::G_RDPHALF_2
            | op::G_RDPHALF_CONT => {}
            // SETKEYGB, SETKEYR, SETCONVERT: chroma key / YUV, unused here.
            0xEA..=0xEC => {}
            op::G_MTX => {
                let mut b = [0u8; 64];
                mem.read_bytes(self.resolve(mem, w1), &mut b);
                self.mtx.load((w0 >> 16) as u8, &matrix::from_fixed(&b));
            }
            op::G_POPMTX => self.mtx.pop(),
            op::G_VTX => self.load_vertices(mem, w0, w1),
            op::G_TRI1 => {
                // Inline `Gfx::decode` (indices ×10; hot: one per triangle,
                // and reading the returned enum stalled on store forwarding).
                let v = [16, 8, 0].map(|sh| (((w1 >> sh) & 0xFF) / 10) as u8);
                self.triangle(v, (w1 >> 24) as u8);
            }
            op::G_SETGEOMETRYMODE => {
                self.geometry_mode |= w1;
                self.dirty();
            }
            op::G_CLEARGEOMETRYMODE => {
                self.geometry_mode &= !w1;
                self.dirty();
            }
            op::G_TEXTURE => {
                let Gfx::Texture {
                    tile,
                    on,
                    scale_s,
                    scale_t,
                    level,
                } = Gfx::decode(w0, w1)
                else {
                    unreachable!()
                };
                let f = |s: u16| if s == 0xFFFF { 1.0 } else { s as f32 / 65536.0 };
                self.texture = TextureState {
                    on,
                    tile,
                    level,
                    scale: [f(scale_s), f(scale_t)],
                };
                self.dirty();
            }
            op::G_MOVEWORD => self.move_word(w0, w1),
            op::G_MOVEMEM => self.move_mem(mem, w0, w1),
            op::G_SETOTHERMODE_H | op::G_SETOTHERMODE_L => {
                let (shift, len) = ((w0 >> 8) & 0xFF, w0 & 0xFF);
                let mask = (((1u64 << len) - 1) << shift) as u32;
                let m = if opc == op::G_SETOTHERMODE_H {
                    &mut self.omh
                } else {
                    &mut self.oml
                };
                *m = (*m & !mask) | (w1 & mask);
                self.tmem.othermode_h = self.omh;
                self.dirty();
            }
            op::G_RDPSETOTHERMODE => {
                self.omh = w0 & 0x00FF_FFFF;
                self.oml = w1;
                self.tmem.othermode_h = self.omh;
                self.dirty();
            }
            op::G_SETCOMBINE => {
                self.combine = (w0, w1);
                self.dirty();
            }
            op::G_SETPRIMCOLOR => {
                self.prim = rgba8(w1);
                self.prim_lod = (w0 & 0xFF) as f32 / 255.0;
                self.dirty();
            }
            op::G_SETENVCOLOR => {
                self.env = rgba8(w1);
                self.dirty();
            }
            op::G_SETFOGCOLOR => {
                self.fog = rgba8(w1);
                self.dirty();
            }
            op::G_SETBLENDCOLOR => {
                self.blend = rgba8(w1);
                self.dirty();
            }
            op::G_SETFILLCOLOR => {
                // 16-bit framebuffer: the high half is the RGBA5551 fill color.
                self.fill = rgba5551((w1 >> 16) as u16);
                self.fill_word = (w1 >> 16) as u16;
                self.dirty();
            }
            op::G_SETPRIMDEPTH => {
                self.prim_depth = ((w1 >> 16) & 0x7FFF) as f32 / 32767.0;
            }
            op::G_SETSCISSOR => {
                let f = |v: u32| (v & 0xFFF) as f32 / 4.0;
                self.scissor = [f(w0 >> 12), f(w0), f(w1 >> 12), f(w1)];
                self.dirty();
            }
            op::G_SETCIMG => {
                self.cimg = self.resolve(mem, w1);
                self.dirty();
            }
            op::G_SETZIMG => {
                self.zimg = self.resolve(mem, w1);
                self.dirty();
            }
            op::G_FILLRECT => self.fill_rect(w0, w1),
            op::G_SETTIMG => {
                let g = Gfx::decode(w0, w1);
                let Gfx::SetTImg { format, width, .. } = g else {
                    unreachable!()
                };
                let addr = self.resolve(mem, w1);
                self.timg = Some(TImg {
                    siz: format.1,
                    width: width as u32,
                    addr,
                });
                self.tmem_run(
                    &Gfx::SetTImg {
                        format,
                        width,
                        addr,
                    },
                    None,
                );
            }
            op::G_SETTILE | op::G_SETTILESIZE => {
                self.tmem_run(&Gfx::decode(w0, w1), None);
                self.dirty();
            }
            op::G_LOADBLOCK | op::G_LOADTILE | op::G_LOADTLUT => {
                let g = Gfx::decode(w0, w1);
                let Some(t) = self.timg else {
                    log::warn!("texture load before G_SETTIMG");
                    return;
                };
                let bytes = |px: u64| (px * t.siz.bits() as u64).div_ceil(8) as usize;
                let len = match g {
                    Gfx::LoadBlock { ult, lrs, .. } => {
                        bytes(ult as u64 * t.width as u64 + lrs as u64 + 1)
                    }
                    Gfx::LoadTile { lrt, .. } => bytes(((lrt >> 2) as u64 + 1) * t.width as u64),
                    _ => 512,
                };
                let mut buf = std::mem::take(&mut self.load_buf);
                buf.resize(len.min(1 << 22) + 16, 0);
                mem.read_raw(t.addr, &mut buf);
                self.tmem_gen += 1;
                self.tmem_run(&g, Some(&buf));
                self.load_buf = buf;
                self.dirty();
            }
            _ => {
                if self.unknown.insert(opc) {
                    log::warn!(
                        "unimplemented GBI command {:02X} ({}) w0={w0:08X} w1={w1:08X}",
                        opc,
                        op::name(opc).unwrap_or("unknown")
                    );
                }
            }
        }
    }

    /// Runs one texture command on the TMEM model. TMEM contents change only
    /// through the load path above, which bumps `tmem_gen`.
    fn tmem_run(&mut self, g: &Gfx, src: Option<&[u8]>) {
        let r = self
            .tmem
            .run(std::slice::from_ref(g), &|_, _| src.or(Some(&[])));
        if let Err(e) = r {
            log::warn!("TMEM: {e}");
        }
    }

    fn move_word(&mut self, w0: u32, w1: u32) {
        let index = (w0 & 0xFF) as u8;
        let offset = (w0 >> 8) & 0xFFFF;
        match index {
            mw::SEGMENT => self.segments[((offset / 4) & 0xF) as usize] = w1 & 0x1FFF_FFFF,
            mw::FOG => {
                self.fog_mul = (w1 >> 16) as i16 as f32;
                self.fog_off = w1 as i16 as f32;
            }
            mw::NUMLIGHT => {
                let n = (w1.wrapping_sub(0x8000_0000) / 32).saturating_sub(1);
                self.num_lights = (n as usize).min(7);
            }
            mw::LIGHTCOL => {
                // Offsets: light n at n * 0x20 (+4 for the copy).
                let n = (offset / 0x20) as usize;
                if n < 8 && offset.is_multiple_of(0x20) {
                    let c = rgba8(w1);
                    self.lights[n].color = [c[0], c[1], c[2]];
                }
            }
            // gSPClipRatio: four words (±x, ±y); RNX (offset 4) = ratio.
            mw::CLIP => {
                if offset == 4 {
                    self.clip_ratio = ((w1 & 0xFFFF) as f32).max(1.0);
                }
            }
            mw::PERSPNORM => {}
            _ => {
                if self.unknown.insert(op::G_MOVEWORD) {
                    log::warn!("unhandled G_MOVEWORD index {index:#x}");
                }
            }
        }
    }

    fn move_mem(&mut self, mem: &dyn Memory, w0: u32, w1: u32) {
        let index = ((w0 >> 16) & 0xFF) as u8;
        let mut b = [0u8; 16];
        let addr = self.resolve(mem, w1);
        if (mv::L0..=mv::L7).contains(&index) {
            // Light_t is all bytes: no swapping on hosts with LE structs.
            mem.read_raw(addr, &mut b);
        } else {
            mem.read_bytes(addr, &mut b);
        }
        let s16 = |i: usize| i16::from_be_bytes([b[i], b[i + 1]]) as f32;
        match index {
            mv::VIEWPORT => {
                // Vp: vscale[4], vtrans[4] (s16, x/y in quarter pixels).
                self.viewport = (
                    [s16(0) / 4.0, s16(2) / 4.0, s16(4) / 1023.0],
                    [s16(8) / 4.0, s16(10) / 4.0, s16(12) / 1023.0],
                );
            }
            mv::LOOKATX | mv::LOOKATY => {}
            mv::L0..=mv::L7 => {
                let n = ((index - mv::L0) / 2) as usize;
                self.lights[n] = Light {
                    color: [0, 1, 2].map(|i| b[i] as f32 / 255.0),
                    dir: normalize([8, 9, 10].map(|i| b[i] as i8 as f32)),
                };
            }
            _ => {
                if self.unknown.insert(op::G_MOVEMEM) {
                    log::warn!("unhandled G_MOVEMEM index {index:#x}");
                }
            }
        }
    }

    fn load_vertices(&mut self, mem: &dyn Memory, w0: u32, w1: u32) {
        let n = ((w0 >> 20) & 0xF) as usize + 1;
        let v0 = ((w0 >> 16) & 0xF) as usize;
        let mut all = [0u8; 256];
        let buf = &mut all[..n * 16];
        let addr = self.resolve(mem, w1);
        mem.read_bytes(addr, buf);
        // Vtx.cn[4] (color / normal) is bytes: take it from a raw read, so
        // hosts that swap u16s in `read_bytes` (native LE structs) keep the
        // byte order. One raw read of the whole range, not one per vertex.
        let mut raw = [0u8; 256];
        mem.read_raw(addr, &mut raw[..n * 16]);
        for (v, r) in buf
            .as_chunks_mut::<16>()
            .0
            .iter_mut()
            .zip(raw.as_chunks::<16>().0)
        {
            v[12..].copy_from_slice(&r[12..]);
        }
        let mvp = self.mtx.mvp();
        let mv = *self.mtx.modelview();
        let gm = self.geometry_mode;
        let (vs, vt) = self.viewport;
        for (i, b) in buf.as_chunks::<16>().0.iter().enumerate() {
            let slot = v0 + i;
            if slot >= 16 {
                break;
            }
            let s16 = |k: usize| i16::from_be_bytes([b[k], b[k + 1]]);
            let pos = [s16(0), s16(2), s16(4)].map(|v| v as f32);
            let c = matrix::transform(pos, &mvp);
            let mut color = [12, 13, 14, 15].map(|k| b[k] as f32 / 255.0);
            let mut st = [s16(8) as f32, s16(10) as f32];
            if gm & (geom::G_LIGHTING | geom::G_TEXTURE_GEN) != 0 {
                let n = normalize([12, 13, 14].map(|k| b[k] as i8 as f32));
                let nv = normalize(matrix::transform_dir(n, &mv));
                if gm & geom::G_LIGHTING != 0 {
                    let amb = self.lights[self.num_lights].color;
                    let mut rgb = amb;
                    for l in &self.lights[..self.num_lights] {
                        let d = (nv[0] * l.dir[0] + nv[1] * l.dir[1] + nv[2] * l.dir[2]).max(0.0);
                        for (c, lc) in rgb.iter_mut().zip(l.color) {
                            *c += d * lc;
                        }
                    }
                    color = [rgb[0].min(1.0), rgb[1].min(1.0), rgb[2].min(1.0), color[3]];
                }
                if gm & geom::G_TEXTURE_GEN != 0 {
                    // Spherical env map: normal x/y → 0..1 in s10.5 units of 1024 texels.
                    st = [(nv[0] + 1.0) * 16384.0, (nv[1] + 1.0) * 16384.0];
                }
            }
            if gm & geom::G_FOG != 0 {
                // F3D: shade alpha = clamp(z/w * fm + fo) (fixed-point in ucode).
                let z = if c[3] > 0.0 { c[2] / c[3] } else { -1.0 };
                color[3] = ((z * self.fog_mul + self.fog_off) / 255.0).clamp(0.0, 1.0);
            }
            let w = c[3];
            if let Some(t) = self.trace.as_mut() {
                use std::fmt::Write;
                let _ = writeln!(
                    t,
                    "    v{slot}: obj {pos:?} clip [{:.1} {:.1} {:.1} {:.1}] rgba {:?} st {:?}",
                    c[0],
                    c[1],
                    c[2],
                    c[3],
                    color.map(|x| (x * 255.0) as u8),
                    st.map(|x| x / 32.0)
                );
            }
            self.vtx[slot] = ProcVertex {
                pos: [
                    c[0] * vs[0] + w * vt[0],
                    -c[1] * vs[1] + w * vt[1],
                    (w - c[2]) * 0.5,
                    w,
                ],
                color,
                st: [
                    st[0] / 32.0 * self.texture.scale[0],
                    st[1] / 32.0 * self.texture.scale[1],
                ],
            };
        }
    }

    fn cycle_type(&self) -> CycleType {
        CycleType::from_othermode_h(self.omh)
    }

    /// Builds the draw template for the current state. Boxed: it is moved
    /// in and out of `self.template` per triangle, and a ~300-byte move
    /// costs a memcpy plus store-forwarding stalls on the reads after it.
    fn template(&mut self, mode: ShaderMode, rect: bool) -> Box<Template> {
        let cycle = self.cycle_type();
        let combiner = CombinerKey::new(self.combine.0, self.combine.1, cycle == CycleType::Two);
        let blend = BlendState::new(cycle, self.oml);
        let depth = if rect {
            let prim = self.oml & oml::ZSRC_PRIM != 0;
            DepthState {
                test: prim && self.oml & oml::Z_CMP != 0,
                write: prim && self.oml & oml::Z_UPD != 0,
                decal: false,
            }
        } else {
            DepthState::new(self.oml, self.geometry_mode & geom::G_ZBUFFER != 0)
        };
        let cull = if rect {
            0
        } else {
            ((self.geometry_mode & geom::G_CULL_FRONT != 0) as u8)
                | (((self.geometry_mode & geom::G_CULL_BACK != 0) as u8) << 1)
        };
        // `rect_point` overrides the filter (see `tex_rect`): with it, a
        // bilinear-flagged rect still binds point tiles.
        let linear = !self.rect_point && rdp::bilinear(self.omh) && mode != ShaderMode::Copy;
        let used = match mode {
            ShaderMode::Normal | ShaderMode::DepthImage => combiner.uses_texel(),
            ShaderMode::Copy => [true, false],
            _ => [false, false],
        };
        let tile = self.texture.tile;
        let mut textures = [None, None];
        for (i, u) in used.iter().enumerate() {
            if *u && (self.texture.on || rect) {
                textures[i] = self.bind((tile + i as u8) & 7, linear);
            }
        }
        // Per-pixel RDP LOD (shader `rdp_lod`): only for mip-mapped
        // triangles (max level ≥ 1). Max level 0 would make the RDP's
        // LOD_FRACTION 1.0 (always "distant") — TEXEL1 is then an unrelated
        // tile, so such draws keep LOD_FRACTION 0 as before. Rects keep it
        // too (the texrect command carries no max level).
        let max_level = self.texture.level;
        let mut lod_mode = LodMode::Off;
        if !rect
            && self.texture.on
            && max_level > 0
            && matches!(mode, ShaderMode::Normal | ShaderMode::DepthImage)
        {
            if self.omh & rdp::omh::TEXTLOD != 0
                && used.contains(&true)
                && let Some(chain) = self.bind_chain(tile, max_level, linear)
            {
                textures = [0, 1].map(|i| used[i].then_some(chain));
                lod_mode = LodMode::Chain;
            } else if combiner.uses_lod_frac() {
                lod_mode = LodMode::Tile;
            }
        }
        let lodp = match lod_mode {
            LodMode::Off => [0.0; 4],
            m => [
                m as u8 as f32,
                max_level as f32,
                if linear && m == LodMode::Chain {
                    0.5
                } else {
                    0.0
                },
                0.0,
            ],
        };
        // Every texture a draw references goes into the frame; a template
        // is always drawn with right after it is built.
        for b in textures.iter().flatten() {
            if !self.frame.textures.contains_key(&b.key)
                && let Some(img) = self.textures.map.get(&b.key)
            {
                self.frame.textures.insert(b.key, img.clone());
            }
        }
        self.template_serial += 1;
        let tile_uniform = |b: &Option<TileBinding>| match b {
            Some(b) => (
                [b.shift[0], b.shift[1], b.origin[0], b.origin[1]],
                [1.0 / b.width as f32, 1.0 / b.height as f32, 0.0, 0.0],
            ),
            None => ([1.0, 1.0, 0.0, 0.0], [1.0, 1.0, 0.0, 0.0]),
        };
        let (tile0, size0) = tile_uniform(&textures[0]);
        let (tile1, size1) = tile_uniform(&textures[1]);
        Box::new(Template {
            pipeline: PipelineKey {
                shader: ShaderKey {
                    mode,
                    combiner,
                    blend,
                    alpha_cvg_sel: self.oml & oml::ALPHA_CVG_SEL != 0
                        && self.oml & oml::CVG_X_ALPHA == 0,
                },
                depth,
                cull,
            },
            uniforms: DrawUniforms {
                screen: [0.0; 4],
                prim: self.prim,
                env: self.env,
                fog: self.fog,
                blend: self.blend,
                fill: self.fill,
                lod: [self.prim_lod, 0.0, 0.0, 0.0],
                tile0,
                size0,
                tile1,
                size1,
                lodp,
                filt: [0.0; 4],
            },
            textures,
            serial: self.template_serial,
        })
    }

    /// `G_TL_LOD`: binds tiles `base..=base+max_level` as one mip-chained
    /// texture ([`Frame::chains`]) when they form a proper chain — each
    /// level half the previous one's size (floor, ≥ 1) and S/T mapping
    /// (shift × 2, origin / 2), same wrap/filter; decoded (or replaced)
    /// images halving too. `None` → the caller falls back to plain
    /// TEXEL0/TEXEL1 = tile/tile+1. The binding is level 0's with the chain
    /// key. (PW64 never enables `G_TL_LOD` per the GBI audit — gfx.md —
    /// so this path is for completeness; logged once when hit.)
    fn bind_chain(&mut self, base: u8, max_level: u8, linear: bool) -> Option<TileBinding> {
        let n = max_level as usize + 1;
        if base as usize + n > 8 {
            return None;
        }
        let b0 = self.bind(base, linear)?;
        let half = if linear { 0.5 } else { 0.0 };
        let mut keys = Vec::with_capacity(n);
        keys.push(b0.key);
        for i in 1..n {
            let b = self.bind(base + i as u8, linear)?;
            let f = (1u32 << i) as f32;
            let proper = b.width == (b0.width >> i).max(1)
                && b.height == (b0.height >> i).max(1)
                && b.sampler == b0.sampler
                && (0..2).all(|a| {
                    b.shift[a] * f == b0.shift[a] && (b.origin[a] + half) * f == b0.origin[a] + half
                });
            if !proper {
                return None;
            }
            keys.push(b.key);
        }
        let imgs: Vec<_> = keys
            .iter()
            .map(|k| self.textures.map.get(k).cloned())
            .collect::<Option<_>>()?;
        let halves = imgs.iter().enumerate().all(|(i, m)| {
            m.width == (imgs[0].width >> i).max(1) && m.height == (imgs[0].height >> i).max(1)
        });
        if !halves {
            return None;
        }
        // Chain key: FNV-1a over the level keys (never equal to a content key
        // in practice; tagged so a 1-level chain can't collide with its tile).
        let mut key = 0xcbf2_9ce4_8422_2325u64 ^ 0x0043_4841_494E; // "CHAIN"
        for k in &keys {
            key = (key ^ k).wrapping_mul(0x0100_0000_01b3);
        }
        for (k, img) in keys.iter().zip(imgs) {
            self.frame.textures.entry(*k).or_insert(img);
        }
        self.frame.chains.entry(key).or_insert_with(|| keys.into());
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| log::info!("pw64-gfx: G_TL_LOD mip-chain draw ({n} levels)"));
        Some(TileBinding { key, ..b0 })
    }

    /// [`texture::bind_tile`] through the per-tile [`BindMemo`].
    fn bind(&mut self, tile: u8, linear: bool) -> Option<TileBinding> {
        let t = self.tmem.tile(tile)?;
        let tlut = self.tmem.tlut_mode();
        let slot = &mut self.bind_memo[tile as usize & 7];
        if let Some(m) = slot
            && m.tmem_gen == self.tmem_gen
            && m.tile == t
            && m.tlut == tlut
            && m.linear == linear
        {
            return Some(m.binding);
        }
        let binding = texture::bind_tile(&mut self.tmem, tile, linear, &mut self.textures)?;
        self.bind_memo[tile as usize & 7] = Some(BindMemo {
            tmem_gen: self.tmem_gen,
            tile: t,
            tlut,
            linear,
            binding,
        });
        Some(binding)
    }

    fn push(&mut self, t: &Template, verts: &[Vertex], is_3d: bool) {
        let first = self.frame.vertices.len();
        self.frame.vertices.extend_from_slice(verts);
        self.record(t, first, is_3d);
    }

    /// Adds the vertices appended since `first` to the draw list (merging
    /// into the last draw when all state matches).
    fn record(&mut self, t: &Template, first: usize, is_3d: bool) {
        let verts = &self.frame.vertices[first..];
        let count = verts.len() as u32;
        let first = first as u32;
        let scissor = if is_3d { self.clip_box() } else { self.scissor };
        let (wide, stretch_y) = self.wide_class(verts, is_3d, t);
        let same_template = self.last_serial == t.serial;
        self.last_serial = t.serial;
        if let Some(last) = self.frame.draws.last_mut()
            && last.first_vertex + last.vertex_count == first
            && (same_template
                || (last.pipeline == t.pipeline
                    && last.uniforms == t.uniforms
                    && last.textures == t.textures))
            && last.scissor == scissor
            && last.is_3d == is_3d
            && std::mem::discriminant(&last.wide) == std::mem::discriminant(&wide)
            // Consecutive stretches merge only when they span the same rows
            // (drawScreenBorder's top + bottom bars merged would span the
            // view and the fill view would stretch both onto it).
            && last.stretch_y == stretch_y
            && last.anchor == self.anchor
            && last.vanchor == self.vanchor
            && last.hud == self.hud
        {
            last.vertex_count += count;
            if let (Wide::Stretch(a), Wide::Stretch(b)) = (&mut last.wide, wide) {
                *a = [a[0].min(b[0]), a[1].max(b[1])];
            }
            return;
        }
        self.frame.draws.push(DrawCall {
            pipeline: t.pipeline,
            first_vertex: first,
            vertex_count: count,
            uniforms: t.uniforms,
            textures: t.textures,
            scissor,
            is_3d,
            wide,
            anchor: self.anchor,
            vanchor: self.vanchor,
            stretch_y,
            hud: self.hud,
        });
    }

    /// Widescreen class of a triangle/rect (see [`Wide`]): perspective
    /// projection (w depends on the vertex) → world geometry (inside a
    /// world-view bracket, see `wide_tags`); untextured and
    /// spanning the whole 4:3 width → a full-screen fill/fade/bar (with its
    /// min/max vertex row, [`DrawCall::stretch_y`]); else HUD.
    fn wide_class(&self, verts: &[Vertex], is_3d: bool, t: &Template) -> (Wide, [f32; 2]) {
        let p = &self.mtx.projection;
        if is_3d && (p[0][3] != 0.0 || p[1][3] != 0.0 || p[2][3] != 0.0) {
            return if self.wide_view || !self.wide_tags {
                (Wide::Extend, [0.0; 2])
            } else {
                (Wide::Fixed, [0.0; 2])
            };
        }
        if t.textures.iter().all(Option::is_none) {
            let (lo, hi, lo_y, hi_y) = verts.iter().fold(
                (f32::MAX, f32::MIN, f32::MAX, f32::MIN),
                |(lo, hi, lo_y, hi_y), v| {
                    let (x, y) = (v.pos[0] / v.pos[3], v.pos[1] / v.pos[3]);
                    (lo.min(x), hi.max(x), lo_y.min(y), hi_y.max(y))
                },
            );
            if lo <= Wide::MAIN_VIEW[0] && hi >= Wide::MAIN_VIEW[1] {
                return (Wide::Stretch([lo, hi]), [lo_y, hi_y]);
            }
        }
        (Wide::Fixed, [0.0; 2])
    }

    /// Scissor for 3D triangles: the RSP clips them to the viewport scaled
    /// by the clip ratio (`gSPClipRatio`; ratio 1 = exactly the viewport),
    /// so with a wider RDP scissor nothing is drawn past that box.
    fn clip_box(&self) -> [f32; 4] {
        let (vs, vt) = self.viewport;
        let r = self.clip_ratio;
        let (hx, hy) = (vs[0].abs() * r, vs[1].abs() * r);
        let s = self.scissor;
        [
            s[0].max(vt[0] - hx),
            s[1].max(vt[1] - hy),
            s[2].min(vt[0] + hx),
            s[3].min(vt[1] + hy),
        ]
    }

    /// Mode of a 1/2-cycle draw: into the z image when the color image
    /// points at it (`uvGfxStateDrawDL`'s shadow-volume pass).
    fn combine_mode(&self) -> ShaderMode {
        if self.cimg == self.zimg {
            ShaderMode::DepthImage
        } else {
            ShaderMode::Normal
        }
    }

    fn triangle(&mut self, v: [u8; 3], flag: u8) {
        if self.geometry_mode & (geom::G_CULL_FRONT | geom::G_CULL_BACK)
            == geom::G_CULL_FRONT | geom::G_CULL_BACK
        {
            return;
        }
        let mode = match self.cycle_type() {
            CycleType::Fill => ShaderMode::Fill,
            CycleType::Copy => ShaderMode::Copy,
            _ => self.combine_mode(),
        };
        // The cached template is reused until a state change clears it.
        let t = match self.template.take() {
            Some(t) if t.pipeline.shader.mode == mode => t,
            _ => self.template(mode, false),
        };
        let flat = self.geometry_mode & geom::G_SHADING_SMOOTH == 0;
        let flat_color = self.vtx[(v[(flag as usize).min(2)] & 15) as usize].color;
        // Straight into the frame (a stack array + extend_from_slice costs
        // a memcpy call per triangle).
        let first = self.frame.vertices.len();
        self.frame.vertices.reserve(3);
        for i in v {
            let p = &self.vtx[(i & 15) as usize];
            self.frame.vertices.push(Vertex {
                pos: p.pos,
                color: if flat {
                    [flat_color[0], flat_color[1], flat_color[2], p.color[3]]
                } else {
                    p.color
                },
                st: p.st,
                st_clamp: Vertex::NO_CLAMP,
            });
        }
        self.record(&t, first, true);
        self.template = Some(t);
        if let Some(tr) = self.trace.as_mut() {
            use std::fmt::Write;
            let _ = writeln!(tr, "    -> draw {}", self.frame.draws.len() - 1);
        }
    }

    /// Two triangles covering a screen rectangle (N64 pixels).
    fn rect_vertices(&self, x: [f32; 2], y: [f32; 2], st: [[f32; 2]; 4]) -> [Vertex; 6] {
        let z = 1.0 - self.prim_depth;
        let v = |px: f32, py: f32, st: [f32; 2]| Vertex {
            pos: [px, py, z, 1.0],
            color: [0.0; 4],
            st,
            st_clamp: Vertex::NO_CLAMP,
        };
        let (a, b, c, d) = (
            v(x[0], y[0], st[0]),
            v(x[1], y[0], st[1]),
            v(x[1], y[1], st[2]),
            v(x[0], y[1], st[3]),
        );
        // Counter-clockwise on screen with y up (culling is off for rects anyway).
        [a, d, c, a, c, b]
    }

    fn fill_rect(&mut self, w0: u32, w1: u32) {
        let f = |v: u32| (v & 0xFFF) as f32 / 4.0;
        let (lrx, lry, ulx, uly) = (f(w0 >> 12), f(w0), f(w1 >> 12), f(w1));
        let cycle = self.cycle_type();
        // Fill/copy modes include the lower-right edge.
        let inc = if matches!(cycle, CycleType::Fill | CycleType::Copy) {
            1.0
        } else {
            0.0
        };
        let mode = if cycle == CycleType::Fill {
            if self.cimg == self.zimg {
                ShaderMode::DepthClear
            } else {
                ShaderMode::Fill
            }
        } else {
            self.combine_mode()
        };
        let mut t = self.template(mode, true);
        if mode == ShaderMode::DepthClear {
            t.pipeline.depth = DepthState {
                test: false,
                write: true,
                decal: false,
            };
        }
        let mut verts = self.rect_vertices([ulx, lrx + inc], [uly, lry + inc], [[0.0; 2]; 4]);
        if mode == ShaderMode::DepthClear {
            // The fill word is the z value (the game clears to the max = far).
            let z = rdp::zbuffer_word_to_depth(self.fill_word);
            verts.iter_mut().for_each(|v| v.pos[2] = z);
        }
        self.push(&t, &verts, false);
    }

    fn tex_rect(&mut self, w0: u32, w1: u32, st: u32, d: u32, flip: bool) {
        let f = |v: u32| (v & 0xFFF) as f32 / 4.0;
        let (lrx, lry) = (f(w0 >> 12), f(w0));
        let (ulx, uly) = (f(w1 >> 12), f(w1));
        let tile = ((w1 >> 24) & 7) as u8;
        let mut s = (st >> 16) as i16 as f32 / 32.0;
        let mut t0 = st as i16 as f32 / 32.0;
        let mut dsdx = (d >> 16) as i16 as f32 / 1024.0;
        let dtdy = d as i16 as f32 / 1024.0;
        let cycle = self.cycle_type();
        let copy = cycle == CycleType::Copy;
        if copy {
            dsdx /= 4.0;
        }
        let inc = if copy { 1.0 } else { 0.0 };
        let (x1, y1) = (lrx + inc, lry + inc);
        let (w, h) = (x1 - ulx, y1 - uly);
        let mut st_clamp = Vertex::NO_CLAMP;
        let mut point = false;
        if !copy && rdp::bilinear(self.omh) {
            // The RDP evaluates S/T at each pixel's top-left corner, the GPU
            // at its center. Bilinear sampling (texel centers at integers,
            // see `TileBinding::origin`) must hit the same texel positions,
            // else 1:1 rects blend half of the next row/column in — e.g. the
            // garbage TMEM rows under font glyphs. At higher resolutions the
            // sub-pixel samples are also clamped to the first/last pixel's S/T.
            // (S follows dsdx and T dtdy whichever screen axis they map to.)
            let (ns, nt) = if flip { (h, w) } else { (w, h) };
            let (s1, t1) = (
                s + (ns - 1.0).max(0.0) * dsdx,
                t0 + (nt - 1.0).max(0.0) * dtdy,
            );
            // The RDP bilinear weights are the fractional S/T of the pixel
            // corner, so it blends only when that falls between texel
            // centers. With unit deltas and integral alignment (the sprite
            // library's 1:1 blits and fonts) every corner is exactly on a
            // center: hardware output is unfiltered, and linear sampling of
            // the up-scaled GPU render would smear neighbours in (IA blits'
            // white A=0 background bleeds halos around glyphs). Such rects
            // bind point tiles. The test is on N64-space values only, so it
            // is the same at every output scale. (S follows y under flip.)
            let (us, ut) = if flip { (uly, ulx) } else { (ulx, uly) };
            point = dsdx.abs() == 1.0
                && dtdy.abs() == 1.0
                && (s - us * dsdx).fract() == 0.0
                && (t0 - ut * dtdy).fract() == 0.0;
            // Shift S/T so a pixel's GPU center hits the RDP corner value:
            // on a texel center (linear tiles' origin is half a texel lower)
            // or, for point tiles (texel k spans [k, k+1)), mid-texel, so the
            // whole up-scaled pixel stays on that texel even when mirrored.
            let (os, ot) = if point {
                (0.5 - 0.5 * dsdx, 0.5 - 0.5 * dtdy)
            } else {
                (-0.5 * dsdx, -0.5 * dtdy)
            };
            // Point: clamp to the first/last texel's center (robust against
            // rounding at texel edges).
            let c = if point { 0.5 } else { 0.0 };
            st_clamp = [s.min(s1) + c, t0.min(t1) + c, s.max(s1) + c, t0.max(t1) + c];
            s += os;
            t0 += ot;
        }
        // Corners: (ul, ur, lr, ll); flip swaps which screen axis S follows.
        let st_at = |dx: f32, dy: f32| {
            if flip {
                [s + dy * dsdx, t0 + dx * dtdy]
            } else {
                [s + dx * dsdx, t0 + dy * dtdy]
            }
        };
        let corners = [st_at(0.0, 0.0), st_at(w, 0.0), st_at(w, h), st_at(0.0, h)];
        let saved = self.texture;
        self.texture.tile = tile;
        let mode = if copy {
            ShaderMode::Copy
        } else {
            self.combine_mode()
        };
        let t = {
            self.rect_point = point;
            let t = self.template(mode, true);
            self.rect_point = false;
            t
        };
        self.texture = saved;
        let mut verts = self.rect_vertices([ulx, x1], [uly, y1], corners);
        verts.iter_mut().for_each(|v| v.st_clamp = st_clamp);
        self.push(&t, &verts, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::VecMemory;

    fn words(cmds: &[(u32, u32)]) -> Vec<u32> {
        cmds.iter().flat_map(|&(a, b)| [a, b]).collect()
    }

    /// Per-draw LOD parameters: `G_TL_TILE` + the mip-lerp combiner +
    /// max level 2 → `LodMode::Tile`; `G_TL_LOD` with a proper 8/4/2 chain →
    /// `LodMode::Chain` (one texture, levels in `Frame::chains`); a broken
    /// chain falls back to Tile; max level 0 and texrect-free non-LOD
    /// combiners stay Off.
    #[test]
    fn lod_params_per_draw() {
        let mut mem = VecMemory::default();
        let ident = mem.push(&matrix::to_fixed(&matrix::IDENTITY), 8);
        let texels: Vec<u8> = (0..168).map(|i| i as u8).collect();
        let timg = mem.push(&texels, 8);
        let mut vtx = Vec::new();
        for (x, y) in [(0i16, 0i16), (1, 0), (0, 1)] {
            for v in [x, y, 0, 0, 0, 0] {
                vtx.extend_from_slice(&v.to_be_bytes());
            }
            vtx.extend_from_slice(&[255; 4]);
        }
        let vaddr = mem.push(&vtx, 8);
        // RGBA16 tiles 0/1/2: 8×8 at TMEM 0, 4×4 at 16, 2×2 at 20 (words),
        // wrapping masks = log2 size, shift = level.
        let tile = |t: u32, line: u32, tmem: u32, mask: u32, shift: u32, size: u32| {
            [
                (
                    0xF510_0000 | (line << 9) | tmem,
                    (t << 24) | (mask << 14) | (shift << 10) | (mask << 4) | shift,
                ),
                (
                    0xF200_0000,
                    (t << 24) | (((size - 1) * 4) << 12) | ((size - 1) * 4),
                ),
            ]
        };
        let mut list = |level: u32, lod: bool, shift1: u32, combine: (u32, u32)| {
            let mut c = vec![
                (0x0103_0040, ident),
                (0x0102_0040, ident),
                (0xB700_0000, geom::G_SHADE),
                // 2-cycle, bilinear, TEXTLOD on/off.
                (0xBA00_0018, 1 << 20 | (lod as u32) << 16 | 2 << 12),
                combine,
                (0xFD10_0000, timg),
                (0xF510_0000, 0x0700_0000),
                (0xF300_0000, 0x0700_0000 | 83 << 12),
            ];
            c.extend(tile(0, 2, 0, 3, 0, 8));
            c.extend(tile(1, 1, 16, 2, shift1, 4));
            c.extend(tile(2, 1, 20, 1, 2, 2));
            c.extend([
                (0xBB00_0001 | level << 11, 0xFFFF_FFFF), // gSPTexture tile 0
                (0x0420_0030, vaddr),
                (0xBF00_0000, 0x0000_0A14),
                (0xB800_0000, 0),
            ]);
            mem.push_words(&words(&c))
        };
        let mip = (0xFC26_A004, 0x1F10_93FF);
        let t0_only = (0xFC12_1824, 0xFF33_FFFF); // no LOD_FRACTION
        let cases = [
            (list(2, false, 1, mip), [1.0, 2.0, 0.0, 0.0]),
            (list(2, true, 1, mip), [2.0, 2.0, 0.5, 0.0]),
            (list(2, true, 0, mip), [1.0, 2.0, 0.0, 0.0]), // tile 1 shift ≠ 1
            (list(0, false, 1, mip), [0.0; 4]),
            (list(2, false, 1, t0_only), [0.0; 4]),
        ];
        assert!(!CombinerKey::new(t0_only.0, t0_only.1, true).uses_lod_frac());
        for (i, (dl, lodp)) in cases.into_iter().enumerate() {
            let f = Interpreter::new().run(&mem, dl);
            let d = &f.draws[0];
            assert_eq!(d.uniforms.lodp, lodp, "case {i}");
            let chain = lodp[0] == 2.0;
            assert_eq!(f.chains.len(), chain as usize, "case {i}");
            if chain {
                let t = d.textures[0].unwrap();
                assert_eq!(d.textures[1], Some(t));
                let levels = &f.chains[&t.key];
                let sizes: Vec<_> = levels
                    .iter()
                    .map(|k| (f.textures[k].width, f.textures[k].height))
                    .collect();
                assert_eq!(sizes, [(8, 8), (4, 4), (2, 2)]);
                assert_eq!((t.width, t.shift, t.origin), (8, [1.0; 2], [-0.5; 2]));
            }
        }
    }

    /// A 1-triangle list through segments, a nested DL and matrices.
    #[test]
    fn runs_a_triangle_list() {
        let mut mem = VecMemory::default();
        let ident = mem.push(&matrix::to_fixed(&matrix::IDENTITY), 8);
        let mut vtx = Vec::new();
        for (x, y) in [(0i16, 0i16), (1, 0), (0, 1)] {
            for v in [x, y, 0, 0, 0, 0] {
                vtx.extend_from_slice(&v.to_be_bytes());
            }
            vtx.extend_from_slice(&[255, 0, 0, 255]);
        }
        let vaddr = mem.push(&vtx, 8);
        let inner = mem.push_words(&words(&[
            (0x0420_0030, 0x0300_0000 | (vaddr - 0x10)), // G_VTX 3 via segment 3
            (0xBF00_0000, 0x0000_0A14),                  // G_TRI1 0 1 2
            (0xB800_0000, 0),
        ]));
        let main = mem.push_words(&words(&[
            (0xBC00_0C06, 0x10),  // segment 3 = 0x10
            (0x0103_0040, ident), // projection load
            (0x0102_0040, ident), // modelview load
            (0xB700_0000, geom::G_SHADE | geom::G_SHADING_SMOOTH),
            (0x0600_0000, inner),
            (0xE700_0000, 0),
            (0x5500_0000, 0), // unknown opcode: logged, not fatal
            (0xB800_0000, 0),
        ]));
        let mut it = Interpreter::new();
        let f = it.run(&mem, main);
        assert_eq!(f.vertices.len(), 3);
        assert_eq!(f.draws.len(), 1);
        // Identity MVP + default viewport: (1, 0) → x = 160 + 160 = 320.
        assert_eq!(f.vertices[1].pos, [320.0, 120.0, 0.5, 1.0]);
        assert_eq!(f.vertices[0].color, [1.0, 0.0, 0.0, 1.0]);
        assert!(it.unknown_opcodes().contains(&0x55));
    }

    /// Widescreen classes: perspective → world (`Extend`; with `wide_tags`
    /// only inside a `WIDE_TAG_ON/OFF` bracket), affine untextured spanning
    /// the full width → `Stretch`, affine narrower → `Fixed`.
    #[test]
    fn wide_classes() {
        let mut mem = VecMemory::default();
        let ident = mem.push(&matrix::to_fixed(&matrix::IDENTITY), 8);
        let mut persp = matrix::IDENTITY;
        persp[2][3] = 1.0; // w = z + 1: perspective, same result at z = 0
        let persp = mem.push(&matrix::to_fixed(&persp), 8);
        let mut vtx = Vec::new();
        for (x, y) in [(-1i16, 0i16), (1, 0), (0, 1), (0, 0), (1, 0), (0, 1)] {
            for v in [x, y, 0, 0, 0, 0] {
                vtx.extend_from_slice(&v.to_be_bytes());
            }
            vtx.extend_from_slice(&[255, 255, 255, 255]);
        }
        let vaddr = mem.push(&vtx, 8);
        let tri = |proj: u32| {
            [
                (0x0103_0040, proj),
                (0x0102_0040, ident),
                (0x0450_0060, vaddr),       // G_VTX 6
                (0xBF00_0000, 0x0000_0A14), // (-1..1 wide) 0 1 2
                (0xBF00_0000, 0x001E_2832), // (0..1 wide) 3 4 5
            ]
        };
        let mut cmds = Vec::new();
        cmds.extend(tri(ident));
        cmds.extend(tri(persp));
        cmds.push((0xC000_0000, WIDE_TAG_ON));
        cmds.extend(tri(persp));
        cmds.push((0xC000_0000, WIDE_TAG_OFF));
        cmds.push((0xB800_0000, 0));
        let main = mem.push_words(&words(&cmds));
        let classes = |tags: bool| {
            let mut it = Interpreter::new();
            it.wide_tags = tags;
            let f = it.run(&mem, main);
            f.draws
                .iter()
                .map(|d| (d.wide, d.vertex_count))
                .collect::<Vec<_>>()
        };
        use Wide::*;
        assert_eq!(
            classes(false),
            [(Stretch([0.0, 320.0]), 3), (Fixed, 3), (Extend, 12)]
        );
        assert_eq!(
            classes(true),
            [(Stretch([0.0, 320.0]), 3), (Fixed, 9), (Extend, 6)]
        );

        // HUD anchor markers tag (and split) the following draws; unknown
        // anchor letters are ignored; each run starts centred.
        let tag = |c: u8| (0xC000_0000, ANCHOR_TAG | c as u32);
        let mut cmds = vec![tag(b'L')];
        cmds.extend(tri(ident));
        cmds.push(tag(b'R'));
        cmds.extend(tri(ident));
        cmds.push(tag(b'X'));
        cmds.extend(tri(ident));
        cmds.push((0xB800_0000, 0));
        let anchored = mem.push_words(&words(&cmds));
        let mut it = Interpreter::new();
        let f = it.run(&mem, anchored);
        let got: Vec<_> = f.draws.iter().map(|d| (d.anchor, d.vertex_count)).collect();
        use Anchor::*;
        // Stretch + Fixed per `tri`; 'X' keeps Right.
        assert_eq!(
            got,
            [
                (Left, 3),
                (Left, 3),
                (Right, 3),
                (Right, 3),
                (Right, 3),
                (Right, 3)
            ]
        );
        let f = it.run(&mem, main);
        assert!(f.draws.iter().all(|d| d.anchor == Centre));
        assert_eq!(anchor_tag(0x5057_4143), Some(Centre));
        assert_eq!(anchor_tag(WIDE_TAG_ON), None);

        // Vertical anchor markers: same encoding, independent "PWV" tag
        // space; reset per run; PWA and PWV words don't decode as each
        // other.
        let vtag = |c: u8| (0xC000_0000, VANCHOR_TAG | c as u32);
        let mut cmds = vec![vtag(b'T')];
        cmds.extend(tri(ident));
        cmds.push(vtag(b'B'));
        cmds.extend(tri(ident));
        cmds.push((0xB800_0000, 0));
        let vanchored = mem.push_words(&words(&cmds));
        let f = Interpreter::new().run(&mem, vanchored);
        let got: Vec<_> = f.draws.iter().map(|d| d.vanchor).collect();
        use VAnchor::*;
        assert_eq!(got, [Top, Top, Bottom, Bottom]);
        let f = it.run(&mem, main);
        assert!(f.draws.iter().all(|d| d.vanchor == Middle));
        assert_eq!(vanchor_tag(ANCHOR_TAG | b'L' as u32), None);
        assert_eq!(anchor_tag(VANCHOR_TAG | b'T' as u32), None);
        assert_eq!(vanchor_tag(0x5057_5642), Some(Bottom));

        // Flight-HUD tags ("PWH"): own tag space (disjoint from PWA/PWV),
        // brackets the following draws, resets per run.
        let mut cmds = vec![(0xC000_0000, HUD_TAG_ON)];
        cmds.extend(tri(ident));
        cmds.push((0xC000_0000, HUD_TAG_OFF));
        cmds.extend(tri(ident));
        cmds.push((0xB800_0000, 0));
        let tagged = mem.push_words(&words(&cmds));
        let f = Interpreter::new().run(&mem, tagged);
        let got: Vec<_> = f.draws.iter().map(|d| d.hud).collect();
        assert_eq!(got, [true, true, false, false]);
        let f = it.run(&mem, main);
        assert!(f.draws.iter().all(|d| !d.hud), "hud resets per run");
        assert_eq!(anchor_tag(HUD_TAG_ON), None);
        assert_eq!(vanchor_tag(HUD_TAG_ON), None);
        assert_eq!(anchor_tag(HUD_TAG_OFF), None);
        assert_eq!(vanchor_tag(HUD_TAG_OFF), None);
    }

    /// TEXRECT consumes exactly the two following half commands; the command
    /// after them still runs.
    #[test]
    fn tex_rect_consumes_two_halves() {
        let rect = [
            (0xE400_A028, 0x0000_0000), // TEXRECT 0,0 .. 40,10
            (0xB300_0000, 0x0000_0000), // RDPHALF_1 (sprite.c numbering): s, t
            (0xB200_0000, 0x0400_0400), // RDPHALF_2: dsdx, dtdy = 1.0
        ];
        let mut mem = VecMemory::default();
        let one = mem.push_words(&words(&[rect[0], rect[1], rect[2], (0xB800_0000, 0)]));
        let two = mem.push_words(&words(&[
            rect[0],
            rect[1],
            rect[2],
            (0xF6_4FC3BC, 0), // FILLRECT: must not be skipped
            (0xB800_0000, 0),
        ]));
        let n1 = Interpreter::new().run(&mem, one).vertices.len();
        let n2 = Interpreter::new().run(&mem, two).vertices.len();
        assert!(n1 > 0);
        assert_eq!(n2, 2 * n1);
    }

    /// A host whose `read_bytes` swaps u16s (native LE structs) must still
    /// see `Vtx.cn` bytes in order: they come through `read_raw`.
    #[test]
    fn vertex_color_bytes_are_read_raw() {
        struct HostLe(VecMemory);
        impl Memory for HostLe {
            fn read_u32(&self, addr: u32) -> u32 {
                self.0.read_u32(addr)
            }
            fn read_bytes(&self, addr: u32, out: &mut [u8]) {
                self.0.read_bytes(addr, out);
                for p in out.as_chunks_mut::<2>().0 {
                    p.swap(0, 1);
                }
            }
            fn read_raw(&self, addr: u32, out: &mut [u8]) {
                self.0.read_bytes(addr, out);
            }
        }
        let mut mem = VecMemory::default();
        // One LE-stored vertex (u16 fields swapped), cn = 10, 20, 30, 40.
        let mut vtx = Vec::new();
        for v in [0i16, 0, 0, 0, 0, 0] {
            vtx.extend_from_slice(&v.to_le_bytes());
        }
        vtx.extend_from_slice(&[10, 20, 30, 40]);
        let vaddr = mem.push(&vtx, 8);
        let main = mem.push_words(&words(&[(0x0400_0010, vaddr), (0xB800_0000, 0)]));
        let mut it = Interpreter::new();
        it.run(&HostLe(mem), main);
        let c = it.vtx[0].color;
        assert_eq!(c.map(|x| (x * 255.0).round() as u8), [10, 20, 30, 40]);
    }

    /// Bilinear texrect (fractional S, so it stays linear): pixel centers
    /// hit the RDP's top-left S/T (shifted half a pixel back) and sampling
    /// is clamped to the first/last pixel's S/T, so neighbouring rows in the
    /// tile (font sheet glyphs, garbage TMEM) never bleed in.
    #[test]
    fn bilinear_tex_rect_samples_only_its_texels() {
        let mut mem = VecMemory::default();
        let main = mem.push_words(&words(&[
            (0xBA00_1402, 0),           // 1-cycle
            (0xBA00_0C02, 0x2000),      // G_TF_BILERP
            (0xE402_8028, 0x0001_4014), // TEXRECT (5,5)..(10,10)
            (0xB400_0000, 0x0050_0020), // s = 2.5, t = 1
            (0xB300_0000, 0x0400_0400), // 1:1
            (0xB800_0000, 0),
        ]));
        let f = Interpreter::new().run(&mem, main);
        let v = &f.vertices[0];
        assert_eq!(v.pos[..2], [5.0, 5.0]);
        assert_eq!(v.st, [2.0, 0.5]);
        // 5×5 pixels: S 2.5..=6.5 × T 1..=5.
        assert_eq!(v.st_clamp, [2.5, 1.0, 6.5, 5.0]);
    }

    /// A 1:1 bilinear texrect whose pixel corners land on texel centers (the
    /// sprite library's blits/fonts) binds point sampling: the RDP never
    /// blends there, and linear would smear the up-scaled render — IA blits'
    /// white "invisible" background bleeds bright halos around the glyphs.
    #[test]
    fn one_to_one_texrect_binds_point_tiles() {
        let mut mem = VecMemory::default();
        let common = [
            (0xBA00_1402, 0),           // 1-cycle
            (0xBA00_0C02, 0x2000),      // G_TF_BILERP
            (0xFC12_1824, 0xFF33_FFFF), // combiner reads TEXEL0
            (0xF510_0000, 0),           // SETTILE 0: RGBA 16b, tmem 0
            (0xF200_0000, 0x0001_C01C), // SETTILESIZE 0: 8×8
        ];
        // Integral rect origin and S/T (ulx = 5, s = 2, t = 1).
        let mut list = common.to_vec();
        list.extend([
            (0xE402_8028, 0x0001_4014), // TEXRECT (5,5)..(10,10)
            (0xB400_0000, 0x0040_0020), // s = 2, t = 1
            (0xB300_0000, 0x0400_0400), // 1:1
            (0xB800_0000, 0),
        ]);
        let main = mem.push_words(&words(&list));
        let f = Interpreter::new().run(&mem, main);
        assert!(
            !f.draws[0].textures[0].as_ref().unwrap().sampler.linear,
            "1:1 integral rect must bind point tiles"
        );
        // Point tiles span [k, k+1): the pixel area maps onto its whole
        // texel (no half-texel shift), clamped to the edge texels' centers.
        let v = &f.vertices[0];
        assert_eq!(v.st, [2.0, 1.0]);
        assert_eq!(v.st_clamp, [2.5, 1.5, 6.5, 5.5]);
        // Mirrored (dsdx = -1): pixel 0 still shows texel s = 2 — the
        // corner S runs 3 → 2 across it.
        let mut list = common.to_vec();
        list.extend([
            (0xE402_8028, 0x0001_4014),
            (0xB400_0000, 0x0040_0020), // s = 2, t = 1
            (0xB300_0000, 0xFC00_0400), // dsdx = -1
            (0xB800_0000, 0),
        ]);
        let main = mem.push_words(&words(&list));
        let f = Interpreter::new().run(&mem, main);
        assert!(!f.draws[0].textures[0].as_ref().unwrap().sampler.linear);
        let v = &f.vertices[0];
        assert_eq!(v.st, [3.0, 1.0]);
        assert_eq!(v.st_clamp, [-1.5, 1.5, 2.5, 5.5]);
        // Fractional S: the RDP blends there, so the tile stays linear.
        let mut list = common.to_vec();
        list.extend([
            (0xE402_8028, 0x0001_4014),
            (0xB400_0000, 0x0030_0000), // s = 1.5
            (0xB300_0000, 0x0400_0400),
            (0xB800_0000, 0),
        ]);
        let main = mem.push_words(&words(&list));
        let f = Interpreter::new().run(&mem, main);
        assert!(f.draws[0].textures[0].as_ref().unwrap().sampler.linear);
    }

    /// Clip ratio 1 (`gSPClipRatio(FRUSTRATIO_1)`): 3D triangles stay in the
    /// viewport even under a full-screen scissor; rects keep the scissor.
    #[test]
    fn clip_ratio_bounds_3d_draws() {
        let mut mem = VecMemory::default();
        // Vp: scale (100, 80), translate (160, 120), in quarter pixels.
        let mut vp = Vec::new();
        for v in [400i16, 320, 511, 0, 640, 480, 511, 0] {
            vp.extend_from_slice(&v.to_be_bytes());
        }
        let vp = mem.push(&vp, 8);
        let ident = mem.push(&matrix::to_fixed(&matrix::IDENTITY), 8);
        let main = mem.push_words(&words(&[
            (0x0380_0010, vp),
            (0xBC00_0404, 1), // clip ratio 1
            (0x0103_0040, ident),
            (0x0102_0040, ident),
            (0x0420_0030, ident), // any 3 vertices
            (0xBF00_0000, 0x0000_0A14),
            (0xF64F_C3BC, 0), // FILLRECT
            (0xB800_0000, 0),
        ]));
        let f = Interpreter::new().run(&mem, main);
        assert_eq!(f.draws[0].scissor, [60.0, 40.0, 260.0, 200.0]);
        assert_eq!(f.draws[1].scissor, [0.0, 0.0, 320.0, 240.0]);
    }

    #[test]
    fn fill_rect_on_depth_image_clears_depth() {
        let mut mem = VecMemory::default();
        let main = mem.push_words(&words(&[
            (0xBA00_1402, 0x0030_0000), // cycle type = fill
            (0xFF10_013F, 0x1000),      // SETCIMG
            (0xFE00_0000, 0x1000),      // SETZIMG, same address
            (0xF6_4FC3BC, 0),           // FILLRECT 0,0 .. 319,239
            (0xB800_0000, 0),
        ]));
        let f = Interpreter::new().run(&mem, main);
        assert_eq!(f.draws.len(), 1);
        assert_eq!(f.draws[0].pipeline.shader.mode, ShaderMode::DepthClear);
        assert_eq!(f.vertices[2].pos[0], 320.0);
    }
}
