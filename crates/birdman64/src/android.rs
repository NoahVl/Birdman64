//! Android entry point: `android_main`, called by android-activity's
//! NativeActivity glue (through winit's `android-native-activity` backend)
//! on its own thread once the activity is created.
//!
//! NativeActivity rather than GameActivity: it needs no Java/AndroidX code
//! and no Gradle, so the APK is a manifest plus this library; pad keys
//! arrive through winit either way. GameActivity (IME, insets) can replace
//! it once a Java side exists anyway (the SAF ROM picker needs JNI).
//!
//! Everything after the paths runs the desktop code unchanged
//! (`crate::start` → `window::run`); the window's `resumed`/`suspended`
//! handle the Android lifecycle, `input/android_pad.rs` the pads.

use std::ffi::{c_char, c_int};
use std::os::fd::FromRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use winit::platform::android::activity::AndroidApp;

static APP: OnceLock<AndroidApp> = OnceLock::new();

/// The activity handle (`window::run` hands it to winit's event loop).
pub(crate) fn app() -> AndroidApp {
    APP.get()
        .expect("android_main stores the AndroidApp first")
        .clone()
}

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    stderr_to_logcat();
    // Data dir (pw64.toml, pw64.eep, crash.log) = the app's internal files
    // dir; also the cwd, so the relative `rom/` scan looks there.
    let internal = app.internal_data_path();
    if let Some(dir) = &internal {
        let _ = std::fs::create_dir_all(dir);
        crate::paths::set_data_dir(dir.clone());
        let _ = std::env::set_current_dir(dir);
    }
    // Temporary ROM source until the SAF picker exists (TODO(android)):
    // `<external files dir>/rom/` (writable with plain `adb push`, see
    // docs/notes/android.md), then `<internal files dir>/rom/`.
    let dirs: Vec<PathBuf> = [app.external_data_path(), internal]
        .into_iter()
        .flatten()
        .map(|d| d.join("rom"))
        .collect();
    let rom = dirs.iter().find_map(|d| first_rom_in(d));
    if rom.is_none() {
        eprintln!(
            "[android] no ROM in {}: push one there (adb push <rom>.z64 <dir>/)",
            dirs.iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(" or ")
        );
    }
    let _ = APP.set(app);
    crate::start(None, rom);
}

/// The first file (by name) in `dir` with a ROM extension (`.z64`, `.n64`,
/// `.v64`, `.zip`, any case); the loader verifies it.
fn first_rom_in(dir: &Path) -> Option<PathBuf> {
    let mut roms: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().is_some_and(|e| {
                    matches!(
                        e.to_string_lossy().to_ascii_lowercase().as_str(),
                        "z64" | "n64" | "v64" | "zip"
                    )
                })
        })
        .collect();
    roms.sort();
    roms.into_iter().next()
}

#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

/// An Android app's stdout/stderr go nowhere: route both into logcat (tag
/// `birdman64`, one entry per line) so every `eprintln!` diagnostic, the
/// panic report and env_logger output show in `adb logcat -s birdman64`.
fn stderr_to_logcat() {
    let mut fds = [0 as c_int; 2];
    // SAFETY: plain pipe + dup2 over our own stdout/stderr descriptors.
    let read = unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return;
        }
        libc::dup2(fds[1], 1);
        libc::dup2(fds[1], 2);
        libc::close(fds[1]);
        std::fs::File::from_raw_fd(fds[0])
    };
    let _ = std::thread::Builder::new()
        .name("logcat".into())
        .spawn(move || {
            use std::io::BufRead as _;
            const ANDROID_LOG_INFO: c_int = 4;
            for line in std::io::BufReader::new(read)
                .split(b'\n')
                .map_while(Result::ok)
            {
                let mut text: Vec<u8> = line.into_iter().filter(|&b| b != 0).collect();
                text.push(0);
                // SAFETY: NUL-terminated tag and text.
                unsafe {
                    __android_log_write(
                        ANDROID_LOG_INFO,
                        c"birdman64".as_ptr(),
                        text.as_ptr().cast(),
                    )
                };
            }
        });
}
