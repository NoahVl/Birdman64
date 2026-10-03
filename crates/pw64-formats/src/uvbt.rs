//! `UVBT` "blits": 2D images drawn with the sprite microcode, split into
//! strips of `Bitmap`s. Mirrors `_uvParseUVBT` in
//! `decomp/src/kernel/texture.c` (`ParsedUVBT` in `uv_graphics.h`).
//!
//! One uncompressed `COMM` block, big-endian:
//!
//! | type | meaning |
//! |---|---|
//! | u16 | `bmfmt` (`G_IM_FMT_*`) |
//! | u16 | bit depth (4/8/16/32) |
//! | u16 | width (visible) |
//! | u16 | stride (width padded to whole tiles; only sizes the buffer) |
//! | u16 | height |
//! | u16 | tile width (`Bitmap.width_img`) |
//! | u16 | tile height (`texelHeight`) |
//! | u8\[stride × height × depth / 8\] | texels |
//!
//! The texels are stored tile by tile (row-major over tiles), each tile
//! `tile width × actual height` texels. Blits drawn with `SP_TEXSHUF`
//! (everything but RGBA32, see [`Uvbt::swizzled`]) are pre-shuffled for a
//! `LoadBlock` with `dxt = 0`: on every odd row of a tile the two 32-bit
//! words of each 64-bit word are swapped. [`Uvbt::decode`] undoes that.

use crate::gbi::{ImFmt, ImSiz};
use crate::reader::Reader;
use crate::tmem::{Image, linear_texel};
use anyhow::{Context, Result, ensure};
use pw64_rom::Form;

/// One libultra `Bitmap` of the blit (`bitmap[j + i * cols]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitmapTile {
    /// Visible size of this tile.
    pub width: u16,
    pub height: u16,
    /// Top-left position in the whole image.
    pub x: u16,
    pub y: u16,
    /// Byte offset of the tile in [`Uvbt::data`].
    pub offset: usize,
}

#[derive(Debug, Clone)]
pub struct Uvbt {
    pub fmt: u16,
    pub depth: u16,
    pub width: u16,
    pub stride: u16,
    pub height: u16,
    pub tile_width: u16,
    pub tile_height: u16,
    pub data: Vec<u8>,
    pub tiles: Vec<BitmapTile>,
}

impl Uvbt {
    pub fn parse(form: &Form) -> Result<Self> {
        ensure!(form.tag.0 == *b"UVBT", "not a UVBT file ({})", form.tag);
        let comm = form.block(b"COMM").context("UVBT without COMM block")?;
        Self::parse_comm(&comm.data)
    }

    /// Mirrors `_uvParseUVBT`.
    pub fn parse_comm(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b, "UVBT COMM");
        let mut h = [0u16; 7];
        for v in &mut h {
            *v = r.u16()?;
        }
        let [fmt, depth, width, stride, height, tile_width, tile_height] = h;
        ensure!(
            matches!(depth, 4 | 8 | 16 | 32) && tile_width > 0 && tile_height > 0,
            "bad UVBT header: depth {depth}, tile {tile_width}x{tile_height}"
        );
        let size = stride as usize * height as usize * depth as usize / 8;
        let data = r.take(size)?.to_vec();
        r.expect_padding()?;

        let cols = (width as usize).div_ceil(tile_width as usize);
        let rows = (height as usize).div_ceil(tile_height as usize);
        let mut tiles = Vec::with_capacity(cols * rows);
        let mut offset = 0;
        for i in 0..rows {
            let h = if i + 1 == rows && height % tile_height != 0 {
                height % tile_height
            } else {
                tile_height
            };
            for j in 0..cols {
                let w = if j + 1 == cols && width % tile_width != 0 {
                    width - tile_width * (cols as u16 - 1)
                } else {
                    tile_width
                };
                tiles.push(BitmapTile {
                    width: w,
                    height: h,
                    x: j as u16 * tile_width,
                    y: i as u16 * tile_height,
                    offset,
                });
                offset += h as usize * tile_width as usize * depth as usize / 8;
            }
        }
        ensure!(
            offset <= data.len(),
            "tiles need {offset} bytes, have {}",
            data.len()
        );
        Ok(Self {
            fmt,
            depth,
            width,
            stride,
            height,
            tile_width,
            tile_height,
            data,
            tiles,
        })
    }

    pub fn im_fmt(&self) -> ImFmt {
        ImFmt::from_bits(self.fmt as u32)
    }

    pub fn im_siz(&self) -> ImSiz {
        match self.depth {
            4 => ImSiz::B4,
            8 => ImSiz::B8,
            16 => ImSiz::B16,
            _ => ImSiz::B32,
        }
    }

    /// Whether the texels are TMEM-shuffled (odd rows word-swapped).
    /// Mirrors `uvSprtSetBlit`: every format but RGBA gets `SP_TEXSHUF`, RGBA
    /// only at 16 bit; `spDraw` then loads with `gDPLoadTextureBlockS`
    /// (`dxt = 0`, so the RDP does not swap), else with a swapping
    /// `gDPLoadTextureBlock` from linear data. Needs whole 64-bit tile rows.
    pub fn swizzled(&self) -> bool {
        let shuf = self.im_fmt() != ImFmt::Rgba || self.depth == 16;
        shuf && (self.tile_width as usize * self.depth as usize).is_multiple_of(64)
    }

    /// The texels with the TMEM shuffle undone (see [`Self::swizzled`]):
    /// tile by tile, linear.
    pub fn linear_data(&self) -> std::borrow::Cow<'_, [u8]> {
        if !self.swizzled() {
            return std::borrow::Cow::Borrowed(&self.data);
        }
        let mut data = self.data.clone();
        let row = self.tile_width as usize * self.depth as usize / 8;
        for t in &self.tiles {
            for y in (1..t.height as usize).step_by(2) {
                let start = t.offset + y * row;
                for q in data[start..start + row].as_chunks_mut::<8>().0 {
                    q.rotate_left(4);
                }
            }
        }
        std::borrow::Cow::Owned(data)
    }

    /// Assembles the tiles into one RGBA8 image (TMEM shuffle undone).
    pub fn decode(&self) -> Result<Image> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut rgba = vec![0; w * h * 4];
        let bits = self.depth as usize;
        let data = self.linear_data();
        for t in &self.tiles {
            let tile = &data[t.offset..];
            for y in 0..t.height as usize {
                for x in 0..t.width as usize {
                    let i = y * self.tile_width as usize + x;
                    let px = linear_texel(self.im_fmt(), self.im_siz(), tile, i)
                        .with_context(|| format!("unsupported blit format {}/{bits}", self.fmt))?;
                    let o = ((t.y as usize + y) * w + t.x as usize + x) * 4;
                    rgba[o..o + 4].copy_from_slice(&px);
                }
            }
        }
        Ok(Image {
            width: w as u32,
            height: h as u32,
            rgba,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-tile blit whose rows are 8 bytes each.
    fn comm(fmt: u16, depth: u16, rows: &[[u8; 8]]) -> Vec<u8> {
        let w = 64 / depth;
        let mut b = Vec::new();
        for v in [fmt, depth, w, w, rows.len() as u16, w, rows.len() as u16] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        rows.iter().for_each(|r| b.extend_from_slice(r));
        b
    }

    #[test]
    fn odd_rows_unshuffled_except_rgba32() {
        let rows = [[0, 1, 2, 3, 4, 5, 6, 7], [4, 5, 6, 7, 0, 1, 2, 3]];
        let b = Uvbt::parse_comm(&comm(0, 16, &rows)).unwrap();
        assert!(b.swizzled());
        assert_eq!(&b.linear_data()[8..], &[0, 1, 2, 3, 4, 5, 6, 7]);
        let b = Uvbt::parse_comm(&comm(3, 8, &rows)).unwrap();
        assert!(b.swizzled());
        let b = Uvbt::parse_comm(&comm(0, 32, &rows)).unwrap();
        assert!(!b.swizzled());
        assert_eq!(&b.linear_data()[8..], &rows[1]);
    }
}
