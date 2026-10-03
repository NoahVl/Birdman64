# Gameplay sweeps

Scripted end-to-end runs at real time (`PW64_NO_THROTTLE`), frames dumped and visually checked.
Scripts live in `crates/birdman64/scripts/`.

## Runs

- `attract_demo.txt` (no input): intro flight → title (the 20 s idle timeout,
  `title_screen.c` `func_80343550` loop) → attract demo (rocket belt) with the
  controller overlay. 3300 retraces. 4:3 + 21:9.
- `fly_hang_glider_finish.txt`: the verified glider flight + 2 photos (Z) →
  scripted cliff crash → crash-cam flow (game.c `unk7C` 1→4, ~4.5 s) →
  results screen → "Check photo" submenu. 4200 retraces, 4:3.
- `birdman_flap.txt` (`PW64_EEP`= copy of the crafted all-gold `tmp/cannon.eep`): title → 3 stick-rights land on the bonus page's Birdman row → A → Lark → Holiday Island Skywalk 1 (airborne ~2000) → A 5×/s 2100–3288. 3400 retraces; flap-impulse regression: altitude HUD 104→398 m @60 and @144.

## Findings

- **Attract demo crash (fixed):** `demoInit` overran `sDemoRecHeader` (ROM RHDR 24 B > struct
  20 B) into `sDemoRecording` → write of 0x8. Any idle title (and any script ending before ~2050) hit it.
  Symbolize `[crash] ... RIP` with `llvm-symbolizer --obj=target/release/birdman64.exe <rip>` before blaming wgpu.

- **Attract demo cycle:** 6 demos (`code_61A60.c` `D_8034EA64`: Lark, Kiwi, Goose, Ibis, Hawk,
  Robin), ~6000 retraces each headless (title idle included): Ibis = Little States dusk rocket belt at ~20500-24000,
  Hawk = Everfrost hang glider ~26000-28500. Those two showed rainbow-noise ground = env tint endianness bug
  (native-build.md §3, fixed). Run 37000 retraces to see all six.
- **Attract demo clean:** demo world fills the wide view; the
  controller overlay (`hud.c` `hudDemoController`, anchor L) sits at
  the left output edge in 21:9, compass right — nothing clipped. Title/menus
  unchanged.
- **Full test flow works**: flight → photos (HUD count 6→5) → crash →
  crash-cam → results (Rings/Landing Accuracy/Landing Impact tally renders,
  Check photo/Replay/Next menu) → A opens the photo submenu. First time the
  results state has been exercised.
- **Rocket-belt Z** (script comment and renderer.md both match the decomp): Z
  sets the thrust target to 0 (`code_AC1A0.c` `rocketBeltMovementFrame`:197
  `spB4 = 0.0f` → `unk78` eases to 0; applied thrust = `unk2E8 + unk78·unk2E4`
  at `rocket_belt.c`:472 with `unk2E8` = 0 in every config, `code_AC1A0.c`:515
  etc.) while still burning fuel (`rocket_belt.c`:423) and lighting the jet
  (`unk84 = 1.0`, `rocket_belt.c`:442).
- **"Check which photo ?" list was empty (fixed)** after 2 photos
  (`fly_hang_glider_finish.txt`, ~retrace 3660+): the submenu title rendered
  but no photo entries, although the flight HUD counted the photos down and
  the results menu showed "Check photo" (`resultListPhoto()` returned 1).
  **Root cause: framebuffer persistence, not data.** The "photos" are not
  stored images: `func_8033D3EC`
  re-renders the world from each photo's saved camera into an 80×59 cell
  (`func_8033A244`, 3 gfx tasks per cell; empty slots = grey 0x50 fill),
  draws the film strip (`func_8033A72C`) and the yellow cursor
  (`func_8033CBD0`) **twice (once per framebuffer)**, uses `uvCopyFrameBuf`,
  and then its input loop draws *only* the "Check which photo ?" text each
  frame (`uvGfxBegin` never clears). The N64 keeps the old framebuffer
  contents; our HLE renders every gfx task as a fresh `Frame` onto a target
  cleared to black (`renderer.rs` `LoadOp::Clear`, hle.rs "last task wins"),
  and `uvCopyFrameBuf` only memcpys unused RDRAM → text on black. Same
  pattern: single-photo view `func_8033DFD0`/`func_8033DDD8` (draws once,
  then empty frames) and `replay_screen.c` `screen_fadeout` (alpha quads
  accumulated over a copied frame). Data path checked clean: shutter
  `func_80338A14` fills `D_80373390[D_8035052C++]` (`Unk80373060`, no
  bitfields, `unk43[0]` ≥ 1); the grid reads the same array. The `Unk8033F050`
  bitfield is the EEPROM album record only (real bug, fixed separately:
  native-build.md §5, `snap.c.patch`).
  **Fix:** framebuffer persistence — renderer.md "Framebuffer persistence"
  (`renderer/fb.rs`, hle.rs/window.rs op stream, graphics.c.patch
  `pw64_fb_copy`). Verified below ("Photo flow").
- EEPROM (`pw64.eep`) untouched by the results flow (only menus save);
  `saveFileWrite` is gated on `func_8033E3A8(2) != 0`.
- **Cannonball bonus game reached and verified** (scripts `cannonball.txt` +
  crafted `tmp/cannon.eep`, see the bullet below): the bonus HUD renders and
  the aiming loop runs. Bonus-vehicle menus differ from the main flow —
  see the cannonball bullets.

## Cannonball bonus game

- **Crafted save** (`tmp/cannon.eep`, builder `tmp/make_cannon_eep.py`, not
  committed): mirrors `save.c` `saveFileWrite` — 'P''W' magic, bit-packed
  7-bit test points from bit 0x10, then 0x408 zero photo-flag bits
  (snap.c `func_8033F050` reads them as no-photos), checksum = sum(0..0xFE).
  Point values: beginner class × 3 main vehicles = **80 exactly** (silver
  threshold; single test → total 80), everything else 100 (gold).
  Test counts parsed from the ROM's task DB (the 61 UPWT files' COMM blocks:
  classNum@0, vehNum@1, testNum@2) — do not hand-edit them: the load path
  reads exactly `taskGetTestCount` values per (class, veh) in vehicle-major
  order (vehicles 0..6, classes 4 for mains / 3 for bonus). Layout:
  veh 0-2 = 1/2/3/3 tests, veh 3 (cannonball) = 4/4/4, veh 4/5 = 1/1/1,
  veh 6 (birdman) = 4/4/4/4. Same 0x100 save in both file slots.
- **Verified accepted** (headless, PW64_EEP): file select shows silver
  medals on Beginner × 3 mains, gold on A/B/P, both files "Continue",
  **"Extra Games" unlocked** (right column page 2 of the class select).
- **Route** (`crates/birdman64/scripts/cannonball.txt`): title A @1100 → file
  A @1200 → 3 stick-right taps (1310/1380/1430, centre 20 fields apart)
  onto the bonus page → stick-up @1470 (row 0 = cannonball) → A @1560 →
  pilot A @1720 → overview A @1860 → details A @2000 (Start is
  highlighted) → aiming HUD ~2050. Bonus pages read the ANALOG stick only
  (D-pad ignored), one press per deflection like the class grid.
- **Bonus HUD** (dumped 2050/2150–3290): no TIME/rings/fuel — instead
  cannonball ammo icons top-left (3 shots), vertical **POW** power bar
  left (colour gradient bottom→top, oscillates while charging), elevation
  gauge right (10–50, red pointer), horizontal aim strip bottom (W/E
  compass ends, range ticks, centre number 45), compass top-right, and a
  red/white target ring over Mt. Rushmore. "Target 1 First Shot" warning
  text on entry. Flow before it: test details shows "TEST 1 Super Cannon"
  (top score 400 PTS), TEST_SETUP renders 2 black fields
  (game.c `gameUpdateStateTestSetup` → `cannonLoad802D77D8` clears the
  screen twice), then the aiming state idles (dumping 2150–3290 shows the
  POW bar cycling, no input needed).
- Bonus test-details quirk (`test_menu.c`): bonus vehicles reuse the
  class cursor — `sCurTestIdx` = `unkC->cls` and A on START maps cell
  index 3; the main grid's Scoring/Photo cells are unreachable there.

## Sky diving + jungle ropper bonus games

- Same crafted save (`tmp/cannon.eep`) unlocks all three; scripts
  `skydiving.txt` / `jungle_ropper.txt` follow `cannonball.txt`. Route delta:
  the bonus page reads rows via **up-taps** (row → vehicle: row 0 cannonball,
  1 sky diving, 2 J. Hopper — level_select.c sets `sp18->veh = row + 3`), so
  sky diving needs 2 up-taps / A @1610→1770→1910→2050, J. Hopper 3 up-taps /
  A @1660→1820→1960→2100 (each menu ~150 retraces behind the cannonball one
  per extra tap). Pilot select starts on Lark for both
  (pilot_select.c `D_8034EE30` per-vehicle model — skydiving-Lark shows the
  parachute, Hopper-Lark the pogo stick).
- **TEST_SETUP shared**: non-cannonball vehicles take the common
  `gameUpdateStateTestSetup` path (game.c:431, same 2 black clear fields as
  cannonball) → `taskInitTest` + `levelLoad` + `<vehicle>LoadLevel` →
  TEST_UPDATE with "Start !". No per-game setup state.
- **Sky diving HUD** (`HUD_RENDER_SKYDIVING`, dump 2150–3500): TIME top-left,
  compass top-right (N needle + green formation markers, red = landing
  point), altitude bottom-right (`2576 m` counting down + SEA LEVEL label),
  "Start !" centre text; player freefalls centre-screen with skydivers in
  formation alongside. Crescent Island, "TEST 1 Sky Dive 1". Details text:
  formations then land on the landing point. Levels in the bonus page show
  100 PTS silver medals.
- **Jungle ropper HUD** (`HUD_RENDER_JUMBLE_HOPPER`, dump 2150–3500): TIME
  top-left, SPEED bottom-left (km/h), compass top-right, bottom-right jump
  gauge (vertical bar + red `Nm` height marker) + SEA LEVEL. Holiday Island,
  "TEST 1 Triple Jump" — "Jump to the goal area, two points deducted each
  time you land in the water". Third-person behind the hopper on the hill;
  the world loads and the hopper runs/bounces (0→67 km/h, gauge tracks).
  Levels in the bonus page show 100 PTS silver medals.
- Verified headless: skydiving frames 1450/1600/1750/1910/2050/2150–3500, jungle
  1450/1600/1750/1910/2050/2150–3500 — no artifacts in either HUD; both
  games run their own update loops to the retrace limit.

## Gotchas for future sweeps

- **Skip the menus with `PW64_START`** (e.g. `PW64_START=a:hg:2` = Class A hang glider test 2 "Chicken Dive"): the patched title state loads the save file via `fileMenu_802E8FF4` and returns `GAME_STATE_TEST_SETUP`; airborne by retrace ~100 headless. One-shot: after the test the normal menus follow. Verified for a:hg:2 (same start state as the menu route); bonus vehicles (cb/sd/jh) put the 1-based test in `cls` like the bonus test menu — unverified.
- Results menus read `demoButtonPress` (A/stick); a press is consumed the
  frame it's pressed — script presses 1–6 retraces apart, then idle.
- **Always release (`+6`) between presses.** Repeating `A` on consecutive
  steps holds it: the game sees one rising edge (`controller.c:92`). The pad
  is polled every frame in flight/crash-cam (`map3d.c:67`), but not in the
  results pre-loop or the first 0.75 s of the results menu. So a held A only
  reaches the menu if the game was already in RESULTS when it went down
  (this caused an early "menu never advances" sweep failure).
- `fly_hang_glider.txt` at 2700 has the glider 9 m up heading for the cliff —
  the crash lands in the results screen ~2900–3000 without extra input.

## Verification runs

Headless, `PW64_NO_AUDIO=1 PW64_NO_INPUT=1 PW64_NO_THROTTLE=1` unless noted; RTX 3060 Laptop.

- **60 vs 144 fps:** both runs `stopped: RetraceLimit`, `VI retraces 2704`; 144 log:
  `6444 present ticks (143.0/s of game time)`. Frames 600/1200/1800/2400/2700: same
  flight — file select identical, in-flight over lighthouse/lagoon with matching attitude,
  timer drift ≤ 6 ticks (3"13/3"15, 13"28/13"34, 18"42/18"47). Reproduced in a second
  batch (3"12/3"15, 13"29/13"34, 18"41/18"47).
- **Audio pacing:** `PW64_AUDIO_VIRTUAL=1` alone is NOT silent — audio.rs only opens the
  virtual device when `PW64_NO_AUDIO` is set (otherwise the real output device plays).
  `PW64_AUDIO_VIRTUAL=1 PW64_NO_AUDIO=1 PW64_FPS=144` real-time: `0 lost … 0 late`,
  `underruns 0 (0 ms silence)`; no `sc recover` / `RSP timeout` / `state error` lines.
- **Photo flow** (finish script with A released +6 after every press, fresh save
  `PW64_EEP=<tmp file>` deleted before every run; dump 2800–4200 every 50): crash-cam at
  2800–3050 (tumbling glider), first MENU retrace 3300 (Check photo cursor, tally box
  over the replay), first GRID 3350 — "Check which photo ?" with **2 photo cells
  (thumbnails visible), 4 grey cells, film strip, yellow cursor**, first PHOTO 3500
  (single photo + "No"), then GRID ↔ PHOTO alternating at each A through 4200.
  `PW64_MSAA=4` (first GRID one 50-step later, same screens incl. "Save this Photo?"
  with edge AA) and `PW64_WIDESCREEN=1` (grid centred in 16:9, pillarboxed) match.
  Before the press-release fix in the script, the menu never advanced (see Gotchas).
- **Photo-view flicker + audio stutter (fixed):** the photo-flow check above missed it — every-50 dumps
  sample one phase of a 2-retrace flicker (single photo with blue frame/"Save this Photo?" ↔ photo + "▶No"
  only; grid also lacked its blue header). Cause + fix: renderer.md "Photo-view flicker". Verify with
  consecutive retraces (finish script, 3400–3460 every 1, `PW64_DUMP_HEIGHT=120`, 60 and `PW64_FPS=144`)
  and audio: real-time `PW64_AUDIO_VIRTUAL=1 PW64_AUDIO_STATS=1 PW64_HEADLESS=1`, 3700 retraces →
  before 8 underruns (1026 ms silence), 18 AI late, 6 restarts; after 0/0/0.
- **Rendering old vs new vs N64 filter** (LOD/3-point work): same scripted glider flight,
  `PW64_DUMP_FRAMES=1700,2200,2600`; r_old = the build before, r_new = after, r_n64 = after
  with `PW64_FILTER=n64`.
  - 1700 (lighthouse): all three identical composition — castle, lighthouse,
    glider low over landing zone, distant hills/sky clean; no black/garbled
    textures; HUD unchanged r_old↔r_new.
  - 2200 (cliff turn, 54 km/h): same scene/attitude in all sets (timer
    9"88/9"86/9"94); distant terrain clean in r_old and r_new; near geometry
    unchanged r_old↔r_new.
  - 2600 (close cliff fill): same cliff face + glider; moss/rock texture
    unchanged r_old↔r_new; r_n64's HUD digits (17m, 63 km/h, 23m) visibly
    chunkier than r_new's, as expected for 3-point N64 filtering.
  - r_n64 vs r_new: same scenes, slightly chunkier texture detail on HUD
    digits/text at all three frames — filter works. No black or garbled
    textures anywhere in any set.
- **Skydiving 60 vs 144** (`tmp/cannon.eep`; `skydiving.txt`, dump 2200–3000 every 200):
  freefall over cloud sky with formation divers beside the player in every
  frame; divers' wobble pose near-identical per 60/144 pair (limb spread
  same size both rates); altitudes 2557/2555 → 2480/2479 → 2405/2402 →
  2330/2327 → 2255/2252 (1 m apart), timers ≤2 ticks — rate-independent.

## Pixel-diff sweeps: determinism

Attempted old-vs-new regression sweep for `00a7ccb` (VI overscan) + `87aee0e`
(texrect point-sampling): dumped frames 150/1550/2700 (+3300..4200 photo flow)
from pre-fix (`2c7ba90`) and post-fix builds, plus **same-commit control runs**
to calibrate noise. Key learnings:

- **Scripted headless runs are NOT pixel-deterministic.** Two dumps of the
  *same* commit differ by 0.7–4.4 % of pixels (title 0.7 %, briefing 4.2 %,
  flight 4.4 %): timer skew moves HUDs by a frame, chaos/ambient motion differs.
  Any old-vs-new diff below ~5 % with that spatial signature is unattributable;
  pixel-diff sweeps need per-draw comparisons (PW64_DUMP_PIXEL) or deterministic
  anchors (briefing blits), not full-frame hashes. Control runs are mandatory
  first — the before/after numbers alone looked alarming for nothing.
- **The fixes' visual effect is verified directly instead:** briefing frame
  1550 (blit-heavy, includes "Top Score") dumped pre- and post-port-batch is
  byte-identical; briefing + title dumps show the 2-row/column blacked overscan
  (`00a7ccb`) and crisp 1:1 texrects (`87aee0e`).
- **A/B harness that worked:** two worktrees (`6442661` vs `fb32986`), same
  script; frames 1550 IDENTICAL, 150 alpha-noise only (0.06 %), 2700 timer
  skew (28 %). Recipe: `tmp/pngdiff.ps1 a b` (tol 2, bbox report) +
  `tmp/crop.ps1 in out x y w zoom` for inspection. PowerShell caveats: `-f`
  format strings get `$`-mangled inline; use script files.

## Fill screen sweep

Headless, `PW64_NO_AUDIO=1 PW64_NO_INPUT=1 PW64_NO_THROTTLE=1 PW64_FPS=60`,
fresh `PW64_DATA_DIR` per run, all 9 runs exit 0.

| Run | Script | Aspect | Fill | Frames checked |
|---|---|---|---|---|
| 1 | fly_hang_glider.txt | 16:9 | on | 1050, 1450, 1550, 1700, 2300, 2700 |
| 2 | fly_hang_glider.txt | 4:3 | on | 1050, 1450, 1550, 1700, 2300, 2700 |
| 3 | fly_hang_glider.txt | 21:9 | on | 1050, 1450, 1550, 1700, 2300, 2700 |
| 4 | fly_hang_glider.txt | 16:9 | off | 1050, 1450, 1550, 1700, 2300, 2700 |
| 5 | fly_hang_glider.txt | 4:3 | off | 1050, 1450, 1550, 1700, 2300, 2700 |
| 6 | fly_hang_glider_finish.txt | 16:9 | on | 2850, 2900, 2950, 3000, 3100, 3300, 3350, 3500, 4000 |
| 7 | skydiving.txt (tmp/cannon.eep) | 16:9 | on | 2200, 2400, 2600, 2800, 3000 |
| 8 | fly_hang_glider.txt | 16:9 | on, MSAA 4 | 2300 |
| 9 | fly_rocket_belt.txt | 21:9 | on | 2500 |

Verdict: no artifacts found.
- Fill on, all aspects: title, class summary, briefing, flight, crash cam
  (finish 2850-3000), replay fade (3100, stretched to full height), results
  (3300), sky diving cloud view (all five frames), rocket belt: world reaches
  the true top/bottom edges, no black or stale rows, no bars; HUD anchored at
  the true edges (TIME/radar top, SPEED/PHOTO/SEA LEVEL/FUEL/throttle bottom),
  nothing clipped at 16:9 or 21:9; 4:3 + fill loses only the letterbox bars.
- Fades: crash/cloud/replay fades cover the full output height (vertical
  Stretch works; 3100 is a mid-fade wash over the whole frame, expected).
- Photos unchanged: album grid (3350/4000) 2 thumbnails + 4 grey cells, film
  strip, cursor; single-photo view (3500) keeps the original framing, black
  surround; no album-clear artifacts from the fill stretch.
- Fill off: 16:9 shows the Hor+ letterbox look, 4:3 the original
  bars; identical to the pre-fill output.
- Menus/options/pilot select stay 4:3 with bars.
- Edge-pop between consecutive flight frames: dumps 2290..2310 every 5, no
  popping.
- MSAA 4 + fill: correct, no new artifacts.

## Texture packs

- **Verified end-to-end:** dumped blit `ae6a6302eead8d2f.png`
  (Top Score, 40x102) recoloured red into `tmp/pack/`, `PW64_TEX_PACKS` picked
  it up (binds at briefing frames 1550/1600, key match confirmed in the
  `_dl.txt` tex trace) and the on-frame render is red.

## Pre-release visual sweep

After `55b4c10` (time-of-day tint) + `3e170f6` (endianness audit). Release build,
headless, `PW64_NO_THROTTLE=1 PW64_NO_INPUT=1 PW64_NO_AUDIO=1`; reviewed as contact
sheets. Attract demo: dump every 1000, 2000-36000, 37000 retraces.

- **Attract demo 4:3:** All 6 demos render: Lark rocket belt (~2000-5000,
  Copter Harbor), Kiwi gyrocopter (~8000-11000), Goose rocket belt
  (~14000-17000), Ibis Little States DUSK (~20000-23000), Hawk Everfrost SNOW
  (~26000-28000), Robin hang glider (~31000-34000), titles/intro between.
  Dusk ground and snow both correct, no rainbow noise (the old env-tint bug
  stays fixed). Demo transitions dump as empty ocean with no HUD
  (`frame_06000/12000/18000/24000`, fade gap between demos, expected).
- **Attract demo `PW64_WIDESCREEN=1`:** Same cycle, world fills 16:9,
  HUD/controller overlay at true edges, nothing clipped vs 4:3.
- **All 8 scripts** (8 flight dumps each): `fly_hang_glider` 1750-2650,
  `fly_hang_glider_finish` 1750-2800, `fly_rocket_belt` 1750-2550 (jet flame
  VFX over the docks), `fly_gyrocopter` 1850-2600 (night test: dark runway
  then headlight cone, correct), `fly_hang_glider_photo` 1750-2250, and
  cannonball/skydiving/jungle_ropper 2100/2150-3200 (`PW64_EEP=tmp/cannon.eep`;
  POW bar + Rushmore target ring / cloud freefall / hopper hills all correct).
  No garbled textures, wrong colours, missing geometry, black polygons or
  broken HUD in any frame.
- Note (not a bug): the hang-glider canopy shows a rainbow gradient stripe in
  every glider/demo frame - same art in demo + scripted flight, consistent
  across aspects; it is the wing texture, not the env-tint endianness bug.
- Environments covered: day (Holiday Island, Copter Harbor, Little States day),
  dusk (Ibis demo), night (gyrocopter), snow (Hawk demo).

- **HUD tag (OLED care) identity:** the build emits 2 extra commands per HUD
  task (the PWH bracket); frame 1900 has 270 draws, 26 hud true on the
  contiguous HUD block (draws 244..269), 0 elsewhere. Pixel A/B vs a
  worktree at HEAD: strong diffs (delta > 12) are 48 px new-vs-old vs 121
  px old-vs-old noise floor (timer digits only), so defaults stay
  output-identical. Method: `git worktree add tmp/ab <commit>` +
  submodule init, PW64_ZIG=<repo>/tools/zig/zig.exe, fresh
  PW64_DATA_DIR per run, then a per-pixel compare of the two dump dirs.
- **Congratulations scene pilots = UVMD rest pose, not a bug:** `code_A8C30.c`
  only poses part 0 (`userPath_8034A8B0` → `uvPathPoseLine` sets `unk2D8 = 1`); every other part
  keeps `mtxTable` (identity rotations, legs straight down, arms out to the rockets). Goose's RB
  model (0x122) has long thin red legs, so close to the camera (clipNear 1) it looks "spiky";
  Lark/Kiwi show the same pose and look normal; matches the extractor's glb. Fast repro: worktree
  hack in `game.c.patch` `gameUpdateStateTestOverview`: `if (gameState != 0) gameState =
  GAME_STATE_CONGRATULATIONS;`, then `fly_rocket_belt.txt` up to retrace 1680 (pilot select:
  stick down = Goose); pilots arrive ~2500-2700 (world black: the test level isn't loaded).

## Flicker scan

Hunt for alternating-frame (A/B/A) flicker — consecutive retraces alternate between two
images (the bug seen in the photo view). Tool `tools/flicker_scan.py`: flags retrace
n when d(n,n-1) > 8.0 and d(n,n-2) < 2.0 (frame matches n-2 but not n-1), merges runs.

Method: every script in `crates/birdman64/scripts/*.txt` run headless with **every retrace
dumped** (`PW64_DUMP_FRAMES=1,2,…n`, `PW64_DUMP_HEIGHT=96`, full-length per script:
last retrace +600; attract_demo 6000). ~60 retraces/s headless → ~15-36 s per script, so
full every-retrace dumps are cheap; no windowing needed. Old sweeps dumped every 50
retraces, which hides A/B alternation by construction.

**Result: no flags in any of the 9 scripts** at the thresholds, none with a looser test
(d2 < d1/2, d1 > 8) either — the photo-view alternation did not reproduce in these
scripted runs (flow fly_hang_glider_finish 2900-4700 covers grid ↔ single-photo ↔ white
fade ↔ dark transition; contact sheets looked normal).

Largest ordinary (single) transitions per script, all legitimate cuts — the big ones are
scene changes, d2 == d1 (previous frame is old scene too, so not alternation):

- attract_demo: 2051 (d 105.3, demo start), 2375, 5904 (demo loop)
- fly_hang_glider_finish: **d 106.4 every 200 retraces (3608, 3808, 4008, …) = the A-tap
  grid ↔ single-photo cycle**, period 200 = script's press every 100; contact sheets
  (grid, white fade, photo view, dark transition) all correct
- fly_rocket_belt: 1703 (d 110.7); 2350-2900 sheet = normal flight
- Others ≤ 84.7, all single cuts.

Small d2<2 near-misses are just gradual motion (d1 < 3.2), except fly_rocket_belt
2443-2868 (d1 up to 3.1, d2 < 1.8): rocket-belt exhaust flicker over clouds, legit
animation, sheet confirms.

Gotcha for concurrent runs: run pw64 with a scratch **cwd** (its own `tmp/`), `PW64_ROM`
absolute — repo `tmp/frame_*.png` gets clobbered by concurrent runs; and wait for the exe
(not the `cargo run` wrapper) to exit before moving frames.

