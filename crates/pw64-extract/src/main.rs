//! pw64-extract: verify a user-supplied ROM and dump its filesystem.
//!
//! Usage:
//!   pw64-extract <rom> [--out <dir>]   extract every FORM file to <dir> (default: data)
//!   pw64-extract <rom> --list          print a per-tag summary only
//!   pw64-extract <rom> --textures [--all-tiles] [--out <dir>]
//!                                      decode all UVTX to <dir>/textures/<typeindex>.png
//!   pw64-extract <rom> --models [--out <dir>]
//!                                      convert all UVMD to <dir>/models/<typeindex>.glb
//!   pw64-extract <rom> --terrain [--out <dir>]
//!                                      each UVTR terra to <dir>/terrain/<n>.glb + <n>_top.png
//!   pw64-extract <rom> --blits | --fonts | --anims [--out <dir>]
//!                                      UVBT → blits/*.png, UVFT → fonts/*.png+json, UVAN → anims/*.json
//!   pw64-extract <rom> --levels [--out <dir>]
//!                                      UVLV/UVEN/UVTP → <dir>/levels.json (+ range checks)
//!   pw64-extract <rom> --audio [--out <dir>]
//!                                      sound banks to <dir>/audio/samples/*.wav,
//!                                      sequences to <dir>/audio/seq/<n>.{seq,mid}

use anyhow::{Context, Result, bail};
use pw64_rom::{Filesystem, Form, Rom};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

mod audio;
mod gltf;
mod misc;
mod models;
mod terrain;
mod textures;

#[derive(Serialize)]
struct ManifestFile<'a> {
    #[serde(flatten)]
    entry: &'a pw64_rom::FileEntry,
    dir: String,
    form: Form,
}

const USAGE: &str = "usage: pw64-extract <rom> [--out <dir>] [--list | --textures [--all-tiles] | --models | --terrain | --blits | --fonts | --anims | --levels | --audio]";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut rom_path = None;
    let mut out = PathBuf::from("data");
    let mut list_only = false;
    let mut textures = false;
    let mut all_tiles = false;
    let mut models = false;
    let mut audio = false;
    let mut terrain = false;
    let mut blits = false;
    let mut fonts = false;
    let mut anims = false;
    let mut levels = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => out = args.next().context("--out needs a directory")?.into(),
            "--list" => list_only = true,
            "--textures" => textures = true,
            "--all-tiles" => all_tiles = true,
            "--models" => models = true,
            "--audio" => audio = true,
            "--terrain" => terrain = true,
            "--blits" => blits = true,
            "--fonts" => fonts = true,
            "--anims" => anims = true,
            "--levels" => levels = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if rom_path.is_none() => rom_path = Some(PathBuf::from(a)),
            _ => bail!("unexpected argument {a:?}"),
        }
    }
    let rom_path = rom_path.context(USAGE)?;

    let rom = Rom::load(&rom_path)?;
    println!("ROM OK: {:?} release", rom.version);
    let fs = Filesystem::open(&rom)?;

    let mut per_tag = BTreeMap::<String, usize>::new();
    for e in &fs.entries {
        *per_tag.entry(e.tag.to_string()).or_default() += 1;
    }
    println!("{} files:", fs.entries.len());
    for (tag, n) in &per_tag {
        println!("  {tag}  {n:4}");
    }
    if list_only {
        return Ok(());
    }
    if textures {
        return textures::export(&fs, &out.join("textures"), all_tiles);
    }
    if models {
        return models::export(&fs, &out.join("models"));
    }
    if terrain {
        return terrain::export(&fs, &out.join("terrain"));
    }
    if blits {
        return misc::blits(&fs, &out.join("blits"));
    }
    if fonts {
        return misc::fonts(&fs, &out.join("fonts"));
    }
    if levels {
        return misc::levels(&fs, &out);
    }
    if anims {
        return misc::anims(&fs, &out.join("anims"));
    }
    if audio {
        return audio::export(&rom, &fs, &out.join("audio"));
    }

    let fs_dir = out.join("fs");
    std::fs::create_dir_all(&fs_dir)?;
    let mut manifest = Vec::new();
    for e in &fs.entries {
        let form = fs.read(e)?;
        let dir = format!("{:04}_{}_{:03}", e.index, e.tag, e.type_index).replace(' ', "_");
        let path = fs_dir.join(&dir);
        std::fs::create_dir_all(&path)?;
        std::fs::write(path.join("raw.bin"), fs.raw(e)?)?;
        for (i, b) in form.blocks.iter().enumerate() {
            let name = format!("{i:03}_{}.bin", b.tag).replace(' ', "_");
            std::fs::write(path.join(name), &b.data)?;
        }
        manifest.push(ManifestFile {
            entry: e,
            dir,
            form,
        });
    }
    std::fs::write(
        out.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    println!("extracted to {}", out.display());
    Ok(())
}
