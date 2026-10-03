//! `UVTX` textures. Mirrors `_uvExpandTexture` / `_uvExpandTextureImg` in
//! `decomp/src/kernel/texture.c`; the parsed struct is `ParsedUVTX` in
//! `decomp/include/kernel/uv_graphics.h`.
//!
//! A UVTX file has one (MIO0-compressed) `COMM` block:
//!
//! | off | type | meaning |
//! |---|---|---|
//! | 0 | u16 | `size`: image bytes (≤ 0x1000, i.e. fits TMEM) |
//! | 2 | u16 | `gfx_count`: display-list commands |
//! | 4 | f32×2 | scroll speed S,T of image 0 (textures per second; 0,0 = none) |
//! | 0xC | f32×2 | scroll speed S,T of image 1 |
//! | 0x14 | u8\[size\] | image data, already in TMEM order (see [`crate::tmem`]) |
//! | … | Gfx\[gfx_count\] | texture-load display list, ends with `G_ENDDL` |
//! | … | 22 bytes | trailer, see [`Uvtx`] fields; then 6 bytes of zero padding |
//!
//! The display list's `SetTImg` addresses are offsets: the engine ORs in the
//! image's address (1st `SetTImg`) or that of texture [`Uvtx::image2`] (later ones).

use crate::gbi::{G_TX_LOADTILE, Gfx};
use crate::tmem::{Image, Tile, Tmem};
use anyhow::{Context, Result, bail, ensure};
use pw64_rom::Form;

/// Engine cap on `size` (`_uvExpandTexture`: "txt image too big").
pub const MAX_IMAGE_SIZE: usize = 0x1000;
/// Value of [`Uvtx::image2`] meaning "no second image".
pub const NO_TEXTURE: u16 = 0xFFF;

#[derive(Debug, Clone)]
pub struct Uvtx {
    /// Texel data as it is copied to TMEM (`_uvExpandTextureImg`).
    pub image: Vec<u8>,
    /// The load/tile-setup display list (`ParsedUVTX.dlist`).
    pub dlist: Vec<Gfx>,
    /// Per-image UV scroll speeds `[s, t]` (`ParsedUVTX.unk18` / `unk1C`).
    pub scroll: [[f32; 2]; 2],
    pub width: u16,
    pub height: u16,
    /// `unkE`: bits per texel of the main image (4, 8 or 16).
    pub bits_per_texel: u8,
    /// `unkF` / `unk10`: S/T address mode: 0 = clamp, 1 = wrap, 2 = mirror
    /// (matches the own-image tile's cms/cmt on all 463 ROM textures).
    pub wrap_s: u8,
    pub wrap_t: u8,
    /// `unk12`: low 12 bits = texture id (always its own UVTX index in the
    /// ROM); high 4 bits = render flags (`0x8000` selects XLU surface render
    /// modes in `uvGfxStateDraw`).
    pub state: u16,
    /// `unk14`: texture whose image the 2nd+ `SetTImg` refers to, or [`NO_TEXTURE`].
    pub image2: u16,
    /// `unk20`: unknown (0, 4 or 32).
    pub unk20: u16,
    /// `unk22`: channel count (1 = I, 2 = IA, 4 = RGBA); decides render mode.
    pub channels: u8,
    /// `unk23..unk26`: looks like the average texel color (first `channels`
    /// bytes used; `code_8170.c` reads bytes 0..=2 as RGB when `channels == 4`).
    pub avg_color: [u8; 4],
    /// `unk28`: blend weight toward the fog/light color in `code_8170.c`.
    pub unk28: f32,
}

/// All tiles a UVTX's display list sets up, decoded.
#[derive(Debug, Clone)]
pub struct DecodedUvtx {
    /// Tile passed to `gSPTexture` (the one sampled at level 0).
    pub render_tile: u8,
    /// Number of mip levels (`gSPTexture` level + 1), tiles `render_tile..`.
    pub levels: u8,
    pub tiles: Vec<DecodedTile>,
}

#[derive(Debug, Clone)]
pub struct DecodedTile {
    pub index: u8,
    /// Which `SetTImg` the tile's texels came from (0 = own image, 1 = image2).
    pub source: Option<usize>,
    pub tile: Tile,
    pub image: Image,
}

impl DecodedUvtx {
    /// Level-0 image of the texture's own image.
    ///
    /// The texture's own image: the lowest tile sampling data loaded from the
    /// first `SetTImg`. In two-image lists that is not always the render tile
    /// (e.g. `gSPTexture` tile 0 = the other texture, tile 1 = this one).
    pub fn base(&self) -> &DecodedTile {
        self.tiles
            .iter()
            .find(|t| t.source == Some(0))
            .or_else(|| self.tiles.iter().find(|t| t.index == self.render_tile))
            .expect("render tile decoded")
    }
}

impl DecodedUvtx {
    /// The tile sampled at level 0 (`gSPTexture`'s tile, i.e. TEXEL0).
    pub fn render(&self) -> &DecodedTile {
        self.tiles
            .iter()
            .find(|t| t.index == self.render_tile)
            .expect("render tile decoded")
    }
}

/// Maps a vertex's S/T (s10.5 texels) to UV normalized to `tile`'s size,
/// the way the RSP/RDP do: `gSPTexture` scale, then the tile's shift, then
/// minus the tile origin (`uls`/`ult`, 10.2).
pub fn tile_uv(tile: &Tile, scale: [u16; 2], st: [i16; 2]) -> [f32; 2] {
    let fixed = |s: u16| if s == 0xFFFF { 1.0 } else { s as f32 / 65536.0 };
    let shift = |n: u8| match n {
        0 => 1.0,
        1..=10 => 1.0 / (1u32 << n) as f32,
        _ => (1u32 << (16 - n)) as f32,
    };
    let d = &tile.desc;
    let s = st[0] as f32 / 32.0 * fixed(scale[0]) * shift(d.shifts) - tile.uls as f32 / 4.0;
    let t = st[1] as f32 / 32.0 * fixed(scale[1]) * shift(d.shiftt) - tile.ult as f32 / 4.0;
    [s / tile.width() as f32, t / tile.height() as f32]
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self
            .b
            .get(self.pos..self.pos + n)
            .with_context(|| format!("UVTX COMM truncated at {:#x}+{n}", self.pos))?;
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
}

impl Uvtx {
    /// Parses a `UVTX` FORM (as returned by `Filesystem::read`).
    pub fn parse(form: &Form) -> Result<Self> {
        ensure!(form.tag.0 == *b"UVTX", "not a UVTX file ({})", form.tag);
        let comm = form.block(b"COMM").context("UVTX without COMM block")?;
        Self::parse_comm(&comm.data)
    }

    /// Parses the (decompressed) `COMM` block. Mirrors `_uvExpandTexture`.
    pub fn parse_comm(b: &[u8]) -> Result<Self> {
        let mut r = Reader { b, pos: 0 };
        let size = r.u16()? as usize;
        // The engine clamps to 0x1000 but still advances by the clamped size,
        // which would desync the parse; no ROM file exceeds it.
        ensure!(size <= MAX_IMAGE_SIZE, "UVTX image too big ({size} bytes)");
        let gfx_count = r.u16()? as usize;
        let scroll = [[r.f32()?, r.f32()?], [r.f32()?, r.f32()?]];
        let image = r.take(size)?.to_vec();
        let dlist = Gfx::decode_list(r.take(gfx_count * 8)?);
        Ok(Self {
            image,
            dlist,
            scroll,
            width: r.u16()?,
            height: r.u16()?,
            bits_per_texel: r.u8()?,
            wrap_s: r.u8()?,
            wrap_t: r.u8()?,
            state: r.u16()?,
            image2: r.u16()?,
            unk20: r.u16()?,
            channels: r.u8()?,
            avg_color: [r.u8()?, r.u8()?, r.u8()?, r.u8()?],
            unk28: r.f32()?,
        })
    }

    pub fn texture_id(&self) -> u16 {
        self.state & 0xFFF
    }

    /// `gSPTexture` parameters: (render tile, mip level count).
    pub fn render_tile(&self) -> Option<(u8, u8)> {
        self.dlist.iter().find_map(|g| match *g {
            Gfx::Texture { tile, level, .. } => Some((tile, level + 1)),
            _ => None,
        })
    }

    /// `gSPTexture` S/T scale (0.16 fixed; 0xFFFF ≈ 1.0).
    pub fn texture_scale(&self) -> [u16; 2] {
        self.dlist
            .iter()
            .find_map(|g| match *g {
                Gfx::Texture {
                    scale_s, scale_t, ..
                } => Some([scale_s, scale_t]),
                _ => None,
            })
            .unwrap_or([0xFFFF; 2])
    }

    /// Runs the display list against a fresh TMEM. `image2` must be the
    /// `image` of texture [`Self::image2`] if the list has a second `SetTImg`.
    pub fn load_tmem(&self, image2: Option<&[u8]>) -> Result<Tmem> {
        let mut tmem = Tmem::new();
        tmem.run(&self.dlist, &|n, addr| {
            let img = if n == 0 {
                Some(&self.image[..])
            } else {
                image2
            }?;
            img.get(addr as usize..)
        })?;
        Ok(tmem)
    }

    /// Decodes every tile the display list sets up (except the load tile).
    pub fn decode(&self, image2: Option<&[u8]>) -> Result<DecodedUvtx> {
        let Some((render_tile, levels)) = self.render_tile() else {
            bail!("UVTX display list has no G_TEXTURE");
        };
        let mut tmem = self.load_tmem(image2)?;
        let mut tiles = Vec::new();
        for i in 0..8u8 {
            if i == G_TX_LOADTILE && i != render_tile {
                continue;
            }
            // Some lists (e.g. 2D/sprite textures) set tiles up without
            // G_SETTILESIZE; the size then comes from the header.
            if tmem.desc(i).is_some() && tmem.tile(i).is_none() {
                let (w, h) = (self.width.max(1) - 1, self.height.max(1) - 1);
                tmem.set_tile_size(i, 0, 0, w << 2, h << 2);
            }
            if let Some(tile) = tmem.tile(i) {
                tiles.push(DecodedTile {
                    index: i,
                    source: tmem.source_of(i),
                    tile,
                    image: tmem.decode_tile(i)?,
                });
            }
        }
        ensure!(
            tiles.iter().any(|t| t.index == render_tile),
            "render tile {render_tile} is not set up"
        );
        Ok(DecodedUvtx {
            render_tile,
            levels,
            tiles,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic 8x2 IA8 texture: header, image, display list, trailer.
    #[test]
    fn parses_and_decodes_comm() {
        let image: Vec<u8> = (0..16).map(|i| (i << 4) | 0xF).collect();
        let dl: [u64; 5] = [
            0xBB00_0001_FFFF_FFFF, // gSPTexture(tile 0, 1 level)
            0xFD70_0000_0000_0000, // SetTImg IA 16b (8b LoadBlock idiom), offset 0
            0xF570_0000_0700_0000, // SetTile 7
            0xF300_0000_0700_7800, // LoadBlock 8 16-bit texels, dxt 0x800 (1 word/line)
            0xB800_0000_0000_0000,
        ];
        let mut b = Vec::new();
        b.extend_from_slice(&16u16.to_be_bytes());
        b.extend_from_slice(&(dl.len() as u16 + 2).to_be_bytes());
        b.extend_from_slice(&[0; 16]);
        b.extend_from_slice(&image);
        for c in &dl[..4] {
            b.extend_from_slice(&c.to_be_bytes());
        }
        // SetTile 0: IA8, line 1, and SetTileSize 8x2.
        b.extend_from_slice(&0xF568_0200_0000_0000u64.to_be_bytes());
        b.extend_from_slice(&0xF200_0000_0001_C004u64.to_be_bytes());
        b.extend_from_slice(&dl[4].to_be_bytes());
        b.extend_from_slice(&[
            0, 8, 0, 2, 8, 1, 1, 0x50, 7, 0x0F, 0xFF, 0, 0, 2, 1, 2, 0, 0,
        ]);
        b.extend_from_slice(&1.0f32.to_be_bytes());
        let t = Uvtx::parse_comm(&b).unwrap();
        assert_eq!(
            (t.width, t.height, t.texture_id(), t.image2),
            (8, 2, 7, NO_TEXTURE)
        );
        assert_eq!((t.channels, t.unk28), (2, 1.0));
        let dec = t.decode(None).unwrap();
        let img = &dec.base().image;
        assert_eq!((img.width, img.height), (8, 2));
        // With dxt the second row is swapped on load and unswapped on sampling.
        let expect: Vec<u8> = image.iter().flat_map(|&v| crate::tmem::ia8(v)).collect();
        assert_eq!(img.rgba, expect);
    }
}
