//! N64 display-list (GBI) command decoding.
//!
//! Pilotwings 64 uses the F3D-family GBI (not F3DEX2): `G_ENDDL` = `0xB8`,
//! `G_TEXTURE` = `0xBB`, `G_SETOTHERMODE_H` = `0xBA`. Field layouts follow
//! `decomp/include/libultra/PR/gbi.h` (its plain F3D branch: neither
//! `F3DEX_GBI` nor `F3DEX_GBI_2`). Commands used by textures and models are
//! decoded into typed variants; everything else is kept as [`Gfx::Other`].
//! Plain F3D has a 16-entry vertex cache and no `G_TRI2`.

use std::fmt;

/// Opcodes (first byte of word 0).
pub mod op {
    /// Declares the opcode constants plus [`name`] from one table.
    macro_rules! opcodes {
        ($($name:ident = $v:literal,)*) => {
            $(pub const $name: u8 = $v;)*
            /// Macro name of an opcode (e.g. `"G_VTX"`), `None` if unknown.
            pub fn name(op: u8) -> Option<&'static str> {
                match op {
                    $($v => Some(stringify!($name)),)*
                    _ => None,
                }
            }
        };
    }
    opcodes! {
        // RSP DMA commands.
        G_SPNOOP = 0x00,
        G_MTX = 0x01,
        G_MOVEMEM = 0x03,
        G_VTX = 0x04,
        G_DL = 0x06,
        // RSP immediate commands (G_IMMFIRST - n).
        G_RDPHALF_CONT = 0xB2,
        G_RDPHALF_2 = 0xB3,
        G_RDPHALF_1 = 0xB4,
        G_LINE3D = 0xB5,
        G_CLEARGEOMETRYMODE = 0xB6,
        G_SETGEOMETRYMODE = 0xB7,
        G_ENDDL = 0xB8,
        G_SETOTHERMODE_L = 0xB9,
        G_SETOTHERMODE_H = 0xBA,
        G_TEXTURE = 0xBB,
        G_MOVEWORD = 0xBC,
        G_POPMTX = 0xBD,
        G_CULLDL = 0xBE,
        G_TRI1 = 0xBF,
        // RDP commands.
        G_NOOP = 0xC0,
        G_TEXRECT = 0xE4,
        G_TEXRECTFLIP = 0xE5,
        G_RDPLOADSYNC = 0xE6,
        G_RDPPIPESYNC = 0xE7,
        G_RDPTILESYNC = 0xE8,
        G_RDPFULLSYNC = 0xE9,
        G_SETSCISSOR = 0xED,
        G_SETPRIMDEPTH = 0xEE,
        G_RDPSETOTHERMODE = 0xEF,
        G_LOADTLUT = 0xF0,
        G_SETTILESIZE = 0xF2,
        G_LOADBLOCK = 0xF3,
        G_LOADTILE = 0xF4,
        G_SETTILE = 0xF5,
        G_FILLRECT = 0xF6,
        G_SETFILLCOLOR = 0xF7,
        G_SETFOGCOLOR = 0xF8,
        G_SETBLENDCOLOR = 0xF9,
        G_SETPRIMCOLOR = 0xFA,
        G_SETENVCOLOR = 0xFB,
        G_SETCOMBINE = 0xFC,
        G_SETTIMG = 0xFD,
        G_SETZIMG = 0xFE,
        G_SETCIMG = 0xFF,
    }
}

/// Geometry-mode bits for `G_SETGEOMETRYMODE` / `G_CLEARGEOMETRYMODE` (F3D values).
pub mod geom {
    pub const G_ZBUFFER: u32 = 0x0000_0001;
    pub const G_TEXTURE_ENABLE: u32 = 0x0000_0002;
    pub const G_SHADE: u32 = 0x0000_0004;
    pub const G_SHADING_SMOOTH: u32 = 0x0000_0200;
    pub const G_CULL_FRONT: u32 = 0x0000_1000;
    pub const G_CULL_BACK: u32 = 0x0000_2000;
    pub const G_FOG: u32 = 0x0001_0000;
    pub const G_LIGHTING: u32 = 0x0002_0000;
    pub const G_TEXTURE_GEN: u32 = 0x0004_0000;
    pub const G_TEXTURE_GEN_LINEAR: u32 = 0x0008_0000;
    pub const G_LOD: u32 = 0x0010_0000;
}

/// `G_MTX` parameter bits (F3D values; they differ in F3DEX2).
pub mod mtx {
    pub const G_MTX_PROJECTION: u8 = 0x01;
    pub const G_MTX_LOAD: u8 = 0x02;
    pub const G_MTX_PUSH: u8 = 0x04;
}

/// Size of one `Vtx` in bytes (the unit of `G_VTX` lengths).
pub const VTX_SIZE: u32 = 16;
/// Entries in the F3D vertex cache (indices are 4-bit).
pub const VTX_CACHE_SIZE: usize = 16;

/// `G_MDSFT_TEXTLUT`: shift of the TLUT-mode field in the othermode-H word.
pub const G_MDSFT_TEXTLUT: u8 = 14;

/// Tile index conventionally used for loads (`G_TX_LOADTILE`).
pub const G_TX_LOADTILE: u8 = 7;

/// Texel format (`G_IM_FMT_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ImFmt {
    Rgba,
    Yuv,
    Ci,
    Ia,
    I,
    /// Values 5..7 are invalid on hardware.
    Invalid(u8),
}

impl ImFmt {
    pub fn from_bits(v: u32) -> Self {
        match v & 7 {
            0 => Self::Rgba,
            1 => Self::Yuv,
            2 => Self::Ci,
            3 => Self::Ia,
            4 => Self::I,
            n => Self::Invalid(n as u8),
        }
    }
}

/// Texel size (`G_IM_SIZ_*`), in bits per texel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ImSiz {
    B4,
    B8,
    B16,
    B32,
}

impl ImSiz {
    pub fn from_bits(v: u32) -> Self {
        match v & 3 {
            0 => Self::B4,
            1 => Self::B8,
            2 => Self::B16,
            _ => Self::B32,
        }
    }
    pub fn bits(self) -> u32 {
        4 << self as u32
    }
}

/// e.g. `RGBA16`, `CI4`, `IA8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TexFormat(pub ImFmt, pub ImSiz);

impl fmt::Display for TexFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.0 {
            ImFmt::Rgba => "RGBA",
            ImFmt::Yuv => "YUV",
            ImFmt::Ci => "CI",
            ImFmt::Ia => "IA",
            ImFmt::I => "I",
            ImFmt::Invalid(_) => "FMT?",
        };
        write!(f, "{name}{}", self.1.bits())
    }
}

/// Tile wrap mode bits (`G_TX_MIRROR` = 1, `G_TX_CLAMP` = 2; 0 = wrap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WrapMode {
    pub mirror: bool,
    pub clamp: bool,
}

impl WrapMode {
    fn from_bits(v: u32) -> Self {
        Self {
            mirror: v & 1 != 0,
            clamp: v & 2 != 0,
        }
    }
}

/// A tile descriptor as set by `G_SETTILE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileDesc {
    pub format: TexFormat,
    /// Row stride in 64-bit TMEM words.
    pub line: u16,
    /// TMEM address in 64-bit words (0..512).
    pub tmem: u16,
    pub tile: u8,
    /// CI4 palette (selects 16 TLUT entries).
    pub palette: u8,
    pub cmt: WrapMode,
    pub maskt: u8,
    pub shiftt: u8,
    pub cms: WrapMode,
    pub masks: u8,
    pub shifts: u8,
}

/// A decoded GBI command. Coordinates (`uls`, `lrs`, ...) are raw 10.2 fixed point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gfx {
    /// `gSPVertex`: load `count` vertices from `addr` into cache slots `v0..`.
    Vertex {
        count: u8,
        v0: u8,
        addr: u32,
    },
    /// `gSP1Triangle`: cache indices of the corners; `flag` picks the
    /// flat-shading vertex.
    Tri1 {
        v: [u8; 3],
        flag: u8,
    },
    /// `gSPMatrix` (`params`: [`mtx`] bits).
    Matrix {
        params: u8,
        addr: u32,
    },
    /// `gSPPopMatrix`.
    PopMatrix,
    /// `gSPDisplayList`; `branch` = `gSPBranchList` (no return).
    DisplayList {
        branch: bool,
        addr: u32,
    },
    /// `gSPSetGeometryMode` ([`geom`] bits).
    SetGeometryMode(u32),
    /// `gSPClearGeometryMode` ([`geom`] bits).
    ClearGeometryMode(u32),
    /// `gSPTexture`: enables texturing, picks the render tile and mip count.
    Texture {
        /// Number of extra mip levels (`levels - 1`).
        level: u8,
        tile: u8,
        on: bool,
        /// S/T scale, 0.16 fixed point.
        scale_s: u16,
        scale_t: u16,
    },
    /// `gDPSetTextureImage`: DRAM source for following loads.
    SetTImg {
        format: TexFormat,
        /// Width in texels (only used by `LoadTile`).
        width: u16,
        addr: u32,
    },
    SetTile(TileDesc),
    /// `gDPLoadBlock`: copy `lrs - uls + 1` texels linearly into TMEM.
    /// `dxt` (1.11 fixed) is the per-64-bit-word line increment; 0 means the
    /// data is already stored in TMEM (odd-row swapped) order.
    LoadBlock {
        tile: u8,
        uls: u16,
        ult: u16,
        lrs: u16,
        dxt: u16,
    },
    /// `gDPLoadTile`: copy a rectangle into TMEM.
    LoadTile {
        tile: u8,
        uls: u16,
        ult: u16,
        lrs: u16,
        lrt: u16,
    },
    /// `gDPLoadTLUT`: load `count` 16-bit palette entries.
    LoadTlut {
        tile: u8,
        count: u16,
    },
    SetTileSize {
        tile: u8,
        uls: u16,
        ult: u16,
        lrs: u16,
        lrt: u16,
    },
    SetOtherModeH {
        shift: u8,
        len: u8,
        data: u32,
    },
    SetOtherModeL {
        shift: u8,
        len: u8,
        data: u32,
    },
    SetCombine {
        w0: u32,
        w1: u32,
    },
    SetPrimColor {
        min_level: u8,
        lod_frac: u8,
        rgba: [u8; 4],
    },
    SetEnvColor {
        rgba: [u8; 4],
    },
    LoadSync,
    PipeSync,
    TileSync,
    EndDl,
    Other {
        w0: u32,
        w1: u32,
    },
}

fn bits(v: u32, shift: u32, len: u32) -> u32 {
    (v >> shift) & ((1 << len) - 1)
}

impl Gfx {
    pub fn decode(w0: u32, w1: u32) -> Self {
        let tile = bits(w1, 24, 3) as u8;
        let (uls, ult) = (bits(w0, 12, 12) as u16, bits(w0, 0, 12) as u16);
        let (lrs, lrt) = (bits(w1, 12, 12) as u16, bits(w1, 0, 12) as u16);
        match (w0 >> 24) as u8 {
            // F3D gSPVertex: w0 = cmd | ((n-1)<<4 | v0) << 16 | n*sizeof(Vtx).
            op::G_VTX => Self::Vertex {
                count: bits(w0, 20, 4) as u8 + 1,
                v0: bits(w0, 16, 4) as u8,
                addr: w1,
            },
            // F3D gSP1Triangle: indices are stored multiplied by 10.
            op::G_TRI1 => Self::Tri1 {
                v: [
                    (bits(w1, 16, 8) / 10) as u8,
                    (bits(w1, 8, 8) / 10) as u8,
                    (bits(w1, 0, 8) / 10) as u8,
                ],
                flag: bits(w1, 24, 8) as u8,
            },
            op::G_MTX => Self::Matrix {
                params: bits(w0, 16, 8) as u8,
                addr: w1,
            },
            op::G_POPMTX => Self::PopMatrix,
            op::G_DL => Self::DisplayList {
                branch: bits(w0, 16, 8) != 0,
                addr: w1,
            },
            op::G_SETGEOMETRYMODE => Self::SetGeometryMode(w1),
            op::G_CLEARGEOMETRYMODE => Self::ClearGeometryMode(w1),
            op::G_TEXTURE => Self::Texture {
                level: bits(w0, 11, 3) as u8,
                tile: bits(w0, 8, 3) as u8,
                on: bits(w0, 0, 8) != 0,
                scale_s: (w1 >> 16) as u16,
                scale_t: w1 as u16,
            },
            op::G_SETTIMG => Self::SetTImg {
                format: TexFormat(ImFmt::from_bits(w0 >> 21), ImSiz::from_bits(w0 >> 19)),
                width: bits(w0, 0, 12) as u16 + 1,
                addr: w1,
            },
            op::G_SETTILE => Self::SetTile(TileDesc {
                format: TexFormat(ImFmt::from_bits(w0 >> 21), ImSiz::from_bits(w0 >> 19)),
                line: bits(w0, 9, 9) as u16,
                tmem: bits(w0, 0, 9) as u16,
                tile,
                palette: bits(w1, 20, 4) as u8,
                cmt: WrapMode::from_bits(bits(w1, 18, 2)),
                maskt: bits(w1, 14, 4) as u8,
                shiftt: bits(w1, 10, 4) as u8,
                cms: WrapMode::from_bits(bits(w1, 8, 2)),
                masks: bits(w1, 4, 4) as u8,
                shifts: bits(w1, 0, 4) as u8,
            }),
            op::G_LOADBLOCK => Self::LoadBlock {
                tile,
                uls,
                ult,
                lrs,
                dxt: lrt,
            },
            op::G_LOADTILE => Self::LoadTile {
                tile,
                uls,
                ult,
                lrs,
                lrt,
            },
            // gsDPLoadTLUTCmd puts (count - 1) at bit 14 (= lrs in 10.2).
            op::G_LOADTLUT => Self::LoadTlut {
                tile,
                count: bits(w1, 14, 10) as u16 + 1,
            },
            op::G_SETTILESIZE => Self::SetTileSize {
                tile,
                uls,
                ult,
                lrs,
                lrt,
            },
            op::G_SETOTHERMODE_H | op::G_SETOTHERMODE_L => {
                let (shift, len) = (bits(w0, 8, 8) as u8, bits(w0, 0, 8) as u8);
                if w0 >> 24 == op::G_SETOTHERMODE_H as u32 {
                    Self::SetOtherModeH {
                        shift,
                        len,
                        data: w1,
                    }
                } else {
                    Self::SetOtherModeL {
                        shift,
                        len,
                        data: w1,
                    }
                }
            }
            op::G_SETCOMBINE => Self::SetCombine { w0, w1 },
            op::G_SETPRIMCOLOR => Self::SetPrimColor {
                min_level: bits(w0, 8, 8) as u8,
                lod_frac: bits(w0, 0, 8) as u8,
                rgba: w1.to_be_bytes(),
            },
            op::G_SETENVCOLOR => Self::SetEnvColor {
                rgba: w1.to_be_bytes(),
            },
            op::G_RDPLOADSYNC => Self::LoadSync,
            op::G_RDPPIPESYNC => Self::PipeSync,
            op::G_RDPTILESYNC => Self::TileSync,
            op::G_ENDDL => Self::EndDl,
            _ => Self::Other { w0, w1 },
        }
    }

    /// Encodes the geometry commands the engine builds at load time (mirrors
    /// the `gSPVertex` / `gSP1Triangle` / `gSPEndDisplayList` macros).
    /// Returns `None` for anything else.
    pub fn encode(&self) -> Option<(u32, u32)> {
        let cmd = |c: u8| (c as u32) << 24;
        Some(match *self {
            Self::Vertex { count, v0, addr } => (
                cmd(op::G_VTX)
                    | ((((count as u32 - 1) << 4) | v0 as u32) << 16)
                    | (count as u32 * VTX_SIZE),
                addr,
            ),
            Self::Tri1 { v, flag } => (
                cmd(op::G_TRI1),
                ((flag as u32) << 24)
                    | ((v[0] as u32 * 10) << 16)
                    | ((v[1] as u32 * 10) << 8)
                    | (v[2] as u32 * 10),
            ),
            Self::EndDl => (cmd(op::G_ENDDL), 0),
            _ => return None,
        })
    }

    /// Opcode byte of the command.
    pub fn opcode(&self) -> u8 {
        match *self {
            Self::Vertex { .. } => op::G_VTX,
            Self::Tri1 { .. } => op::G_TRI1,
            Self::Matrix { .. } => op::G_MTX,
            Self::PopMatrix => op::G_POPMTX,
            Self::DisplayList { .. } => op::G_DL,
            Self::SetGeometryMode(_) => op::G_SETGEOMETRYMODE,
            Self::ClearGeometryMode(_) => op::G_CLEARGEOMETRYMODE,
            Self::Texture { .. } => op::G_TEXTURE,
            Self::SetTImg { .. } => op::G_SETTIMG,
            Self::SetTile(_) => op::G_SETTILE,
            Self::LoadBlock { .. } => op::G_LOADBLOCK,
            Self::LoadTile { .. } => op::G_LOADTILE,
            Self::LoadTlut { .. } => op::G_LOADTLUT,
            Self::SetTileSize { .. } => op::G_SETTILESIZE,
            Self::SetOtherModeH { .. } => op::G_SETOTHERMODE_H,
            Self::SetOtherModeL { .. } => op::G_SETOTHERMODE_L,
            Self::SetCombine { .. } => op::G_SETCOMBINE,
            Self::SetPrimColor { .. } => op::G_SETPRIMCOLOR,
            Self::SetEnvColor { .. } => op::G_SETENVCOLOR,
            Self::LoadSync => op::G_RDPLOADSYNC,
            Self::PipeSync => op::G_RDPPIPESYNC,
            Self::TileSync => op::G_RDPTILESYNC,
            Self::EndDl => op::G_ENDDL,
            Self::Other { w0, .. } => (w0 >> 24) as u8,
        }
    }

    /// Decodes a big-endian display list (8 bytes per command).
    pub fn decode_list(bytes: &[u8]) -> Vec<Self> {
        bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| {
                let w0 = u32::from_be_bytes(c[..4].try_into().unwrap());
                let w1 = u32::from_be_bytes(c[4..].try_into().unwrap());
                Self::decode(w0, w1)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_texture_setup() {
        // gsSPTexture(0xFFFF, 0xFFFF, 5, 1, G_ON)
        assert_eq!(
            Gfx::decode(0xBB00_2901, 0xFFFF_FFFF),
            Gfx::Texture {
                level: 5,
                tile: 1,
                on: true,
                scale_s: 0xFFFF,
                scale_t: 0xFFFF
            }
        );
        // gsDPSetTile(RGBA, 16b, line 16, tmem 0, tile 1, pal 0, cmt wrap, maskt 4, cms clamp)
        let Gfx::SetTile(t) = Gfx::decode(0xF510_2000, 0x0101_0200) else {
            panic!()
        };
        assert_eq!(t.format, TexFormat(ImFmt::Rgba, ImSiz::B16));
        assert_eq!((t.line, t.tmem, t.tile, t.maskt), (16, 0, 1, 4));
        assert!(t.cms.clamp && !t.cmt.clamp);
        assert_eq!(
            Gfx::decode(0xF300_0000, 0x0756_0000),
            Gfx::LoadBlock {
                tile: 7,
                uls: 0,
                ult: 0,
                lrs: 0x560,
                dxt: 0
            }
        );
        assert_eq!(
            Gfx::decode(0xF000_0000, 0x0703_C000),
            Gfx::LoadTlut { tile: 7, count: 16 }
        );
        assert_eq!(TexFormat(ImFmt::Ia, ImSiz::B8).to_string(), "IA8");
    }

    #[test]
    fn geometry_roundtrip() {
        // gsSPVertex(0x100, 16, 0) and gsSP1Triangle(1, 2, 15, 0), F3D encoding.
        let v = Gfx::decode(0x04F0_0100, 0x100);
        let expect = Gfx::Vertex {
            count: 16,
            v0: 0,
            addr: 0x100,
        };
        assert_eq!(v, expect);
        assert_eq!(v.encode(), Some((0x04F0_0100, 0x100)));
        let t = Gfx::decode(0xBF00_0000, 0x000A_1496);
        let expect = Gfx::Tri1 {
            v: [1, 2, 15],
            flag: 0,
        };
        assert_eq!(t, expect);
        assert_eq!(t.encode(), Some((0xBF00_0000, 0x000A_1496)));
        assert_eq!(op::name(t.opcode()), Some("G_TRI1"));
        assert_eq!(
            Gfx::decode(0xB700_0000, geom::G_ZBUFFER),
            Gfx::SetGeometryMode(geom::G_ZBUFFER)
        );
    }
}
