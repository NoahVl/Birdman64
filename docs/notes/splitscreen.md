# Split-screen (2-player local): feasibility notes

Status: **rendering spike (SS-S1) landed** behind the C flag `pw64_split`
(`native/src/pw64_split.c`, set by `pw64_game::set_split`; input side was
already wired). With it on: one player still, flight view drawn twice per
frame — channel 0 = normal camera (top half), channel 1 = rear view from the
same spot (bottom half). Proves two channels, two viewports, two world passes,
DL budget. Paths are under `decomp/src` unless noted.

## Spike (SS-S1, what landed where)
- `Camera D_80362E88[1]` → `[2]` (`game.c.patch`): camera-1 storage; BSS symbols
  resolve by name in the native build, so the following `D_80364210` just moves.
  No `sizeof` on it anywhere; `gameInit`'s loop stays `i < 1`, the flag adds
  `cameraInit(1, &D_80362E88[1])`. The other per-player arrays (`D_80362748`
  vehicle data, `D_80364210` task state) and `Unk80362690.unkC` stay `[1]`:
  one player, no second pilot state. Note: the build system only patches
  `.c` files under `src/` (build.rs rejects a src-header patch — unpatched
  TUs would silently include the decomp copy) and a `native/include` shadow
  can't win because quoted includes search the includer's directory first —
  so `game.h` is unpatchable. Plan: don't grow `unkC`; the context swap copies
  each player's `Unk80362690_Unk0` in/out of `unkC[0]` (`unk9C` stays 0), with
  the per-player copies in a native array. Arrays defined in `.c` files
  (`D_80362748`, `D_80364210`) can grow in their patches.
- `pw64_split_viewport` (camera.c.patch): halves the subscreen (top = cam 0,
  bottom = cam 1; N64 y is bottom-up) and reshapes each camera's clip
  frustum to the viewport's aspect (Hor+: keep x, shrink y). Called once per
  level load; `cameraSetViewport` is only ever called from `cameraInit`, and
  nothing in the flight frame resets a camera's `viewX0..`/`clipX0..` after
  that (`camera_802D50D0` rebuilds the channel clip *from* them each frame —
  that's the design, not a reset).
- `pw64_split_draw_rear` (camera.c.patch): runs `func_8034B624`'s kernel
  sequence for channel 1 with cam 0's `unk108` matrix, rows 0/1 of the
  rotation columns negated (180° turn about up). Chan 1 has no HUD callback
  (`uvChan_80204AB0(1, 1, NULL)`).
- DL buffer `gGfxDisplayListBase[2][PW64_DL_LEN]` with `PW64_DL_LEN 16384`
  (pw64_native.h; was 4200) — two world passes nearly double the count; on
  overflow `uvGfxEnd` silently skips the frame (prints "display list
  overflow"), so watch for a frozen image.
- Rust: `pw64_game::set_split(u8)` (static + dylib paths), `main.rs` sets it
  before boot from `input::splitscreen_enabled()` (renamed from `enabled`).
- Verified: channel 1 draws terrain (a temporary 90° side view showed the coast; the rear view
  just faces the sea at the hang-glider start). Cost: DL interp 0.42 ms/gfx task averaged
  over a whole split run, no overflow.
- Bug: facing the sun, translucent screen-space quads (likely the sun lens flare) draw over
  the whole screen, not clipped to their view: they show in the other half too. Needs a
  per-view scissor or clip.
- Not handled: the birdman change-vehicle path (`func_802EE14C`, game.c
  ~1215) reloads a level without re-running the split block, so chan 1's
  terra/env go stale there (spike: acceptable, one player).

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
  (done in the spike: `PW64_DL_LEN 16384`, graphics.c.patch).
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
- **Done (settings):** Controls page rows "Split-screen (2 players)" (needs a restart) and
  "Player 2 controller" (live). Details: settings.md "Split-screen rows on the Controls page".
- **Open:** a "press a button on the controller for player 2" capture
  (reuse `start_pad_capture`).
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
