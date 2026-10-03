# High frame rate (≥ 144 Hz) — architecture

Supersedes an earlier "logic stays at native 30 Hz, interpolate matrices" plan.

## Facts that drive the design
- **The game is delta-time driven, not fixed-rate.** `func_80313D74` (code_9A960.c)
  sets `D_8034F854` (frame dt) = `uvGfxGetFrameTime()` clamped to **[0.01, 0.1] s**
  and accumulates game time `D_8034F850`. Physics, boats, thermals, HUD, sequences
  (`fx_seq.c`) all scale by it. The N64 ran it at a variable ~20–30 fps.
- Frame time = wall time between `uvGfxEnd` calls (`UV_CLKID_GFX`, `osGetCount`
  based — continuous, not retrace-quantized). `uvGfxSetFrameTime` overrides it
  (snap.c / replay_screen.c freeze time with ~0).
- **What caps the rate today is the scheduler, not the game:** `_uvScHandleRetrace`
  (sched.c, VI every field, `numFields = 1`) is the only place gfx tasks are taken
  from `cmdQ` and the swap flip (`D_802B9C6E ^= 1`) happens; `_uvScRunGfx` also
  waits for `osViGetCurrentFramebuffer() == osViGetNextFramebuffer()` (swap
  latched at retrace). The game thread only blocks on `UV_MESG_GFX` (previous
  task done). With our fast HLE the port therefore already runs at **60 fps**
  (dt ≈ 1/60), vs the N64's ~25.
- Audio is retrace-driven: `__amMain` makes one audio frame per
  `OS_SC_RETRACE_MSG`, sized from the AI queue fill. It must stay at 60 Hz.
- Interp output (`pw64-gfx` `Frame`) is CPU-transformed clip-space vertices, so
  frame interpolation would need draw/vertex matching across frames (RT64 needs
  game-side matrix-group IDs for that) and breaks on LOD/culling changes.

## Design: let the game run at the display rate
1. **Present tick** (new): the OS core posts a scheduler message (`PW64_PRESENT_MSG`,
   patched `_uvScMain`) at the target rate. Its handler does only the gfx half of
   `_uvScHandleRetrace`: swap flip, drain `cmdQ` (audio tasks are stored, still
   started at the next VI retrace as today), `_uvScRunGfx`. The 60 Hz VI retrace
   keeps clock update, RSP/RDP timeouts, audio start and client notification.
   `osViSwapBuffer` latches on the present tick instead of the VI retrace, and the
   window presents per swap. Target 60 = today's behaviour exactly.
2. **dt clamp patch:** lower bound 0.01 → ~0.002 (500 Hz) so 144 Hz doesn't run the
   game 1.44× fast. Plus whatever the rate-dependence audit below finds.
3. **Rate:** config `fps` = `monitor` (default: winit monitor refresh) | N | 0 =
   uncapped; `PW64_FPS` env. Headless/scripted runs keep time-based input scripts
   (retrace-keyed), so 60 vs 144 runs of one script are comparable.
4. **Lower-end hardware degrades gracefully:** if a frame takes longer, dt grows —
   exactly what the N64 did. No fixed per-present budget to miss, unlike
   interpolation. Settings screen offers 30/60/monitor caps.
5. **Budget levers (in order):** DL interpretation (~2–2.5 ms/frame in flight,
   ~250 ns/GBI command — state churn, TMEM hashing), window render scale/MSAA,
   wgpu per-draw overhead. At 144 Hz the game thread at today's cost uses ~40 % of
   one core; the render runs on the window thread in parallel.

Rejected: matrix/vertex interpolation at fixed logic rate (complex, artifacts, and
the game is natively variable-rate). Revisit only if the audit finds rate-dependent
behaviour that can't be patched.

## Rate-dependence audit
Audit of all of `decomp/src/app` + `src/kernel` (fully decompiled, no asm).
`dt` = `D_8034F854`. `REF_DT` = the N64 frame time the per-frame constants were tuned at:
variable ~20–30 fps, use **1/30** (one `#define`, calibrate later). Every per-frame item below
**already differs at 60 Hz** unless marked "rate > N". Nothing found makes the native-rate design
unworkable: all fixes are local and small.

Look for **one-frame impulses** too: `x += c·dt` inside a button-press (edge) branch is a
per-event step that shrinks with the frame time (birdman flap below). And **relays**: a
branch on a sign/threshold of a state the branch itself drives (birdman stall pitch torque)
chatters once per frame; at 30 Hz each switch overshoots, at high rates it slides along the
threshold → different average. Fix: sample-and-hold the branch output per REF frame.
A second audit pass checked: every vehicle's velocity/rate state writes (only the two
weathervane lines lacked dt), `demoButtonPress` branches (others: camera toggles, ±0.25
`unk2DC` = rate-independent), frame counters (`++`/`--` on statics/fields: event counts,
a 1-frame contact bool in RB, a 2-frame env-sound start delay), camera offsets (recomputed
per frame), landing checks (impact speed = velocity at contact, rate-independent). HG has the
same stall switch (`hang_glider2.c:273`), but its falling-leaf swings |airflow| ±1 m/s, so it
doesn't slide: scripted full-pull stall looked alike at 30/60/144 Hz (left unpatched).
Safe as-is (checked): all vehicle force/torque accumulators are integrated with `dt`
(HG, RB, gyro, cannonball, skydiving, JH, birdman); `func_80313AF4`/`BAC` approach helpers;
balls, boats, rings (pass detection only improves), thermals, wind vectors, whales, planes,
smoke, splash spawns, snow (camera-delta parallax), `uvSeqUpdate`, sprite/UVTX scroll, fx
lifetimes; `D_8034F850`-difference timers; divisions by dt (`gyrocopter.c:518/524/1856/1885`,
`planes.c:68-97`: rate limits / one-frame velocity cancel, fine for dt ≥ 0.002); demo playback
(time-keyed inputs + `demoAttUpdate` pose override); button edges (no hold-N-frames input logic
exists; menus use stick-recentre flags); collision bounces (impulse only when v·n < 0).

| Sev | Where | What | Fix |
|---|---|---|---|
| high | `hang_glider2.c` `hangGlider_802F3A80` :712-715 | `hgData->unk90++` per contact frame (after t>2 s); ≥91 → forced crash (`unk8C=2`); reset only at :405. 91 frames = ~3.6 s scraping on N64, 0.63 s at 144 Hz | accumulate contact time; crash at `91*REF_DT` — **done** (`pw64_ticks`) |
| high | `fdr.c` `fdr_802E65AC` :81-91 | flight recorder samples pose/sticks/vehicle floats every 2nd frame into 200-entry rings → replay = last 400 frames (16 s N64, 6.7 s @60, 2.8 s @144) | sample when `D_8035AF68 - last ≥ 2*REF_DT` (playback already interpolates by timestamp) — **done**; forced (`fdr_802E66DC`, HG crash) every REF_DT |
| high | `gyrocopter.c` `func_80309090` :1896-1928 (+ trigger `func_80308478` :1574) | hands-off level-flight trim: `unk7C++` per frame, blends accel over 50/100 frames; `unk88 = …/(100*dt)` | time accumulator; progress = t/(100*REF_DT); replace `100*dt` by `100*REF_DT` — **done** (blends use count+fraction; `==1` setup once per trigger) |
| high | `code_82B90.c` `func_802FD388` :689 | JH yaw `unk140.x -= stick·|stick|·5°` **per frame** while crouching/charging (`func_802FD794`, `func_80301090`) | `× dt/REF_DT` — **done** |
| high | `environment.c` `env_802E15F0` :313-331 (also run from `replay_screen.c:241`) | wind speed/dir random walk, step `(rand-0.5)·rate·dt` per frame → spread ∝ √dt: wind varies 0.42× (144 Hz) / 0.65× (60 Hz) as much as on N64. Feeds all flight physics | step `× sqrt(REF_DT/dt)` (or tick the walk at REF rate via accumulator) — **done** (`pw64_walk`) |
| high | `audio_emitter.c` `uvEmitterFlush` :283-286, `_uvaPlay`, `_uvaUpdateVoice` (run per `EVENT_FRM_END` via `snd.c:sndEvent`) | playState 2 (alSndpPlay issued, audio thread is 60 Hz) times out after **5 game frames** → release+retrigger: marginal at 144 Hz (35 ms ≈ 2 audio frames), loops restart forever at ≳300 Hz. Also 4 alSndp events/voice/frame into a 256-event queue (16 voices → 64/frame): overflow at rate > ~240 Hz | run `sndEvent` body only when ≥1/60 s accumulated (fixes both; also makes per-frame random sound jitter 60 Hz) — **done** in `snd.c:sndEvent` (wall clock id 0, ≥0.9/60 s; explicit `func_8033FB14` calls stay immediate) |
| high (rate > 217) | `camera.h` `Unk802D3658_Unk230` (50 entries), `code_72010.c` `func_802EAC18`/`func_802EAC9C`, used by `camera_802D3444`/`802D3BE8`, `cannonball.c:283` | chase-cam pose history, 1 push/frame, looked up at `now - camera->unk48` (HG 0.23 s, birdman ≤0.15, gyro/cannon 0.06, default 0.7). History too short → returns entry `[0]` (arbitrary) → camera jumps. HG breaks > 217 Hz | enlarge ring to 512 (≈0.7 s @ 700 Hz) — **done**, ring moved into `code_72010.c` (see below) |
| med | `code_82B90.c` `func_802FE2FC`…`func_802FEEC0` :989, 1014-1178; `func_802FDF8C`/`802FE054`/`802FE1A8`/`802FE9DC`/`802FEAD0` :904-1099 | JH leg swing amplitudes `*= 0.95/0.985/0.99/0.995` per frame; leg angles `+= stick·25` per frame (visual) | `powf(k, dt/REF_DT)`; `× dt/REF_DT` — **done** |
| med | `camera.c` `camera_802D3790` :246-251 | JH camera (mode 9) elevation rate-limited 3°/frame | `3·dt/REF_DT` — **done** |
| med | `ski_lift.c` `skiLiftUpdate` :24 | chair path param `+= 0.03`/frame | `× dt/REF_DT` — **done** |
| med | `shuttle.c` `shuttle_803358D4` :310-326 | `sShuttleAltitude += sShuttleThrust` per frame (thrust grows by dt) → ascent ∝ 1/dt; `D_803504A0 ±5`/frame | `× dt/REF_DT` — **done** |
| high | `birdman0.c` `birdMovementFrame` :198-207 (missed by the first audit, found in playtesting) | flap: on the A/B **press frame** `unk2D0 += 9·dt` (one-frame impulse ∝ frame time) and the `unk2D0`/`unk2D4` decays are skipped that frame. Lift/thrust ∝ `unk2D0²`, takeoff from standing needs `unk2D0 > 1.9` (`birdman2.c:201`). At 5 presses/s the flap settled at 1.6/0.8/0.65 @60/120/144 → flaps barely climb, **no takeoff after landing** | impulse `9·REF_DT`; skip REF_DT of decay time after each press (`sPw64FlapHold`, dt temporarily reduced around the two `func_80313AF4` calls) — **done**. Measured (scripted, `cannon.eep` → Birdman, 5 A/s): flap ≈3.4–3.6, altitude after 44 s 825.05/825.45/825.39 m @60/120/144; standing takeoff ~1.5 s after flapping starts @60/120 (before: flap stuck ≤1.7/0.8, never takes off) |
| high | `birdman2.c` `bird_802CF24C` :254-260 (audit 2) | **relay sampled per frame**: pitch torque = stick term if forward airflow `unk274.y ≤ 0`, else backward-flow term. In a flat stall the sign chatters; at 30 Hz each switch applies a REF frame of stick torque (4.1 rad/s² × 1/30) → overshoot → `|unk274.y| > 0.1` gates lift on (`bird_802CFAC8`); at ≥100 Hz it slides along 0, lift stays off, the bird falls to the ~26 m/s terminal speed (drag table sign flips at 24 m/s) and the stall landing crashes (`|vz·2.52| ≥ 50`) | sample-and-hold the term every REF frame (`pw64_ticks`, ≤30 Hz unchanged) — **done**. Measured (`bird_3500` stall script): before land 30–90/110–130 Hz, crash 100/144/165/200/240 Hz (25.9 m/s); after: lands at all 14 rates 30…240 Hz, touchdown 26.2–27.1 s at 12–15.6 m/s (30 Hz bit-identical); 5 A/s flap flight unchanged (825.4 m @44 s at 60 and 144) |
| med | `birdman2.c` `bird_802CF24C` :302, `hang_glider2.c` :357 (audit 2) | sideslip weathervane `yawRate -= 0.005·vx·|vx|` per frame, no dt → 4.8× stronger @144 than @30 | `× pw64_scale()` — **done** |
| low | `cannonball.c` :286-291 (audit 2) | chase cam switches after 3 frames (`D_8034E9F8 == 3`, first shot only) | cosmetic, **left** |
| closed | `func_802DC1DC` collision torque (`birdman2.c:690`, `hang_glider2.c:709`, `rocket_belt.c:798`) (audit 2) | adds an angular *acceleration* ∝ impact velocity, integrated with dt next frame: a one-frame bounce gives a spin kick ∝ dt | **left, measured** (temp `pw64_log` instrumentation, scripted runs @30/60/144). **RB: dead write** — collision runs after the integration (`func_8032867C` :176 → `func_8032975C` :186) and :132 zeroes `unk220` before the next one; kicks of 30–240 changed ω by 0 at every rate. RB ground skid (hops, e=0.4) and oblique wall hit: every bounce 1 contact frame at all rates, rebound/approach 0.40 (0.20 when `sp298`=0) at all rates; same rest pose. **HG/birdman**: kick factor is non-zero only when that contact sets the crash state (HG `sp244`=0.7, bm `var_fs1`=0.7; bm also `ballsPopped`) → crash tumble only, outcome already fixed. HG cliff crash (`fly_hang_glider.txt`): ω 0.5 s after impact 7.1/5.6/5.8 rad/s @30/60/144 (dominated by the crash-state spin; impact frame dt ≈0.06 at all rates); cosmetic |
| closed | `birdman2.c` :619-621, `rocket_belt.c` :804-806 (audit 2) | "stuck" push `v += normal` (1 m/s) when the bounce changed velocity < 0.1 | **left, measured**: in RB it fires once per low-speed settling hop, never on consecutive contact frames (the penetration push-out ends the contact): 4/7/9 pushes @30/60/144 in the skid run (finer steps resolve more micro-hops ≤1 m/s), same rest. Rate-independent per event |
| med | `birdman2.c` `bird_802D0080` :626-628 | after landing, each contact frame nudges pos `0.01·normal` (0.25 m/s @25 → 1.4 m/s @144) | `× dt/REF_DT` — **done** |
| med | `code_9A960.c` `func_80313AF4`/`func_80313BAC` (174 call sites) | explicit-Euler approach with overshoot snap; for k ≥ 10 (HG crash k=15, birdman k=10-15) k·dt≈0.5 on N64 → decays faster than at 144 Hz (converges to e^-kt). Feel difference ≤ ~30 % in time constant | optional: factor `1-(1-k·REF_DT)^(dt/REF_DT)` when `k·REF_DT<1` — **done** for `func_80313AF4` (k≤0 or k·REF_DT≥1 unchanged); `80313BAC` is rate-independent |
| med | `D_8034F850` + per-test timers (`hgData->unk8`, `gcData->elapsedTime`, `rbData->unk8`, `cbData->unk8`, `bmData->unk8`) | f32 `+= dt` rounding bias ≤0.5 ulp/frame: ~0.03 s per 120 s test @144, ~0.1 s @500; after 1 h free flight ulp(D_8034F850)=2.4e-4 (3.5 % of dt @144) | accumulate in f64 shadows, store (f32) — **done** for `D_8034F850` (resyncs when others write it) and the 5 test timers (one `+= dt` site each: `hang_glider1.c`, `code_7CF30.c`, `code_AC1A0.c`, `cannonball.c`, `birdman0.c`; static shadow per file, resync on mismatch covers load resets + the shared vehicle buffer) |
| low | `hud.c` `hudDrawRadar` :1190-1200 | waypoint blink alpha ±50/frame (10-frame period → 14 Hz strobe @144) | `× dt/REF_DT` — **done** (rounded to int) |
| low | `hud.c` `hudDrawCamera` :705-717 | photo shutter lasts `CAMERA_SHUTTER_FRAMES`=3 frames | time-based — **done** (`pw64_ticks`, clamped ≥0: high bits are flags) |
| low | `hud.c` `hudDrawTimer` :1585-1595 | rolling hundredths `+1..4` per frame | leave / time-based — **skipped** (cosmetic) |
| low | `splash.c` `splashDraw` :76-77 | ripple fade −0.02 / size +0.01 per frame | `× dt/REF_DT` — **done** |
| low | `falco.c` `falco_802E55A0` :783-785 | creature height filter `0.8·x` per frame | `powf(0.8, dt/REF_DT)` — **done** |
| low | `env_sound.c` `envSound_802E2A00` :245 | emitter fade `vol *= 0.95` per frame | `powf` (not covered by the throttle: runs per game frame) — **done** |
| low | `code_AE460.c` `rocketBelt_803279F0` :246-248 | jet flame spin `+= 1.5·sp74` rad/frame + random scale per frame (flicker rate) | `× dt/REF_DT` — spin **done**; flicker left |
| low | `code_BA190.c` `func_80332FCC` :165-167, `skydiving.c` `func_803322CC` :1132-1138, `birdman1.c` `bird_802CE190` :102-107 | per-frame random target low-passed (k=10/15) → jitter amplitude ∝ √dt (skydiver drift, wing flutter) | hold random target for REF_DT — **done** (`pw64_ticks`, redraw on ≥1 tick, so REF rate or slower = unchanged; player sdData/bmData: one static slot keyed by pointer, `pw64_fatal` on a 2nd — only one vehicle buffer `game.c D_80362748[1]` exists; formation divers: per `D_80371970[i]` index). RNG `uvRandF_RANLUX` is one global stream (also boats, snow, JH, RB flame, results, demo): fewer draws at high rate shift their sequences — already rate-dependent before, identical to the N64 order at REF |
| low | `*_sound.c` (HG :96-121, RB, gyro, bm, sd, cb), `env_sound.c` :250-285 | random vol/pitch per frame | covered by 60 Hz sound throttle — **done** |
| low | `replay_screen.c` `func_8032D51C` :514, `func_8032D90C` :549 | fade loop `+= 1/60` per frame; colour cycle `+= 0.003`/frame | `+= uvGfxGetFrameTime()` — **done** with `pw64_real_dt()` (60 Hz-normalised; the override may be frozen) |
| low | `camera.c` `camera_802D559C` :956-959 | fly-by camera rises 2 m/frame while occluded | `× dt/REF_DT` — **done** |
| low | `fx.c` `uvFxProps` case 1 :233-241, `:682` | model-trail segment period = lifetime/frameTime **at creation** → wrong if fps varies (uncapped) | recompute per frame from dt — **skipped** (kernel fx, fine at a steady rate) |
| low | `snap.c` :1567/1773/1820 (1e-5), `replay_screen.c` :385 (1e-6) | "frozen" frame-time sentinels, clamped to the lower bound; with 0.002 frozen screens advance 5× less game time/frame than today. Never let dt hit 0 (`/dt` above) | clamp patch: raw < 1e-4 → keep today's 0.01 (or decide) — **done** per Decisions |

## Patch plan
All in `crates/pw64-game/patches/…` (+ shadow header). Order:
1. `native/include/pw64_rate.h`: `PW64_REF_DT` (1/30), `pw64_scale()` = dt/REF_DT,
   `pw64_decay(k)` = `powf(k, pw64_scale())`, `pw64_ticks(f32 *acc)` (whole REF frames
   elapsed; for `++` counters), `pw64_walk()` = `sqrtf(REF_DT/dt)`. One helper covers every
   med/low per-frame item.
2. dt clamp (`func_80313D74`): lower bound 0.002, sentinel handling, f64 shadow of `D_8034F850`.
3. Sound throttle in `snd.c:sndEvent` (≥1/60 s accumulated) — required before any rate > 144.
4. Camera history ring 50 → 512 (`camera.h`; C-only struct, no raw offsets).
5. FDR time-based sampling.
6. Gameplay: HG contact counter, gyro trim counter, JH crouch yaw, wind random walk.
7. Visual/medium: JH legs, JH camera 3°/frame, ski lift, shuttle, birdman nudge.
8. Low items as time permits; test timers to f64 last.
Verify each with scripted runs at 30/60/144/uncapped (`PW64_FPS`), comparing trajectories/scores.

## Implementation (steps 1–7 + most of 8)
- `native/include/pw64_rate.h` + `native/src/pw64_rate.c`: `PW64_REF_DT`, `PW64_DT_MIN/MAX`,
  global `pw64_rate_scale` (REF frames per frame, set in `func_80313D74`), `pw64_scale()`,
  `pw64_scale_dt(dt)`, `pw64_decay(k)`, `pw64_ticks(&acc)`, `pw64_walk()`, `pw64_real_dt()`
  (uvGfxEnd-measured wall time, reads graphics.c `gGfxFrameTime[gGfxFbIndex^1]`, clamped).
  Patched files `#include <pw64_rate.h>`; every hunk is commented `PW64 rate:`.
- Frozen sentinel: `dt = 0.01·real/REF_DT` but `pw64_rate_scale = real/REF_DT`, so per-frame
  visuals (HUD, shutter, fades) still run at N64 speed while game time crawls.
- `pw64_rate_scale` is only fresh in loops that call `func_80313D74` (all gameplay). Menu
  loops that don't (replay_screen fades) use `pw64_real_dt()`.
- **`src/**/*.h` patches are silently ignored** by build.rs (only `.c` and `include/**` are
  patched; `check_patches_used` still accepts them). Struct changes in app headers need a
  workaround: the camera history ring lives in `code_72010.c` (keyed by the owning
  `Unk802D3658_Unk230*`, 2 slots, `pw64_fatal` on a 3rd); `camera.h` `unk0[50]` is now unused.
- Sound throttle uses `uvClkGetSec(0)`: clock ids 0–2 are unused by the game.
- Host test of the helpers (decay/ticks/scale/approach equal at 30/60/144/500 Hz): passes.

## Decisions
- **`PW64_REF_DT` = 1/30** for now. Normalising per-frame constants to it deliberately changes
  today's 60 Hz behaviour *toward the N64's* (the port at 60 Hz already differs); the
  constant is one `#define` to calibrate against a reference emulator later.
- **Frozen frame-time sentinels** (raw frame time < 1e-4, from `uvGfxSetFrameTime`): dt =
  `0.01 · real_dt / REF_DT` — the same game-time per real second the N64 got from the 0.01
  clamp at ~30 fps, independent of our rate. Never 0.
- Patches are unconditional (not gated on the rate): at any rate the game should behave as
  on the N64 at REF_DT.

## Present tick (points 1 + 3)
- Rate: `opts::fps` (`PW64_FPS` > `[graphics] fps` > default `monitor` in the window /
  60 headless) → `opts::present_rate` → `os::set_present_rate` before boot. ~60 Hz
  (59.5..60.5, incl. 59.94 panels and `monitor` with no monitor) = `None` = **the old
  path exactly**: no tick, swaps latch at the VI retrace, sched.c unchanged at runtime.
- Monitor change: with `fps = monitor`, `Moved` / `ScaleFactorChanged` / fullscreen
  toggle re-read `current_monitor().refresh_rate_millihertz()` → `opts::present_rate`
  (same ~60 → `None` rule) and queue it in `window::PendingRate`; the game thread's
  retrace hook applies it via `os::set_present_rate` (`os/mod.rs` — kernel state is
  not shared with the window thread) and logs `[window] monitor rate → …`. Only
  changed readings queue; the window's present mode stays as chosen at startup.
- OS core (`os/vi.rs`): `PresentRate::Fixed{interval}` keeps phase (`+= interval`), after a
  stall fires once and resyncs; `Uncapped` fires when a swap is pending (`next_fb != current_fb`
  = previous gfx task done) ≥ 2 ms (`MIN_PRESENT_COUNTS`) after the last tick, else a
  1/60 s keep-alive. Rates > 500 Hz act as 500 (tick ≥ 2 ms = `PW64_DT_MIN`). A tick latches `current_fb = next_fb`, then posts the message the
  scheduler registered (`pw64_present_set_event`, `PW64_PRESENT_MSG` 670). Retraces stop
  latching while `pw64_present_active()`. `deliver_due` order: timers, retraces, retried
  events, tick — a same-time retrace is handled first.
- sched.c.patch: `_uvScHandlePresent` = flip (`D_802B9C68`), drain cmdQ (audio only stored in
  `D_802B9C58`), `_uvScRunGfx` if the RSP is idle. Retrace skips its flip when present
  ticks are active; the rest (clock, timeouts, audio start/yield, clients, gfx start of an
  already flipped-in task) unchanged. Why gfx may start while audio waits: the deferred-gfx
  invariant finishes it before the next retrace is posted, so the retrace sees the RSP
  idle and starts audio directly (yield path `D_802B9C6B` unreachable, as today). Not
  starting gfx then would pin gfx to retraces: the audio task sits in cmdQ for almost the
  whole retrace interval. RSP 'a' at the tick (retrace + tick in one delivery): flip +
  drain only; `_uvScDoneAud` starts the flipped-in task.
- Timing: game dt = `osGetCount` between `uvGfxEnd`s; one `uvGfxEnd` per tick in steady
  state (it waits for the previous task, which starts at the tick), so with
  `PW64_NO_THROTTLE` (idle skipped to the next tick) dt = 1/N × 46.875/45.75 (the game's
  clock-constant mismatch, same at 60). Input scripts, `PW64_DUMP_FRAMES` and
  `PW64_MAX_RETRACES` stay retrace-keyed = game time, so runs at different `PW64_FPS` line up.
- Present hook (`os::set_present_hook`) → `Hle::latch` (sink handoff); retrace hook →
  `Hle::retrace` (dumps). 60 path: `Hle::present` = both at the retrace.
- `PW64_PROFILE_RETRACES`: per-retrace cost now sums all loop iterations since the last
  retrace (ticks included) + prints present ticks/s of game time.
- Settings overlay: "Frame rate" row (Monitor/30/60/120/144/165/240/Uncapped) and
  V-Sync are live: both live in `Gpu` (`fps`, `vsync`,
  `present_modes`), the overlay's apply sets them + `present_dirty`, and
  `apply_present()` recomputes the present tick and reconfigures the surface
  (old `monitor_moved` early return at `fps == Monitor` dropped).
- **V-Sync**, optional, **off by default** (adds input latency; the
  `fps` cap stays the main control). `PW64_VSYNC=<0|1>` > `[graphics] vsync` > off;
  live in the settings screen. Matrix:

  | vsync | fps | present tick (OS core) | present mode (window) |
  |---|---|---|---|
  | off (default) | monitor | `Fixed` timer at that rate (as above) | `Mailbox` if supported, else `Fifo` (immediate only if tearing is allowed: never with `Monitor`) |
  | off | 0 (uncapped) | uncapped (as above) | `Immediate` if supported, else `Mailbox`, else `Fifo` (tearing allowed) |
  | off | N > refresh + 1 Hz | `Fixed` timer at that rate | `Immediate` if supported, else `Mailbox`, else `Fifo` (`opts::allows_tearing`) |
  | off | N <= refresh + 1 Hz | `Fixed` timer at that rate | `Mailbox` if supported, else `Fifo` (tearing not allowed) |
  | on | any | `Display` (below), fallback = the monitor rate's interval | `Fifo` |

  Tearing rule (`opts::allows_tearing`): immediate modes only when
  the player asked for more than the display shows (uncapped, or fixed rate above the
  monitor's refresh + 1 Hz slack); an unknown refresh never allows it. Unsupported
  Immediate falls back to Mailbox with a `PRESENT_FALLBACK` once-log.

  Headless runs ignore vsync (60 Hz default); the 59.5–60.5 → old VI-path rule applies
  only with vsync off. Startup log: `[window] fps … vsync {on|off} → present tick …, …`.
- `Display` pacing (`PresentRate::Display { fallback_interval }`, os/vi.rs): the window
  thread bumps the `VBLANK` atomic right after each successful `surface.present()`
  (window.rs); a tick is due when `VBLANK` changed since the last tick (one tick per
  presented frame — the tick can never beat the real vblank, so no repeat/drop beat). If
  the counter is silent for `2 * fallback_interval` counts (headless, minimised, dumps),
  ticks fire like `Fixed { interval: fallback_interval }` would (phase kept, resync after
  a stall), keeping those runs deterministic. While paused (`os::PAUSED`), `run` delivers
  nothing and the clock resumes where it left off. `set_present_rate` initialises the
  fallback deadline (`now + 2 * interval`) and seeds `display_seen` from `VBLANK`.
- Display wake: the throttled idle sleep in `os::run` used to
  sleep to `min(retrace, fallback deadline)`, so a present was only noticed there —
  V-Sync ran at ~half the monitor rate / on retraces. Now `vi::sleep_until_vblank`
  polls `VBLANK` every 0.5 ms and ends the wait at once. `VBLANK` is process-global:
  tests touching it hold `vblank_lock()` (tests.rs).
- `VBLANK` counts presents, not vblanks: in `Fifo` the tick runs ahead until the
  swapchain is full, then `get_current_texture` blocks → pacing is backpressure (+1–2
  frames latency). Any window redraw (resize, overlay) also bumps it (harmless tick).

## Verify (Windows, Git Bash)
```sh
export PW64_INPUT_SCRIPT=crates/birdman64/scripts/fly_hang_glider.txt PW64_MAX_RETRACES=2700 \
  PW64_NO_THROTTLE=1 PW64_NO_INPUT=1 PW64_NO_AUDIO=1 PW64_PROFILE_RETRACES=1 \
  PW64_DUMP_FRAMES=600,1200,1800,2400,2700
for f in 60 144; do PW64_FPS=$f cargo run --release -p birdman64 2>&1 | tee tmp/fps$f.log
  mkdir -p tmp/fps$f; mv tmp/frame_*.png tmp/fps$f/; done
```
Expect `stopped: RetraceLimit` in both, ~144 present ticks/s in `fps144.log`, the same flight
in both PNG sets (small drift is fine; a different trajectory → check `_uvScHandlePresent`
and the rate patches first). Window: `PW64_NO_AUDIO=1 cargo run --release -p birdman64` → log
`[window] fps Monitor (monitor 144.xxx Hz) → present tick Some(Fixed…), Mailbox`, title
~144 fps; also `PW64_FPS=60`, `PW64_FPS=0`, F10 overlay.

- **Low-end preset:** `PW64_FALLBACK_ADAPTER=1` (env) forces `force_fallback_adapter`;
  software adapters (`DeviceType::Cpu`, or name contains llvmpipe/softpipe/swiftshader/
  "microsoft basic render") call `opts::set_low_end()`: default fps becomes Hz(60) and
  default scale 0.75 via `precedence`, so env/config still win. Log + toast:
  `software GPU (<name>): defaults 60 fps, 75% render resolution`. Verified on the
  fallback adapter: run A shows `Hz(60)` + `scale 0.75`; run B with `PW64_FPS=144` shows
  `Hz(144)` (the low-end default never overrides an explicit rate).
- **Fallback adapter (WARP) is GPU-bound by seconds (open issue):** a
  `device.poll(wait)` after each redraw took 1–6 s (scale 0.5: ~0.6×, so not just fill
  rate); one drain's fb work alone 2–5 s. Nothing bounds GPU-queued work (`MAX_QUEUED_OPS`
  is CPU-side), so the game runs on while the backlog grows: DX12 acquire waits out
  wgpu's 1 s frame-latency timeout on most frames → ~1 redraw/s; `presented` (+1 per
  drain with frames, window.rs) then grows ~1/s, so `PW64_WIN_SHOT=200` takes minutes,
  and a shot's readback waits out the whole backlog (5 s seen). Title fps is now
  frames / elapsed (was a raw count over multi-second redraw gaps → "352 fps").
  Tried + reverted: no frame-latency waitable (acquire µs, backlog unbounded); waiting
  for the previous drain's submission (correct bound, but then ~3 drains/15 s). Next:
  find which pass is pathological on WARP (timestamp queries / PIX); until then the
  preset can't make WARP playable.

## Monitor-rate polling + exclusive focus

- `redraw` sets `present_dirty` once a second (with the fps title), so a refresh-rate change without a window move is picked up; unchanged readings queue nothing.
- winit exclusive = topmost window + mode switch, nothing on focus loss. `Gpu::focus_changed`: focus lost in exclusive (> 1 s after the last switch) → `set_fullscreen(None)` (winit restores the desktop mode) + minimise; focus back → `set_display_mode(Exclusive)`. No automated test: check by hand (Alt+Tab, Win+L).
- winit only `debug_assert`s `ChangeDisplaySettingsExW`; `sys::video_mode_supported` tests the mode with `CDS_TEST` first, falling back to borderless with a toast.
- `sys::keep_display_awake` (`SetThreadExecutionState`, window thread) from game start to `quit`; Linux: nothing.
