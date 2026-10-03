//! The embedded build kit (T8, docs/notes/first-run-build.md): everything the
//! first-run module builder needs that is OUR code, packed into one
//! deterministic byte string the release exe ships inside itself:
//!
//! - `native/**`: our C + the shadow headers,
//! - `dylib/*`: the module shim and its imports header (plus the dev
//!   reference script),
//! - `ops.txt`: every patch from `patches/` in the context-free ops format
//!   (`ops`), generated at build time. No context or removed line of any
//!   patch travels (see the "Patches: legal note"): the pristine decomp file
//!   is pinned by the decomp manifest hash instead.
//!
//! This module is compiled twice from this one file: into the library (and
//! its tests) and, via `#[path]`, into pw64-cbuild's own `build.rs`, which
//! writes `$OUT_DIR/kit.bin` for `include_bytes!` (`pw64_cbuild::KIT`). One
//! source for packer and reader, so they cannot drift apart.
//!
//! The container is a flat framed list, not `tar`: the builder only needs
//! paths and bytes, and a fixed layout keeps the bytes stable across builds
//! (`write` sorts the entries and stores no timestamps), which the T10 ABI
//! hash and the cache key require.

use std::path::Path;

use sha2::{Digest, Sha256};

/// Container magic ("pw64 build kit, format 1").
pub const MAGIC: &[u8] = b"PW64KIT1";

/// The kit's ops file (the `file <path>` blocks of `ops`).
pub const OPS_PATH: &str = "ops.txt";

/// Serialises the kit: [`MAGIC`], then one `(path length, path, data length,
/// data)` record per entry, u32 little-endian, entries sorted by path so the
/// same inputs always give the same bytes (determinism: the T10 ABI string
/// hashes the kit, and the cache key must be stable across machines).
pub fn write(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut sorted: Vec<&(String, Vec<u8>)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = MAGIC.to_vec();
    for (path, data) in sorted {
        out.extend(u32::try_from(path.len()).unwrap().to_le_bytes());
        out.extend(path.as_bytes());
        out.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
        out.extend(data);
    }
    out
}

/// Parses [`write`]'s output into `(path, data)` entries (borrowed from the
/// kit bytes). Framing: one `(path length, path, data length, data)` record
/// per entry, exactly [`write`]'s layout. Paths are sanity-checked: they are
/// extracted to disk by the builder, so no separator tricks, absolute paths
/// or `..` are accepted.
pub fn parse(kit: &[u8]) -> Result<Vec<(&str, &[u8])>, String> {
    let Some(mut rest) = kit.strip_prefix(MAGIC) else {
        return Err("not a pw64 build kit".into());
    };
    let mut out = Vec::new();
    while !rest.is_empty() {
        let Some((path_len, r)) = take_u32(rest) else {
            return Err("truncated path length".into());
        };
        rest = r;
        let Some((path_bytes, r)) = take(rest, path_len) else {
            return Err("truncated path".into());
        };
        rest = r;
        let Some((data_len, r)) = take_u32(rest) else {
            return Err("truncated data length".into());
        };
        rest = r;
        let Some((data, r)) = take(rest, data_len) else {
            return Err("truncated data".into());
        };
        rest = r;
        let path = std::str::from_utf8(path_bytes).map_err(|_| "non-UTF8 path".to_string())?;
        if path.contains('\\') || path.contains("..") || path.starts_with('/') {
            return Err(format!("unsafe kit path {path:?}"));
        }
        out.push((path, data));
    }
    Ok(out)
}

fn take_u32(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let (n, rest) = bytes.split_first_chunk::<4>()?;
    Some((u32::from_le_bytes(*n), rest))
}

fn take(bytes: &[u8], len: u32) -> Option<(&[u8], &[u8])> {
    bytes.split_at_checked(usize::try_from(len).ok()?)
}

/// Every file under `root` as a kit entry, path `<prefix>/<rel>` with forward
/// slashes (mirrors pw64-game build.rs's patch walking: a mirror of the
/// builder's on-disk layout).
pub fn collect_files(
    root: &Path,
    prefix: &str,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), String> {
    let read = std::fs::read_dir(root).map_err(|e| format!("{}: {e}", root.display()))?;
    for entry in read {
        let entry = entry.map_err(|e| format!("{}: {e}", root.display()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("{}: non-UTF8 name", root.display()))?;
        let path = entry.path();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            collect_files(&path, &rel, out)?;
        } else {
            let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            out.push((rel, data));
        }
    }
    Ok(())
}

/// Builds the kit's [`OPS_PATH`] from a `patches/` tree: every
/// `<rel>.patch` becomes one `file <rel>` block of context-free ops
/// (`ops::from_unified_diff` + `ops::to_text`), blocks sorted by path.
/// Deterministic, and free of any context or removed line by construction
/// (the ops carry only counts and `+` lines).
pub fn ops_text(patches: &Path) -> Result<Vec<u8>, String> {
    let mut blocks: Vec<(String, Vec<u8>)> = Vec::new();
    collect_patch_files(patches, "", &mut blocks)?;
    blocks.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (_, text) in blocks {
        out.extend(text);
    }
    Ok(out)
}

fn collect_patch_files(
    dir: &Path,
    rel: &str,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in read {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("{}: non-UTF8 name", dir.display()))?;
        let child_rel = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        let path = entry.path();
        if path.is_dir() {
            collect_patch_files(&path, &child_rel, out)?;
        } else if let Some(file_rel) = child_rel.strip_suffix(".patch") {
            let file_rel = file_rel.replace('\\', "/");
            let diff =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let ops = crate::ops::from_unified_diff(&diff);
            out.push((
                file_rel.clone(),
                crate::ops::to_text(&file_rel, &ops).into_bytes(),
            ));
        }
    }
    Ok(())
}

/// The module/exe ABI string (T10): `<pkg version>-<first 16 hex of
/// sha256(pw64_dll_imports.h + pw64_dll_shim.c + kit bytes)>`. Both sides
/// compute it from the same embedded kit: pw64-game's build.rs sets it as
/// `DLL_ABI` (the exe refuses a module whose `pw64_dll_abi()` differs) and
/// the builder passes it as `-DPW64_DLL_ABI` and uses it as the cache key's
/// ABI part, so any kit change (a shim or import-list edit) moves every
/// module to a new key and forces a rebuild.
///
/// `version` is the shared package version (`CARGO_PKG_VERSION` of the
/// workspace version): pw64-game's build.rs and pw64-cbuild's examples both
/// pass their own, which are equal by construction.
pub fn abi(version: &str, kit_bytes: &[u8]) -> String {
    let entries = parse(kit_bytes).unwrap_or_else(|e| panic!("embedded build kit invalid: {e}"));
    let entry = |path: &str| -> Vec<u8> {
        entries
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, d)| d.to_vec())
            .unwrap_or_else(|| panic!("embedded build kit: no {path}"))
    };
    let mut h = Sha256::new();
    h.update(entry("dylib/pw64_dll_imports.h"));
    h.update(entry("dylib/pw64_dll_shim.c"));
    h.update(kit_bytes);
    let digest: [u8; 32] = h.finalize().into();
    format!("{version}-{}", hex16(&digest))
}

/// First 16 hex characters of a SHA-256 digest (= its first 8 bytes).
fn hex16(digest: &[u8; 32]) -> String {
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}
