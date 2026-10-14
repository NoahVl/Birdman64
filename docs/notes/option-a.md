# Option A: C-visible memory anywhere (no fixed low-4 GB window)

Goal: drop the fixed RDRAM window (0x80000000), fixed exe base (0xC0000000) and module
base (0xB0000000), so RDRAM, stacks and the image may live anywhere (macOS arm64,
Android). Game output must stay bit-identical to the fixed-window build at every step.

## Decisions

- **Keep 32-bit addresses ("vaddr") in the C; translate at the conversions.** Every
  int↔pointer conversion is already explicit (`PW64_U32`/`PW64_PTR`; the build has
  `-Werror` on int/pointer casts and `shorten-64-to-32`). These are `static inline`
  C (`pw64_u32_fast`/`pw64_ptr_fast`) that translate through one region (`pw64_vmap`,
  RDRAM when registered) and call Rust `pw64_to_vaddr`/`pw64_to_host` (pw64-platform
  `vaddr.rs`) only on a miss. No region registered = `pw64_vmap` covers all 2^32
  values at host 0 = identity, always hits = today's build.
- **Display lists: keep 8-byte `Gfx` with vaddr words; the HLE rebases.** Rejected
  widening to 16 bytes because: UVTX dlists are raw-copied from ROM and patched in
  place (`setimg.dram |= K0(base)`, texture.c 807-812) and would need a size-changing
  converter; buffers are sized in N64 `Gfx` counts; `pw64-gfx`, DL capture/bench and
  `interp_identity` all use the N64 format; and the vaddr layer is needed anyway for
  the allocator/filesystem. The gbi.h macros already store `PW64_U32(p)`, so dlists
  get vaddrs for free; `pw64-gfx::Memory` stays u32 (the game's `Memory` impl in
  hle.rs maps vaddr → host).
- **Allocator, memory map, filesystem, `gGfxFbPtrs`, `uvFileReadHeader`: no new
  types.** `_uvMemAlloc` already returns `void*` (`PW64_PTR(alignedStart)`); its
  bookkeeping (`D_802B892C`, `D_802B8830[]` block table, scratch `switch` on
  literals) stays u32 vaddr, exactly the N64 numbers. `dataInfo->address` stays a
  u32 vaddr-or-ROM-offset, read through `PW64_PTR`. `gGfxFbPtrs` are `u8*` filled from
  `PW64_PTR(literal)`.
- **Bit-31 "RAM vs ROM" test stays**, applied to `PW64_U32(p)`: every region's vaddr
  range has bit 31 set (`set_regions` asserts it), ROM offsets are < 64 MB, and
  physical values (`K0 - 0x80000000`) pass through untranslated.
- **Invariant for relocated builds: C-visible host blocks lie above 4 GB**
  (`set_regions` asserts). A value < 4 GB is then always a vaddr / ROM offset /
  physical address, so both conversions are idempotent on the other kind, and an
  unregistered host pointer reaching `PW64_U32` panics instead of truncating.
  Android/Linux: map with a high hint; Windows: `MEM_TOP_DOWN` or fixed high tries.
- **Regions** (vaddr → host): RDRAM 0x80000000 + 8 MB; coroutine stacks
  0x80800000..0xA0000000 (one block reserved up front, registered once; stays
  < 0xA0000000 so libultra's 29-bit `K0_TO_PHYS` keeps them); game image (static exe
  at 0xC0000000, dylib module at 0xB0000000). `set_regions` is once-only: register
  after `dylib::load`, before `bootproc`.
- **Co-existence:** the switch is runtime, in the same binary: no regions = old
  model. `PW64_RELOC=alias|strict` (S4/S5) selects relocation.
  Remove the fixed window/base code once strict mode passes the full sweep
  (sweep.md) on Windows + Linux and has shipped in one release.

## Inventory (2026-10, before S2)

Routed by the layer already (occurrences of `PW64_U32|PW64_PTR|K0` macros in the
patched tree): graphics.c 18, memory.c 17, gbi.h 15, geometry.c 12, reverb.c 10,
load.c 9, filesystem.c 7, texture.c 6, os_convert.h 6, system.c 4, bnkf.c 3,
audio_manager.c 3, toys.c 3, resample.c 2, env.c 2, R4300.h 2, pw64_arena.c 2,
1 each: save.c heapinit.c cseq.c sched.c font.c debug.c snow.c game.c
pw64_rom_structs.c pw64_audio_load.c. No per-site work unless strict mode trips.

**Hidden sites** (int *constant* → pointer: clang doesn't flag these; S2 done,
line numbers = decomp source):
- graphics.c 105-107 `gGfxFbPtrs`, `D_80299278` (3) — **done (S1 template)**.
- user_file.c 33 (1), sched.c 219 `osViSwapBuffer((void*)0x80100000)` (1),
  system.c 223/227 `bootproc` clear loops (4 literals).
- memory.c 41-42 `D_803805E0` / `kernel_TEXT_START` defines, 72/77/81 clear loops.
  The `D_802B8830[]` / `D_802B892C` / `osMemSize + 0x80000000` u32 literals stay.
- debug.c 626/646 (2); audio_manager.c 41 `gAudioHeapStart` (static initializer:
  `PW64_PTR` is not a constant, so it is assigned in `uvaManagerInit` before the first use).

**Rust consumers** (take or produce C addresses):
- pw64-platform: misc.rs 50 `osVirtualToPhysical` (`as u32`); pi.rs 265 truncates the
  DMA dest to 32 bits, pi.rs 322 mio0 `WINDOW` literal; vi.rs 303 stores the host fb
  pointer; stack.rs fixed search window; headless.rs 218 `data_ptr` (host, fine).
- birdman64: hle.rs 116-122 `GfxMemory::host` + `map()` returning host as u32,
  396 `data_ptr as u32`, 438 `fb_key(framebuffer)`; audio.rs 52 `AudioMem::host`.
- pw64-game: memmap.rs `RDRAM_BASE` in `hle_writable`, `check_low_4gb`; build.rs
  `/BASE`; dylib.rs `DLL_BASE`; crash.rs ranges. pw64-kernel: math only, none.
- `pw64_fb_copy`/`pw64_fb_pixels` take vaddrs and the renderer keys fbs by vaddr: fine.

## Migration steps (each: build, `cargo test --workspace`, clippy, identity run)

Identity run: release build, then the fly_hang_glider script with `PW64_NO_THROTTLE=1
PW64_NO_INPUT=1 PW64_MAX_RETRACES=2700 PW64_DUMP_FRAMES=150,600,1200,1800,2400,2690
PW64_DUMP_HEIGHT=240 PW64_DUMP_AUDIO=<out>/audio.wav PW64_NO_AUDIO=1`; sha256 of the
PNGs + WAV must equal the pre-change baseline. Grep gate per step: the step's
pattern has zero hits left in its files.

- S1 **done**: layer (vaddr.rs, pw64_native.h macros, dylib import) + graphics.c.
- S2a: user_file.c, sched.c, system.c literals → `PW64_PTR`. S2b: memory.c.
  S2c: debug.c, audio_manager.c. Gate: `grep -nE '\)\s*\(?0x8[0-9A-Fa-f]{7}'` on the
  patched file (`target/release/build/pw64-game-*/out/patched/src/...`) = 0 hits.
- S3a **done**: misc.rs `osVirtualToPhysical` = `to_vaddr(p) − K0`; pi.rs DMA dest =
  `to_host(to_vaddr(p))`; mio0 bound = `to_vaddr(src)` in the vaddr RDRAM range
  0x80000000..0x80800000 (`to_host(end−1)+1` bounds within the region); vi.rs keeps
  the host fb pointer, the consumers (hle.rs `latch`/`retrace`) convert via `to_vaddr`.
- S3b **done**: hle.rs `GfxMemory::map` returns the vaddr (`raw | K0` if bit 31 clear);
  reads translate once per range through `VMapCell::load()`'s one-region fast path,
  `to_host` on miss (pure `GfxMemory::host(vm, regions, addr)`, unit-tested with local
  1/3-region lists — `set_regions` is once-only); `data_ptr`/`framebuffer` via
  `to_vaddr`; audio.rs `AudioMem::host` = `to_host(addr + K0)`. Bit-identical; perf
  unchanged (gfx HLE 0.48–0.50 ms/task both builds, whole-run avg within spread).
- S4 **done**: `PW64_RELOC=alias` (memmap.rs `Reloc`; unset/`off` = old path, byte-
  identical). `map_rdram_window` maps one zeroed shared block twice (Windows
  `CreateFileMappingW` + `MapViewOfFileEx`, Linux `memfd_create` + `mmap MAP_SHARED|
  MAP_FIXED_NOREPLACE`): at 0x80000000 and at the first free `0x7_2000_0000 + k·4 GB`,
  registered as `[RDRAM → high]` by `memmap::register_regions` (since S5; main.rs
  `run_game`, after `dylib::load`, before `os::boot`). Low 32 bits of the high view are 0x20xxxxxx, so a straggler that
  truncates a pointer gets a bit-31-clear "ROM offset", not the window.
  `memmap::rdram_host()` = the translated view; `hle_writable` uses it (high only).
  Stacks, exe/module image and any Rust heap pointer stay low and unregistered: the
  "no region" passthrough round-trips them; `to_vaddr_in`'s assert only fires above
  4 GB, so the rule is unchanged. Test: pw64-game `tests/rdram_alias.rs`.
- S4 gate result: default 7/7; alias 6/7 (static and first-run module alike).
  Frame 1800 differs in 2 HUD pixels: the red altitude digits' texrect (`G_SETTIMG
  0x800D16F0`, IA16, 822-byte LOADBLOCK) samples one row past its glyph data into a
  heap block that holds native pointers, whose bytes differ per mode. Pre-existing
  over-read, not a translation bug. `PW64_DUMP_RDRAM=1` writes `tmp/frame_<n>.rdram`
  (8 MB, vaddr order) next to each dumped frame; diffing off vs alias at all 6 frames:
  every differing word is a stored 8-byte host pointer, except ≤ 6 words in the audio
  heap (0x80034xxx): csplayer.c 569 reuses `evt` for `AL_CSP_NOTEOFF_EVT` after
  `evt.msg.vol.voice`, so the unread `midi.ticks` keeps the pointer's low half
  (harmless). Translation-layer bugs found: none. Logs: hle.rs fb lines now print vaddrs.
- S4 perf (6 interleaved runs, medians, ms/retrace avg | p99 | "rest"): pre-S4 0.592 |
  1.17 | 0.0345; S4 off 0.601 | 1.21 | 0.0350 (in spread); alias 0.613 | 1.23 | 0.0385:
  every `PW64_U32`/`PW64_PTR` of a stack/image address now misses the fast path.

## Next steps (specs)

Gate for S5-S7: release build; identity run in default mode 7/7; in the relocated mode
every PNG but frame 1800 matches and `PW64_DUMP_RDRAM` diffs show only pointer words;
all sweep scripts (sweep.md) run to the end with no panic and no crash.log; test +
clippy (Windows + `--target x86_64-unknown-linux-gnu`); perf vs pre-step (6 runs).

- S5 **done**: `PW64_RELOC=strict` (memmap.rs `map_rdram_strict`, `register_regions`;
  vaddr.rs `set_strict`, `to_vaddr_strict_in`/`to_host_strict_in`; main.rs `run_game`).
  RDRAM mapped once at the first free `0x7_2000_0000 + k·4 GB`; 0x80000000+8 MB
  reserved no-access (a window literal faults at its C site). `register_regions()`
  (all modes, once, in `run_game` before `os::boot`, i.e. after `dylib::load`):
  RDRAM → high, plus identity regions (`host == vbase`, now accepted by
  `set_regions`) for the stack window 0x80800000..0xA0000000, the module (dylib)
  and the exe image; then strict. Strict `to_host`: a vaddr ≥ 0x80000000 in no
  region panics. Strict `to_vaddr`: in no region, passes values < 0x80000000 and
  32-bit values that are already a region's vaddr; panics on the rest (any
  unregistered host pointer ≥ 4 GB, sign-extended s32, bit-31 value in no range).
  The panic message names the value; `RUST_BACKTRACE=1` names the C function
  (frames symbolise through the inline `pw64_*_fast`). Test: `tests/rdram_strict.rs`.
- S5 findings:
  - Panicking on every bit-31 value tripped at once on texture.c 833: `OS_PHYSICAL_TO_K0`
    (os_convert.h.patch) applies `PW64_U32` to a u32 that is already a vaddr. Not a
    bug: values < 4 GB are never host pointers, so `PW64_U32` stays idempotent on vaddrs.
  - Nothing else: no C or Rust straggler, no low-window fault, no sign-extended
    pointer. S2/S3 covered everything these flows reach.
  - Not detectable by S5 strict mode on Windows: a Rust heap pointer handed to the C
    (the heap sits below 0x80000000 there, the same range as ROM offsets). S6's
    high heap closes this.
- S5 gate: default 7/7 (static). Strict, static and first-run module: 6/7 (frame 1800
  = the known glyph over-read), RDRAM diffs = pointer words + the ≤ 6 csplayer
  words. All 9 sweep scripts + `PW64_START` a:hg:2, a:rb:1, a:gc:1, b:jh:1,
  beginner:cb:1, a:sd:1, beginner:bm:1 run with no panic and no crash.log. Frames
  match off mode, except 6-10 px in 5 of 48 frames, in the red HUD digits' texrect
  (`G_SETTIMG 0x800D16F0`, same over-read). Perf (6 runs, medians, avg | p99 |
  rest): pre-S1 0.593 | 1.20 | 0.0333; off 0.609 | 1.21 | 0.0350; strict 0.602 |
  1.21 | 0.0380 (image/stack conversions miss the fast path, as in alias).
- S6 **done** (stacks + Rust heap high in strict mode):
  - stack.rs: window `BASE` (default `SEARCH_START`), `CURSOR` is an offset from it,
    `stack_slot_in(base, ..)` pure, `window()`; `reserve_region_at(base)` reserves the
    whole window (Linux: PROT_NONE `MAP_NORESERVE`; slots `MAP_FIXED` over it, drop
    re-reserves). Strict `map_rdram_window` → `relocate_stacks`: window at the first
    free `0x7_4000_0000 + k·4 GB` (low bits bit-31-clear, apart from RDRAM's), old
    window 0x80800000..0xA0000000 reserved no-access; `register_regions` maps vaddrs
    0x80800000.. there. crash.rs filter uses `window()`. Offsets inside 64 KB, and
    the order RDRAM < stacks, match the vaddrs (alignment-dependent C unchanged).
  - high_heap.rs: `#[global_allocator] HighHeap` (birdman64 + rdram_strict test) =
    `System` until `enable()` (strict `memmap::init`), then a 64 GB arena at 1 TB+
    (power-of-two classes, free lists, one lock, never unmapped; blocks from before
    `enable` are freed to `System`, realloc moves them high). So on Windows too, a
    Rust heap pointer reaching `PW64_U32` panics. Strict perf: faster than System
    (avg 0.519 vs 0.610 ms/retrace) — audit tool, not a production allocator.
- S6 audit (Rust memory → C): no Rust heap or Rust-owned pointer reaches the C.
  - The C has no malloc; it allocates only from RDRAM/arena (pw64_players.c "no malloc").
  - Thunk pointer args all come from the C; Rust writes only scalars through them
    (osContGetReadData, osEepromLongRead, osPiReadIo, osRecvMesg, pw64_direct_start).
  - Pointer returns: `osVi{Current,Next}Framebuffer` (the C's own fb value),
    `__osGetCurrFaultedThread` (null), crt memset/memcpy (dest), `pw64_to_host`.
  - Every `OSMesg` Rust posts (events, timers, VI, PI `mb`) is a C-registered value.
  - Rust data the C reads by address: pw64-platform statics (`osTvType`,
    `osMemSize`, `osViModeTable`, ucode symbols) live in the image (S7; the
    dylib shim defines its own); pw64-game writes C globals by value only.
  - Finding, fixed: `pw64_rsp()` returned `rsp as u32`; now `to_vaddr(rsp)`.
  - C side: no raw `(uintptr_t)` of a pointer kept as an address (only differences
    and the `ALIGNED_BUFFER` low-bit test, graphics.c 95).
- S6 gate: default 7/7. Strict, static and first-run module: 6/7 (frame 1800, same
  hash as S4/S5); RDRAM diffs vs off (same exe) = pointer words + ≤ 6 csplayer words,
  no stack word. 16 sweep flows: no panic, no crash.log, frames = S5 strict. Perf
  (6 interleaved, medians, avg | p99 | rest): pre-S6 0.587 | 1.19 | 0.0342; default
  0.610 | 1.24 | 0.0345; strict 0.519 | 0.99 | 0.0367 (+0.002 rest: no second inline
  region needed).
- **S7 image anywhere** (build.rs, dylib.rs, memmap.rs, crash.rs):
  1. In `register_regions`, turn the identity image regions into `Region { vbase:
     0xC000_0000, host: exe start }` and module `vbase: 0xB000_0000`; then drop `/BASE` + `/DYNAMICBASE:NO` (Windows),
     `--no-pie --image-base` (Linux: C objects need `-fPIC`), `DLL_BASE`,
     `reserve_module_range` and the module `/FIXED`, and `check_low_4gb`'s image asserts.
  2. Check function pointers and `&global` stored in u32 slots go through the macros
     (display lists already do); crash.rs symbolises by real ranges, not constants.
  3. Image and stack addresses already miss the fast path in strict mode: "rest"
     +0.002 ms over off with stacks relocated (S6), under the 0.005 ms bar for a
     second inline region in `pw64_vmap`; re-measure with the image relocated.
  4. Ordering: today the image (0xC0000000 host, low) sorts below RDRAM/stacks
     (high), unlike its vaddrs; relocated above them it matches again. Keep the
     image's 64 KB alignment (true for PE/ELF loads), like S6 did for stacks.
  5. Strict + high heap stay the audit tool; the image's Rust statics (`osTvType`,
     `osViModeTable`, ucode symbols) become C-visible through the exe region.
- S8: make relocation the default, delete the window/reservation code.

## Performance (inline fast path, measured after S2)

- Fast path = 3 loads of `pw64_vmap` + sub/cmp + one always-taken branch + add; the
  Rust call stays as a cold miss path. Dylib flavour: the module can't read exe data,
  so the shim defines its own `pw64_vmap` (exported, DATA); `dylib::load` hands it to
  `vaddr::set_module_vmap` and `set_regions` writes both copies. `set_regions` rejects
  overlapping regions (the fast path tries one region before the others).
- Hang-glider script, 2700 retraces headless, `PW64_PROFILE_RETRACES=1`, 12 runs per exe
  interleaved; medians (spread of avg in brackets). "rest" = avg − gfx − audio HLE per
  retrace, i.e. C game + OS core, the only part the macros touch:

  | exe | avg ms/retrace | p99 | gfx ms/task | rest ms |
  |---|---|---|---|---|
  | pre-S1 static | 0.591 (0.562-0.608) | 1.194 | 0.483 | 0.0331 |
  | S1 call, static | 0.584 (0.564-0.611) | 1.169 | 0.474 | 0.0338 |
  | inline, static | 0.584 (0.566-0.622) | 1.192 | 0.470 | 0.0339 |
  | pre-S1 dylib | 0.619 (0.611-0.649) | 1.220 | 0.506 | 0.0337 |
  | S1 call, dylib | 0.624 (0.603-0.644) | 1.220 | 0.508 | 0.0357 |
  | inline, dylib | 0.619 (0.607-0.677) | 1.232 | 0.506 | 0.0346 |

- The S1 "+3%" was noise (run-to-run spread ±5%). The real cost is in "rest": ≤ 0.002 ms
  per retrace (S1 call, dylib), ≤ 0.001 ms inline: < 0.02% of the 6.9 ms 144 Hz frame.
  So no compile-time `PW64_FIXED_WINDOW` (macros = plain casts) flag. Fallback if a later
  step regresses: define that flag from build.rs for the Windows/Linux x86_64 fixed-window
  builds and make both macros plain casts there (no regions are ever registered then).
- Profiling old commits: `git worktree` + its own `CARGO_TARGET_DIR` (a shared one
  reuses the other tree's workspace crates: same relative paths, stale link).

## Pattern (S1 template: literal RDRAM address → host pointer)

In the `.patch` file only the `+` line changes (same line count, hunk header stays):

```diff
-    gGfxFbPtrs[0] = (u8*)0x800DA800;
+    gGfxFbPtrs[0] = (u8*)PW64_PTR(0x800DA800);
```

- `PW64_PTR(vaddr)` for every literal used as a pointer, `PW64_U32(ptr)` for every
  pointer stored in or compared as an int. Never `(u8*)0x80...` or `(u32)ptr`.
- Arithmetic stays on the side it started: `(u8*)PW64_PTR(0x80000000) + off` is fine
  inside one region; don't mix a converted pointer with an unconverted literal.
- Not in static initializers (reads a global, may call): assign at init instead.
- Rust side: `pw64_platform::vaddr::{to_host, to_vaddr}`; never `as u32` a host
  address or `as usize` a C address.
- Applier (pw64-cbuild `apply_unified_diff`): no fuzz — it skips to each hunk's
  `-start`, then matches context 1:1; any empty line in the `.patch` file is an
  empty context line, so never leave a trailing blank line (new hunks must end
  at a real context line). Blank context lines are a single space.
- Gate grep note: `rg` has no `-E`; use the plain pattern `rg -n "\)\s*\(?0x8[0-9A-Fa-f]{7}"`.
- Identity runs: `bash` in PATH is WSL (paths are `/mnt/c/...`, run.sh needs
  `/c/...` and zig.exe won't exec there) — use `C:\Program Files\Git\bin\bash.exe`.
