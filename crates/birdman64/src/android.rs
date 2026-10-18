//! Android entry point: `android_main`, called by android-activity's
//! NativeActivity glue (through winit's `android-native-activity` backend)
//! on its own thread once the activity is created.
//!
//! NativeActivity rather than GameActivity: it needs no AndroidX or Gradle,
//! so the APK is a manifest, one small Java class and this library; pad keys
//! arrive through winit either way. The Java class (`PwActivity`,
//! packaging/android/java) extends `NativeActivity` only to forward the SAF
//! ROM picker's `onActivityResult` (android/rom_picker.rs).
//!
//! Everything after the paths runs the desktop code unchanged
//! (`crate::start` → `window::run`); the window's `resumed`/`suspended`
//! handle the Android lifecycle, `input/mobile_pad.rs` the pads.

pub(crate) mod rom_picker;

use jni::objects::JObject;
use jni::{Env, JValue, jni_sig, jni_str};
use std::ffi::{c_char, c_int};
use std::os::fd::FromRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use winit::platform::android::activity::{AndroidApp, WindowManagerFlags};

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
    // ROM sources, in order: the remembered pick (`[rom] path` — written by
    // the SAF ROM picker, whose copy lands in the internal `rom/` folder)
    // beats the folders, like the desktop's remembered pick beats its scan
    // (rom_setup.rs). The folders themselves are the manual path
    // (`adb push`, docs/notes/android.md): `<external files dir>/rom/`
    // first (writable from the shell), then `<internal files dir>/rom/`.
    let dirs: Vec<PathBuf> = [app.external_data_path(), internal]
        .into_iter()
        .flatten()
        .map(|d| d.join("rom"))
        .collect();
    let remembered = crate::config::get()
        .rom
        .path
        .as_deref()
        .map(PathBuf::from)
        .filter(|p| p.is_file());
    let rom = remembered.or_else(|| dirs.iter().find_map(|d| first_rom_in(d)));
    // Created by the app, not by `adb shell mkdir`: on a fresh install the
    // app couldn't list a shell-made folder there (scoped storage).
    for d in &dirs {
        let _ = std::fs::create_dir_all(d);
    }
    if rom.is_none() {
        eprintln!(
            "[android] no ROM in {} yet: the window's ROM screen offers the \
             system file picker (android/rom_picker.rs)",
            dirs.iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(" or ")
        );
    }
    // Touch play sends no key events, so without this the screen times out
    // mid-flight. The flag only applies while the activity is in front.
    app.set_window_flags(
        WindowManagerFlags::KEEP_SCREEN_ON,
        WindowManagerFlags::empty(),
    );
    let _ = APP.set(app);
    apply_window_mode();
    crate::start(None, rom);
}

/// The APK's extracted native library dir (`ApplicationInfo.nativeLibraryDir`,
/// needs `extractNativeLibs="true"`): the directory of this library, found
/// with `dladdr` on one of its functions (no JNI needed). Empty if unknown.
#[cfg_attr(not(feature = "first-run"), allow(dead_code))]
pub(crate) fn native_lib_dir() -> PathBuf {
    use std::os::unix::ffi::OsStrExt as _;
    // SAFETY: Dl_info is plain data; dladdr fills it for an address inside
    // a loaded object (this function) and returns 0 otherwise.
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    let found = unsafe { libc::dladdr(native_lib_dir as *const libc::c_void, &mut info) };
    if found == 0 || info.dli_fname.is_null() {
        return PathBuf::new();
    }
    // SAFETY: dli_fname is a NUL-terminated path owned by the linker.
    let name = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) };
    Path::new(std::ffi::OsStr::from_bytes(name.to_bytes()))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// Display-cutout safe insets, px (left, top, right, bottom), as the UI
/// thread last read them ([`apply_window_mode`]); 0 until then.
static CUTOUT: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

/// The cutout insets for the touch layout. NativeActivity's content rect
/// doesn't carry them: targetSdk 35 draws edge to edge, under the cutout.
pub(crate) fn cutout_insets() -> [f32; 4] {
    CUTOUT.each_ref().map(|v| v.load(Ordering::Relaxed) as f32)
}

/// Immersive mode (status and navigation bars hidden; a swipe from the
/// edge shows them for a moment) and a fresh read of the cutout insets.
/// Both are View / Window calls that must run on the Java UI thread, not
/// on `android_main`'s. Call at start and on every resume: the bars come
/// back when the activity returns to the front.
pub(crate) fn apply_window_mode() {
    on_ui_thread(true);
}

/// Re-reads the cutout insets only (a 180° turn moves the cutout to the
/// other side; no event reports that to the native side). The window calls
/// it about once a second; the bars are left alone, so a swipe that showed
/// them isn't undone.
pub(crate) fn refresh_cutout() {
    on_ui_thread(false);
}

fn on_ui_thread(hide_bars: bool) {
    let Some(app) = APP.get().cloned() else {
        return;
    };
    let ui = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        // SAFETY: the process's JavaVM, valid for its lifetime.
        let vm = unsafe { jni::JavaVM::from_raw(ui.vm_as_ptr().cast()) };
        let r = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            // SAFETY: the activity's global reference, alive while the
            // activity is (this runs from its own UI thread).
            let activity = unsafe { JObject::from_raw(env, ui.activity_as_ptr().cast()) };
            window_mode(env, &activity, hide_bars)
        });
        if let Err(e) = r {
            eprintln!("[android] window mode: {e}");
        }
    }));
}

/// The display's current refresh rate, mHz (0 = not read yet). winit has no
/// refresh rate on Android, so `fps = monitor` (the default) reads this.
static REFRESH_MHZ: AtomicU32 = AtomicU32::new(0);

pub(crate) fn refresh_mhz() -> Option<u32> {
    Some(REFRESH_MHZ.load(Ordering::Relaxed)).filter(|&m| m > 0)
}

/// Reads the current refresh rate and, with `request` (start / resume),
/// asks for the fastest mode at the current resolution on our surface
/// (`ANativeWindow_setFrameRate`, API 30+). Without the request the system
/// may keep a game at 60 Hz on a 90/120 Hz panel. Battery saver or the
/// user's "smooth display" setting can still hold it lower; the current
/// rate is re-read every second, so the present tick follows.
fn display_rate(
    env: &mut Env,
    activity: &JObject,
    sdk: i32,
    request: bool,
) -> jni::errors::Result<()> {
    let display = if sdk >= 30 {
        env.call_method(
            activity,
            jni_str!("getDisplay"),
            jni_sig!("()Landroid/view/Display;"),
            &[],
        )?
        .l()?
    } else {
        let wm = env
            .call_method(
                activity,
                jni_str!("getWindowManager"),
                jni_sig!("()Landroid/view/WindowManager;"),
                &[],
            )?
            .l()?;
        env.call_method(
            &wm,
            jni_str!("getDefaultDisplay"),
            jni_sig!("()Landroid/view/Display;"),
            &[],
        )?
        .l()?
    };
    if display.is_null() {
        return Ok(());
    }
    let hz = env
        .call_method(&display, jni_str!("getRefreshRate"), jni_sig!("()F"), &[])?
        .f()?;
    let mhz = (hz * 1000.0).round() as u32;
    if REFRESH_MHZ.swap(mhz, Ordering::Relaxed) != mhz {
        eprintln!("[android] display refresh {hz:.1} Hz");
    }
    if !request || sdk < 30 {
        return Ok(());
    }
    // The fastest mode with the current mode's resolution.
    let size = |env: &mut Env, mode: &JObject| -> jni::errors::Result<(i32, i32)> {
        let w = env
            .call_method(mode, jni_str!("getPhysicalWidth"), jni_sig!("()I"), &[])?
            .i()?;
        let h = env
            .call_method(mode, jni_str!("getPhysicalHeight"), jni_sig!("()I"), &[])?
            .i()?;
        Ok((w, h))
    };
    let cur = env
        .call_method(
            &display,
            jni_str!("getMode"),
            jni_sig!("()Landroid/view/Display$Mode;"),
            &[],
        )?
        .l()?;
    let cur_size = size(env, &cur)?;
    let modes = env
        .call_method(
            &display,
            jni_str!("getSupportedModes"),
            jni_sig!("()[Landroid/view/Display$Mode;"),
            &[],
        )?
        .l()?;
    let modes = env.cast_local::<jni::objects::JObjectArray>(modes)?;
    let mut best = hz;
    for i in 0..modes.len(env)? {
        let mode = modes.get_element(env, i)?;
        if size(env, &mode)? == cur_size {
            let r = env
                .call_method(&mode, jni_str!("getRefreshRate"), jni_sig!("()F"), &[])?
                .f()?;
            best = best.max(r);
        }
    }
    TOP_MHZ.store((best * 1000.0).round() as u32, Ordering::Relaxed);
    request_rate();
    Ok(())
}

/// The panel's fastest mode at the current resolution, mHz (0 = unknown).
static TOP_MHZ: AtomicU32 = AtomicU32::new(0);
/// The chosen fixed frame rate, mHz (0 = monitor / uncapped: no cap).
static FPS_CAP_MHZ: AtomicU32 = AtomicU32::new(0);

/// The frame-rate setting changed (the window calls this with every present
/// re-read; only a change re-requests): a fixed 30 / 60 fps choice asks the
/// panel for that rate instead of holding it at its top rate.
pub(crate) fn set_fps_cap(mhz: u32) {
    if FPS_CAP_MHZ.swap(mhz, Ordering::Relaxed) != mhz {
        request_rate();
    }
}

/// Votes `min(top rate, fps cap)` on our surface. Any thread:
/// `ANativeWindow_setFrameRate` is thread-safe.
fn request_rate() {
    let top = TOP_MHZ.load(Ordering::Relaxed);
    let cap = FPS_CAP_MHZ.load(Ordering::Relaxed);
    let mhz = if cap > 0 && top > 0 {
        top.min(cap)
    } else {
        top
    };
    if mhz == 0 {
        return;
    }
    if let Some(window) = APP.get().and_then(|a| a.native_window()) {
        set_frame_rate(window.ptr().as_ptr().cast(), mhz as f32 / 1000.0);
    }
}

/// `ANativeWindow_setFrameRate(window, hz, COMPATIBILITY_DEFAULT)`, looked
/// up at run time (API 30+; minSdk is 26).
fn set_frame_rate(window: *mut std::ffi::c_void, hz: f32) {
    type SetFrameRate = unsafe extern "C" fn(*mut std::ffi::c_void, f32, i8) -> i32;
    static FN: OnceLock<Option<SetFrameRate>> = OnceLock::new();
    let f = FN.get_or_init(|| {
        // SAFETY: dlopen/dlsym with NUL-terminated names; libandroid is
        // already loaded (we link it), so this only takes a reference.
        unsafe {
            let lib = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                return None;
            }
            let sym = libc::dlsym(lib, c"ANativeWindow_setFrameRate".as_ptr());
            (!sym.is_null()).then(|| std::mem::transmute::<*mut libc::c_void, SetFrameRate>(sym))
        }
    });
    if let Some(f) = f {
        // SAFETY: a live ANativeWindow (the caller holds a reference).
        let r = unsafe { f(window, hz, 0) };
        eprintln!("[android] requested {hz:.1} Hz for the surface (result {r})");
    }
}

/// [`apply_window_mode`]'s JNI calls (UI thread).
fn window_mode(env: &mut Env, activity: &JObject, hide_bars: bool) -> jni::errors::Result<()> {
    let sdk = env
        .get_static_field(
            jni_str!("android/os/Build$VERSION"),
            jni_str!("SDK_INT"),
            jni_sig!("I"),
        )?
        .i()?;
    let window = env
        .call_method(
            activity,
            jni_str!("getWindow"),
            jni_sig!("()Landroid/view/Window;"),
            &[],
        )?
        .l()?;
    let decor = env
        .call_method(
            &window,
            jni_str!("getDecorView"),
            jni_sig!("()Landroid/view/View;"),
            &[],
        )?
        .l()?;
    if !hide_bars {
        // Only the cutout read below.
    } else if sdk >= 30 {
        let bars = env
            .call_static_method(
                jni_str!("android/view/WindowInsets$Type"),
                jni_str!("systemBars"),
                jni_sig!("()I"),
                &[],
            )?
            .i()?;
        let ctl = env
            .call_method(
                &window,
                jni_str!("getInsetsController"),
                jni_sig!("()Landroid/view/WindowInsetsController;"),
                &[],
            )?
            .l()?;
        if !ctl.is_null() {
            // BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            env.call_method(
                &ctl,
                jni_str!("setSystemBarsBehavior"),
                jni_sig!("(I)V"),
                &[JValue::Int(2)],
            )?;
            env.call_method(
                &ctl,
                jni_str!("hide"),
                jni_sig!("(I)V"),
                &[JValue::Int(bars)],
            )?;
        }
    } else {
        // IMMERSIVE_STICKY | LAYOUT_{STABLE,HIDE_NAVIGATION,FULLSCREEN} |
        // HIDE_NAVIGATION | FULLSCREEN (API 26-29).
        env.call_method(
            &decor,
            jni_str!("setSystemUiVisibility"),
            jni_sig!("(I)V"),
            &[JValue::Int(0x1706)],
        )?;
    }
    if let Err(e) = display_rate(env, activity, sdk, hide_bars) {
        eprintln!("[android] refresh rate: {e}");
    }
    if sdk >= 28 {
        let insets = env
            .call_method(
                &decor,
                jni_str!("getRootWindowInsets"),
                jni_sig!("()Landroid/view/WindowInsets;"),
                &[],
            )?
            .l()?;
        if !insets.is_null() {
            let cut = env
                .call_method(
                    &insets,
                    jni_str!("getDisplayCutout"),
                    jni_sig!("()Landroid/view/DisplayCutout;"),
                    &[],
                )?
                .l()?;
            let names = [
                jni_str!("getSafeInsetLeft"),
                jni_str!("getSafeInsetTop"),
                jni_str!("getSafeInsetRight"),
                jni_str!("getSafeInsetBottom"),
            ];
            for (slot, name) in CUTOUT.iter().zip(names) {
                let v = if cut.is_null() {
                    0
                } else {
                    env.call_method(&cut, name, jni_sig!("()I"), &[])?.i()?
                };
                slot.store(v.max(0) as u32, Ordering::Relaxed);
            }
        }
    }
    Ok(())
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
