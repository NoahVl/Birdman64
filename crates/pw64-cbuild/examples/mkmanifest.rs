//! Regenerates `crates/pw64-cbuild/decomp-manifest.txt` from a
//! Pilotwings64Decomp checkout: one SHA-256 per file under the pinned dirs
//! ([`pw64_cbuild::manifest::pinned_dirs`], CR stripped so our CRLF checkout
//! and the LF GitHub archive hash the same), at the submodule's HEAD commit.
//!
//! ```sh
//! cargo run -p pw64-cbuild --example mkmanifest
//! ```
//!
//! Rerun after moving the submodule to a new commit; the
//! `manifest_matches_submodule` test fails on a stale manifest.

use pw64_cbuild::manifest::{self, CODELOAD_URL};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() -> Result<(), String> {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = crate_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("cannot find the repo root")?
        .to_path_buf();
    let decomp = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.join("decomp"));
    let out = crate_dir.join("decomp-manifest.txt");

    let commit = git_head(&decomp)?;
    let mut files = Vec::new();
    for dir in manifest::pinned_dirs() {
        collect(&decomp.join(dir), &mut files)?;
    }
    files.sort();
    if files.is_empty() {
        return Err(format!(
            "no files found under {:?} in {decomp:?}",
            manifest::pinned_dirs()
        ));
    }

    let mut text = format!("commit {commit}\nurl {CODELOAD_URL}{commit}\n");
    for f in &files {
        let rel = f
            .strip_prefix(&decomp)
            .map_err(|_| format!("{}: not under {}", f.display(), decomp.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = fs::read(f).map_err(|e| format!("{}: {e}", f.display()))?;
        text.push_str(&format!("{} {rel}\n", manifest::hash_cr_stripped(&bytes)));
    }
    fs::write(&out, &text).map_err(|e| format!("{}: {e}", out.display()))?;
    println!(
        "wrote {} ({} files at commit {commit})",
        out.display(),
        files.len()
    );
    Ok(())
}

/// `git -C decomp rev-parse HEAD`: the commit the manifest pins.
fn git_head(decomp: &Path) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(decomp)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git rev-parse HEAD failed in {}: {}",
            decomp.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if head.len() != 40 || !head.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("unexpected git rev-parse output: {head:?}"));
    }
    Ok(head)
}

/// Recursive file walk; symlinks are skipped (none live under the pinned dirs
/// today, but the archive extractor skips them too).
fn collect(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let read = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in read {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect(&entry.path(), files)?;
        } else {
            files.push(entry.path());
        }
    }
    Ok(())
}
