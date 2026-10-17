# Changelog

All notable changes to Birdman64 are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Texture packs without command-line setup: put a pack folder in the new
  `packs` folder (Settings > Display & graphics > "Open texture packs
  folder"), pick it under "Texture pack" and restart.

## [1.2.0] - 2026-10-14

### Added

- Linux: `linux_window` setting in `pw64.toml` (`auto`, `wayland` or `x11`)
  to choose between a native Wayland window and X11 (XWayland).
- Two new Display & graphics options, both off by default and applied after a
  restart: Infinite LOD draws every model at full detail however far away, and
  Infinite view distance lets you see the whole island instead of the haze
  about 2 km out (it can lower the frame rate, more so in split-screen).
  Contributed by djultra64.

### Changed

- The controller's D-pad now moves the stick, so it works in menus and can steer. Rebind any
  D-pad direction to another button (C buttons, A, ...) under Settings > Controls.

### Fixed

- Linux (Wayland) on laptops with an NVIDIA GPU next to an AMD or Intel one:
  the picture no longer freezes before the main menu, and the game no longer
  crashes right after choosing Start game. The game now opens its window
  through X11 (XWayland) on these laptops.
- Linux: the first-start ROM tip now names the AppImage (or the birdman64
  program) instead of Birdman64.exe.

## [1.1.0] - 2026-10-13

### Added

- Two-player split-screen: pick 1 Player or 2 Players on the title menu.
  Both players fly the same test at once, each with their own pilot,
  controller, HUD, objectives, sounds, results, replay and photos. Works with
  every vehicle and bonus game (hang glider, rocket belt, gyrocopter,
  cannonball, sky diving, jumble hopper, birdman) and with Free flight, which
  you can start from the test menu.
- A Split-screen page in the settings/pause menu: Assign player 2 (picks
  the next controller you press, including a second pad of the same model),
  Split layout (top/bottom or side by side), Pilots collide, and Share best
  score (when player 2 scores higher, the run counts for player 1's save
  file and class totals). The Assign row names player 2's controller.
- Split-screen: each player's radar shows the other player as a cyan arrow.

### Fixed

- The game no longer crashes when a matrix translation goes out of range,
  which could happen after saving a photo.
- Snowy levels in widescreen: falling snow no longer shrinks the picture to
  a 4:3 box, and now covers the whole screen.
- Settings: when rebinding a controller button times out, the
  "No button pressed" message now stays visible.

## [1.0.2] - 2026-10-10

### Fixed

- The settings screen (and the notifications and first-run setup screen) is
  centered again on windows taller than 720 pixels, such as 1080p; it used to
  sit right of and below the middle.

## [1.0.1] - 2026-10-08

### Fixed

- Linux: on systems without a file chooser (no xdg-desktop-portal, zenity or
  kdialog) the game no longer quits on first start. The window now asks for
  the ROM itself: drag the file onto it, or copy it next to the AppImage and
  choose Check again.
- Linux AppImage: no longer crashes at start on systems without
  libxkbcommon-x11; the AppImage carries a fallback copy.

## [1.0.0] - 2026-10-03

First public release.

### Added

- Native Windows and Linux port of Pilotwings 64: the decompiled game runs on
  a Rust replacement for the N64's operating system (libultra), rendered by a
  high-level Fast3D/RDP graphics engine. No emulator.
- One-time setup on first start: the game's open-source decompiled code is
  downloaded (hash-checked) and built on the player's PC; the download itself
  contains no game code. Bring your own Pilotwings 64 (US) ROM, picked in a
  dialog or placed next to the exe (zipped ROMs work).
- Widescreen (16:9, 21:9 or a custom ratio) with the HUD at the true screen
  edges; menu backgrounds stay 4:3 by design.
- High frame rates: the game runs at the monitor's refresh rate (144 Hz,
  240 Hz and beyond, or uncapped), with gameplay timing matched to the
  original.
- Graphics options: MSAA (up to 8x), render scale up to 400%, the N64's
  3-point texture filter, windowed, borderless and exclusive fullscreen.
- Texture packs: PNG files keyed by content hash can replace any texture.
- Full audio: music and sound effects synthesised from the ROM, with a
  volume control.
- In-game settings and pause menu (Esc, F10 or pad Select), saved
  automatically.
- Controllers: keyboard, Xbox, PlayStation and Switch Pro pads with hot-plug,
  fully rebindable in the settings; Switch 2 controllers over Bluetooth LE
  (experimental).
- OLED care: optional HUD drift and dimming.
- Saves as `pw64.eep`, interchangeable with common N64 emulator saves, stored
  in the per-user data folder or next to the exe (portable mode).
- Windows zip (no installer, no runtime to install) and Linux AppImage plus
  tar.gz, with checksums and build provenance attestations.
- `--version`, and crash reports written to `crash.log` in the data folder.

[Unreleased]: https://github.com/NoahVl/Birdman64/compare/v1.2.0...HEAD
[1.2.0]: https://github.com/NoahVl/Birdman64/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/NoahVl/Birdman64/compare/v1.0.2...v1.1.0
[1.0.2]: https://github.com/NoahVl/Birdman64/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/NoahVl/Birdman64/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/NoahVl/Birdman64/releases/tag/v1.0.0
