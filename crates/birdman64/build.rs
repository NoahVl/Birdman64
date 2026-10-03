//! Links the exe below 4 GB with pw64-game's fixed-base link args (it can't
//! apply them to other crates' binaries itself). See pw64-game/build.rs.
//! Also exports the short git commit as `PW64_GIT_HASH` for `--version`
//! and the crash log.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let args = std::env::var("DEP_PW64GAME_LOW_BASE_LINK_ARGS")
        .expect("pw64-game must export low_base_link_args");
    for a in args.split_whitespace() {
        println!("cargo:rustc-link-arg-bins={a}");
    }
    println!("cargo:rerun-if-env-changed=DEP_PW64GAME_LOW_BASE_LINK_ARGS");
    let hash = git_hash();
    println!("cargo:rustc-env=PW64_GIT_HASH={hash}");
    windows_resource();
}

/// Embeds the app icon + version metadata as a Windows resource
/// (`pw64.rc`: icon + VERSIONINFO). Keyed on the *target* OS: build
/// scripts see `CARGO_CFG_TARGET_OS`, while `cfg!(windows)` would describe
/// the host and break `cargo clippy --target x86_64-unknown-linux-gnu`
/// (and any Linux-host build). `.manifest_optional()`: the icon is
/// cosmetic, so a machine without the Windows SDK's rc.exe (embed-resource
/// has no llvm-rc fallback on Windows hosts; only cross builds use it)
/// builds without icon/metadata instead of failing.
fn windows_resource() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").expect("build scripts have CARGO_PKG_VERSION");
    let mut v = version.split('.').map(|n| n.parse::<u32>().unwrap_or(0));
    let (major, minor, patch) = (
        v.next().unwrap_or(0),
        v.next().unwrap_or(0),
        v.next().unwrap_or(0),
    );
    println!("cargo:rerun-if-changed=pw64.rc");
    println!("cargo:rerun-if-changed=../../assets/icon/birdman64.ico");
    // The .rc stringizes VER_VERSION (rc does not expand macros inside
    // quoted strings); the numeric macros go straight into FILEVERSION.
    let macros = [
        format!("VER_MAJOR={major}"),
        format!("VER_MINOR={minor}"),
        format!("VER_PATCH={patch}"),
        format!("VER_VERSION={version}"),
    ];
    embed_resource::compile("pw64.rc", &macros)
        .manifest_optional()
        .expect("pw64.rc failed to compile");
}

/// Short hash of the current commit (`git rev-parse --short=9 HEAD`),
/// or "unknown" when git or the repo is unavailable. Never fails the
/// build. Reruns when `.git/HEAD` or the current ref file changes, so a
/// new commit refreshes the hash.
fn git_hash() -> String {
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("build scripts have CARGO_MANIFEST_DIR"),
    );
    // The build script's cwd is the crate dir; walk up to the repo root.
    let mut dir = manifest.clone();
    let repo = loop {
        let git = dir.join(".git");
        if git.join("HEAD").is_file() {
            break git;
        }
        dir = dir.parent().map(Path::to_path_buf).unwrap_or_default();
        if dir.as_os_str().is_empty() {
            return "unknown".to_string();
        }
    };
    // Rebuild when HEAD moves (branch switch, detached, initial clone).
    println!(
        "cargo:rerun-if-changed={}",
        rel_to_crate(&manifest, &repo.join("HEAD"))
    );
    // A commit on the current branch only rewrites the ref file, not HEAD:
    // watch it too, whichever source answers below.
    if let Some(r) = std::fs::read_to_string(repo.join("HEAD"))
        .ok()
        .and_then(|h| h.trim().strip_prefix("ref: ").map(|r| repo.join(r.trim())))
        .filter(|f| f.is_file())
    {
        println!("cargo:rerun-if-changed={}", rel_to_crate(&manifest, &r));
    }
    // `git` is the primary source (it resolves packed refs too).
    if let Ok(out) = Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .output()
        && out.status.success()
        && let Ok(s) = String::from_utf8(out.stdout)
        && !s.trim().is_empty()
    {
        return s.trim().to_string();
    }
    // No git on PATH: fall back to reading the ref file (rare).
    let Ok(head) = std::fs::read_to_string(repo.join("HEAD")) else {
        return "unknown".to_string();
    };
    let head = head.trim();
    let Some(r) = head.strip_prefix("ref: ") else {
        return head.to_string(); // detached HEAD: the hash itself
    };
    let r = r.trim();
    let ref_file = repo.join(r);
    if ref_file.is_file() {
        println!(
            "cargo:rerun-if-changed={}",
            rel_to_crate(&manifest, &ref_file)
        );
        if let Ok(h) = std::fs::read_to_string(&ref_file) {
            let h = h.trim();
            if !h.is_empty() {
                return h.chars().take(9).collect();
            }
        }
    }
    "unknown".to_string()
}

/// `path` relative to the build script's cwd (the crate dir), as the
/// rerun-if-changed paths are interpreted.
fn rel_to_crate(manifest: &Path, path: &Path) -> String {
    path.strip_prefix(manifest)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}
