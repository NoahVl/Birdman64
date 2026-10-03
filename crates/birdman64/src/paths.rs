//! Where Birdman64 keeps its files: `pw64.toml`, the `pw64.eep` save and
//! `crash.log` all land in one directory (PLAN release blocker B4: the cwd
//! is not writable for a double-clicked exe and scattered files are a
//! support nightmare).
//!
//! `data_dir()` is decided once, in this order:
//! 1. `PW64_DATA_DIR`, when set (the directory is created when missing).
//! 2. Developer mode: a cargo build (the exe path has a `target` directory
//!    component) keeps today's behaviour (the current working directory),
//!    so checkouts and scripted runs are unaffected.
//! 3. Portable install: the exe's directory, when it holds one of the
//!    marker files `portable.txt` or `pw64.eep` (not `pw64.toml`, S4) and
//!    is writable (probed once by creating and deleting a temp file there;
//!    a refused one is explained by [`startup_notice`]).
//!    ux-1.0 D1: the marker means the portability is on purpose; a plain
//!    extraction has none, so it stays per-user and an update unzipped
//!    into a new folder keeps saves, settings and the remembered ROM.
//! 4. Per-user fallback: `%APPDATA%\Birdman64` (Windows) /
//!    `$XDG_DATA_HOME/birdman64`, else `$HOME/.local/share/birdman64`
//!    (Linux), created when missing.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Supplied by an embedding frontend before the first [`data_dir`] call: a
/// future Android build points this at its app storage dir. Later calls
/// (or ones after `data_dir` ran) are ignored (returns `false` then).
#[allow(dead_code)] // for the future frontend; nothing in the exe calls it yet
pub fn set_data_dir(p: PathBuf) -> bool {
    OVERRIDE.set(p).is_ok()
}

/// [`set_data_dir`]'s value; wins over every rule in [`compute`].
static OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// The directory `pw64.toml`, `pw64.eep` and `crash.log` live in (see the
/// module docs for the rule order). Never panics: every fallible step falls
/// back to the next rule, so it is safe inside the panic hook.
pub fn data_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(compute)
}

/// The first-run build cache (decomp download + the compiled game module,
/// first-run-build.md "Cache"): `<data dir>/cache`, except for the per-user
/// data dir (rule 4), whose roaming profile is no place for 20 MB of
/// rebuildable files: `%LOCALAPPDATA%\Birdman64\cache` /
/// `$XDG_CACHE_HOME/birdman64` (else `~/.cache/birdman64`) then.
/// XDG base spec: an override is honoured only when it is an absolute
/// path; an empty or relative value falls back to the default (S10 review:
/// saves otherwise followed the cwd). Pure, so tests can pin the rule
/// without touching the environment.
#[cfg(any(not(windows), feature = "first-run"))]
fn xdg_or_default(value: Option<PathBuf>, default: Option<PathBuf>) -> Option<PathBuf> {
    value.filter(|d| d.is_absolute()).or(default)
}

#[cfg(feature = "first-run")]
pub fn cache_dir() -> PathBuf {
    let data = data_dir();
    let local = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Birdman64").join("cache"))
    } else {
        // XDG base spec: only an absolute $XDG_CACHE_HOME overrides the
        // default; `None` (no valid variable, no $HOME) falls back to
        // <data dir>/cache below, as before.
        xdg_or_default(
            std::env::var_os("XDG_CACHE_HOME").map(|d| PathBuf::from(d).join("birdman64")),
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/birdman64")),
        )
    };
    match (per_user_dir(), local) {
        (Some(user), Some(local)) if user == data => local,
        _ => data.join("cache"),
    }
}

/// U14: the marker file for "this run is up" (`<data dir>/running.lock`),
/// created when the game thread starts and removed on a clean quit
/// (`window::quit`). Left behind after a crash or power loss, it triggers
/// the "didn't close properly" toast at the next start.
pub fn running_lock() -> PathBuf {
    data_dir().join("running.lock")
}

/// Opens `dir` in the system file manager (ux-1.0 U13 "Open log folder",
/// U14 "Open the folder now?"). Fire and forget: the child outlives us,
/// so a slow file manager never delays the exit, and failures are logged
/// only (there is nothing better to do once the report box is up).
pub fn open_in_file_manager(dir: &Path) {
    #[cfg(windows)]
    let opened = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(not(windows))]
    let opened = std::process::Command::new("xdg-open").arg(dir).spawn();
    if let Err(e) = opened {
        eprintln!("error: could not open {}: {e}", dir.display());
    }
}

/// The rule order applied to already-computed inputs, so tests can exercise
/// it without touching the real environment or filesystem. Side effects
/// (creating the env / per-user directory) are the caller's job. `marker`
/// says the exe dir holds a [`portable_marker`] file; `writable` is the
/// write probe result for it.
fn decide(
    env: Option<&Path>,
    exe: Option<&Path>,
    marker: bool,
    writable: bool,
    cwd: &Path,
    per_user: Option<&Path>,
) -> PathBuf {
    if let Some(p) = env {
        return p.to_path_buf();
    }
    if is_dev_build(exe) {
        return cwd.to_path_buf();
    }
    if marker
        && writable
        && let Some(dir) = exe.and_then(|e| e.parent())
    {
        return dir.to_path_buf();
    }
    if let Some(p) = per_user {
        return p.to_path_buf();
    }
    // Last resort for odd cases (no exe path, no home): the cwd. Developers
    // hit rule 2 before this; real installs hit 3 or 4.
    cwd.to_path_buf()
}

fn compute() -> PathBuf {
    if let Some(p) = OVERRIDE.get() {
        let _ = std::fs::create_dir_all(p);
        return p.clone();
    }
    let env = std::env::var_os("PW64_DATA_DIR").map(PathBuf::from);
    let exe = std::env::current_exe().ok();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = exe.as_ref().and_then(|e| e.parent());
    // Rule 3 only applies to a folder with a marker file (D1), so the write
    // probe (one create/delete) runs just when one is present.
    let marker = exe_dir.is_some_and(portable_marker);
    let writable = marker && exe_dir.is_some_and(dir_writable);
    let per_user = per_user_dir();
    let chosen = decide(
        env.as_deref(),
        exe.as_deref(),
        marker,
        writable,
        &cwd,
        per_user.as_deref(),
    );
    // Rules 1 and 4 promise an existing directory (rules 2/3 point at one
    // that exists by construction); creating an existing dir is a no-op.
    let _ = std::fs::create_dir_all(&chosen);
    chosen
}

/// Rule 2: any build whose exe path contains a `target` directory component
/// is a cargo build (dev trees, `cargo test` binaries), not an install.
fn is_dev_build(exe: Option<&Path>) -> bool {
    exe.is_some_and(|e| {
        e.components()
            .any(|c| c.as_os_str() == std::ffi::OsStr::new("target"))
    })
}

/// Rule 3's marker files: a folder holding one of them is a portable
/// install on purpose (ux-1.0 D1). The save doubles as an automatic marker
/// for installs made before the marker rule existed. `pw64.toml` is NOT a
/// marker (S4): the README tells players to edit it, and one created next to
/// the exe would silently switch folders and make the progress "vanish".
fn portable_marker(dir: &Path) -> bool {
    ["portable.txt", "pw64.eep"]
        .iter()
        .any(|m| dir.join(m).is_file())
}

/// S4: a message for the player (shown as a toast at start) when the
/// folder that was not chosen holds something they probably expect to be
/// used. `None` for env / dev data dirs and when everything is where it
/// should be.
pub fn startup_notice() -> Option<String> {
    let exe = std::env::current_exe().ok();
    if std::env::var_os("PW64_DATA_DIR").is_some() || OVERRIDE.get().is_some() {
        return None;
    }
    if is_dev_build(exe.as_deref()) {
        return None;
    }
    notice(
        data_dir(),
        exe.as_deref().and_then(Path::parent),
        per_user_dir().as_deref(),
        &|p: &Path| p.is_file(),
    )
}

/// [`startup_notice`]'s decision on already-known paths (`exists` checks a
/// file), so tests can drive it without a filesystem.
fn notice(
    chosen: &Path,
    exe_dir: Option<&Path>,
    per_user: Option<&Path>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<String> {
    let (exe_dir, per_user) = (exe_dir?, per_user?);
    if exe_dir == per_user || exists(&chosen.join("pw64.eep")) {
        return None;
    }
    if chosen == per_user {
        if exists(&exe_dir.join("pw64.eep")) {
            // A save next to the exe that rule 3 refused: the folder isn't
            // writable (Program Files, a read-only share).
            return Some(format!(
                "Birdman64 can't save in its own folder ({}), so your save there \
                 isn't loaded. Move the Birdman64 folder somewhere you can write to, \
                 like Documents, to keep playing it.",
                exe_dir.display()
            ));
        }
        if exists(&exe_dir.join("portable.txt")) {
            return Some(format!(
                "Birdman64 can't save in its own folder ({}), so it saves in {} \
                 instead.",
                exe_dir.display(),
                per_user.display()
            ));
        }
        if exists(&exe_dir.join("pw64.toml")) {
            return Some(format!(
                "The pw64.toml next to Birdman64 isn't used. Your settings file is \
                 in {} (Settings, Open save folder).",
                per_user.display()
            ));
        }
    } else if chosen == exe_dir && exists(&per_user.join("pw64.eep")) {
        return Some(format!(
            "Found a save in {}. This copy keeps its files in its own folder \
             (portable.txt). To play that save here, copy pw64.eep from there \
             into {}.",
            per_user.display(),
            exe_dir.display()
        ));
    }
    None
}

#[cfg(test)]
mod notice_tests {
    use super::{notice, portable_marker};
    use std::path::Path;

    const EXE: &str = "/games/birdman64";
    const USER: &str = "/home/u/birdman64";

    fn run(chosen: &str, files: &[&str]) -> Option<String> {
        let exists = |p: &Path| files.iter().any(|f| Path::new(f) == p);
        notice(
            Path::new(chosen),
            Some(Path::new(EXE)),
            Some(Path::new(USER)),
            &exists,
        )
    }

    #[test]
    fn quiet_when_the_save_is_where_it_is_used() {
        assert_eq!(run(USER, &[]), None);
        assert_eq!(run(USER, &["/home/u/birdman64/pw64.eep"]), None);
        assert_eq!(
            run(
                EXE,
                &["/games/birdman64/pw64.eep", "/home/u/birdman64/pw64.eep"]
            ),
            None
        );
    }

    #[test]
    fn save_in_the_other_folder_is_reported() {
        // Portable copy (portable.txt) with the save still in the per-user dir.
        let m = run(EXE, &["/home/u/birdman64/pw64.eep"]).unwrap();
        assert!(m.starts_with("Found a save in"), "{m}");
        // A save next to an unwritable exe (rule 3 refused the folder).
        let m = run(USER, &["/games/birdman64/pw64.eep"]).unwrap();
        assert!(m.contains("can't save in its own folder"), "{m}");
        // portable.txt in an unwritable folder.
        let m = run(USER, &["/games/birdman64/portable.txt"]).unwrap();
        assert!(m.contains("saves in"), "{m}");
        // A hand-made pw64.toml next to the exe is not used (S4).
        let m = run(USER, &["/games/birdman64/pw64.toml"]).unwrap();
        assert!(m.contains("isn't used"), "{m}");
    }

    /// S4: only portable.txt or a save make a portable folder.
    #[test]
    fn pw64_toml_is_not_a_portable_marker() {
        let dir = std::env::temp_dir().join(format!("pw64-marker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pw64.toml"), "").unwrap();
        assert!(!portable_marker(&dir));
        std::fs::write(dir.join("portable.txt"), "").unwrap();
        assert!(portable_marker(&dir));
        std::fs::remove_file(dir.join("portable.txt")).unwrap();
        std::fs::write(dir.join("pw64.eep"), "").unwrap();
        assert!(portable_marker(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Rule 3's write probe: can a small temp file be created and deleted here?
fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(".birdman64-write-probe");
    match std::fs::File::create(&probe) {
        Ok(f) => {
            drop(f);
            std::fs::remove_file(&probe).is_ok()
        }
        Err(_) => false,
    }
}

/// Rule 4: the per-user data directory (not created yet; `compute` does).
#[cfg(windows)]
fn per_user_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Birdman64"))
}

#[cfg(not(windows))]
fn per_user_dir() -> Option<PathBuf> {
    // XDG base spec: only an absolute $XDG_DATA_HOME overrides the
    // default; empty or relative falls back to $HOME, so the saves never
    // follow the cwd (S10 review). None when neither is set, as before.
    xdg_or_default(
        std::env::var_os("XDG_DATA_HOME").map(|d| PathBuf::from(d).join("birdman64")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/birdman64")),
    )
}

#[cfg(test)]
mod tests {
    use super::decide;
    use std::path::Path;

    /// XDG spec (S10): an override counts only when it is an absolute
    /// path; empty or relative values fall back to the default, so saves
    /// and the cache never follow the cwd.
    #[test]
    #[cfg(not(windows))]
    fn xdg_overrides_need_absolute_paths() {
        use super::xdg_or_default;
        use std::path::PathBuf;
        let abs = PathBuf::from("/xdg/data");
        let def = PathBuf::from("/home/u/.local/share/birdman64");
        // An absolute override wins.
        assert_eq!(
            xdg_or_default(Some(abs.clone()), Some(def.clone())),
            Some(abs)
        );
        // Empty and relative ones fall back (they used to be used as-is).
        assert_eq!(
            xdg_or_default(Some(PathBuf::from("")), Some(def.clone())),
            Some(def.clone())
        );
        assert_eq!(
            xdg_or_default(Some(PathBuf::from("data")), Some(def.clone())),
            Some(def.clone())
        );
        // No override → the default; neither → nothing.
        assert_eq!(
            xdg_or_default(None, Some(def)),
            Some(PathBuf::from("/home/u/.local/share/birdman64"))
        );
        assert_eq!(xdg_or_default(None, None), None);
    }

    #[test]
    fn env_var_wins_over_everything() {
        let env = Path::new("/special/data");
        let exe = Path::new("/repo/target/debug/birdman64");
        assert_eq!(
            decide(
                Some(env),
                Some(exe),
                true,
                true,
                Path::new("/cwd"),
                Some(Path::new("/user"))
            ),
            env
        );
        // Also over a portable install.
        assert_eq!(
            decide(
                Some(env),
                Some(Path::new("/install/pw64")),
                true,
                true,
                Path::new("/cwd"),
                Some(Path::new("/user"))
            ),
            env
        );
    }

    #[test]
    fn dev_build_uses_the_cwd() {
        // Rule 2: a `target` component in the exe path → cwd, even when the
        // exe dir would be a writable portable install.
        let cwd = Path::new("/repo/crates/pw64");
        assert_eq!(
            decide(
                None,
                Some(Path::new("/repo/target/debug/birdman64")),
                true,
                true,
                cwd,
                Some(Path::new("/user"))
            ),
            cwd
        );
        // The check is per component, not a substring: a non-dev exe goes to
        // rule 3/4 (here no marker and not writable, so per-user).
        assert_eq!(
            decide(
                None,
                Some(Path::new("/home/targetfan/pw64")),
                false,
                false,
                cwd,
                Some(Path::new("/user"))
            ),
            Path::new("/user")
        );
        // No exe path at all (odd) → per-user, not the dev rule.
        assert_eq!(
            decide(None, None, false, false, cwd, Some(Path::new("/user"))),
            Path::new("/user")
        );
    }

    #[test]
    fn portable_then_per_user_fallback() {
        let exe_dir = Path::new("/install");
        // Rule 3: a marker file next to the exe and a writable exe dir.
        assert_eq!(
            decide(
                None,
                Some(Path::new("/install/pw64")),
                true,
                true,
                Path::new("/cwd"),
                Some(Path::new("/user"))
            ),
            exe_dir
        );
        // D1: without a marker, even a writable exe dir stays per-user, so
        // an update unzipped into a new folder keeps the old saves.
        assert_eq!(
            decide(
                None,
                Some(Path::new("/install/pw64")),
                false,
                true,
                Path::new("/cwd"),
                Some(Path::new("/user"))
            ),
            Path::new("/user")
        );
        // A marker in an unwritable folder (Program Files) → per-user too.
        assert_eq!(
            decide(
                None,
                Some(Path::new("/program files/birdman64/birdman64.exe")),
                true,
                false,
                Path::new("/cwd"),
                Some(Path::new("/user"))
            ),
            Path::new("/user")
        );
        // No per-user dir available either → cwd last resort.
        assert_eq!(
            decide(
                None,
                Some(Path::new("/program files/birdman64/birdman64.exe")),
                false,
                false,
                Path::new("/cwd"),
                None
            ),
            Path::new("/cwd")
        );
    }
}
