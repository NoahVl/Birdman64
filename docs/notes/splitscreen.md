# Split-screen (2-player local)

Status: two hang-glider pilots fly the same test (top view = player 1, bottom = player 2),
each on its own controller and camera, seeing each other. Other vehicles fall back to one
player. Everything is gated: `pw64_level_players == 1` runs the original code paths.
Paths are under `decomp/src` unless noted; patches under `crates/pw64-game/patches/`.

## Architecture
- **Flag:** `pw64_split` (`native/src/pw64_split.c`, set before boot by
  `pw64_game::set_split` from `input::splitscreen_enabled()`). gameInit sets
  `pw64_players = pw64_split ? 2 : 1`; `pw64_level_players` = players in the current test
  (1 for vehicles not supported yet; logged via `PW64_LOGF`, visible with `PW64_TRACE_OS`).
- **Player context switch** (`native/src/pw64_players.c`): per-player state lives in the
  game's own globals; only the current player's copy is live. Registered regions
  (`PW64_REG(sym)` inside per-file `pw64_reg_<file>()`, called from `pw64_players_init`;
  explicit, no constructors because of the three build modes) are swapped by
  `pw64_player_switch(i)`. Fixed arena: 64 regions, 32 KB per slot, `pw64_fatal` on overflow.
  `pw64_players_sync()` copies live state to the other slots (used before P1's vehicle load,
  so P2 starts from the reset HUD/radar/shadow state). Function-local statics are hoisted to
  file scope to be registered.
- **`unkC`:** `game.h` can't be patched (build.rs only patches `.c` under `src/` and
  `include/` headers; a src-header patch would leave unpatched TUs on the old layout). So
  `unkC` stays `[1]`, `unk9C` stays 0, and `D_80362698.unkC[0]` is a registered region.
  `.c`-defined arrays grew: `D_80362748[2]` (vehicle data), `D_80362E88[2]` (cameras),
  `D_80364210[2]` (task state); player i's `unkC[0]` points at index i.
- **Controllers:** `kernel/controller.c.patch` maps pad 0 to the current player's pad
  (`PW64_CONT`) in the five read functions, so every hard-wired "controller 0"/`unk9C`/
  `contIdx` read means the current player (menus run with player 0 live).
- **Level load:** world once (levelLoad, terra, env, taskLoad); then `pw64_setup_players`:
  per player switch(i), `func_8034B5E0(i)` (chan HUD callback NULL for P2), terra/env on
  channel i, `pw64_split_viewport`, vehicle load with `contIdx = i`, P2 offset 10 m along the
  start frame's x (`func_802EDDEC`, patched to use `unkC[].vehicleData`). Teardown
  `pw64_leave_players` (from func_802ECE94); Retry sets up a fresh 2-pilot test.
- **Frame** (gameUpdateStateTestUpdate): world once; per player: vehicle update + status +
  `pw64_task_player_update`; `pw64_task_world_update` once with P1 live (therm, wind, env,
  balls, falco, fire/splash/smoke, env sound). Draw: `func_8034B624` per player under
  switch(i). `taskFrameUpdate` = both parts, original order, when single-player.
- **Viewports:** `pw64_split_viewport` (camera.c.patch) halves the subscreen (N64 y is
  bottom-up) and reshapes the camera's clip frustum to the view's aspect (wider: keep x).
- **Budgets** (shared by both world passes): DL `PW64_DL_LEN 16384` (was 4200; overflow
  silently skips the frame), `UV_GFX_NUM_MATRICES 0x400` (0x15E), `LOOKS 0x60` (0x1E),
  `UV_GEOM_NUM_VERTICES 0x800` (0x320). Also apply to single-player (headroom only).

## Gotchas found
- Matrix stack overflow in pass 2 → red/blue streaks (garbage matrices): budgets above.
- The kernel z clear (`uvGfx_80222A98`) sets a full-screen scissor right after
  `uvGfxViewport`: P2's near geometry and the sun flare drew over P1's half. chan.c.patch
  re-issues `uvGfxViewport(arg0)` after both z clears when split.
- fdr.c flight recorder is P1-only, else its clock runs 2×. Photo shutter is P1-only.
- `hgSoundEvent13` removed the last-registered sound callback (P2's): now removes its own.
- At the start the other glider is just outside the FOV (camera ~18 m back, 10 m apart).

## Remaining work (from the per-player statics audit)
- **HUD per view:** only P1's HUD is drawn and its bottom row is hidden by P2's view. Plan:
  tag each view's 2D draws and let pw64-gfx scale them into the view rect.
- **Objectives per player:** hit state lives in shared arrays (rings, balls, missile targets,
  hover pads, ball targets, bonus stars, hopper goals, photos) and hits change shared dobjs;
  the kernel has no per-view dobj visibility. Today only P1 hits objectives (landing pads:
  both). One player finishing calls `taskDeinit()` (task.c:388) for everyone.
- **Other vehicles:** register their audit lists; first fix our rate patches that
  `pw64_fatal` on a second vehicle of the same type (birdman1 `sPw64FlutterOwner`,
  code_BA190 `sPw64DriftOwner`). Gyro `D_8034F380` keeps last frame's buttons (Z edge).
  Cannonball/skydiving scoring statics; skydiving formation partners need a second set.
- **Audio:** engine-sound events are broadcast to both players' handlers; one music
  sequence; one listener (kernel `D_80250E80[1]`), follows P1.
- **Hard-wired player 0 in gameplay:** falco.c (bird targets P1), snap.c shutter,
  env_sound.c:494, task.c:358-372, game.c:770 (splash billboards on camera 0), cannonball.c:380,
  code_B6580.c:24, code_BA190.c:235, skydiving.c:369.
- **Pause:** P2 can't pause; the pause menu likely redraws only view 0. Never run
  split-screen in attract/demo playback (single stream).
- Ground starts: P2's offset keeps P1's ground height. Widescreen/fill-screen + split
  untested (the view scissor may fight the renderer's Hor+ extension). Cockpit views that
  hide their own pilot dobj hide it for the other view too (dobj state is global).

## Input (per-player controller assignment)
- `headless.rs` `PADS[4]` + `set_controller(n, ..)` + `set_controllers_present(n)` (pad 2
  present only with split-screen on). `input.rs` keeps a `Pad` per source and `route()`
  splits them by `P2Input` (`[splitscreen] p2_input` / `PW64_P2_INPUT`; default every
  gilrs pad → P2, keyboard → P1). `PW64_INPUT_SCRIPT_P2` drives controller 2. input.md.
- Settings > Controls: "Split-screen (2 players)" (restart) + "Player 2 controller" (live).
  settings.md.
- Open: "press a button for player 2" capture; a single Joy-Con held sideways as its own pad.

## Decisions (2026-10-07)
- **Modes:** both pilots on the same test, each scored on their own, results for both; and
  free flight. Pilots see each other.
- **Interactions toggle:** on = pilots collide (new sphere test → existing crash path);
  off = pass through. Objectives per player either way. Default off.
- **Layout:** top/bottom (default) and left/right in settings. Deferred: one window per
  player / multi-monitor (`fb_present` already takes a target rect).
- **Audio:** both players mixed; optional pan per side for left/right.
- **Built as flag-gated C patches now** rather than after a Rust port of the app code: the
  C side is small, and split-off keeps single-player identical.

## Frame cost
Each view has half the pixels, so GPU fill stays about the same; CPU per frame roughly
doubles in DL interpretation and draw calls. Measured (two gliders in flight, headless,
`PW64_PROFILE_RETRACES=1`): 1.8 ms/retrace avg, DL interp 1.3 ms/gfx task. If needed later:
shorter draw distance per view or a lower render scale in split-screen.
