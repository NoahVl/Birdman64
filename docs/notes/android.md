# Android port (arm64): feasibility and plan

Status: runs on a device (a recent arm64 phone with a 1080p-class screen):
boots to the title, renders correctly widescreen, music plays (AAudio), touch
controls fly, Home → back resumes paused. `aarch64-linux-android` builds and lints (CI job
`android`); `packaging/android/build-apk.sh` makes a
debug-signed APK. Target is **arm64-v8a**. Distribution is an
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

## Groundwork

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

## Touch controls (`touch.rs`)

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
  same controller-1 entry). Any pad key press hides all but the menu button
  (never hide it: without a pad it is the only way out); the next touch shows
  everything. Pause menu "Touch buttons: only menu" does that permanently
  (`[touch] hidden = true`).
- Pause menu "Move touch controls": edit mode, game stays paused (overlay closed,
  `os::set_paused` kept), drag a control (C diamond as one group), Reset / Done;
  Done / Back saves `[touch] <group> = [dx, dy]` (fractions of the screen,
  clamped on screen) and reopens the pause menu.
- Sizes in dp (winit scale factor = density), scaled to the screen (fit 0.7-1.3),
  36 dp side / 12 dp top-bottom margins, or inset + 8 dp where an inset is larger.
  targetSdk 35 is edge to edge: the window draws under the cutout and
  `content_rect` doesn't report it, so `android.rs` reads `DisplayCutout` safe
  insets via JNI (re-read every second: a 180° turn moves the cutout).
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
- Scripted checks: `adb exec-out screencap -p` captures game + touch overlay;
  `adb shell input tap` presses a button, `input swipe x0 y0 x1 y1 2500` holds the
  stick. `wm user-rotation` doesn't turn a `sensorLandscape` activity: rotation
  needs the real sensor.

## Device findings (first bring-up)

- Taps shorter than a game poll were lost (2 of 6 `input tap`s): a press and
  release between two `osContGetReadData` calls. headless.rs latches newly
  pressed bits until every present controller's read saw them (all inputs).
- Screen timed out during touch play (no key events): `KEEP_SCREEN_ON` window flag.
- Immersive mode: View / `WindowInsetsController` calls throw off the UI thread;
  `AndroidApp::run_on_java_main_thread` + the `jni` crate (0.22, android-activity's)
  runs them there, at start and on every `resumed` (the bars come back).
- A start from the lock screen reports a portrait monitor (taller than wide): the
  widescreen default fell to 4:3. Landscape-only, so long side / short side.
- `running.lock` "didn't close properly" toast is off on Android: a kill is the
  normal end of an app process.
- Refresh rate: winit has none on Android. `android.rs` reads
  `Display.getRefreshRate` (UI thread, re-read each second → `window::monitor_mhz`)
  and asks for the fastest same-size mode with `ANativeWindow_setFrameRate`
  (dlsym, API 30+). Measured 120 fps presents (`dumpsys SurfaceFlinger --latency`).
- On the test device, `isGame` / `appCategory="game"` came with a 60 fps game
  frame-rate override for our uid (`GameFrameRateOverrides` in `dumpsys
  SurfaceFlinger`), which held even with a requested rate: removed. The override
  stayed on the old uid until an uninstall.
- The vote is `min(top rate, fixed fps)`: a 30 / 60 fps choice lets the panel drop.
- `adb shell mkdir` of `<external files>/rom` on a fresh install made a folder the
  app couldn't list; the app now creates both `rom/` folders at start.
- Saves (`pw64.eep`, `pw64.toml`) are in internal storage (`files/`): kept by
  `adb install -r`, wiped by an uninstall (as is the external `rom/`).
- Pause menu rows that mean nothing on Android yet: "Open save folder", "Quit".

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
3. **Android, pad only:** groundwork done and device-tested (above); next is the
   pad stick path. A module built on desktop: `Os::Android` exists in pw64-cbuild,
   but the builder only targets its host so far.
4. **On-device build, touch controls, SAF ROM picker:** usable by beginners.
