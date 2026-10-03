# Notes: N64 graphics (GBI / RDP)

## GBI texture commands (decoded in `crates/pw64-formats/src/gbi.rs`)
- PW64 uses the F3D-family GBI (not F3DEX2): `G_ENDDL B8`, `G_SETOTHERMODE_L B9`, `_H BA`, `G_TEXTURE BB`.
- RDP: `E6` LOADSYNC, `E7` PIPESYNC, `E8` TILESYNC, `F0` LOADTLUT, `F2` SETTILESIZE, `F3` LOADBLOCK, `F4` LOADTILE,
  `F5` SETTILE, `FA` SETPRIMCOLOR, `FB` SETENVCOLOR, `FC` SETCOMBINE, `FD` SETTIMG.
- UVTX lists use only: BB FA FB BA FC E6 E8 FD F5 F3 F2 B8. Typical: `BB` (tile, mip levels-1), `BA`s (2-cycle, persp,
  LOD, bilerp, detail/sharpen), `FD` (16b "LoadBlock size" for 4/8-bit data), `F5` tile 7, `F3`, then `F5`+`F2` per mip level.
- Field gotchas: `SETTILE` line/tmem are in 64-bit words; `SETTILESIZE`/`LOADTILE` coords are 10.2 fixed;
  `LOADBLOCK` uls/ult are integer texels, lrs = texel count − 1 (in SETTIMG's size), dxt is 1.11 (0 = data pre-swizzled);
  `LOADTLUT` count−1 sits at w1 bit 14. `G_TEXTURE` w0: level bits 11–13, tile 8–10, on 0–7.
- TMEM model (`tmem.rs`): odd rows swap 32-bit halves (`addr ^ 4`) on load and sample; RGBA32 splits RG/BA across the
  two 2 KiB banks; TLUT entries are quadrupled, entry i at `0x800 + i*8`; TLUT type = othermode-H bits 14–15.
- With mip-mapping, tiles `render_tile..render_tile+levels` are the mip chain. Two-image textures put the other image
  on the `gSPTexture` tile and this texture's chain on the following tiles (2-cycle TEXEL0/TEXEL1).

## GBI geometry (F3D, `gbi.rs` + `uvmd::Executor`)
- Plain F3D (not F3DEX): 16-entry vertex cache, no G_TRI2. `G_VTX 04`: w0 = `((n-1)<<4|v0)<<16 | n*16`, w1 = addr.
  `G_TRI1 BF`: w1 = `flag<<24 | v0*10<<16 | v1*10<<8 | v2*10`. `G_MTX 01`, `G_DL 06`, `G_POPMTX BD`,
  `G_SETGEOMETRYMODE B7` / `G_CLEARGEOMETRYMODE B6` (F3D bit values differ from F3DEX2: smooth 0x200, cull front/back 0x1000/0x2000).
- `Vtx`: s16 xyz, u16 flag, s16 st (s10.5 texels), u8 rgba (or s8 normal xyz + alpha when G_LIGHTING).
- Render state → GBI is `uvGfxStateDraw` (`graphics.c`): state bits → geometry mode, texture id → the UVTX's own list,
  (DECAL|XLU|AA|ZBUFFER) → render mode (XLU picks SURF vs TEX_TERR by UVTX flag 0x8000 / 1 channel). LIGHTING adds
  `G_TEXTURE_GEN` + `gSPTexture(0x7C0,0x7C0)` + `G_CC_DECALRGB` (env map).

## Fast3D command encodings (verified against gbi.h's non-F3DEX branches; used by `pw64-gfx`)
- `G_MOVEWORD BC`: w0 = `offset << 8 | index` (gsImmp21; F3DEX2 differs). Index: NUMLIGHT 2 (data = `(n+1)*32 + 0x80000000`),
  SEGMENT 6 (offset = seg*4), FOG 8 (data = `fm << 16 | fo`, `fm = 128000/(max-min)`, `fo = (500-min)*256/(max-min)`;
  shade alpha = clamp(z/w * fm + fo) / 255), LIGHTCOL 0x0A (offset = n*0x20).
- `G_MOVEMEM 03`: w0 = `index << 16 | size` (gsDma1p). VIEWPORT 0x80 (Vp: s16 vscale[4], vtrans[4], xy in ¼ px),
  LOOKATY 0x82, LOOKATX 0x84, L0..L7 0x86..0x94 (step 2). `G_MTX`/`G_DL` params also sit at w0 bits 16..23.
- Matrix stack (PW64 runs `gspFast3D`, graphics.c; ref: sm64 decomp `rsp/fast3d.s` `imm_POPMTX` / G_MTX push):
  the current modelview lives in DMEM; `G_MTX_PUSH` DMAs it to `dram_stack` and the end is hard-coded at
  `dram_stack + 0x280` (10 saved, not the task's 1024-byte `dram_stack_size`) → 11 levels incl. current. Full: push
  skipped silently, LOAD/MUL still applied (next pop restores one level too high). `G_POPMTX` on empty: no-op.
  PW64 keeps 1 level pushed under the world (camera `PUSH|LOAD`), and the demo-pilot models Kiwi/Ibis
  (UVMD 0x15D/0x15E; seen in the attract-mode pilot intro, probably also pilot select) nest 10 parts → 11 pushes: one genuine overflow per leaf, also on HW.
- `G_TEXRECT E4` is 3 commands: E4 (`lrx<<12|lry`, `tile<<24|ulx<<12|uly`, 10.2), `G_RDPHALF_1 B4` (s, t s10.5),
  `G_RDPHALF_2 B3` (dsdx, dtdy s5.10; copy mode dsdx ×4). `G_FILLRECT F6`: w0 = lr, w1 = ul (10.2); fill/copy include the lr edge.
- `G_SETFILLCOLOR` on a 16-bit color image = two packed RGBA5551. The game clears the z-buffer by pointing the color
  image at it and filling `GPACK_RGBA5551(255,255,240,0)` (`uvGfx_80222A98`).
- Engine render path: always 2-cycle; cycle 1 of the render mode = `G_RM_FOG_SHADE_A` or `G_RM_PASS`; cycle 2 from
  `uvGfxStateDraw`'s switch on DECAL|XLU|AA|ZBUFFER. Most common UVTX combiner `0xFC26A004 1F1093FF` =
  (T1−T0)·LOD_FRAC+T0, then ·SHADE (mip lerp). UVTX lists also set filter (`G_TF_*`, 32 of 463 point-sampled),
  `G_TL_LOD`, detail/sharpen (`G_TD_*`).
- Z-buffer word (16 bit) = `z14 << 2 | dz`; z14 = 3-bit exponent + 11-bit mantissa, 18-bit z = `mant << shift[e] +
  offset[e]` (shift 6,5,4,3,2,1,0,0; offset 0,0x20000,0x30000,0x38000,0x3C000,0x3E000,0x3F000,0x3F800). 0xFFFC = far,
  0 = near. Drawing with `cimg == zimg` writes the blender's RGBA5551 word as z (`uvGfxStateDrawDL` shadow volumes).
- `G_MOVEWORD` CLIP (index 4): `gSPClipRatio` = 4 words at offsets 4/0xC/0x14/0x1C (±x, ±y); triangles are clipped to
  ratio × viewport, the scissor crops the rest. PW64 sets ratio 1 early in a frame and 2 later (seen in menu and flight lists).
- GBI coverage sweep (`pw64-viewer` headless, `Interpreter::unknown_opcodes()`, Sept 2026): terras 0/1/2/4 with
  `--env 2` and terra 0 `--setup 5` hit **no** unimplemented opcodes — the scene lists use only the commands
  `pw64-gfx` implements. Untested GBI surface (menus/HUD are covered by the native-game sweeps instead).

## GBI coverage audit (2026-09-28)
Method: full per-command traces (`PW64_DUMP_DL=1`) of native runs — intro flyover (frame 300), title screen
(900), flight (1700, 2775) — plus two full-run log greps (attract 950 retraces, glider 2755): **zero
"unimplemented/unhandled" warnings**. 32 distinct opcodes across 38.5 k traced commands; all in the
`pw64-formats::gbi::op` table (45 names) and all handled by `pw64-gfx`. Frequencies (4 frames, cmds/frame):

| group | opcodes (rough share of cmds) | status |
|---|---|---|
| RSP core | `TRI1` 36 %, `VTX` 8 %, `MTX`+`POPMTX` 2 %, `DL`/`ENDDL` 9 %, `MOVEMEM` (VIEWPORT only), `MOVEWORD` (SEGMENT, CLIP ×4, FOG; no NUMLIGHT/LIGHTCOL in these frames — lighting path exercised via viewer states) | full |
| RDP state | `SETTILE`/`SETTILESIZE`/`SETTIMG`/`LOADBLOCK` 26 %, `SETCOMBINE` 2 %, `SETPRIMCOLOR`, `SETFOGCOLOR`, `SETFILLCOLOR`, `SETBLENDCOLOR` (title only, alpha-threshold ref), `SETSCISSOR`, `SETCIMG`/`SETZIMG` | full |
| othermode | `SETOTHERMODE_H`/`_L` 12 % | full (approximations below) |
| sync/fixup | `RDPPIPESYNC`/`LOADSYNC`/`TILESYNC`/`FULLSYNC`, `TEXRECT`+`RDPHALF_1` (contiguous pairs; the stray 1/frame `RDPHALF_1` 0xB4 = `gSPPerspNormalize` from `chan.c` — RSP-side, correctly no-op), `FILLRECT` | full |

Never emitted by the game's lists: `LOADTLUT`, `LOADTILE`, `TEXRECTFLIP`, `LINE3D`, `CULLDL`, `SETPRIMDEPTH`,
`RDPSETOTHERMODE`, `SETENVCOLOR` (viewer-only), chroma-key/YUV (`0xEA..EC`). No CI/TLUT textures exist.

Othermode-H surface actually used: cycle types {1, 2, fill/copy}; filter `G_TF_BILERP` (32/463 UVTX
point-sample); `G_MDSFT_TEXTLOD/TEXTDETAIL/TEXTSHARPEN/TEXTLUT` all 0 → **no LOD/detail/sharpen/chroma-key/
YUV mode is ever enabled**, and RGB dither = `G_CD_NOISE` on most draws.

Combiner: 13 distinct muxes/4 frames; the generic mux→WGSL generator covers every selector used (no chroma
CENTER/SCALE). Top: `0x26a004_1f1093ff` `(T1−T0)·LOD_FRAC+T0 → ·SHADE` (~65 % of flight draws), plain
`SHADE`/`T0·SHADE` fog modes, title XLU. Blender: 5 distinct (pre,last) cycles — all `G_RM_FOG_SHADE_A` pre +
OPA/XLU/TEX_TERR last cycles → Opaque/Alpha/coverage approximations.

Top gaps by draw frequency (fidelity, not errors — output verified visually):
1. ~~**LOD_FRACTION = 0**~~ fixed 2026-09-29: per-pixel RDP LOD fraction (renderer.md "LOD fraction + 3-point
   filter"). Was: (`interp.rs` `lod: [prim_lod,0,0,0]`): the dominant mip-lerp combiner always picks
   TEXEL0 → the game's own mip levels (TEXEL1) are never lerped in. Minification is still filtered: TEXEL0 is
   sampled with the GPU mip chain `renderer.rs` `mip_chain` builds (trilinear for bilinear tiles), so the
   difference vs HW is the mip *content* (box-filtered level 0 vs the artist's levels), not aliasing.
2. **3-point filtering / sampling** (3-point now optional: `PW64_FILTER=n64`): GPU bilinear instead of N64 3-point; 3D triangle S/T still sampled at
   pixel centers (texrects RDP-accurate) → sub-pixel texture wobble on every textured draw.
3. **Coverage/AA**: no per-pixel coverage; `CVG_X_ALPHA` → discard thresholds (CoverageBlend/Edge),
   `ALPHA_CVG_SEL` → 1.0 shortcut → thin alpha-edged strips (canopy fringes), MSAA ≠ N64 edge AA.
4. **Dithering** (`G_CD_NOISE` requested) unemulated → 16-bit banding in gradients (sky bands).
5. `LOOKATX/Y` MOVEMEMs ignored → `G_TEXTURE_GEN` env reflection is spherical-normal, not lookat (glossy
   model highlights drift; few draws). `G_CULLDL` would be a no-op anyway (CPU culls).

Nothing "decoded but rendered incorrectly" remains: the old texture-fetch and black-polygon bugs are fixed,
texrect bilinear is RDP-accurate, z-image shadow volumes verified (renderer.md). Static scene lists:
a435a7e's sweep already confirmed no unimplemented opcodes in terras 0/1/2/4/7.

## UVBT blits / `uvSprt*` sprites (2026-09-29, "Top Score" investigation)
- Blit 0x0D ("Top Score", `1182_UVBT_013`): header fmt=3 (IA), depth 8, width 39, stride 40, height 10, tile
  40×102. `_uvParseUVBT`'s `texelHeight` (tile height) flows into the sprite lib's `bmheight`, so `spDraw` loads a
  40×**102** IA8 tile (LOADBLOCK lrs = 2039 texels; garbage past row 10) while the texrect draws only 39×10 —
  the garbage TMEM rows are never sampled (st_clamp), but they're why the decoded texture dump is 40×102.
- The blit DL (`spDraw`/`drawbitmap`): SETTIMG IA 8b (width field 0 → 1), load tile 7, LOADBLOCK (dxt 0), render
  tile IA8 line 5 (40 bytes/row), SETTILESIZE (0,0)..(39, 101), TEXRECT (239,25)..(278,35) with dsdx = dtdy = 1
  (non-FASTCOPY). RGBA16 blits use FASTCOPY: dsdx = isx·4 with COPY cycle `/4` → also 1.0 texel/px (`sprite.c`
  `g->dsdx = sx * 4`).
- `uvSprtSetBlit` attrs per bmfmt: IA → `SP_TEXSHUF|SP_CUTOUT` (pre-swizzled load, 1-bit cutout via
  alpha compare vs blend alpha 0x01), no FASTCOPY (that's RGBA only).
- Ia8 blits draw with MODULATEIA over a background whose texels are I=255, A=0 ("white invisible"): any linear
  cross-texel blending smears that white into glyph edges — the renderer must point-sample these 1:1 rects
  (renderer.md); fonts survive the same filter because their background is black-transparent.
