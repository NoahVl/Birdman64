use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::Path;

/// SHA1 of the big-endian (.z64) North American ROM, as expected by the decomp.
pub const SHA1_US: &str = "ec771aedf54ee1b214c25404fb4ec51cfd43191a";

/// What a failed ROM check is, in player terms (ux-1.0 U11): the friendly
/// counterpart of the technical `Rom::load` errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RomProblem {
    /// Not an N64 ROM at all (bad header magic, or too small to have one).
    NotN64,
    /// A valid N64 ROM of a different game (header title at 0x20).
    OtherGame(String),
    /// Pilotwings 64, but the `{European|Japanese|non-US}` version.
    Region(String),
    /// Pilotwings 64 (US) header, but the SHA1 didn't match: a hack or a
    /// bad dump.
    Modified,
}

/// ROM file extensions a zip member may have (checked case-insensitively).
const ROM_EXTENSIONS: [&str; 3] = ["z64", "n64", "v64"];

/// N64 carts top out at 64 MiB: raw files larger than that are rejected by
/// stat alone, and zip members are only inflated up to this size.
const MAX_ROM: u64 = 64 << 20;

/// Zip local-file-header magic (`Rom::load` routes these to [`Rom::from_zip`]).
const ZIP_MAGIC: &[u8; 4] = b"PK\x03\x04";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RomVersion {
    Us,
}

/// A verified ROM image, normalized to big-endian (.z64) byte order.
pub struct Rom {
    data: Vec<u8>,
    pub version: RomVersion,
}

/// Is this zip member name a ROM image? Only the extension counts (the file
/// name is arbitrary), matching is case-insensitive and nested folders are
/// allowed (zip names always use `/`).
fn is_rom_member(name: &str) -> bool {
    // macOS Finder zips add `__MACOSX/._<name>` resource-fork stubs with the
    // same extension; they'd make a single-ROM zip look like two.
    let base = name.rsplit('/').next().unwrap_or(name);
    if name.starts_with("__MACOSX/") || base.starts_with("._") {
        return false;
    }
    ROM_EXTENSIONS
        .iter()
        .any(|ext| name.to_ascii_lowercase().ends_with(&format!(".{ext}")))
}

impl Rom {
    pub fn load(path: &Path) -> Result<Self> {
        let mut file =
            std::fs::File::open(path).with_context(|| format!("reading ROM {}", path.display()))?;
        let mut magic = [0u8; 4];
        let n = file
            .read(&mut magic)
            .with_context(|| format!("reading ROM {}", path.display()))?;
        if n < magic.len() {
            bail!(
                "file is too small to be an N64 ROM ({n} bytes): {}",
                path.display()
            );
        }
        // Zips are accepted anywhere a bare ROM is (the root scan filters on
        // `.zip` too): probe members without reading the whole container.
        if magic == *ZIP_MAGIC {
            return Self::from_zip(path, file);
        }
        // Raw image: a stat rejects implausibly large files before reading.
        let size = file
            .metadata()
            .with_context(|| format!("reading ROM {}", path.display()))?
            .len();
        if size > MAX_ROM {
            bail!(
                "{} is too large to be an N64 ROM ({size} bytes)",
                path.display()
            );
        }
        let mut data = Vec::with_capacity(size.max(magic.len() as u64) as usize);
        data.extend_from_slice(&magic);
        file.read_to_end(&mut data)
            .with_context(|| format!("reading ROM {}", path.display()))?;
        Self::from_bytes(data)
    }

    /// Extracts the single `.z64`/`.n64`/`.v64` member of a zip archive,
    /// opened on `file` (File-backed: only the central directory and the one
    /// member are read), and verifies it via [`Self::from_bytes`].
    fn from_zip(path: &Path, file: impl std::io::Read + std::io::Seek) -> Result<Self> {
        let name = path.display();
        let mut archive =
            zip::ZipArchive::new(file).with_context(|| format!("reading zip archive {name}"))?;
        let members: Vec<String> = archive.file_names().map(str::to_owned).collect();
        let roms: Vec<&str> = members
            .iter()
            .map(String::as_str)
            .filter(|n| is_rom_member(n))
            .collect();
        match roms[..] {
            [rom] => {
                let mut file = archive
                    .by_name(rom)
                    .with_context(|| format!("member {rom:?} of zip archive {name}"))?;
                // N64 carts top out at 64 MiB: don't trust the header size
                // for the allocation (a bogus zip would abort on OOM).
                if file.size() > MAX_ROM {
                    bail!("member {rom:?} of zip archive {name} is too large for an N64 ROM");
                }
                let mut bytes = Vec::with_capacity(usize::try_from(file.size()).unwrap_or(0));
                (&mut file)
                    .take(MAX_ROM + 1)
                    .read_to_end(&mut bytes)
                    .with_context(|| format!("reading member {rom:?} of zip archive {name}"))?;
                // Byte-order normalization + SHA1 check happen in from_bytes.
                Self::from_bytes(bytes)
                    .with_context(|| format!("ROM member {rom:?} of zip archive {name}"))
            }
            _ => bail!(
                "zip archive {name} must contain exactly one ROM member \
                 (.z64/.n64/.v64), found {}: {}",
                roms.len(),
                members.join(", ")
            ),
        }
    }

    /// Accepts .z64 (big-endian), .v64 (16-bit swapped) or .n64 (32-bit little-endian) images.
    pub fn from_bytes(mut data: Vec<u8>) -> Result<Self> {
        if data.len() < 0x1000 || !data.len().is_multiple_of(4) {
            bail!(
                "file is too small or misaligned to be an N64 ROM ({} bytes)",
                data.len()
            );
        }
        match data[..4] {
            [0x80, 0x37, 0x12, 0x40] => {}
            [0x37, 0x80, 0x40, 0x12] => data
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .for_each(|c| c.swap(0, 1)),
            [0x40, 0x12, 0x37, 0x80] => data
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .for_each(|c| c.reverse()),
            _ => bail!("not an N64 ROM (unknown header magic {:02x?})", &data[..4]),
        }
        let hash = sha1_smol::Sha1::from(&data).digest().to_string();
        let version = match hash.as_str() {
            SHA1_US => RomVersion::Us,
            _ => bail!(
                "unsupported ROM (sha1 {hash}); only Pilotwings 64 (USA) is supported, expected {SHA1_US}"
            ),
        };
        Ok(Self { data, version })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn slice(&self, offset: usize, len: usize) -> Result<&[u8]> {
        self.data
            .get(offset..offset + len)
            .with_context(|| format!("ROM read out of range: {offset:#x}+{len:#x}"))
    }
}

/// Diagnoses ROM bytes that failed verification (call it only on a load
/// error): reads the normalized header, the same way [`Rom::from_bytes`]
/// accepts byte orders, and classifies the problem for a friendly message.
/// Because it runs only after the SHA1 check failed, a Pilotwings 64 (US)
/// header means [`RomProblem::Modified`] without recomputing the hash.
pub fn diagnose(data: &[u8]) -> RomProblem {
    let Some(header) = normalized_header(data) else {
        return RomProblem::NotN64;
    };
    let title = String::from_utf8_lossy(&header[0x20..0x34]);
    // Titles are padded with NULs (sometimes spaces): drop the padding.
    let title = title.trim_end_matches(['\0', ' ']);
    if title_is_pilotwings64(title) {
        match header[0x3E] {
            b'E' => RomProblem::Modified,
            b'P' => RomProblem::Region("European".into()),
            b'J' => RomProblem::Region("Japanese".into()),
            // Other regional codes (France, Germany, ...) are just as
            // unsupported; naming them all is not worth the table.
            _ => RomProblem::Region("non-US".into()),
        }
    } else {
        RomProblem::OtherGame(title.to_string())
    }
}

/// The first 0x40 bytes of `data` with the byte order normalized to .z64
/// (big-endian): enough for the title and the country byte. `None` when the
/// header magic is none of the three known ones.
fn normalized_header(data: &[u8]) -> Option<[u8; 0x40]> {
    let mut header = [0u8; 0x40];
    header.copy_from_slice(data.get(..0x40)?);
    match header[..4] {
        [0x80, 0x37, 0x12, 0x40] => {}
        // .v64: 16-bit words are byte-swapped.
        [0x37, 0x80, 0x40, 0x12] => header
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .for_each(|c| c.swap(0, 1)),
        // .n64: 32-bit words are little-endian.
        [0x40, 0x12, 0x37, 0x80] => header
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .for_each(|c| c.reverse()),
        _ => return None,
    }
    Some(header)
}

/// Is the header title Pilotwings 64's? Spaces and underscores are ignored,
/// so "PILOTWINGS64", "PILOT WINGS 64" and the like all count.
fn title_is_pilotwings64(title: &str) -> bool {
    let squashed: String = title
        .chars()
        .filter(|c| !matches!(c, ' ' | '_'))
        .collect::<String>()
        .to_ascii_lowercase();
    squashed.contains("pilotwings64")
}

pub(crate) fn be_u32(b: &[u8], off: usize) -> Result<u32> {
    let s = b
        .get(off..off + 4)
        .with_context(|| format!("read past end at {off:#x}"))?;
    Ok(u32::from_be_bytes(s.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// Fake ROM image: right magic, so `from_bytes` fails on the SHA1 check —
    /// its message proves the bytes made it through extraction.
    fn fake_rom() -> Vec<u8> {
        let mut v = vec![0x80, 0x37, 0x12, 0x40];
        v.resize(0x1000, 0xAB);
        v
    }

    /// Builds a zip in memory with the given (member name, contents) pairs.
    fn zip_of(members: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut buf);
        for (name, data) in members {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
        buf.into_inner()
    }

    /// Writes `data` to a per-test temp file and returns its path.
    fn temp_file(tag: &str, data: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("pw64-rom-{tag}-{}.bin", std::process::id()));
        std::fs::write(&p, data).unwrap();
        p
    }

    #[test]
    fn zip_rom_member_is_extracted_and_verified() {
        let data = zip_of(&[("Pilotwings 64 (USA).z64", fake_rom())]);
        let path = temp_file("one", &data);
        // from_bytes must be the thing that fails (SHA1 message), proving the
        // member was extracted in memory — not the zip parse.
        let err = format!("{:#}", Rom::load(&path).map(|_| ()).unwrap_err());
        assert!(
            err.contains("unsupported ROM (sha1"),
            "expected the SHA1 error, got: {err}"
        );
    }

    #[test]
    fn zip_nested_rom_member_works() {
        let data = zip_of(&[("roms/inner/PW64.N64", fake_rom())]);
        let path = temp_file("nested", &data);
        let err = format!("{:#}", Rom::load(&path).map(|_| ()).unwrap_err());
        assert!(err.contains("unsupported ROM (sha1"), "got: {err}");
    }

    #[test]
    fn zip_without_rom_member_errors() {
        let data = zip_of(&[("readme.txt", b"hi".to_vec())]);
        let path = temp_file("none", &data);
        let err = format!("{:#}", Rom::load(&path).map(|_| ()).unwrap_err());
        assert!(err.contains("exactly one ROM member"), "got: {err}");
        assert!(err.contains("readme.txt"), "members must be listed: {err}");
    }

    #[test]
    fn zip_with_two_rom_members_errors() {
        let data = zip_of(&[("a.z64", fake_rom()), ("b.n64", fake_rom())]);
        let path = temp_file("two", &data);
        let err = format!("{:#}", Rom::load(&path).map(|_| ()).unwrap_err());
        assert!(err.contains("found 2"), "got: {err}");
        assert!(err.contains("a.z64") && err.contains("b.n64"), "got: {err}");
    }

    #[test]
    fn zip_macos_resource_forks_are_ignored() {
        let data = zip_of(&[
            ("PW64.z64", fake_rom()),
            ("__MACOSX/._PW64.z64", b"fork".to_vec()),
        ]);
        let path = temp_file("macos", &data);
        let err = format!("{:#}", Rom::load(&path).map(|_| ()).unwrap_err());
        assert!(err.contains("unsupported ROM (sha1"), "got: {err}");
    }

    /// Synthetic big-endian header (no ROM data): title at 0x20, country
    /// byte at 0x3E, zero padding elsewhere.
    fn header(title: &str, country: u8) -> Vec<u8> {
        let mut v = vec![0x80u8, 0x37, 0x12, 0x40];
        v.resize(0x1000, 0);
        v[0x20..0x20 + title.len()].copy_from_slice(title.as_bytes());
        v[0x3E] = country;
        v
    }

    #[test]
    fn diagnose_classifies_by_title_and_country() {
        use super::super::RomProblem;
        // US header reached through diagnose: the SHA1 must have failed, so
        // this is the Modified case (hack / bad dump).
        assert_eq!(
            diagnose(&header("PILOTWINGS64", b'E')),
            RomProblem::Modified
        );
        assert_eq!(
            diagnose(&header("PILOTWINGS64", b'P')),
            RomProblem::Region("European".into())
        );
        assert_eq!(
            diagnose(&header("PILOTWINGS64", b'J')),
            RomProblem::Region("Japanese".into())
        );
        // The title match tolerates spaces ("Pilotwings64-ish").
        assert_eq!(
            diagnose(&header("PILOT WINGS64", b'P')),
            RomProblem::Region("European".into())
        );
        // Another game (right magic, different title).
        assert_eq!(
            diagnose(&header("MARIOKART64", b'E')),
            RomProblem::OtherGame("MARIOKART64".into())
        );
        // Garbage magic / too short: not an N64 ROM at all.
        assert_eq!(diagnose(b"not an n64 rom"), RomProblem::NotN64);
        assert_eq!(diagnose(&[0; 0x20]), RomProblem::NotN64);
    }

    #[test]
    fn diagnose_normalizes_swapped_byte_orders() {
        use super::super::RomProblem;
        // Byte-swap each format into .v64 (16-bit) and .n64 (32-bit) form;
        // the country byte and title must read the same afterwards.
        let be = header("PILOTWINGS64", b'P');
        let mut v64 = be.clone();
        v64.as_chunks_mut::<2>()
            .0
            .iter_mut()
            .for_each(|c| c.swap(0, 1));
        let mut n64 = be;
        n64.as_chunks_mut::<4>()
            .0
            .iter_mut()
            .for_each(|c| c.reverse());
        assert_eq!(diagnose(&v64), RomProblem::Region("European".into()));
        assert_eq!(diagnose(&n64), RomProblem::Region("European".into()));
    }
}
