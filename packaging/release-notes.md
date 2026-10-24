## What's new in 1.5.2-pre1

This is a pre-release of the upcoming 1.5.2: the Windows controller fixes for
the 8BitDo 64 and Switch Online N64 controllers, plus the fixes that landed on
main since 1.5.1. Please test it! If anything is off, attach the controller log
(see below).

### Added

- A controller log for bug reports: create an empty `pad-debug.txt` next to
  `Birdman64.exe` (or set `PW64_PAD_LOG`) and the game records what your
  controller sends — name, USB ids, every raw button/axis event, and what the
  game receives. It has no personal data. Attach it if you report a controller
  problem.

### Fixed

- Windows: the 8BitDo 64 controller in DInput mode now has working C buttons
  (up/down were swapped with left/right) and pressing R no longer leaves C-up
  stuck.
- Windows: the Switch Online N64 controller (USB) now has working C buttons
  (C-left read as C-right and C-up/C-down did nothing).
- Linux: the game no longer crashes at startup on some Wayland setups whose
  graphics driver offers a 16-bit display format first.
- iOS: opening the pause menu (the touch menu button or a controller's
  settings button) no longer crashes the game.

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
