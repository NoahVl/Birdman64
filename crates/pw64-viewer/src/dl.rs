//! A tiny F3D display-list builder (the `gs*`/`g*` macros of gbi.h, plain
//! F3D encodings) plus the constants the engine's `graphics.c` uses.

use pw64_formats::gbi::op;
use pw64_gfx::combiner::{CombinerKey, Cycle};

/// Set bit in a physical address the way the engine's pointers look (K0).
pub const K0: u32 = 0x8000_0000;

// Render-mode flags (gbi.h).
pub const AA_EN: u32 = 0x8;
pub const Z_CMP: u32 = 0x10;
pub const Z_UPD: u32 = 0x20;
pub const IM_RD: u32 = 0x40;
pub const CLR_ON_CVG: u32 = 0x80;
pub const CVG_DST_WRAP: u32 = 0x100;
pub const CVG_DST_FULL: u32 = 0x200;
pub const ZMODE_INTER: u32 = 0x400;
pub const ZMODE_XLU: u32 = 0x800;
pub const ZMODE_DEC: u32 = 0xC00;
pub const CVG_X_ALPHA: u32 = 0x1000;
pub const ALPHA_CVG_SEL: u32 = 0x2000;
pub const FORCE_BL: u32 = 0x4000;

const fn gbl_c2(p: u32, a: u32, m: u32, b: u32) -> u32 {
    (p << 28) | (a << 24) | (m << 20) | (b << 16)
}
const fn gbl_c1(p: u32, a: u32, m: u32, b: u32) -> u32 {
    (p << 30) | (a << 26) | (m << 22) | (b << 18)
}
// Blender inputs.
const IN: u32 = 0;
const MEM: u32 = 1;
const FOG: u32 = 3;
const A_IN: u32 = 0;
const A_SHADE: u32 = 2;
const A0: u32 = 3;
const ONE_MA: u32 = 0;
const A_MEM: u32 = 1;
const ONE: u32 = 2;
const BL_BLEND: u32 = gbl_c2(IN, A_IN, MEM, ONE_MA);
const BL_OPA: u32 = gbl_c2(IN, A_IN, MEM, A_MEM);

pub const G_RM_PASS: u32 = gbl_c1(IN, A0, IN, ONE);
pub const G_RM_FOG_SHADE_A: u32 = gbl_c1(FOG, A_SHADE, IN, ONE_MA);
pub const G_RM_OPA_SURF: u32 = FORCE_BL | gbl_c1(IN, A0, IN, ONE);
pub const G_RM_OPA_SURF2: u32 = FORCE_BL | gbl_c2(IN, A0, IN, ONE);
pub const RM_AA_ZB_XLU_DECAL2: u32 =
    AA_EN | Z_CMP | IM_RD | CVG_DST_WRAP | CLR_ON_CVG | FORCE_BL | ZMODE_DEC | BL_BLEND;
pub const RM_ZB_XLU_DECAL2: u32 = Z_CMP | IM_RD | CVG_DST_FULL | FORCE_BL | ZMODE_DEC | BL_BLEND;
pub const RM_AA_ZB_OPA_DECAL2: u32 =
    AA_EN | Z_CMP | IM_RD | CVG_DST_WRAP | ALPHA_CVG_SEL | ZMODE_DEC | BL_OPA;
pub const RM_ZB_OPA_DECAL2: u32 = Z_CMP | CVG_DST_FULL | ALPHA_CVG_SEL | ZMODE_DEC | BL_OPA;
pub const RM_AA_ZB_XLU_INTER2: u32 =
    AA_EN | Z_CMP | IM_RD | CVG_DST_WRAP | CLR_ON_CVG | FORCE_BL | ZMODE_INTER | BL_BLEND;
pub const RM_AA_ZB_XLU_SURF2: u32 =
    AA_EN | Z_CMP | IM_RD | CVG_DST_WRAP | CLR_ON_CVG | FORCE_BL | ZMODE_XLU | BL_BLEND;
pub const RM_AA_ZB_TEX_TERR2: u32 =
    AA_EN | Z_CMP | Z_UPD | IM_RD | CVG_X_ALPHA | ALPHA_CVG_SEL | BL_BLEND;
pub const RM_AA_XLU_SURF2: u32 = AA_EN | IM_RD | CVG_DST_WRAP | CLR_ON_CVG | FORCE_BL | BL_BLEND;
pub const RM_AA_TEX_TERR2: u32 = AA_EN | IM_RD | CVG_X_ALPHA | ALPHA_CVG_SEL | BL_BLEND;
pub const RM_ZB_XLU_SURF2: u32 = Z_CMP | IM_RD | CVG_DST_FULL | FORCE_BL | ZMODE_XLU | BL_BLEND;
pub const RM_AA_ZB_OPA_TERR2: u32 = AA_EN | Z_CMP | Z_UPD | IM_RD | ALPHA_CVG_SEL | BL_BLEND;
pub const RM_ZB_OPA_SURF2: u32 = Z_CMP | Z_UPD | CVG_DST_FULL | ALPHA_CVG_SEL | BL_OPA;
pub const RM_AA_ZB_OPA_SURF2: u32 = AA_EN | Z_CMP | Z_UPD | IM_RD | ALPHA_CVG_SEL | BL_OPA;
pub const RM_AA_OPA_TERR2: u32 = AA_EN | IM_RD | ALPHA_CVG_SEL | BL_BLEND;
pub const RM_XLU_SURF2: u32 = IM_RD | CVG_DST_FULL | FORCE_BL | BL_BLEND;

// Color-combiner selectors ("0" differs per slot).
const C_COMBINED: u8 = 0;
const C_TEXEL0: u8 = 1;
const C_SHADE: u8 = 4;
const CA0: u8 = 15;
const CC0: u8 = 31;
const CD0: u8 = 7;
const AZ: u8 = 7;
/// Alpha mux "1" (same 8-value encoding as `AZ`).
const A_ONE: u8 = 6;

pub const CC_SHADE: Cycle = Cycle {
    rgb: [CA0, CA0, CC0, C_SHADE],
    alpha: [AZ, AZ, AZ, C_SHADE],
};
pub const CC_PASS2: Cycle = Cycle {
    rgb: [CA0, CA0, CC0, C_COMBINED],
    alpha: [AZ, AZ, AZ, C_COMBINED],
};
/// `gDPSetCombineLERP(0,0,0,0, 0,0,0,1, 0,0,0,COMBINED, 0,0,0,COMBINED)`:
/// rgb black, alpha 1 (`uvGfxStateDrawDL`'s z-image pass → the blender's
/// RGBA5551 word `0x0001` = depth nearest).
pub const CC_BLACK_A1: Cycle = Cycle {
    rgb: [CA0, CA0, CC0, CD0],
    alpha: [AZ, AZ, AZ, A_ONE],
};
pub const CC_MODULATEIDECALA: Cycle = Cycle {
    rgb: [C_TEXEL0, CA0, C_SHADE, CD0],
    alpha: [AZ, AZ, AZ, C_TEXEL0],
};

/// A display list under construction (pairs of big-endian words).
#[derive(Default, Clone)]
pub struct Dl(pub Vec<u32>);

impl Dl {
    fn cmd(&mut self, w0: u32, w1: u32) -> &mut Self {
        self.0.extend([w0, w1]);
        self
    }
    fn op(o: u8) -> u32 {
        (o as u32) << 24
    }
    pub fn segment(&mut self, seg: u32, base: u32) -> &mut Self {
        // gsMoveWd(G_MW_SEGMENT, seg * 4, base): offset << 8 | index.
        self.cmd(Self::op(op::G_MOVEWORD) | ((seg * 4) << 8) | 0x06, base)
    }
    pub fn fog_position(&mut self, min: i32, max: i32) -> &mut Self {
        let fm = 128000 / (max - min);
        let fo = (500 - min) * 256 / (max - min);
        self.cmd(
            Self::op(op::G_MOVEWORD) | 0x08,
            ((fm as u32) << 16) | (fo as u32 & 0xFFFF),
        )
    }
    pub fn matrix(&mut self, addr: u32, params: u8) -> &mut Self {
        self.cmd(
            Self::op(op::G_MTX) | ((params as u32) << 16) | 64,
            addr | K0,
        )
    }
    pub fn pop_matrix(&mut self) -> &mut Self {
        self.cmd(Self::op(op::G_POPMTX), 0)
    }
    pub fn viewport(&mut self, addr: u32) -> &mut Self {
        self.cmd(Self::op(op::G_MOVEMEM) | (0x80 << 16) | 16, addr | K0)
    }
    pub fn display_list(&mut self, addr: u32) -> &mut Self {
        self.cmd(Self::op(op::G_DL), addr | K0)
    }
    pub fn end(&mut self) -> &mut Self {
        self.cmd(Self::op(op::G_ENDDL), 0)
    }
    pub fn set_geometry(&mut self, m: u32) -> &mut Self {
        self.cmd(Self::op(op::G_SETGEOMETRYMODE), m)
    }
    pub fn clear_geometry(&mut self, m: u32) -> &mut Self {
        self.cmd(Self::op(op::G_CLEARGEOMETRYMODE), m)
    }
    pub fn texture_off(&mut self) -> &mut Self {
        self.cmd(Self::op(op::G_TEXTURE), 0)
    }
    pub fn pipe_sync(&mut self) -> &mut Self {
        self.cmd(Self::op(op::G_RDPPIPESYNC), 0)
    }
    pub fn full_sync(&mut self) -> &mut Self {
        self.cmd(Self::op(op::G_RDPFULLSYNC), 0)
    }
    pub fn othermode_h(&mut self, shift: u32, len: u32, data: u32) -> &mut Self {
        self.cmd(Self::op(op::G_SETOTHERMODE_H) | (shift << 8) | len, data)
    }
    pub fn othermode_l(&mut self, shift: u32, len: u32, data: u32) -> &mut Self {
        self.cmd(Self::op(op::G_SETOTHERMODE_L) | (shift << 8) | len, data)
    }
    /// `gDPSetCycleType`: 0 = 1-cycle, 1 = 2-cycle, 2 = copy, 3 = fill.
    pub fn cycle_type(&mut self, c: u32) -> &mut Self {
        self.othermode_h(20, 2, c << 20)
    }
    pub fn render_mode(&mut self, c1: u32, c2: u32) -> &mut Self {
        self.othermode_l(3, 29, c1 | c2)
    }
    pub fn combine(&mut self, c0: Cycle, c1: Cycle) -> &mut Self {
        let (w0, w1) = CombinerKey::encode([c0, c1]);
        self.cmd(w0, w1)
    }
    pub fn fill_color(&mut self, rgba5551: u16) -> &mut Self {
        self.cmd(
            Self::op(op::G_SETFILLCOLOR),
            ((rgba5551 as u32) << 16) | rgba5551 as u32,
        )
    }
    pub fn fog_color(&mut self, rgb: [u8; 3]) -> &mut Self {
        self.cmd(
            Self::op(op::G_SETFOGCOLOR),
            u32::from_be_bytes([rgb[0], rgb[1], rgb[2], 255]),
        )
    }
    pub fn color_image(&mut self, addr: u32) -> &mut Self {
        // RGBA 16b, width 320.
        self.cmd(Self::op(op::G_SETCIMG) | (2 << 19) | 319, addr)
    }
    pub fn depth_image(&mut self, addr: u32) -> &mut Self {
        self.cmd(Self::op(op::G_SETZIMG), addr)
    }
    pub fn scissor(&mut self, x0: u32, y0: u32, x1: u32, y1: u32) -> &mut Self {
        self.cmd(
            Self::op(op::G_SETSCISSOR) | ((x0 * 4) << 12) | (y0 * 4),
            ((x1 * 4) << 12) | (y1 * 4),
        )
    }
    /// `gDPSetTileSize` (coordinates in 10.2 fixed point).
    pub fn tile_size(&mut self, tile: u32, uls: i32, ult: i32, lrs: i32, lrt: i32) -> &mut Self {
        let f = |v: i32| v as u32 & 0xFFF;
        self.cmd(
            Self::op(op::G_SETTILESIZE) | (f(uls) << 12) | f(ult),
            (tile << 24) | (f(lrs) << 12) | f(lrt),
        )
    }
    /// `gSPVertex` (F3D encoding).
    pub fn vertex(&mut self, addr: u32, n: u32, v0: u32) -> &mut Self {
        self.cmd(
            Self::op(op::G_VTX) | ((((n - 1) << 4) | v0) << 16) | (n * 16),
            addr | K0,
        )
    }
    /// `gSP1Triangle` (F3D encoding, flag 0).
    pub fn tri1(&mut self, a: u32, b: u32, c: u32) -> &mut Self {
        self.cmd(
            Self::op(op::G_TRI1),
            ((a * 10) << 16) | ((b * 10) << 8) | (c * 10),
        )
    }
    pub fn fill_rect(&mut self, x0: u32, y0: u32, x1: u32, y1: u32) -> &mut Self {
        self.cmd(
            Self::op(op::G_FILLRECT) | ((x1 * 4) << 12) | (y1 * 4),
            ((x0 * 4) << 12) | (y0 * 4),
        )
    }
}

/// `GPACK_RGBA5551`.
pub fn rgba5551(r: u8, g: u8, b: u8, a: u8) -> u16 {
    ((r as u16 >> 3) << 11) | ((g as u16 >> 3) << 6) | ((b as u16 >> 3) << 1) | (a as u16 & 1)
}
