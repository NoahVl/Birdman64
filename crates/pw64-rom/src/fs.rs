//! The "UV" ROM filesystem: a compressed index (`UVRM`/`TABL`) followed by a
//! run of IFF-style `FORM` files. Mirrors `uvMemInitBlockHdr` / `uvFileReadBlock`
//! in `decomp/src/kernel/{texture,filesystem}.c`.

use crate::{Rom, mio0, rom::be_u32};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::fmt;

/// ROM offset of the `UVRM` filesystem index (US release).
pub const INDEX_OFFSET_US: usize = 0x0DE720;
/// ROM offset of the first filesystem `FORM` (US release).
pub const FS_BASE_US: usize = 0x0DF5B0;

/// A four-character IFF tag such as `UVMD`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tag(pub [u8; 4]);

impl Tag {
    pub fn from_u32(v: u32) -> Self {
        Self(v.to_be_bytes())
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in &self.0 {
            let c = if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            };
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "'{self}'")
    }
}

impl Serialize for Tag {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// One `FORM` block (chunk), decompressed if it was stored as `GZIP`.
#[derive(Debug, Clone, Serialize)]
pub struct Block {
    pub tag: Tag,
    pub compressed: bool,
    #[serde(skip)]
    pub data: Vec<u8>,
    pub size: usize,
}

/// A parsed `FORM` file.
#[derive(Debug, Clone, Serialize)]
pub struct Form {
    pub tag: Tag,
    pub blocks: Vec<Block>,
}

impl Form {
    /// Parses the `FORM` at `offset` in `bytes`, decompressing `GZIP` blocks.
    pub fn parse(bytes: &[u8], offset: usize) -> Result<Self> {
        let magic = Tag::from_u32(be_u32(bytes, offset)?);
        ensure!(
            magic.0 == *b"FORM",
            "expected FORM at {offset:#x}, found {magic}"
        );
        let end = offset + be_u32(bytes, offset + 4)? as usize + 8;
        let tag = Tag::from_u32(be_u32(bytes, offset + 8)?);
        let mut pos = offset + 0xC;
        let mut blocks = Vec::new();
        while pos < end {
            let btag = Tag::from_u32(be_u32(bytes, pos)?);
            let size = be_u32(bytes, pos + 4)? as usize;
            let data = bytes
                .get(pos + 8..pos + 8 + size)
                .with_context(|| format!("block {btag} at {pos:#x} runs past end"))?;
            let block = if btag.0 == *b"GZIP" {
                let inner = Tag::from_u32(be_u32(data, 0)?);
                let out_len = be_u32(data, 4)? as usize;
                let out = mio0::decompress(&data[8..])
                    .with_context(|| format!("decompressing {inner} block at {pos:#x}"))?;
                ensure!(out.len() == out_len, "GZIP size mismatch at {pos:#x}");
                Block {
                    tag: inner,
                    compressed: true,
                    size: out.len(),
                    data: out,
                }
            } else {
                Block {
                    tag: btag,
                    compressed: false,
                    size,
                    data: data.to_vec(),
                }
            };
            blocks.push(block);
            pos += 8 + size;
        }
        Ok(Self { tag, blocks })
    }

    pub fn block(&self, tag: &[u8; 4]) -> Option<&Block> {
        self.blocks.iter().find(|b| b.tag.0 == *tag)
    }
}

/// An entry in the filesystem index.
#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    /// Position in the index (global file number).
    pub index: usize,
    /// Position among files of the same tag (the engine's per-type id).
    pub type_index: usize,
    pub tag: Tag,
    pub rom_offset: usize,
    pub size: usize,
}

pub struct Filesystem<'a> {
    rom: &'a Rom,
    pub entries: Vec<FileEntry>,
}

impl<'a> Filesystem<'a> {
    pub fn open(rom: &'a Rom) -> Result<Self> {
        let index =
            Form::parse(rom.bytes(), INDEX_OFFSET_US).context("reading filesystem index")?;
        ensure!(
            index.tag.0 == *b"UVRM",
            "filesystem index has tag {}",
            index.tag
        );
        let tabl = index
            .block(b"TABL")
            .context("filesystem index has no TABL")?;

        let mut entries = Vec::new();
        let mut counts = std::collections::HashMap::<Tag, usize>::new();
        let mut rom_offset = FS_BASE_US;
        for pair in tabl.data.as_chunks::<8>().0 {
            let tag = Tag::from_u32(be_u32(pair, 0)?);
            let size = be_u32(pair, 4)? as usize;
            if tag.0 != [0; 4] {
                let n = counts.entry(tag).or_default();
                entries.push(FileEntry {
                    index: entries.len(),
                    type_index: *n,
                    tag,
                    rom_offset,
                    size,
                });
                *n += 1;
            }
            rom_offset += size;
        }
        Ok(Self { rom, entries })
    }

    pub fn read(&self, entry: &FileEntry) -> Result<Form> {
        let form = Form::parse(self.rom.bytes(), entry.rom_offset)
            .with_context(|| format!("file #{} ({})", entry.index, entry.tag))?;
        ensure!(
            form.tag == entry.tag,
            "file #{} tag mismatch: index says {}, FORM says {}",
            entry.index,
            entry.tag,
            form.tag
        );
        Ok(form)
    }

    pub fn raw(&self, entry: &FileEntry) -> Result<&'a [u8]> {
        self.rom.slice(entry.rom_offset, entry.size)
    }
}
