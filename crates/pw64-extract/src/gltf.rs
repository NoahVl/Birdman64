//! Shared glTF/GLB building for `--models` and `--terrain`: texture
//! decoding for model use, materials from render states, one primitive per
//! state, GLB packing.

use anyhow::Result;
use pw64_formats::uvmd::{State, TEXTURE_NONE, Vtx, state};
use pw64_formats::uvtx::{NO_TEXTURE, tile_uv};
use pw64_formats::{Image, Uvtx};
use serde_json::{Value, json};
use std::collections::HashMap;

/// What the exporter needs from a texture.
pub struct Tex {
    pub png: Vec<u8>,
    pub tile: pw64_formats::tmem::Tile,
    pub scale: [u16; 2],
    pub wrap: [u8; 2],
    /// Mean colour (alpha-weighted), for previews.
    pub avg: [u8; 4],
}

/// Decodes texture `id` for model use (None if the id is out of range).
pub fn load_texture(uvtx: &[Uvtx], id: u16) -> Result<Option<Tex>> {
    let Some(t) = uvtx.get(id as usize) else {
        return Ok(None);
    };
    let image2 = match t.image2 {
        NO_TEXTURE => None,
        i => uvtx.get(i as usize).map(|t| &t.image[..]),
    };
    // Use the tile the model's UVs address: gSPTexture's (TEXEL0).
    let dec = t.decode(image2)?;
    let r = dec.render();
    Ok(Some(Tex {
        png: encode_png(&r.image)?,
        tile: r.tile,
        scale: t.texture_scale(),
        wrap: [t.wrap_s, t.wrap_t],
        avg: average(&r.image),
    }))
}

fn average(img: &Image) -> [u8; 4] {
    let (mut sum, mut wsum, mut asum) = ([0f64; 3], 0f64, 0f64);
    for p in img.rgba.as_chunks::<4>().0 {
        let a = p[3] as f64 / 255.0;
        for i in 0..3 {
            sum[i] += p[i] as f64 * a;
        }
        wsum += a;
        asum += p[3] as f64;
    }
    let n = (img.rgba.len() / 4).max(1) as f64;
    let c = |i: usize| (sum[i] / wsum.max(1e-9)) as u8;
    [c(0), c(1), c(2), (asum / n) as u8]
}

pub fn encode_png(img: &Image) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, img.width, img.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&img.rgba)?;
    Ok(out)
}

/// Accumulates the glTF JSON arrays and the binary chunk.
#[derive(Default)]
pub struct Gltf {
    pub bin: Vec<u8>,
    pub views: Vec<Value>,
    pub accessors: Vec<Value>,
    pub meshes: Vec<Value>,
    pub nodes: Vec<Value>,
    pub materials: Vec<Value>,
    pub textures: Vec<Value>,
    pub images: Vec<Value>,
    pub samplers: Vec<Value>,
    pub material_ids: HashMap<(u16, u32), usize>,
    pub texture_ids: HashMap<u16, usize>,
}

impl Gltf {
    pub fn view(&mut self, data: &[u8], target: Option<u32>) -> usize {
        while !self.bin.len().is_multiple_of(4) {
            self.bin.push(0);
        }
        let mut v = json!({"buffer": 0, "byteOffset": self.bin.len(), "byteLength": data.len()});
        if let Some(t) = target {
            v["target"] = json!(t);
        }
        self.bin.extend_from_slice(data);
        self.views.push(v);
        self.views.len() - 1
    }

    pub fn accessor(&mut self, data: &[u8], target: u32, extra: Value) -> usize {
        let view = self.view(data, Some(target));
        let mut a = json!({"bufferView": view});
        a.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        self.accessors.push(a);
        self.accessors.len() - 1
    }

    pub fn f32s(&mut self, v: &[[f32; 3]]) -> usize {
        let (mut min, mut max) = ([f32::MAX; 3], [f32::MIN; 3]);
        let mut bytes = Vec::with_capacity(v.len() * 12);
        for p in v {
            for i in 0..3 {
                min[i] = min[i].min(p[i]);
                max[i] = max[i].max(p[i]);
                bytes.extend_from_slice(&p[i].to_le_bytes());
            }
        }
        self.accessor(
            &bytes,
            34962,
            json!({"componentType": 5126, "count": v.len(), "type": "VEC3", "min": min, "max": max}),
        )
    }

    pub fn texture(&mut self, id: u16, tex: &Tex) -> usize {
        if let Some(&i) = self.texture_ids.get(&id) {
            return i;
        }
        let view = self.view(&tex.png, None);
        self.images.push(
            json!({"bufferView": view, "mimeType": "image/png", "name": format!("uvtx_{id:03}")}),
        );
        // UVTX wrap: 0 clamp, 1 wrap, 2 mirror.
        let wrap = |w: u8| match w {
            0 => 33071,
            2 => 33648,
            _ => 10497,
        };
        self.samplers.push(json!({
            "magFilter": 9729, "minFilter": 9729,
            "wrapS": wrap(tex.wrap[0]), "wrapT": wrap(tex.wrap[1]),
        }));
        let n = self.images.len() - 1;
        self.textures.push(json!({"source": n, "sampler": n}));
        self.texture_ids.insert(id, n);
        n
    }

    pub fn material(&mut self, s: &State, tex: Option<(u16, &Tex)>) -> usize {
        let flags = s.state & (state::XLU | state::CULL_BACK | state::LIGHTING | state::DECAL);
        let key = (tex.map_or(TEXTURE_NONE, |t| t.0), flags);
        if let Some(&i) = self.material_ids.get(&key) {
            return i;
        }
        let mut pbr = json!({"metallicFactor": 0.0, "roughnessFactor": 1.0});
        if let Some((id, t)) = tex {
            pbr["baseColorTexture"] = json!({"index": self.texture(id, t)});
        }
        let mut m = json!({
            "name": format!("tex{:03x}_state{:08x}", key.0, flags),
            "pbrMetallicRoughness": pbr,
            "doubleSided": flags & state::CULL_BACK == 0,
            "extras": {"state": s.state},
        });
        if flags & state::XLU != 0 {
            m["alphaMode"] = json!("BLEND");
        } else if tex.is_some() {
            // Opaque render modes still discard low-coverage texels.
            m["alphaMode"] = json!("MASK");
        }
        if flags & state::LIGHTING == 0 {
            m["extensions"] = json!({"KHR_materials_unlit": {}});
        }
        self.materials.push(m);
        self.material_ids.insert(key, self.materials.len() - 1);
        self.materials.len() - 1
    }

    /// One primitive for a render state, or None if it draws nothing.
    pub fn primitive(
        &mut self,
        tris: &[[Vtx; 3]],
        s: &State,
        tex: Option<(u16, &Tex)>,
    ) -> Result<Option<Value>> {
        if tris.is_empty() {
            return Ok(None);
        }
        let lit = s.state & state::LIGHTING != 0;
        let mut index_of = HashMap::<Vtx, u32>::new();
        let (mut pos, mut nrm, mut uv, mut col, mut idx) = (vec![], vec![], vec![], vec![], vec![]);
        for v in tris.iter().flatten() {
            let i = *index_of.entry(*v).or_insert_with(|| {
                pos.push(v.pos.map(|c| c as f32));
                let n = v.normal();
                if lit {
                    nrm.push(n);
                    col.extend_from_slice(&[255, 255, 255, v.color[3]]);
                } else {
                    col.extend_from_slice(&v.color);
                }
                uv.push(match tex {
                    // G_TEXTURE_GEN: UVs come from the view-space normal at
                    // runtime; approximate with the model-space normal.
                    _ if lit => [n[0] * 0.5 + 0.5, n[1] * 0.5 + 0.5],
                    Some((_, t)) => tile_uv(&t.tile, t.scale, v.st),
                    None => [0.0, 0.0],
                });
                pos.len() as u32 - 1
            });
            idx.push(i);
        }
        let mut attrs = json!({"POSITION": self.f32s(&pos)});
        let colors = self.accessor(
            &col,
            34962,
            json!({"componentType": 5121, "normalized": true, "count": pos.len(), "type": "VEC4"}),
        );
        attrs["COLOR_0"] = json!(colors);
        if lit {
            attrs["NORMAL"] = json!(self.f32s(&nrm));
        }
        if tex.is_some() {
            let bytes: Vec<u8> = uv.iter().flatten().flat_map(|f| f.to_le_bytes()).collect();
            attrs["TEXCOORD_0"] = json!(self.accessor(
                &bytes,
                34962,
                json!({"componentType": 5126, "count": uv.len(), "type": "VEC2"}),
            ));
        }
        let (bytes, ctype): (Vec<u8>, u32) = if pos.len() <= u16::MAX as usize {
            (
                idx.iter().flat_map(|&i| (i as u16).to_le_bytes()).collect(),
                5123,
            )
        } else {
            (idx.iter().flat_map(|i| i.to_le_bytes()).collect(), 5125)
        };
        let indices = self.accessor(
            &bytes,
            34963,
            json!({"componentType": ctype, "count": idx.len(), "type": "SCALAR"}),
        );
        Ok(Some(json!({
            "attributes": attrs,
            "indices": indices,
            "material": self.material(s, tex),
        })))
    }
}

impl Gltf {
    /// Adds the collected arrays and the buffer to `doc` and packs a GLB.
    pub fn finish(mut self, mut doc: Value) -> Result<Vec<u8>> {
        for n in &mut self.nodes {
            if n["children"].as_array().is_some_and(|c| c.is_empty()) {
                n.as_object_mut().unwrap().remove("children");
            }
        }
        let obj = doc.as_object_mut().unwrap();
        for (k, v) in [
            ("nodes", self.nodes),
            ("meshes", self.meshes),
            ("materials", self.materials),
            ("textures", self.textures),
            ("images", self.images),
            ("samplers", self.samplers),
            ("accessors", self.accessors),
            ("bufferViews", self.views),
        ] {
            if !v.is_empty() {
                obj.insert(k.into(), Value::Array(v));
            }
        }
        if obj.contains_key("materials") {
            obj.insert("extensionsUsed".into(), json!(["KHR_materials_unlit"]));
        }
        while !self.bin.len().is_multiple_of(4) {
            self.bin.push(0);
        }
        if !self.bin.is_empty() {
            obj.insert("buffers".into(), json!([{"byteLength": self.bin.len()}]));
        }
        Ok(glb(&serde_json::to_vec(&doc)?, &self.bin))
    }
}

/// Packs a GLB container (JSON chunk padded with spaces, BIN chunk).
pub fn glb(json: &[u8], bin: &[u8]) -> Vec<u8> {
    let mut j = json.to_vec();
    while !j.len().is_multiple_of(4) {
        j.push(b' ');
    }
    let bin_len = if bin.is_empty() { 0 } else { 8 + bin.len() };
    let total = 12 + 8 + j.len() + bin_len;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(j.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&j);
    if !bin.is_empty() {
        out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(bin);
    }
    out
}
