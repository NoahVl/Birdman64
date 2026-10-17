## What's new in 1.2.0

Big thanks to **@djultra64** for contributing the new Infinite LOD and
Infinite view distance options, a first contribution to Birdman64!

### Added

- Linux: `linux_window` setting in `pw64.toml` (`auto`, `wayland` or `x11`)
  to choose between a native Wayland window and X11 (XWayland).
- Two new Display & graphics options, both off by default and applied after a
  restart: Infinite LOD draws every model at full detail however far away, and
  Infinite view distance lets you see the whole island instead of the haze
  about 2 km out (it can lower the frame rate, more so in split-screen).
  Contributed by @djultra64.

### Changed

- The controller's D-pad now moves the stick, so it works in menus and can steer.
  Rebind any D-pad direction to another button (C buttons, A, ...) under
  Settings > Controls.

### Fixed

- Linux (Wayland) on laptops with an NVIDIA GPU next to an AMD or Intel one:
  the picture no longer freezes before the main menu, and the game no longer
  crashes right after choosing Start game. The game now opens its window
  through X11 (XWayland) on these laptops.
- Linux: the first-start ROM tip now names the AppImage (or the birdman64
  program) instead of Birdman64.exe.

**Windows:** download `Birdman64-windows-x64.zip`. **Linux:** download the `.AppImage`.

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
