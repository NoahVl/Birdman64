//! `UVTP` texture palettes: per-environment texture id remaps (e.g. snowy or
//! dusk variants of terrain textures). Mirrors `_uvParseUVTP` in
//! `decomp/src/kernel/texture.c`; selected with `uvMemLoadPal` (palette id =
//! COMM index) before a level is appended.
//!
//! Each `COMM`: u16 count, then count × (u16 texture id, u16 replacement id).
//!
//! How the engine applies it (`uvLevelAppend`): texture slot `id` is loaded
//! from UVTX `remap(id)` (and so is its image, `D_802B6E30[id]`). A
//! texture's second image (`unk14`) is looked up by slot, so it is remapped
//! too. `_uvExpandTexture` maps the loaded texture's own id back to the slot.

use crate::reader::Reader;
use anyhow::{Context, Result, ensure};
use pw64_rom::Form;

/// `ParsedUVTP`.
#[derive(Debug, Clone, Default)]
pub struct Uvtp {
    /// (texture id, replacement UVTX id); the first match wins.
    pub remaps: Vec<(u16, u16)>,
}

impl Uvtp {
    /// Mirrors `_uvParseUVTP`.
    pub fn parse_comm(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b, "UVTP COMM");
        let n = r.u16()?;
        let remaps = (0..n)
            .map(|_| Ok((r.u16()?, r.u16()?)))
            .collect::<Result<Vec<_>>>()?;
        r.expect_padding()?;
        Ok(Self { remaps })
    }

    /// The UVTX that texture slot `id` is loaded from.
    pub fn remap(&self, id: u16) -> u16 {
        self.remaps
            .iter()
            .find(|&&(from, _)| from == id)
            .map_or(id, |&(_, to)| to)
    }
}

/// Parses every palette in the `UVTP` file (index = palette id).
pub fn parse(form: &Form) -> Result<Vec<Uvtp>> {
    ensure!(form.tag.0 == *b"UVTP", "not a UVTP file ({})", form.tag);
    form.blocks
        .iter()
        .filter(|b| b.tag.0 == *b"COMM")
        .enumerate()
        .map(|(i, b)| Uvtp::parse_comm(&b.data).with_context(|| format!("palette {i}")))
        .collect()
}
