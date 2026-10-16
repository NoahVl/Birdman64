# Android port (arm64): feasibility and plan

Status: compile-only groundwork. `aarch64-linux-android` builds and lints (CI job
`android`), the app library links, and `packaging/android/build-apk.sh` makes a
debug-signed APK. Nothing has run on a device yet. Target is **arm64-v8a**. Distribution is an
APK on GitHub releases, not Google Play, so Play's rule against downloaded or locally
built executable code doesn't apply. Goal: one codebase. Android-specific code sits
behind `cfg(target_os = "android")`, the same way the Windows and Linux splits work.

## What carries over

- **Game C:** arm64 is little-endian and LP64, the same as x86_64 Linux, so the
  pointer, `long` and layout patches apply unchanged. `-funsigned-char` and
  `-ffp-contract=off` are already in the shared flags (`pw64-cbuild`). Without them,
  arm64 clang would treat `char` as signed differently and fuse mul-add, which changes
  float results. Not re-diffed for `aarch64-linux-android`: same LP64/AAPCS64 layout
  as aarch64 Linux (aarch64.md), only the triple differs.
- **Rust:** wgpu (Vulkan, GLES fallback), winit 0.30 (android-activity), cpal (AAudio),
  egui and corosensei all support aarch64 Android. pw64-gfx, pw64-audio, the formats
  code and the OS core have no target-specific code.
- **Linux platform code:** `mmap(MAP_FIXED_NOREPLACE)` stacks and window, the signal
  crash tracer (register names need arm64 variants: PC/SP instead of RIP/RSP).

## Address space

Option A (option-a.md) removed the low-4 GB requirement: the C sees 32-bit vaddrs
translated to host addresses, so RDRAM, stacks, the app `.so` and the module live
anywhere. Android is relocatable only, like aarch64 Linux (pw64-game build.rs sets
`pw64_reloc`; `PW64_RELOC` defaults to `on`). No low reservation, so ART's use of the
low 4 GB doesn't matter. The fixed-window modes (`strict`/`alias`) still compile there.

## Groundwork (compile-only, nothing run on a device)

- **C:** zig has no bionic libc, but the C is `-nostdinc` and the module imports
  nothing, so zig still compiles and links it. `Os::Android` uses the target
  `aarch64-linux-android26` (API 26 = minSdk, needed for AAudio) plus
  `-mno-outline-atomics`, because the triple's default calls compiler-rt helpers the
  module can't resolve. The NDK is needed only for rustc's final link and for `ring`
  (first-run). A test link with `-z text` showed no text relocations.
- **Platform:** the libc crate lacks `MAP_FIXED_NOREPLACE` on Android, hence
  `pw64_platform::MAP_FIXED_NOREPLACE` (old kernels: hint, address checked). memfd
  goes through the raw syscall (bionic's wrapper is API 30+). Bionic has no
  `backtrace`, so frames come from `_Unwind_Backtrace`. An app `.so` has no
  `__executable_start`, so the image range is its `dl_iterate_phdr` PT_LOAD span.
- **App:** birdman64 = lib + thin bin; Android builds
  `cargo rustc -p birdman64 --lib --crate-type cdylib`. NativeActivity was picked
  because it needs no Java, AndroidX or Gradle. stdout/stderr are piped to logcat
  (tag `birdman64`). Data dir = cwd = the internal files dir. `suspended` pauses
  through the overlay, releases all input, drops the surface (`Gpu::surface =
  None`) and sets `audio::set_suspended`; the game thread (cpal streams are
  `!Send`) pauses the stream from the OS pause hook (`audio::sync_suspend`).
  `resumed` makes a new surface and restarts the stream; the overlay stays open.
  EEPROM writes are fsynced. Activity destroyed → `run_app` returns, the process
  exits, so `android_main` is never re-entered.
- **Pads:** `input/android_pad.rs` maps Android keycodes to gilrs `Button`s, so
  bindings, capture and nav are unchanged. Back toggles settings. gilrs and btleplug
  compile on Android but are never started; rfd is replaced by `android/rfd.rs`
  (log only).
- **Gaps:** winit 0.30 turns joystick MotionEvents into `Touch` (X/Y only), so there
  is no stick or trigger path yet (hook: `android_pad::set_axes`; the d-pad steers
  meanwhile). Also missing: SAF picker, dialogs, pad names, a "pad connected"
  signal (needs `InputManager` via JNI).
- **Crash handler:** the `.so` range (`dl_iterate_phdr`, not signal-safe) is
  computed in `install`; the handler only reads the cache (Release/Acquire flag).

## Touch controls (`touch.rs`, compile-checked only)

- Pure part (layout, hit-test, finger state machine) builds everywhere and is
  unit-tested on desktop; the egui `Painter` is Android-only. Drawn after the
  toasts and the `PW64_WIN_SHOT` readback, never under the overlay; PNG dumps use
  the game thread's renderer, so they never show it.
- **Floating stick:** a finger landing in the left 45% (not on a button) puts the
  base under it; travel = base radius, fed to `stick_to_n64` (same deadzone/gate
  as pads). Idle base drawn at a rest spot. Other fingers keep their own role
  (ids from winit `Touch`); one stick finger at a time.
- Buttons: A, B, C×4, R, Z (above the stick), Start (bottom centre), menu (top
  left, opens settings, never reaches the game). L omitted (game reads it only
  for the demo HUD). A finger slides between A/B/C/R/Z; Start needs its own press.
- State → `android_pad::set_touch` (N64 bits, not rebindable; merged into the
  same controller-1 entry). Any pad key press hides the controls and releases
  them; the next touch shows them again. No "pad connected" check yet.
- Sizes in dp (winit scale factor = density), scaled to the screen (fit 0.7-1.3),
  36 dp side / 12 dp top-bottom margins plus NativeActivity's `content_rect`
  insets (cutouts: the default cutout mode letterboxes landscape windows anyway).
  Opacity/size are constants (`touch::OPACITY`, `scale` 1.0); sliders: TODO.

## Build + first device test

- `ANDROID_HOME=… ANDROID_NDK_HOME=… packaging/android/build-apk.sh` (no Gradle,
  16 KB-aligned) → `target/android-apk/birdman64.apk`, ~21 MB, debug-signed. It has
  the game C linked in, so it is for development only.
- `adb install -r` it, start once, `adb push <rom>.z64
  /sdcard/Android/data/app.birdman64/files/rom/`, restart; `adb logcat -s birdman64`.
- Test order: platform test binaries via `adb push` + `adb shell` first, then boot to
  the title, then touch controls, then pad buttons, then home → back (audio stops
  and resumes, `[audio] stream paused/resumed` in logcat).

## Building the game module

- **On device:** zig ships `aarch64-linux` builds (~37 MB xz). Android 10+ blocks
  `exec()` of files in app data, so ship zig in the APK as `lib/arm64-v8a/libzig.so`
  and run it from `nativeLibraryDir`. Check that `dlopen` of a module written to app
  storage still works on current Android versions. Use the same download, hash check,
  patch and cache flow as the desktop. Compile time on a phone is unmeasured.
- **Fallback:** the desktop exe builds an Android module with
  `--target=aarch64-linux-android`, and the player copies it over. Simpler and no
  zig in the APK, but a worse experience for beginners.

## Frontend work left

- **ROM / files:** pick the ROM through the Storage Access Framework
  (`ACTION_OPEN_DOCUMENT`, JNI), copy it into app storage.
- **Release APK:** debug-signed APK as a release asset (CI signing); no static game C.

## Order

1. **Option A on desktop** (Windows/Linux x86_64): done.
2. **aarch64 Linux:** builds, tests pass under qemu (aarch64.md).
3. **Android, pad only:** compile-only groundwork done (above); next is the device test,
   then the stick path. A module built on desktop: `Os::Android` exists in pw64-cbuild,
   but the builder only targets its host so far.
4. **On-device build, touch controls, SAF ROM picker:** usable by beginners.
