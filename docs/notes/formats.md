# Notes: ROM & asset formats

Hard-won facts only. Keep entries short; link code instead of repeating it.

## ROM / filesystem (`crates/pw64-rom`)
- US ROM only; normalized to z64 (big-endian) on load. Magic: z64 `80 37 12 40`, v64 `37 80 40 12`, n64 `40 12 37 80`.
- `UVRM` index FORM at ROM `0x0DE720`; its `TABL` block is (tag u32, size u32) pairs.
  Files start at `0x0DF5B0`; offsets accumulate sizes, **including tag-0 entries** (skip them but still advance).
- File = IFF `FORM`: `FORM`, len (excl. 8-byte header), tag, then blocks (tag, size, data) from +0xC.
  `PAD ` blocks exist. `GZIP` block data = inner tag u32, decompressed size u32, MIO0 stream.
- MIO0: backref `len = (v>>12)+3`, `dist = (v&0xFFF)+1`; references can overlap (copy byte by byte).
- 1272 files. Counts: UVTX 463, UVMD 363, UVAN 115, UVBT 102, UVCT 101, UVTP 1, UPWT 61, PDAT 25,
  3VUE 12, UVFT 9, SPTH 8, UPWL 4, and 1 each of ADAT UVEN UVLT UVLV UVSQ UVSX UVSY UVTR.
  The UVSY `COMM` block's counts match for UVMD/UVCT/UVTX/UVAN/UVFT; its UVEN/UVTR/UVLV numbers count sub-items inside single files.
- Music ctl/tbl and sequences are **not** in the UV filesystem; the SFX bank is (`UVSX` `.CTL`/`.TBL`). See `audio.md`.

## Engine parsing
- The engine's parsers live in `decomp/src/kernel/texture.c` (`_uvParseUVMD`, `_uvExpandTexture`, …).
  Scalars are read via `uvMemRead`/`uvConsumeBytes`, so on native builds this is the endianness choke point.
- Format docs: `decomp/docs/pilotwings64_filesystem.md`, `decomp/docs/pilotwings64_imhex.hexpat`. Struct layouts: `decomp/include/kernel/*.h`.

## UVTX textures (`crates/pw64-formats/src/uvtx.rs`)
- One MIO0 `COMM` block (plus `PAD `): `size` u16, `gfx_count` u16, 2× scroll (f32 s,t; textures/sec, `uvSprt_802301A4`),
  image\[size\], Gfx\[gfx_count\], 22-byte trailer, then 6 zero bytes. Field table in the module doc. Mirrors `_uvExpandTexture`.
- Image data is **already in TMEM order** (odd rows 32-bit-word swapped); most lists `LoadBlock` with `dxt = 0`.
  Decode by emulating TMEM (`tmem.rs`), not by reading rows linearly. `size` ≤ 0x1000 always.
- `SetTImg` addrs are offsets: 1st → own image, later ones → image of texture `unk14` (13 files use a 2nd image).
  In those, `gSPTexture`'s tile is often the *other* image (tile 0) and the own image is tile 1+.
- Trailer: width, height, bpp (4/8/16), wrap S/T (0 clamp, 1 wrap, 2 mirror; verified vs tiles), state (low 12 = own
  index, always; high 4 = render flags, `0x8000` → XLU surf), image2 id (0xFFF none), unk20, channels (1/2/4), 4 color bytes, f32.
- Some tiles have no `SetTileSize` (2D/sprite textures): size = header width/height.
- Formats (own image): RGBA16 249, I4 96, IA8 56, I8 20, IA4 20, IA16 9 (+13 two-image). **No CI, no RGBA32, no TLUTs.**
  Mip levels: 1 ×148, 6 ×285 (others 3–5). Non-power-of-2 heights are common (e.g. 32×47).
- A few 2D UI textures (e.g. 318, 322) are stored upside down; the game flips via UVs. PNG export keeps TMEM row 0 on top.
- I/IA semantics: I formats replicate intensity into alpha (hardware behaviour), so I4/I8 PNGs are translucent.

## UVMD models (`crates/pw64-formats/src/uvmd.rs`)
- One MIO0 `COMM` block; byte layout table in the module doc. Mirrors `_uvParseUVMD`. Ends with 2–6 zero pad bytes.
- Geometry is a compact stream, not Gfx: u16 `c`; `c & 0x4000` → tri (3× 4-bit cache indices), else
  `gSPVertex(&vtx[c & 0x3FFF], (b>>4)+1, b&15)` with an extra u8 `b`. The engine expands it to F3D at load.
  All 363 files: only G_VTX/G_TRI1 (+ appended G_ENDDL). Texture/render state comes from the state word, not the list.
- State word (`uvGfxState_t.state`): low 12 bits = texture id (0xFFF/0xFFE none), high bits `GFX_STATE_*`
  (`uv_graphics.h`). `GFX_STATE_LIGHTING` never set in ROM models (0 of 4590 states); 495 XLU, 1787 untextured.
- Parts: one Mtx4F per part index, shared by all LODs; part matrices are parent-relative. `unk6` = hierarchy depth
  (parent = nearest earlier part with smaller depth; draw code pushes per part and pops `d[i]-d[i+1]+1`).
  Lower LODs may flatten depths (e.g. model 9: `[0,1]` → `[0,0]`). Model 207 starts at depth 1.
- Part 0's matrix is identity except model 207; in-game it is replaced by the object's position (`uvDobjPosm`),
  which also scales by `1/unk20`. So vertices are in model units, world = model / `unk20` (10 ×215, 100 ×128, 1 ×12).
- Space is Z-up (verified visually: pilot, Ferris wheel, carousel, Empire State building).
- Counts: 744 LODs (208 models with >1), 27 billboard LODs, 1913 parts, 81 949 vertices, 73 633 tris,
  `triCount` always matches the list. 830 `UnkUVMD_24` volumes (collision; per-part, cumulative item ends).
- Model UVs: vertex s10.5 × `gSPTexture` scale (from the UVTX list) × tile shift − tile uls, ÷ tile size (`uvtx::tile_uv`).

## Shared parsing (`pw64-formats`: `reader.rs`, `uvmd::read_state`)
- All `_uvParse*` read sequentially with `uvConsumeBytes`: **no alignment padding** between fields (e.g. UVTR's u8,u8 is
  followed directly by an f32). `reader::Reader` mirrors that; `expect_padding` checks the ≤7 trailing zero bytes.
- UVCT and UVMD share the render-state record + compact geometry stream (`uvmd::read_state`, `uvmd::triangles`).
- Placement matrices are libultra fixed-point `Mtx` (16 s16 integer halves, then 16 u16 fractions; `mtx_fixed_to_f32`).

## UVTR terras (`uvtr.rs`)
- One file, **one `COMM` per terra** (10); terra id = COMM index (`uvFile_80224170` picks the n-th). Layout in module doc.
- Grid is row-major from the box min (`col + cols*row`); cell `Mtx4F` translation = cell centre. The cell's u16 is a
  **global** UVCT index (`gLevelData.contours` is indexed by global id). Rotation byte = quarter turns (culling only;
  0 in all ROM cells).
- Terras: 0 Holiday Island (2×2×600), 1 Crescent Island (8×8×512), 2 = 2 cells of the same grid, 3 Little States
  (8×5×1000, 38 cells), 4–6 subsets/variants of 3's grid (own UVCTs 58–85 overlap 3's range), 7 Ever-Frost (2×5×1000),
  8 = 1 cell of 7's grid, 9 = one 6000² flat tile. 120 cells, 193 empty; every UVCT is used by some terra.
- World units = contour units (`unk1608` = UVSY f32 = 1.0). Space is Z-up like models.

## UVCT contours (`uvct.rs`)
- Layout in module doc. Counts over all 101: 29 768 vtx, 2127 states, 20 846 tris (= collision tris; state
  `tri_count` always matches), 1364 placements, 86 untextured states. Opcodes: only G_VTX/G_TRI1/G_ENDDL.
- Collision tris (`Unk80225FBC_0x28_UnkC`: 3 vtx indices + sector mask) — each draw state owns a contiguous range and
  the ranges tile the table in order.
- 16-bit "sector" masks on states, collision tris and placements: `_uvTerraDraw` computes which of 16 angular sectors
  around the tile are visible and skips the rest.
- Placements (`UnkSobjDraw`): one fixed `Mtx` **per model part** (count always = the UVMD's part count), used instead of
  the model's own part matrices; part 0 = tile-relative placement × `1/uvmd.scale` (verified all 1364), and its
  translation = the placement's xyz. Drawn with LOD 0..n by distance (`uvSobjGetLODIndex`) inside the tile's matrix.
- Level texture palettes (`UVTP`, `uvMemLoadPal`) can remap texture ids per level; the terrain export ignores this.

## UVAN animations (`uvan.rs`)
- Blocks: `COMM` (frames, step, UVMD id) + one `PART` per animated part (rotation keys only, quaternion xyzw + frame).
  115 files, 1604 tracks, 5943 keys; all quaternion-format, unit length, sorted, within first..=last; parts < model parts.
- `uvMat4SetQuaternionRotation` in row-vector convention = rotation by the conjugate: glTF rotation is `[-x,-y,-z,w]`.
- Playback (`uvJanimPoseLine`): the caller passes **normalized progress** in [0,1); it scales it by
  `frame_count() - 1` frames and lerps between the keys at those frame positions (wrapping after the last key
  continues to the first). `first_frame` is subtracted from key frames. The `models` export assumes 30 fps
  (frame_count ticks per play) and converts animated part nodes to TRS (part matrices are rotation+translation).

## UVLV levels / UVEN environments / UVTP palettes (`uvlv.rs`, `uven.rs`, `uvtp.rs`)
- One file each; one `COMM` per item (id = COMM index). Counts match UVSY sub-item counts:
  UVLV 136, UVEN 24, UVTP 7 (UVTR 10). Verified with `pw64-extract --levels` (also range-checks every id).
- UVLV: ten u16-count lists (terras, lights, environments, models, contours, textures, sequences, animations,
  fonts, blits). Mirrors `_uvExpandTextureCpy`. A flight appends: the map's level (`MapId` = UVLV id),
  shared 0x1A/0xC/0xD/0x2E, vehicle+pilot levels, then `0x70 + env` (`env_802E1A80`).
- `EnvSetup::for_env` inverts `envGetCurrentId`: 20 flyable envs 2..=21 across the 4 maps, conditions 0..=5;
  `envLoadTerrainPal` gives UVTP palettes 0–5 to envs 6, 11, 17, 18–21.
- UVEN: model table (UVMD id + flags: 1 keep-zbuffer, 2 far 27000 projection, 4 fogged, 8 follow camera x/y)
  + `ParsedUVEN` (0x3C bytes): clear RGBA, fog RGBA, unused RGBA, f32 fog min/max, fog on, clear on.
  All flying envs: sky dome (flags 8) + sea model (flags 0). `_uvEnvDraw` draws them before the terrain;
  fog factor = min/max if enabled.
- UVTP: u16 pairs (slot → replacement UVTX id), first match wins, applied by `uvLevelAppend` when loading
  texture slots (image2 lookup is by slot too, so it remaps as well).

## UVFT fonts (`uvft.rs`)
- Blocks `STRG` (charset), `FRMT` (s32 fmt, s32 siz), `BITM` (libultra `Bitmap`s; `buf` = IMAG index), `IMAG` sheets
  (MIO0, linear texels, `width_img` wide). 9 fonts: IA4 ×5, RGBA16 ×4; glyph count always = charset length.

## UVBT blits (`uvbt.rs`)
- Uncompressed `COMM`: 7×u16 header (fmt, depth, width, stride, height, tile w, tile h) + texels stored **tile by tile**
  (each tile `tile_w × actual_h`). Tile widths may exceed the visible width (padding). 102 files: RGBA16, RGBA32,
  IA8, IA16 (no CI).
- **Odd rows are TMEM-swizzled** unless RGBA32: `uvSprtSetBlit` sets `SP_TEXSHUF` for every format but RGBA, and
  for RGBA only at 16 bit, so `spDraw` loads with `gDPLoadTextureBlockS` (`dxt = 0`): on every odd row of a tile the
  two 32-bit words of each 64-bit word are pre-swapped. RGBA32 loads via the swapping `gDPLoadTextureBlock` and is
  linear. `Uvbt::decode` undoes it (`swizzled()`/`linear_data()`); verified on all 102: 81 shuffled ones all got
  smoother (mean vertical diff), the 21 RGBA32 unchanged. All tile rows are whole 64-bit words.

## EEP save files (EEPROM backend in `crates/pw64-platform/src/headless.rs`)
- 0x800 bytes = 16 Kbit EEPROM. Contains two 0x100-byte `PilotwingsSaveFile`s (`decomp/src/app/save.c`), file 1 at
  offset 0x0, file 2 at 0x100. Each: `'P''W'` magic (save.c init writes `'p''w'`), bit-packed state from bit 0x10 —
  7-bit test points in vehicle-major order (classes 4 for mains 0–2, 3 for bonus 3–6, `taskGetTestCount` per slot),
  then 0x408 photo-flag bits (snap.c), checksum byte at 0xFF = sum(bytes 0..0xFE) & 0xFF.
- The engine reaches it via `uvFileWrite/uvFileRead` (`kernel/system.c`: offset multiple of 8, `offs+nbytes` ≤ 0x208)
  → `osEepromLongRead/Write` (address in 8-byte blocks). Every write flushes the **whole 0x800** to disk; the file is
  created zero-filled on first run. `PW64_NO_EEPROM` restores the "no EEPROM" probe failure.
- Path: `PW64_EEP`, else `pw64.eep` in the CWD (`*.eep` is gitignored). Contents are cached at first use per process.
- **Emulator import is drop-in**: Project64/mupen64plus `.eep` files are this exact raw layout (no header, no padding) —
  copy as `PW64_EEP=...` and Continue. 4 Kbit (0x200) files load as a zero-filled prefix; files >0x800 lose their tail
  (and are truncated on the first in-game write). Test: `headless::tests::eep_image_accepts_emulator_layout`.
- Verified end-to-end headless (real game): boot with a zero file → both magics written to disk; second boot leaves the
  file byte-identical (magic detected, nothing rewritten); the read direction is proven by the crafted `tmp/cannon.eep`
  (sweep.md "Cannonball bonus game": file select shows medals + Continue). Assumed: other emulators' `.eep` variants
  (only the 0x800/0x200 raw layouts are tested).
