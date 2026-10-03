//! `--terrain`: every terra of the UVTR file → `<dir>/<terra>.glb` (UVCT
//! tiles at their grid cells, placed UVMD models instanced per part,
//! textures embedded) plus a top-down preview `<dir>/<terra>_top.png`, and
//! parse statistics for all UVCT/UVTR data.
//!
//! GLB layout: root node (Z-up → Y-up) → one node per cell with the cell's
//! `Mtx4F` → the tile mesh (one primitive per draw state) and one node tree
//! per placement, built from the placement's own per-part matrices (as
//! `uvSobj_8022C8D0` pushes them) over the model's LOD 0 part meshes.
//! Meshes are shared between cells/placements that use the same data.

use crate::gltf::{Gltf, Tex, encode_png, load_texture};
use anyhow::{Context, Result};
use pw64_formats::gbi::op;
use pw64_formats::uvct::Uvct;
use pw64_formats::uvmd::{State, Vtx};
use pw64_formats::{Image, Terra, Uvmd, Uvtx, uvtr};
use pw64_rom::Filesystem;
use serde_json::{Value, json};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

type Mat = [[f32; 4]; 4];

/// Row-vector product: apply `a`, then `b`.
fn mul(a: &Mat, b: &Mat) -> Mat {
    let mut m = [[0.0; 4]; 4];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..4).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

fn xform(p: [i16; 3], m: &Mat) -> [f32; 3] {
    let v = [p[0] as f32, p[1] as f32, p[2] as f32, 1.0];
    [0, 1, 2].map(|j| (0..4).map(|k| v[k] * m[k][j]).sum())
}

fn tag_entries<'a>(
    fs: &'a Filesystem,
    tag: &'a [u8; 4],
) -> impl Iterator<Item = &'a pw64_rom::FileEntry> {
    fs.entries.iter().filter(move |e| e.tag.0 == *tag)
}

struct Assets {
    uvtx: Vec<Uvtx>,
    models: Vec<Uvmd>,
    contours: Vec<Uvct>,
    tex: HashMap<u16, Option<Tex>>,
}

impl Assets {
    fn texture(&mut self, s: &State) -> Result<Option<u16>> {
        let Some(id) = s.texture() else {
            return Ok(None);
        };
        if let Entry::Vacant(e) = self.tex.entry(id) {
            e.insert(load_texture(&self.uvtx, id)?);
        }
        Ok(self.tex[&id].as_ref().map(|_| id))
    }
}

pub fn export(fs: &Filesystem, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let parse_all = |tag: &'static [u8; 4]| {
        tag_entries(fs, tag).map(move |e| {
            let f = fs.read(e)?;
            Ok::<_, anyhow::Error>((e.type_index, f))
        })
    };
    let uvtx = parse_all(b"UVTX")
        .map(|r| r.and_then(|(i, f)| Uvtx::parse(&f).with_context(|| format!("UVTX {i}"))))
        .collect::<Result<Vec<_>>>()?;
    let models = parse_all(b"UVMD")
        .map(|r| r.and_then(|(i, f)| Uvmd::parse(&f).with_context(|| format!("UVMD {i}"))))
        .collect::<Result<Vec<_>>>()?;
    let contours = parse_all(b"UVCT")
        .map(|r| r.and_then(|(i, f)| Uvct::parse(&f).with_context(|| format!("UVCT {i}"))))
        .collect::<Result<Vec<_>>>()?;
    let uvtr_entry = tag_entries(fs, b"UVTR").next().context("no UVTR file")?;
    let terras = uvtr::parse(&fs.read(uvtr_entry)?)?;
    let mut a = Assets {
        uvtx,
        models,
        contours,
        tex: HashMap::new(),
    };

    print_stats(&a, &terras);

    for (ti, t) in terras.iter().enumerate() {
        let glb = build_glb(&mut a, t, ti).with_context(|| format!("terra {ti}"))?;
        std::fs::write(dir.join(format!("{ti:02}.glb")), glb)?;
        let img = top_down(&mut a, t)?;
        std::fs::write(dir.join(format!("{ti:02}_top.png")), encode_png(&img)?)?;
    }
    println!("wrote {} terras to {}", terras.len(), dir.display());
    Ok(())
}

fn print_stats(a: &Assets, terras: &[Terra]) {
    let mut ops = BTreeMap::<u8, usize>::new();
    let mut st = BTreeMap::<&str, usize>::new();
    let mut add = |k, v| *st.entry(k).or_default() += v;
    for c in &a.contours {
        add("uvct files", 1);
        add("vertices", c.vertices.len());
        add("collision tris", c.collision.len());
        add("draw states", c.states.len());
        add("placements", c.placements.len());
        // Collision ranges of consecutive states should tile the table.
        let mut next = 0;
        let mut contiguous = true;
        for s in &c.states {
            contiguous &= s.collision.start == next;
            next = s.collision.end;
            let tris = s
                .state
                .dlist
                .iter()
                .filter(|g| matches!(g, pw64_formats::gbi::Gfx::Tri1 { .. }))
                .count();
            add("render tris", tris);
            add(
                "tri_count mismatches",
                (tris != s.state.tri_count as usize) as usize,
            );
            add("untextured states", s.state.texture().is_none() as usize);
            for g in &s.state.dlist {
                *ops.entry(g.opcode()).or_default() += 1;
            }
            if let Err(e) = c.triangles(&s.state) {
                eprintln!("executor error: {e}");
                add("executor errors", 1);
            }
            if let Some(id) = s.state.texture()
                && id as usize >= a.uvtx.len()
            {
                add("texture ids out of range", 1);
            }
        }
        add(
            "collision tables not tiled by states",
            (!contiguous || next != c.collision.len()) as usize,
        );
        for p in &c.placements {
            if p.model == 0xFFFF {
                add("placements with model 0xFFFF", 1);
                continue;
            }
            let Some(m) = a.models.get(p.model as usize) else {
                add("placement model out of range", 1);
                continue;
            };
            add(
                "placement mtx count != model parts",
                (p.matrices.len() != m.matrices.len()) as usize,
            );
            let s0 = (0..3)
                .map(|i| p.matrices[0][i][..3].iter().map(|v| v * v).sum::<f32>())
                .sum::<f32>()
                .sqrt()
                / 3f32.sqrt();
            // Uniform scale of part 0 vs the model's 1/scale.
            add(
                "placement root scale != 1/model scale (±2%)",
                ((s0 * m.scale - 1.0).abs() > 0.02) as usize,
            );
            let affine = |mx: &Mat| mx[0][3] == 0.0 && mx[1][3] == 0.0 && mx[2][3] == 0.0;
            add(
                "placement non-affine matrices",
                p.matrices.iter().filter(|m| !affine(m)).count(),
            );
            let t = p.matrices[0][3];
            let d =
                ((t[0] - p.pos[0]).powi(2) + (t[1] - p.pos[1]).powi(2) + (t[2] - p.pos[2]).powi(2))
                    .sqrt();
            add("placement pos != root translation (>1)", (d > 1.0) as usize);
        }
    }
    let mut used = BTreeSet::new();
    for t in terras {
        add("terras", 1);
        for c in t.cells.iter().flatten() {
            add("cells", 1);
            used.insert(c.contour);
            add(
                "cell contour out of range",
                (c.contour as usize >= a.contours.len()) as usize,
            );
            add("cells rotated", (c.rotation != 0) as usize);
        }
        add(
            "empty cells",
            t.cells.iter().filter(|c| c.is_none()).count(),
        );
    }
    add("uvct referenced by no terra", a.contours.len() - used.len());
    println!("UVCT display-list opcode histogram:");
    for (o, c) in &ops {
        println!("  {o:02X} {:<22} {c:7}", op::name(*o).unwrap_or("UNKNOWN"));
    }
    println!("terrain stats:");
    for (k, v) in &st {
        println!("  {k:<44} {v}");
    }
    println!("terras:");
    for (i, t) in terras.iter().enumerate() {
        let n = t.cells.iter().flatten().count();
        let ids: BTreeSet<u16> = t.cells.iter().flatten().map(|c| c.contour).collect();
        println!(
            "  {i:2}: {}x{} cells of {}x{}, {n} used, box {:?}..{:?}, uvct {:?}..={:?}",
            t.cols,
            t.rows,
            t.cell_size[0],
            t.cell_size[1],
            t.min,
            t.max,
            ids.first(),
            ids.last()
        );
    }
}

/// One mesh with a primitive per state (None if nothing is drawn).
fn mesh(
    g: &mut Gltf,
    a: &mut Assets,
    name: String,
    states: &[(Vec<[Vtx; 3]>, &State)],
) -> Result<Option<usize>> {
    let mut prims = Vec::new();
    for (tris, s) in states {
        let id = a.texture(s)?;
        let tex = id.map(|id| (id, a.tex[&id].as_ref().unwrap()));
        if let Some(p) = g.primitive(tris, s, tex)? {
            prims.push(p);
        }
    }
    if prims.is_empty() {
        return Ok(None);
    }
    g.meshes.push(json!({"name": name, "primitives": prims}));
    Ok(Some(g.meshes.len() - 1))
}

fn node(g: &mut Gltf, v: Value) -> usize {
    g.nodes.push(v);
    g.nodes.len() - 1
}

fn add_child(g: &mut Gltf, parent: usize, child: usize) {
    g.nodes[parent]["children"]
        .as_array_mut()
        .unwrap()
        .push(json!(child));
}

fn flat(m: &Mat) -> Vec<f32> {
    // Row-vector Mtx4F in memory order == glTF's column-major array.
    m.iter().flatten().copied().collect()
}

fn build_glb(a: &mut Assets, t: &Terra, ti: usize) -> Result<Vec<u8>> {
    let mut g = Gltf::default();
    let mut contour_mesh = HashMap::<u16, Option<usize>>::new();
    let mut part_mesh = HashMap::<(u16, usize), Option<usize>>::new();
    let mut cells = Vec::new();
    for row in 0..t.rows as usize {
        for col in 0..t.cols as usize {
            let Some(cell) = t.cell(col, row) else {
                continue;
            };
            let ci = cell.contour;
            let cell_node = node(
                &mut g,
                json!({
                    "name": format!("cell_{col}_{row}_uvct{ci:03}"),
                    "matrix": flat(&cell.matrix),
                    "children": [],
                    "extras": {"uvct": ci, "rotation": cell.rotation},
                }),
            );
            cells.push(cell_node);
            let Some(c) = a.contours.get(ci as usize).cloned() else {
                continue;
            };
            let mesh_id = match contour_mesh.get(&ci) {
                Some(&m) => m,
                None => {
                    let states = c
                        .states
                        .iter()
                        .map(|s| Ok((c.triangles(&s.state)?, &s.state)))
                        .collect::<Result<Vec<_>>>()?;
                    let m = mesh(&mut g, a, format!("uvct{ci:03}"), &states)?;
                    contour_mesh.insert(ci, m);
                    m
                }
            };
            if let Some(m) = mesh_id {
                let n = node(&mut g, json!({"name": format!("uvct{ci:03}"), "mesh": m}));
                add_child(&mut g, cell_node, n);
            }
            for (pi, p) in c.placements.iter().enumerate() {
                let Some(model) = a.models.get(p.model as usize).cloned() else {
                    continue;
                };
                let lod = &model.lods[0];
                let mut nodes: Vec<usize> = Vec::new();
                for (part, pt) in lod.parts.iter().enumerate() {
                    let mesh_id = match part_mesh.get(&(p.model, part)) {
                        Some(&m) => m,
                        None => {
                            let states = pt
                                .states
                                .iter()
                                .map(|s| Ok((model.triangles(s)?, s)))
                                .collect::<Result<Vec<_>>>()?;
                            let m =
                                mesh(&mut g, a, format!("uvmd{:03}_part{part}", p.model), &states)?;
                            part_mesh.insert((p.model, part), m);
                            m
                        }
                    };
                    let mut v = json!({
                        "name": format!("uvmd{:03}_part{part}", p.model),
                        "children": [],
                    });
                    if let Some(mx) = p.matrices.get(part) {
                        v["matrix"] = json!(flat(mx));
                    }
                    if let Some(m) = mesh_id {
                        v["mesh"] = json!(m);
                    }
                    if part == 0 {
                        v["name"] = json!(format!("place{pi}_uvmd{:03}", p.model));
                        v["extras"] =
                            json!({"uvmd": p.model, "sectors": p.sectors, "unk16": p.unk16});
                    }
                    let id = node(&mut g, v);
                    match model.parent(0, part) {
                        Some(par) => add_child(&mut g, nodes[par], id),
                        None => add_child(&mut g, cell_node, id),
                    }
                    nodes.push(id);
                }
            }
        }
    }
    let h = std::f32::consts::FRAC_1_SQRT_2;
    let root = node(
        &mut g,
        json!({
            "name": format!("terra{ti:02}"),
            "rotation": [-h, 0.0, 0.0, h],
            "children": cells,
            "extras": {
                "min": t.min, "max": t.max, "cols": t.cols, "rows": t.rows,
                "cell_size": t.cell_size, "unk24": t.unk24,
            },
        }),
    );
    let doc = json!({
        "asset": {"version": "2.0", "generator": "pw64-extract"},
        "scene": 0,
        "scenes": [{"name": format!("terra{ti:02}"), "nodes": [root]}],
    });
    g.finish(doc)
}

/// Top-down orthographic preview: z-buffered (highest wins), colour =
/// mean texture colour × vertex colour × a little directional shading.
fn top_down(a: &mut Assets, t: &Terra) -> Result<Image> {
    let ext = [t.max[0] - t.min[0], t.max[1] - t.min[1]];
    let px = (ext[0].max(ext[1]) / 1024.0).max(0.5);
    let (w, h) = ((ext[0] / px).ceil() as usize, (ext[1] / px).ceil() as usize);
    let mut rgba = vec![0u8; w * h * 4];
    let mut zbuf = vec![f32::MIN; w * h];
    let mut tris: Vec<([[f32; 3]; 3], [f32; 4])> = Vec::new();
    let mut add = |a: &mut Assets, m: &Mat, s: &State, list: Vec<[Vtx; 3]>| -> Result<()> {
        let tex = a.texture(s)?.map(|id| a.tex[&id].as_ref().unwrap().avg);
        for tri in list {
            let p = tri.map(|v| xform(v.pos, m));
            let mut c = [0.0; 4];
            for v in &tri {
                for (i, ci) in c.iter_mut().enumerate() {
                    *ci += v.color[i] as f32 / 255.0 / 3.0;
                }
            }
            if let Some(tc) = tex {
                for (i, ci) in c.iter_mut().enumerate() {
                    *ci *= tc[i] as f32 / 255.0;
                }
            }
            tris.push((p, c));
        }
        Ok(())
    };
    for cell in t.cells.iter().flatten() {
        let Some(c) = a.contours.get(cell.contour as usize).cloned() else {
            continue;
        };
        for s in &c.states {
            add(a, &cell.matrix, &s.state, c.triangles(&s.state)?)?;
        }
        for p in &c.placements {
            let Some(model) = a.models.get(p.model as usize).cloned() else {
                continue;
            };
            let mut world: Vec<Mat> = Vec::new();
            for (part, pt) in model.lods[0].parts.iter().enumerate() {
                let parent = match model.parent(0, part) {
                    Some(par) => world[par],
                    None => cell.matrix,
                };
                let local = p
                    .matrices
                    .get(part)
                    .copied()
                    .unwrap_or(model.matrices[part]);
                let m = mul(&local, &parent);
                for s in &pt.states {
                    add(a, &m, s, model.triangles(s)?)?;
                }
                world.push(m);
            }
        }
    }
    for (p, c) in tris {
        if c[3] < 0.3 {
            continue;
        }
        let e1 = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
        let e2 = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
        let n = [
            e1[1] * e2[2] - e1[2] * e2[1],
            e1[2] * e2[0] - e1[0] * e2[2],
            e1[0] * e2[1] - e1[1] * e2[0],
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-9);
        let shade = 0.55 + 0.45 * ((n[0] * -0.4 + n[1] * 0.4 + n[2] * 0.82) / len).abs();
        let s = p.map(|q| [(q[0] - t.min[0]) / px, (t.max[1] - q[1]) / px, q[2]]);
        let (x0, x1) = (
            s.iter()
                .map(|q| q[0])
                .fold(f32::MAX, f32::min)
                .floor()
                .max(0.0) as usize,
            s.iter()
                .map(|q| q[0])
                .fold(f32::MIN, f32::max)
                .ceil()
                .min(w as f32) as usize,
        );
        let (y0, y1) = (
            s.iter()
                .map(|q| q[1])
                .fold(f32::MAX, f32::min)
                .floor()
                .max(0.0) as usize,
            s.iter()
                .map(|q| q[1])
                .fold(f32::MIN, f32::max)
                .ceil()
                .min(h as f32) as usize,
        );
        let area =
            (s[1][0] - s[0][0]) * (s[2][1] - s[0][1]) - (s[2][0] - s[0][0]) * (s[1][1] - s[0][1]);
        if area.abs() < 1e-6 {
            continue;
        }
        for y in y0..y1 {
            for x in x0..x1 {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let edge = |a: [f32; 3], b: [f32; 3]| {
                    (b[0] - a[0]) * (fy - a[1]) - (b[1] - a[1]) * (fx - a[0])
                };
                let w0 = edge(s[1], s[2]) / area;
                let w1 = edge(s[2], s[0]) / area;
                let w2 = edge(s[0], s[1]) / area;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let z = w0 * s[0][2] + w1 * s[1][2] + w2 * s[2][2];
                let i = y * w + x;
                if z <= zbuf[i] {
                    continue;
                }
                zbuf[i] = z;
                for k in 0..3 {
                    rgba[i * 4 + k] = (c[k] * shade * 255.0).clamp(0.0, 255.0) as u8;
                }
                rgba[i * 4 + 3] = 255;
            }
        }
    }
    Ok(Image {
        width: w as u32,
        height: h as u32,
        rgba,
    })
}
