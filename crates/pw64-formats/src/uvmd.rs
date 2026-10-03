//! `UVMD` models. Mirrors `_uvParseUVMD` in `decomp/src/kernel/texture.c`;
//! the parsed structs are `ParsedUVMD`, `uvModelLOD`, `uvModelPart` and
//! `uvGfxState_t` in `decomp/include/kernel/uv_graphics.h`.
//!
//! A UVMD file has one (MIO0-compressed) `COMM` block, all big-endian:
//!
//! | type | meaning |
//! |---|---|
//! | u16 | vertex count |
//! | u8 | LOD count |
//! | u8 | matrix count (= parts per LOD) |
//! | u8 | volume count (`sp7A`, see [`Volume`]) |
//! | u8 | transparent flag (→ `UVMD_ATTR_TRANSPARENT`) |
//! | u16 | volume item count (`sp6E`) |
//! | Vtx\[vertex count\] | 16 bytes each: s16 xyz, u16 flag, s16 st, u8 rgba/normal |
//! | LOD\[\] | u8 part count, u8 billboard, parts, then f32 LOD radius |
//! | part | u8 state count, u8 `unk5`, u8 depth (`unk6`), states |
//! | state | u32 state, s16 xfm count, s16 tri count, u16 cmd count, cmds |
//! | cmd | u16 `c`: `c & 0x4000` → triangle `(c>>8)&15, (c>>4)&15, c&15`; else `gSPVertex(vtx[c & 0x3FFF], (b>>4)+1, b&15)` with a following u8 `b` |
//! | Mtx4F\[matrix count\] | f32 4×4, one per part (row vectors, `m[3]` = translation) |
//! | Volume\[volume count\] | 0x24 raw bytes each (C struct incl. padding and a pointer slot) |
//! | f32 ×3 | `unk1C`, `scale` (`unk20`), `unk24` |
//! | u16×3 \[item count\] | volume items (`UnkUVMD_6`) |
//!
//! The engine expands the compact command stream into real F3D display lists
//! at load time; [`State::dlist`] holds that expansion, with vertex addresses
//! given as byte offsets into [`Uvmd::vertices`].

use crate::gbi::{Gfx, VTX_CACHE_SIZE, VTX_SIZE};
use crate::reader::Reader;
use anyhow::{Context, Result, bail, ensure};
use pw64_rom::Form;
use std::ops::Range;

/// Render-state bits of [`State::state`] (`GFX_STATE_*`). The low 12 bits
/// are the texture id ([`TEXTURE_NONE`] = untextured).
pub mod state {
    pub const TEXTURE_MASK: u32 = 0xFFF;
    pub const GOURAUD: u32 = 1 << 17;
    pub const CULL_FRONT: u32 = 1 << 19;
    pub const CULL_BACK: u32 = 1 << 20;
    pub const ZBUFFER: u32 = 1 << 21;
    pub const AA: u32 = 1 << 22;
    /// Translucent render mode.
    pub const XLU: u32 = 1 << 23;
    pub const DECAL: u32 = 1 << 24;
    /// Drawn twice via `uvGfxStateDrawDL` (shadow/cutout pass).
    pub const DRAW_DL: u32 = 1 << 25;
    /// `G_LIGHTING | G_TEXTURE_GEN`: vertex `cn` are normals, UVs are
    /// generated (environment map) with `gSPTexture(0x7C0, 0x7C0)`.
    pub const LIGHTING: u32 = 1 << 27;
    pub const FOG: u32 = 1 << 31;
}

/// Texture id meaning "no texture" (`GFX_STATE_TEXTURE_NONE`).
pub const TEXTURE_NONE: u16 = 0xFFF;

/// One `Vtx` (`Vtx_t` / `Vtx_tn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Vtx {
    pub pos: [i16; 3],
    pub flag: u16,
    /// Texture coordinates, s10.5 texels (before `gSPTexture` scaling).
    pub st: [i16; 2],
    /// RGBA, or (with lighting) a signed normal in bytes 0..3 plus alpha.
    pub color: [u8; 4],
}

impl Vtx {
    pub fn from_bytes(b: &[u8; 16]) -> Self {
        let s16 = |i: usize| i16::from_be_bytes([b[i], b[i + 1]]);
        Self {
            pos: [s16(0), s16(2), s16(4)],
            flag: u16::from_be_bytes([b[6], b[7]]),
            st: [s16(8), s16(10)],
            color: [b[12], b[13], b[14], b[15]],
        }
    }

    /// `color` read as a normal (`Vtx_tn.n`), normalized.
    pub fn normal(&self) -> [f32; 3] {
        let n = [0, 1, 2].map(|i| self.color[i] as i8 as f32);
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len == 0.0 {
            [0.0, 0.0, 1.0]
        } else {
            n.map(|c| c / len)
        }
    }
}

/// `uvGfxState_t`: a render state and the display list drawn with it.
#[derive(Debug, Clone)]
pub struct State {
    /// `GFX_STATE_*` bits ([`state`]) | texture id.
    pub state: u32,
    /// Vertex / triangle counts for the engine's stats.
    pub xfm_count: i16,
    pub tri_count: i16,
    /// The expanded F3D list (`G_VTX`, `G_TRI1`, ..., `G_ENDDL`).
    pub dlist: Vec<Gfx>,
}

impl State {
    /// Texture id, or `None` if untextured.
    pub fn texture(&self) -> Option<u16> {
        // uvGfxStateDraw treats 0xFFE like 0xFFF.
        match (self.state & state::TEXTURE_MASK) as u16 {
            id if id >= 0xFFE => None,
            id => Some(id),
        }
    }
}

/// `uvModelPart`: a transform node with its render states.
#[derive(Debug, Clone)]
pub struct Part {
    pub states: Vec<State>,
    pub unk5: u8,
    /// Depth in the part hierarchy (`unk6`): the parent is the nearest
    /// earlier part with depth − 1 (see `uvDobj_80217B4C`'s pop counts).
    pub depth: u8,
}

/// `uvModelLOD`.
#[derive(Debug, Clone)]
pub struct Lod {
    pub parts: Vec<Part>,
    /// Drawn camera-facing (`uvDobj_80217E24`).
    pub billboard: bool,
    /// `lodRadius`: switch distance used by `uvDobjGetLODIndex`.
    pub radius: f32,
}

/// `UnkUVMD_24`: per-part volume, used by the collision code
/// (`func_802133C8`, `_uvSegInMboxs`). Field meanings are guesses.
#[derive(Debug, Clone, PartialEq)]
pub struct Volume {
    /// Part index the volume belongs to.
    pub part: u8,
    pub unk1: u8,
    pub unk2: u8,
    /// 6 floats (`UnkUVMD_24_Unk4`), probably a box.
    pub bounds: [f32; 6],
    /// Range into [`Uvmd::volume_items`] (the file stores cumulative ends;
    /// the parser converts them like `_uvParseUVMD` does).
    pub items: Range<usize>,
}

#[derive(Debug, Clone)]
pub struct Uvmd {
    pub vertices: Vec<Vtx>,
    pub lods: Vec<Lod>,
    /// One matrix per part index (shared by all LODs), row-vector
    /// convention (`m[3]` = translation), relative to the parent part.
    pub matrices: Vec<[[f32; 4]; 4]>,
    /// `UVMD_ATTR_TRANSPARENT`: sorted as translucent.
    pub transparent: bool,
    pub volumes: Vec<Volume>,
    /// `UnkUVMD_6` entries referenced by [`Volume::items`].
    pub volume_items: Vec<[u16; 3]>,
    /// `unk1C` (queried as `MODEL_PROPID_UNK1`, used as a collision radius).
    pub unk1c: f32,
    /// `unk20`: model-to-world scale (`uvModelGetPosm` divides translations by it).
    pub scale: f32,
    /// `unk24`: blend factor in `code_8170.c` lighting.
    pub unk24: f32,
}

impl Uvmd {
    /// Parses a `UVMD` FORM (as returned by `Filesystem::read`).
    pub fn parse(form: &Form) -> Result<Self> {
        ensure!(form.tag.0 == *b"UVMD", "not a UVMD file ({})", form.tag);
        let comm = form.block(b"COMM").context("UVMD without COMM block")?;
        Self::parse_comm(&comm.data)
    }

    /// Parses the (decompressed) `COMM` block. Mirrors `_uvParseUVMD`.
    pub fn parse_comm(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b, "UVMD COMM");
        let vtx_count = r.u16()? as usize;
        let lod_count = r.u8()?;
        let mtx_count = r.u8()?;
        let volume_count = r.u8()?;
        let transparent = r.u8()? != 0;
        let item_count = r.u16()?;
        let vertices: Vec<Vtx> = (0..vtx_count)
            .map(|_| Ok(Vtx::from_bytes(&r.arr()?)))
            .collect::<Result<_>>()?;

        let mut lods = Vec::with_capacity(lod_count as usize);
        for _ in 0..lod_count {
            let part_count = r.u8()?;
            let billboard = r.u8()? != 0;
            let mut parts = Vec::with_capacity(part_count as usize);
            for _ in 0..part_count {
                let state_count = r.u8()?;
                let unk5 = r.u8()?;
                let depth = r.u8()?;
                let mut states = Vec::with_capacity(state_count as usize);
                for _ in 0..state_count {
                    states.push(read_state(&mut r, vertices.len())?);
                }
                parts.push(Part {
                    states,
                    unk5,
                    depth,
                });
            }
            let radius = r.f32()?;
            lods.push(Lod {
                parts,
                billboard,
                radius,
            });
        }

        let matrices = (0..mtx_count)
            .map(|_| r.mtx4f())
            .collect::<Result<Vec<_>>>()?;

        // UnkUVMD_24 is copied raw (sizeof = 0x24, with padding and pointer).
        let mut raw_volumes = Vec::with_capacity(volume_count as usize);
        for _ in 0..volume_count {
            let v = r.take(0x24)?;
            let f = |i: usize| f32::from_be_bytes(v[4 + i * 4..8 + i * 4].try_into().unwrap());
            raw_volumes.push((
                [v[0], v[1], v[2]],
                [0, 1, 2, 3, 4, 5].map(f),
                u16::from_be_bytes([v[0x1C], v[0x1D]]) as usize,
            ));
        }
        let unk1c = r.f32()?;
        let scale = r.f32()?;
        let unk24 = r.f32()?;
        let volume_items = (0..item_count)
            .map(|_| Ok([r.u16()?, r.u16()?, r.u16()?]))
            .collect::<Result<Vec<_>>>()?;
        r.expect_padding()?;

        // Each volume stores the cumulative end of its item range.
        let mut volumes = Vec::with_capacity(raw_volumes.len());
        let mut start = 0;
        for (i, (h, bounds, n)) in raw_volumes.into_iter().enumerate() {
            let end = n;
            ensure!(
                end >= start && end <= volume_items.len(),
                "volume {i} items {start}..{end} out of range"
            );
            volumes.push(Volume {
                part: h[0],
                unk1: h[1],
                unk2: h[2],
                bounds,
                items: start..end,
            });
            start = end;
        }

        Ok(Self {
            vertices,
            lods,
            matrices,
            transparent,
            volumes,
            volume_items,
            unk1c,
            scale,
            unk24,
        })
    }

    /// Parent part of `part` in `lod` (None for the root).
    pub fn parent(&self, lod: usize, part: usize) -> Option<usize> {
        let parts = &self.lods[lod].parts;
        let d = parts[part].depth;
        (0..part).rev().find(|&j| parts[j].depth < d)
    }

    /// Runs a state's display list against this model's vertex table.
    pub fn triangles(&self, state: &State) -> Result<Vec<[Vtx; 3]>> {
        triangles(&self.vertices, state)
    }
}

/// Runs a state's display list against `vertices` (G_VTX addresses are byte
/// offsets into the table, as [`read_state`] builds them).
pub fn triangles(vertices: &[Vtx], state: &State) -> Result<Vec<[Vtx; 3]>> {
    let mut ex = Executor::default();
    ex.run(&state.dlist, &|addr, n| {
        let i = (addr / VTX_SIZE) as usize;
        vertices.get(i..i + n)
    })?;
    Ok(ex.triangles)
}

/// Reads one `uvGfxState_t` with its compact geometry stream and expands it
/// to F3D, exactly as `_uvParseUVMD` and `_uvParseUVCT` do: u32 state,
/// u16 xfm count, u16 tri count, u16 command count, then the commands.
pub(crate) fn read_state(r: &mut Reader, vtx_count: usize) -> Result<State> {
    let state = r.u32()?;
    let xfm_count = r.u16()? as i16;
    let tri_count = r.u16()? as i16;
    let gfx_count = r.u16()?;
    let mut dlist = Vec::with_capacity(gfx_count as usize + 1);
    for _ in 0..gfx_count {
        let c = r.u16()?;
        let g = if c & 0x4000 != 0 {
            let v = [(c >> 8) & 0xF, (c >> 4) & 0xF, c & 0xF].map(|i| i as u8);
            Gfx::Tri1 { v, flag: 0 }
        } else {
            let b = r.u8()?;
            let index = (c & 0x3FFF) as usize;
            let count = (b >> 4) + 1;
            ensure!(
                index + count as usize <= vtx_count,
                "gSPVertex {index}+{count} past {vtx_count} vertices"
            );
            Gfx::Vertex {
                count,
                v0: b & 0xF,
                addr: index as u32 * VTX_SIZE,
            }
        };
        // Round-trip through the real encoding so the list is exactly what
        // the engine hands the RSP.
        let (w0, w1) = g.encode().expect("geometry command");
        dlist.push(Gfx::decode(w0, w1));
    }
    dlist.push(Gfx::EndDl);
    Ok(State {
        state,
        xfm_count,
        tri_count,
        dlist,
    })
}

/// A minimal RSP geometry executor: F3D vertex cache + triangle assembly.
/// Texture/RDP state commands are accepted and ignored (the caller maps
/// texture coordinates); anything that would need matrices or nested lists
/// is an error for now.
#[derive(Debug, Clone)]
pub struct Executor {
    pub cache: [Option<Vtx>; VTX_CACHE_SIZE],
    pub geometry_mode: u32,
    pub triangles: Vec<[Vtx; 3]>,
}

impl Default for Executor {
    fn default() -> Self {
        Self {
            cache: [None; VTX_CACHE_SIZE],
            geometry_mode: 0,
            triangles: Vec::new(),
        }
    }
}

impl Executor {
    /// Executes `dl` until `G_ENDDL`. `vtx(addr, n)` resolves a `G_VTX` source.
    pub fn run<'v>(
        &mut self,
        dl: &[Gfx],
        vtx: &dyn Fn(u32, usize) -> Option<&'v [Vtx]>,
    ) -> Result<()> {
        for g in dl {
            match *g {
                Gfx::Vertex { count, v0, addr } => {
                    let (n, v0) = (count as usize, v0 as usize);
                    ensure!(
                        v0 + n <= VTX_CACHE_SIZE,
                        "G_VTX {v0}+{n} overflows the cache"
                    );
                    let src =
                        vtx(addr, n).with_context(|| format!("G_VTX source {addr:#x}+{n}"))?;
                    for (slot, v) in self.cache[v0..v0 + n].iter_mut().zip(src) {
                        *slot = Some(*v);
                    }
                }
                Gfx::Tri1 { v, .. } => {
                    let tri = v.map(|i| self.cache.get(i as usize).copied().flatten());
                    let [Some(a), Some(b), Some(c)] = tri else {
                        bail!("G_TRI1 {v:?} uses an unloaded cache slot");
                    };
                    self.triangles.push([a, b, c]);
                }
                Gfx::SetGeometryMode(m) => self.geometry_mode |= m,
                Gfx::ClearGeometryMode(m) => self.geometry_mode &= !m,
                Gfx::EndDl => return Ok(()),
                Gfx::Texture { .. }
                | Gfx::SetOtherModeH { .. }
                | Gfx::SetOtherModeL { .. }
                | Gfx::SetCombine { .. }
                | Gfx::SetPrimColor { .. }
                | Gfx::SetEnvColor { .. }
                | Gfx::LoadSync
                | Gfx::PipeSync
                | Gfx::TileSync => {}
                other => bail!(
                    "unsupported command {} ({other:?})",
                    crate::gbi::op::name(other.opcode()).unwrap_or("unknown")
                ),
            }
        }
        bail!("display list without G_ENDDL")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-LOD, one-part model with a single quad.
    fn quad_comm() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&4u16.to_be_bytes()); // vertices
        b.extend_from_slice(&[1, 1, 0, 0]); // lods, mtx, volumes, transparent
        b.extend_from_slice(&0u16.to_be_bytes()); // items
        for (x, y) in [(0i16, 0i16), (10, 0), (10, 10), (0, 10)] {
            for v in [x, y, 0, 0, x * 32, y * 32] {
                b.extend_from_slice(&v.to_be_bytes());
            }
            b.extend_from_slice(&[0xFF, 0x80, 0x00, 0xFF]);
        }
        b.extend_from_slice(&[1, 0]); // parts, billboard
        b.extend_from_slice(&[1, 0, 0]); // states, unk5, depth
        b.extend_from_slice(&(state::ZBUFFER | 5).to_be_bytes());
        b.extend_from_slice(&[0, 4, 0, 2, 0, 3]); // xfm, tri, cmds
        b.extend_from_slice(&[0x00, 0x00, 0x30]); // gSPVertex(vtx[0], 4, 0)
        b.extend_from_slice(&0x4012u16.to_be_bytes()); // tri 0 1 2
        b.extend_from_slice(&0x4023u16.to_be_bytes()); // tri 0 2 3
        b.extend_from_slice(&100.0f32.to_be_bytes()); // lod radius
        for i in 0..16 {
            let v: f32 = if i % 5 == 0 { 1.0 } else { 0.0 };
            b.extend_from_slice(&v.to_be_bytes());
        }
        for f in [1.0f32, 2.0, 0.0] {
            b.extend_from_slice(&f.to_be_bytes());
        }
        b
    }

    #[test]
    fn parses_and_executes() {
        let m = Uvmd::parse_comm(&quad_comm()).unwrap();
        assert_eq!(
            (m.vertices.len(), m.lods.len(), m.matrices.len()),
            (4, 1, 1)
        );
        assert_eq!((m.scale, m.lods[0].radius), (2.0, 100.0));
        let s = &m.lods[0].parts[0].states[0];
        assert_eq!(s.texture(), Some(5));
        assert_eq!(
            s.dlist[0],
            Gfx::Vertex {
                count: 4,
                v0: 0,
                addr: 0
            }
        );
        let tris = m.triangles(s).unwrap();
        assert_eq!(tris.len(), 2);
        assert_eq!(tris[1][2].pos, [0, 10, 0]);
        assert_eq!(tris[0][1].st, [320, 0]);
    }
}
