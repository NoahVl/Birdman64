# Plane variant of the gyrocopter (experimental)

Code: `crates/pw64-game/native/src/pw64_plane.c` (hooks + flight model), hooks in
`gyrocopter.c.patch`, `code_7CF30.c.patch`, `code_7C4C0.c.patch`, save guard in
`game.c.patch` / `results.c.patch`. `pw64_plane` (exported data symbol, set by
`pw64_game::set_plane`): `PW64_PLANE_OFF` 0, `_BIG` 1, `_SMALL` 2 (pw64_native.h).
Settings Main page "Gyrocopter" cycles Gyrocopter / Small plane / Big plane;
`[extras] plane = "small"|"big"` (old bool accepted: `true` = big); `PW64_PLANE=0|1|small|big`
(1 = big); `PW64_START=<class>:pl:<test>` (big) / `ps` (small).

- **Latch:** `pw64_plane_run` is set by the first `gyrocopterLoadLevel` of a test
  (not in the attract demo) and kept across replay reloads; `pw64_plane_end` (test
  setup + `func_802ECE94`) clears it. Every hook returns 0 without a run: standard
  frames and eep were pixel/byte identical to the pre-change build (a:gc:1, 1700
  retraces).
- **Saves:** the plane run skips `saveFileWrite` and `pw64_plane_end` restores a
  snapshot of `D_80364210[unk9C]` taken at the latch, so the tallies it made in memory
  never reach a later save. Verified: landing with 28 pts, Another test, eep equal to
  a fresh one.
- **Models:** big = 39 unless the level didn't load it (Ever-Frost: all pilot
  class tests) → 27; small = 27 always (`sPlaneModels` index 1). The same flight
  model and the per-model geometry serve both; both sit right on the runways. Both are one part, nose +y, origin not at the CG (39's is behind
  the tail, ~20 m), so `PlaneModel.offset` is applied after every root
  `uvDobjPosm(objId, 0)` (`pw64_plane_posm`). Geometry (gear, capsules, camera) was
  measured from the meshes (part scale 0.1).
- **Gyro parts:** `func_802F50CC`/`F5358`/`F56B4` pose parts 1..5 and play janims
  that only exist on the gyro model (`uvDobjPosm` writes the slot before its
  part-count check), so the hooks skip them. Rotor-disc fx stay allocated (freed by
  the stock cleanup), alpha 0. Rotor rate `unkE8` winds to 0 (rotor sound silent);
  `unkF0` drives the engine sound. Shadow stays the gyro's blob; the wreck is the
  plane model.
- **Physics:** replaces `func_80308478` (no hover lock), `func_80307BAC` (wing lift,
  CD0 + induced drag, fin; wing tip capsule instead of the rotor) and `func_80307EA8`
  (prop thrust falling with airspeed); airborne `func_80304738` (attitude hold, bank
  ≤55°, pitch ≤35°, coordinated yaw, AoA stall nose drop) and airborne
  `func_80309090` (no autorotation/trim/speed cap). Ground handling stays the gyro's.
- **Tumble guard:** `func_8030399C` tumbles past |pitch| 0.803 / |roll| 1.065; rates
  are zeroed beyond 0.72 / 1.0.
- **Ground:** after a touchdown >1 m/s the ground solver cancels lift for 4 s
  (`unk8C` < 4); stick back above 15 m/s ends that (takeoff/touch-and-go). Throttle
  closed on the wheels spoils lift (×0.3) and brakes; the test ends at <0.1 m/s.
  Lift-off chatters 0↔1 for ~0.7 s (harmless).
- **Numbers seen:** lift-off ~90-110 km/h HUD, cruise (throttle 0.5) ~115, full ~150,
  stall ~55. The gyro runways are short: land slow (cut throttle early).
- Open: no plane-specific sound/shadow; split-screen flies two planes (one global flag).
