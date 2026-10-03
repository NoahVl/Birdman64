//! `UVTR` "terras": terrain grids of `UVCT` tiles. Mirrors `_uvParseUVTR`
//! in `decomp/src/kernel/texture.c` (`ParsedUVTR`, `uvUnkTileStruct` in
//! `uv_graphics.h`).
//!
//! The single UVTR file holds one `COMM` block per terra; the engine picks
//! the n-th with `uvFile_80224170` (terra id = COMM index). Each block,
//! big-endian, read sequentially (no padding):
//!
//! | type | meaning |
//! |---|---|
//! | f32 ×6 | bounding box: min xyz, max xyz (world units, Z-up) |
//! | u8 ×2 | grid columns (x), rows (y) |
//! | f32 ×3 | cell size x, cell size y, `unk24` (a height, see `uvChan`) |
//! | cell\[cols × rows\] | u8 present; if non-zero: `Mtx4F`, u8 rotation, u16 UVCT id |
//!
//! Cells are row-major from the box minimum (`index = col + cols * row`,
//! `_uvTerraDraw`). The cell matrix places the tile (translation = cell
//! centre); `rotation` counts quarter turns (`func_80214840`), already baked
//! into the matrix and only used for sector culling.

use crate::reader::Reader;
use anyhow::{Context, Result, ensure};
use pw64_rom::Form;

/// `uvUnkTileStruct`.
#[derive(Debug, Clone)]
pub struct Cell {
    /// Tile transform (row-vector convention, `m[3]` = translation).
    pub matrix: [[f32; 4]; 4],
    /// Quarter turns (0..=3) of the tile about Z.
    pub rotation: u8,
    /// Global UVCT index.
    pub contour: u16,
}

/// `ParsedUVTR`: one terra.
#[derive(Debug, Clone)]
pub struct Terra {
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub cols: u8,
    pub rows: u8,
    pub cell_size: [f32; 2],
    pub unk24: f32,
    /// `cols * rows` cells, `None` for empty ones.
    pub cells: Vec<Option<Cell>>,
}

impl Terra {
    /// Mirrors `_uvParseUVTR`.
    pub fn parse_comm(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b, "UVTR COMM");
        let min = [r.f32()?, r.f32()?, r.f32()?];
        let max = [r.f32()?, r.f32()?, r.f32()?];
        let cols = r.u8()?;
        let rows = r.u8()?;
        let cell_size = [r.f32()?, r.f32()?];
        let unk24 = r.f32()?;
        let cells = (0..cols as usize * rows as usize)
            .map(|_| {
                if r.u8()? == 0 {
                    return Ok(None);
                }
                Ok(Some(Cell {
                    matrix: r.mtx4f()?,
                    rotation: r.u8()?,
                    contour: r.u16()?,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        r.expect_padding()?;
        Ok(Self {
            min,
            max,
            cols,
            rows,
            cell_size,
            unk24,
            cells,
        })
    }

    /// Cell at grid position (`col`, `row`).
    pub fn cell(&self, col: usize, row: usize) -> Option<&Cell> {
        self.cells.get(col + self.cols as usize * row)?.as_ref()
    }
}

/// Parses every terra in the `UVTR` file (index = terra id).
pub fn parse(form: &Form) -> Result<Vec<Terra>> {
    ensure!(form.tag.0 == *b"UVTR", "not a UVTR file ({})", form.tag);
    form.blocks
        .iter()
        .filter(|b| b.tag.0 == *b"COMM")
        .enumerate()
        .map(|(i, b)| Terra::parse_comm(&b.data).with_context(|| format!("terra {i}")))
        .collect()
}
