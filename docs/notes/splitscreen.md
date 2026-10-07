# Split-screen (2-player local): feasibility notes

Status: research only, nothing implemented. Paths are under `decomp/src` unless noted.

## What the original code already has
- The game singleton `Unk80362690` (`app/game.h:94`) is shaped for N players: `unkC[1]`
  (per-player struct), `unk98` (player count, set to 1 in `gameInit`, never read),
  `unk9C` (current player index, ~300 uses as `unkC[unk9C]`).
- Per-player globals are all arrays of 1 (`app/game.c:48-50`): `D_80362748[1][0x740]` vehicle
  data, `Camera D_80362E88[1]`, `D_80364210[1]` task/save state. `gameInit` wires them in a
  `for (i = 0; i < 1; i++)` loop (`game.c:140`), including `cameraInit(i, ...)`.
- Camera index = kernel channel id; the kernel has 2 channels (`kernel/chan.c:14`) and 4
  viewport slots (`kernel/graphics.c:32`). One world pass = `func_8034B624(camera)`
  (`app/code_D2B10.c:14`), keyed off the channel, so it can run twice per frame.
  `snap.c:941` already changes a channel viewport mid-frame.
- Input goes through `demoButtonPress/demoGetInputs(idx, ...)`; vehicles keep a `contIdx`
  in their data. ~18 call sites pass a literal `0` (mostly menus).
- Kernel polls up to 6 pads (`kernel/controller.c`); `osContGetReadData` fills all of them.

## What is single-player
- Seven "current vehicle" pointer globals `D_803676F0..708` (`game.c:51-57`), set from
  `D_80362748[unk9C]` (`game.c:~1116`).
- HUD: one static `sHudState` (`app/hud.c:236`), screen-fixed geometry.
- Mission objects compare against the current player (e.g. `app/balls.c:109`); scoring writes
  `unkC[unk9C].unk74` (current player's save slot).
- One game-state machine / pause / results flow. Engine sounds read the current player
  (`bmsound.c:132`, `cbsound.c:151`).
- DL buffer `gGfxDisplayListBase[2][4200]` (`graphics.c:31`): two world passes need more room
  (easy to enlarge with a native patch).
- Our side: `crates/pw64-platform/src/headless.rs` `osContInit` reports only pad 1, pads 2-4
  return `CONT_NO_RESPONSE_ERROR`; `crates/birdman64/src/input.rs` merges every device into
  pad 1. `pw64-gfx` already honours per-draw viewport + scissor, so two viewports inside one
  frame need no renderer or window change.

## Suggested approach: per-player context swap
Instead of threading a player argument through ~300 sites: arrays → `[2]`, then per frame
for each player `i`: set `unk9C = i`, re-point the vehicle globals and swap in that player's
HUD state, run that player's update, then draw each camera into its own viewport. This is
how the `unk9C` design looks meant to be used.
Main risk: file-level statics in vehicle/task/HUD code that hold per-player state and would
be shared without anyone noticing. Audit every non-const static in `app/` before relying on the
swap.

## Requirement: per-player controller assignment
Every input method, the keyboard included, must be assignable to a player on its own: e.g.
keyboard = P1 and a Joy-Con 2 pair (BLE) = P2. That needs a setting.
- **Done (input core):** `headless.rs` `PADS[4]` + `set_controller(n, ..)` +
  `set_controllers_present(n)` (before boot; pad 2 present only with split-screen on).
  `input.rs` keeps a `Pad` per source (each gilrs pad by name, keyboard, BLE virtual pad
  (L+R Joy-Con merged), script, script2); pure `route()` splits them by `P2Input`
  (`[splitscreen] p2_input` / `PW64_P2_INPUT`; default `"gamepad"` = every gilrs pad → P2,
  keyboard → P1). `PW64_INPUT_SCRIPT_P2` drives controller 2 for tests. Details: input.md.
- **Open:** Settings > Controls: per player "Controller: Keyboard / <pad name> / Joy-Con 2 / none",
  plus a "press a button on the controller for player 2" capture (reuse `start_pad_capture`).
  Persist in `pw64.toml` by device name/kind (gilrs ids are not stable across runs);
  an unassigned new device goes to P1.
- Bindings: the keyboard table is already its own; a shared gamepad table is fine at first.
- Settings overlay navigation and its open button stay reachable from every device.
- Later option: a single Joy-Con held sideways as its own pad (one pair = two players) would
  need a sideways mapping in `ble.rs` instead of the L+R merge.

## Decisions (2026-10-07)
- **Modes:** (1) both pilots on the same test, each scored on their own, results screen for
  both; (2) free flight. Pilots see each other: each player's vehicle + pilot must be a drawn
  dobj in the shared world (check that vehicle code doesn't hide/skip the other model).
- **Interactions toggle** (setting): on = pilots can collide (new: the game has no
  vehicle-vs-vehicle collision; add a sphere test that triggers the existing crash path);
  off = pass through each other. Objectives (rings, balloons, targets) are per player in
  both cases, so a test stays fair. Default: off.
- **Layout:** top/bottom and left/right, chosen in settings (both are just two viewport
  rects; per-viewport aspect through the widescreen Hor+ projection; HUD laid out in each
  viewport). Default top/bottom. Deferred: one window per player / multi-monitor
  (each window presents its own viewport rect; `fb_present` already takes a target rect).
- **Audio:** both players' sounds mixed. Engine-sound code (`bmsound.c`, `cbsound.c`, …)
  assumes one instance per vehicle: check its statics; raise the synth voice count if two
  engines starve the music. Optional: pan each player toward their side for left/right.
- **Timing:** do it now as C patches, not after the Rust port (see below).

## Frame cost
Not 2×: each viewport has half the pixels, so GPU fill stays about the same. What doubles
is CPU work per frame: the DL interpreter (~0.5-1 ms/frame after the interp speed-up,
renderer.md) and the draw calls. Both views go into one DL → one frame → one GPU pass
(no second submission). Measure with `PW64_PROFILE_RETRACES=1` before optimising. If needed:
a shorter draw distance / coarser LOD per view, or a lower render scale in split-screen.
Sharing work between the two views (one cull for both) doesn't help much, because the
cameras differ.

## Timing: why now, in C
- The C side is mostly array sizes + a per-player loop + the context swap: little code, so
  little to rewrite later. Phase 7 has ported only kernel leaves; app ports are far off and
  would port the patched C.
- Keep it all behind a split-screen flag (off = original code paths), so single-player and
  the Phase 7 differential tests stay identical to the original.
- The input/settings work is Rust either way and is useful on its own.
