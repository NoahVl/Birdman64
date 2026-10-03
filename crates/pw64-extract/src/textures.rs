//! `--textures`: decode every UVTX to PNG and print a format histogram.

use anyhow::{Context, Result};
use pw64_formats::{Image, Uvtx};
use pw64_rom::Filesystem;
use std::collections::BTreeMap;
use std::path::Path;

pub fn export(fs: &Filesystem, dir: &Path, all_tiles: bool) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let entries: Vec<_> = fs.entries.iter().filter(|e| e.tag.0 == *b"UVTX").collect();
    let uvtx = entries
        .iter()
        .map(|e| Uvtx::parse(&fs.read(e)?).with_context(|| format!("UVTX {}", e.type_index)))
        .collect::<Result<Vec<_>>>()?;

    let mut formats = BTreeMap::<String, usize>::new();
    let mut sizes = BTreeMap::<(u32, u32), usize>::new();
    let mut levels = BTreeMap::<u8, usize>::new();
    let mut header_mismatch = 0;
    let mut wraps = BTreeMap::<(u8, u8, u8, u8), usize>::new();
    for (i, t) in uvtx.iter().enumerate() {
        let image2 = match t.image2 {
            pw64_formats::uvtx::NO_TEXTURE => None,
            id => Some(&uvtx.get(id as usize).context("image2 out of range")?.image[..]),
        };
        let dec = t
            .decode(image2)
            .with_context(|| format!("decoding UVTX {i}"))?;
        let base = dec.base();
        let (w, h) = (base.image.width, base.image.height);
        if (w, h) != (t.width as u32, t.height as u32) {
            header_mismatch += 1;
            eprintln!("UVTX {i}: tile {w}x{h} != header {}x{}", t.width, t.height);
        }
        let mut key = base.tile.desc.format.to_string();
        if let Some(d) = dec.tiles.iter().find(|d| d.source != Some(0)) {
            key += &format!(" + {} (image2)", d.tile.desc.format);
        }
        let wrap = |m: pw64_formats::gbi::WrapMode| m.clamp as u8 * 2 + m.mirror as u8;
        *wraps
            .entry((
                t.wrap_s,
                wrap(base.tile.desc.cms),
                t.wrap_t,
                wrap(base.tile.desc.cmt),
            ))
            .or_default() += 1;
        *formats.entry(key).or_default() += 1;
        *sizes.entry((w, h)).or_default() += 1;
        *levels.entry(dec.levels).or_default() += 1;

        write_png(&dir.join(format!("{i:03}.png")), &base.image)?;
        if all_tiles {
            for d in dec.tiles.iter().filter(|d| d.index != base.index) {
                write_png(&dir.join(format!("{i:03}_t{}.png", d.index)), &d.image)?;
            }
        }
    }

    println!("decoded {} UVTX to {}", uvtx.len(), dir.display());
    println!("formats (own image [+ image2]):");
    for (k, n) in &formats {
        println!("  {k:<24} {n:4}");
    }
    println!("(wrap_s, tile cms, wrap_t, tile cmt): {wraps:?}");
    println!("mip levels: {levels:?}");
    println!("sizes: {sizes:?}");
    println!("tile size != header width/height: {header_mismatch}");
    Ok(())
}

fn write_png(path: &Path, img: &Image) -> Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, img.width, img.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&img.rgba)?;
    Ok(())
}
