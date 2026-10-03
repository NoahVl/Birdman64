//! First-run decomp download (T6, docs/notes/first-run-build.md "Decomp
//! download"): fetches the pinned Pilotwings64Decomp commit from the codeload
//! URL, verifies every file against the committed manifest and writes the
//! verified tree to `cache/decomp/<commit>/` (plus a `.complete` marker).
//!
//! Dev overrides swap the network for local bytes; the hash check always
//! runs:
//!
//! - `PW64_DECOMP_ZIP=<zip>`: verify + extract this archive.
//! - `PW64_DECOMP_DIR=<tree>`: verify an existing tree (dev: `decomp/`).
//!
//! Per-file SHA-256 over CR-stripped bytes (both sides), not a zip hash:
//! GitHub regenerates archive bytes. Tests use a synthetic in-memory zip.

use crate::manifest::{self, Manifest};
use crate::{Progress, Step};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use zip::ZipArchive;

/// `User-Agent` for the codeload request (GitHub wants one).
const USER_AGENT: &str = concat!(
    "Birdman64/",
    env!("CARGO_PKG_VERSION"),
    " (Pilotwings64 first-run builder)"
);

/// Hard cap on the downloaded archive (~1 MB expected): the 10 MB body limit
/// of `read_to_vec` is plenty, this only rejects absurd responses early.
const MAX_ZIP_BYTES: usize = 32 << 20;

/// Cap on one extracted manifest file (inflated size).
const MAX_ENTRY_BYTES: u64 = 16 << 20;

/// A codeload/extract failure. Variants map to the player-facing messages in
/// the notes ("Steps + UX"): `Network` = check the connection, `Http` =
/// GitHub itself refused the file (a report-worthy surprise), `HashMismatch`
/// = wrong version, the rest name a local problem.
#[derive(Debug)]
pub enum FetchError {
    /// Download failed after all retries (sleeps 1 s, 4 s between attempts).
    Network(String),
    /// GitHub answered with a non-success status (404, 403, 451, ...):
    /// not a connection problem, so there is nothing to retry.
    Http { code: u16 },
    /// A file's bytes don't match the manifest hash: corrupted download or
    /// stale manifest.
    HashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    /// The archive or the manifest is structurally bad (unsafe path, missing
    /// manifest file, not a zip).
    Archive(String),
    /// Local filesystem error, pre-formatted with context.
    Io(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Network(e) => write!(f, "could not download the decomp source: {e}"),
            FetchError::Http { code } => {
                write!(f, "GitHub refused the decomp download (HTTP {code})")
            }
            FetchError::HashMismatch {
                path,
                expected,
                actual,
            } => write!(
                f,
                "the downloaded source doesn't match the expected version: {path} \
                 (sha256 {actual}, manifest {expected})"
            ),
            FetchError::Archive(e) => write!(f, "bad decomp archive: {e}"),
            FetchError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FetchError {}

/// The verified decomp tree for `manifest`: cache hit (`.complete` marker),
/// env override, or network download + verify + extract. Returns the tree
/// directory (the commit is `manifest.commit`).
pub fn decomp_tree(
    manifest: &Manifest,
    cache_root: &Path,
    progress: &mut dyn FnMut(Progress),
) -> Result<PathBuf, FetchError> {
    // Dev override: an existing tree, still hash-checked.
    if let Some(dir) = std::env::var_os("PW64_DECOMP_DIR") {
        let dir = PathBuf::from(dir);
        verify_tree(&dir, manifest)?;
        return Ok(dir);
    }
    let tree = cache_root.join("decomp").join(&manifest.commit);
    if tree.join(".complete").is_file() {
        // Re-hash anyway (4 MB, only when a module is being built): a crash
        // or power loss, or a user edit, must not feed a damaged tree to the
        // compiler. A bad tree is replaced by a fresh download.
        if verify_tree(&tree, manifest).is_ok() {
            return Ok(tree);
        }
        remove_dir(&tree)?;
    }
    let bytes = if let Some(zip_path) = std::env::var_os("PW64_DECOMP_ZIP") {
        std::fs::read(&zip_path).map_err(|e| {
            FetchError::Io(format!("reading {}: {e}", Path::new(&zip_path).display()))
        })?
    } else {
        let bytes = download(&manifest.url, progress)?;
        (progress)(Progress {
            step: Step::Download,
            fraction: 0.15,
        });
        bytes
    };
    // Extract into a per-process dir, then rename: a crash mid-extract leaves
    // no half tree at `tree`, and two instances starting at once never read
    // files the other is still writing.
    let tmp =
        cache_root
            .join("decomp")
            .join(format!("{}.tmp-{}", manifest.commit, std::process::id()));
    remove_dir(&tmp)?;
    let r = verify_and_extract(&bytes, manifest, &tmp, progress);
    if let Err(e) = r {
        let _ = remove_dir(&tmp);
        return Err(e);
    }
    if tree.join(".complete").is_file() {
        // Another instance finished the same verified tree meanwhile (and
        // may be compiling from it now): keep it.
        let _ = remove_dir(&tmp);
        return Ok(tree);
    }
    if tree.exists() {
        // A partial tree from an older layout / interrupted extract.
        remove_dir(&tree)?;
    }
    if let Err(e) = crate::build::retry_fs(|| std::fs::rename(&tmp, &tree)) {
        let _ = remove_dir(&tmp);
        // Another instance won the race with an identical verified tree.
        if !tree.join(".complete").is_file() {
            return Err(FetchError::Io(format!("moving to {}: {e}", tree.display())));
        }
    }
    Ok(tree)
}

/// Deletes `p` if it exists (retry: antivirus holds freshly extracted files
/// for a moment).
fn remove_dir(p: &Path) -> Result<(), FetchError> {
    match crate::build::retry_fs(|| std::fs::remove_dir_all(p)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(FetchError::Io(format!("deleting {}: {e}", p.display()))),
    }
}

/// Downloads the codeload zip: 60 s timeout, `User-Agent`, proxy env vars
/// (honoured by ureq's default agent config), 3 attempts with 1 s / 4 s
/// sleeps in between (the notes' automatic retries).
fn download(url: &str, progress: &mut dyn FnMut(Progress)) -> Result<Vec<u8>, FetchError> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent();
    let mut last = String::new();
    for (attempt, wait) in [0u32, 1, 4].into_iter().enumerate() {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(wait as u64));
        }
        (progress)(Progress {
            step: Step::Download,
            fraction: 0.05,
        });
        match agent.get(url).header("User-Agent", USER_AGENT).call() {
            Ok(mut response) => match response.body_mut().read_to_vec() {
                Ok(bytes) if bytes.len() <= MAX_ZIP_BYTES => return Ok(bytes),
                Ok(_) => {
                    return Err(FetchError::Archive(format!(
                        "codeload zip from {url} is implausibly large"
                    )));
                }
                Err(e) => last = format!("reading the response failed: {e}"),
            },
            Err(e) => match e {
                // A status is an answer, not a network failure: no point
                // retrying a 404 (ureq's default http_status_as_error turns
                // every 4xx/5xx into this variant).
                ureq::Error::StatusCode(code) => {
                    return Err(FetchError::Http { code });
                }
                other => last = format!("{other}"),
            },
        }
    }
    Err(FetchError::Network(last))
}

/// Verifies every manifest entry in `zip_bytes` and extracts the manifest's
/// paths into `dest` (CR stripped, so the tree is byte-identical whether the
/// archive was LF or CRLF). Rejects unsafe paths, symlinks on manifest paths
/// and any manifest file the archive is missing.
pub fn verify_and_extract(
    zip_bytes: &[u8],
    manifest: &Manifest,
    dest: &Path,
    progress: &mut dyn FnMut(Progress),
) -> Result<(), FetchError> {
    (progress)(Progress {
        step: Step::VerifyExtract,
        fraction: 0.20,
    });
    let mut archive = ZipArchive::new(std::io::Cursor::new(zip_bytes)).map_err(|e| {
        FetchError::Archive(format!("the downloaded archive is not a valid zip: {e}"))
    })?;
    let expected: HashMap<&str, &str> = manifest
        .entries
        .iter()
        .map(|(h, p)| (p.as_str(), h.as_str()))
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| FetchError::Archive(format!("zip entry {i} cannot be read: {e}")))?;
        // Archive layout: one top directory per commit (`strip top dir`).
        let name = file.name().to_string();
        let Some(rel) = name.split_once('/').map(|(_, r)| r.to_string()) else {
            continue; // top-level entry: never a manifest path
        };
        // Directory entries (trailing `/`) carry no content: check their name
        // for traversal, then skip (the manifest has files only).
        let no_slash = rel.trim_end_matches('/');
        if no_slash.is_empty() {
            continue; // the top directory itself
        }
        // Reject `..`/absolute/traversal names before anything else (even
        // entries that are not manifest paths: no entry may be a traversal).
        if !manifest::is_safe_path(no_slash) {
            return Err(FetchError::Archive(format!("unsafe zip path {name:?}")));
        }
        if rel.ends_with('/') {
            continue;
        }
        let Some(want) = expected.get(rel.as_str()).copied() else {
            continue; // extracted paths only (notes): the rest is skipped
        };
        if file.is_symlink() {
            return Err(FetchError::Archive(format!(
                "manifest path {rel} is a symlink in the archive"
            )));
        }
        // Size cap before inflating: the header size is untrusted (zip bomb),
        // so read at most MAX_ENTRY_BYTES + 1 and reject anything larger (the
        // biggest pinned file is well under 1 MB).
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_ENTRY_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| FetchError::Archive(format!("reading zip entry {rel}: {e}")))?;
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            return Err(FetchError::Archive(format!(
                "zip entry {rel} is implausibly large"
            )));
        }
        let actual = manifest::hash_cr_stripped(&bytes);
        if actual != want {
            return Err(FetchError::HashMismatch {
                path: rel.clone(),
                expected: want.to_string(),
                actual,
            });
        }
        let path = dest.join(&rel);
        std::fs::create_dir_all(path.parent().unwrap_or(dest))
            .map_err(|e| FetchError::Io(format!("creating {}: {e}", path.display())))?;
        // Strip CR on disk too: the dev checkout is CRLF, the cache tree is
        // the archive's LF content (ops/apply and the compiler don't care).
        std::fs::write(&path, cr_stripped(&bytes))
            .map_err(|e| FetchError::Io(format!("writing {}: {e}", path.display())))?;
        seen.insert(rel);
    }
    let missing: Vec<String> = expected
        .keys()
        .filter(|p| !seen.contains(**p))
        .map(|p| p.to_string())
        .collect();
    if !missing.is_empty() {
        return Err(FetchError::Archive(format!(
            "the archive is missing manifest files, e.g. {}",
            missing.first().map(String::as_str).unwrap_or("")
        )));
    }
    std::fs::create_dir_all(dest)
        .map_err(|e| FetchError::Io(format!("creating {}: {e}", dest.display())))?;
    std::fs::write(dest.join(".complete"), b"verified\n")
        .map_err(|e| FetchError::Io(format!("writing {}/.complete: {e}", dest.display())))?;
    Ok(())
}

/// Hash-checks an existing tree (dev override `PW64_DECOMP_DIR`): every
/// manifest file must exist with the manifest's CR-stripped hash.
pub fn verify_tree(tree: &Path, manifest: &Manifest) -> Result<(), FetchError> {
    for (want, rel) in &manifest.entries {
        let path = tree.join(rel);
        let bytes = std::fs::read(&path)
            .map_err(|e| FetchError::Io(format!("reading {}: {e}", path.display())))?;
        let actual = manifest::hash_cr_stripped(&bytes);
        if actual != *want {
            return Err(FetchError::HashMismatch {
                path: path.display().to_string(),
                expected: want.clone(),
                actual,
            });
        }
    }
    Ok(())
}

/// `bytes` with every CR removed (see [`manifest::hash_cr_stripped`]).
fn cr_stripped(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().copied().filter(|b| *b != b'\r').collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::CODELOAD_URL;
    use std::io::Write;

    const COMMIT: &str = "2529dabfb5f5776818e78d06f7976631d1823fbe";

    fn quiet(_: Progress) {}

    /// A minimal manifest over `paths` with the real hashes of `contents`.
    fn manifest_of(entries: &[(&str, &str)]) -> Manifest {
        let mut text = format!("commit {COMMIT}\nurl {CODELOAD_URL}{COMMIT}\n");
        for (path, content) in entries {
            text.push_str(&format!(
                "{} {path}\n",
                manifest::hash_cr_stripped(content.as_bytes())
            ));
        }
        manifest::parse(&text).unwrap()
    }

    /// Builds an in-memory zip with the given members under the standard
    /// codeload top dir, plus an entry that is not a manifest path (skipped;
    /// the archive's real symlink, tools/diff.py, is not a manifest path and
    /// zip's writer can't emit S_IFLNK anyway).
    fn zip_of(members: &[(&str, &str)]) -> Vec<u8> {
        let top = format!("Pilotwings64Decomp-{COMMIT}/");
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, content) in members {
            w.start_file(format!("{top}{name}"), opts).unwrap();
            w.write_all(content.as_bytes()).unwrap();
        }
        w.start_file("tools/diff.py", opts).unwrap();
        w.write_all(b"not in the manifest\n").unwrap();
        w.finish().unwrap();
        buf.into_inner()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pw64-cbuild-fetch-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn synthetic_zip_extracts_and_verifies() {
        let m = manifest_of(&[
            (
                "src/kernel/bootproc.c",
                "int bootproc(void) { return 1; }\n",
            ),
            ("include/x.h", "#pragma once\n#define X 2\n"),
        ]);
        let zip = zip_of(&[
            (
                "src/kernel/bootproc.c",
                "int bootproc(void) { return 1; }\r\n",
            ),
            ("include/x.h", "#pragma once\n#define X 2\n"),
        ]);
        let dest = temp_dir("extract");
        verify_and_extract(&zip, &m, &dest, &mut quiet).unwrap();
        let c = std::fs::read_to_string(dest.join("src/kernel/bootproc.c")).unwrap();
        assert_eq!(c, "int bootproc(void) { return 1; }\n"); // CR stripped
        assert_eq!(
            std::fs::read_to_string(dest.join("include/x.h")).unwrap(),
            "#pragma once\n#define X 2\n"
        );
        assert!(dest.join(".complete").is_file());
        verify_tree(&dest, &m).unwrap();
        std::fs::remove_dir_all(&dest).unwrap();
    }

    #[test]
    fn zip_entry_with_dotdot_is_rejected() {
        let m = manifest_of(&[("src/evil.c", "x\n")]);
        for name in ["../evil.c", "a/../evil.c", "/evil.c"] {
            let zip = zip_of(&[(name, "x\n")]);
            let dest = temp_dir("dotdot");
            match verify_and_extract(&zip, &m, &dest, &mut quiet) {
                Err(FetchError::Archive(e)) => assert!(e.contains("unsafe"), "{e}"),
                other => panic!("expected Archive error for {name}, got {other:?}"),
            }
            std::fs::remove_dir_all(&dest).unwrap();
        }
    }

    #[test]
    fn directory_entries_are_skipped() {
        // Codeload archives have one `dir/` entry per directory.
        let m = manifest_of(&[("src/x.c", "x\n")]);
        let zip = zip_of(&[("src/", ""), ("src/x.c", "x\n")]);
        let dest = temp_dir("dirs");
        verify_and_extract(&zip, &m, &dest, &mut quiet).unwrap();
        assert!(dest.join("src/x.c").is_file());
        std::fs::remove_dir_all(&dest).unwrap();
    }

    #[test]
    fn wrong_hash_is_rejected() {
        let m = manifest_of(&[("src/x.c", "good\n")]);
        let zip = zip_of(&[("src/x.c", "tampered\n")]);
        let dest = temp_dir("hash");
        match verify_and_extract(&zip, &m, &dest, &mut quiet) {
            Err(FetchError::HashMismatch { path, .. }) => {
                assert!(path.ends_with("src/x.c"), "{path}");
            }
            other => panic!("expected HashMismatch, got {other:?}"),
        }
        std::fs::remove_dir_all(&dest).unwrap();
    }

    #[test]
    fn missing_manifest_file_is_rejected() {
        let m = manifest_of(&[("src/x.c", "x\n"), ("src/gone.c", "y\n")]);
        let zip = zip_of(&[("src/x.c", "x\n")]);
        let dest = temp_dir("missing");
        match verify_and_extract(&zip, &m, &dest, &mut quiet) {
            Err(FetchError::Archive(e)) => assert!(e.contains("missing manifest files"), "{e}"),
            other => panic!("expected Archive error, got {other:?}"),
        }
        std::fs::remove_dir_all(&dest).unwrap();
    }
}
