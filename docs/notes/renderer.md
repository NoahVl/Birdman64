# Notes: renderer (`crates/pw64-gfx`, `crates/pw64-viewer`)

## Architecture
- `Interpreter::run(&dyn Memory, dl)` → `Frame`. `Memory` = big-endian, **physical** addresses; the interpreter does
  segment resolution (`seg[(a>>24)&0xF] + (a & 0xFFFFFF)`, masked to 29 bits), so K0 pointers (`0x80xxxxxx`) resolve
  via segment 0. Native game: implement `Memory` over the RDRAM window (host `0x8000_0000 + phys`).
- RSP work on the CPU (F3D, `interp.rs`): matrix stack (`matrix.rs`, projection + 11-level modelview = Fast3D's 10 saved + current, overflow push ignored but still loads, warned once per session (gfx.md), MUL pre-multiplies),
  16-entry vertex cache, lighting, texgen, fog → shade alpha, flat shading (per-triangle color copy).
- Vertices leave the interpreter in "N64 screen clip space": `xy/w` = N64 pixels (viewport applied), `z/w` = reversed
  depth `(w - z)/2w`. The renderer maps 320×240 → target per draw (uniform `screen`), so resolution, aspect
  (pillar/letterbox) and widescreen are renderer-only concerns.
- Draws are merged while pipeline key, uniforms, texture bindings, scissor and 2D/3D flag stay equal; submission order
  is kept (the game orders translucent geometry itself).
- `renderer.rs`: wgpu. Caches: shader module per `ShaderKey`, pipeline per `PipelineKey` (+ depth/cull/blend), texture per
  content key (CPU box-filter mip chain; RGB of alpha-0 texels dilated to stop bilinear/mip bleeding — **not** for I
  textures, r=g=b=a: there alpha-0 = black color the combiner uses), sampler per wrap/filter,
  bind group per texture pair. One uniform slot (256 B) per draw with dynamic offsets. `Depth32Float` reversed-Z (clear 0,
  `GreaterEqual`), decals = depth bias. MSAA via `RenderOptions::msaa`. `render_to_rgba` for headless screenshots.

## Combiner / blender → WGSL (`combiner.rs`, `rdp.rs`, `shader.rs`)
- Key = raw 56-bit mux + 2-cycle flag. Generated `combine(t0,t1,shade,nz)` does `(A-B)*C+D` per active cycle, clamped.
  1-cycle runs the **second** cycle's fields; in cycle 2, TEXEL0/TEXEL1 swap. LOD_FRACTION = per-pixel
  RDP LOD (`lf`, see "LOD fraction + 3-point filter").
- Blender: 2-cycle first cycle is evaluated in the shader (fog: `G_RM_FOG_SHADE_A`). Last cycle → GPU state:
  `M=MEM, B=1-A` with `FORCE_BL` or `AA_EN|CVG_X_ALPHA` → alpha blend; `P=M=MEM` → no color write; else opaque.
  `CVG_X_ALPHA`: discard `a < 1/8` when blending, `a < 0.5` otherwise; `G_AC_THRESHOLD` vs blend alpha.
- Depth: `Z_CMP`/`Z_UPD` only if geometry mode `G_ZBUFFER`; `ZMODE_DEC` → bias. FILLRECT in fill mode with
  `cimg == zimg` = depth clear (writes the fill word's z via `rdp::zbuffer_word_to_depth`, the game's = far).
- Color image = z image (`ShaderMode::DepthImage`, e.g. `uvGfxStateDrawDL` shadow volumes: pass 1 draws the volume's
  back faces black into the z-buffer → z = 0 (nearest) where they are in front of the scene, so pass 2's translucent
  front faces only land where the scene is inside the volume). Renderer: step `ZMark` depth-tests, stencil = 1;
  step `ZWrite` (stencil == 1, `fs_depth`) writes the blender color's RGBA5551 word as depth. Depth format is
  `Depth32FloatStencil8` if the device has it (`pw64_gfx::device_descriptor`), else `Depth24PlusStencil8`.
  Before this, pass 1 was drawn as color = the big black polygon under the hang glider.
- Textures: `texture::bind_tile` decodes the tile from `pw64_formats::tmem::Tmem` with size = `2^mask` (wrapping axes) or
  the tile rectangle (clamped / mask 0). Key = FNV-1a of format, size, covered TMEM bytes (+TLUT for CI): stable across
  runs → `TextureCache::replacer` is the texture-pack hook. Shader UV = `(st*shift - (ul - 0.5 if bilinear)) / size`.
- Bilinear TEXRECTs: the RDP samples S/T at each pixel's top-left, the GPU at the center → st shifted by -½·dsdx/dtdy,
  and `Vertex::st_clamp` limits S/T to the first/last pixel's texel (at >1× resolution bilinear would otherwise
  reach the next glyph row / garbage TMEM: dotted lines under all HUD/menu text).
- 1:1 bilinear TEXRECTs (sprite blits/fonts) bind **point** tiles (2026-09-29, "Top Score" fix): with unit deltas and
  integral corner alignment the RDP never blends (every pixel corner is exactly on a texel center), so linear only
  hurt — the up-scaled GPU render blended neighbouring texels, and IA blits' white "invisible" background
  (I=255, A=0) bled bright halos around every glyph. Scaled rects (map screen) keep bilinear — real hardware
  filters those. Point tiles span texel k over [k, k+1), so these rects shift st by ½−½·d (not −½·d) and clamp to
  the edge texels' *centers*; with the −½ shift the up-scaled image sat half an N64 pixel off and lost its last
  row/column ("TEST" rendered as "TFST"). Fixed 2026-09-30.
- VI overscan: `Renderer::vi_border` (fb.rs) blacks the outermost top row + right column of each output rect —
  the game's own letterbox (`drawScreenBorder` → `uvVtxRect(0, SCREEN_HEIGHT-1, SCREEN_WIDTH-1, …)`, 1-cycle
  texrects, exclusive lower-right edges — our edge rule is right) stops one N64 pixel short of the top/right
  edge; a TV hides it. Width = ceil(h/240). Widescreen skips the right column (extended view reaches the edge).
- 3D draws get scissor ∩ (viewport × `gSPClipRatio`) (`G_MW_CLIP`, default 2): menus use ratio 1 + a full-screen
  scissor, so 3D must stop at the viewport. With ratio 2 (flight objects) geometry may still leak a few rows into
  the black border — hardware does the same (hidden by overscan).
- Debug: `PW64_DUMP_DL=1` / `PW64_DUMP_PIXEL=x,y` (see `crates/birdman64/src/hle.rs`); `Interpreter::trace` (vertex lines
  include S/T in texels). `PW64_DUMP_TEX` PNGs are pre-dilation (what TMEM holds, not what the GPU samples).

## LOD fraction + 3-point filter (2026-09-29)
- **LOD (default on):** per draw `DrawUniforms::lodp` = (`LodMode`, max level = `gSPTexture` level, ½ if chain is
  bilinear, 0); shader `rdp_lod` per pixel: L = max |dS|,|dT| over `dpdx`/`dpdy` of the pre-shift S/T; L < 1 →
  fraction 0; floor(log2 L) ≥ max level or L ≥ 256 → "distant", fraction 1; else fraction = L/2^t − 1 (linear in the
  octave, the RDP's `((lod<<3)>>t)&0xFF`, not fract(log2)). L is per **target** pixel (1× = HW; at 3× the level-1 blend
  starts 3× farther — sharper, like the GPU mips of TEXEL0; scaling L by target/N64 px would give the HW distances).
- `LodMode::Tile` (`G_TL_TILE`, what PW64 uses): TEXEL0/1 stay tile/tile+1, only the fraction is computed. Only for
  triangles with max level ≥ 1 whose combiner reads LOD_FRACTION (`CombinerKey::uses_lod_frac`); max level 0 would
  be fraction 1 on HW (always "distant" → an unrelated TEXEL1), so it stays 0 = old output, as do rects. With max
  level ≥ 2 tile mode saw-tooths per octave (HW does too). TEXEL0 keeps its GPU box mips (no minification aliasing).
- `LodMode::Chain` (`G_TL_LOD`, never seen in PW64 — logged once if hit): `Interpreter::bind_chain` binds tiles
  base..=base+max as one texture (`Frame::chains`: key → level keys; renderer `upload_chain` uses the N64 tiles as
  the mip levels) if each halves size/shift/origin with equal wrap and the (replaced) images halve; shader picks
  levels t/t+1 per pixel with `textureSampleLevel`, UV + (2^l − 1)·½/w0 (each level's own texel centers). Else
  falls back to Tile.
- **3-point** (`RenderOptions::filter` = `TexFilter::N64`; `PW64_FILTER=n64`, `[graphics] filter`, settings row
  live, viewer `--filter n64`): only bilinear tiles (`filt` uniform, renderer-filled: flag + wrap code per texel).
  `texel3` = RDP triangle (fx+fy<1: t00,t10,t01 else t11,t01,t10) via `textureLoad` with clamp/repeat/mirror
  indexing = the samplers'; texture-pack images use their own `textureDimensions`. While magnifying pure 3-point;
  minifying it fades into the GPU trilinear/aniso sample over one LOD step (`filter3`) — the N64 has no mips and
  would alias. Shaders + pipelines are cached per filter, so a live switch just builds the other set.
- Bilinear default = old pixels for non-mip draws: `tests/render_identity.rs` (lavapipe hashes; `lodp` zeroed =
  pre-change constants), `tests/interp_identity.rs` (`frame_hash_pre_lod` = old constants). GPU checks vs CPU
  formulas: `tests/lod_filter.rs` (receding plane tile/chain, 2×2 3-point clamp/repeat).

## Viewer (`pw64-viewer`)
- Builds an arena like the engine: UVTX images + their own lists (SetTImg patched: 1st → own image, later → `image2`),
  UVCT/UVMD vertices and expanded state lists, `gGfxDList1/2`. Per frame: clear (z via color image, then sky fill),
  camera-relative matrices loaded `LOAD|PUSH` per cell (`uvGfx_802236CC`), placements push/pop per part depth
  (`uvSobj_8022C8D0`, billboards `8022CC28`), LOD by distance (`uvSobjGetLODIndex`), transparent models sorted last.
  States go through a port of `uvGfxStateDraw` (render-mode switch, fog combiner override).
- Game camera: near 1, far 2000, fog factor 0.996 (`environment.c`), frustum ±0.49×±0.35 at near. The viewer defaults
  to fog off / far 16000 for overviews; `--fog 0.996 --near 1 --far 2000` looks like the game.
- Level polish (`scene.rs`): `--env N` draws UVEN N (`_uvEnvDraw`: LOD 0 / part 0 states of each env model at the
  origin, camera-follow in x/y via flag 8, far projection via flag 2, fog per flag 4) plus the env callback haze
  (`env_802E0CF0`: a fog-colored translucent quad across the view at the horizon, opaque at `far`, fading at
  0.875 × `far`, hidden when the camera pitches up). `--pal N` remaps texture slots through UVTP (`uvLevelAppend`:
  image2 looked up by slot, so it remaps too). `--setup N` picks terra+env+palette like `envGetCurrentId`.
- UVTX scrolling (`texture_draw`): `uvSprt_802301A4` accumulates speed × time, wrapped to [0,1); the viewer bakes
  it into `gDPSetTileSize` offsets (own image tile 1, image2 tile 0). `--time T` for headless screenshots.

## 144 Hz profiling (2026-09-28)
Measurement for the 144 Hz design (framerate.md) (budget ≈ 6.9 ms/present). Instrumentation:
`PW64_PROFILE_RETRACES=1` (temporary, in-tree): per-retrace wall time in `os::run`
(`pw64-platform/src/os/mod.rs`, min/avg/p50/p95/p99/max, whole run + last half),
plus splits — gfx/audio task HLE (`headless.rs run_task`), DL interpretation and
GPU dump renders (`pw64/src/hle.rs`). Near-zero cost when unset. Method: scripted
flights, 2700 retraces, headless (no GPU present), `PW64_NO_THROTTLE=1
PW64_NO_INPUT=1 PW64_NO_AUDIO=1 PW64_AUDIO_VIRTUAL=1` (host CPU: laptop i7/RTX 3060).

- **CPU ms/retrace** (whole game thread: coroutine OS + DL HLE + audio HLE + VI hook; steady state = "last half", past title/menus):
  glider avg 2.75 / p50 2.88 / p95 3.81 / p99 4.17; gyrocopter 2.41 / 2.55 / 3.08 / 3.25;
  rocket belt 1.27 / 0.84 / 2.94 / 3.27. All three: max ≈ 14 ms (one-off, level load),
  whole-run avg ≈ half the last-half value (title/menus are cheap).
- **Splits:** DL interpretation = gfx-HLE ≈ 100 % of the "RSP" time (1.29–1.96 ms/task
  whole-run; in flight ≈ 2–2.5 ms/task — it tracks the triangle/command count). Audio
  HLE 0.06–0.07 ms/task. Everything else (coroutine OS, scheduling, VI hook/present)
  ≈ 0.1–0.9 ms/retrace. **The DL interpreter is the CPU suspect.**
- **Per-frame GBI workload** (in-flight frames; `PW64_DUMP_DL` trace follows `G_DL`
  sublists, so it counts more than the OS task's `data_size/8`):
  glider 8235 commands, 2346 tris, 532 `G_VTX`, 177 texture loads (182 merged draws);
  gyrocopter 11161 / 3241 / 884 / 224 (251 draws); belt 1859 / 476 / 79 (63 draws).
  State churn is heavy: `G_SETTILE`(858)/`SETTILESIZE`(685)/`SETOTHERMODE_*`(1176) on
  the glider — pipeline-key churn, not triangle count, may drive interp cost.
- **GPU present proxy** (headless dump renders at 640×480, incl. readback; RTX 3060
  Laptop): 2.83 ms/frame at MSAA 1, 4.61 ms at MSAA 4 (+63 %). A 960×720 window render
  is 2.25× the pixels (readback excluded, present included) → **MSAA 4 or `PW64_SCALE=2`
  at 144 Hz very likely blow the 6.9 ms budget** (scale 2 = 4× the 960×720 pixels).
- **Present pacing today (from code):** `window.rs` presents on `UserEvent::Frame`
  (one per VI retrace, latest-frame slot) with `PresentMode::AutoVsync` (FIFO) and
  `desired_maximum_frame_latency 2`; the event loop is otherwise idle, so the present
  rate equals the game's 60 Hz retrace cadence — a 144 Hz display gains nothing yet.
- **Suspects for the budget, ranked:** (1) re-rendering each interpolated frame must
  repeat the ~2–2.5 ms DL-HLE + a GPU render — only the DL-HLE part repeats per
  vehicle frame today; (2) window GPU render scale (MSAA/`PW64_SCALE`); (3) wgpu
  per-draw overhead (uniform slot + pipeline switch per draw, 63–251 draws/frame);
  (4) audio HLE is negligible.
- **Only a real windowed run on the 144 Hz panel can tell:** actual present rate/fps
  (the counter is window-title only), true 960×720 present cost without readback,
  vsync queue depth/latency on that panel, and driver overhead per draw at 144 Hz.
- **Render scale:** `PW64_SCALE` min lowered to 0.5 (was 1.0):
  below 1 the game renders small and `fb_present` upscales with the Scale filter
  (blurry; `nearest` gives the blocky look), above 1 supersamples. Sizes come from
  the pure `Gpu::fb_size_for(scale, out, max_tex)` (shared by `fb_size_for_scale`
  and the settings screen's "Render resolution" value; unit test `fb_size_for`).
- Pre-existing quirk found: with `PW64_DUMP_FRAMES=<n>` (every-retrace dumps) the
  per-retrace host cost exceeds one VI period, the OS loop never idles (the virtual
  clock advances with the slow hook, so a retrace is always due) and the
  `PW64_MAX_RETRACES` stop check in `os::run` never runs — the game just runs in slow
  motion. Harmless for sparse dump lists and real-time runs; matters only for scripts
  that assume the retrace cap.

## DL interpreter performance (2026-09-29)
- Benchmark: `cargo run --release -p pw64-gfx --example bench_interp [-- <iters>]` runs
  `tests/synth` (generated in-flight-sized frame: 9.8 k commands, 2.75 k tris, ~260 TMEM loads,
  SETTILE/SETTILESIZE/othermode churn; no ROM data) through one long-lived `Interpreter`.
  Cloud Xeon 2.1 GHz: **2.23 → 0.48 ms/frame, 228 → 49 ns/command** (4.6×). `run_into(mem, dl,
  &mut frame)` reuses a Frame's allocations (−2 %; `run` already pre-sizes from the last frame).
- Real frames: `pw64_gfx::capture` records every `Memory` answer of one run (words, bytes, raw,
  `map`) as sparse pages → replays identically on any host. Game hookup is one line in
  `pw64/src/hle.rs` (not wired yet): `self.interp.run(&self.mem, dl)` →
  `pw64_gfx::capture::run(&mut self.interp, &self.mem, dl)`; then `PW64_CAPTURE_DL=<n>[,<n>..]`
  writes gfx task n (1-based) to `tmp/dl_capture_<n>.bin` (warns if the replay differs), and
  `PW64_BENCH_CAPTURE=tmp/dl_capture_<n>.bin cargo run --release -p pw64-gfx --example bench_interp`
  benchmarks it. Captures are ROM-derived: tmp/ only, never commit.
  Real frame (RTX 3060 Laptop, capture of gfx task 1800 mid-flight,
  13.6 k commands / 3767 tris / 312 draws): run 0.986 ms/frame (72.3 ns/cmd),
  run_into 0.772 ms/frame (56.6 ns/cmd) — medians. Later batch
  (RTX 3060 Laptop, `fly_hang_glider.txt`, `PW64_CAPTURE_DL=1790..1810`):
  in-flight task 30 (6.2 k commands / 3408 tris / 88 draws) run 0.573 ms (92.7 ns/cmd),
  run_into 0.361 ms (58.4 ns/cmd); briefing task 10 (3.9 k commands / 1528 tris)
  run 0.361 ms (92.4 ns/cmd), run_into 0.255 ms (65.1 ns/cmd) — the per-command cost is
  unchanged; smaller absolute ms than task 1800's simply reflects command count.
- `GfxMemory::read_u32` RDRAM fast path (2 range checks vs `copy` → `mapped`):
  measured on the real game (`PW64_PROFILE_RETRACES=1`, scripted flight 2688 tasks,
  2 runs each, otherwise identical tree): 0.380 → 0.376 ms/task DL interpretation
  (~1 %) — below the ≥ 3 % keep rule, **reverted**; `copy`'s `OnceLock` hit is
  evidently cheaper than feared.
- Output identity: `tests/interp_identity.rs` hashes the whole Frame (vertex bits, draws via
  `Debug`, texture keys + pixels) against constants recorded with the old interpreter, cold and
  warm cache, `run_into`, RDRAM changes between runs, capture round trip. Re-record only for an
  intended output change.
- What was slow (callgrind + `perf`, `/usr/lib/linux-tools-*/perf` works in the cloud container):
  every template rebuild (each state change before a tri) re-hashed the bound tile's TMEM bytes
  (FNV-1a byte loop, ~4 cycles/byte, + a Vec) and `uses_texel` built ~16 Strings; per-triangle
  Arc/HashMap texture registration + 300-byte template clones (memcpy + store-forwarding stalls);
  byte-wise TMEM loads; a Vec per G_VTX / texture load.
- Fixes: `BindMemo` per tile (valid while no TMEM load happened — `tmem_gen` — and tile
  desc/size, TLUT mode, filter are equal); `texture::KeyMemo` maps a fast 4-lane hash of the key
  input bytes to (stored bytes, FNV key), **verified by byte compare**, so keys stay exactly the
  FNV content keys packs/`PW64_DUMP_TEX` use (64 MiB cap, then cleared); allocation-free
  `uses_texel` (exhaustively tested vs the string scan); textures registered in the Frame once per
  template; boxed template + serial (same template ⇒ skip draw-merge compares); tri vertices
  pushed straight into the frame; TMEM loads by 64-bit word (`rotate_left(4)` = odd-line swap;
  tested vs the old byte loops); G_VTX colors from one `read_raw` of the whole range.
- Left (steady state): TMEM loads ~20 % (inherent copy), key verification ~13 %, vertex
  transform/lighting (divides kept for bit-identity), command fetch. In the game,
  `GfxMemory::read_u32` does a `mapped()` range check per word (2 per command) — worth a fast path.

## Known gaps
- No dithering, coverage/AA emulation, chroma key, YUV, detail/sharpen textures, prim min level. 3D triangle S/T
  still sampled at pixel centers.
- Viewer `GFX_STATE_2000000` (`uvGfxStateDrawDL`): both passes are emitted now — pass 1 = `color_image(ZBUFFER)`
  with the black/alpha-1 combiner (`CC_BLACK_A1`, blender word `0x0001` = nearest) and `G_CULL_FRONT` (back faces),
  pass 2 = the translucent front faces (`pw64-gfx`'s `DepthImage` mode). Z-image draws ignore blending and in-pass
  ordering (each triangle tests the pre-pass depth; fine for the constant-black shadow pass). Only UVMD 274/288/297/317
  (runtime shadow blobs) carry `GFX_STATE_DRAW_DL`; no UVCT/UVEN state does, so the branch is unexercisable from
  static scene data — a synthetic test (`scene::tests::state_draw_dl_draws_both_passes`) drives it end-to-end.
- Native game shadow (verified Sep 2026, `fly_hang_glider.txt`): the glider's shadow is a volume (wing outline
  extruded 23050 units down, `G_FOG` still on → shade alpha = fog, ~0.6 black). Visible only when the ground under
  the glider is on screen (low altitude, ~2600–2800); higher up the ground patch is below the screen and pass 1 just
  z-marks the volume sides (the pair of draws changes no pixel — correct, as on HW). Checked 1× and MSAA 4×
  (`PW64_DUMP_PIXEL` inside the shadow: ×0.4 after the pass-2 draw). Pass 1 leaves z = nearest over the volume,
  hiding any later 3D there (HW does the same; the game draws shadows late). `fs_depth` uses the combiner output,
  not the blender (fine for `G_RM_PASS` + alpha 1). Native runs are not bit-deterministic across launches.
- `uvGfxTextureDL`'s `GFX_PATCH_DL` decal patch is not in the viewer. Env models only draw their LOD 0 / part 0;
  no LOD by distance for them (the real ones are far away anyway). Lighting/texgen are approximate (normals ×
  modelview, no lookat).
- Thin light fringes on canopy edge strips (alpha-edged textures seen edge-on).
- Widescreen: 2D-textured menu backgrounds (pilot select) stay pillarboxed; menus/briefing stay 4:3-centred
  (flight HUD is edge-anchored). See "Widescreen" below.
- Native game sweep (`fly_hang_glider.txt`, frames 1080–3150) after the fixes above: no obvious errors left.
  Sky light bands (flight 1700–2200, title demo 600) were two things: (a) **bug, fixed**: the UVEN sky ring (z 592 →
  −100, T 49 → −15, I4 256×32 cloud strip, S wrap / T clamp, combiner `(1−T0)·SHADE+T0`) clamps T > 31 to texel row
  31 (all I = 0 → pure shade, seamless with the cap above); `dilate_transparent` had recoloured that row from rows
  30 and (wrapped) 0 → a pale band with vertical streaks over T 31..49 plus a hard seam at the ring top. (b) **content**:
  near thermals are big translucent cylinders (UVMD, no cull, vertex alpha 0 top/bottom, `T0·T1·SHADE` with two
  scrolling tiles of one texture) → a pale column with a hard silhouette edge, tilted when banking. 1–2 px of 3D (castle tip, thermals) in the top border row during flight (ratio-2 guard
  band + full-screen scissor, likely HW-accurate); translucent thermal columns on the briefing map (1550) are real.
  3D triangle S/T still use pixel-center sampling (texrects fixed only).
- Visual sweep of all 3 vehicles (`fly_*.txt`, 3000 retraces, Sep 2026): no renderer artifacts in flight
  (glider 600–2900, gyrocopter 1800–3000, rocket belt 1900–3000 — correct letterbox/HUD/compass/gauges,
  no black polygons, garbled textures or glitch rows). Glider frame 3000 full white = the script flies it
  into the cliff (9 m at 2700) → crash fade at 2900+; rocket belt 1700 black = briefing→level load fade.
  Both are game behaviour, not renderer bugs. Rocket belt thrust = **A** (hold); Z alone fires the
  flame VFX and burns fuel but never lifts (Z sets thrust target 0 in `rocketBeltMovementFrame`
  while `rocket_belt.c` still burns fuel + lights the flame) — `fly_rocket_belt.txt` holds A.
  Sky bands above (also title demo 600): fixed / explained, see the sweep bullet.

## Native game (`pw64` HLE) notes
- Sprite lists (libultra sprite.c `drawbitmap`, title screen): `TEXRECT, RDPHALF_1 (0xB3, s/t), RDPHALF_2
  (0xB2, dsdx/dtdy)` — contiguous; sprite.c only renumbers the halves (old gbi.h). An early "NOOP padding"
  was the 16-byte native `Gfx` union (fixed); `G_TEXRECT` again consumes exactly the next two
  commands (test `tex_rect_consumes_two_halves`).
- Host-LE memory: `Memory::read_bytes` swaps u16s for the native game, so byte fields (`Vtx.cn`, `Light_t`) are
  read with `read_raw` (test `vertex_color_bytes_are_read_raw`).
- Quality envs (window + PNG dumps): `PW64_MSAA=<1|4|8>` → `RenderOptions::msaa` (window + dump renderers;
  dumps stay at GFX_W×GFX_H so dump coords/`PW64_DUMP_PIXEL` keep their meaning). Adapter support = wgpu format
  feature flags (`adapter.get_texture_format_features(fmt).flags.sample_count_supported(n)` — X4/X8 bits), fallback
  to the highest supported count below (8→4→1) with an eprintln. The check covers the depth-stencil format too, and uses the WebGPU guarantee (1/4 samples)
  unless the device has `TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES` (`device_descriptor` requests it — and
  DEPTH32FLOAT_STENCIL8 — only when the adapter has it: requiring a missing feature fails `request_device`;
  unchecked 8× was a wgpu validation panic). `pw64/src/opts.rs` parses (shared by `window.rs`/`hle.rs`; invalid/absent → 1).
- `PW64_SCALE=<f>` (window only): the framebuffer targets (next section) are sized output area × f (`Gpu::fb_size`,
  f clamped so neither side exceeds `max_texture_dimension_2d` (8192)); `Renderer::fb_present` downsamples the shown
  one into the surface with `blit`'s machinery (fullscreen triangle, cached pipeline/sampler per filter,
  `PW64_SCALE_FILTER` linear default; equal sizes → nearest = exact copy). Linear with both axes shrinking > 2× uses `fs_box_main` instead (8×8 taps
  over the dest pixel's footprint, footprint = `fwidth(uv)`, so no size uniform; one bind group layout for both
  entry points). GPU tests skip without an adapter; in a Linux container `apt-get install mesa-vulkan-drivers`
  (lavapipe) makes them run. Blit test (`blit_downsamples_a_scaled_render`) builds a `Frame` by hand —
  `Fill` draws with a real `PipelineKey`/`DepthState`; identity-modelview DLs produce no draws (no projection
  mapping), don't reuse that shortcut.

- Window icon: the title-bar/taskbar icon is `assets/icon/birdman64-64.png` (render.py)
  decoded in `window.rs` `window_icon()` and applied at window creation (`with_window_icon`,
  winit RGBA Icon). Windows additionally embeds the .ico + VERSIONINFO as an exe resource
  (`crates/birdman64/pw64.rc` via embed-resource in build.rs, target-OS gated: `CARGO_CFG_TARGET_OS`,
  not `cfg!(windows)`), so the taskbar shows it even for the gui-subsystem exe.

## Framebuffer persistence (`pw64-gfx` `renderer/fb.rs`, 2026-09-29)
- Why: the N64 keeps framebuffer pixels until overwritten; the game clears with its own fill rects, and some
  screens draw once and then only patch part of the image (snap.c photo grid `func_8033D3EC`: cells + film strip
  drawn into both fbs, then text-only frames; single photo `func_8033DFD0`/`func_8033DDD8`: then empty tasks;
  replay_screen.c `screen_fadeout`: alpha quads accumulate over a copied frame). Rendering each task onto a
  cleared target showed only the last task's draws (empty "Check which photo ?" grid).
- Model: one persistent colour target per fb address (physical, `fb_key`), created opaque black, sized to the
  **output area only** (4:3 or the widescreen aspect; no letterbox bars) → a resize or live `PW64_SCALE` change
  just resamples the old contents (static screens survive). `fb_draw` = `draw_frame(persist)`: color `Load`,
  depth still cleared; empty tasks change nothing. `fb_copy` = texture copy. `fb_present` blits the shown target
  into `output_rect` of the surface (black around). `FbOp::{Draw, Copy}` is the replayable op stream.
- MSAA: resolve-and-reload, not a persistent MSAA texture per fb. Pass 1 (`fs_load`, `textureLoad` at the pixel)
  writes the resolved target into every sample of the shared MSAA attachment, pass 2 draws + resolves back
  (resolve(all samples = c) = c: exact). Cost: one fullscreen pass per task; a per-fb MSAA texture would cost up
  to 265 MB each at 4K × 8. Separate passes because the target is both sampled (pass 1) and the resolve target
  (pass 2) — one pass would be a usage conflict.
- Depth is **not** persisted: the z image is shared on the N64, but every z-buffered channel clears it first
  (`chan.c` `uvChan_80204FE4` → `uvGfx_80222A98` full-screen fill of the z image unless channel flag 4 — no caller
  of `uvChan_80204A8C` sets 4) and `dobj.c` does too, so no task reads an older task's z.
- Widescreen margins aren't N64 memory: a task that draws nothing outside the 4:3 columns (by its draws' target
  scissors) gets black margins first (1×: `fs_black` scissored pass; MSAA: reload only the 4:3 columns), as before
  persistence — else a pillarboxed menu after flight would show stale world in the bars. Tasks that do draw there
  (world view, stretched fills such as `screen_fadeout`'s quads, edge-anchored HUD) keep the margins persistent.
- `uvCopyFrameBuf` → graphics.c.patch → `pw64_fb_copy(dst, src)` (pw64-platform `headless.rs`: runs a started
  gfx task first, then the host hook) → `Hle::fb_copy`. The RDRAM memcpy stays.
- Photo-view flicker (fixed 2026-10-02): snap.c spins 0.1 s (`uvClkGetSec` loop) before `uvCopyFrameBuf(0)`
  expecting the 2nd `func_8033ADD4` task to finish meanwhile; natively the spin never yielded, so that task was
  still queued in sched (not even pending) at the copy → the black fb was copied over the framed one → one fb had
  the frame + "Save this Photo?", the other only photo + "▶No", alternating every retrace (headless too; the
  every-50 dumps in sweep.md always hit the same phase). Fix: `osYieldThread()` in the 3 snap.c spins
  (snap.c.patch). Check such flows with *consecutive* dump retraces.
- CPU pixel writes (snow): `snowDraw`'s `0xFFFF` writes into the previous, finished fb's RDRAM don't reach the
  GPU targets → snow.c.patch also calls `pw64_fb_pixels(fb, idx, count, color)` (headless.rs, like
  `pw64_fb_copy`) → `Hle::fb_pixels`: `pw64-gfx::pixels::fill_pixels_dl` encodes the indexes (y*320+x) as
  1-pixel FILL rects (G_SETCIMG needed — a FILL rect on cimg==zimg is a depth clear) run on a separate
  `snow_interp`, then the same `Hle::submit` path as a gfx task.
- pw64 wiring: `Hle` (game thread) records `FbOp`s in game order and hands them + the newly latched fb to the
  `FrameSink` at each swap latch (retrace, or the present tick with `PW64_FPS`, framerate.md); the window queues
  them, replays **all** of them on every `UserEvent::Frame` (so the queue stays short even without redraws) and
  `redraw` presents the shown fb.
- Backpressure: the queue is bounded (`window.rs` `MAX_QUEUED_OPS` = 32); a full queue makes the game thread
  wait in the sink (condvar, 50 ms re-check, never while `QUIT`). Dropping ops is not an option (persistent
  targets); stalling the OS core is: its clock runs on, so dt grows like a slow N64 frame. Only bites when the
  window thread blocks (FIFO present, modal move loop) or the GPU can't follow an uncapped rate.
- Present mode: FIFO (`AutoVsync`) on the 60 Hz VI path as before; with a present tick **Mailbox** when the
  surface offers it (newest frame at the next vblank, window thread never blocks; a tick at the monitor's
  refresh drifts against vblank, so a frame repeats/drops once per beat period 1/|Δf| — minutes with
  `monitor`), else FIFO. Rejected: Immediate/AutoNoVsync (tearing); vblank-driven ticks (best, but needs
  window→OS-core signalling; later). `PW64_DUMP_FRAMES`: the dump
  renderer applies every task too (only when dumps are on) and reads back the shown target. `PW64_DUMP_PIXEL`
  prefixes and `PW64_GFX_SHOTS` still render one task on black.
- Not honoured (CPU access to framebuffer RDRAM): `gGfxCallback` = snow.c `snowDraw` (writes 0xFFFF at a list of
  precomputed pixel indices `sSnowData->fbIdx` into the just-finished fb on the CPU — snow areas likely show no
  snow; a fix would hand that index list to the renderer as white points on that fb); filesystem.c GZIP
  decompression uses the back fb as scratch (garbage the next frame overwrites on HW). The first-present /
  `PW64_FB_SHOTS` RDRAM reads never contained our rendering anyway.

## Widescreen (`PW64_WIDESCREEN=1|<w>:<h>`, Hor+)
- Why not rewrite the projection in the HLE: the image is already resolution/aspect independent (vertices in N64
  pixels), so geometry outside x 0..320 just needs to not be cropped — projection and 4:3 image stay exactly as on
  HW. What the HLE can't fix is the game's CPU culling, so a small C patch set does that.
- `pw64-gfx`: `RenderOptions::widescreen` = `Widescreen::{Off, Fill, Aspect(a)}` → output area of aspect `a` fitted
  in the target, 4:3 area centred in it. Interpreter tags each draw `DrawCall::wide` (`interp::wide_class`):
  `Extend` = perspective projection (`proj[0..3][3] != 0`); `Stretch([x0,x1])` = untextured, affine, spans
  `Wide::MAIN_VIEW` (x ≤ 16 and ≥ 304: clears, fades, letterbox bars — the crash fade is 10..310); else `Fixed`.
  Renderer (`placement`): `Extend` with a main-view scissor keeps the 4:3 mapping but scissors to the full output
  width; `Stretch` maps x0..x1 onto the full width; `Fixed` = 4:3. Off = old placement (dumps identical within
  run-to-run noise).
- World-view tags: `Interpreter::wide_tags` (set by `pw64/hle.rs`) → only perspective draws between `G_NOOP` w1 =
  `WIDE_TAG_ON`/`OFF` ("PWW1"/"PWW0") extend; else pilot select's glider (3D over a 2D pillarboxed background) pokes
  out. The viewer doesn't set it (all perspective extends).
- C side (`native/src/pw64_widescreen.c`, `pw64_game::set_widescreen_aspect` before boot): channel = world view if it
  draws terrain (`unk0 & 2`) or an env (`unk2 != 0xFFFF`) and its viewport is the main view.
  `code_7150.c` `func_802061A0`: frustum x (planes `unk298` → `unk2E0` for objects/fx/sprites, corners `unk208` →
  terrain footprint `unk250`) × `240·aspect/(viewX1-viewX0)·1.02`. `chan.c` `uvChan_80204FE4`: recomputes the planes
  per draw (viewport may be set after the frustum) and emits the ON/OFF tags. `code_D2B10.c` `drawScreenBorder`: no
  side bars (they'd sit inside the wide image). Per-object projections (`dobj.c` `unk34 & 2`) stay 4:3; fine.
- Window opens at `720·a × 720`; PNG dumps `480·a × 480` (even width), height
  overridable per run with `PW64_DUMP_HEIGHT=<px>` (width even at the output
  aspect; fixed per run so `PW64_DUMP_PIXEL` coordinates stay put).
- Verified (hang glider 16:9 + 21:9, rocket belt): title, menus, briefing, flight, crash fade, results — no edge
  popping, HUD proportions correct. Left: 2D menu backgrounds pillarboxed (textured full-screen blits — stretching
  distorts the art); the 1-row top-border 3D leak spans the wide width.
- Camera shutter bars: `hud.c` `hudDrawCamera`'s two closing `uvVtxRect` fills anchor L/R (`hud.c.patch`), and
  their width `x` (0/80/160) is scaled by `aspect·¾` so each reaches half the output width (240·aspect N64 px) —
  anchoring alone left the last frame open in the middle (bars 0..160 and W−160..W). Reticle + photo count stay
  centred. First version checked with `fly_hang_glider_photo.txt` (Z press) 21:9 + off; the width scaling
  (2026-09-29) still needs that run. Cannonball HUD + demo controller were already anchored (8d9ea40); the title-demo controller
  infographic (`control_info.c` `contInfoMainRender`) is a textured full-screen blit → same class as menu backgrounds.
- HUD edge anchoring (done): moving HUD coords in C can't work — the HUD scissor (0..320, `hudMainRender`'s
  viewport) crops at the 4:3 edges and a scissor left of x 0 is unencodable (unsigned 12-bit). So the renderer
  moves whole draws: `G_NOOP` w1 `ANCHOR_TAG` "PWA"+`L`/`C`/`R` sets `Interpreter::anchor` (reset per run) →
  `DrawCall::anchor`; `placement` shifts `Wide::Fixed` draws (transform + scissor) by ±(output − 4:3 width)/2.
  Off/4:3 → shift 0; `Extend`/`Stretch` ignore it. C (`pw64_widescreen.c`, only emits when widescreen is on):
  `pw64_hud_anchor_x(x)` at the top of each `hud.c` gauge helper (x < 80 → L, ≥ 200 → R, else C: speed/timer/
  throttle left, radar/sea level/altimeter right, fuel/photo/reticle/camera/messages centred); cannonball's
  inline gauges set L/C/R by hand. Text is queued (`uvFontPrintStr`) and drawn later in `uvFontGenDlist`, so
  `font.c` stores `pw64_hud_anchor` per message and re-emits it there. Anchor back to C after the HUD, so menus,
  briefing and results are untouched.

## Fill screen (`PW64_FILL_SCREEN`)
Option: remove the game's own black bars around world views (flight, demo, replay, title, class summary,
briefing, results, credits) by showing the 3D there too; HUD moves to the true top/bottom edges. Name it
`fill_view`/`fill_screen` everywhere — `Widescreen::Fill` already means "output = window aspect".

**How the letterbox is made today** (all N64 px; "rows" = top-down, hud/uvVtx y = bottom-up):
- Camera channel (`cameraInit`, camera.c): viewport = `SUBSCREEN` (uv_graphics.h `OVERSCAN_*`): x 10..310, rows
  8..222 → bars: 8 rows top, 18 bottom, 10 columns each side. Frustum ±0.4906542 × ±0.35 at near 1 (= 300/214).
- `uvGfxClipRect` (graphics.c) builds the RSP viewport over the view + 5 px guard: x 5..315, rows 3..227 →
  projection centre = column 160, **row 115**; NDC ±1 = ±155 / ±112 px. Rows 8..222 are symmetric about 115.
- `uvChan_80204FE4` (chan.c): `uvGfxViewport` (scissor = view), then `uvGfx_80222A98` (z clear + scissor 0..320 ×
  0..240; camera channels have flag 3, never 4). So world draws are bounded only by the RSP clip box: env/sky at
  clip ratio 1 (= viewport, rows 3..227), terrain/objects at ratio 2 (whole screen — the known top-row leak).
  `_uvEnvDraw`'s `uvGfxClearScreen` = FILL rect over the *view* (10..310 × rows 8..222).
- Bars = 4 untextured `uvVtxRect`s: `drawScreenBorder` (code_D2B10.c; callback `func_8034B6F8` of flight, title,
  results, class summary/briefing, post-pilot-select; credits.c also calls it after its scene); the same 4 rects
  inline in level_select.c `func_8030F448` (class summary map) and options.c `optionsDrawBorder` (2D art).
- CPU culling: `func_802061A0` (code_7150.c) far-plane corners from `unk1E8/1EC` (x0/x1) and `unk1F0/1F4` (y0
  bottom / y1 top) → planes `unk298`→`unk2E0` (dobj, sobj, fx, code_30EA0, terrain) + terrain footprint
  `unk208`→`unk250`→`unk328`. Corners = the guard viewport (±155 × ±112 px).
- Renderer `vi_border` blacks row 0 (+ column 319 at 4:3) at present: nothing ever draws there.

**Spike (reverted, led to the design below):** bars off in `drawScreenBorder`, y-cull ×1.14, Extend draws whose clip box
spans the view vertically scissored to the full output height, untextured fills spanning the view stretched
vertically, `vi_border` top row off. `fly_hang_glider.txt` 16:9 frames 1050 (title), 1450 (class summary),
1550 (briefing), 2300/2700 (flight): seamless sky at the top and terrain at the bottom, no holes, no popping,
HUD unchanged. The mechanism works; everything below is plumbing + HUD.

**Approach** (same idea as Hor+: projection unchanged, the letterboxed image stays pixel-identical, we only stop
covering/cropping the bar rows):
1. C: when fill is on and a world view was drawn this frame, skip the bars (all four sides).
2. C: widen culling vertically (and horizontally at 4:3) so nothing pops at the new edges.
3. Renderer: world-view `Extend` draws whose clip box spans the view vertically get the full output height as
   scissor (the sky's ratio-1 box ends at rows 3/227); untextured fills spanning the view both ways (env clear,
   crash fade = hud.c `hudDrawBox` SUBSCREEN quad, `hudDrawSkyDiving` cloud fade, replay_screen.c fades at
   SUBSCREEN±1) are stretched vertically onto the output height, like `Stretch` does horizontally.
4. HUD vertical anchor Top/Middle/Bottom, analogous to `ANCHOR_TAG`.
5. `vi_border` skips row 0 (and column 319) on framebuffers whose last non-empty task drew a filled world view.

**FOV math.** Tan per px: y 0.35/112, x 0.4906542/155 (the game's pixels are 1.3 % non-square; unchanged).
- Vertical, letterbox: rows 8..222 = ±107 px → tan 0.3344 → **37.0°**. Fill: rows 0..240 = 115 up / 125 down
  → tan 0.3594 / 0.3906 → 19.8° + 21.3° = **41.1°**, off-axis (the centre row stays 115; a lens shift, correct
  perspective). Not recentred: moving the projection to row 120 would shift the 4:3 image against the HUD/
  reticle (code_7CF30.c computes the gyro reticleY from 107 px = the view's half height) and lose identity.
- Horizontal (Vert+ on top of Hor+ = hFOV unchanged): 4:3 letterbox ±150 px → 50.8°; 4:3 + fill ±160 → 53.7°;
  16:9 ±213.3 → 68.1°; 21:9 ±280 → 83.1° (with or without fill).
- Cull factor y: guard viewport of the channel (as `uvGfxClipRect`): g0 = max(viewY0−5, 0), g1 = min(viewY1+5,
  239), a = g1−g0, half = a/2, centre row ct = 240 − g0 − (a>>1) → `k_y = max(ct, 240−ct)/half × 1.02` (≥ 1).
  Flight: max(115,125)/112 × 1.02 = **1.138**. Apply the same k_y to both `temp_ft4` and `temp_ft5` (max of
  up/down: no sign mix-up between y0/y1, negligible extra drawing). x at 4:3 + fill: the existing
  `pw64_widescreen_xscale` formula with aspect 4/3 (= 1.088).

**What stays letterboxed / untouched, and why**
- 2D screens: options (art + inline bars), file menu, pilot select (its glider channel has env 0xFFFF and no
  terrain → not a world view, no tag; bars not drawn during selection anyway), photo album. The C flag is only
  set by a world view, so their bars stay; the renderer only extends tagged world draws.
- Photos: **not** taken from the framebuffer. snap.c stores the camera pose and re-renders each photo with the
  frustum × 0.45 into album viewports (`func_8033A610`: single photo 63..252 × rows 47..186, 80×60 grid cells)
  → not the main view, so neither Hor+ nor fill touch them; they keep the original framing. Scoring
  (`func_8034B354` → 320×240 from *NDC*; snap.c ~330 bounds 0..320/0..240, ~560 centre 25–75 %) is
  projection-relative, independent of bars/Hor+. Keep it that way: a wider photo would show things scoring
  never considered.
- Per-object projections (dobj `unk34 & 2`) stay 4:3 (as Hor+). Snow (snow.c: fb indexes, x ±150, z −102..112
  around row 120) stays inside the old view (possible follow-up: snow.c.patch z bounds −119..120 with fill).
- Game logic: searched `viewY0/1`, `gGfxViewY*`, `SUBSCREEN_*`, `clipY*`, `camera->aspect` (written, never read):
  only clears/fades/bars, culling, snow, the gyro reticle (projection-based), camera near-probe
  (`camera_802D4514`, collision), photo scoring (projection-based). Nothing reads what the bars hide. Low risk.

**Interactions**
- Default: `fill_screen` = on iff widescreen is on (config key absent). Widescreen off + fill on = 4:3 with no
  bars on any side (x culling 4/3, side bars dropped, `Extend`/`Stretch` placement also at 4:3). Outputs
  taller than 4:3 (`Widescreen::Fill` in a portrait window): out of scope, unchanged.
- MSAA / `PW64_SCALE`: nothing special (placement is in target px from the output rect).
- Camera shutter (`hudDrawCamera`): `uvVtxRect(.., SCREEN_HEIGHT-1, ..)` covers rows 1..239 → with fill row 0
  would show the world: pass `SCREEN_HEIGHT` when fill is on. Crash/cloud/replay fades: vertical Stretch.
- Demo/attract, replay, cutscene cams: all use the camera channel → filled. Title (env 0x17, own frustum ±0.70 ×
  ±0.5, same aspect): filled, 2D logo/characters stay 4:3 (their bottom edge now shows world under it — fine).
- Results (test_summary: env 0x17 + model): filled; its sprites (y 190/220) stay centred, unanchored.
- Photo album `uvGfxClearScreen` (viewport 5..314 × 5..234) will stretch vertically too (gray to the edge instead
  of a 5 px black rim) — cosmetic, same class as the existing horizontal stretch.

**C design** (`pw64_widescreen.c`)
- Globals: `int pw64_fill_screen` (set by Rust before boot), `int pw64_fill_frame` (1 once a filled world view
  was drawn this frame; reset at the top of `uvGfxBegin`, graphics.c.patch).
- `pw64_world_view(chan)`: terrain (`unk0 & 2`) or env (`unk2 != 0xFFFF`), viewX0 ≤ 16 && viewX1 ≥ 304.
  `pw64_fill_view(chan)` = fill on && world view && viewY0 ≤ 20 && viewY1 ≥ 220 (bottom-up; flight 18..232).
- `pw64_widescreen_xscale`: effective aspect = `pw64_widescreen_aspect > 0 ? it : (fill_view ? 4/3 : 0)`.
- `pw64_fill_yscale(chan)`: k_y above for a fill view, else 1.
- `pw64_widescreen_tag(chan, on)`: emit when `xscale > 1 || fill_view`; with on=1 and fill_view set
  `pw64_fill_frame = 1`. Bars: top/bottom drawn only if `!pw64_fill_frame`, sides only if also widescreen off.
- HUD: `pw64_hud_vanchor` ('T'/'M'/'B'), tag `"PWV"+c` = `0x50575600 | c` (no clash with `PWA`/`PWW`);
  emitted only when widescreen or fill is on. `pw64_hud_set_anchor(h)` also resets V to 'M';
  `pw64_hud_set_anchor2(h, v)` for by-hand spots; `pw64_hud_anchor_xy(x, y)` = H as today + V: y ≥ 160 → T,
  y < 100 → B, else M. Top: timer (27,222), radar (215,222). Bottom: speed/sea level/fuel (y 37), throttle (82),
  photo count (`hudDrawPhotoCount`, by hand C/B), demo controller (L/B), cannonball power gauge, heading bar,
  elevation gauge (by hand, B). Middle: altimeter (129), reticle, camera, messages. `font.c` stores both
  anchors per message (u16 `h | v<<8`) and re-emits both.

**Renderer design** (`pw64-gfx`)
- `RenderOptions::fill_view: bool` (default false). `Wide::MAIN_VIEW_Y = [10.0, 220.0]` (rows: a scissor/fill must
  reach at least this far up/down; view 8..222, replay fade 7..223, sky clip box 3..227).
- `DrawCall::stretch_y: [f32; 2]` (min/max vertex row, set for `Stretch`, else `[0, 0]`); consecutive `Stretch`
  draws merge only when `stretch_y` is equal (drawScreenBorder's top + bottom bars merged would span the view).
- `DrawCall::vanchor: VAnchor {Top, Middle (default), Bottom}` from `Interpreter::vanchor` (reset per run, part of
  the merge key). `VAnchor::rows()`: Top −8, Bottom +12 (the timer keeps its old gap to the edge ≈ 9 rows; the
  bottom row gets ≈ 8 rows instead of 20; tune visually).
- `placement(widescreen, fill, d, size)`: `wide_x = aspect > 4/3 + 1e-4 || fill` gates `main_view` (was
  aspect only). `spans_y(a, b) = fill && a ≤ MAIN_VIEW_Y[0] && b ≥ MAIN_VIEW_Y[1]`.
  `extend_y = Extend && main_view && spans_y(scissor[1], scissor[3])`; `stretch_y = Stretch && main_view &&
  spans_y(stretch_y) && b > a` → y mapping rows a..b → y0..y0+rh: `k = rh/(b−a)`, `screen[2] = −2k/th`,
  `screen[3] = 1 − 2(y0 − a·k)/th`. Either → scissor rows = (y0, y0+rh) (output area). `Fixed` with fill:
  `y0 += vanchor.rows() · rh/240` (transform + scissor, as the H anchor does with x0). Fill off ⇒ exactly today.
- fb.rs: `FbTarget::filled` = the last non-empty task had an `extend_y` draw (fb_copy copies it); `vi_border`
  blacks row 0 only if `!filled`, column 319 only if widescreen off && `!filled`.
- Identity: `tests/synth` `frame_hash_with` must strip `, stretch_y: [..]` and `, vanchor: Middle` from the draw
  `Debug` text (like `lodp`/`filt`), so `interp_identity` constants stay; never re-record them for this.

**Implementation notes:**
- Verification runs: `PW64_NO_AUDIO=1 PW64_NO_INPUT=1 PW64_NO_THROTTLE=1 PW64_FPS=60`, fresh save via
  `PW64_DATA_DIR=<empty dir>`, `PW64_INPUT_SCRIPT=crates/birdman64/scripts/<s>.txt`, dumps in `tmp/frame_<n>.png`.
- Placement tests (`widescreen_placement`): fill on, Extend with scissor [0,0,320,240] and [5,3,315,227] →
  full output height; Stretch x 10..310, y 8..222 → N64 rows 8/222 at target rows 0/th; Stretch y 1..8 (a bar)
  untouched; 4:3 + fill: Extend [5,3,315,227] → [0,0,w,h]; fill off: every older assert unchanged. Keep
  `extend` (x) and `extend_y` separate; the full-height scissor uses the unshifted y0.
- `Wide::MAIN_VIEW_Y = [10, 220]` rows; `RenderOptions::fill_view` (default false), `DrawCall::stretch_y`
  ([0,0] when not `Stretch`), `DrawCall::vanchor` (`VAnchor`, default `Middle`) as designed. `wide_class`
  returns `(Wide, stretch_y)`; consecutive Stretch draws merge only when `stretch_y` matches (the bars stay
  unmixed).
- `placement(widescreen, fill, d, size)` gates `main_view` on `aspect > 4/3 || fill` (the design's
  `wide_x`); `extend_y` = Extend + main_view + `spans_y(scissor rows)` opens the scissor to (y0, y0+rh) with
  the 4:3 mapping unchanged (Hor+ vertically: un-crop, not rescale; verified by `assert_eq!` of the screen
  transforms fill on vs off). `stretch_y` = Stretch + main_view + `spans_y(stretch_y)` maps rows a..b onto
  y0..y0+rh (`screen[2] = -2k/th`, `screen[3] = 1 - 2(y0 - a·k)/th`), scissor (y0, y0+rh). Both keep the
  unshifted y0.
- `VAnchor::rows()` = Top -8 / Bottom +12 (N64 rows, × rh/240); applied to `Wide::Fixed` draws with fill on
  only, transform + scissor together, like the H anchor. `interp::vanchor_tag` decodes "PWV"+T/M/B
  (0x5057_5600), reset per run, part of the merge key; PWA and PWV spaces are disjoint (tested both ways).
- `Renderer::extend_y(d, size)` is the single `extend_y` source for placement and `fb_draw` (fill_view +
  main_view + spans_y); `Target::filled` = the last non-empty task had such a draw; empty tasks keep it,
  copies carry it, non-extend tasks clear it. `vi_border(rect, right, filled)`: filled → both strips empty.
  Fill off never touches `filled` (spans_y is fill-gated), so nothing changes today.
- Identity: `tests/synth` `strip_placement` removes `, stretch_y: [..]` and `, vanchor: Middle` from the draw
  `Debug` text; `interp_identity`/`render_identity` constants pass unmodified (not re-recorded).
- A/B (fill off, 4:3, `fly_hang_glider.txt`, 4:3 dumps): old-build-twice noise floor on flight frame 2300 was
  21.6 % px (timer-driven HUD + ambient motion), 2700 was 70 %+; new build vs old is 19.2 % (2300) with the
  identical HUD text (TIME 00'11"66 in all three) and frames 1450/1550 (class summary, briefing) byte-identical
  0 px. Fill-off output equals the pre-fill build within noise; unit identity tests unmodified.
- Gotchas: a worktree A/B build needs `decomp` (`git submodule update --init` in the worktree) and the
  gitignored `tools/zig/zig.exe`; `PW64_INPUT_SCRIPT` must be set even for headless runs (else the game sits
  on the title screen and later frames are just the attract loop).
- C plumbing: `pw64_fill_screen` is exported on all three paths (`-export:pw64_fill_screen,DATA`, the Linux
  version script, dylib `sym`), else the dylib build fails to bind at runtime; set via
  `pw64_game::set_fill_screen` next to `set_widescreen_aspect`. `pw64_fill_frame = 0;` is the first line of
  `uvGfxBegin` (graphics.c.patch). Culling: code_7150.c.patch `temp_ft4/ft5 *= pw64_fill_yscale(arg0)`; bars
  gated in code_D2B10.c.patch and level_select.c.patch (`func_8030F448`'s 4 inline rects); shutter in
  hud.c.patch (`fill ? SCREEN_HEIGHT : SCREEN_HEIGHT - 1`).
- Options: `PW64_FILL_SCREEN=0|1` > `[graphics] fill_screen` (`Option<bool>`) > follows widescreen; settings
  row "Fill screen" (restart, like widescreen).
- Result (fill on, 16:9 and 4:3): title/class summary/flight have no bars or stale rows; no edge pop at the
  bottom between consecutive frames; menus/pilot select/briefing/options stay 4:3 with bars. Fill off renders
  the plain widescreen look.
- Shutter (photo script, 16:9): the bars close in from the true edges over the full-height view and the
  HUD gauges stay anchored; PHOTO count 6→5. The bars land ~6-8 retraces after Z release: `snapPhoto`
  first busy-waits (`while (uvClkGetSec(UV_CLKID_APP) < 0.1)`, 0.1 s = 6 retraces) with the framebuffer
  held, then the 3-frame countdown draws the bars.
- **Dense dump artifact (not a bug, explains the earlier "18 identical dumps 1798-1815"):** with
  `PW64_DUMP_FRAMES` listing every retrace of a window + `PW64_NO_THROTTLE`, all dumps in that window show
  the same stale image while the DL trace keeps changing — the GPU dump render + readback is slower than the
  fast-forwarded emulation, so the dumped image lags the latch. Spread dumps out (every ≥5 retraces) inside
  fast windows; the game state itself is fine.
- HUD C side: `pw64_hud_vanchor`/`PW64_VANCHOR_TAG`/`emit_vanchor`,
  `pw64_hud_set_anchor2` and `pw64_hud_anchor_xy` (V: y≥160→T, y<100→B, else M; x<80→L, ≥200→R, else C) in
  `pw64_widescreen.c`; by-hand anchors get V='B'. `set_anchor` also resets V='M' and emits both tags (V only
  when widescreen|fill). `font.c` stores h|v<<8 per message, emits both at `uvFontGenDlist`. Verified:
  glider 2300 16:9+4:3, rocket belt 2500 (fuel), gyrocopter 2500 (throttle), cannonball 2200 (`PW64_EEP`)
  (POW/heading/elevation gauges), photo-count text moved to the bottom row — all at the true output edges,
  nothing clipped; fill off keeps the old layout (V tags decode to a no-op without fill).
- Env vars must be re-set per PowerShell invocation (each call starts a fresh shell) — easy to lose
  `PW64_WIDESCREEN` between batches.
- **Fixed:** the `attract_demo.txt` crash (~retrace 2400, write of 0x8) was not wgpu: the RIP
  symbolizes (`llvm-symbolizer --obj=target/release/birdman64.exe <rip>`) to `demoInit` (demo.c). Every ROM `RHDR`
  block is 24 bytes but `RHDR` is 20: `_uvMediaCopy(&sDemoRecHeader, data, size)` overran into
  `sDemoRecording` (clang placed it right after) → NULL. demo.c.patch clamps the copy. It also explained
  headless runs crashing ~2050 retraces after a script ended (idle title → attract demo).
- Saving with Fill screen back at its default removes a saved `fill_screen` key
  (`SavedSettings::fill_screen: Option<Option<bool>>`); changing Widescreen moves a default-valued Fill screen
  along (Off → 4:3 doesn't save `fill_screen = true`).

**Identity check (fill off must equal today):** build the pre-change commit in a worktree (tmp/ab.sh pattern:
`git worktree add`, `PW64_ZIG=<repo>/tools/zig/zig.exe` in a fresh worktree), dump the same frames with the same
script and env from both builds, and compare PNGs pixel by pixel; run the old build twice first to learn the
run-to-run noise floor (native runs are not bit-deterministic) — the new build must stay within it. Unit
identity tests (`interp_identity`, `render_identity`, all existing placement asserts) must pass unmodified.

- **Sweep:** 9 runs, matrix in sweep.md ("Fill screen sweep"): fill on x {16:9, 4:3, 21:9}, finish script
  (crash cam, replay fade, results, album grid, single photo), skydiving (cloud fade), rocket belt 21:9,
  MSAA 4; fill off 16:9 + 4:3 control. No bars/stale rows in world views at any aspect; HUD at the true
  edges, nothing clipped; fades stretch full height; photos/album unchanged; fill off = the old look.

## OLED care (`RenderOptions::oled`)
- `Oled { brightness: f32, drift: [f32; 2] }`, default (1.0, [0, 0]) =
  output-bit-identical (×1.0 in the shader, [0,0] shift, `u.lod[1] = 0`).
- Tag: `interp::HUD_TAG_ON/OFF = 0x5057_4831/0x5057_4830` ("PWH1"/"PWH0",
  `G_NOOP` w1, `pw64_hud_tag` in the native game, emitted unconditionally);
  `Interpreter.hud` (reset per run) → `DrawCall::hud`, in the merge key.
  Tag space is disjoint from PWA/PWV (unit-tested).
- Drift: `placement(.., hud_shift)` moves `d.hud && d.wide == Wide::Fixed`
  draws only, `(shift * rw/320).round()` / `(shift * rh/240).round()` whole
  target px (fractional would shimmer text), transform + scissor together.
  `oled_drift(t)` = circle radius 2 N64 px, period 240 s.
- Dim: shader `dim()` multiplies rgb by `1.0 - u.lod.y` (alpha kept) in
  Fill/Copy/Normal returns; renderer sets `u.lod[1] = 1 - brightness` for
  hud draws (0 otherwise). DepthImage not dimmed (writes the z image).
- Identity: `tests/synth strip_placement` strips `, hud: false` like
  `stretch_y`/`vanchor`; `interp_identity`/`render_identity` unchanged.

## Texture packs (`PW64_TEX_PACKS=<dir>`, `crates/birdman64/src/packs.rs`)
- Key-named files: `<dir>/<16-hex-key>.png` (any case) — the keys are the F3D TMEM content hashes `PW64_DUMP_TEX`
  writes (FNV-1a of format, size, covered TMEM bytes + TLUT per bound tile, "Textures" above). Any PNG colour
  type (expanded to RGBA8), ≤ 8192 per side (renderer also drops mips past the device limit); UVs are normalized.
- The directory is indexed once at startup (key → path), so misses never touch the file system; the PNG is
  decoded when the key is first bound (`TextureCache` calls the replacer once per key and keeps the image — no
  second cache in packs.rs). Decode happens on the game thread mid-frame: a big pack PNG can hitch that frame.
- **RT64-style packs:** `rt64.json` (the documented RT64 database file; `texture.json` alias) entries
  `textures[].hashes.{rice,rt64}` + `path` (relative, `/` or `\`, extension optional, `.dds` read as `.png`;
  absolute paths and `..` are rejected) name files; a hash without a json path resolves to `<dir>/<hash>.png`
  (RT64's auto path; case-insensitive), which also covers json-less Rice-named folders. Always indexed together
  with the key-named files (an RT64 hash colliding with one of our keys is a 2⁻⁶⁴ event); a broken json is
  reported and ignored.
- **Hash compatibility: none, by design.** Our keys are FNV-1a content hashes of the decoded TMEM tile; RT64
  hashes raw TMEM with XXH3; Rice hashes read RDRAM around the texture (RT64 can't even produce Rice hashes at
  runtime). The key spaces do not correspond, so an RT64 pack does nothing on its own. The interop path is an
  optional bridge file `<pack>/pw64_keys.csv`, one row per texture: `rt64_hash,pw64_hash` (RT64 or Rice name, then
  our key as `PW64_DUMP_TEX` names it); `#` comments, blank lines, an optional header and extra columns are
  skipped, the first row per key wins, and a bridged row overrides a key-named file. Honest summary: *RT64-style folder layout accepted; hash keys are ours unless `rt64.json` +
  `pw64_keys.csv` provide a mapping.*
- RT64 metadata not honored (reasons): `operation`/`operationFilters` (stream/preload/stall — our loading is
  synchronous per frame, so there is no streaming/pop-in to avoid); `shift`/`shiftFilters` (would need a
  per-texture half-texel channel through the replacer hook `Fn(u64, &Image) -> Option<Image>`, which has none);
  `configuration.autoPath` (the fallback lookup covers it). The json carries **no** per-texture size/format hints
  at all, so none needed mapping. DDS is not supported (PNG only — what RT64 accepts during development).
- JSON via `serde_json::Value` (indexing never panics on odd shapes). Tests are synthetic (`packs::tests`: key
  names, colour types, bridged lookup incl. auto path, path traversal, broken json, csv leniency); on Linux they run
  by compiling packs.rs into a scratch crate with `#[path]` (pw64 itself doesn't link there). Not yet verified
  against a real RT64 pack (none exists for PW64).
