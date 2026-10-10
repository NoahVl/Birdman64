# Changelog

All notable changes to Birdman64 are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

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

[Unreleased]: https://github.com/NoahVl/Birdman64/compare/v1.0.2...HEAD
[1.0.2]: https://github.com/NoahVl/Birdman64/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/NoahVl/Birdman64/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/NoahVl/Birdman64/releases/tag/v1.0.0
