//! `UVCT` "contours": one terrain tile's geometry, collision triangles and
//! placed models. Mirrors `_uvParseUVCT` in `decomp/src/kernel/texture.c`;
//! C structs `ParsedUVCT`, `Unk80225FBC_0x28(_UnkC)` (`uv_graphics.h`) and
//! `UnkSobjDraw` (`uv_sobj.h`).
//!
//! One MIO0 `COMM` block, big-endian, read sequentially (no padding):
//!
//! | type | meaning |
//! |---|---|
//! | u16 ×4 | vertex count, collision-tri count, placement count, draw-state count |
//! | Vtx\[vertex count\] | 16 bytes each, tile-local world units (Z-up) |
//! | u16 ×4 \[tri count\] | collision triangles: 3 vertex indices + sector mask |
//! | placement\[\] | u8 matrix count, fixed-point `Mtx`\[n\] (one per model part), u16 model id, f32 xyz, u16 sector mask, u16 `unk16` |
//! | draw state\[\] | state + compact geometry (as UVMD), u16 first tri, u16 tri count, u16 sector mask, u16 `unk14`, f32 ×4 bounding sphere |
//! | f32 ×5 | bounding sphere (xyz, radius), `unk28` (lighting blend) |
//!
//! Sector masks: `_uvTerraDraw` splits the view around a tile into 16
//! angular sectors and only draws states / models / collision tris whose
//! mask intersects the visible ones.

use crate::reader::Reader;
use crate::uvmd::{self, State, Vtx};
use anyhow::{Context, Result, ensure};
use pw64_rom::Form;
use std::ops::Range;

/// `Unk80225FBC_0x28_UnkC`: a collision triangle (`uvTerraGetPt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollisionTri {
    pub v: [u16; 3],
    pub sectors: u16,
}

/// `UnkSobjDraw`: a UVMD model placed on the tile (`uvSobjsDraw`).
#[derive(Debug, Clone)]
pub struct Placement {
    /// UVMD index; `0xFFFF` = skipped by the engine.
    pub model: u16,
    /// Per-part matrices (row-vector convention), pushed like a model's own
    /// part matrices: part 0 is relative to the tile and includes the
    /// model's `1/scale`; the rest are parent-relative.
    pub matrices: Vec<[[f32; 4]; 4]>,
    /// Tile-local position used for culling and LOD distance.
    pub pos: [f32; 3],
    pub sectors: u16,
    pub unk16: u16,
}

/// `Unk80225FBC_0x28`: a render state with its geometry.
#[derive(Debug, Clone)]
pub struct DrawState {
    pub state: State,
    /// Range into [`Uvct::collision`] belonging to this state.
    pub collision: Range<usize>,
    pub sectors: u16,
    pub unk14: u16,
    /// Bounding sphere centre (tile-local) and radius.
    pub center: [f32; 3],
    pub radius: f32,
}

#[derive(Debug, Clone)]
pub struct Uvct {
    pub vertices: Vec<Vtx>,
    pub collision: Vec<CollisionTri>,
    pub placements: Vec<Placement>,
    pub states: Vec<DrawState>,
    /// Bounding sphere of the whole tile (tile-local).
    pub center: [f32; 3],
    pub radius: f32,
    /// `unk28`: vertex-colour lighting blend (`code_8170.c`).
    pub unk28: f32,
}

impl Uvct {
    pub fn parse(form: &Form) -> Result<Self> {
        ensure!(form.tag.0 == *b"UVCT", "not a UVCT file ({})", form.tag);
        let comm = form.block(b"COMM").context("UVCT without COMM block")?;
        Self::parse_comm(&comm.data)
    }

    /// Mirrors `_uvParseUVCT`.
    pub fn parse_comm(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b, "UVCT COMM");
        let vtx_count = r.u16()? as usize;
        let tri_count = r.u16()? as usize;
        let placement_count = r.u16()? as usize;
        let state_count = r.u16()? as usize;
        let vertices: Vec<Vtx> = (0..vtx_count)
            .map(|_| Ok(Vtx::from_bytes(&r.arr()?)))
            .collect::<Result<_>>()?;
        let collision = (0..tri_count)
            .map(|_| {
                let v = [r.u16()?, r.u16()?, r.u16()?];
                let sectors = r.u16()?;
                ensure!(
                    v.iter().all(|&i| (i as usize) < vtx_count),
                    "collision tri {v:?} past {vtx_count} vertices"
                );
                Ok(CollisionTri { v, sectors })
            })
            .collect::<Result<Vec<_>>>()?;

        let mut placements = Vec::with_capacity(placement_count);
        for _ in 0..placement_count {
            let n = r.u8()?;
            let matrices = (0..n).map(|_| r.mtx_fixed()).collect::<Result<_>>()?;
            placements.push(Placement {
                model: r.u16()?,
                matrices,
                pos: [r.f32()?, r.f32()?, r.f32()?],
                sectors: r.u16()?,
                unk16: r.u16()?,
            });
        }

        let mut states = Vec::with_capacity(state_count);
        for i in 0..state_count {
            let state = uvmd::read_state(&mut r, vtx_count)?;
            let first = r.u16()? as usize;
            let count = r.u16()? as usize;
            ensure!(
                first + count <= tri_count,
                "state {i}: collision {first}+{count} past {tri_count}"
            );
            states.push(DrawState {
                state,
                collision: first..first + count,
                sectors: r.u16()?,
                unk14: r.u16()?,
                center: [r.f32()?, r.f32()?, r.f32()?],
                radius: r.f32()?,
            });
        }
        let center = [r.f32()?, r.f32()?, r.f32()?];
        let radius = r.f32()?;
        let unk28 = r.f32()?;
        r.expect_padding()?;
        Ok(Self {
            vertices,
            collision,
            placements,
            states,
            center,
            radius,
            unk28,
        })
    }

    /// Triangles of one draw state (runs its F3D list).
    pub fn triangles(&self, state: &State) -> Result<Vec<[Vtx; 3]>> {
        uvmd::triangles(&self.vertices, state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_tile() {
        let mut b = Vec::new();
        for n in [3u16, 1, 1, 1] {
            b.extend_from_slice(&n.to_be_bytes());
        }
        for (x, y) in [(0i16, 0i16), (100, 0), (0, 100)] {
            for v in [x, y, 5, 0, 0, 0] {
                b.extend_from_slice(&v.to_be_bytes());
            }
            b.extend_from_slice(&[255, 255, 255, 255]);
        }
        for v in [0u16, 1, 2, 0xFFFF] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        // Placement: one identity matrix translated by (1.5, 0, 0).
        b.push(1);
        let mut ints = [0i16; 16];
        let mut fracs = [0u16; 16];
        for i in 0..4 {
            ints[i * 5] = 1;
        }
        ints[12] = 1;
        fracs[12] = 0x8000;
        ints.iter()
            .for_each(|v| b.extend_from_slice(&v.to_be_bytes()));
        fracs
            .iter()
            .for_each(|v| b.extend_from_slice(&v.to_be_bytes()));
        b.extend_from_slice(&7u16.to_be_bytes());
        for f in [1.5f32, 0.0, 0.0] {
            b.extend_from_slice(&f.to_be_bytes());
        }
        b.extend_from_slice(&[0xFF, 0xFF, 0, 0]);
        // Draw state: 1 vertex load + 1 triangle.
        b.extend_from_slice(&0x0020_0FFFu32.to_be_bytes());
        b.extend_from_slice(&[0, 3, 0, 1, 0, 2]);
        b.extend_from_slice(&[0x00, 0x00, 0x20, 0x40, 0x12]);
        for v in [0u16, 1, 0xFFFF, 0] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        for f in [0.0f32; 9] {
            b.extend_from_slice(&f.to_be_bytes());
        }
        b.extend_from_slice(&[0, 0]);
        let c = Uvct::parse_comm(&b).unwrap();
        assert_eq!(c.placements[0].model, 7);
        assert_eq!(c.placements[0].matrices[0][3][0], 1.5);
        assert_eq!(c.placements[0].matrices[0][1][1], 1.0);
        assert_eq!(c.states[0].collision, 0..1);
        let t = c.triangles(&c.states[0].state).unwrap();
        assert_eq!(t[0][1].pos, [100, 0, 5]);
    }
}
