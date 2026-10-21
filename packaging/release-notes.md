## What's new in 1.5.1

Android controller fix: **sticks, analog triggers and d-pads now work**, including
the built-in controls of handhelds (Retroid Pocket, AYN and similar).

### Changed

- iOS: the project no longer provides ready-made test builds. Build the app
  yourself in a private copy of the repository (free, no Mac needed) or on a
  Mac; the README's iPhone section has the steps.

### Fixed

- Android: the analog sticks, analog triggers and hat-style d-pads of
  controllers now work, including the built-in controls of Android handhelds.
  Before, only the buttons were read, so you couldn't steer, and Z (on LT by
  default) never fired. The triggers and d-pad can also be picked when rebinding.
- Android: Settings › Controls no longer says "No controller connected" while
  a controller works. It now names the controllers your phone or handheld
  reports and shows the last button you pressed, so you can test your
  controller there. The Switch 2 Bluetooth option is hidden on Android, where
  it never worked.

**Windows:** download `Birdman64-windows-x64.zip`. **Linux:** download the `.AppImage`.

**Android (experimental):** sideload `Birdman64-android-arm64.apk` (64-bit phones,
Android 8+) and start it. The first launch builds the game on the phone (internet
needed once, about 240 MB installed), then tap **Choose a game file** and pick your
ROM. Prefer copying the file yourself? `Android/data/app.birdman64/files/rom/`
still works too. Later APKs install over it and keep your saves.

**iPhone / iPad (test builds):** not attached here and no ready-made
download: you build the app yourself (free, no Mac needed) and sideload it
with your own Apple ID (with [iLoader](https://iloader.app), recommended);
the [iOS testing guide](https://github.com/NoahVl/Birdman64/pull/14) has the
steps. Feedback is very welcome.

You need your own Pilotwings 64 (US) ROM.

## Quick start

1. Extract the zip and run `Birdman64.exe` from the extracted folder (Linux:
   make the AppImage executable and run it). Never run it from inside the zip.
2. If Windows shows "Windows protected your PC": click **More info**, then
   **Run anyway**.
3. Put your Pilotwings 64 (US) ROM next to the exe, or pick it when the game
   asks. Only the US version works.
4. The one-time setup downloads the game's open-source code and builds it on
   your PC (usually under a minute, internet needed once), then you are
   flying. Press Esc or F10 for settings.

The other files (debug symbols, checksums) are for bug reports and
verification.
