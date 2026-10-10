# Split-screen (2-player local)

Status: two pilots (every vehicle: hang glider, gyrocopter, rocket belt, jumble hopper, birdman,
cannonball, sky diving) fly the same test (top/left view = player 1, bottom/right = player 2),
each on its own controller and camera, seeing each other. Everything is gated: `pw64_level_players == 1` runs the
original code paths. Hopper: only the crash head-shake sfx one-shots (`D_8036905C`) are
per-player; the collision query scratch (`D_80368C00`..) stays shared.
Paths are under `decomp/src` unless noted; patches under `crates/pw64-game/patches/`.

## Architecture
- **Flag:** `pw64_split` (`native/src/pw64_split.c`, set before boot by
  `pw64_game::set_split` from `input::splitscreen_enabled()`). gameInit sets
  `pw64_players = pw64_split ? 2 : 1`; `pw64_level_players` = players in the current test
  (1 for vehicles not supported yet; logged via `PW64_LOGF`, visible with `PW64_TRACE_OS`).
- **Live switch:** title main menu item 3 "1 Player"/"2 Players" (title_screen.c.patch, last so Start/Option keep
  their places; box grows down; hint "Connect a second controller" while `pw64_split_hint`) calls
  `pw64_split_apply` (game.c.patch: P2 camera/results/registry once, `pw64_cont_enable` pad 2). Settings row →
  `pw64_split_req`, applied by `pw64_split_poll` in title..test-menu states (none mid-test). Rust `input::split_tick`
  (each retrace) follows `pw64_split`: controller presence, `set_p2`, saves `[splitscreen] enabled`.
- **Player context switch** (`native/src/pw64_players.c`): per-player state lives in the
  game's own globals; only the current player's copy is live. Registered regions
  (`PW64_REG(sym)` inside per-file `pw64_reg_<file>()`, called from `pw64_players_init`;
  explicit, no constructors because of the three build modes) are swapped by
  `pw64_player_switch(i)`. Fixed arena: 192 regions, 80 KB per slot, `pw64_fatal` on overflow.
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
  per player switch(i), `func_8034B5E0(i)` (chan i + HUD callback), terra/env on
  channel i, `pw64_split_viewport`, vehicle load with `contIdx = i`, P2 offset 10 m along the
  start frame's x (`func_802EDDEC`, patched to use `unkC[].vehicleData`). Teardown
  `pw64_leave_players` (from func_802ECE94); Retry sets up a fresh 2-pilot test.
- **Birdman load path:** `gameUpdateStateTestSetup`'s switch never loads birdman
  (`case VEHICLE_BIRDMAN: return 0;`); the bonus path `func_802EE14C` (game.c ~1181) does
  (`birdLoadLevel` on `D_80367704`) — same `pw64_setup_players` treatment as jumble hopper. Statics registered per player: birdman0, bmsound (gyro-style local `uvEventRemoveCb` info), birdman1 (flutter; a 2nd BirdmanData hit `pw64_fatal` without it), birdman2.
- **Frame** (gameUpdateStateTestUpdate): world once; per player: vehicle update + status +
  `pw64_task_player_update`; `pw64_task_world_update` once with P1 live (therm, wind, env,
  balls, falco, fire/splash/smoke, env sound). Draw: `func_8034B624` per player under
  switch(i). `taskFrameUpdate` = both parts, original order, when single-player.
- **Viewports:** `pw64_split_viewport` (camera.c.patch) halves the subscreen top/bottom or
  left/right (`pw64_split_layout`, `[splitscreen] layout`/`PW64_SPLIT_LAYOUT=tb|lr`, latched
  into `pw64_split_lr` at test start; 1 px seam; N64 y bottom-up) and reshapes the clip
  frustum to the shown aspect (wider: keep x). Left/right on a wider output (widescreen or
  fill): the world channel is tagged WIDE + "PWS" rect outside the HUD bracket and pw64-gfx
  `split_world_placement` stretches the view from its outer edge to the output edge; the
  frustum is widened by 120·aspect−150 N64 px to match (not Hor+ uniform: that would
  off-centre each view). HUD scale = min(view_h/214, view_w/300): left/right fits by width.
- **HUD per view:** every player's channel keeps the HUD callback (`func_8034B6F8`), so each
  HUD draws for 320×240 inside its own channel pass. `hudMainRender` brackets it with
  `pw64_hud_split_tag` (G_NOOP "PWS": view rect from `D_80261730[pw64_player_cur]`) and the
  anchor tags are emitted in split too; pw64-gfx lays the subscreen frame out in the view
  (scale: see Viewports; a whole miniature, so columns never overlap), each element at its
  L/C/R anchor. C-side moves can't work (the 2D
  texrects don't scale; the 0..320 HUD scissor). font.c advanced its sprite-DL offset by the
  message count only: the 2nd flush per frame overwrote P1's text (patched, split only).
  splash.c ripples step in P1's pass only (drawn per view). Shutter bars don't close fully.
- **Budgets** (shared by both world passes): DL `PW64_DL_LEN 16384` (was 4200; overflow
  silently skips the frame), `UV_GFX_NUM_MATRICES 0x400` (0x15E), `LOOKS 0x60` (0x1E),
  `UV_GEOM_NUM_VERTICES 0x800` (0x320). Also apply to single-player (headroom only).
- **Objectives per player** (task.c.patch `pw64_reg_task`): each objective module's arrays
  (rings, balls, missile targets, hover pads, ball goals, bonus stars, hopper goals) plus
  task.c's loaded/all-done/complete flags and `unkA0` are registered regions. Player i's copy
  gets its own dobjs: on its first `pw64_task_player_update` every objId is replaced by
  `pw64_dobj_clone` (dobj.c.patch). `pw64_dobj_owner[id]` = player+1 (0 shared; reset on
  free); `_uvDobjsDraw`, `uvDobj_8021771C` and both dobj collision loops (code_8170.c) skip
  another player's dobjs, so hits free/re-model only the player's own. Modules `pw64_dobj_claim`
  dobjs they allocate mid-test (child rings, split balls, next hover pad). Balls move per player.
  `taskDeinitLevel` frees the other players' copies. P1-only: falcos, teleports, the
  decomp's `unkC[0].veh = ROCKET_BELT` on a ring/ball goal; bonus stars don't warp in split.
- **Test end + results** (game.c.patch, results.c.patch): any per-player end (status 5,
  crash/landing flow, goal, fuel) only marks that player done (`pw64_player_finish`: landing
  pose + `func_8032B560` score *at the finish*, since a later time-up sets the global `unkA1`;
  HUD off, "FINISHED" from hud.c.patch). Done players skip their update (frozen: their
  results animation would post the landing music + event 0x12 to everyone). Land-now/time-up
  run per flying player (`pw64_test_time_split`); results when all are done. Results
  (`pw64_results_split`): every view keeps drawing; results.c reads `D_80364210[unk9C=0]`, so
  "Next" on P1's tally swaps each other player's struct in for a Next-only tally, then P1's
  class totals + Retry/Quit (applies to all). Only P1 is saved. Tallies/replays full screen (see Remaining work).
- **Share best score** (`pw64_split_share`, `[splitscreen] share_scores` / `PW64_SPLIT_SHARE`, Controls row):
  after the tallies, if another player's run total (read back from its entry) beats P1's, `pw64_share_best`
  copies that run's header (bytes before `unk40`) into `D_80364210[0]` and reruns `func_8032B560` with that
  player live and its finish-time `unkA1` (`sPw64TimeUp`; the global may be set by a later time-up).
- `uvEventRemoveCb` (decomp bug, harmless with 1 player): cleared `unk0[count]`, which for a full list is
  `.count` itself → -1, then the next add corrupts the previous event's count (crash). Clears `[count-1]`.
- **Free flight** (test_menu.c.patch, game.c.patch `pw64_free_*`): split-only item in the
  test menu's bottom-right slot (Sample photo's; replaces it on Shutter Bug tests) for
  `pw64_split_vehicle`s; returns like Start with `pw64_free_flight = 1` (cleared on menu entry
  and in func_802ECE94). The highlighted test's level/start, `unkA0 = 0` before taskLoad (every
  objective loader returns early). No time checks/timer/penalty texts/save; fuel topped up per
  frame. A player's end → crash pose + white fade, frozen 1.6 s, `pw64_free_respawn`: vehicle
  EnterLeave (in-place reset to db_getstart) + crash latches + P2 offset; a landing respawns at
  its own sequence's white step (unk7C 3).
- **P2 pilot** (`pw64_p2_pilot`, -1 = P1's): after P1's pick, game.c.patch syncs + switches to P2 and reruns
  `func_802DD200` (P2's pad via PW64_CONT; B → P1's pass again); pilot_select.c.patch labels "PLAYER n" and
  keeps a cursor per player. `pw64_setup_players`/`pw64_cannon_setup` set P2's `unkC[0].pilot`; level.c.patch
  `levelComputeAppend` also appends P2's pilot level (models). The hang glider pose tree (code_51E30 globals:
  per-pilot anim ids + pointers into hgData) is registered per player. `PW64_P2_PILOT` (PW64_START boots only).
- **Pause** (`pw64_pause_player`/`pw64_pause_menu`): any player's Start pauses everyone (not
  in their own landing/crash sequence unless finished or in free flight); the menu runs with
  the pauser live, so their pad (PW64_CONT) drives it; the choice runs with P1 live. Split
  menu = Continue/Quit (replay_screen.c.patch): "Check map" reloads level + task (P1's
  objectives only, others' copies freed), so it's dropped, as is P1's "Check photos".
- **P2 start spot** (`pw64_offset_start`): ground starts (start ≤ 3 m above ground) take the
  first of right/left/back/forward at 10 m then 5 m that `db_getgnd` finds unblocked, not
  water/sea and flat (5 probes, 2 m apart, within 0.5 m of the start's ground); air starts
  keep 10 m right. a:rb:1 (pier): 10 m right was water, P2 crashed = "FINISHED" at once.
- **Collisions** (game.c.patch `pw64_collide_*`): `pw64_split_collide` latched at test setup;
  once per frame after both vehicle updates, sphere test on the vehicles' world matrices
  (radius 4 m glider, 3 gyro, 2.5 birdman, 1 rocket belt/hopper; P2 starts 10 m away), skipped
  for done/respawning players and non-flying states. Hit → per player under
  switch(i), the vehicle's own hard-impact state (glider/gyro/belt/birdman tumble state → crash
  → their unk7C test-end or free-flight respawn; hopper falls over, test goes on).
- **Glyph encoding** (`native/src/pw64_text.c`): font 6 glyph = index in its STRG table:
  `0-9` 0x00, `A-Z` 0x0A, `a-z` 0x24, space 0x42, punctuation after; 0x60.. = bold 0-9A-Za-z
  (main/pause menus). Strings end 0xFFE, -1. `pw64_text_encode[_style]` builds them (bold:
  letters/digits only; missing chars → space).

## Gotchas found
- Matrix stack overflow in pass 2 → red/blue streaks (garbage matrices): budgets above.
- The kernel z clear (`uvGfx_80222A98`) sets a full-screen scissor right after
  `uvGfxViewport`: P2's near geometry and the sun flare drew over P1's half. chan.c.patch
  re-issues `uvGfxViewport(arg0)` after both z clears when split.
- fdr.c flight recorder: one per player (`sPw64Fdr[pw64_player_cur]`, every fdr global a
  macro onto it; not a registered region: rings point into their own arrays, ~32 KB/switch).
  Test setup's start (P1 live) resets all (`PW64_FDR_ALL`).
- **Photos** (snap.c.patch `pw64_reg_snap`): shutter latches + this test's photos/count are per player
  (setup's `pw64_players_sync` copies P1's fresh `snapInit`); pad reads `PW64_CONT(0)`. Scored at each
  finish with the player live. Results: each tally's Check photo runs `func_8033E3A8(2)` with that player
  live (the kernel's pad 0 = the live player's, so P2 browses with their pad); kept photos go to the
  shared album (`D_80373060`, P1's save file). The viewer renders on channel 0 (fine: full screen then).
- `hgSoundEvent13` removed the last-registered sound callback (P2's): now removes its own.
  Same fix for the rocket belt (`rbSoundEvent13`) and the gyro (`gyrocopter_sound.c`
  func_8030B168: static D_80369E50 rebuilt per gyro with the caller's `gcData`).
- At the start the other glider is just outside the FOV (camera ~18 m back, 10 m apart).
- Leaving split left camera 0 on the top-half viewport (test menu map drawn in the top half
  only): `pw64_leave_players` restores it (`pw64_split_viewport(cam, -1)`).
- **Gyro reticle** (code_7CF30.c `func_802F6DC8`): reticleY = 107·tan6°·zoom/clipY1 HUD px,
  i.e. assumes the projection's half height = 107 px and HUD scale 1. Real half height = the
  `uvGfxClipRect` guard viewport (view ±5 px: 112 full, 58 top/bottom); the split HUD scale k
  multiplies the offset. Measured (beginner gc 1, zoom 0.8) offset drawn/true in N64 rows: single
  24.6/26.9 (N64 too), tb 25.2/28.1, lr 4:3 12.5/26.9, lr 16:9 17.8/26.9. Fixed in split only:
  107 → guard_half/k (`pw64_split_reticle_half`, camera.c.patch) → tb 27.75, lr 27.0.
  Open: the tb frustum is sized from the 300×106 view, not the guard viewport (~6 % y stretch).
- Missiles (`func_802F7178`) fly a fixed world ray 6° below the fire-time camera; the camera
  keeps flying forward/up, so the missile always drifts below the reticle on screen (original).
- Test runs: an exe copied out of `target/` with no `pw64.eep` beside it uses the per-user
  data dir (real save + toml): set `PW64_DATA_DIR` (and `PW64_EEP`) to the run dir.

## Remaining work (from the per-player statics audit)
- **Dobj cap:** dobjs are capped at 100 (each player's objectives use their own; beginner gc 1 = 3 rings,
  b:rb:2 = 15 rings → up to id 75); a failed clone logs and leaves that object missing for P2.
- Gyro `D_8034F380` keeps last frame's buttons (Z edge).
- **Sky diving** (skydiving.c.patch `pw64_sky_*`): each player has its own formation: partners
  + slot shadow are owned dobjs (other view hides them), P2's whole formation 10 m right
  (`pw64_sky_offset`); scores, countdown, drift registered. Rate owner checks (birdman1,
  code_BA190) re-own instead of `pw64_fatal`: Retry's `pw64_players_sync` copied P1's owner.
  Island level/pads load once (first diver below; terraId 9 = still sky); divers see each
  other only while both in sky or both below. Count text printed per view. No free flight.
- **Cannonball** (cannonball.c.patch `pw64_cannon_*`, game.c.patch `pw64_cannon_setup/frame_split`):
  lockstep rounds: both aim+fire the same shot; a landed player watches its fly-out until every
  shot is down + 3 s, then one `pw64_cannon_next_round` (target tally P1 → Next → P2, a new
  target's level loaded once then each cannon, or RESULTS_CB). Stays in TEST_UPDATE (no per-shot
  RESULTS state). Scoring statics, aim sweep, fly-out camera, wobble (code_60020) registered;
  TEXT_C_POINTS is one shared string: re-formatted per view before each draw. Each cannon dobj
  is owned (same pillar), pilots shared. cbsound removes its own cb. P2 bests are session-only.
- **Audio** (both mixed; kernel audio_event.c/audio_emitter.c.patch): a handler registered with 0x16 in its list (= vehicle sound) is owned by the live player and runs with its context live; posts in `pw64_evt_scope` i (per-player loops, nested in an owned handler) reach only i's handlers, unscoped ones reach all. World handlers (env, snd) get unscoped posts only.
  Finish/free-flight end: scoped 0x12 + `pw64_evt_mute` bit (blocks 1/0x13/0x16, not 0xD teardown); respawn re-arms crash latches, unmutes, 0x13; mute cleared at the tally.
  Engines are non-positional (always local). Positional emitters use the nearest player's listener (`pw64_listener_set`, P2's pose/camera).
  Hang glider/gyro crashes already self-muted per vehicle (P1 re-unmuted next 0x16), so the A/B WAVs barely differ; the leak hit env-sound fade and non-crash finishes. Open: one music sequence (P2's crash stinger replaces P1's music).
- **Hard-wired player 0 in gameplay:** falco.c (bird targets P1), snap.c shutter,
  env_sound.c:494, task.c:358-372, game.c:770 (splash billboards on camera 0).
  Sky diving's pad-0 reads (code_B6580.c:24, code_BA190.c:235, skydiving.c:369) follow PW64_CONT.
- **Free flight:** respawn audio (unmute + 0x13, see Audio) not run-tested. pads.c.patch: no
  test landing pads/strips (plain level pads, no radar landing point).
- Never run split-screen in attract/demo playback (single stream).
- **Results:** from the tally on, each tally draws only its player's view, full screen
  (`pw64_results_full`; both halves left the 1-row view gap as a dark seam across it).
  Each tally's Replay replays that player's flight full screen (results.c.patch: switch to
  `pw64_results_player` around func_8032C540). P2's results aren't stored past the session.
- Other pilots: glider toys skip both pilots (4 left = max toys); sky partners swap a clash
  with the other diver for the first of the diver's own half (both halves' models loaded).
- Hopper hit (`pw64_jh_fall`): state 6 goes to `unk134` (last landing; origin before one → stuck in
  state 9 / fault), so it falls only on the ground at its own spot; hit in the air → on landing.
- Widescreen/fill-screen + split
  untested (the view scissor may fight the renderer's Hor+ extension).
- Cockpit views: dobj state is global, so a vehicle hiding itself for its own first-person view
  vanished from the other view too. Gyro fixed: `pw64_dobj_cockpit[obj]` = hiding player + 1
  (set/cleared by the gyro each frame, cleared on its sinking crash), `_uvDobjsDraw` draws it in
  other channels. Draws run per channel with that player live. Not via `uvDobjState`: the camera's
  collision probes (`func_802DB050/DB224`) set and restore the state every pass. Rocket belt
  hides only pilot parts (`uvDobjProps` 4/5, part flags global): same issue, not fixed.

## Input (per-player controller assignment)
- `headless.rs` `PADS[4]` + `set_controller(n, ..)` + `set_controllers_present(n)` (pad 2
  present only with split-screen on). `input.rs` keeps a `Pad` per source and `route()`
  splits them by `P2Input` (`[splitscreen] p2_input` / `PW64_P2_INPUT`; default every
  gilrs pad → P2, keyboard → P1). `PW64_INPUT_SCRIPT_P2` drives controller 2. input.md.
- Settings > Controls: "Split-screen (2 players)" + "Player 2 controller" (both live).
  settings.md.
- Open: "press a button for player 2" capture; a single Joy-Con held sideways as its own pad.

## Decisions (2026-10-07)
- **Modes:** both pilots on the same test, each scored on their own, results for both; and
  free flight. Pilots see each other.
- **Free flight entry:** a native "Free flight" item in the game's own test menu (split-screen
  only), not a settings toggle. Menu text = our own strings encoded to the game font's glyph
  indices (menu.c items are 16-bit glyph strings); the same helper serves later P2 menus.
- **Interactions toggle:** a Settings > Controls row next to the split-screen rows (`pw64_split_collide`). On = pilots collide (sphere test → each vehicle's own
  hard-impact path, Architecture "Collisions"); off = pass through. Objectives per player either way. Default off.
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
