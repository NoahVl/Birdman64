//! Texture packs (Phase 6): `PW64_TEX_PACKS=<dir>` replaces decoded
//! textures with PNGs keyed by their content key (`pw64-gfx` `texture.rs`:
//! FNV-1a of format, size, covered TMEM bytes + TLUT per bound tile). The
//! `PW64_DUMP_TEX` dump names are the same keys, so a pack is made by
//! dumping and editing those files.
//!
//! The directory is indexed once at startup, so a texture without a pack
//! file costs one map lookup, never a file-system probe. A PNG is decoded
//! when its key is first bound; `TextureCache` calls the replacer once per
//! key and keeps the result, so nothing is cached here.
//!
//! - `<dir>/<16 hex key>.png` replaces that key.
//! - RT64-style packs (`rt64.json`, alias `texture.json`, or Rice-named
//!   `8hex#fmt#pal.png` files): RT64/Rice hashes do not correspond to our
//!   keys, so those files are reachable only through `<dir>/pw64_keys.csv`
//!   rows `pack_hash,pw64_key`. A pack hash resolves through the json's
//!   `textures[].hashes.{rt64,rice}` → `path`, else to `<dir>/<hash>.png`.
//!   Details and what RT64 metadata is ignored: renderer.md "Texture packs".
//!
//! Any PNG colour type loads (expanded to RGBA8), up to [`MAX_SIDE`] per
//! side (UVs are normalized by the renderer, so higher resolutions just work).

use pw64_formats::Image;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// Largest accepted side: wgpu's default `max_texture_dimension_2d` (the
/// renderer also drops mip levels past the device's real limit), and a
/// bound on decode memory (256 MiB).
const MAX_SIDE: u32 = 8192;

/// The pack index: content key → PNG file.
#[derive(Default)]
pub struct TexPacks {
    files: HashMap<u64, PathBuf>,
}

impl TexPacks {
    /// `PW64_TEX_PACKS=<dir>` (relative to the cwd like the other path
    /// options). Absent/empty → packs off; not a directory → reported, off.
    pub fn from_env() -> Self {
        let Some(dir) = std::env::var_os("PW64_TEX_PACKS").filter(|d| !d.is_empty()) else {
            return Self::default();
        };
        let dir = PathBuf::from(dir);
        if !dir.is_dir() {
            eprintln!(
                "[packs] PW64_TEX_PACKS={}: not a directory - packs off",
                dir.display()
            );
            return Self::default();
        }
        Self::open(&dir)
    }

    /// Indexes `dir`: key-named PNGs, then the `pw64_keys.csv` bridge
    /// (bridged rows override a key-named file of the same key).
    fn open(dir: &Path) -> Self {
        let mut files = HashMap::new();
        // Root PNGs by lower-case stem: auto-path targets of bridged hashes.
        let mut stems = HashMap::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let Some((stem, ext)) = name.to_str().and_then(|n| n.rsplit_once('.')) else {
                continue;
            };
            if !ext.eq_ignore_ascii_case("png") {
                continue;
            }
            if let Some(key) = hex_key(stem) {
                files.insert(key, entry.path());
            }
            stems.insert(norm_name(stem), entry.path());
        }
        let named = files.len();
        let json = json_paths(dir);
        let (mut bridged, mut missing) = (0, 0);
        for (key, name) in parse_bridge_csv(dir) {
            match json.get(&name).or_else(|| stems.get(&name)) {
                Some(path) => {
                    files.insert(key, path.clone());
                    bridged += 1;
                }
                None => missing += 1,
            }
        }
        eprintln!(
            "[packs] {}: {named} key-named PNGs, {bridged} bridged keys ({missing} csv rows without a file)",
            dir.display()
        );
        Self { files }
    }

    /// Packs on (any replacement indexed)?
    pub fn any(&self) -> bool {
        !self.files.is_empty()
    }

    /// Replacement for `key`: decodes its pack file, if it has one.
    pub fn replace(&self, key: u64) -> Option<Image> {
        let path = self.files.get(&key)?;
        load_png(path)
            .inspect_err(|e| eprintln!("[packs] {} (key {key:016x}): {e}", path.display()))
            .ok()
    }
}

/// Reads and decodes one pack PNG to RGBA8.
fn load_png(path: &Path) -> Result<Image, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut dec = png::Decoder::new(std::io::BufReader::new(file));
    // Palette/low-bit → 8-bit, 16-bit → 8-bit: only the channel count varies.
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let (w, h) = reader.info().size();
    if w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!("{w}x{h} exceeds {MAX_SIDE} per side"));
    }
    let mut buf = vec![0; reader.output_buffer_size().ok_or("image too large")?];
    let out = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    buf.truncate(out.buffer_size());
    if out.bit_depth != png::BitDepth::Eight {
        return Err(format!("unsupported bit depth {:?}", out.bit_depth));
    }
    let rgba = match out.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[v, a]| [v, v, v, a])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        other => return Err(format!("unsupported color type {other:?}")),
    };
    Ok(Image {
        rgba,
        width: out.width,
        height: out.height,
    })
}

/// Our key from a file stem: exactly 16 hex digits (any case).
fn hex_key(stem: &str) -> Option<u64> {
    if stem.len() == 16 && stem.bytes().all(|b| b.is_ascii_hexdigit()) {
        u64::from_str_radix(stem, 16).ok()
    } else {
        None
    }
}

/// Pack hash name → file, from `rt64.json` (or the `texture.json` alias).
/// Only `textures[].hashes.{rt64,rice}` + `path` are read; entries with an
/// empty `path` use the auto path (`<hash>.png`, resolved by the caller).
fn json_paths(dir: &Path) -> HashMap<String, PathBuf> {
    let mut names = HashMap::new();
    let Some((file, text)) = ["rt64.json", "texture.json"]
        .into_iter()
        .find_map(|f| Some((f, std::fs::read_to_string(dir.join(f)).ok()?)))
    else {
        return names;
    };
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("[packs] {file}: {e} - ignored");
            return names;
        }
    };
    // Indexing a serde_json::Value never panics: absent/mistyped → Null.
    for entry in json["textures"].as_array().into_iter().flatten() {
        let Some(rel) = entry["path"].as_str().filter(|p| !p.trim().is_empty()) else {
            continue;
        };
        let Some(path) = resolve_json_path(dir, rel) else {
            eprintln!("[packs] {file}: path {rel:?} is not a file inside the pack - skipped");
            continue;
        };
        for hash in ["rt64", "rice"] {
            if let Some(name) = entry["hashes"][hash].as_str() {
                names.insert(norm_name(name), path.clone());
            }
        }
    }
    names
}

/// A json `path` (relative to the pack root, `/` or `\`, extension optional)
/// → an existing file inside the pack. Extensionless and `.dds` paths are
/// read as `.png` (DDS is not supported). Absolute paths and `..` are
/// rejected: pack metadata must not reach outside the pack.
fn resolve_json_path(dir: &Path, rel: &str) -> Option<PathBuf> {
    let rel = PathBuf::from(rel.replace('\\', "/"));
    if !rel
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    let path = dir.join(&rel);
    let path = match path.extension() {
        None => path.with_extension("png"),
        Some(e) if e.eq_ignore_ascii_case("dds") => path.with_extension("png"),
        Some(_) => path,
    };
    path.is_file().then_some(path)
}

/// Optional bridge: `<pack>/pw64_keys.csv`, one row per texture —
/// `pack_hash,pw64_key` (the RT64 `16 hex` or Rice `8hex#fmt#pal` hash that
/// names the file in the pack, then our key as `PW64_DUMP_TEX` names it).
/// `#` comments, blank lines, a header row and other junk rows are skipped;
/// extra columns are ignored; the first row for a key wins. Returns our key
/// → normalized hash name.
fn parse_bridge_csv(dir: &Path) -> HashMap<u64, String> {
    let mut map = HashMap::new();
    let Ok(text) = std::fs::read_to_string(dir.join("pw64_keys.csv")) else {
        return map;
    };
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') {
            continue;
        }
        let mut fields = line.split(',').map(norm_name);
        let (Some(name), Some(key)) = (fields.next(), fields.next()) else {
            continue;
        };
        // The header ("rt64_hash,pw64_hash") fails the hash-name check.
        if let (true, Ok(key)) = (is_hash_name(&name), u64::from_str_radix(&key, 16)) {
            map.entry(key).or_insert(name);
        }
    }
    map
}

/// A pack-side hash name: Rice (`8hex#fmt#pal`, e.g. `61094ee7#4#1`) or
/// RT64 (`16 hex`). Also keeps csv names safe to use as file names.
fn is_hash_name(name: &str) -> bool {
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match name.split('#').collect::<Vec<_>>()[..] {
        [crc, fmt, pal] => hex(crc, 8) && digits(fmt) && digits(pal),
        [rt64] => hex(rt64, 16),
        _ => false,
    }
}

fn norm_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIXELS: [u8; 8] = [255, 0, 0, 255, 0, 0, 255, 255];

    /// Unique temp dir per test (tests run in parallel within one pid).
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pw64-packs-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes a 2×1 PNG of `color` type with `data` pixels.
    fn write_png(path: &Path, color: png::ColorType, data: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut enc = png::Encoder::new(file, 2, 1);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(data).unwrap();
        w.finish().unwrap();
    }

    /// The 2×1 half-red/half-blue RGBA8 test image.
    fn write_test_png(path: &Path) {
        write_png(path, png::ColorType::Rgba, &PIXELS);
    }

    /// Key-named files (any case) replace their key; anything else in the
    /// directory is ignored, and an unknown key is `None`.
    #[test]
    fn loads_png_by_key() {
        let dir = test_dir("flat");
        const KEY: u64 = 0x1234_5678_9abc_def0;
        write_test_png(&dir.join(format!("{KEY:016x}.png")));
        write_test_png(&dir.join("00000000000000AB.PNG"));
        write_test_png(&dir.join("not-a-key.png"));
        std::fs::write(dir.join("0000000000000003.txt"), "x").unwrap();
        let packs = TexPacks::open(&dir);
        assert_eq!(packs.files.len(), 2);
        let img = packs.replace(KEY).expect("packed texture");
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.rgba, PIXELS);
        assert!(packs.replace(0xab).is_some());
        assert!(packs.replace(1).is_none());
        assert!(!TexPacks::default().any());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Non-RGBA PNGs are expanded to RGBA8; a broken file is `None`, not a
    /// panic.
    #[test]
    fn converts_color_types() {
        let dir = test_dir("colors");
        write_png(
            &dir.join("0000000000000001.png"),
            png::ColorType::Rgb,
            &[1, 2, 3, 4, 5, 6],
        );
        write_png(
            &dir.join("0000000000000002.png"),
            png::ColorType::GrayscaleAlpha,
            &[7, 8, 9, 10],
        );
        write_png(
            &dir.join("0000000000000003.png"),
            png::ColorType::Grayscale,
            &[11, 12],
        );
        std::fs::write(dir.join("0000000000000004.png"), b"\x89PNG junk").unwrap();
        let packs = TexPacks::open(&dir);
        let rgba = |key| packs.replace(key).map(|i| i.rgba);
        assert_eq!(rgba(1).unwrap(), [1, 2, 3, 255, 4, 5, 6, 255]);
        assert_eq!(rgba(2).unwrap(), [7, 7, 7, 8, 9, 9, 9, 10]);
        assert_eq!(rgba(3).unwrap(), [11, 11, 11, 255, 12, 12, 12, 255]);
        assert!(rgba(4).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A json-less pack with Rice-named files resolves through the csv
    /// bridge (auto path, case-insensitive).
    #[test]
    fn bridges_jsonless_rice_pack() {
        let dir = test_dir("rice");
        write_test_png(&dir.join("61094EE7#4#1.png"));
        std::fs::write(dir.join("pw64_keys.csv"), "61094ee7#4#1,0000000000000002\n").unwrap();
        let packs = TexPacks::open(&dir);
        assert_eq!(packs.replace(2).expect("bridged rice texture").rgba, PIXELS);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The json + csv path: an RT64 hash and its Rice spelling both resolve
    /// through one entry, an empty json `path` falls back to the auto path,
    /// extensionless paths get `.png`, paths leaving the pack are rejected,
    /// and unbridged keys stay `None`.
    #[test]
    fn rt64_bridge_maps_keys() {
        let dir = test_dir("bridge");
        write_test_png(&dir.join("hd/wing.png"));
        write_test_png(&dir.join("20fff290#4#1.png"));
        write_test_png(&dir.join("hd/sea.png"));
        // A real PNG next to the pack dir, named via `..`.
        let outside_png = dir.with_extension("outside.png");
        write_test_png(&outside_png);
        let outside = format!("../{}", outside_png.file_name().unwrap().to_str().unwrap());
        let json = serde_json::json!({
            "configuration": {"configurationVersion": 3},
            "textures": [
                {"hashes": {"rice": "61094EE7#4#1", "rt64": "0001d0556bc980b5"},
                 "path": "hd\\wing.png"},
                {"hashes": {"rice": "20fff290#4#1"}, "path": ""},
                {"hashes": {"rt64": "00000000000000aa"}, "path": "hd/sea"},
                {"hashes": {"rt64": "00000000000000bb"}, "path": outside},
                {"hashes": {"rt64": "00000000000000cc"}, "path": "/etc/passwd"},
                {"hashes": 5, "path": ["junk"]},
                "junk"
            ]
        });
        std::fs::write(dir.join("rt64.json"), json.to_string()).unwrap();
        std::fs::write(
            dir.join("pw64_keys.csv"),
            "# hand-made bridge\n\
             \n\
             rt64_hash,pw64_hash\n\
             0001D0556BC980B5,123456789abcdef0\n\
             61094ee7#4#1,00000000000000ff,junk-extra-column\n\
             20fff290#4#1,00000000000000fe\n\
             00000000000000aa,00000000000000fd\n\
             00000000000000bb,00000000000000fc\n\
             00000000000000cc,00000000000000fb\n\
             not-a-hash,00ff\n\
             0000000000000003,\n",
        )
        .unwrap();
        let packs = TexPacks::open(&dir);
        for key in [0x1234_5678_9abc_def0, 0xff, 0xfe, 0xfd] {
            let img = packs.replace(key).expect("bridged texture");
            assert_eq!(img.rgba, PIXELS, "key {key:x}");
        }
        for key in [0xfc, 0xfb, 0xee] {
            assert!(packs.replace(key).is_none(), "key {key:x}");
        }
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&outside_png).ok();
    }

    /// A broken json is ignored; csv rows still reach auto-path files.
    #[test]
    fn broken_json_keeps_auto_paths() {
        let dir = test_dir("badjson");
        std::fs::write(dir.join("texture.json"), "not json").unwrap();
        write_test_png(&dir.join("61094ee7#4#1.png"));
        std::fs::write(dir.join("pw64_keys.csv"), "61094ee7#4#1,5\n").unwrap();
        assert!(TexPacks::open(&dir).replace(5).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `pw64_keys.csv` leniency: header, comments, blanks and junk rows are
    /// skipped; extra columns are ignored; the first row for a key wins.
    #[test]
    fn bridge_csv_is_lenient() {
        let dir = test_dir("csv");
        std::fs::write(
            dir.join("pw64_keys.csv"),
            "rt64_hash,pw64_hash\n\
             # comment\n\
             \n\
             0001d0556bc980b5,  0000000000000001  \n\
             61094EE7#4#1,2,extra\n\
             20fff290#4#1,2\n\
             garbage line without hashes\n\
             61094ee7#4#1\n\
             61094ee7#4#1,zzz\n\
             ../../evil,3\n\
             61094ee7#4#1#9,4\n",
        )
        .unwrap();
        let bridge = parse_bridge_csv(&dir);
        assert_eq!(bridge.len(), 2);
        assert_eq!(bridge[&1], "0001d0556bc980b5");
        assert_eq!(bridge[&2], "61094ee7#4#1");
        std::fs::remove_dir_all(&dir).ok();
    }
}
