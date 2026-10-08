// Release builds enable this (`--features gui-subsystem`, release.yml):
// no console window for a double-clicked exe. Developer builds keep the
// console; diagnostics still go to stderr.
#![cfg_attr(all(windows, feature = "gui-subsystem"), windows_subsystem = "windows")]

//! Pilotwings 64 native executable.
//!
//! The C runs on the Rust OS core with HLE gfx tasks (via `pw64-gfx`) and
//! file-backed EEPROM. Windowed by default (`window.rs`: winit on the main
//! thread, the game on a `pw64-game` thread); headless when
//! `PW64_HEADLESS` or `PW64_MAX_RETRACES` is set. See native-build.md §9.
//!
//! Options come from the `PW64_*` env vars, an optional `pw64.toml` in the
//! data dir (`paths.rs`: the cwd for developer builds; `[graphics]` /
//! `[input]` tables in `config.rs`), or
//! built-in defaults — env vars win over the config file, which wins over
//! the defaults. The ROM path itself: first CLI arg, else `PW64_ROM`, else
//! the remembered `[rom] path`, else the `rom/` + cwd/exe-dir scan, else the
//! first-run picker (`rom_setup.rs`).

mod audio;
mod ble;
mod config;
#[cfg(feature = "first-run")]
mod firstrun;
mod hle;
mod input;
mod opts;
mod packs;
mod pad_art;
mod pad_sprites;
mod paths;
mod rom_screen;
mod rom_setup;
mod settings;
mod sys;
mod toast;
mod window;

use pw64_platform::{headless, os};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the window on close: the game thread parks at its next retrace
/// (`PARKED`) so the process can exit while no C / GPU work is in flight.
pub static QUIT: AtomicBool = AtomicBool::new(false);
pub static PARKED: AtomicBool = AtomicBool::new(false);

/// "Birdman64 <version> (<commit>)": what `--version`, the settings screen
/// and the crash log show. The commit comes from pw64's build script
/// (`PW64_GIT_HASH`); "unknown" when git was unavailable at build time.
pub fn version_line() -> String {
    format!(
        "Birdman64 {} ({})",
        env!("CARGO_PKG_VERSION"),
        option_env!("PW64_GIT_HASH").unwrap_or("unknown")
    )
}

fn main() {
    // `--version` / `-V` as the first CLI argument: print and exit before
    // anything else (that argument is otherwise the ROM path).
    let first = std::env::args_os().nth(1);
    if first.as_deref() == Some(std::ffi::OsStr::new("--version"))
        || first.as_deref() == Some(std::ffi::OsStr::new("-V"))
    {
        println!("{}", version_line());
        // The release exe has no console (gui-subsystem): stdout from a
        // plain cmd/PowerShell prompt goes nowhere, so show it in a box too.
        #[cfg(all(windows, feature = "gui-subsystem"))]
        if rom_setup::is_interactive() {
            rfd::MessageDialog::new()
                .set_title("Birdman64")
                .set_description(version_line())
                .show();
        }
        return;
    }
    install_panic_hook();
    // pw64-gfx logs unimplemented opcodes / TMEM errors through `log`;
    // `RUST_LOG` filters them (default level: warn).
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Warn)
        .parse_env(env_logger::Env::default())
        .init();
    // One line, every launch: where pw64.toml / pw64.eep / crash.log live
    // (dev builds: the cwd; see paths.rs for the full rule order).
    eprintln!("[paths] data dir: {}", paths::data_dir().display());
    // The EEPROM save file goes to the data dir too (`PW64_EEP` keeps
    // priority); must be set before the OS core can first touch it.
    pw64_platform::headless::set_default_eep_path(paths::data_dir().join("pw64.eep"));
    // Native (C) crashes report into the same crash.log as the panic hook.
    os::crash::set_crash_log_path(paths::data_dir().join("crash.log"));
    // S9: one interactive copy per data dir (a second would overwrite the
    // first's pw64.eep): say so and exit before any setup work.
    sys::single_instance();
    // RDRAM window at 0x80000000 + checks that the exe (linked at 0xC0000000
    // by build.rs: Windows /BASE, Linux non-PIE) is addressable by the C's
    // 32-bit pointers with bit 31 set.
    eprintln!("{}", pw64_game::memmap::init());
    // `--build-game` (or `PW64_BUILD_ONLY=1`): build or verify the game
    // module, then exit without a ROM or window (release smoke test, T11).
    if first.as_deref() == Some(std::ffi::OsStr::new("--build-game"))
        || std::env::var_os("PW64_BUILD_ONLY").is_some()
    {
        #[cfg(feature = "first-run")]
        println!("{}", firstrun::ensure_blocking().display());
        #[cfg(not(feature = "first-run"))]
        println!("static build: the game is linked into the exe, nothing to build");
        return;
    }
    // Diagnostic first: list BLE devices and exit (`PW64_BLE_LIST=1`,
    // ble.rs) — needs no ROM, so it runs before loading one.
    if std::env::var_os("PW64_BLE_LIST").is_some() {
        ble::list_and_exit();
    }
    // U12: a release exe double-clicked inside its zip runs from a temp
    // folder Windows Explorer extracts it into. Nothing works well there
    // (updates delete saves and the ROM step confuses the user), so explain
    // before anything else can go wrong.
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe()
        && in_zip_temp(&exe, &std::env::temp_dir())
    {
        fatal(
            "Birdman64 is running from inside the zip file. Right-click the zip, choose \
             \"Extract All…\", then start Birdman64.exe from the extracted folder.",
        );
    }
    // ROM: first CLI arg, else `PW64_ROM`, else the remembered `[rom] path`,
    // else the `rom/` + cwd/exe-dir scan, else the first-run picker
    // (interactive only; `rom_setup.rs`).
    let rom_arg = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    // No ROM yet and no file dialog on this system (interactive only, so
    // never headless): the window asks for it first (rom_screen.rs).
    let ask_rom = match rom_setup::resolve(rom_arg.as_deref()) {
        Ok(Some(p)) => {
            eprintln!("ROM: {}", p.display());
            false
        }
        Ok(None) => true,
        Err(e) => fatal(&format!("{e:#}")),
    };
    let headless = std::env::var_os("PW64_HEADLESS").is_some()
        || std::env::var_os("PW64_MAX_RETRACES").is_some();
    // First-run build (firstrun.rs): headless builds right here (stderr
    // progress); the window loads a cached module now, else shows the
    // setup screen and builds while it's open.
    #[cfg(feature = "first-run")]
    let setup = if headless {
        firstrun::ensure_blocking();
        false
    } else {
        match firstrun::cached() {
            Some(p) => match firstrun::load(&p) {
                Ok(()) => false,
                // A damaged cached module (e.g. power loss, antivirus):
                // rebuild it on the setup screen, which shows any error
                // with Retry. The dev override is loaded as given.
                Err(f) if std::env::var_os("PW64_GAME_DLL").is_none() => {
                    eprintln!("cached game module unusable, rebuilding: {f}");
                    true
                }
                Err(f) => fatal(&f.to_string()),
            },
            None => true,
        }
    };
    #[cfg(not(feature = "first-run"))]
    let setup = false;
    input::load_script_from_env();
    // Gamepads → controller 1 (`PW64_NO_INPUT`: no gamepad thread); the BLE
    // thread (`ble.rs`) feeds its virtual pads through the same merge; it
    // starts once BLE is switched on (here or later in the settings).
    if std::env::var_os("PW64_NO_INPUT").is_none() {
        input::spawn();
        ble::init();
    }
    if headless {
        // No monitor: `monitor` means 60; the default is 60 (framerate.md).
        // V-Sync is a window feature and ignored headless.
        let fps = opts::fps(false);
        let rate = opts::present_rate(fps, None, false);
        eprintln!("[pw64] fps {fps:?} → present tick {rate:?}");
        let stop = run_game(None, None, rate, window::PendingRate::default());
        // The wgpu device teardown crashes inside the NVIDIA driver's
        // vkGetInstanceProcAddr when the process is exiting (pw64-viewer, which
        // drops in a different order, is unaffected). Until that's understood,
        // leave the Hle (GPU) alive: terminate without dropping.
        std::process::exit(exit_code(stop));
    }
    window::run(setup, ask_rom);
}

/// U12: true when `exe` sits inside `temp` (the usual `%TEMP%`) under a
/// path component that says an archive was opened in place: a `Temp1_…`
/// extraction folder (Windows Explorer's double-click), a `7zO…` or
/// `Rar$EX…` temp folder (7-Zip / WinRAR opening the zip in place), or any
/// component that is itself a `.zip` file. Case-insensitive: Windows paths
/// are.
#[cfg_attr(not(windows), allow(dead_code))] // Linux: only the tests call it
fn in_zip_temp(exe: &std::path::Path, temp: &std::path::Path) -> bool {
    let components = |p: &std::path::Path| {
        p.components()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
            .collect::<Vec<_>>()
    };
    let (exe, temp) = (components(exe), components(temp));
    // The exe must be strictly below temp (at least one component after it).
    if exe.len() <= temp.len() || exe[..temp.len()] != temp[..] {
        return false;
    }
    exe[temp.len()..].iter().any(|c| {
        c.starts_with("temp1_")
            || c.starts_with("7zo")
            || c.starts_with("rar$ex")
            || c.ends_with(".zip")
    })
}

/// Reports a fatal error and exits: stderr always, plus a native error box
/// when a human may be watching (a double-clicked release exe has no console
/// (`gui-subsystem`) and its window may never have opened).
pub fn fatal(msg: &str) -> ! {
    eprintln!("error: {msg}");
    if rom_setup::is_interactive() {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Birdman64")
            .set_description(msg)
            .show();
    }
    std::process::exit(1);
}

/// U14: the crash/stop report box (interactive only: automation with
/// `PW64_HEADLESS` / `PW64_INPUT_SCRIPT` etc. never sees a modal box, it
/// reads stderr). `{headline}` says what happened ("Birdman64 crashed."),
/// the report path points at `crash.log`; Yes opens the save folder (which
/// holds the log) in the file manager.
pub fn report_dialog(headline: &str, log: &std::path::Path) {
    eprintln!("error: {headline} (report: {})", log.display());
    if !rom_setup::is_interactive() {
        return;
    }
    let yes = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Birdman64")
        .set_description(format!(
            "{headline} A report was saved to {}. Please attach it when you \
             report the bug.\n\nOpen the folder now?",
            log.display()
        ))
        .set_buttons(rfd::MessageButtons::YesNo)
        .show();
    if let rfd::MessageDialogResult::Yes = yes {
        paths::open_in_file_manager(log.parent().unwrap_or(log));
    }
}

/// U14: the OS core deadlocked (`os::run` returned `Stop::Deadlock`, the
/// window gets `UserEvent::Stopped` with a non-zero code): append the
/// reason and the thread dump to `crash.log` (the same file the panic hook
/// writes) and show the report box. `dump` is `os::dump_threads()` taken on
/// the game thread (P1: the OS core is thread-local, so a dump taken here on
/// the window thread would be empty).
pub fn report_deadlock(dump: &str) {
    let dir = paths::data_dir();
    let report = format!(
        "-----\nunix {} {}\nOS core deadlock\n{dump}\n\n",
        unix_secs(),
        version_line(),
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("crash.log"))
    {
        let _ = std::io::Write::write_all(&mut f, report.as_bytes());
    }
    report_dialog("Birdman64 stopped unexpectedly.", &dir.join("crash.log"));
}

/// Seconds since the UNIX epoch (locale-free report timestamps, easy to
/// compare). 0 when the clock is broken: the report still gets written.
fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Crash reporter (release blocker B3): appends a report to
/// `crash.log` in the data dir, chains to the previous hook, and tells the
/// user (interactive only) where the report is. Never panics itself: every
/// fallible step is skipped on error, and `data_dir()` is computed inside
/// `catch_unwind` (if it somehow fails, the report is simply not written).
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let dir = std::panic::catch_unwind(|| paths::data_dir().to_path_buf()).ok();
        if let Some(dir) = dir.filter(|d| !d.as_os_str().is_empty()) {
            // Timestamp as seconds since the UNIX epoch (locale-free, easy
            // to compare); version so a report says which build crashed.
            let report = format!(
                "-----\nunix {} {}\n{info}\n{:?}\n\n",
                unix_secs(),
                version_line(),
                std::backtrace::Backtrace::force_capture()
            );
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("crash.log"))
            {
                let _ = std::io::Write::write_all(&mut f, report.as_bytes());
            }
            previous(info);
            report_dialog("Birdman64 crashed.", &dir.join("crash.log"));
            return;
        }
        previous(info);
    }));
}

/// Window closed (`QUIT`): stop here for good at a retrace / present tick
/// (no C or GPU work in flight); main exits the process.
fn park_if_quit() {
    if QUIT.load(Ordering::Acquire) {
        PARKED.store(true, Ordering::Release);
        loop {
            std::thread::park();
        }
    }
}

pub fn exit_code(stop: os::Stop) -> i32 {
    match stop {
        os::Stop::RetraceLimit => 0,
        os::Stop::Deadlock => 1,
    }
}

/// Boots `bootproc` as a coroutine thread and runs the OS main loop on the
/// **current** host thread (the OS core is thread-local). `gpu`: device for
/// PNG dumps; `sink`: framebuffer ops + swaps (the window, `hle::FrameSink`);
/// `rate`: the present tick (`opts::present_rate`; `None` = 60 Hz VI path).
/// Env: `PW64_MAX_RETRACES=<n>` stop after n VI retraces; `PW64_NO_THROTTLE`
/// skip idle time; `PW64_TRACE_OS` log thread events; gfx/present env in
/// `hle` (`PW64_DUMP_FRAMES`, `PW64_STOP_MILESTONE`, ...).
pub fn run_game(
    gpu: Option<(wgpu::Device, wgpu::Queue)>,
    sink: Option<hle::FrameSink>,
    rate: Option<os::vi::PresentRate>,
    pending_rate: window::PendingRate,
) -> os::Stop {
    // Single-threaded OS core: shared mutable HLE state behind a RefCell.
    let hle = std::rc::Rc::new(std::cell::RefCell::new(hle::Hle::new(gpu, sink)));
    os::set_present_rate(rate);
    {
        let h = hle.clone();
        headless::set_gfx_task_handler(move |info| h.borrow_mut().on_gfx_task(info));
        let h = hle.clone();
        headless::set_fb_copy_handler(move |dst, src| h.borrow_mut().fb_copy(dst, src));
        let h = hle.clone();
        headless::set_fb_pixels_handler(move |fb, idx, c| h.borrow_mut().fb_pixels(fb, idx, c));
        let h = hle.clone();
        let pending_rate = pending_rate.clone();
        // Input scripts and frame dumps stay keyed to the 60 Hz retrace
        // (time), whatever the present rate.
        os::set_retrace_hook(move |r| {
            park_if_quit();
            input::script_tick(r.number);
            // The window re-read the monitor's refresh rate (move to
            // another monitor, fullscreen): apply it here, on the game
            // thread — the kernel state is not shared with the window.
            if let Some(new) = pending_rate.lock().unwrap().take() {
                os::set_present_rate(new);
                eprintln!("[window] monitor rate → {new:?}");
            }
            // Read per retrace, not once at startup: a monitor change can
            // switch between present ticks and the 60 Hz VI latch.
            if os::present_rate().is_some() {
                h.borrow_mut().retrace(r);
            } else {
                h.borrow_mut().present(r);
            }
        });
        let h = hle.clone();
        os::set_present_hook(move |p| {
            park_if_quit();
            h.borrow_mut().latch(p.framebuffer, p.black);
        });
        // S3/P8: a quit while the settings overlay pauses the core (no
        // retraces or present ticks fire) parks from the pause loop.
        os::set_pause_hook(park_if_quit);
    }
    audio::install(); // audio tasks → pw64-audio HLE, AI → cpal (audio.rs)
    pw64_game::set_widescreen_aspect(opts::widescreen().unwrap_or(0.0));
    pw64_game::set_fill_screen(opts::fill_screen());
    os::boot(pw64_game::bootproc, std::ptr::null_mut());
    let cfg = os::RunConfig {
        max_retraces: std::env::var("PW64_MAX_RETRACES")
            .ok()
            .and_then(|v| v.parse().ok()),
        throttle: std::env::var_os("PW64_NO_THROTTLE").is_none(),
    };
    let stop = os::run(&cfg);
    audio::finish();
    // `PW64_PROFILE_RETRACES`: pw64-side splits (the OS core prints its own).
    if let Some(s) = (std::env::var_os("PW64_PROFILE_RETRACES").is_some())
        .then(hle::profile_stats)
        .filter(|s| !s.is_empty())
    {
        eprintln!("{s}");
    }
    eprintln!("stopped: {stop:?}\n{}", os::dump_threads());
    // Keep the Hle (GPU objects) alive: see the teardown note in `main`.
    std::mem::forget(hle);
    stop
}

#[cfg(test)]
mod tests {
    use super::in_zip_temp;

    // Forward slashes on purpose: component splitting works the same on
    // every platform that way, so the tests run everywhere.
    #[test]
    fn explorer_extraction_folder_is_detected() {
        let temp = "C:/Users/n/AppData/Local/Temp";
        assert!(in_zip_temp(
            "C:/Users/n/AppData/Local/Temp/Temp1_Birdman64-windows-x64.zip_4210/Birdman64.exe"
                .as_ref(),
            temp.as_ref()
        ));
        // Case-insensitive, like Windows paths.
        assert!(in_zip_temp(
            "C:/Users/n/AppData/Local/TEMP/TEMP1_birdman64.zip_1/Birdman64.exe".as_ref(),
            temp.as_ref()
        ));
    }

    #[test]
    fn zip_named_component_is_detected() {
        assert!(in_zip_temp(
            "C:/Users/n/AppData/Local/Temp/birdman64.zip/Birdman64.exe".as_ref(),
            "C:/Users/n/AppData/Local/Temp".as_ref()
        ));
    }

    #[test]
    fn archive_tool_temp_folders_are_detected() {
        // 7-Zip's `7zO<random>` and WinRAR's `Rar$EX...` in-place temp dirs.
        let temp = "C:/Users/n/AppData/Local/Temp";
        assert!(in_zip_temp(
            "C:/Users/n/AppData/Local/Temp/7zOA12345/Birdman64.exe".as_ref(),
            temp.as_ref()
        ));
        assert!(in_zip_temp(
            "C:/Users/n/AppData/Local/Temp/Rar$EXa0.567/Birdman64.exe".as_ref(),
            temp.as_ref()
        ));
        // Case-insensitive.
        assert!(in_zip_temp(
            "C:/Users/n/AppData/Local/Temp/7ZOa/Birdman64.exe".as_ref(),
            temp.as_ref()
        ));
        // The pattern must be under temp: the same folder name elsewhere
        // does not count.
        assert!(!in_zip_temp(
            "C:/Games/7zOa/Birdman64.exe".as_ref(),
            temp.as_ref()
        ));
    }

    #[test]
    fn normal_places_are_not_detected() {
        let temp = "C:/Users/n/AppData/Local/Temp";
        // Extracted properly: not under a zip-ish folder.
        assert!(!in_zip_temp(
            "C:/Games/Birdman64/Birdman64.exe".as_ref(),
            temp.as_ref()
        ));
        // Directly in temp, but not inside a zip component.
        assert!(!in_zip_temp(
            "C:/Users/n/AppData/Local/Temp/Birdman64.exe".as_ref(),
            temp.as_ref()
        ));
        // A temp subfolder without the zip pattern (a real install under
        // temp, odd but possible).
        assert!(!in_zip_temp(
            "C:/Users/n/AppData/Local/Temp/portable/Birdman64.exe".as_ref(),
            temp.as_ref()
        ));
        // Under temp, but the pattern is outside temp.
        assert!(!in_zip_temp(
            "C:/Games/Temp1_x.zip/Birdman64.exe".as_ref(),
            temp.as_ref()
        ));
    }

    #[test]
    fn unix_temp_paths_work_too() {
        assert!(in_zip_temp(
            "/tmp/Temp1_Birdman64.zip_9/Birdman64".as_ref(),
            "/tmp".as_ref()
        ));
        assert!(!in_zip_temp("/usr/bin/birdman64".as_ref(), "/tmp".as_ref()));
    }
}
