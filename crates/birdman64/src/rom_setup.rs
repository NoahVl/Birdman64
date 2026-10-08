//! First-run ROM setup for the release exe: resolve the ROM file with the
//! same order `pi::find_rom` uses, but when nothing is configured and a human
//! will actually see the result (a window is about to open), ask for the file
//! once with a native picker and remember the pick in `pw64.toml`
//! (`[rom] path`) — every later launch then starts straight into the game.
//!
//! Headless runs (CI, scripted flights) never prompt and never touch the
//! picker: they keep the headless fallback (`rom/` + cwd/exe-dir scan, zips
//! included, then `decomp/baserom.us.z64`) and its error behaviour exactly.
//! The picker call is gated behind [`is_interactive`], so tests never open one.

use crate::config;
use pw64_platform::pi;
use std::path::{Path, PathBuf};

/// What to do about the ROM before booting the game.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Load this path (CLI arg or `PW64_ROM`).
    Use(PathBuf),
    /// The still-existing `[rom] path`: load it; if it no longer verifies
    /// (file replaced), fall through to the scan / picker like [`Self::Prompt`].
    Remembered(PathBuf),
    /// Nothing configured and interactive: the `rom/` + cwd/exe-dir scan
    /// first (a ROM dropped next to the exe must just work), then the picker.
    Prompt,
    /// Nothing configured, non-interactive: `pi::find_rom`'s old fallback and
    /// its error when that fails too.
    Legacy,
}

/// The resolution order, without the picker/I/O side effects. `explicit` (the
/// first CLI argument) and the `PW64_ROM` env value pass through unvalidated —
/// a bad one errors at load, exactly as before. The configured `[rom] path`
/// is only used while the file still exists: a moved/deleted ROM re-prompts
/// (or falls back headless) instead of erroring forever.
fn decide(
    explicit: Option<PathBuf>,
    env: Option<PathBuf>,
    cfg: Option<PathBuf>,
    interactive: bool,
) -> Decision {
    if let Some(p) = explicit.or(env) {
        return Decision::Use(p);
    }
    if let Some(p) = cfg.filter(|p| p.is_file()) {
        return Decision::Remembered(p);
    }
    if interactive {
        Decision::Prompt
    } else {
        Decision::Legacy
    }
}

/// Resolves and loads the ROM (`pw64_platform::pi::set_rom`): CLI arg →
/// `PW64_ROM` → remembered `[rom] path` → the `rom/` + cwd/exe-dir scan →
/// picker (interactive only). The error propagates to `main`'s usual
/// message + exit(1). `Ok(None)`: no ROM yet and no file dialog on this
/// system; the window asks for it (rom_screen.rs).
pub fn resolve(explicit: Option<&std::path::Path>) -> anyhow::Result<Option<PathBuf>> {
    let env = std::env::var_os(pi::ROM_ENV).map(PathBuf::from);
    let cfg = config::get().rom.path.as_deref().map(PathBuf::from);
    let interactive = is_interactive();
    // F9: a remembered ROM that is gone (decide falls back to Prompt) changes
    // the welcome text when the picker opens.
    let stale_rom =
        explicit.is_none() && env.is_none() && cfg.as_ref().is_some_and(|p| !p.is_file());
    let found = |r: anyhow::Result<PathBuf>| r.map(Some);
    match decide(explicit.map(PathBuf::from), env, cfg, interactive) {
        Decision::Use(p) => found(pi::load_rom(Some(&p)).map_err(|e| friendly_use_error(&p, e))),
        Decision::Remembered(p) => match pi::load_rom(Some(&p)) {
            Ok(p) => Ok(Some(p)),
            Err(e) if interactive => {
                eprintln!("[pw64] remembered ROM unusable ({e:#}); searching again");
                // F9: an unreadable remembered ROM (permissions, antivirus)
                // gets its own box before the search continues.
                if is_io_error(&e) {
                    couldn_read_box(&p, &e);
                }
                found(pi::load_rom(None)).or_else(|_| pick_and_remember(true))
            }
            Err(e) => Err(e),
        },
        Decision::Prompt => found(pi::load_rom(None)).or_else(|_| pick_and_remember(stale_rom)),
        Decision::Legacy => found(pi::load_rom(None)),
    }
}

/// Is the error chain rooted in an OS I/O error (as opposed to ROM content
/// that failed verification)?
fn is_io_error(e: &anyhow::Error) -> bool {
    e.root_cause().is::<std::io::Error>()
}

/// F9: the box for a ROM file that couldn't be read at all (not the file's
/// content being wrong): say what happened instead of pretending it isn't
/// a ROM.
fn couldn_read_box(p: &Path, e: &anyhow::Error) {
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Couldn't read the file")
        .set_description(format!("Birdman64 couldn't read {}: {e:#}", p.display()))
        .show();
}

/// A CLI / `PW64_ROM` path that doesn't load still ends in `fatal`: prepend
/// the friendly `diagnose` wording so the error box says what the file is
/// instead of quoting a SHA1 (ux-1.0 F6).
fn friendly_use_error(p: &Path, e: anyhow::Error) -> anyhow::Error {
    match diagnose_message(p) {
        Some(m) => anyhow::anyhow!("{e:#}\n\n{m}"),
        None => e,
    }
}

/// The friendly diagnosis for a file that `Rom::load` rejected, when there
/// is one to make. Zips stay out: `Rom::from_zip` reports the member problem
/// itself, so the box keeps the detailed error there.
fn diagnose_message(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    if data.starts_with(&[0x50, 0x4B, 0x03, 0x04]) {
        return None; // zip magic: the member error is already specific
    }
    let problem = pw64_rom::diagnose(&data);
    Some(match problem {
        pw64_rom::RomProblem::Region(name) => format!(
            "This is the {name} version of Pilotwings 64. Birdman64 only works with the US version."
        ),
        pw64_rom::RomProblem::OtherGame(title) => {
            format!("This is a ROM of another game ({title}).")
        }
        pw64_rom::RomProblem::NotN64 => "This isn't an N64 ROM file.".to_string(),
        pw64_rom::RomProblem::Modified => {
            "This Pilotwings 64 (US) file is modified or a bad dump.".to_string()
        }
    })
}

/// True when a human may be watching (so dialogs make sense): none of the
/// automation switches `PW64_HEADLESS`, `PW64_MAX_RETRACES`, `PW64_INPUT_SCRIPT`
/// or `PW64_NO_DIALOGS` is set, and never under `cfg(test)` (test runners).
/// Not a `stdin().is_terminal()` check any more: the release exe is built
/// with the `gui-subsystem` feature (no console at all), so the picker and
/// error boxes must still show without a terminal. Headless/scripted runs
/// keep the headless behaviour and never see a dialog.
pub fn is_interactive() -> bool {
    if cfg!(test) {
        return false;
    }
    std::env::var_os("PW64_HEADLESS").is_none()
        && std::env::var_os("PW64_MAX_RETRACES").is_none()
        && std::env::var_os("PW64_INPUT_SCRIPT").is_none()
        && std::env::var_os("PW64_NO_DIALOGS").is_none()
}

/// Picker rounds: welcome box → pick → verify → remember → install. The
/// welcome box explains what the picker is for before it opens (ux-1.0 F6:
/// a bare file dialog left release players guessing); a file that doesn't
/// verify says why in a message box (a double-clicked exe's console closes
/// on exit, so stderr alone would never be read) and re-opens the picker;
/// cancel ends the process cleanly (nothing was started). The boxes show on
/// Linux too: rfd works there, and the same help applies. `Ok(None)`: this
/// system has no file dialog at all; the window asks instead (rom_screen.rs).
fn pick_and_remember(stale_rom: bool) -> anyhow::Result<Option<PathBuf>> {
    // Dev switch: act as if there were no file dialog (checks the in-window
    // ROM screen on any system).
    if std::env::var_os("PW64_NO_FILE_DIALOG").is_some() {
        return Ok(None);
    }
    if !welcome_to_birdman64(stale_rom) {
        no_rom_chosen();
    }
    let mut title = "Select your Pilotwings 64 (USA) ROM file";
    loop {
        let path = match pick_rom(title) {
            Pick::File(p) => p,
            Pick::Cancel => no_rom_chosen(),
            Pick::NoDialog => return Ok(None),
        };
        match use_rom_file(&path) {
            Ok(abs) => return Ok(Some(abs)),
            Err((box_title, description)) => {
                rfd::MessageDialog::new()
                    .set_level(rfd::MessageLevel::Error)
                    .set_title(box_title)
                    .set_description(description)
                    .show();
                title = "That file didn't work — select the Pilotwings 64 (USA) ROM file";
            }
        }
    }
}

/// A ROM file the player chose (picker, or dropped on the window): verify,
/// remember in `pw64.toml`, install. `Err`: a title and a text saying why
/// the file can't be used, for a box or the in-window ROM screen.
pub fn use_rom_file(path: &Path) -> Result<PathBuf, (&'static str, String)> {
    let rom = match pw64_rom::Rom::load(path) {
        Ok(rom) => rom,
        Err(e) => {
            eprintln!(
                "{} is not a usable Pilotwings 64 ROM ({e:#}); only the US (USA) ROM works",
                path.display()
            );
            return Err(rom_problem(path, &e));
        }
    };
    // Absolute, so the config works from any working directory. Not
    // `canonicalize`: on Windows that yields a `\?\C:\…` verbatim path.
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    if let Err(e) = config::save_rom_path(&abs) {
        // The game still runs; the dialog simply shows again next time.
        eprintln!("[pw64] couldn't remember the ROM path ({e})");
    }
    pi::set_rom(rom);
    Ok(abs)
}

/// Why a chosen file can't be used: (title, text).
fn rom_problem(path: &Path, e: &anyhow::Error) -> (&'static str, String) {
    // F9: an unreadable file (permissions, antivirus) is a different
    // problem from the wrong content: say so plainly.
    if is_io_error(e) {
        return (
            "Couldn't read the file",
            format!(
                "Birdman64 couldn't read {}: {e:#}\n\nIf antivirus software is \
                 involved, allow Birdman64 and pick the file again. Otherwise \
                 please pick another file.",
                path.display()
            ),
        );
    }
    let problem = match diagnose_message(path) {
        Some(problem) => format!(
            "{problem}\n\nOnly the US (USA) version works (.z64, .n64 or .v64, \
             or a .zip containing one). Please pick another file."
        ),
        None => format!(
            "This file isn't a Pilotwings 64 ROM this game can use. \
             Only the US (USA) version works (.z64, .n64 or .v64, or a .zip \
             containing one). Please pick another file.\n\nDetails: {e:#}"
        ),
    };
    ("Not the right ROM", problem)
}

/// The welcome box before the first picker round: Ok proceeds to the file
/// dialog, Cancel means the player is not ready yet. `false` then. `stale_rom`
/// (F9): the remembered ROM is gone or unreadable, so say that up front
/// instead of a bare "choose a file".
fn welcome_to_birdman64(stale_rom: bool) -> bool {
    // Linux: rfd's message boxes run zenity, and a missing zenity reads as
    // Cancel; the portal file picker may still work, so never stop there
    // (a real Cancel just leads to the picker, which can be cancelled too).
    let stale_note = if stale_rom {
        "The ROM you used last time can't be found anymore. "
    } else {
        ""
    };
    let ok = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Info)
        .set_buttons(rfd::MessageButtons::OkCancel)
        .set_title("Welcome to Birdman64")
        .set_description(format!(
            "{stale_note}Birdman64 plays your own copy of Pilotwings 64. Choose the game file \
             (the ROM) you made from your cartridge: a .z64, .n64 or .v64 file, or a .zip \
             containing one. Only the US version works.\n\nTip: put the file next to \
             Birdman64.exe and it is found automatically."
        ))
        .show()
        == rfd::MessageDialogResult::Ok;
    ok || !cfg!(windows)
}

/// Cancel on the welcome box or the picker: a friendly goodbye instead of
/// the previous silent exit (the release exe has no console), then exit 0:
/// nothing was started.
fn no_rom_chosen() -> ! {
    eprintln!(
        "No ROM selected: pick again on the next start, or set it in pw64.toml ([rom] path) / PW64_ROM to skip this dialog. Only the US (USA) ROM works."
    );
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Info)
        .set_title("Birdman64")
        .set_description("No game file chosen. Start Birdman64 again when you have it.")
        .show();
    std::process::exit(0);
}

/// What the file dialog gave.
enum Pick {
    File(PathBuf),
    Cancel,
    /// No dialog could be shown at all (Linux without the portal, zenity or
    /// kdialog).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    NoDialog,
}

/// Native open dialog filtered to N64 ROM files (any byte order). Only
/// reached from [`pick_and_remember`], i.e. never in tests or CI.
fn pick_rom(title: &str) -> Pick {
    // rfd: the native dialog (Linux: the xdg desktop portal, then zenity on
    // a portal error). `None` is either a cancel (portal or zenity) or no
    // backend at all; rfd doesn't say which (rfd 0.15 xdg_desktop_portal.rs),
    // so the time it took tells them apart below.
    let started = std::time::Instant::now();
    if let Some(p) = rfd::FileDialog::new()
        .set_title(title)
        // rfd's open dialog has no default-file-name hint, so the filter is
        // the only hint: N64 ROM files and zip-wrapped ones. Uppercase
        // extensions too (Windows filters are case-sensitive, so a ROM named
        // PW64.Z64 was previously invisible), plus an all-files fallback.
        .add_filter(
            "N64 ROM (USA)",
            &["z64", "Z64", "n64", "N64", "v64", "V64", "zip", "ZIP"],
        )
        .add_filter("All files", &["*"])
        .pick_file()
    {
        return Pick::File(p);
    }
    // F2 (Linux): no portal and no zenity. Try kdialog before giving up.
    #[cfg(target_os = "linux")]
    {
        // A dialog that stayed up for a second was shown and cancelled (KDE
        // has the portal and kdialog: without this a cancel opened a second
        // picker, and with neither tool a cancel printed "no file dialog").
        // A missing backend fails at once.
        if started.elapsed() >= std::time::Duration::from_secs(1) {
            return Pick::Cancel;
        }
        if linux_tool_exists("kdialog") {
            return kdialog_pick(title).map_or(Pick::Cancel, Pick::File);
        }
        if linux_tool_exists("zenity") {
            // rfd reached zenity, so this was a cancel (or a one-off zenity
            // failure; the welcome loop treats it the same way).
            return Pick::Cancel;
        }
        // No dialog at all: the window asks for the ROM instead
        // (rom_screen.rs: drop the file on it, or put it in the scanned
        // folder).
        eprintln!(
            "[pw64] no file dialog on this system (none of xdg-desktop-portal, zenity \
             or kdialog): asking for the ROM in the window"
        );
        Pick::NoDialog
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = started;
        Pick::Cancel
    }
}

/// Is `cmd` installed? (Spawn probe: any exit status counts; only a spawn
/// failure means it is missing.)
#[cfg(target_os = "linux")]
fn linux_tool_exists(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// F2 (Linux): the kdialog file picker (`kdialog --getopenfilename . <filters>
/// <title>`). `None` = cancelled or kdialog failed.
#[cfg(target_os = "linux")]
fn kdialog_pick(title: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("kdialog")
        .args([
            "--title",
            title,
            "--getopenfilename",
            ".",
            "N64 ROM (USA) (*.z64 *.Z64 *.n64 *.N64 *.v64 *.V64 *.zip *.ZIP)\nAll files (*)",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let picked = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if picked.is_empty() {
        return None;
    }
    Some(PathBuf::from(picked))
}

#[cfg(test)]
mod tests {
    use super::{Decision, decide};
    use std::path::PathBuf;

    /// A real existing file (for the config-hit case); dropped at the end.
    struct TempRom(PathBuf);
    impl TempRom {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir()
                .join(format!("pw64-rom-setup-{}-{tag}.z64", std::process::id()));
            std::fs::write(&p, b"x").unwrap();
            Self(p)
        }
    }
    impl Drop for TempRom {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn cli_arg_and_env_win_unvalidated() {
        // Both pass through even if they don't exist: load errors later,
        // exactly like the pre-picker behaviour.
        assert_eq!(
            decide(
                Some(PathBuf::from("arg.z64")),
                Some(PathBuf::from("env.z64")),
                Some(PathBuf::from("cfg.z64")),
                true
            ),
            Decision::Use("arg.z64".into())
        );
        assert_eq!(
            decide(
                None,
                Some(PathBuf::from("env.z64")),
                Some(PathBuf::from("cfg.z64")),
                true
            ),
            Decision::Use("env.z64".into())
        );
    }

    #[test]
    fn config_hit_miss_and_stale() {
        let rom = TempRom::new("hit");
        let cfg = rom.0.clone();
        // Hit: an existing configured path is used.
        assert_eq!(
            decide(None, None, Some(cfg.clone()), true),
            Decision::Remembered(cfg.clone())
        );
        // Miss: no config at all → prompt (interactive) / legacy fallback.
        assert_eq!(decide(None, None, None, true), Decision::Prompt);
        assert_eq!(decide(None, None, None, false), Decision::Legacy);
        // Stale: a configured path whose file is gone re-prompts, never panics.
        assert_eq!(
            decide(None, None, Some(PathBuf::from("Z:/gone/pw64.z64")), true),
            Decision::Prompt
        );
        assert_eq!(
            decide(None, None, Some(PathBuf::from("Z:/gone/pw64.z64")), false),
            Decision::Legacy
        );
    }

    /// A synthetic header written to a temp file, dropped at the end.
    fn header_file(tag: &str, country: u8) -> std::path::PathBuf {
        let mut v = vec![0x80u8, 0x37, 0x12, 0x40];
        v.resize(0x1000, 0);
        v[0x20..0x20 + "PILOTWINGS64".len()].copy_from_slice(b"PILOTWINGS64");
        v[0x3E] = country;
        let p =
            std::env::temp_dir().join(format!("pw64-rom-diag-{tag}-{}.z64", std::process::id()));
        std::fs::write(&p, v).unwrap();
        p
    }

    #[test]
    fn diagnose_message_uses_the_decided_wording() {
        // European header → the friendly region message, not a SHA1.
        let p = header_file("euro", b'P');
        let msg = super::diagnose_message(&p).unwrap();
        assert!(
            msg.contains("This is the European version of Pilotwings 64"),
            "got: {msg}"
        );
        let _ = std::fs::remove_file(&p);
        // Bad magic → the plain "isn't an N64 ROM" message.
        let p = std::env::temp_dir().join(format!("pw64-rom-diag-bad-{}.z64", std::process::id()));
        std::fs::write(&p, b"junk").unwrap();
        let msg = super::diagnose_message(&p).unwrap();
        assert_eq!(msg, "This isn't an N64 ROM file.");
        let _ = std::fs::remove_file(&p);
    }
}
