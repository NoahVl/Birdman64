//! `--models`: convert every UVMD to `<dir>/<typeindex>.glb`, with the
//! textures it uses embedded as PNG, the UVAN joint animations targeting it,
//! and parse/opcode statistics.
//!
//! Layout of each GLB: one scene per LOD (scene 0 = LOD 0). Each scene has a
//! root node that converts the game's Z-up model space to glTF's Y-up and
//! applies `1 / Uvmd::scale` (model units → world units, as `uvDobjPosm`
//! does), then the part hierarchy with the model's own part matrices (part 0
//! uses identity: in-game its matrix is replaced by the object's position).
//! One primitive per render state; material = texture + state flags.
//! Animations (`animations[0]` = the first UVAN for this model) rotate the
//! LOD-0 part nodes: `uvJanimPoseLine` feeds normalized progress over
//! `frame_count()` frames and writes the quaternion into the part matrix, so
//! the exporter converts animated part matrices to TRS and adds a rotation
//! channel per track. glTF has no frame rate; 30 Hz (the game's logic tick)
//! is assumed and recorded in `extras.frame_rate`.

use crate::gltf::{Gltf, Tex, load_texture};
use anyhow::{Context, Result};
use pw64_formats::gbi::op;
use pw64_formats::uvmd::state;
use pw64_formats::{Uvan, Uvmd, Uvtx};
use pw64_rom::Filesystem;
use serde_json::json;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub fn export(fs: &Filesystem, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let uvtx = fs
        .entries
        .iter()
        .filter(|e| e.tag.0 == *b"UVTX")
        .map(|e| Uvtx::parse(&fs.read(e)?).with_context(|| format!("UVTX {}", e.type_index)))
        .collect::<Result<Vec<_>>>()?;
    // UVMD id → the UVANs that animate it (typeindices for `extras`).
    let mut anims = HashMap::<i32, Vec<(usize, Uvan)>>::new();
    let mut stats = BTreeMap::<&str, usize>::new();
    for e in fs.entries.iter().filter(|e| e.tag.0 == *b"UVAN") {
        let a = Uvan::parse(&fs.read(e)?).with_context(|| format!("UVAN {}", e.type_index))?;
        *stats.entry("anims").or_default() += 1;
        *stats.entry("anim tracks").or_default() += a.tracks.len();
        *stats.entry("anim keys").or_default() +=
            a.tracks.iter().map(|t| t.keys.len()).sum::<usize>();
        anims.entry(a.model).or_default().push((e.type_index, a));
    }
    let mut tex_cache = HashMap::<u16, Option<Tex>>::new();

    let mut ops = BTreeMap::<u8, usize>::new();
    let mut n = 0;
    for e in fs.entries.iter().filter(|e| e.tag.0 == *b"UVMD") {
        let ctx = || format!("UVMD {}", e.type_index);
        let m = Uvmd::parse(&fs.read(e)?).with_context(ctx)?;
        for s in m.lods.iter().flat_map(|l| &l.parts).flat_map(|p| &p.states) {
            for g in &s.dlist {
                *ops.entry(g.opcode()).or_default() += 1;
            }
        }
        let model_anims = anims.get(&(e.type_index as i32)).map_or(&[][..], |v| v);
        *stats.entry("models with anims").or_default() += !model_anims.is_empty() as usize;
        collect_stats(&m, &mut stats);
        let glb = build_glb(&m, &uvtx, &mut tex_cache, model_anims).with_context(ctx)?;
        std::fs::write(dir.join(format!("{:03}.glb", e.type_index)), glb)?;
        n += 1;
    }

    println!("converted {n} UVMD to {}", dir.display());
    println!("display-list opcode histogram:");
    for (o, c) in &ops {
        println!("  {o:02X} {:<22} {c:7}", op::name(*o).unwrap_or("UNKNOWN"));
    }
    println!("stats:");
    for (k, v) in &stats {
        println!("  {k:<32} {v}");
    }
    Ok(())
}

fn collect_stats(m: &Uvmd, stats: &mut BTreeMap<&'static str, usize>) {
    let mut add = |k, v| *stats.entry(k).or_default() += v;
    add("lods", m.lods.len());
    add("vertices", m.vertices.len());
    add("volumes", m.volumes.len());
    add("models with >1 lod", (m.lods.len() > 1) as usize);
    add("transparent models", m.transparent as usize);
    add("scale != 1", (m.scale != 1.0) as usize);
    let identity =
        |mx: &[[f32; 4]; 4]| (0..4).all(|i| (0..4).all(|j| mx[i][j] == (i == j) as u8 as f32));
    add(
        "root matrix != identity",
        (!m.matrices.first().is_some_and(identity)) as usize,
    );
    // glTF node matrices must be affine (decomposable to TRS).
    let affine = |mx: &[[f32; 4]; 4]| {
        mx[0][3] == 0.0 && mx[1][3] == 0.0 && mx[2][3] == 0.0 && mx[3][3] == 1.0
    };
    add(
        "non-affine matrices",
        m.matrices.iter().filter(|mx| !affine(mx)).count(),
    );
    add(
        "lods with parts != matrices",
        m.lods
            .iter()
            .filter(|l| l.parts.len() != m.matrices.len())
            .count(),
    );
    for l in &m.lods {
        let d: Vec<u8> = l.parts.iter().map(|p| p.depth).collect();
        let bad = d.first().is_some_and(|&d0| d0 != 0) || d.windows(2).any(|w| w[1] > w[0] + 1);
        add("lods with bad part depths", bad as usize);
        add("billboard lods", l.billboard as usize);
        add("parts", l.parts.len());
        for p in &l.parts {
            for s in &p.states {
                add("states", 1);
                add(
                    "states lit (env-mapped)",
                    (s.state & state::LIGHTING != 0) as usize,
                );
                add("states xlu", (s.state & state::XLU != 0) as usize);
                add("states untextured", s.texture().is_none() as usize);
                add("states DRAW_DL", (s.state & state::DRAW_DL != 0) as usize);
                let tris = s
                    .dlist
                    .iter()
                    .filter(|g| matches!(g, pw64_formats::gbi::Gfx::Tri1 { .. }))
                    .count();
                add("triangles", tris);
                add(
                    "tri_count mismatches",
                    (tris != s.tri_count as usize) as usize,
                );
            }
        }
    }
}

fn build_glb(
    m: &Uvmd,
    uvtx: &[Uvtx],
    cache: &mut HashMap<u16, Option<Tex>>,
    anims: &[(usize, Uvan)],
) -> Result<Vec<u8>> {
    // Decode every texture first so the cache can be borrowed immutably.
    for s in m.lods.iter().flat_map(|l| &l.parts).flat_map(|p| &p.states) {
        if let Some(id) = s.texture()
            && let Entry::Vacant(e) = cache.entry(id)
        {
            e.insert(load_texture(uvtx, id)?);
        }
    }
    let mut g = Gltf::default();
    let mut scenes = Vec::new();
    let mut doc_animations: Vec<serde_json::Value> = Vec::new();
    for (li, lod) in m.lods.iter().enumerate() {
        let mut part_nodes: Vec<usize> = Vec::new();
        for (pi, part) in lod.parts.iter().enumerate() {
            let mut prims = Vec::new();
            for s in &part.states {
                let tex = s
                    .texture()
                    .and_then(|id| cache[&id].as_ref().map(|t| (id, t)));
                if let Some(p) = g.primitive(&m.triangles(s)?, s, tex)? {
                    prims.push(p);
                }
            }
            let mut node = json!({"name": format!("lod{li}_part{pi}"), "children": []});
            if pi > 0
                && let Some(mx) = m.matrices.get(pi)
            {
                // Row-vector Mtx4F in memory order == glTF's column-major array.
                node["matrix"] = json!(mx.iter().flatten().collect::<Vec<_>>());
            }
            if !prims.is_empty() {
                g.meshes
                    .push(json!({"name": format!("lod{li}_part{pi}"), "primitives": prims}));
                node["mesh"] = json!(g.meshes.len() - 1);
            }
            g.nodes.push(node);
            let id = g.nodes.len() - 1;
            if let Some(parent) = m.parent(li, pi) {
                let pid: usize = part_nodes[parent];
                g.nodes[pid]["children"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(id));
            }
            part_nodes.push(id);
        }
        let roots: Vec<usize> = (0..lod.parts.len())
            .filter(|&pi| m.parent(li, pi).is_none())
            .map(|pi| part_nodes[pi])
            .collect();
        let s = 1.0 / m.scale;
        // Rotate -90° about X: game Z-up → glTF Y-up.
        let h = std::f32::consts::FRAC_1_SQRT_2;
        g.nodes.push(json!({
            "name": format!("lod{li}"),
            "rotation": [-h, 0.0, 0.0, h],
            "scale": [s, s, s],
            "children": roots,
            "extras": {"billboard": lod.billboard, "lod_radius": lod.radius},
        }));
        scenes.push(json!({"name": format!("lod{li}"), "nodes": [g.nodes.len() - 1]}));
        // The game animates LOD 0 (`uvJanimPoseLine` poses the model as drawn).
        if li == 0 {
            doc_animations = animations(anims, &mut g, &part_nodes);
        }
    }

    let mut doc = json!({
        "asset": {"version": "2.0", "generator": "pw64-extract"},
        "scene": 0,
        "scenes": scenes,
        "extras": {"transparent": m.transparent, "scale": m.scale, "unk1c": m.unk1c, "unk24": m.unk24},
    });
    if !doc_animations.is_empty() {
        doc["animations"] = json!(doc_animations);
    }
    g.finish(doc)
}

/// Animation rate assumed for the glTF export (seconds = frame / this).
const ANIM_FPS: f32 = 30.0;

/// Builds one glTF animation per UVAN targeting this model: a rotation
/// channel per track on its LOD-0 part node. `uvJanimPoseLine` treats the
/// key `frame` as a position in frames (`frame_count()` = the animated
/// length) and writes the rotation into the part matrix — so animated part
/// nodes are converted from `matrix` to TRS and the quaternion (conjugated,
/// `uvMat4SetQuaternionRotation` is row-vector) overrides the TRS rotation.
fn animations(
    anims: &[(usize, Uvan)],
    g: &mut Gltf,
    part_nodes: &[usize],
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if anims.is_empty() {
        return out;
    }
    // TRS-convert every node any track targets (once per part).
    let mut converted = HashSet::new();
    for (_, a) in anims {
        for t in &a.tracks {
            let pi = t.part as usize;
            if let Some(&n) = part_nodes.get(pi)
                && converted.insert(pi)
            {
                let mx = g.nodes[n]
                    .get("matrix")
                    .and_then(|v| v.as_array())
                    .and_then(|a| {
                        a.iter()
                            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                            .collect::<Vec<f32>>()
                            .try_into()
                            .ok()
                    });
                if let Some(mx) = mx {
                    let (tr, rot, sc) = matrix_to_trs(&mx);
                    let node = &mut g.nodes[n];
                    node.as_object_mut().unwrap().remove("matrix");
                    node["translation"] = json!(tr);
                    node["rotation"] = json!(rot);
                    node["scale"] = json!(sc);
                }
            }
        }
    }
    for (idx, a) in anims {
        let mut samplers = Vec::new();
        let mut channels = Vec::new();
        for t in &a.tracks {
            let Some(&node) = part_nodes.get(t.part as usize) else {
                continue;
            };
            let mut times = Vec::with_capacity(t.keys.len());
            let mut quats = Vec::with_capacity(t.keys.len() * 4);
            for k in &t.keys {
                times.push((k.frame as i32 - a.first_frame) as f32 / ANIM_FPS);
                // Row-vector → column-vector: conjugate.
                quats.extend([-k.quat[0], -k.quat[1], -k.quat[2], k.quat[3]]);
            }
            let input = g.accessor(
                &times
                    .iter()
                    .flat_map(|f| f.to_le_bytes())
                    .collect::<Vec<u8>>(),
                34962,
                json!({
                    "componentType": 5126, "count": times.len(),
                    "type": "SCALAR", "min": times.first(), "max": times.last(),
                }),
            );
            let output = g.accessor(
                &quats
                    .iter()
                    .flat_map(|f| f.to_le_bytes())
                    .collect::<Vec<u8>>(),
                34962,
                json!({"componentType": 5126, "count": t.keys.len(), "type": "VEC4"}),
            );
            samplers.push(json!({"input": input, "output": output}));
            channels.push(json!({
                "sampler": samplers.len() - 1,
                "target": {"node": node, "path": "rotation"},
            }));
        }
        if channels.is_empty() {
            continue;
        }
        out.push(json!({
            "name": format!("anim{idx}"),
            "channels": channels,
            "samplers": samplers,
            "extras": {
                "first_frame": a.first_frame,
                "last_frame": a.last_frame,
                "step": a.step,
                "frame_count": a.frame_count(),
                "frame_rate": ANIM_FPS,
            },
        }));
    }
    out
}

/// Decomposes a part matrix (the glTF `matrix` array, column-major:
/// `M[r][c] = mx[c * 4 + r]`) into glTF TRS. Part matrices are rotations
/// with translation, so no shear is expected.
fn matrix_to_trs(mx: &[f32; 16]) -> ([f32; 3], [f32; 4], [f32; 3]) {
    let at = |r: usize, c: usize| mx[c * 4 + r];
    let len = |v: [f32; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    // Basis columns of M.
    let scale = [
        len([at(0, 0), at(1, 0), at(2, 0)]),
        len([at(0, 1), at(1, 1), at(2, 1)]),
        len([at(0, 2), at(1, 2), at(2, 2)]),
    ];
    if scale.iter().any(|&s| s < 1e-9) {
        // Degenerate: glTF default TRS.
        return ([0.0; 3], [0.0, 0.0, 0.0, 1.0], [1.0; 3]);
    }
    let r = |row: usize, col: usize| at(row, col) / scale[col];
    let r00 = r(0, 0);
    let r11 = r(1, 1);
    let r22 = r(2, 2);
    let quat = if r00 > r11 && r00 > r22 {
        let s = (1.0 + r00 - r11 - r22).sqrt() * 2.0;
        [
            0.25 * s,
            (r(0, 1) + r(1, 0)) / s,
            (r(0, 2) + r(2, 0)) / s,
            (r(2, 1) - r(1, 2)) / s,
        ]
    } else if r11 > r22 {
        let s = (1.0 + r11 - r00 - r22).sqrt() * 2.0;
        [
            (r(0, 1) + r(1, 0)) / s,
            0.25 * s,
            (r(1, 2) + r(2, 1)) / s,
            (r(0, 2) - r(2, 0)) / s,
        ]
    } else {
        let s = (1.0 + r22 - r00 - r11).sqrt() * 2.0;
        [
            (r(0, 2) + r(2, 0)) / s,
            (r(1, 2) + r(2, 1)) / s,
            0.25 * s,
            (r(1, 0) - r(0, 1)) / s,
        ]
    };
    let t = [at(3, 0), at(3, 1), at(3, 2)];
    (t, quat, scale)
}
