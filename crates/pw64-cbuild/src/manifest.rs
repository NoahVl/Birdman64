//! First-run decomp manifest (first-run-build.md, "Decomp download"): pins
//! every decomp file the release build compiles by SHA-256, plus the
//! Pilotwings64Decomp submodule commit and its codeload URL. The manifest
//! carries only hashes and paths (no decomp content), so it is safe to commit.
//!
//! Hashes are defined over CR-stripped bytes: our checkout may be CRLF
//! (`core.autocrlf`) while the GitHub archive is LF, and both must hash the
//! same.

use sha2::{Digest, Sha256};

use crate::SRC_DIRS;

/// Codeload zip URL prefix; the commit is appended
/// (`<url><commit>`, see first-run-build.md "Decomp download").
pub const CODELOAD_URL: &str = "https://codeload.github.com/gcsmith/Pilotwings64Decomp/zip/";

/// The committed manifest text (`decomp-manifest.txt`), embedded so the
/// first-run launcher and the example parse the same pins.
pub const DECOMP_MANIFEST: &str = include_str!("../decomp-manifest.txt");

/// Directories pinned by the manifest: every decomp source dir we compile
/// ([`SRC_DIRS`]) plus the whole `include/` tree.
pub fn pinned_dirs() -> Vec<&'static str> {
    let mut dirs: Vec<&'static str> = SRC_DIRS.to_vec();
    dirs.push("include");
    dirs
}

/// SHA-256 of `bytes` with every CR byte removed (see the module doc).
pub fn hash_cr_stripped(bytes: &[u8]) -> String {
    let stripped: Vec<u8> = bytes.iter().copied().filter(|b| *b != b'\r').collect();
    let mut hasher = Sha256::new();
    hasher.update(&stripped);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Parsed `decomp-manifest.txt` (generator: `examples/mkmanifest.rs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// 40-hex Pilotwings64Decomp commit the files were hashed at. Feeds the
    /// build key (first-run-build.md T5/T7) and the download URL.
    pub commit: String,
    /// Full codeload zip URL for `commit`.
    pub url: String,
    /// `(sha256, path)` per file, sorted by path. Paths are '/'-separated and
    /// relative to the archive root (the submodule root).
    pub entries: Vec<(String, String)>,
}

/// Whether `path` is a safe archive-relative path: non-empty, no absolute
/// prefix, no backslash, no `..` component, no empty component. The fetch
/// step (T6) rejects anything else the zip claims to contain.
pub fn is_safe_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return false;
    }
    path.split('/').all(|c| !c.is_empty() && c != "..")
}

/// Parses the manifest format: `commit <sha>`, `url <codeload url>`, then
/// one `<sha256> <path>` per file (blank lines ignored).
pub fn parse(text: &str) -> Result<Manifest, String> {
    let mut commit: Option<String> = None;
    let mut url: Option<String> = None;
    let mut entries = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let n = i + 1;
        if let Some(c) = line.strip_prefix("commit ") {
            if commit.is_some() {
                return Err(format!("manifest line {n}: duplicate commit"));
            }
            if c.len() != 40 || !c.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!("manifest line {n}: not a 40-hex commit"));
            }
            commit = Some(c.to_ascii_lowercase());
            continue;
        }
        if let Some(u) = line.strip_prefix("url ") {
            if url.is_some() {
                return Err(format!("manifest line {n}: duplicate url"));
            }
            url = Some(u.to_string());
            continue;
        }
        let Some((hash, path)) = line.split_once(' ') else {
            return Err(format!(
                "manifest line {n}: expected `commit <sha>`, `url <url>` or `<sha256> <path>`"
            ));
        };
        if hash.len() != 64
            || !hash
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            return Err(format!(
                "manifest line {n}: not a lowercase sha256: {hash:?}"
            ));
        }
        if !is_safe_path(path) {
            return Err(format!("manifest line {n}: unsafe path {path:?}"));
        }
        entries.push((hash.to_string(), path.to_string()));
    }
    Ok(Manifest {
        commit: commit.ok_or("manifest: missing `commit <sha>` line")?,
        url: url.ok_or("manifest: missing `url <codeload url>` line")?,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
commit 2529dabfb5f5776818e78d06f7976631d1823fbe
url https://codeload.github.com/gcsmith/Pilotwings64Decomp/zip/2529dabfb5f5776818e78d06f7976631d1823fbe
0000000000000000000000000000000000000000000000000000000000000000 src/kernel/bootproc.c
";

    #[test]
    fn parse_ok() {
        let m = parse(SAMPLE).unwrap();
        assert_eq!(m.commit, "2529dabfb5f5776818e78d06f7976631d1823fbe");
        assert_eq!(m.url, format!("{CODELOAD_URL}{}", m.commit));
        assert_eq!(
            m.entries,
            vec![("0".repeat(64), "src/kernel/bootproc.c".to_string())]
        );
        assert!(pinned_dirs().contains(&"include"));
    }

    #[test]
    fn parse_accepts_crlf() {
        // Windows checkouts are checked out with CRLF (core.autocrlf): the
        // manifest file on disk must parse identically to the LF text.
        let crlf = SAMPLE.replace('\n', "\r\n");
        assert_ne!(crlf, SAMPLE);
        assert_eq!(parse(&crlf).unwrap(), parse(SAMPLE).unwrap());
    }

    #[test]
    fn parse_rejects() {
        for bad in [
            "0000000000000000000000000000000000000000000000000000000000000000 ../evil.c",
            "0000000000000000000000000000000000000000000000000000000000000000 /abs.c",
            "0000000000000000000000000000000000000000000000000000000000000000 src\\x.c",
            "xyz src/kernel/bootproc.c",
            "url http://x",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        // hash path without the two header lines
        assert!(parse(&"0".repeat(64)).is_err());
    }
}
