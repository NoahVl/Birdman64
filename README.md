# Birdman64

**Pilotwings 64 as a native PC game for Windows and Linux.** Widescreen up to
21:9, your monitor's full refresh rate (144 Hz and beyond), modern gamepads,
rebindable controls and an in-game settings menu. No emulator: the game's own
code, built for your PC from your own cartridge dump (ROM).

[![Latest release](https://img.shields.io/github/v/release/NoahVl/Birdman64?label=download)](https://github.com/NoahVl/Birdman64/releases/latest)
[![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20Linux-blue)](#system-requirements)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange)](#developing)

**[Download the latest release](https://github.com/NoahVl/Birdman64/releases/latest)**,
then follow the [Quick start](#quick-start). An unofficial fan project; see
[Legal](#legal).

## Screenshots

Holiday Island at 1080p, 16:9, 8x MSAA.

![A hang glider banking over the coast in a widescreen view](https://github.com/NoahVl/Birdman64/releases/download/media/hero.jpg)

The original 4:3 image (left) and Birdman64 at 16:9 (right). Widescreen shows
more of the world instead of stretching it.

![Before and after: 4:3 versus 16:9](https://github.com/NoahVl/Birdman64/releases/download/media/before-after.jpg)

The rocket belt at 21:9.

![Rocket belt in flight at 21:9](https://github.com/NoahVl/Birdman64/releases/download/media/rocket-belt-21x9.jpg)

A test briefing at 16:9.

![Test briefing screen at 16:9](https://github.com/NoahVl/Birdman64/releases/download/media/briefing.jpg)

<details>
<summary>More screenshots: controls and first-time setup</summary>

Every key and button can be rebound in the settings menu.

![Controls page of the in-game settings menu](https://github.com/NoahVl/Birdman64/releases/download/media/controls.jpg)

The one-time setup on first start takes about a minute.

![First start setup screen](https://github.com/NoahVl/Birdman64/releases/download/media/setup.png)

</details>

## System requirements

**Windows:** Windows 10 or 11, 64 bit. A graphics card that supports
DirectX 12 or Vulkan (almost every PC from the last ten years); keep the
graphics driver up to date: [NVIDIA](https://www.nvidia.com/Download/index.aspx),
[AMD](https://www.amd.com/en/support/download/drivers.html) or
[Intel](https://www.intel.com/content/www/us/en/download-center/home.html).
Nothing else to install: no Visual C++ runtime, no .NET, no emulator.
An internet connection once, for the one-time setup. About 200 MB of free
disk space. A keyboard or a gamepad (Xbox, PlayStation, Switch Pro and most
USB/Bluetooth pads work).

**Linux:** a 64 bit distro from 2022 or newer (Ubuntu 22.04, Fedora 36,
Debian 12, Steam Deck desktop mode, or newer). Vulkan or OpenGL graphics
drivers (Mesa, already installed on most desktops). For the AppImage:
`libfuse2` (Ubuntu/Debian: `sudo apt install libfuse2`; or start it with
`--appimage-extract-and-run`). Sound uses ALSA/PipeWire (already there on
desktops). Internet once, about 200 MB of disk.

**macOS:** not supported: the port needs a memory layout macOS on Apple
Silicon does not allow.

## Quick start

1. On the [Releases](../../releases) page, download
   **Birdman64-windows-x64.zip** (Windows) or the Birdman64 `.AppImage`
   (Linux). The other files (debug symbols, checksums) are for bug reports
   and verification.
2. Right-click the zip, choose **Extract All** and open the extracted
   folder. Never run `Birdman64.exe` from inside the zip. Keep the
   `toolchain` folder (the bundled compiler) next to the exe: the first
   start needs it.
3. Run `Birdman64.exe` (Linux: the AppImage). Windows may show
   "Windows protected your PC" (SmartScreen, because the program is not
   code signed): click **More info**, then **Run anyway**.
4. On first start, choose your Pilotwings 64 ROM when the game asks (a
   `.z64`, `.n64` or `.v64` file, or a `.zip` containing one; see
   [What's a ROM?](#whats-a-rom)). Put the file next to `Birdman64.exe` and
   it is found automatically. Only the US (USA) version of the game works.
5. Wait for the one-time preparation step: the game downloads about 1 MB of
   the open source decompilation from GitHub and builds it on your machine
   (a progress window, usually under a minute depending on your PC; it needs
   about 200 MB of disk space). Internet is only needed the first time.
   After that the game starts normally every time.
6. Play! The first launch shows a card with the basic keys: move with
   W A S D, A = Space, B = Left Shift, Start = Enter, camera with
   I J K L, F11 for fullscreen, Esc or F10 for settings.

## Controls

Keyboard (defaults; rebind everything on the settings screen's Controls
page, or in `pw64.toml` under `[input.keyboard]`, for example `A = "Space"`):

| Input | N64 action | What it does |
|---|---|---|
| W, A, S, D or arrow keys | Control stick | Steer · move in menus |
| Space | A | Confirm · thrust, flap, jump, fire |
| Left Shift | B | Back · gentle thrust, parachute |
| Z or Left Ctrl | Z | Hold to aim camera/missile, release to shoot |
| E | R | Change camera view |
| Enter | Start | Pause · confirm |
| I, J, K, L | C up, C left, C down, C right | Look around |
| T, F, G, H | D-pad | Not used in this game |
| Q | L | Not used in this game |
| Esc | Settings | Open or close the settings screen (when Esc is not bound to a game input) |
| F10 | Settings | Open or close the settings screen |
| F11 or Alt+Enter | Fullscreen | Switch between the window and fullscreen |

Gamepad (Nintendo layout pads match by position, so on a Switch pad the
button printed B is the N64 A):

| Input | N64 action | What it does |
|---|---|---|
| Left stick | Control stick | Steer · move in menus |
| South button (Xbox: A, PlayStation: Cross, Nintendo: B) | A | Confirm · thrust, flap, jump, fire |
| West/East buttons (Xbox: X/B, PlayStation: Square/Circle, Nintendo: Y/A) | B | Back · gentle thrust, parachute |
| North button (Xbox: Y, PlayStation: Triangle, Nintendo: X) | C up | Look around |
| Right stick (beyond halfway) | C buttons | Look around |
| Left trigger or ZL (Xbox: LT, PlayStation: L2, Nintendo: ZL) | Z | Hold to aim camera/missile, release to shoot |
| LB / L (Xbox: LB, PlayStation: L1, Nintendo: L) | L | Not used in this game |
| RB / R and RT / ZR (Xbox: RB, RT, PlayStation: R1, R2, Nintendo: R, ZR) | R | Change camera view |
| Start (Xbox: Menu, PlayStation: Options, Nintendo: +) | Start | Pause · confirm |
| D-pad | D-pad | Not used in this game |
| Select (Xbox: View, PlayStation: Create, Nintendo: minus) | Settings screen | Open or close the settings screen |

## Features

- Widescreen by default on wide monitors (any ratio up to 21:9, menu
  backgrounds stay 4:3 by design); fill the screen without letterbox bars
- Window, borderless fullscreen or exclusive fullscreen with a chosen video
  mode
- High frame rate: runs at your monitor's refresh rate, 144 Hz and beyond
- MSAA (1x, 4x, 8x) and render scale (supersampling or upscale) with a
  linear or nearest filter
- The N64's 3-point texture filter (set the filter to `n64`, see
  `PW64_FILTER` below)
- Texture packs (PNG files, keyed by content hash)
- Optional V-Sync style pacing to remove judder
- OLED care: the HUD drifts and the screen dims a little to spread wear
- Gamepads (including Switch 2 controllers over Bluetooth, experimental)
- An in-game settings screen (F10, Esc or pad Select) that rebinds every
  key and button and saves to `pw64.toml`
- One-time setup that builds the game from your own ROM: nothing but our own
  code ships in the download

## How it was made

Birdman64 went from an empty repository to a 1.0 release in **six days**
(27 September to 3 October 2026). [NoahVl](https://github.com/NoahVl)
designed and led it, directing a team of AI coding agents:

- **Orchestrator and workers:** a lead agent (Claude Opus 5.5) plans the work,
  delegates it and reviews every change. A fast, low-cost model
  (GLM 5.3-Flash) runs the routine tasks in parallel and wrote 44% of the
  commits, which keeps the expensive model for architecture, debugging and
  review.
- **Human in the loop:** product and design decisions, legal ground rules,
  and play-testing every build.
- **Verified, not trusted:** CI on Windows and Linux with warnings as errors,
  321 tests, and scripted flights whose frames are checked before a change
  lands.

The result: about 47,000 lines of Rust in 12 crates and roughly 250 commits.
That covers a replacement for the N64 operating system, graphics and audio
engines, asset tools and a one-click launcher. The
[design notes](docs/notes) are the agents' shared memory, so every session
picks up where the last one stopped.

## Settings and files

**Settings screen:** press F10, Esc (or pad Select) to pause the game and
change options. Changes are saved when you close the screen; volume and
most graphics options apply immediately, a few (MSAA, widescreen, fill
screen) are marked "(restart)" and need the game restarted to apply. Reset
rows restore the defaults.

`pw64.toml` example (all keys optional, the same options exist as
environment variables):

```toml
[graphics]
msaa = 4                   # 1, 4 or 8
scale = 2.0                # render scale, 0.5 or more (1 = window size)
scale_filter = "linear"    # or "nearest"
filter = "n64"             # N64 3-point texture filter, or "bilinear"
widescreen = "16:9"        # 1, "w:h", a ratio like "2.33", or 0 (off)
fps = "monitor"            # "monitor", 30..1000, or 0 (uncapped)
vsync = false              # display-paced pacing (less judder, more latency)
display_mode = "windowed"  # "windowed", "borderless" or "exclusive"
fullscreen_resolution = "1920x1080@60" # exclusive video mode WxH[@Hz]
show_fps = false           # frame rate in the window title
no_audio = true            # no audio output device
volume = 0.8               # 0.0..1.0

[input.ble]
enabled = true             # Switch 2 pads over Bluetooth LE

[oled]
drift = false              # HUD drift for OLED burn-in care
brightness = 1.0           # 1.0, 0.85, 0.8, 0.75 or 0.7

[ui]
pause_in_background = true # pause when you switch windows or unplug the controller
```

**Where files live:** everything goes in one folder. How that folder is chosen
is described under **Save files** below.

The one-time setup cache (the downloaded decompilation and the compiled game
module) lives in a `cache` folder next to them: next to the exe in a portable
install, else `%LOCALAPPDATA%\Birdman64\cache` (Windows) or `~/.cache/birdman64`
(Linux). The Linux AppImage always uses the per-user locations above. Deleting
the cache is safe: the next start just builds the game once more.

**Save files:** the folder holds the save (`pw64.eep`), the settings
(`pw64.toml`) and `crash.log`. It is chosen once at start, in this order: the
`PW64_DATA_DIR` environment variable when set; next to the exe when that
folder holds `portable.txt` or `pw64.eep` and is writable
(portable use, for example on a USB stick: create an empty `portable.txt`
there; a folder like `C:\Program Files` is not writable); otherwise the
per-user folder, `%APPDATA%\Birdman64` (Windows) or
`$XDG_DATA_HOME/birdman64`, else `~/.local/share/birdman64` (Linux). A fresh
extraction has no marker next to the exe, so it uses the per-user folder:
updating stays a matter of unzipping the new version anywhere, and saves,
settings and the remembered ROM carry over. The Linux AppImage always uses
the per-user location. The start-up log prints `[paths] data dir: <path>` with
the folder that was chosen. To edit `pw64.toml` by hand, open that folder from
the settings screen ("Open save folder"); outside a portable install, a
`pw64.toml` placed next to the exe is not used.

The save itself, `pw64.eep`, is a raw N64 EEPROM image: no header and no byte
swapping (unlike `.v64` ROM files). It is written as 2048 bytes. The game's
two save files sit at offsets 0x000 and 0x100 inside it (Birdman64 emulates a
4 Kbit EEPROM, so the first 512 bytes are the part the game uses).

**Using an emulator save:** quit Birdman64 and back up your `pw64.eep` first,
then copy the emulator's save file over it. A Project64, mupen64plus or
simple64 `.eep` file (512 or 2048 bytes) works as it is. A RetroArch
Mupen64Plus-Next `.srm` file starts with the EEPROM contents, so a copy
renamed to `pw64.eep` works too (only the first 2048 bytes are read; the next
save writes a 2048-byte file). To go back to an emulator, copy `pw64.eep`
under the emulator's save file name, or set `PW64_EEP=<path>` and Birdman64
uses (and updates) that file directly. Never replace the save file while the
game is running: the game keeps the save in memory and overwrites the whole
file at the next save.

**Useful environment variables for players:** every option in `pw64.toml`
also exists as a `PW64_*` environment variable, which wins over the file
when both are set:

| Variable | Effect |
|---|---|
| `PW64_MSAA=<1\|4\|8>` | MSAA sample count (restart) |
| `PW64_SCALE=<f>` | render resolution as a fraction of the window (below 1 upscales) |
| `PW64_SCALE_FILTER=<linear\|nearest>` | filter for the blit when the render resolution differs |
| `PW64_FILTER=<bilinear\|n64>` | texture filter (the N64 look is `n64`) |
| `PW64_WIDESCREEN=<1\|w:h\|0>` | widescreen aspect; the default is your monitor's aspect when it is wider than 4:3 (up to 21:9) |
| `PW64_FILL_SCREEN=<0\|1>` | no letterbox bars around world views (default: follows widescreen) |
| `PW64_FPS=<monitor\|N\|0>` | frame rate (uncapped with 0) |
| `PW64_VSYNC=<0\|1>` | display-paced pacing (V-Sync) |
| `PW64_DISPLAY_MODE=<windowed\|borderless\|exclusive>` | window, borderless fullscreen or exclusive fullscreen |
| `PW64_FULLSCREEN_RES=WxH[@Hz]` | the exclusive fullscreen video mode |
| `PW64_OLED=<0\|1>` | HUD drift and dimming for OLED screens |
| `PW64_BLE=<1\|0>` | Switch 2 controllers over Bluetooth LE |
| `PW64_ROM=<path>` | which ROM file to use |
| `PW64_DATA_DIR=<path>` | folder for saves and settings |
| `PW64_EEP=<path>` | exact save file to use instead of `pw64.eep` |
| `PW64_TEX_PACKS=<dir>` | replace textures from a PNG pack |
| `PW64_NO_AUDIO=1` | run without an audio device |

The developer variables live in [Developing](#developing).

## What's a ROM?

A ROM is a copy of the game cartridge's data, made with a cartridge dumper
from a cartridge you own. We can't provide or link one. Birdman64 needs the
US version of Pilotwings 64: [Troubleshooting](#troubleshooting) shows how
to check which file you have.

## Updating

Download the new zip and extract it anywhere. Saves, settings and the
remembered ROM live in your user folder (see **Save files** above), so the
new version picks them up automatically. Keep the `toolchain` folder next
to the exe: the first start of the new version rebuilds the game once,
briefly showing the setup screen again.

## Uninstalling

Delete the folder you extracted, then delete the per-user folder:
`%APPDATA%\Birdman64` and `%LOCALAPPDATA%\Birdman64` on Windows,
`~/.local/share/birdman64` (or `$XDG_DATA_HOME/birdman64`) and
`~/.cache/birdman64` on Linux. That removes your saves and settings too,
so back up `pw64.eep` first if you want to keep them.

## Troubleshooting

- **"The ROM does not work" or the game refuses to start:** only the
  **US (USA)** version of Pilotwings 64 is supported. European or Japanese
  cartridges and ROMs from other regions will not work. To check which file
  you have, compute its SHA-1 hash: on Windows,
  `certutil -hashfile rom.z64 SHA1`; on Linux, `sha1sum rom.z64`. The US
  version in `.z64` byte order has SHA-1
  `ec771aedf54ee1b214c25404fb4ec51cfd43191a` (`.n64`/`.v64` files have a
  different hash but are converted automatically).
- **Windows shows a SmartScreen warning:** the program is not code signed.
  Click **More info**, then **Run anyway**.
- **"No compatible graphics adapter was found":** the renderer needs Vulkan,
  DirectX 12 or OpenGL support on your GPU. Update your graphics driver from your GPU
  vendor's website (links in [System requirements](#system-requirements)) and try again.
- **The one-time build step fails or your antivirus intervenes:** the game
  compiles the decompiled game on your machine once, which looks like a
  compiler to antivirus software. Allow it in your antivirus, or add an
  exclusion for the game folder, then try again. If the bundled compiler
  (the `toolchain` folder next to the exe) was removed or blocked, re-extract
  the zip, keeping the exe and the folder together.
- **The setup screen shows an error:** it has a **Retry** button and shows
  the path of a `build.log` file. Try **Retry** first; if it keeps failing,
  open an issue and attach the `build.log` it points to (bug reports about
  the setup without that log are hard to diagnose).
- **Linux: the AppImage does not start:** it needs `libfuse2`, which recent
  distributions (Ubuntu 22.04 and newer) no longer install by default. Either
  install `libfuse2` or run it with `--appimage-extract-and-run`.
- **Verify a download:** every release file carries a build provenance
  attestation. With the GitHub `gh` CLI:
  `gh attestation verify <file> --repo <owner>/<repo>`.
- **Where are my saves?** in the save file `pw64.eep` (see
  [Settings and files](#settings-and-files)). To back a save up, copy that
  file somewhere safe while the game is not running.
- **The game crashes:** a `crash.log` file is written in the same folder as
  your saves. To report a bug, open an issue and attach `crash.log`, the
  output of `Birdman64.exe --version`, your GPU model, your OS and the SHA-1
  of your ROM. **Never attach the ROM itself.**

## Building from source

You need:

- Rust stable (<https://rustup.rs>), any of the usual toolchains (Windows:
  the MSVC one). No Python and no LLVM install needed.
- Linux x86_64: `libasound2-dev`, `libudev-dev`, `libdbus-1-dev`,
  `pkg-config` and `xz-utils`.
- The C compiler, zig 0.16.0 (the same one the release uses to build the game
  on players' machines): one command downloads it into `tools/zig/` and
  checks its sha256. Or point `PW64_ZIG` at your own zig 0.16.0.

The decompilation is a git submodule, so clone recursively:

```sh
git clone --recursive https://github.com/USER/Birdman64
cd Birdman64
cargo run -p pw64-cbuild --example get_zig --features fetch
cargo run --release -p birdman64
```

Pass your ROM path as the argument, drop it next to the exe, or let the
first-run picker find it. macOS is not supported.

## Developing

**How it works:**

1. **Native game:** the game logic from the
   [Pilotwings 64 decompilation](https://github.com/gcsmith/Pilotwings64Decomp)
   is compiled natively (zig's bundled clang, the same compiler players use, driven by `pw64-game`'s
   build script) and runs on a Rust replacement for libultra
   (`pw64-platform`): coroutine threads, PI/SI/AI/VI devices, message queues.
2. **Rust renderer and audio:** the display lists the game builds are
   interpreted by `pw64-gfx` (Fast3D + RDP HLE on wgpu); the RSP audio
   microcode (`aspMain`) is HLE'd by `pw64-audio`.
3. **Full Rust (long-term):** C modules are replaced one at a time by Rust
   equivalents, each checked against the C behaviour, until no C remains.

**Documentation:** [ROADMAP.md](ROADMAP.md) is the roadmap and status; the
`docs/notes/*.md` files are topic notes (formats, graphics, audio, input,
native build, the Rust port).

**Testing and linting:**

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

A scripted headless flight check (drives the title menu into a flight and
dumps PNG frames to `tmp/`):

```sh
PW64_INPUT_SCRIPT=crates/birdman64/scripts/fly_hang_glider.txt \
PW64_MAX_RETRACES=2700 PW64_NO_THROTTLE=1 PW64_NO_INPUT=1 \
PW64_DUMP_FRAMES=150 cargo run --release -p birdman64
```

**C patches:** fixes and small changes to the decompiled C live in
`crates/pw64-game/patches/` as unified diffs applied at build time.
**Rust ports:** replacements for C modules are tracked in `ported.txt`, which
keeps the port and the C in sync bit-for-bit. See
[docs/notes/native-build.md](docs/notes/native-build.md) and
[docs/notes/rust-port.md](docs/notes/rust-port.md).

**Developer environment variables** (the player ones are above; the
compile-time C macros at the bottom are part of the native layer, not
environment variables):

<details>
<summary>Full PW64_* variable list</summary>

| Variable | Effect |
|---|---|
| `PW64_HEADLESS=1` | no window (automation) |
| `PW64_MAX_RETRACES=<n>` | headless run, stop after n frames (implies headless) |
| `PW64_NO_THROTTLE=1` | skip idle time between frames |
| `PW64_NO_INPUT=1` | no gamepad thread |
| `PW64_TRACE_OS=1` | log OS-core thread events |
| `PW64_INPUT_SCRIPT=<file>` | scripted controller input |
| `PW64_DUMP_FRAMES=<n\|r1,r2..>` | dump frames to `tmp/frame_<retrace>.png` |
| `PW64_DUMP_HEIGHT=<px>` | PNG dump height (default 480) |
| `PW64_DUMP_DL=1` | also dump the draw list per dumped frame |
| `PW64_DUMP_PIXEL=x,y` | list which draws change that pixel |
| `PW64_WIN_SHOT=<n1,n2..>` | dump the presented window frame n to `tmp/win_<n>.png` |
| `PW64_FB_SHOTS=<n>` / `PW64_GFX_SHOTS=<n>` | headless: dump the first n RDRAM framebuffers / gfx outputs as PNGs |
| `PW64_SETTINGS_SHOT=<n>` / `PW64_SETTINGS_KEYS=<k1,k2..>` | settings-screen automation: open it at frame n, feed the keys, capture `tmp/win_settings.png` |
| `PW64_STOP_MILESTONE=1` | exit at the first boot milestone |
| `PW64_NO_GPU=1` | run without the wgpu renderer |
| `PW64_NO_DIALOGS=1` | suppress file-picker dialogs (automation) |
| `PW64_NO_EEPROM=1` | headless: don't auto-create a blank save |
| `PW64_PAD_ART_PREVIEW=1` | tests write the controller drawing PNGs |
| `PW64_PROFILE_RETRACES=1` | per-frame timing profile |
| `PW64_FALLBACK_ADAPTER` | force the fallback graphics adapter |
| `PW64_CAPTURE_DL=<n[,..]>` | capture real display lists (bench input) |
| `PW64_BENCH_CAPTURE=<file>` | replay a capture in the interpreter bench |
| `PW64_DUMP_TEX=1` | write texture-pack keys as PNGs |
| `PW64_AUDIO_VIRTUAL=1` | silent virtual audio device for stats |
| `PW64_DUMP_AUDIO=<file>` | dump the audio stream to a WAV |
| `PW64_AUDIO_STATS` | periodic audio counters and the Acmd histogram at exit |
| `PW64_AUDIO_METER=<1\|2>` | RMS level meter (music, SFX, reverb return) |
| `PW64_BLE_LIST=1` | scan for Bluetooth LE pads and exit |
| `PW64_MAP=<path>` | link a map file so crash addresses can be symbolized |
| `PW64_WATCH=<addr>` | x64 hardware write watchpoint at that address (crash debugging) |
| `PW64_BUILD_ONLY=1` | build or verify the game module and exit |
| `PW64_GAME_DLL=<module>` | load this game module instead of building it |
| `PW64_ZIG=<zig>` | zig binary for the C build (else the bundled one) |
| `PW64_CC=clang-cl\|clang` / `PW64_CLANG_CL=<path>` / `PW64_CLANG=<path>` | legacy LLVM C compiler override |
| `PW64_DECOMP_ZIP=<zip>` / `PW64_DECOMP_DIR=<dir>` | use a local decompilation instead of downloading |
| `PW64_GIT_HASH` | git commit embedded at build time (shown by `--version`) |
| `PW64_U32` / `PW64_PTR` / `PW64_LONG32` | native C layer type macros (pointer-width portability) |
| `PW64_SWAP` / `PW64_SWAP16` / `PW64_SWAP32` | byte-swap macros for big-endian ROM structs |
| `PW64_SEG` / `PW64_N64_BITMAP_SIZE` / `PW64_PHOTO_BITS` | native C layer macros (segment addresses, ROM struct and photo sizes) |
| `PW64_REF_DT` / `PW64_DT_MIN` / `PW64_DT_MAX` / `PW64_RATE_H` / `PW64_CLKID_SND` / `PW64_SND_FLUSH_SEC` | frame-rate scaling constants (reference dt, dt clamps, audio timing) |
| `PW64_ANCHOR_TAG` / `PW64_VANCHOR_TAG` / `PW64_WIDE_TAG_ON` / `PW64_WIDE_TAG_OFF` / `PW64_HUD_TAG_ON` / `PW64_HUD_TAG_OFF` | display-list tag constants for widescreen and HUD debug tooling |
| `PW64_NATIVE` / `PW64_NATIVE_H` / `PW64_ENDIAN_H` / `PW64_LOGF` / `PW64_VTX` / `PW64_PRESENT_MSG` / `PW64_CAM_HIST` | internal C-layer macros (native build switches, logging, debug hooks) |
| `PW64_EXPORT` / `PW64_THUNK` / `PW64_DLL_ABI` | C macros in the game module shim (DLL exports and ABI) |

</details>

## Contributing and forking

Contributions are welcome! Forks are welcome too: the original code here is
MIT licensed, so build whatever you like on top of it. Please read
[CONTRIBUTING.md](CONTRIBUTING.md) first: it explains how to build and test,
what the legal rules are (never commit or attach ROMs or extracted game
assets) and what a good bug report looks like.

## Credits

This project stands on the work of others:

| Project | Used for | License |
|---|---|---|
| [Pilotwings64Decomp](https://github.com/gcsmith/Pilotwings64Decomp) by Garrett Smith and contributors | Decompiled game source (downloaded and compiled at first run) and the filesystem format docs | MIT |
| [Pilotwings64Recomp](https://github.com/gcsmith/Pilotwings64Recomp) | Inspiration and a behavioural reference for enhancements (referenced only, never copied: GPL) | GPL-3.0 |
| [N64Recomp](https://github.com/N64Recomp/N64Recomp) / [RT64](https://github.com/rt64/rt64) | Inspiration for the recompilation and rendering approach, and texture-pack conventions | MIT |
| [skate-3-rust-engine](https://github.com/SK8-ENGINE/skate-3-rust-engine) | Inspiration for the Rust engine and "bring your own game" asset model (referenced, not copied) | none |
| [splat](https://github.com/ethteck/splat), [m2c](https://github.com/matt-kempster/m2c), [ido-static-recomp](https://github.com/decompals/ido-static-recomp) | Tooling used by the decompilation | various |
| The N64 decompilation and preservation community | Documentation of the N64 hardware, RSP microcode and libultra | various |

Rust crates we depend on are listed in `Cargo.lock`, each under its own
license.

## Legal

Birdman64 is an unofficial, non-commercial fan project. It is not affiliated with,
endorsed by or sponsored by Nintendo or Paradigm Entertainment. "Pilotwings" and
"Nintendo 64" are trademarks of Nintendo; we use them only to say which game this
port runs.

Screenshots show the game running in Birdman64 from the author's own cartridge
dump; they are not included in the download.

### What the download contains

- **Our own code:** the platform layer (a Rust replacement for the N64 operating
  system), the renderer, the audio engine, the launcher and our patches, under the MIT
  licence, built with open-source Rust libraries (wgpu, winit, egui and others). The patches ship as edit instructions ("keep n lines, drop n lines, insert
  this line") that carry only the lines we add, never the decompiled code around them.
  Like the few headers we supply, they name the game's functions, variables and data
  layouts where they change them, and some changed lines necessarily repeat the one
  statement they fix (for example with an added type cast).
- **A standard C compiler** ([Zig](https://ziglang.org), MIT licence), which the launcher
  uses on your computer.
- **Licence notices** for everything above and for every library we use.

It contains **no game code, no game assets and no ROM data**. Birdman64 never
downloads a ROM and never uploads anything from yours.

### What happens on your computer

You provide your own copy of the game (a dump of the US cartridge). On first start, the
launcher downloads the public
[Pilotwings 64 decompilation](https://github.com/gcsmith/Pilotwings64Decomp) from its own
repository, at a fixed version that is checked against a checksum, and compiles it on
your computer so that your copy of the game can run on a PC. Graphics, sound and all
other game data are read from your ROM while you play. The result stays on your machine.

### Why it is built this way

European copyright law allows the lawful owner of a program to reproduce, translate and
adapt it where that is necessary to use it for its intended purpose, including to correct
errors (EU Software Directive 2009/24/EC, article 5(1), implemented in the Netherlands in
articles 45h to 45n of the Auteurswet). The Court of Justice of the EU confirmed in
*Top System* (C-13/20, 2021) that this can even include decompiling the program. The N64
is no longer made, so playing a cartridge you own on current hardware requires adapting
its program to that hardware. Birdman64 is a tool that lets owners make that adaptation
for their own copy, on their own machine. We do not distribute the adapted game.

This is our good-faith reading of the law, not legal advice, and fan ports like this one
have not been tested in court. If you are a rights holder with a concern, please open an
issue and we will respond promptly.

### Your part

- Only use a ROM that you dumped from a cartridge you own.
- Never ask for, link to or share ROMs or extracted game files in issues, discussions or
  pull requests. Such posts are removed.

## License

The original code in this repository is released under the MIT License. It
does not grant any rights to Nintendo's or Paradigm's intellectual property.
