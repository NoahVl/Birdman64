//! `UVFT` fonts. Mirrors `uvParseTopUVFT` in `decomp/src/kernel/font.c`
//! (`ParsedUVFT` in `uv_graphics.h`).
//!
//! Blocks (any order, `PAD ` skipped):
//! - `STRG`: the character set, NUL-terminated ASCII; glyph `i` draws `str[i]`.
//! - `FRMT`: s32 `bmfmt` (`G_IM_FMT_*`), s32 `bmsiz` (`G_IM_SIZ_*`).
//! - `BITM`: libultra `Bitmap`\[\] (16 bytes: s16 width, s16 width_img,
//!   s16 s, s16 t, u32 buf = IMAG index, s16 actualHeight, s16 LUToffset).
//! - `IMAG` (MIO0): one per image sheet, linear texels, `width_img` wide.

use crate::gbi::{ImFmt, ImSiz};
use crate::tmem::{Image, linear_texel};
use anyhow::{Context, Result, ensure};
use pw64_rom::Form;

/// A glyph: a `Bitmap` window into an image sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyph {
    pub ch: u8,
    pub width: i16,
    /// Row stride of the sheet, in texels.
    pub width_img: i16,
    pub s: i16,
    pub t: i16,
    /// Index into [`Uvft::images`].
    pub image: u32,
    pub height: i16,
    pub lut_offset: i16,
}

#[derive(Debug, Clone)]
pub struct Uvft {
    pub chars: Vec<u8>,
    pub fmt: ImFmt,
    pub siz: ImSiz,
    pub glyphs: Vec<Glyph>,
    pub images: Vec<Vec<u8>>,
}

impl Uvft {
    pub fn parse(form: &Form) -> Result<Self> {
        ensure!(form.tag.0 == *b"UVFT", "not a UVFT file ({})", form.tag);
        let mut chars = Vec::new();
        let mut frmt = None;
        let mut bitm: &[u8] = &[];
        let mut images = Vec::new();
        for b in &form.blocks {
            match &b.tag.0 {
                b"STRG" => chars = b.data.split(|&c| c == 0).next().unwrap_or(&[]).to_vec(),
                b"FRMT" => {
                    ensure!(b.data.len() >= 8, "FRMT too short");
                    let w = |i: usize| u32::from_be_bytes(b.data[i..i + 4].try_into().unwrap());
                    frmt = Some((ImFmt::from_bits(w(0)), ImSiz::from_bits(w(4))));
                }
                b"BITM" => bitm = &b.data,
                b"IMAG" => images.push(b.data.clone()),
                b"PAD " => {}
                t => anyhow::bail!("unexpected UVFT block {}", String::from_utf8_lossy(t)),
            }
        }
        let (fmt, siz) = frmt.context("UVFT without FRMT")?;
        ensure!(bitm.len().is_multiple_of(16), "BITM size {}", bitm.len());
        let glyphs = bitm
            .as_chunks::<16>()
            .0
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let h = |o: usize| i16::from_be_bytes([e[o], e[o + 1]]);
                let g = Glyph {
                    ch: chars.get(i).copied().unwrap_or(0),
                    width: h(0),
                    width_img: h(2),
                    s: h(4),
                    t: h(6),
                    image: u32::from_be_bytes(e[8..12].try_into().unwrap()),
                    height: h(12),
                    lut_offset: h(14),
                };
                ensure!(
                    (g.image as usize) < images.len(),
                    "glyph {i} uses image {} of {}",
                    g.image,
                    images.len()
                );
                Ok(g)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            chars,
            fmt,
            siz,
            glyphs,
            images,
        })
    }

    /// Decodes image sheet `i` (width from the glyphs' `width_img`).
    pub fn decode_image(&self, i: usize) -> Result<Image> {
        let data = &self.images[i];
        let width = self
            .glyphs
            .iter()
            .find(|g| g.image as usize == i)
            .map(|g| g.width_img as usize)
            .context("image without glyphs")?;
        ensure!(width > 0, "zero-width sheet");
        let height = data.len() * 8 / (self.siz.bits() as usize * width);
        let mut rgba = Vec::with_capacity(width * height * 4);
        for idx in 0..width * height {
            rgba.extend_from_slice(
                &linear_texel(self.fmt, self.siz, data, idx)
                    .with_context(|| format!("unsupported font format {:?}", self.fmt))?,
            );
        }
        Ok(Image {
            width: width as u32,
            height: height as u32,
            rgba,
        })
    }
}
