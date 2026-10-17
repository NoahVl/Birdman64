## What's new in 1.3.0

### Added

- Performance overlay (Pause menu > Performance overlay): a small corner readout
  that works in fullscreen. The FPS counter is on its own; frame time, 1% lows,
  a frame-time graph, CPU load (busiest core), GPU load and the screen corner
  are each optional. The old "Show FPS" option is now the counter here.
- Texture packs without command-line setup: put a pack folder in the new
  `packs` folder (Settings > Display & graphics > "Open texture packs
  folder"), pick it under "Texture pack" and restart.

**Windows:** download `Birdman64-windows-x64.zip`. **Linux:** download the `.AppImage`.
**Android (experimental):** sideload `Birdman64-android-arm64.apk` (64-bit phones,
Android 8+), start it once, then copy your ROM into `Android/data/app.birdman64/files/rom/`
on the phone and start it again. The first launch builds the game on the phone (internet
needed once, about 240 MB installed). Later APKs install over it and keep your saves.

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
