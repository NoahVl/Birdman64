//! Small OS integrations that need a native call: the single-instance guard
//! (S9), the display-sleep blocker (W4) and the exclusive-mode pre-check
//! (W6). Raw `extern "system"` declarations (Windows) keep this free of
//! extra crate features; everything is a no-op where it doesn't apply.

use std::fs::File;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The lock file held for the whole run (S9). Unlocked only by
/// [`release_instance`], before "Restart now" spawns the next copy.
static INSTANCE: Mutex<Option<File>> = Mutex::new(None);

/// Set on the command a "Restart now" (U16) spawns: the new copy waits for
/// the old one to let go of the instance lock instead of showing the
/// "already running" box.
pub const RESTARTED_ENV: &str = "PW64_RESTARTED";

/// S9: one copy per data dir (two would overwrite each other's
/// `pw64.eep`). Interactive runs only: automation (headless sweeps, scripted
/// runs) may run several copies in parallel on purpose. Locks
/// `<data dir>/instance.lock` (`File::try_lock`: an OS lock, so a crash
/// never leaves it stale). A second copy shows a friendly box and exits.
pub fn single_instance() {
    if !crate::rom_setup::is_interactive() {
        return;
    }
    let path = crate::paths::data_dir().join("instance.lock");
    let file = match File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    {
        Ok(f) => f,
        // Can't even create the file (read-only dir): no guard, play on.
        Err(e) => {
            eprintln!("[instance] {}: {e}", path.display());
            return;
        }
    };
    // A restart waits for the old copy to exit (it quits right after the
    // spawn); anything else gives up at once.
    let wait = if std::env::var_os(RESTARTED_ENV).is_some() {
        Duration::from_secs(5)
    } else {
        Duration::ZERO
    };
    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => {
                *INSTANCE.lock().unwrap() = Some(file);
                return;
            }
            Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < wait => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(std::fs::TryLockError::WouldBlock) => break,
            Err(std::fs::TryLockError::Error(e)) => {
                eprintln!("[instance] lock {}: {e}", path.display());
                return;
            }
        }
    }
    eprintln!("error: Birdman64 is already running");
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Info)
        .set_title("Birdman64")
        .set_description("Birdman64 is already running. Look for its window on the taskbar.")
        .show();
    std::process::exit(0);
}

/// Lets go of the instance lock (U16 "Restart now", right before spawning
/// the new copy). The file stays open so a failed spawn can
/// [`reclaim_instance`].
pub fn release_instance() {
    if let Some(f) = INSTANCE.lock().unwrap().as_ref() {
        let _ = f.unlock();
    }
}

/// Takes the instance lock back after a failed "Restart now" spawn, so the
/// still-running copy keeps guarding its save. Best effort (no-op when no
/// lock was held).
pub fn reclaim_instance() {
    if let Some(f) = INSTANCE.lock().unwrap().as_ref()
        && let Err(e) = f.try_lock()
    {
        eprintln!("[instance] could not take the lock back: {e}");
    }
}

#[cfg(windows)]
mod win {
    pub const ES_CONTINUOUS: u32 = 0x8000_0000;
    pub const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;
    pub const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    pub const CDS_FULLSCREEN: u32 = 0x0000_0004;
    pub const CDS_TEST: u32 = 0x0000_0002;
    pub const DM_BITSPERPEL: u32 = 0x0004_0000;
    pub const DM_PELSWIDTH: u32 = 0x0008_0000;
    pub const DM_PELSHEIGHT: u32 = 0x0010_0000;
    pub const DM_DISPLAYFREQUENCY: u32 = 0x0040_0000;

    /// `DEVMODEW` (wingdi.h), display variant of the unions; 220 bytes.
    #[repr(C)]
    pub struct DevModeW {
        pub device_name: [u16; 32],
        pub spec_version: u16,
        pub driver_version: u16,
        pub size: u16,
        pub driver_extra: u16,
        pub fields: u32,
        pub position: [i32; 2],
        pub display_orientation: u32,
        pub display_fixed_output: u32,
        pub color: i16,
        pub duplex: i16,
        pub y_resolution: i16,
        pub tt_option: i16,
        pub collate: i16,
        pub form_name: [u16; 32],
        pub log_pixels: u16,
        pub bits_per_pel: u32,
        pub pels_width: u32,
        pub pels_height: u32,
        pub display_flags: u32,
        pub display_frequency: u32,
        pub tail: [u32; 8],
    }
    const _: () = assert!(std::mem::size_of::<DevModeW>() == 220);

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn SetThreadExecutionState(flags: u32) -> u32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        pub fn ChangeDisplaySettingsExW(
            device: *const u16,
            mode: *const DevModeW,
            hwnd: isize,
            flags: u32,
            param: *const core::ffi::c_void,
        ) -> i32;
    }
}

/// W4: keep the display (and the machine) awake while the game runs.
/// Windows only counts keyboard/mouse as activity, so a gamepad-only
/// session would otherwise blank the screen after the power-plan timeout.
/// Applies to the calling thread until changed or the thread ends: call it
/// on the window (main) thread. `false` restores the normal behaviour.
pub fn keep_display_awake(on: bool) {
    #[cfg(windows)]
    {
        let flags = if on {
            win::ES_CONTINUOUS | win::ES_DISPLAY_REQUIRED | win::ES_SYSTEM_REQUIRED
        } else {
            win::ES_CONTINUOUS
        };
        // SAFETY: plain flag call with no pointers; the previous state it
        // returns is not needed.
        unsafe { win::SetThreadExecutionState(flags) };
    }
    #[cfg(not(windows))]
    let _ = on;
}

/// W6: would the display driver accept this exclusive video mode? winit
/// only `debug_assert`s the result of its own `ChangeDisplaySettingsExW`
/// (a dev build panics, a release build silently stays in the old mode
/// with a topmost window), so ask first with `CDS_TEST`. `true` off
/// Windows (no pre-check there) and when the monitor has no device name.
pub fn video_mode_supported(
    monitor: &winit::monitor::MonitorHandle,
    mode: &winit::monitor::VideoModeHandle,
) -> bool {
    #[cfg(windows)]
    {
        let Some(name) = monitor.name() else {
            return true;
        };
        let device: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let size = mode.size();
        // SAFETY: an all-zero DEVMODEW is a valid "no fields" value.
        let mut dm: win::DevModeW = unsafe { std::mem::zeroed() };
        dm.size = std::mem::size_of::<win::DevModeW>() as u16;
        dm.fields =
            win::DM_BITSPERPEL | win::DM_PELSWIDTH | win::DM_PELSHEIGHT | win::DM_DISPLAYFREQUENCY;
        dm.bits_per_pel = u32::from(mode.bit_depth());
        dm.pels_width = size.width;
        dm.pels_height = size.height;
        // winit reports dmDisplayFrequency * 1000.
        dm.display_frequency = mode.refresh_rate_millihertz() / 1000;
        // SAFETY: `device` is NUL-terminated and `dm` a sized DEVMODEW, both
        // alive for the call; CDS_TEST changes nothing.
        let r = unsafe {
            win::ChangeDisplaySettingsExW(
                device.as_ptr(),
                &dm,
                0,
                win::CDS_FULLSCREEN | win::CDS_TEST,
                std::ptr::null(),
            )
        };
        if r != 0 {
            eprintln!(
                "[window] display driver rejects {}x{}@{} ({} bit): code {r}",
                size.width, size.height, dm.display_frequency, dm.bits_per_pel
            );
        }
        r == 0
    }
    #[cfg(not(windows))]
    {
        let _ = (monitor, mode);
        true
    }
}
