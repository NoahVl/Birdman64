//! A model of the RDP's texture memory (TMEM) and texel decoders.
//!
//! Textures are decoded by *executing* their load commands into a 4 KiB TMEM
//! and then sampling each tile the way the RDP does. This handles pre-swizzled
//! data (`LoadBlock` with `dxt = 0`), mip chains packed at different TMEM
//! addresses, palettes and the RGBA32 bank split without special cases.
//!
//! TMEM facts relied upon (see the N64 programming manual, section 13):
//! - 4096 bytes, addressed in 64-bit words; tiles give `tmem`/`line` in words.
//! - On odd rows the two 32-bit halves of every 64-bit word are swapped
//!   (byte address `^ 4`). Loads write with that swap; sampling undoes it.
//! - RGBA32 stores R,G in the low 2 KiB and B,A at the same offset + 0x800.
//! - `LoadTLUT` writes each 16-bit entry four times (8 bytes per entry);
//!   CI lookups read entry `i` at byte `0x800 + i * 8`.

use crate::gbi::{G_MDSFT_TEXTLUT, Gfx, ImFmt, ImSiz, TexFormat, TileDesc};
use anyhow::{Context, Result, bail, ensure};

pub const TMEM_SIZE: usize = 4096;

/// A decoded RGBA8 image, rows top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Palette type from othermode-H `G_MDSFT_TEXTLUT` (`G_TT_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlutMode {
    #[default]
    None,
    Rgba16,
    Ia16,
}

/// A tile descriptor plus its `SetTileSize` rectangle (10.2 fixed point).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tile {
    pub desc: TileDesc,
    pub uls: u16,
    pub ult: u16,
    pub lrs: u16,
    pub lrt: u16,
}

impl Tile {
    pub fn width(&self) -> u32 {
        ((self.lrs.saturating_sub(self.uls)) >> 2) as u32 + 1
    }
    pub fn height(&self) -> u32 {
        ((self.lrt.saturating_sub(self.ult)) >> 2) as u32 + 1
    }
}

#[derive(Debug, Clone, Copy)]
struct TImg {
    /// Index of this `SetTImg` in the list (0 = first).
    index: usize,
    format: TexFormat,
    width: u16,
    addr: u32,
}

/// RDP texture state: TMEM contents, the 8 tile descriptors and the modes
/// that affect sampling.
pub struct Tmem {
    pub mem: Box<[u8; TMEM_SIZE]>,
    descs: [Option<TileDesc>; 8],
    sizes: [Option<(u16, u16, u16, u16)>; 8],
    pub othermode_h: u32,
    timg: Option<TImg>,
    timg_count: usize,
    /// (first byte, end byte, SetTImg index) of every texel load, in order.
    loads: Vec<(usize, usize, usize)>,
}

impl Default for Tmem {
    fn default() -> Self {
        Self::new()
    }
}

impl Tmem {
    pub fn new() -> Self {
        Self {
            mem: Box::new([0; TMEM_SIZE]),
            descs: [None; 8],
            sizes: [None; 8],
            othermode_h: 0,
            timg: None,
            timg_count: 0,
            loads: Vec::new(),
        }
    }

    pub fn tlut_mode(&self) -> TlutMode {
        match (self.othermode_h >> G_MDSFT_TEXTLUT) & 3 {
            2 => TlutMode::Rgba16,
            3 => TlutMode::Ia16,
            _ => TlutMode::None,
        }
    }

    /// Tile `i`, if both `SetTile` and `SetTileSize` were issued for it.
    pub fn tile(&self, i: u8) -> Option<Tile> {
        let desc = self.descs[i as usize & 7]?;
        let (uls, ult, lrs, lrt) = self.sizes[i as usize & 7]?;
        Some(Tile {
            desc,
            uls,
            ult,
            lrs,
            lrt,
        })
    }

    /// Descriptor of tile `i` from its last `SetTile`.
    pub fn desc(&self, i: u8) -> Option<TileDesc> {
        self.descs[i as usize & 7]
    }

    /// Which `SetTImg` (0 = first) last loaded the TMEM word tile `i` starts at.
    pub fn source_of(&self, i: u8) -> Option<usize> {
        let a = self.descs[i as usize & 7]?.tmem as usize * 8;
        self.loads
            .iter()
            .rev()
            .find(|l| (l.0..l.1).contains(&a))
            .map(|l| l.2)
    }

    /// Sets a tile rectangle (10.2 fixed point), like `G_SETTILESIZE`.
    pub fn set_tile_size(&mut self, tile: u8, uls: u16, ult: u16, lrs: u16, lrt: u16) {
        self.sizes[tile as usize & 7] = Some((uls, ult, lrs, lrt));
    }

    /// Executes texture commands. `images(n, addr)` resolves the n-th
    /// `SetTImg` of this list to the DRAM bytes starting at `addr`.
    pub fn run<'a>(
        &mut self,
        cmds: &[Gfx],
        images: &dyn Fn(usize, u32) -> Option<&'a [u8]>,
    ) -> Result<()> {
        for cmd in cmds {
            match *cmd {
                Gfx::SetTImg {
                    format,
                    width,
                    addr,
                } => {
                    self.timg = Some(TImg {
                        index: self.timg_count,
                        format,
                        width,
                        addr,
                    });
                    self.timg_count += 1;
                }
                Gfx::SetTile(d) => self.descs[d.tile as usize] = Some(d),
                Gfx::SetTileSize {
                    tile,
                    uls,
                    ult,
                    lrs,
                    lrt,
                } => {
                    self.sizes[tile as usize] = Some((uls, ult, lrs, lrt));
                }
                Gfx::SetOtherModeH { shift, len, data } => {
                    let mask = ((1u64 << len) - 1) as u32;
                    self.othermode_h =
                        (self.othermode_h & !(mask << shift)) | (data & (mask << shift));
                }
                Gfx::LoadBlock {
                    tile,
                    uls,
                    ult,
                    lrs,
                    dxt,
                } => {
                    let (timg, src) = self.source(images)?;
                    self.load_block(timg, src, tile, uls, ult, lrs, dxt)?;
                    let base = self.descs[tile as usize].map_or(0, |d| d.tmem as usize * 8);
                    let len = ((lrs.saturating_sub(uls)) as usize + 1)
                        * timg.format.1.bits() as usize
                        / 8;
                    self.loads.push((base, base + len.max(1), timg.index));
                }
                Gfx::LoadTile {
                    tile,
                    uls,
                    ult,
                    lrs,
                    lrt,
                } => {
                    let (timg, src) = self.source(images)?;
                    self.load_tile(timg, src, tile, uls, ult, lrs, lrt)?;
                    let d = self.descs[tile as usize].context("LoadTile with unset tile")?;
                    let base = d.tmem as usize * 8;
                    let rows = ((lrt.saturating_sub(ult)) >> 2) as usize + 1;
                    self.loads
                        .push((base, base + rows * d.line.max(1) as usize * 8, timg.index));
                }
                Gfx::LoadTlut { tile, count } => {
                    let (_, src) = self.source(images)?;
                    let d = self.descs[tile as usize].context("LoadTLUT with unset tile")?;
                    let base = d.tmem as usize * 8;
                    for i in 0..count as usize {
                        let e = [byte(src, i * 2), byte(src, i * 2 + 1)];
                        for k in 0..4 {
                            let a = (base + i * 8 + k * 2) & (TMEM_SIZE - 1);
                            self.mem[a..a + 2].copy_from_slice(&e);
                        }
                    }
                }
                Gfx::EndDl => break,
                _ => {}
            }
        }
        Ok(())
    }

    fn source<'a>(
        &self,
        images: &dyn Fn(usize, u32) -> Option<&'a [u8]>,
    ) -> Result<(TImg, &'a [u8])> {
        let t = self.timg.context("texture load before SetTImg")?;
        let src = images(t.index, t.addr)
            .with_context(|| format!("SetTImg #{} (addr {:#x}) not resolvable", t.index, t.addr))?;
        Ok((t, src))
    }

    #[allow(clippy::too_many_arguments)]
    fn load_block(
        &mut self,
        timg: TImg,
        src: &[u8],
        tile: u8,
        uls: u16,
        ult: u16,
        lrs: u16,
        dxt: u16,
    ) -> Result<()> {
        let d = self.descs[tile as usize].context("LoadBlock with unset tile")?;
        let bits = timg.format.1.bits() as usize;
        ensure!(lrs >= uls, "LoadBlock lrs < uls");
        let texels = (lrs - uls) as usize + 1;
        // LoadBlock's uls/ult are integer texel coordinates.
        let start = (ult as usize * timg.width as usize + uls as usize) * bits / 8;
        let base = d.tmem as usize * 8;
        if timg.format.1 == ImSiz::B32 {
            // Each 64-bit DRAM word holds 2 texels; RG to the low bank, BA high.
            for k in 0..texels {
                let word = k / 2;
                let odd = ((word * dxt as usize) >> 11) & 1 == 1;
                let mut a = base + k * 2;
                if odd {
                    a ^= 4;
                }
                let s = start + k * 4;
                self.put_split(
                    a,
                    [
                        byte(src, s),
                        byte(src, s + 1),
                        byte(src, s + 2),
                        byte(src, s + 3),
                    ],
                );
            }
        } else {
            // Whole 64-bit words: `base` is word aligned, so a word never
            // straddles the TMEM wrap and the odd-line swap (byte `^ 4`)
            // just exchanges its two halves.
            let words = (texels * bits).div_ceil(64);
            // Words fully inside `src`; past them, `byte` zero-fills.
            let full = src.len().saturating_sub(start) / 8;
            let tmem = self.mem.as_chunks_mut::<8>().0;
            for w in 0..words {
                let odd = ((w * dxt as usize) >> 11) & 1 == 1;
                let s = start + w * 8;
                let mut word: [u8; 8] = if w < full {
                    src[s..s + 8].try_into().unwrap()
                } else {
                    std::array::from_fn(|b| byte(src, s + b))
                };
                if odd {
                    word.rotate_left(4);
                }
                tmem[(base / 8 + w) % (TMEM_SIZE / 8)] = word;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn load_tile(
        &mut self,
        timg: TImg,
        src: &[u8],
        tile: u8,
        uls: u16,
        ult: u16,
        lrs: u16,
        lrt: u16,
    ) -> Result<()> {
        let d = self.descs[tile as usize].context("LoadTile with unset tile")?;
        self.sizes[tile as usize] = Some((uls, ult, lrs, lrt));
        let bits = timg.format.1.bits() as usize;
        let (s0, t0) = ((uls >> 2) as usize, (ult >> 2) as usize);
        let (s1, t1) = ((lrs >> 2) as usize, (lrt >> 2) as usize);
        let base = d.tmem as usize * 8;
        let line = d.line as usize * 8;
        for (row, t) in (t0..=t1).enumerate() {
            let swap = if row & 1 == 1 { 4 } else { 0 };
            let row_src = t * timg.width as usize;
            if timg.format.1 == ImSiz::B32 {
                for (col, s) in (s0..=s1).enumerate() {
                    let o = (row_src + s) * 4;
                    let a = (base + row * line + col * 2) ^ swap;
                    self.put_split(
                        a,
                        [
                            byte(src, o),
                            byte(src, o + 1),
                            byte(src, o + 2),
                            byte(src, o + 3),
                        ],
                    );
                }
            } else {
                let row_bytes = ((s1 - s0 + 1) * bits).div_ceil(8);
                let o = (row_src + s0) * bits / 8;
                // By 64-bit word (the row start is word aligned): byte `i`
                // of a word lands at `i ^ swap` inside it.
                let row_word = (base + row * line) / 8;
                let full = src.len().saturating_sub(o) / 8;
                let tmem = self.mem.as_chunks_mut::<8>().0;
                for k in 0..row_bytes.div_ceil(8) {
                    let s = o + k * 8;
                    let dst = &mut tmem[(row_word + k) % (TMEM_SIZE / 8)];
                    let n = (row_bytes - k * 8).min(8);
                    if n == 8 {
                        let mut word: [u8; 8] = if k < full {
                            src[s..s + 8].try_into().unwrap()
                        } else {
                            std::array::from_fn(|b| byte(src, s + b))
                        };
                        word.rotate_left(swap);
                        *dst = word;
                    } else {
                        for i in 0..n {
                            dst[i ^ swap] = byte(src, s + i);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn put_split(&mut self, a: usize, rgba: [u8; 4]) {
        let lo = a & 0x7FF;
        self.mem[lo] = rgba[0];
        self.mem[lo + 1] = rgba[1];
        self.mem[lo | 0x800] = rgba[2];
        self.mem[(lo | 0x800) + 1] = rgba[3];
    }

    /// Samples every texel of `tile` and converts it to RGBA8.
    pub fn decode_tile(&self, tile: u8) -> Result<Image> {
        let t = self
            .tile(tile)
            .with_context(|| format!("tile {tile} not fully set up"))?;
        let (w, h) = (t.width(), t.height());
        let d = t.desc;
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        let rd16 = |a: usize| -> u16 {
            let a = a & (TMEM_SIZE - 1);
            u16::from_be_bytes([self.mem[a], self.mem[(a + 1) & (TMEM_SIZE - 1)]])
        };
        for y in 0..h as usize {
            let row = d.tmem as usize * 8 + y * d.line as usize * 8;
            let swap = if y & 1 == 1 { 4 } else { 0 };
            for x in 0..w as usize {
                let px = match d.format {
                    TexFormat(ImFmt::Rgba, ImSiz::B16) => rgba16(rd16((row + x * 2) ^ swap)),
                    TexFormat(ImFmt::Rgba, ImSiz::B32) => {
                        let a = ((row + x * 2) ^ swap) & 0x7FF;
                        let [r, g] = rd16(a).to_be_bytes();
                        let [b, al] = rd16(a | 0x800).to_be_bytes();
                        [r, g, b, al]
                    }
                    TexFormat(ImFmt::Ia, ImSiz::B16) => ia16(rd16((row + x * 2) ^ swap)),
                    TexFormat(ImFmt::Ia | ImFmt::I | ImFmt::Ci, ImSiz::B8) => {
                        let v = self.mem[((row + x) ^ swap) & (TMEM_SIZE - 1)];
                        match d.format.0 {
                            ImFmt::Ia => ia8(v),
                            ImFmt::I => i8(v),
                            _ => self.tlut(v as usize),
                        }
                    }
                    TexFormat(ImFmt::Ia | ImFmt::I | ImFmt::Ci, ImSiz::B4) => {
                        let v = self.mem[((row + x / 2) ^ swap) & (TMEM_SIZE - 1)];
                        let n = if x & 1 == 0 { v >> 4 } else { v & 0xF };
                        match d.format.0 {
                            ImFmt::Ia => ia4(n),
                            ImFmt::I => i4(n),
                            _ => self.tlut(((d.palette as usize) << 4) | n as usize),
                        }
                    }
                    f => bail!("unsupported texel format {f} on tile {tile}"),
                };
                rgba.extend_from_slice(&px);
            }
        }
        Ok(Image {
            width: w,
            height: h,
            rgba,
        })
    }

    fn tlut(&self, index: usize) -> [u8; 4] {
        let a = 0x800 + (index & 0xFF) * 8;
        let v = u16::from_be_bytes([self.mem[a], self.mem[a + 1]]);
        match self.tlut_mode() {
            TlutMode::Ia16 => ia16(v),
            TlutMode::Rgba16 => rgba16(v),
            // Without a TLUT the RDP passes the raw index through.
            TlutMode::None => i8(index as u8),
        }
    }
}

fn byte(src: &[u8], i: usize) -> u8 {
    // Loads may run a few bytes past the stored image (LoadBlock rounds up);
    // real hardware reads whatever follows in RDRAM. Zero is deterministic.
    src.get(i).copied().unwrap_or(0)
}

fn expand5(v: u16) -> u8 {
    let v = (v & 0x1F) as u8;
    (v << 3) | (v >> 2)
}

/// RGBA 5551.
pub fn rgba16(v: u16) -> [u8; 4] {
    [
        expand5(v >> 11),
        expand5(v >> 6),
        expand5(v >> 1),
        if v & 1 != 0 { 255 } else { 0 },
    ]
}

/// IA 8.8: intensity high byte, alpha low byte.
pub fn ia16(v: u16) -> [u8; 4] {
    let [i, a] = v.to_be_bytes();
    [i, i, i, a]
}

/// IA 4.4.
pub fn ia8(v: u8) -> [u8; 4] {
    let i = (v >> 4) * 17;
    let a = (v & 0xF) * 17;
    [i, i, i, a]
}

/// IA 3.1.
pub fn ia4(n: u8) -> [u8; 4] {
    let i3 = (n >> 1) & 7;
    let i = (i3 << 5) | (i3 << 2) | (i3 >> 1);
    [i, i, i, if n & 1 != 0 { 255 } else { 0 }]
}

/// I8: the RDP replicates intensity into alpha.
pub fn i8(v: u8) -> [u8; 4] {
    [v, v, v, v]
}

/// I4: intensity replicated into alpha.
pub fn i4(n: u8) -> [u8; 4] {
    let v = (n & 0xF) * 17;
    [v, v, v, v]
}

/// Texel `index` of a *linear* (DRAM-order, not TMEM-swizzled) image, as
/// sprites/blits read it. `None` for out-of-range data or unsupported
/// formats (CI needs a TLUT, YUV is unused).
pub fn linear_texel(
    fmt: crate::gbi::ImFmt,
    siz: crate::gbi::ImSiz,
    data: &[u8],
    index: usize,
) -> Option<[u8; 4]> {
    use crate::gbi::{ImFmt as F, ImSiz as S};
    let nib = || {
        let b = *data.get(index / 2)?;
        Some(if index.is_multiple_of(2) {
            b >> 4
        } else {
            b & 0xF
        })
    };
    let byte = || data.get(index).copied();
    let half = || {
        let b = data.get(index * 2..index * 2 + 2)?;
        Some(u16::from_be_bytes([b[0], b[1]]))
    };
    Some(match (fmt, siz) {
        (F::Rgba, S::B16) => rgba16(half()?),
        (F::Rgba, S::B32) => data.get(index * 4..index * 4 + 4)?.try_into().ok()?,
        (F::Ia, S::B4) => ia4(nib()?),
        (F::Ia, S::B8) => ia8(byte()?),
        (F::Ia, S::B16) => ia16(half()?),
        (F::I, S::B4) => i4(nib()?),
        (F::I, S::B8) => i8(byte()?),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gbi::{ImFmt, ImSiz, TexFormat, WrapMode};

    #[test]
    fn texel_decoders() {
        assert_eq!(rgba16(0xFFFF), [255, 255, 255, 255]);
        assert_eq!(rgba16(0xF800), [255, 0, 0, 0]);
        assert_eq!(rgba16(0x07C1), [0, 255, 0, 255]);
        assert_eq!(rgba16(0x003F), [0, 0, 255, 255]);
        assert_eq!(rgba16(0x4210), [66, 66, 66, 0]); // 01000 01000 01000 0
        assert_eq!(ia16(0x80FF), [0x80, 0x80, 0x80, 0xFF]);
        assert_eq!(ia8(0xF3), [255, 255, 255, 51]);
        assert_eq!(ia4(0xF), [255, 255, 255, 255]);
        assert_eq!(ia4(0x8), [146, 146, 146, 0]);
        assert_eq!(ia4(0x1), [0, 0, 0, 255]);
        assert_eq!(i4(0x8), [136; 4]);
        assert_eq!(i8(0x42), [0x42; 4]);
    }

    fn desc(format: TexFormat, line: u16, tmem: u16, tile: u8) -> TileDesc {
        TileDesc {
            format,
            line,
            tmem,
            tile,
            palette: 0,
            cmt: WrapMode::default(),
            maskt: 0,
            shiftt: 0,
            cms: WrapMode::default(),
            masks: 0,
            shifts: 0,
        }
    }

    /// Loads `img` (w×h, 16 bit) with LoadBlock + dxt and samples it back.
    #[test]
    fn load_block_with_dxt_round_trips() {
        // 4x4 RGBA16: one 64-bit word per row, dxt = 2048 (1 line per word).
        let texels: Vec<u16> = (0..16).map(|i| (i as u16) << 1 | 1).collect();
        let bytes: Vec<u8> = texels.iter().flat_map(|t| t.to_be_bytes()).collect();
        let f = TexFormat(ImFmt::Rgba, ImSiz::B16);
        let cmds = [
            Gfx::SetTImg {
                format: f,
                width: 1,
                addr: 0,
            },
            Gfx::SetTile(desc(f, 0, 0, 7)),
            Gfx::LoadBlock {
                tile: 7,
                uls: 0,
                ult: 0,
                lrs: 15,
                dxt: 2048,
            },
            Gfx::SetTile(desc(f, 1, 0, 0)),
            Gfx::SetTileSize {
                tile: 0,
                uls: 0,
                ult: 0,
                lrs: 3 << 2,
                lrt: 3 << 2,
            },
        ];
        let mut t = Tmem::new();
        t.run(&cmds, &|_, a| bytes.get(a as usize..)).unwrap();
        // Row 1 must be stored swapped in TMEM...
        assert_eq!(&t.mem[8..12], &bytes[12..16]);
        // ...and sample back in order.
        let img = t.decode_tile(0).unwrap();
        let expect: Vec<u8> = texels.iter().flat_map(|&v| rgba16(v)).collect();
        assert_eq!(img.rgba, expect);
    }

    /// The word-wise LoadBlock/LoadTile paths write exactly what the
    /// original per-byte loops wrote (kept here as the reference), incl.
    /// TMEM wrap-around, odd-line swaps, partial words and short sources.
    #[test]
    fn word_loads_match_bytewise_reference() {
        let mut seed = 0x1234_5678_9ABC_DEF1u64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let src: Vec<u8> = (0..6000).map(|i| (i * 31 % 251) as u8).collect();
        for _ in 0..400 {
            let siz = [ImSiz::B4, ImSiz::B8, ImSiz::B16][rnd(3) as usize];
            let f = TexFormat(ImFmt::I, siz);
            let bits = siz.bits() as usize;
            let tmem = rnd(512) as u16;
            let line = rnd(12) as u16;
            let width = 1 + rnd(200) as u16;
            let src_len = rnd(6000) as usize;
            let src = &src[..src_len];
            let timg = TImg {
                index: 0,
                format: f,
                width,
                addr: 0,
            };
            let mut t = Tmem::new();
            t.descs[7] = Some(desc(f, line, tmem, 7));
            let mut want = t.mem.clone();
            if rnd(2) == 0 {
                let (uls, ult) = (rnd(8) as u16, rnd(4) as u16);
                let lrs = uls + rnd(2048) as u16;
                let dxt = rnd(2048) as u16;
                t.load_block(timg, src, 7, uls, ult, lrs, dxt).unwrap();
                let start = (ult as usize * width as usize + uls as usize) * bits / 8;
                let words = (((lrs - uls) as usize + 1) * bits).div_ceil(64);
                for w in 0..words {
                    let odd = ((w * dxt as usize) >> 11) & 1 == 1;
                    for b in 0..8 {
                        let mut a = tmem as usize * 8 + w * 8 + b;
                        if odd {
                            a ^= 4;
                        }
                        want[a & (TMEM_SIZE - 1)] = byte(src, start + w * 8 + b);
                    }
                }
            } else {
                let (uls, ult) = ((rnd(16) as u16) << 2, (rnd(8) as u16) << 2);
                let (lrs, lrt) = (uls + ((rnd(64) as u16) << 2), ult + ((rnd(40) as u16) << 2));
                t.load_tile(timg, src, 7, uls, ult, lrs, lrt).unwrap();
                let (s0, t0) = ((uls >> 2) as usize, (ult >> 2) as usize);
                let (s1, t1) = ((lrs >> 2) as usize, (lrt >> 2) as usize);
                for (row, tt) in (t0..=t1).enumerate() {
                    let swap = if row & 1 == 1 { 4 } else { 0 };
                    let row_bytes = ((s1 - s0 + 1) * bits).div_ceil(8);
                    let o = (tt * width as usize + s0) * bits / 8;
                    for b in 0..row_bytes {
                        let a = (tmem as usize * 8 + row * line as usize * 8 + b) ^ swap;
                        want[a & (TMEM_SIZE - 1)] = byte(src, o + b);
                    }
                }
            }
            assert_eq!(t.mem[..], want[..]);
        }
    }

    #[test]
    fn ci4_with_tlut_and_rgba32_tile() {
        let f16 = TexFormat(ImFmt::Rgba, ImSiz::B16);
        let fci = TexFormat(ImFmt::Ci, ImSiz::B4);
        let tlut: Vec<u8> = [0xF801u16, 0x07C1]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        let pix = [0x01u8, 0x10, 0, 0, 0, 0, 0, 0, 0x10, 0x01, 0, 0, 0, 0, 0, 0];
        let cmds = [
            Gfx::SetOtherModeH {
                shift: 14,
                len: 2,
                data: 2 << 14,
            },
            Gfx::SetTImg {
                format: f16,
                width: 1,
                addr: 0,
            },
            Gfx::SetTile(desc(f16, 0, 256, 7)),
            Gfx::LoadTlut { tile: 7, count: 2 },
            Gfx::SetTImg {
                format: fci,
                width: 16,
                addr: 0,
            },
            Gfx::SetTile(desc(fci, 1, 0, 7)),
            Gfx::LoadTile {
                tile: 7,
                uls: 0,
                ult: 0,
                lrs: 3 << 2,
                lrt: 1 << 2,
            },
            Gfx::SetTile(desc(fci, 1, 0, 0)),
            Gfx::SetTileSize {
                tile: 0,
                uls: 0,
                ult: 0,
                lrs: 3 << 2,
                lrt: 1 << 2,
            },
        ];
        let (r, g) = ([255, 0, 0, 255], [0, 255, 0, 255]);
        let mut t = Tmem::new();
        t.run(&cmds, &|n, _| {
            Some(if n == 0 { &tlut[..] } else { &pix[..] })
        })
        .unwrap();
        let img = t.decode_tile(0).unwrap();
        let expect: Vec<u8> = [r, g, g, r, g, r, r, g].concat();
        assert_eq!(img.rgba, expect);

        // RGBA32 via LoadTile: 2x2.
        let f32_ = TexFormat(ImFmt::Rgba, ImSiz::B32);
        let px: Vec<u8> = (0..16).collect();
        let cmds = [
            Gfx::SetTImg {
                format: f32_,
                width: 2,
                addr: 0,
            },
            Gfx::SetTile(desc(f32_, 1, 0, 7)),
            Gfx::LoadTile {
                tile: 7,
                uls: 0,
                ult: 0,
                lrs: 1 << 2,
                lrt: 1 << 2,
            },
            Gfx::SetTile(desc(f32_, 1, 0, 0)),
            Gfx::SetTileSize {
                tile: 0,
                uls: 0,
                ult: 0,
                lrs: 1 << 2,
                lrt: 1 << 2,
            },
        ];
        let mut t = Tmem::new();
        t.run(&cmds, &|_, _| Some(&px[..])).unwrap();
        assert_eq!(t.decode_tile(0).unwrap().rgba, px);
    }
}
