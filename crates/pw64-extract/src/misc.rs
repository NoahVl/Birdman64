//! Lighter formats:
//! - `--blits`: every UVBT → `<dir>/blits/<n>.png`.
//! - `--fonts`: every UVFT → `<dir>/fonts/<n>_<sheet>.png` + `<n>.json`
//!   (character → sheet rectangle).
//! - `--anims`: every UVAN → `<dir>/anims/<n>.json` (tracks of rotation
//!   keys per model part), checked against the target UVMD's part count.

use crate::gltf::encode_png;
use anyhow::{Context, Result, ensure};
use pw64_formats::{Uvan, Uvbt, Uvft, Uvmd};
use pw64_rom::Filesystem;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

fn entries<'a>(
    fs: &'a Filesystem,
    tag: &'a [u8; 4],
) -> impl Iterator<Item = &'a pw64_rom::FileEntry> {
    fs.entries.iter().filter(move |e| e.tag.0 == *tag)
}

pub fn blits(fs: &Filesystem, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut hist = BTreeMap::<String, usize>::new();
    let mut n = 0;
    for e in entries(fs, b"UVBT") {
        let ctx = || format!("UVBT {}", e.type_index);
        let b = Uvbt::parse(&fs.read(e)?).with_context(ctx)?;
        *hist
            .entry(format!(
                "{:?}{} tile {}x{} ({} tiles)",
                b.im_fmt(),
                b.depth,
                b.tile_width,
                b.tile_height,
                b.tiles.len()
            ))
            .or_default() += 1;
        let img = b.decode().with_context(ctx)?;
        std::fs::write(
            dir.join(format!("{:03}.png", e.type_index)),
            encode_png(&img)?,
        )?;
        n += 1;
    }
    println!("decoded {n} UVBT to {}", dir.display());
    for (k, v) in &hist {
        println!("  {k:<40} {v}");
    }
    Ok(())
}

pub fn fonts(fs: &Filesystem, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    for e in entries(fs, b"UVFT") {
        let ctx = || format!("UVFT {}", e.type_index);
        let f = Uvft::parse(&fs.read(e)?).with_context(ctx)?;
        ensure!(
            f.glyphs.len() == f.chars.len(),
            "UVFT {}: {} glyphs for {} chars",
            e.type_index,
            f.glyphs.len(),
            f.chars.len()
        );
        for i in 0..f.images.len() {
            let img = f.decode_image(i).with_context(ctx)?;
            std::fs::write(
                dir.join(format!("{:02}_{i}.png", e.type_index)),
                encode_png(&img)?,
            )?;
        }
        let glyphs: Vec<_> = f
            .glyphs
            .iter()
            .map(|g| {
                json!({"char": (g.ch as char).to_string(), "sheet": g.image, "x": g.s, "y": g.t,
                       "w": g.width, "h": g.height})
            })
            .collect();
        std::fs::write(
            dir.join(format!("{:02}.json", e.type_index)),
            serde_json::to_string_pretty(&json!({
                "format": format!("{:?}{}", f.fmt, f.siz.bits()),
                "glyphs": glyphs,
            }))?,
        )?;
        println!(
            "  UVFT {}: {:?}{}, {} glyphs, {} sheets, glyph {}x{}",
            e.type_index,
            f.fmt,
            f.siz.bits(),
            f.glyphs.len(),
            f.images.len(),
            f.glyphs.first().map_or(0, |g| g.width),
            f.glyphs.first().map_or(0, |g| g.height),
        );
    }
    println!("fonts written to {}", dir.display());
    Ok(())
}

pub fn anims(fs: &Filesystem, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let models = entries(fs, b"UVMD")
        .map(|e| Uvmd::parse(&fs.read(e)?))
        .collect::<Result<Vec<_>>>()?;
    let mut st = BTreeMap::<&str, usize>::new();
    let mut add = |k, v| *st.entry(k).or_default() += v;
    for e in entries(fs, b"UVAN") {
        let a = Uvan::parse(&fs.read(e)?).with_context(|| format!("UVAN {}", e.type_index))?;
        add("files", 1);
        add("tracks", a.tracks.len());
        let keys: Vec<_> = a.tracks.iter().flat_map(|t| &t.keys).collect();
        add("keys", keys.len());
        add(
            "keys not quaternion",
            keys.iter().filter(|k| k.format() != 1).count(),
        );
        add(
            "keys with unk12_4 flag",
            keys.iter().filter(|k| k.flag()).count(),
        );
        add(
            "keys outside first..=last frame",
            keys.iter()
                .filter(|k| !(a.first_frame..=a.last_frame).contains(&(k.frame as i32)))
                .count(),
        );
        add(
            "tracks with unsorted frames",
            a.tracks
                .iter()
                .filter(|t| t.keys.windows(2).any(|w| w[0].frame >= w[1].frame))
                .count(),
        );
        let unit = |q: &[f32; 4]| (q.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 0.01;
        add(
            "non-unit quaternions",
            keys.iter().filter(|k| !unit(&k.quat)).count(),
        );
        match models.get(a.model as usize) {
            None => add("model out of range", 1),
            Some(m) => add(
                "tracks with part >= model parts",
                a.tracks
                    .iter()
                    .filter(|t| t.part as usize >= m.matrices.len())
                    .count(),
            ),
        }
        add("step != 1", (a.step != 1) as usize);
        let tracks: Vec<_> = a
            .tracks
            .iter()
            .map(|t| {
                json!({"part": t.part, "keys": t.keys.iter().map(|k| json!({
                    "frame": k.frame, "quat_xyzw": k.quat, "flags": k.flags,
                })).collect::<Vec<_>>()})
            })
            .collect();
        std::fs::write(
            dir.join(format!("{:03}.json", e.type_index)),
            serde_json::to_string_pretty(&json!({
                "model": a.model, "first_frame": a.first_frame, "last_frame": a.last_frame,
                "unk8": a.unk8, "step": a.step, "unk14": a.unk14,
                "frame_count": a.frame_count(), "tracks": tracks,
            }))?,
        )?;
    }
    println!("anims written to {}", dir.display());
    for (k, v) in &st {
        println!("  {k:<36} {v}");
    }
    Ok(())
}

/// `--levels`: UVLV + UVEN + UVTP → `<dir>/levels.json`, with range checks
/// of every id against the file counts and the sub-item counts in UVSY.
pub fn levels(fs: &Filesystem, out: &Path) -> Result<()> {
    use pw64_formats::{uven, uvlv, uvtp, uvtr};
    let one = |tag: &'static [u8; 4]| -> Result<pw64_rom::Form> {
        fs.read(
            entries(fs, tag)
                .next()
                .with_context(|| format!("no {tag:?}"))?,
        )
    };
    let levels = uvlv::parse(&one(b"UVLV")?)?;
    let envs = uven::parse(&one(b"UVEN")?)?;
    let pals = uvtp::parse(&one(b"UVTP")?)?;
    let terras = uvtr::parse(&one(b"UVTR")?)?;
    // UVSY COMM: f32, then u16 counts; these offsets hold the per-file
    // sub-item counts (UVEN, UVTR, UVLV, UVTP).
    let sy = one(b"UVSY")?;
    let sy = &sy.block(b"COMM").context("UVSY COMM")?.data;
    let sy16 = |o: usize| u16::from_be_bytes([sy[o], sy[o + 1]]) as usize;
    for (name, got, want) in [
        ("UVEN", envs.len(), sy16(0xC)),
        ("UVTR", terras.len(), sy16(0x10)),
        ("UVLV", levels.len(), sy16(0x14)),
        ("UVTP", pals.len(), sy16(0x20)),
    ] {
        println!("  {name}: {got} parsed, UVSY says {want}");
        ensure!(got == want, "{name} count mismatch");
    }
    let count = |tag: &'static [u8; 4]| entries(fs, tag).count();
    let limits = [
        terras.len(),
        1, // UVLT: one file, one COMM per light (only id 0 exists)
        envs.len(),
        count(b"UVMD"),
        count(b"UVCT"),
        count(b"UVTX"),
        usize::MAX, // UVSQ: sub-items of one file, not cross-checked
        count(b"UVAN"),
        count(b"UVFT"),
        count(b"UVBT"),
    ];
    let mut bad = 0;
    let mut totals = [0usize; 10];
    for (li, lv) in levels.iter().enumerate() {
        for (k, (name, ids)) in lv.lists().iter().enumerate() {
            totals[k] += ids.len();
            for &id in *ids {
                if id as usize >= limits[k] {
                    println!("  level {li}: {name} id {id} out of range");
                    bad += 1;
                }
            }
        }
    }
    for (i, e) in envs.iter().enumerate() {
        for m in &e.models {
            if m.model as usize >= limits[3] {
                println!("  env {i}: model {} out of range", m.model);
                bad += 1;
            }
        }
    }
    for (i, p) in pals.iter().enumerate() {
        for &(a, b) in &p.remaps {
            if a as usize >= limits[5] || b as usize >= limits[5] {
                println!("  palette {i}: remap {a}->{b} out of range");
                bad += 1;
            }
        }
    }
    let names = levels
        .first()
        .map(|l| l.lists().map(|(n, _)| n))
        .unwrap_or_default();
    println!(
        "  ids per type: {}",
        names
            .iter()
            .zip(totals)
            .map(|(n, t)| format!("{n} {t}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  out-of-range ids: {bad}");
    ensure!(bad == 0, "ids out of range");

    let json_levels: Vec<_> = levels
        .iter()
        .map(|l| {
            serde_json::Value::Object(
                l.lists()
                    .iter()
                    .filter(|(_, ids)| !ids.is_empty())
                    .map(|(n, ids)| (n.to_string(), json!(ids)))
                    .collect(),
            )
        })
        .collect();
    let json_envs: Vec<_> = envs
        .iter()
        .map(|e| {
            json!({
                "models": e.models.iter().map(|m| json!({"model": m.model, "flags": m.flags})).collect::<Vec<_>>(),
                "screen": e.screen, "fog_color": e.fog_color, "unused_color": e.unused_color,
                "fog_min": e.fog_min, "fog_max": e.fog_max, "fog_enabled": e.fog_enabled,
                "fog_factor": e.fog_factor(), "clear": e.clear, "unk": e.unk,
            })
        })
        .collect();
    let setups: Vec<_> = uvlv::EnvSetup::all()
        .map(|s| {
            json!({
                "env": s.env, "map": format!("{:?}", s.map), "map_level": s.map as u16,
                "terra": s.map.terra(), "condition": s.condition,
                "palette": s.palette, "env_level": s.env_level,
            })
        })
        .collect();
    std::fs::create_dir_all(out)?;
    let path = out.join("levels.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&json!({
            "env_setups": setups,
            "environments": json_envs,
            "palettes": pals.iter().map(|p| &p.remaps).collect::<Vec<_>>(),
            "levels": json_levels,
        }))?,
    )?;
    println!("wrote {}", path.display());
    for s in uvlv::EnvSetup::all() {
        let e = &envs[s.env as usize];
        println!(
            "  env {:2} {:<16} cond {} terra {} pal {:>4} | clear {} {:?} fog {:.3} {:?} | models {:?}",
            s.env,
            format!("{:?}", s.map),
            s.condition,
            s.map.terra(),
            s.palette.map_or("-".into(), |p| p.to_string()),
            e.clear as u8,
            &e.screen[..3],
            e.fog_factor(),
            &e.fog_color[..3],
            e.models
                .iter()
                .map(|m| (m.model, m.flags))
                .collect::<Vec<_>>()
        );
    }
    Ok(())
}
