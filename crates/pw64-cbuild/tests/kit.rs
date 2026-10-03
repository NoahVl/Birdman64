//! T8 acceptance (first-run-build.md): the embedded kit. Two properties:
//!
//! 1. **Determinism + no drift**: regenerating the kit from
//!    `crates/pw64-game` (`native/**`, `dylib/*`, ops from `patches/`) with
//!    the library's own packer gives exactly the embedded bytes. The
//!    pw64-cbuild build.rs generator compiles the same `kit.rs`/`ops.rs`
//!    sources, so the bytes match only if the kit is what the sources say
//!    and no timestamps or filesystem order leak in (the T10 ABI hash and
//!    cache key depend on that).
//! 2. **Legal regression**: no context (`' '`) or removed (`'-'`) line of
//!    any patch (trimmed, >= 12 chars, not only braces/punctuation) may
//!    appear anywhere in the kit, except two documented cases: the exact
//!    text may also be a `+` line of some patch (the notes' "minimal,
//!    unavoidable" restatements, e.g. reordered bitfields), or it may also
//!    sit in our own `native/**`/`dylib/**` sources as committed (interface
//!    facts such as `#include <uv_memory.h>`; our files are T16's audit,
//!    not the ops mechanism's). Every kit entry path must be ours:
//!    `native/`, `dylib/` or the ops file, never a decomp `src/`/`include/`
//!    file (our shadow headers live under `native/include`).

use pw64_cbuild::kit;
use std::fs;
use std::path::{Path, PathBuf};

fn game_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../pw64-game")
}

/// Regenerates the kit exactly like pw64-cbuild's build.rs.
fn regenerate() -> Vec<u8> {
    let game = game_dir();
    let mut entries = Vec::new();
    kit::collect_files(&game.join("native"), "native", &mut entries).unwrap();
    kit::collect_files(&game.join("dylib"), "dylib", &mut entries).unwrap();
    entries.push((
        kit::OPS_PATH.to_string(),
        kit::ops_text(&game.join("patches")).unwrap(),
    ));
    kit::write(&entries)
}

#[test]
fn embedded_kit_is_deterministic() {
    assert_eq!(regenerate(), pw64_cbuild::KIT);
}

/// Diff furniture, never decomp text: headers and hunk markers.
fn is_diff_furniture(line: &str) -> bool {
    line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("@@")
        || line.starts_with("diff ")
        || line.starts_with("index ")
        || line.starts_with("old mode")
        || line.starts_with("new file")
        || line.starts_with('\\')
}

#[test]
fn kit_has_no_decomp_lines_or_files() {
    let kit_bytes = pw64_cbuild::KIT;

    // No decomp file may ship as a kit entry: paths are ours only (the shadow
    // headers live under native/include, our C under native/src).
    let entries = kit::parse(kit_bytes).expect("kit parses");
    assert!(!entries.is_empty());
    for (path, _) in &entries {
        assert!(
            *path == kit::OPS_PATH || path.starts_with("native/") || path.starts_with("dylib/"),
            "kit entry {path:?}: not our code (a decomp file?)"
        );
    }

    // Every entry is our text (the container's length fields are binary, so
    // the raw kit bytes are not searchable as text): a leaked decomp line
    // would sit inside one of them.
    let texts: Vec<String> = entries
        .iter()
        .map(|(path, data)| {
            std::str::from_utf8(data)
                .unwrap_or_else(|e| panic!("{path}: not text: {e}"))
                .to_string()
        })
        .collect();

    // The ops format only carries `+` lines, and the notes ("Patches: legal
    // note") flag a few as unavoidable restatements of a decomp statement
    // (reordered bitfields, moved code): a context/removed line whose text is
    // also a `+` line of some patch is one of those and may travel. Our own
    // native/dylib sources legitimately name the same interfaces (e.g.
    // `#include <uv_memory.h>`), so lines they contain are facts too (their
    // full content is T16's audit, not this test's). Anything else leaking
    // would mean the ops carry real context or removed lines.
    let mut patches = Vec::new();
    collect_patches(&game_dir().join("patches"), &mut patches);
    patches.sort();
    assert!(!patches.is_empty(), "no patches found");
    let mut insert_lines: Vec<String> = Vec::new();
    for patch in &patches {
        let diff = fs::read_to_string(patch).unwrap_or_else(|e| panic!("{}: {e}", patch.display()));
        for line in diff.lines() {
            let line = line.trim_end_matches('\r');
            if let Some(text) = line.strip_prefix('+') {
                insert_lines.push(text.trim().to_string());
            }
        }
    }
    // The lines of our own committed sources (the same files the kit packs).
    let mut own = Vec::new();
    for root in ["native", "dylib"] {
        collect_files_lines(&game_dir().join(root), &mut own);
    }

    let mut checked = 0usize;
    let mut restated = 0usize;
    for patch in &patches {
        let diff = fs::read_to_string(patch).unwrap_or_else(|e| panic!("{}: {e}", patch.display()));
        for line in diff.lines() {
            let line = line.trim_end_matches('\r');
            if is_diff_furniture(line) {
                continue;
            }
            let Some(text) = line.strip_prefix([' ', '-']) else {
                continue; // '+' lines are ours and may travel
            };
            let text = text.trim();
            if text.len() < 12 {
                continue;
            }
            // Not only braces/punctuation: some alphanumeric must remain.
            if text.trim_matches(|c: char| !c.is_alphanumeric()).is_empty() {
                continue;
            }
            checked += 1;
            if insert_lines.iter().any(|t| t.contains(text)) {
                // Unavoidable restatement: the ops may re-insert this exact
                // line, possibly with a leading qualifier or cast (counted;
                // the T16 audit owns the shipped-text budget).
                restated += 1;
                // Listed for the T16 audit (`-- --nocapture`).
                eprintln!("restated: {}: {text}", patch.display());
                continue;
            }
            if own.iter().any(|t| t == text) {
                // An interface fact our own sources also state (an include
                // line, a signature): not patch-derived content.
                continue;
            }
            assert!(
                texts.iter().all(|t| !t.contains(text)),
                "{}: decomp line {text:?} leaked into the kit",
                patch.display()
            );
        }
    }
    eprintln!(
        "checked {checked} context/removed patch lines against the kit \
         ({restated} restated as '+' lines, exempt)"
    );
    assert!(checked > 1000, "{checked}: implausibly few lines checked");
}

/// Every `.patch` under `dir`, recursively (mirrors tests/ops_vs_patches.rs).
fn collect_patches(dir: &Path, out: &mut Vec<PathBuf>) {
    let read = fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in read {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect_patches(&entry.path(), out);
        } else if entry.path().extension().is_some_and(|e| e == "patch") {
            out.push(entry.path());
        }
    }
}

/// Every line (trimmed) of every file under `dir`, recursively.
fn collect_files_lines(dir: &Path, out: &mut Vec<String>) {
    let read = fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in read {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            collect_files_lines(&entry.path(), out);
        } else {
            let text = fs::read_to_string(entry.path())
                .unwrap_or_else(|e| panic!("{}: {e}", entry.path().display()));
            out.extend(text.lines().map(|l| l.trim().to_string()));
        }
    }
}
