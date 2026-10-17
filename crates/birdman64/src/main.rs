// Release builds enable this (`--features gui-subsystem`, release.yml):
// no console window for a double-clicked exe. Developer builds keep the
// console; diagnostics still go to stderr.
#![cfg_attr(all(windows, feature = "gui-subsystem"), windows_subsystem = "windows")]

//! Birdman64 desktop executable: the whole app lives in the library
//! (lib.rs), which Android builds as the app's `.so` instead.

#[cfg(not(target_os = "android"))]
fn main() {
    birdman64::desktop_main();
}

/// Android starts the library's `android_main` from the activity; this
/// executable has nothing to run there.
#[cfg(target_os = "android")]
fn main() {
    eprintln!(
        "Birdman64 on Android runs as the app's library (android_main), not as an executable"
    );
    std::process::exit(1);
}
