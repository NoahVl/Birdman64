## What's new in 1.5.0

A big one: **move the HUD** anywhere on screen, **export and import your save**
(PC ↔ Android), an **Unlock everything** cheat, a nicer settings menu on phones,
and early **iOS** support.

### Added

- Early iOS support (iPhone and iPad): full-screen play with on-screen touch
  controls or a paired controller, and the ROM and save in the Files app's
  Birdman64 folder. iOS builds are unsigned test builds for now (you sign
  them yourself when sideloading) and not part of the release downloads. As
  on Android, a save change asks you to reopen the app.
- "Unlock everything" in the pause menu: gives a gold medal on every test in
  both save files, which opens every class, the bonus games and Birdman
  (on Android it asks you to reopen the app). Your scores are only ever
  raised, and your previous save is kept as `pw64.eep.before-unlock` in the
  save folder. "Restore save from before unlock" in the same menu puts that
  save back.
- Move your save between PC and Android (or keep a backup): the pause menu has
  "Export save…" and "Import save…". Importing keeps your old save next to it
  as `pw64.eep.before-import` and restarts the game (on Android it asks you to
  reopen the app). On Android, "Open save folder" is replaced by these two.
- Move the HUD: pause menu "Move HUD" lets you drag the flight gauges (speed,
  timer, radar, altimeter, fuel, photo count, messages, cannonball gauges) and
  the performance overlay anywhere on screen, by touch or mouse. Reset puts
  everything back; your layout is saved.

### Changed

- Android/iOS: the touch Z button (take a photo) now sits on the right, next to
  the C buttons, so you can steer while taking pictures.

### Fixed

- The settings menu keeps its size while you move between rows on a page; it
  only changes size when you go to another page.
- Android: the settings lists scroll by dragging with a finger, and a new drag
  continues from where the list was. The rows are bigger and easier to tap.
- Android: the Controls page fits the screen, with the controller beside the
  list, and the close button and dimmed background line up with the screen.
- Android: picking the ROM from some storage apps could close the app
  instead of falling back to a default file name.

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
