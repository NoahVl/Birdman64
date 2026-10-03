//! T5 acceptance (first-run-build.md): the committed decomp-manifest.txt
//! must match the checked-out decomp submodule exactly (same commit, same
//! file set, same CR-stripped hashes). On a mismatch the dev reruns
//! `cargo run -p pw64-cbuild --example mkmanifest`. Skipped when the
//! submodule is not checked out.

use pw64_cbuild::manifest;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const MANIFEST: &str = include_str!("../decomp-manifest.txt");

/// Recursive file walk under one of the pinned dirs; symlinks skipped.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let read = fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in read {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect(&entry.path(), out);
        } else {
            out.push(entry.path());
        }
    }
}

#[test]
fn manifest_matches_submodule() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let decomp = crate_dir.parent().unwrap().parent().unwrap().join("decomp");
    if !decomp.join("src").is_dir() || !decomp.join("include").is_dir() {
        eprintln!("skipping manifest_matches_submodule: decomp/ is not checked out");
        return;
    }

    let m = manifest::parse(MANIFEST).expect("decomp-manifest.txt parses");
    assert!(
        m.url.ends_with(&m.commit),
        "manifest url does not end in the commit: {}",
        m.url
    );

    let mut expected = Vec::new();
    for dir in manifest::pinned_dirs() {
        collect(&decomp.join(dir), &mut expected);
    }
    expected.sort();
    let expected: Vec<(String, String)> = expected
        .iter()
        .map(|p| {
            let rel = p
                .strip_prefix(&decomp)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = fs::read(p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
            (manifest::hash_cr_stripped(&bytes), rel)
        })
        .collect();
    assert_eq!(
        m.entries.len(),
        expected.len(),
        "decomp-manifest.txt is stale (entry count): rerun `cargo run -p pw64-cbuild --example mkmanifest`"
    );
    assert_eq!(
        m.entries, expected,
        "decomp-manifest.txt is stale (hashes or paths): rerun `cargo run -p pw64-cbuild --example mkmanifest`"
    );

    // Commit line = the submodule's HEAD.
    match Command::new("git")
        .arg("-C")
        .arg(&decomp)
        .args(["rev-parse", "HEAD"])
        .output()
    {
        Ok(out) if out.status.success() => {
            let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
            assert_eq!(
                m.commit, head,
                "manifest commit is stale: rerun `cargo run -p pw64-cbuild --example mkmanifest`"
            );
        }
        _ => eprintln!("manifest commit check skipped: git unavailable"),
    }
}
