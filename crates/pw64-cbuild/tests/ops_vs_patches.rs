//! T4 acceptance (first-run-build.md): every patch in pw64-game/patches,
//! converted to ops and applied, must produce exactly the strict
//! unified-diff applier's result on the pristine decomp file (compared CR
//! stripped). The serialized ops form is also checked to carry no ' '
//! (context) or '-' (removed) text lines: only `file <path>`, `=n`, `-n` and
//! `+text` may appear, so no decomp content leaks into the shipped kit.
//! Skipped when the decomp submodule is not checked out.

use pw64_cbuild::{apply_unified_diff, ops};
use std::fs;
use std::path::{Path, PathBuf};

/// Every `.patch` under `dir`, recursively.
fn patch_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let read = fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in read {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            patch_files(&entry.path(), out);
        } else if entry.path().extension().is_some_and(|e| e == "patch") {
            out.push(entry.path());
        }
    }
}

/// Whether `line` is `<tag><digits>` (a positive count, no text).
fn is_count(line: &str, tag: char) -> bool {
    match line.strip_prefix(tag) {
        Some(digits) => !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

#[test]
fn ops_match_unified_diff_on_every_patch() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let patches = crate_dir.parent().unwrap().join("pw64-game/patches");
    let decomp = crate_dir.parent().unwrap().parent().unwrap().join("decomp");
    assert!(patches.is_dir(), "missing {}", patches.display());
    if !decomp.join("src").is_dir() {
        eprintln!("skipping ops_match_unified_diff_on_every_patch: decomp/ is not checked out");
        return;
    }

    let mut files = Vec::new();
    patch_files(&patches, &mut files);
    files.sort();
    assert!(!files.is_empty(), "no patches found");

    for patch in &files {
        let rel = patch
            .strip_prefix(&patches)
            .unwrap()
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let pristine = decomp.join(&rel);
        let original = fs::read_to_string(&pristine)
            .unwrap_or_else(|e| panic!("{rel}: reading pristine decomp file: {e}"));
        let diff =
            fs::read_to_string(patch).unwrap_or_else(|e| panic!("{rel}: reading patch: {e}"));

        let ops = ops::from_unified_diff(&diff);
        let via_ops = ops::apply(&original, &ops).unwrap_or_else(|e| panic!("{rel}: {e}"));
        let via_diff = apply_unified_diff(&original, &diff)
            .unwrap_or_else(|e| panic!("{rel}: apply_unified_diff: {e}"));
        assert_eq!(
            via_ops.replace('\r', ""),
            via_diff.replace('\r', ""),
            "{rel}: ops do not reproduce the unified-diff result"
        );

        // Serialization: only `file <path>` / `=n` / `-n` / `+text` lines, so
        // no ' ' or '-' text lines (decomp content) can be in the kit.
        let text = ops::to_text(&rel, &ops);
        for line in text.lines() {
            let is_op = line.starts_with("file ")
                || line.starts_with('+')
                || is_count(line, '=')
                || is_count(line, '-');
            assert!(
                is_op,
                "{rel}: serialized ops line {line:?} is not `file <path>`/`=n`/`-n`/`+text`: \
                 context or removed text would leak decomp content"
            );
        }
        let reparsed = ops::from_text(&text).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert_eq!(reparsed.len(), 1, "{rel}: one file per ops block");
        assert_eq!(reparsed[0].0, rel);
        let via_text =
            ops::apply(&original, &reparsed[0].1).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert_eq!(
            via_text.replace('\r', ""),
            via_diff.replace('\r', ""),
            "{rel}: serialized ops do not round trip"
        );
    }
}
