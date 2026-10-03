# Roadmap

Birdman64 is a native PC port of Pilotwings 64: the game's own code, compiled and
run on a Rust runtime instead of the N64. This file tracks where the port stands
and what is left before and after the 1.0 release. Maintainers keep it current;
ideas and offers to help are always welcome in issues.

## Where it stands

The port is playable end to end: every licence class, the bonus games and the
full single-player game. It boots from your own US ROM, builds the game code on
first run, and plays with keyboard or gamepads. Highlights: widescreen up to
21:9 with the 3D view filling the screen, a frame loop that runs at the
monitor's refresh rate (144 Hz and beyond), window, borderless and exclusive
fullscreen with a render resolution setting, an in-game settings menu with
control rebinding (it shows the N64 pad and what each button does), sensible
default layouts for Xbox, PlayStation and Switch pads, an OLED care mode, and
saves in your user folder.

## Before 1.0

- A full play pass on real machines: a clean Windows install with no developer
  tools, the first-run ROM dialogs, every settings page, real gamepads, display
  modes (including alt-tab out of exclusive fullscreen), dusk, night and snow
  levels, the photo album, and saves surviving a restart.
- The Linux AppImage on a real desktop.
- A full human playthrough: every licence, test and bonus stage finishable.

## After 1.0

- Renderer accuracy gaps: dither, coverage anti-aliasing, chroma key, detail
  textures, canopy edge fringes, lighting accuracy.
- Present ticks driven by vblank.
- HDR output (until then, Windows 11 Auto HDR may already apply).
- Switch 2 controllers over Bluetooth verified on real hardware.
- More of the game code ported from C to Rust.
- Ghost replays, later perhaps multiplayer.
- An Android port.

## See also

- README.md: what Birdman64 is, how to install, play and build it.
- CONTRIBUTING.md: how to build, test and send changes.
- docs/notes/: design notes for anyone who wants to dig deeper.
