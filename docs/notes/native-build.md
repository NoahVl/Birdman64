# Native build (Phase 3) — design study

Paths are relative to `decomp/`. Numbers come from the WSL build: `mips-linux-gnu-nm` on
`build/src/{kernel,app}/*.o`, plus a `gcc -fsyntax-only` pass over kernel+app with `long`
forced to 32 bits (so every pointer-width cast gets flagged).

## 1. libultra surface used by kernel+app (complete undefined-symbol list)

| Group | Symbols | Plan |
|---|---|---|
| Threads/msgs | osCreateThread, osStartThread, osSetThreadPri, osCreateMesgQueue, osSendMesg, osRecvMesg, osSetEventMesg, osInitialize, __osGetCurrFaultedThread | Rust shim (OS core, §4) |
| Timers | osGetCount (clocks.c:9,14; tick 45.75 MHz, uv_clocks.h:7), osSetTimer (system.c:339) | shim on a host clock |
| PI/ROM | osCreatePiManager, osPiStartDma (system.c:437, audio_manager.c:302), osPiReadIo (memory.c:174), osPiRawReadIo (system.c:234, reads the ROM 0xFFB000 debug flags) | shim over the ROM buffer; synchronous DMA, then post the message |
| VI | osCreateViManager, osViSetMode, osViSetEvent, osViBlack, osViSwapBuffer, osViGetCurrent/NextFramebuffer, osViSetSpecialFeatures, osViModeTable | shim; VI retrace comes from a host 60 Hz timer |
| SI/EEPROM | osContInit, osContStartReadData, osContGetReadData, osEepromProbe, osEepromLongRead/Write (system.c:89,115) | shim (gilrs + a `.eep` file) |
| RSP | osSpTaskLoad, osSpTaskStartGo, osSpTaskYield, osSpTaskYielded (sched.c:139-370) | shim → HLE gfx/audio, then post SP/DP done |
| AI | osAiSetFrequency, osAiSetNextBuffer, osAiGetLength (audio_manager.c:126,205) | shim → cpal ring |
| Cache/addr | osInvalDCache, osWritebackDCache(All): no-op. osVirtualToPhysical (18 uses): `addr - 0x80000000` mod 2^32 | shim |
| Low-RAM globals | osTvType, osMemSize (=0x80000318 in the map), osResetType, osAppNMIBuffer, osRomBase, osRomType, osCicId, osVersion, osClockRate | forced-include macros → fixed arena addresses, written by Rust at boot |
| Audio lib | alInit, alClose, alHeapInit, alHeapDBAlloc, alAudioFrame, alBnkfNew, alSeqFileNew, alCSeqNew, alCSPNew, alSeqp* (7), alSndp* (11), alLink/alUnlink, alGlobals | compile `src/libultra/audio` (7.7k lines) natively, patching bnkf/seq loaders (§3); HLE the Acmd ABI in Rust |
| Sprite lib | spInit, spDraw, spMove, spScale, spColor, spFinish | compile `src/libultra/sp` as-is |
| gu* | **none** except `sqrtf`. The kernel has its own matrix code (matrix.c) | host `sqrtf` |
| libc/compiler | ldiv (utils.c:18), __ll_lshift/__ull_rshift (IDO helpers, not needed by clang) | host / trivial |
| asm-only | mio0_decompress (filesystem.c:81, asm at 0x80231a20) | export from `pw64-rom` Rust |
| Linker symbols | `*_ROM_START/END` (ROM offsets, e.g. filesys 0xDF5B0), kernel_/app_ segment symbols, ~130 `gsp*Text/DataStart` ucode symbols, D_803805E0 (=app_BSS_END) | shadow `segment_symbols.h` with constants. Set app_ROM_START==END and app_BSS_START==END so Thread_App's code copy (system.c:312) becomes a no-op |

Also referenced by headers but unused: __osPiTable, __OSGlobalIntMask. `_uvDebugPrintf` is empty (system.c:415); we can override it to log.

## 2. 32-bit pointer hazards

- **Fixed RAM map** (memory.c:41-76, graphics.c:105-107, sched.c:219, user_file.c:33, audio_manager.c:40, debug.c:626,646, system.c:223-227): 39 literal `0x80xxxxxx`.
  Layout: 0x80000400 audio heap · 0x800DA800 fb0 · 0x80100000 fb1 · 0x803DA800 zbuf/scratch ·
  0x8004181C..osMemSize+0x80000000 is the texture/level heap (`_uvMemAlloc`, which skips the reserved blocks) · kernel/app image 0x802000A0-0x803805E0.
- **Allocator returns `s32`** (`_uvMemAlloc` memory.c:259, ~60 call sites, all cast to pointers). `_uvMemGetScratch` returns literals. `_uvDMA` does `s32 dest = vAddr` (system.c:419).
  On x64 an `s32` ≥ 0x80000000 **sign-extends** when cast to a pointer, so every such site breaks.
- **Addresses in u32 fields**: the filesystem (`dataInfo->address`, filesystem.c:34-89), the memory map (`Struct802B8830`), `gGfxFbPtrs`, and `uvFileReadHeader(s32 addr)` called with pointers (13 sites).
- **ROM vs RAM by bit 31**: `(u32)p & 0x80000000` at memory.c:146,195 and filesystem.c:89. ROM "pointers" are raw ROM offsets.
- **Display lists hold K0/physical addresses in u32 words**: `OS_PHYSICAL_TO_K0` (texture.c ×5, geometry.c ×11, sprite.c) and `+0x80000000` (geometry.c:347).
  Only segment 0 is used, with base 0 (graphics.c:135). UVTX dlists are patched in place (`setimg.dram |= K0`, texture.c:810).
- **ROM structs with pointer slots**, raw-copied at `sizeof`: ParsedUVEN (`modelTable`, `callback`; uv_graphics.h:169-171, texture.c:413), UnkUVMD_24 (`unk20`, texture.c:533).
  Audio `alBnkfNew`/`alSeqFileNew` (bnkf.c:34-80) patch offsets into pointers **in place** inside BE ROM-layout structs.
  Also (found in task 3): font.c:104 `'BITM'` raw-copies `Bitmap[]` (`void* buf` holds an IMAG index, then patched to a pointer; count = `nbytes/sizeof(Bitmap)`);
  audio_manager.c:102-106 sizes `ALSeqFile` from the ROM header with host `sizeof(ALSeqData)` (16 on x64);
  `OSTask`/`OSScTask` (sptask.h) have 8-byte pointer fields: the Rust SP side reads the host layout.
- Static `Gfx` initialisers holding pointers only in debug.c:83-95 (3 lists, compile errors on 64-bit).
- **Negated unsigned index** `&p[-u32]`: 32-bit wraps back, 64-bit adds ~4G elements (no warning).
  Hit in audio reverb.c (`ALDelay.input/output` are u32); fixed with `-(s32)` in the patch.
- Compiler-flagged sites (int↔pointer of a different size): kernel 121 (texture.c 50, memory.c 17, filesystem 8, audio_manager 8, graphics 7, font 6, sprite 5, system 5, …), app 23, libultra audio+sp 17. **This is the complete to-do list.**

**Recommendation: Option C, "x86_64 + a low-4 GB identity window"** (fallback: Option D, an i686 build).
1. At startup, `VirtualAlloc` 0x80000000-0x80800000 at that exact address (mmap `MAP_FIXED_NOREPLACE` on Linux) as RDRAM. All literals, framebuffers, heaps and fixed globals then work unchanged.
2. Link the exe below 4 GB with bit 31 set: `/BASE:0xC0000000 /DYNAMICBASE:NO /HIGHENTROPYVA:NO` (§7 "Address space"). The default 0x140000000 is above 4 GB.
   Run game threads on stacks we allocate inside the window. Assert at runtime that every pointer the C code sees is < 2^32.
3. Keep `Gfx`/`Acmd`/`Mtx` 32-bit, so display lists stay bit-identical to N64 and the HLE renderer can be tested on real dumps.
   The renderer maps segment address p → host `(p + 0x80000000) mod 2^32`. This works for both window and image addresses, because K0↔phys is just ±0x80000000 mod 2^32.
4. Fix the ~160 C sites the compiler flags (`s32` → `u32`/`uintptr_t`, so values zero-extend), and turn debug.c's static dlists into runtime-built ones.
   ROM-layout structs with pointer slots get layout-aware loaders, which are needed for endianness anyway (§3).

- Option A (widen Gfx like the SM64 port) is a strict superset of C's fixes. It also needs every literal to go through a base, the allocator and filesystem made 64-bit, ROM Gfx widened on load, and the renderer to use a non-N64 format. Only do it later, for macOS arm64 (its 4 GB `__PAGEZERO` forbids low mappings).
- Option B (arena + offset translation) can't be done for compiled C without rewriting every dereference. C is B with base 0.
- Option D (i686): fewest C changes (no sign-extension, N64 struct sizes), but it forces wgpu/cpal/gilrs onto 32-bit, rules out macOS, and still needs the window.

## 3. Endianness hazards

- **Scalar choke point**: `uvMemRead`/`uvConsumeBytes` (memory.c:185, texture.c:370) with sizes 1/2/4. Override both in Rust: BE reads from ROM *or* from RAM holding raw file bytes (GZIP blocks decompressed into scratch).
  `uvConsumeBytes` with any other size falls through to a raw `_uvMediaCopy` (texture.c:385).
- **Typed raw copies need a per-type swap after the copy** (about 60 sites):
  - texture.c: Vtx arrays 480, 634 (s16×6 + u8×4); Mtx4F arrays 528 (f32); Mtx 646 (swap **u16 halves**, because C accesses Mtx via `Mtx_u`, uv_matrix.h:18-29, matrix.c:95-147); ParsedUVEN 413; UnkUVMD_24 533; UnkC 638 (u16); level id arrays 855-945 (u16).
  - Also: fx_seq.c:60, audio_manager.c:102-105,386-391, font.c:99-112, and app level.c:294-315, task.c:614-679 (≈20 `Task*` structs; ROM data is pointer-free, task.h:369-387), spath.c:73-88, demo.c:182-189, demo_attitude.c:81, text_data.c:40-45 (s16 table).
  - Proposal: generate `swap_<Type>()` from clang record layouts (`-Xclang -fdump-record-layouts`) and insert calls via the patch set.
- **Gfx from ROM**: UVTX dlists are copied raw (texture.c:761) → swap u32 words. There is one Gfx **bitfield** read (`setimg.cmd`, texture.c:807; gbi.h:1567 assumes MSB-first bitfields) → patch to `w0 >> 24`.
  UVMD/UVCT dlists are rebuilt from BE u16 elements through uvConsumeBytes, so they are safe.
- **Texels** (`_uvExpandTextureImg` texture.c:839, font bitmaps font.c:108) stay raw BE bytes; the renderer's TMEM load reads them BE, like the RDP. CPU pixel writers (graphics.c:1133 fb copy, `gGfxCallback`) need a case-by-case look in Phase 4.
  - **Fixed 2026-10-01: env texture tint** (code_8170.c `func_802077BC`/`8020921C`/`8020B894`/`8020D0D8`, driven by
    environment.c `env_802E1C1C` for dusk/night/snow envs 4,5,6,9-11,14-17,20) rewrites texels in RAM as **u16** (RGBA16,
    IA16, I16) → read host-LE = rainbow noise on ground/roads/water (Little States dusk rocket-belt demo, Everfrost
    hang-glider demo; any such env in play too). Patch swaps 16-bit images to host and back around the loops
    (`pw64_tint_swap16`). Found by VirtualProtect-ing the texture's page after load → crash RIP → symbolize.
    Debug recipe for "texture X turns to garbage": log `uvParseTopUVTI` id/addr via `PW64_LOGF` (+ `PW64_TRACE_OS=1`).
  - **Audit 2026-10-01 (CPU texel/pixel access), no other texel writers:** the tint loops are the only C that
    touches texels at >8-bit width. Photos are not captured images: the album re-renders each saved camera into
    an 80×59 viewport (`func_8033A244`) + `uvCopyFrameBuf` (byte memcpy, mirrored on the GPU by `pw64_fb_copy`);
    run of `fly_hang_glider_finish.txt` shows correct cells. Snow writes `0xFFFF` (byte-symmetric, mirrored by
    `pw64_fb_pixels`). No TLUT/palette writes, no gDP texture loads in game C besides libultra `spDraw`; blit/
    font texels are raw byte copies (`uvConsumeBytes` with size ≠ 1/2/4). Vertex colours (`Vtx.cn`, bytes) are
    written to host-swapped Vtx and read by `pw64-gfx` via `read_raw`, so tint/fog vertex writers are fine.
  - **Fixed: byte-punned compiled-in data**: code_A8C30.c `func_80321760` (Congratulations camera) read
    `((u8*)D_8034F980)[k]` from an `s32[]` whose tail packs a byte table → LE picked the wrong bonus-vehicle
    camera preset; patched to take the BE byte. Grep for `((u8*)`/`((u16*)` casts of wider arrays found no others
    (`gMedalPointRequirements` is an all-u16 struct: fine).
- **Audio**: .ctl bank/.seq headers need BE→LE plus a 32→64 layout deserialiser in place of bnkf.c/seq.c patching. ADPCM is a nibble stream; raw PCM in .tbl is BE s16 (HLE reads it BE).
- **Task 6 implementation (done)**: `PW64_SWAP(p, count, T, layout)` (pw64_native.h) → Rust `pw64_swap` (pw64-platform/src/swap.rs): swaps `count` elements of `sizeof(T)` at `p`, layout = `[N]b/h/w/d` per field; **panics unless the layout sums to sizeof(T)** — this caught 3 wrong layouts live (TaskPHTS is "5w", TaskObjects_Unk10 has 4 f32s not 2 = "7w"). `PW64_SWAP16/32`, `pw64_fatal` also in swap.rs. Sites (patches): texture.c (Vtx ×2, Mtx4F ×2, Mtx "32h" (u16 halves!), Gfx "2w", UnkUVMD_24 data-prefix loader, ParsedUVEN prefix, UnkC "4h", UVLV id u16s ×9, UVTR f32s, UVBT `sp80 >>= 32` → `__builtin_bswap32`, 'FRMT' via uvMemRead, TABL headers "2w", gbi bitfield → `words.w0 >> 24`), memory.c (gUVBlockCounts "w 16h"), font.c (N64 Bitmap = 16 B with 4-byte buf → host loader `pw64_load_bitmaps`, pw64_rom_structs.c), audio: alSeqFileNew/alBnkfNew in-place patching replaced by deserialisers `pw64_alSeqFileLoad`/`pw64_alBnkfLoad` (native/src/pw64_audio_load.c; BE ctl staged in a static buffer, offsets→pointers, host structs from `pw64_arena_alloc` — the audio heap is too small widened; instArray offset 0 = empty instrument, as on N64), audio_seq.c (ALCMidiHdr "17w" after the raw seq copy), app: level.c (ESND/WOBJ/LPAD/TOYS/TPTS/APTS/BNUS; LEVL is all u8), task.c (comm f32 fields, Unk803599D0 ×11 "21w" (5 named + 6 hidden in `pad1A4`: snap.c photo scoring reads [5..10]; ×5 crashed photographing the oil rig — swap untyped `pad` regions too when the code indexes past the named members), every data block; SDFM = pad only), demo.c (RPKT/RHDR), demo_attitude.c (PHDR read via uvMemRead, PPOS "6w"), spath.c (u32 count + PathTimeVal prefix of the block), text_data.c (DATA s16 table). fx_seq ParsedUVSQ: built via uvConsumeBytes already — no swap.
- **Host structs wider than N64** (pointer fields): use a field-wise loader from pw64_rom_structs.c / pw64_audio_load.c, not a swap.
- **Safe**: tag constants `'COMM'` (same value in clang, gcc and MSVC; tags are read via BE uvMemRead). The EEPROM save is packed bytewise (save.c:13-30), so `.eep` files stay emulator-compatible. App bitfields (code_51E30.h:8, snap.h:16) are RAM-only (verify).

## 4. Threading model

| Thread | id / prio | Where | Role |
|---|---|---|---|
| boot (`bootproc`) | – | system.c:220-247 | clears fbs, osInitialize, ROM debug flags, starts Kernel |
| Kernel | 1 / 12 → 0 | system.c:317-329 | starts PI mgr, Render, scheduler, App; then `osSetThreadPri(NULL,0); while(1){}` |
| Render (fault screen) | 0 / 250 | system.c:297-310 | blocks on OS_EVENT_FAULT forever: drop it |
| Scheduler | 4 / 127 | sched.c:182-265 | interruptQ: VIDEO / RSP_DONE / RDP_DONE / PRE_NMI |
| Audio mgr | 3 / 110 | audio_manager.c:91,161-236 | on a sched client msg → alAudioFrame → M_AUDTASK to sched cmdQ |
| App | 6 / 10 | system.c:323, map3d.c:63 | game. **Nested per-screen loops** (e.g. map3d.c:74-78) each call uvGfxBegin/End |
| VI / PI managers | 254 / 150 | sched.c:217, system.c:318 | libultra internals: not needed |

Flow: VI retrace → VIDEO_MSG → `_uvScHandleRetrace` (sched.c:269): `uvClkUpdate`; drain cmdQ (gfx task from uvGfxEnd graphics.c:611, audio task); run audio first (yielding gfx) else gfx; notify clients (audio frameQ, app D_802C3B50, which is never read).
SP done → RSP_DONE → task msgQ. Gfx done → D_802C3B90, which uvGfxEnd waits on (graphics.c:616, 1081). DMA waits on gPiDmaQ (system.c:439).
App pacing = previous gfx task done. Logic uses a measured frame time (graphics.c:624, app code_9A960.c:9); a fixed override exists (graphics.c:900).

**Proposal**: emulate the single-core libultra scheduler on one host thread.
- Each OSThread is a stackful coroutine (e.g. `corosensei`, custom stack in the window). A Rust run-queue follows libultra rules: the highest-priority ready thread runs, and switches happen only on block/send/start/setpri.
- "Interrupts" (VI 60 Hz, SP/DP done after HLE, SI, timers) are posted by the host loop between switches. When nothing is ready, the host pumps winit, presents, and sleeps until the next VI.
- `osSetThreadPri(NULL,0)` parks the caller forever, which skips the spin at system.c:328.
- Real parallel threads would race on unlocked shared globals (gSchedRspStatus, D_802B9C60, …). If coroutines cause trouble, use baton-passing real threads (one runs at a time).
- The App thread can't become a per-frame callback because of the nested loops.

## 5. Compiler hazards

- **GLOBAL_ASM**: both left are **JP-only** (`#if defined(VERSION_JP)`, control_info.c:418-469). The US build has none. `GLOBAL_ASM` expands to nothing without `__sgi` (macros.h:4).
- **Use clang** (clang-cl or `--target=x86_64-pc-windows-msvc`), which is **not installed** (VS "C++ Clang tools" component or LLVM). cl.exe chokes on `__builtin_va_*` (include/libc/stdarg.h:5-9), and pragmas are GCC-style.
- **Flags**: `-nostdinc -std=gnu11 -funsigned-char` (IDO chars are unsigned) `-fno-strict-aliasing -fwrapv -ffp-contract=off` (no FMA on the VR4300), no fast-math, `-Wno-multichar`.
  Defines: `-D_LANGUAGE_C -DVERSION_US -DBUILD_VERSION=VERSION_D -D_FINALROM -DNDEBUG -DTARGET_N64 -DNON_MATCHING -DAVOID_UB -DRECOMP_BUILD`. Not `__sgi`, not `F3DEX_GBI` (the game uses **Fast3D**: graphics.c:606 overrides F3DEX).
- **RECOMP_BUILD** (macros.h:57-63) makes STATIC_DATA/STATIC_FUNC non-static, so Rust can override or inspect any function or global (Phase 7). Check for duplicate names at link time.
- `long` is 32-bit only on LLP64 (Windows). On LP64 (Linux) the patched headers spell it `PW64_LONG32` (§10).
- **Bitfield allocation differs** (found via the pw64_swap "2w vs 16" assert, 2026-09): MSVC/clang-cl on the MS target gives a bitfield of a *different* type than the preceding member its own storage unit, and `-mno-ms-bitfields` is unsupported for x86_64-pc-windows-msvc ("Itanium-compatible layout ... not yet supported"). IDO bit-packs like GCC. Only two structs in the decomp actually hit this — both in the `Gfx` union, which must stay 8 bytes: `Gpopmtx` (`int cmd:8; int pad1:24; int pad2:24; unsigned char param:8` → 12 bytes) and `Gsetcolor` (`int cmd:8; unsigned char pad/prim_min_level/prim_level; unsigned long color` → 12 bytes). Fixed in `patches/include/libultra/PR/gbi.h.patch` by making every member an `int`/`unsigned int` bitfield (`Gfx` 16 → 8; member bit positions differ from IDO, harmless because the game only uses `words` for these). `native/src/pw64_layout_checks.c` static-asserts Gfx/Vtx/Mtx sizes and the OSTask offsets headless.rs reads. Find such structs with `_Static_assert(sizeof(T)==N)` probes compiled with the exact build flags (`c-flags.txt`); audit any struct whose ROM size is known (pw64-formats) plus every `PW64_SWAP` layout string.
- **Bitfield order within a word** also differs: IDO packs a u16 bitfield group MSB-first (first-declared = most-significant), MSVC LSB-first. `UnkPartStruct_Unk8`'s `{pad12_0:4, unk12_4:1, unk12_5:3, pad12_8:8}` is declared reversed natively in `patches/include/kernel/uv_filesystem.h.patch` (ground truth: pw64-formats `uvan.rs` — format=(flags>>8)&7, flag=flags&0x800).
- **snap.h `Unk8033F050` is NOT RAM-only** (the §3 "verify"): it is the EEPROM photo-album record. `func_8033E860` (save) / `func_8033F050` (load) bit-copy the first 172 bits of the struct's *memory* (LSB-first per byte) ↔ the 0x408-bit album in `saveFileWrite/Load`. IDO: size 0x18, `unk9_*` packed into bytes 9–11 and `unk12_*` into 0x12–0x15, MSB-first; MSVC: 0x1C (new u32 unit at 0xC and 0x18, LSB-first) → the native album saved padding and lost every `unk12_*` field (veh, subject count `unk12_7`, subject extras) and wasn't emulator-compatible. Fixed in `patches/src/app/snap.c.patch`: `pw64_photoToIdo/FromIdo` build/parse the IDO byte image and the bit loops run on it. Encoder verified against clang `--target=mips-unknown-linux-gnu` (MIPS BE = IDO bitfield rules) with 32 field vectors folded to `ret i1 true` at -O3 (+ a broken-encoder negative control); decoder = exact inverse (100k random 172-bit images). Probe recipe for any other IDO bitfield struct: C++ probe with `S` + encoder, `check(k)` = memcmp of `memcpy(&S)` vs encoder, one `extern "C" bool tK()` per vector, `clang++ --target=mips-unknown-linux-gnu -O3 -mllvm -inline-threshold=10000 -S -emit-llvm` → grep `ret i1` (constexpr `__builtin_bit_cast` of bitfields is unsupported in clang 18). Host size pinned in `pw64_layout_checks.c` (0x1C).
- The IDO `stdlib.h` `size_t` conflicts (compiler/ido/stdlib.h:9) and ldiv.c won't compile: shadow them.
- UB relied upon:
  - uninitialised locals marked @bug (audio_emitter.c:302, camera.c:839, code_66F70.c:127, task.c:709, total_results.c:259, hang_glider1.c:169) — **now built with `-ftrivial-auto-var-init=zero`**: clang -O2 had turned `hangGliderMovementFrame` (reads `sp5B`) into a fall-through into `int3` (0x80000003) on the first flight frame;
  - `va_start` on a u8 param (audio_emitter.c:179, chan.c:362);
  - the deliberate fault `*(u16*)1 = 0` (memory.c:165);
  - prototype mismatch `app_entrypoint(s32)` vs `(void)` (system.c:63, map3d.c:63);
  - self-copy bug (task.c:176);
  - `switch ((int)msg)` on an OSMesg (sched.c:250);
  - float→int out-of-range results differ (MIPS 0x7FFFFFFF vs x86 0x80000000), so audit if behaviour diverges.

## 6. Ordered tasks to "boots and emits dlists" (relative effort estimate in bold parentheses)

1. **Done** (see §7): `pw64-game` build.rs (clang-cl), patch mechanism, shadow headers, forced include, panic stubs; links clean and boots to the first stub. **(1-2)**
2. **Done** (§7 "Address space"): RDRAM window, exe base, <4 GB assertions. **(0.5)**
3. **Done** (§7 "Pointer patch set"): pointer-width warnings 247 → 0, now `-Werror`; debug.c static dlists. **(2-3)**
4. **Done** (§8): OS core: coroutine threads, message queues, events, timers, osGetCount; boot `bootproc`. **(2)**
5. **Done** (§8): PI/ROM: `osPiStartDma`/`osPiReadIo`/`osPiRawReadIo` from the ROM (the C `_uvMediaCopy`/`uvMemRead`/`_uvDMA` stay: they funnel into these, and `uvMemRead` is already BE-safe); mio0 exported; debug flags zeroed. **(1)**
6. **Done** (§3 "Task 6 implementation"): `PW64_SWAP` + Rust `pw64_swap` (layout-string validator), per-site swaps in the patches, audio bank/seq deserialisers (native/src/pw64_audio_load.c, arena 0x802000A0..0x80380000 — not 0x80200000: `uvMemClearRegions` zeroes up to kernel_TEXT_START 0x802000A0 on every level load, see audio.md "SFX much louder than the music"). **(1)**
7. VI/SP: framebuffer/VI shims; `osSpTaskStartGo` hands gfx tasks to `pw64-gfx` (first just dump/validate them), and audio tasks get silence + SP done. SI: pads + EEPROM file. **(1-2)**
8. Boot to the title screen's first dlist, then Phase 4. Native libultra audio + bnkf/seq loaders belong with Phase 5. **(1-2)**

## 7. Native build setup (implemented, step 1)

Crates: `pw64-game` (build.rs compiles the C; lib declares C entry points), `pw64-platform` (`src/stubs.rs`: every C import), `pw64` (exe).
`cargo run -p birdman64` calls `pw64_game::memmap::init()` (RDRAM window + asserts), then boots `bootproc` (§8).

- **Sources**: `decomp/src/{kernel,app,libultra/audio,libultra/sp}/*.c` (197 files). Not libultra os/io/libc (Rust replaces them).
- **Compiler** (first-run-build.md "zig as the dev compiler"): **zig 0.16.0** (`zig clang`, clang 21) with exactly the first-run builder's `pw64_cbuild::zig_flags` (-O3 in every profile; + `-g`/`-gcodeview` when the profile has debug info), archived by `zig lib` (Windows: COFF archive for link.exe) / `zig ar` (Linux). zig = `PW64_ZIG`, else `tools/zig/zig[.exe]` (`cargo run -p pw64-cbuild --example get_zig --features fetch`); no PATH lookup; version must be 0.16.0 (in the objects' rebuild key).
  Legacy fallback `PW64_CC=clang-cl` (Windows) / `PW64_CC=clang` (Linux): `PW64_CLANG_CL` env, else `clang-cl.exe` on PATH, else `C:\Program Files\LLVM\bin\clang-cl.exe`; cc's baseline (`--target`, `-MD`, opt from the profile, `-Z7`) + the MSVC/GNU twins; `cc` archives. Switching `PW64_CC` rebuilds every object.
  Full command line: `$OUT_DIR/c-flags.txt`. Our build.rs drives the compiler itself (parallel, per-object).
- **Flags** beyond §5: `/clang:-fgnuc-version=4.2.1` (clang-cl doesn't define `__GNUC__`, which macros.h/PRinternal key `__attribute__`/ALIGNED on), `-Wno-incompatible-library-redeclaration` (os_libc.h bcopy/bzero), `-Werror=` int-conversion, pointer-to-int-cast, int-to-pointer-cast, void-pointer-to-int-cast, int-to-void-pointer-cast, shorten-64-to-32 (all 0 after task 3); `-Wno-error=` for the other clang 16+ default errors.
  cc adds `-W0` when warnings are off: build.rs strips `-W0`/`-W4`, so clang's default set applies.
- **Headers**: `crates/pw64-game/native/include` comes first on the include path and shadows `segment_symbols.h` (ROM offsets/N64 addresses from the map as constants; app overlay copy/BSS clear made no-ops), `libc/stdint.h`, `libc/stddef.h` (64-bit intptr/ptrdiff), `stdlib.h`, `math.h`, `stdio.h` (IDO ones have a 32-bit `size_t`).
  `native/pw64_native.h` is force-included: host `size_t`, `PW64_NATIVE`. `-Iinclude/libultra/compiler` is needed for sgidefs.h's `"gcc/sgidefs.h"`.
- **Patches**: `crates/pw64-game/patches/<path under decomp>.patch`, unified diff (`--- a/src/..`), for `src/**.c` or `include/**.h`.
  A `.c` is copied (plus its dir's `.h`, for quoted includes) to `$OUT_DIR/patched/` and patched there. `decomp/include` is mirrored **whole** to `$OUT_DIR/patched/include` (header patches applied; files rewritten only on change, so mtimes stay) and the `include*` `-I` dirs point at the mirror: a partial copy would let quoted includes from sibling headers reach the unpatched original.
  Strict, no-fuzz applier; compares lines ignoring CR on both sides (decomp checkout is CRLF; `.patch` files are LF via .gitattributes, CRLF patches tested to apply too).
  Make one: `tr -d '\r'` the file to `a/<p>` and `b/<p>`, edit `b/`, `git -c core.autocrlf=false diff --no-index --no-prefix a/<p> b/<p>` from the parent dir, drop the `diff`/`index` lines.
  `#ifdef PW64_NATIVE` guards aren't needed (patches only exist natively); comment semantic changes with `PW64:`.
- **Address space** (`pw64-game/src/memmap.rs`): `map_rdram_window` (VirtualAlloc at 0x80000000, 8 MB); `check_low_4gb` asserts the exe image (`__ImageBase`+SizeOfImage: all C globals/functions) is < 4 GB and above the window, plus window, a Rust heap block and the current stack < 4 GB; `assert_low` for other ranges.
  Exe base **0xC0000000** (chosen over patching call sites): < 4 GB and bit 31 set, because `_uvMediaCopy`/`uvMemRead`/`uvFileReadBlock` treat `(u32)p & 0x80000000 == 0` as a ROM offset, so a C global at 0x10xxxxxx passed as a source was read from ROM. Those sites are generic (any pointer can reach them), so fixing the address space covers all of them. Clear of the window and the coroutine stacks (0x80800000..0xA0000000, §8).
  Link args `/BASE:0xC0000000 /DYNAMICBASE:NO /HIGHENTROPYVA:NO /IGNORE:4281` live in pw64-game build.rs: applied to its own tests (`rustc-link-arg`) and exported as `links` metadata to `crates/birdman64/build.rs` (`DEP_PW64GAME_LOW_BASE_LINK_ARGS` → `rustc-link-arg-bins`). A new crate whose bins/tests link the C needs the same build.rs.
  Measured: image 0xc0000000..0xc0288000 (C global 0xc0251f98), Rust heap ~0x5da860, main stack ~0x14fa9f (C runs on the §8 stacks). The Rust heap has bit 31 clear: don't hand heap buffers to C code that does the ROM/RAM bit test.
- **Pointer patch set** (task 3): 35 patches (30 .c, 5 headers). Helpers in `pw64_native.h`: `PW64_U32(p)` pointer→u32 (lossless: all addresses < 4 GB), `PW64_PTR(a)` s32/u32→pointer **zero-extended**. The real hazard is an s32 address ≥ 0x80000000 sign-extending on its way to a pointer (every C-visible address has bit 31 set now).
  - Headers: `_uvMemAlloc`/`_uvMemAllocAlign8` return `void*` (were s32; ~60 callers); `uvFileReadHeader(void*)` (RAM pointer or ROM offset; was s32; same for `uvaManager_80204438`); `OS_PHYSICAL_TO_K0/K1`, `OS_K0_TO_PHYSICAL`, `K0_TO_PHYS` compute in u32 (mod 2^32); gbi.h `g*` (not `gs*`) address operands go through `PW64_U32` (gDma0p/1p/2p, gSPVertex, gImmp1, branchZ, LoadUcode, Dma_io, gSetImage).
  - Real sign-extension bugs fixed: memory.c `alignCeil`, `_uvDMA` dest (zero-extends now; the osPiStartDma truncation workaround is no longer needed), audio_manager `lastInfo` (s32 → `AudioInfo*`), string.c `%s` (`argStr = arg = va_arg`), bnkf.c offsets (s32 → u32), `_uvMemAlloc` callers, `uvFileReadHeader` params.
  - Kept 32-bit by design: event data (`uvEventPost(0xD, PW64_U32(&sp44))`: a handler that derefs it must `PW64_PTR`), proxanim `clientData`, audio `memin`/`dramout` (ROM offsets/phys), audio setParam ints packed in `void*` (`__INTPTR_TYPE__` round trip).
  - Gfx words hold raw pointers (window, image 0xC0xxxxxx, stacks) **or** `OS_PHYSICAL_TO_K0` values (= p + 0x80000000 mod 2^32: image → 0x40xxxxxx, window → 0x00xxxxxx). Renderer: in window/image/stack range → identity, else +0x80000000 mod 2^32.
  - `K0_TO_PHYS` masks to 29 bits (audio load.c: ADPCM book/state, in the window heap): only window addresses survive it.
  - Other: `app_entrypoint` prototype fixed to `(void)`; debug.c `uvDbg_80233FC8` walks varargs via `&arg1` (only the first arg works on x64; all callers pass one `%d`); ptrdiff → s32 casts made explicit.
  - Left (non-pointer): -Wunused-value 5, -Wunsequenced env.c:537, -Wreturn-type load.c:436, -Wtautological fx.c:1285 → 8 warnings total.
- **Incremental**: build.rs reruns only on changes under decomp src/include, `native/`, `patches/`. Then an object is rebuilt only if its source/headers (clang depfile) or flags+source path changed. Full C build ≈3.5 s (debug).
- **Diagnostics**: per object `<obj>.log`; all in `$OUT_DIR/c-warnings.log`, summarised in one cargo warning.
- **Linking**: the archive is `+whole-archive`, so every object is linked and every unresolved symbol shows. `pw64-game` depends on `pw64-platform`, so anything linking the C (the exe, tests) gets the stubs.
  Stubs are `extern "C-unwind"` fns that panic with the name + C prototype. The panic unwinds through the C frames (x64 table-based unwinding) to `main`: exit 101, clean message.
- **Unresolved set** (53 after the memory.c patch): os threads/mesg/events/`osSetIntMask` (from the audio lib), timers, cache, PI, VI, SI/EEPROM, SP tasks, AI, `mio0_decompress`, plus data: `osTvType`, `osMemSize`, `osViModeTable` (zero placeholder; `src/libultra/io/vitbl.c` + `vimodes/` hold the real table), and 8 ucode symbols (address-only).
  Not needed: bcopy/bzero, ldiv/sqrtf (host CRT/builtins), `alGlobals` (audio lib compiled).
### Warning stats before task 3 (historical; now 8, 0 pointer-width)

255 warnings, 247 pointer-width, from the `-O0` build (`c-warnings.log`):

| Flag | Count |
|---|---|
| -Wint-conversion (implicit ptr↔int, mostly `s32`/`u32` params and fields) | 56 |
| -Wpointer-to-int-cast | 53 |
| -Wint-to-pointer-cast | 49 |
| -Wvoid-pointer-to-int-cast | 41 |
| -Wint-to-void-pointer-cast | 34 |
| -Wshorten-64-to-32 (ptrdiff/size_t → s32/u32; all benign lengths, except graphics.c:595 `uintptr_t`→u32 and texture.c:381) | 14 |
| other: -Wunused-value 5 (comma ops), -Wunsequenced 1 (audio env.c:537), -Wreturn-type 1 (audio load.c:436), -Wtautological 1 (fx.c:1285) | 8 |

By dir: kernel 186, app 24, libultra/audio 24, libultra/sp 13.
Top files: texture.c 62, geometry.c 34, memory.c 18, graphics.c 18, sp/sprite.c 13, audio/load.c 9, sprite.c 9, filesystem.c 8, font.c 7, audio_manager.c 7, audio/reverb.c 5, system.c 5, dobj.c 4, anim.c 4, audio/bnkf.c 3, app/{user_paths,toys,text_data,demo_attitude,code_A64C0}.c 3 each.
List a file's sites: `grep '<file>' $OUT_DIR/c-warnings.log`.

## 8. OS core + PI (tasks 4/5, `crates/pw64-platform`)

- **Coroutines: `corosensei`** (Windows x64 incl. TEB stack fields, panics propagate across `resume`, custom `Stack` trait). Win32 fibers or own asm would need our own unwind/TEB handling; real threads + baton add locking for nothing.
- **Scheduler** (`os/mod.rs`): one host thread; every switch goes via the root loop (`os::run`). Highest-priority ready thread runs, FIFO within a priority; switches only at block/yield/stop/setpri/start/send-wake. Blocked threads are Rust-side states (`WaitRecv(mq)`), so `OSMesgQueue.mtqueue/fullqueue` stay null; the ring buffer (`validCount`, `first`, `msg`) is the C struct's, since C reads `validCount`.
- **Priority 0 never runs** (the host loop is the idle thread): parks Kernel after `osSetThreadPri(NULL,0)` instead of spinning. `bootproc` runs as its own thread (id -1, pri 1) via `os::boot`.
- **Interrupts** (VI 60 Hz, timers, SP/DP/SI events) are delivered by the root loop when all threads block, and at OS-call checkpoints (entry of send/recv/jam/yield) unless `osSetIntMask(OS_IM_NONE)`. `os::run` sleeps until the next event, or skips idle time (`PW64_NO_THROTTLE`).
- **Deferred gfx task (RSP HLE)**: `osSpTaskStartGo` only makes a gfx task pending; `headless::run_pending_rsp` runs it when the root loop would resume a thread below `RSP_PRI` (100), when idle, and before *every* interrupt delivery (`os::deliver_interrupts`, root loop and checkpoints alike). Invariants `sched.c` relies on: (1) the scheduler never sees a retrace while a task it started before that retrace is unfinished — two retraces with a pending yield request (`D_802B9C6B`) and a queued audio task run `_uvScDlistRecover`; (2) SP and DP done are posted with no switch in between (`run_task` posts both, then `reschedule`), else a retrace can land between them, find `gSchedRdpStatus` busy and delay the swap a frame.
- **Pause** (`os::PAUSED`, settings overlay): `Clock::freeze` pins the count; `unfreeze` resumes from the frozen value via a `paused` offset subtracted from wall time (adding the pause to `skipped` — the first version — jumped the clock forward by 2× the pause).
- Count = 46.875 MHz (`OS_CPU_COUNTER`; host monotonic clock + skipped idle). The game's `UV_CLK_TICK_FREQ` assumes 45.75 MHz; hardware behaviour kept.
- VI (`os/vi.rs`): `osViSetEvent` every N retraces; `osViSwapBuffer` latches at the next retrace (sched.c `_uvScRunGfx` waits for current == next); `os::set_retrace_hook` for presenting.
- **Present tick** (`PW64_FPS` ≠ 60, framerate.md "Present tick"): `os::set_present_rate` → the swap latches at the tick instead, which posts `PW64_PRESENT_MSG` (670) to the scheduler's interruptQ (`_uvScHandlePresent`, sched.c.patch) after any same-time retrace; `os::set_present_hook`. It goes through the same delivery points as retraces, so invariant (1) holds for ticks too (a started gfx task finishes before a tick is posted; test `gfx_task_finishes_before_the_next_present_tick`).
- **Stacks** (`os/stack.rs`): 1 MB, fully committed, fixed addresses in **0x80800000..0xA0000000** (just above the RDRAM window). They must be < 4 GB **and have bit 31 set**: `_uvMediaCopy`/`uvMemRead` treat `(u32)p & 0x80000000 == 0` as a ROM offset, and `uvMemRead` of ROM data re-reads it from a stack temp, so stacks below 2 GB recursed into a stack overflow. The C stack arrays (8-16 KB) are ignored.
  Same hazard for C globals: solved by the exe base 0xC0000000 (§7 "Address space").
  - **Slot layout** (1 MB + 64 KB, 64 KB-granular: a fallback MEM_RESERVE at an unaligned address is rounded down): no-access page | 56 KB RW headroom | PAGE_GUARD page | 1 MB stack. TEB `DeallocationStack` = the guard page, so a guard hit has no room to "grow" and the kernel raises STATUS_STACK_OVERFLOW (0xC00000FD) at once; the exception dispatch + `[crash]` report run on the headroom (one guard page alone left too little: silent death). Test: crash.rs `coroutine_stack_overflow_is_reported` (child process). **Gotcha**: `VirtualProtect(PAGE_GUARD)` moves the *calling* thread's TEB StackLimit to just above the new guard page; osCreateThread runs on a coroutine, whose StackLimit then sat above its StackBase and the next `__chkstk` probed down from there into the neighbour slot's guard (STATUS_GUARD_PAGE_VIOLATION, silent exit 1). `map_fixed` saves/restores gs:[0x10] (test `new_stack_keeps_the_callers_stack_limit`). Before guard pages, those probes silently wrote zeros into the neighbour's stack. Linux: the 3 low parts are PROT_NONE (SIGSEGV runs on the sigaltstack).
  - **Early reservation (Windows)**: `pw64_game::memmap::init` (before window/GPU) MEM_RESERVEs 0x80800000..0xA0000000 (`stack::reserve_region`) and, `dylib`, 0xB0000000..0xC0000000 (`dylib::reserve_module_range`); stacks are committed out of it (drop decommits), the module range is released right before `LoadLibraryW`. Else a large GPU-driver reservation landing there gave "address range taken". Reservation failure falls back to per-slot reserve+commit. Linux: none (unhinted mmaps go top-down from high addresses).
- PI (`pi.rs`): ROM from CLI arg / `PW64_ROM` / `rom/*.{z64,n64,v64}` / `decomp/baserom.us.z64`. DMA is synchronous, then the `OSIoMesg` is posted. `osPi(Raw)ReadIo` store the word in ROM byte order (both callers use it as bytes). 0xFFB000..+0x40 (debug flags) reads 0. `vAddr` is truncated to 32 bits (`_uvDMA`'s `s32 dest` sign-extends 0x80xxxxxx).
- `headless.rs` (task-7 placeholders, logged on first call): SP tasks complete instantly (SP done, + DP done for gfx; `set_sp_task_hook`), controller 1 from `set_controller1` (idle by default; input.md), no EEPROM, AI accepts everything.
- **Task 7 (implemented, `hle.rs` in the `pw64` exe + `headless.rs`)**: gfx tasks → `pw64-gfx` `Interpreter` over the C address space (headless `osSpTaskStartGo` calls `set_gfx_task_handler`); `GfxMemory` implements `pw64-gfx`'s `Memory` with a `map` hook: host pointers (window/stacks/image) identity, everything else +0x80000000; RAM is host-LE (`read_u32` LE, `read_bytes` swaps u16 pairs — right for Mtx/Vp/Vtx s16 fields, `read_raw` = byte-exact for texels and byte fields: pw64-gfx reads `Vtx.cn` and `Light_t` through it).
  - HLE bounds: `GfxMemory` and `AudioMem` (audio.rs) check every access with `pw64_game::memmap::hle_readable` / `hle_writable`, not the whole 0x80800000..0xA0000000 window + exe..image span (that let a bad pointer hit a stack no-access/guard page or reserved-uncommitted memory, and let audio writes land in .text). Readable = RDRAM window, usable range of a live stack slot (`os::stack::in_live_stack`: slot index arithmetic + a per-slot `LIVE` bitmap set in `LowStack::new`, cleared in drop), exe image (`exe_range`, OnceLock), loaded module (`dylib::loaded_range`); NOT the module..exe gap. Writable = RDRAM + live stacks only. Failure behaviour unchanged (gfx: zeros + one warning; audio: zeros / dropped write + one warning). Linux ELF/.so images may have unmapped gaps between PT_LOAD segments inside the image range (not handled; no C data there). EEPROM = `pw64.eep` (`PW64_EEP`), `osEepromProbe`=1 → `saveModuleInit` runs. VI present: the retrace hook reads the latched fb as RGBA5551 320×240 → `tmp/fb_NNN.png`; gfx frames → `tmp/gfx_NNN.png` (`PW64_GFX_SHOTS`/`PW64_FB_SHOTS`, `PW64_STOP_MILESTONE=1` exits at the first).
- **Where it stops now**: **600 retraces complete, exit 0** ( milestones: "first framebuffer present at retrace 2" and "first gfx task rendered", ~100 commands/28 draws/152 tris/3 textures each; title-screen text renders recognisably, textures still red noise — renderer gaps, see renderer.md). The crash *after* the run (NVIDIA `vkGetInstanceProcAddr` at process teardown through RTSS's hook; exit 0xC0000005) is avoided by not dropping the wgpu state (`std::process::exit` in `pw64` main; `pw64-viewer` teardown is unaffected). Env notes: `DISABLE_RTSS_LAYER=1` (RTSS Vulkan implicit layer crashes at instance creation under gdb) and `WGPU_BACKEND=dx12` help on affected setups.
- **Interrupt delivery learned**: task completions are events (`osSetEventMesg`), which *drop their message on a full queue*; under fast idle-skipping the host loop can post several retraces per `deliver_due` while the scheduler consumes one per loop, the 8-slot interruptQ fills, and a dropped SP/DP-done permanently wedges `gSchedRspStatus` → 51-retrace RSP timeout → `_uvScDlistRecover` → `IO_WRITE` to unmapped RCP MMIO `0xA4040010` = 0xC0000005 (gdb + `llvm-symbolizer`). Fix: events are level-triggered (`Kernel::pending_events`, `flush_pending_events` in `deliver_due`); sched.c's recover path lost its `IO_WRITE` via a `pw64_log` patch (patches/src/kernel/sched.c.patch). VIDEO backlog is still coalesced by the full queue (harmless: the scheduler only needs the latest); honest fix later = virtual-time delivery pacing.
- **Where it stopped next (task 8)**: 600 retraces run clean and the flow goes **past the controller check into level init and rendering** — `tmp/gfx_*.png` show real terrain (island over sea) instead of red noise. Fixes this round:
  1. `gSiContStatus` → `gSiContStatus[4]` (system.c.patch): headless `osContInit` writes 4 × 4-byte statuses; natively the linker packed `gSiContPattern` right after it, and entry[0] clobbered the pattern → the title's `uvControllerCheckInserted(0)` stuck the game in replay_screen.c's warning `while(1)`. ROM had `UNUSED` slack there; natively it doesn't.
  2. Gfx union 16 → 8 bytes (see §5 bitfields) — the "2w" `PW64_SWAP` of dlists panicked on a 16-byte element.
  3. `uvJanimLoad` (anim.c) reads whole UVAN COMM/PART records raw — no swaps existed; count read BE garbage → `_uvMemAlloc` fell into `_uvMemOverAlloc` → returned 0 → `movss` to NULL in the key copy. Fixed with a new anim.c.patch: `PW64_SWAP(temp1, 1, UnkCommStruct, "5w h 2b")` for COMM (s16 `unk14` + pad at 0x14), `{2w}` header + `4w 2h` keys for PART. General rule: every block that C reads as whole records through a struct needs a swap; only scalar `uvConsumeBytes` reads are converted automatically.
  4. **ALParam pool stride** (synthesizer.c.patch): `alSynNew` allocates the update pool as `maxUpdates * sizeof(ALStartParamAlt)` (40 B) and links with that stride. Host `ALStartParamAlt` is 40 B (`next` + 8-aligned `wave`) vs `sizeof(ALParam)` 32; writing `update->wave` clobbered the next entry's `next`, the chain dereferenced arena `ALWaveTable`s → varied audio-thread crashes. (On the N64 both are 28 B.)
  5. **user_paths.c BE blocks** (user_paths.c.patch): `userPath_8034A4F8` reads COMM/QUAT/XLAT raw; byte-swapped counts → `_uvMemAllocAlign8` fail → NULL `unkC` → NULL write. Swaps mirror anim.c: COMM `PW64_SWAP(temp1, 1, UnkCommStruct, "5w h 2b")`, header `pw64_swap(x, 1, 8, "2w")`, entries `"6w"` (QUAT, 0x18) / `"4w"` (XLAT, 0x10). Dest `Unk8037DCA0_UnkC` bitfields are host-only, no swap.
  6. **`customFxParams` must be `static`** (audio_manager.c.patch): decomp UB — the array is declared inside `if (fxType == AL_FX_CUSTOM) { ... }`, `gALSynConfig.params` points at it, and `amCreateAudioMgr` → `alFxNew` reads it *after* the block ends (dangling pointer). IDO left the slot intact; clang -O2 legally drops the stores to the dead object, so the fx walk started from garbage (`section_count` 0x98) and read past the coroutine stack top (`read of 0x80d00004`, the next slot). -O0 kept the stores, so only release crashed. Fix: function-scope `static` → the table is baked into `.data` (verified: one occurrence in the image; `params` now points at a 0xc0xxxxxx `.data` address). Tools that made it visible: `pw64_rsp()` (current RSP from patched C, pw64-platform swap.rs), a temporary drvrnew probe logging `param`/`section_count`/`length` (removed), the VEH `[crash]` tracer. **Heisenbug rule**: a probe that *reads* a suspect local changes the optimizer's DSE view and makes the crash disappear.
  7. **Stale release binaries**: changing a patch (or `PW64_MAP`) does NOT rebuild `pw64-game` in release — delete `target/<profile>/build/pw64-game-*` and `.fingerprint/pw64-game-*` before comparing builds.
  8. **Patch context**: trailing-whitespace-only lines (`    `) in decomp sources defeat context matching — anchor hunks on non-blank lines only.
  9. **Hardware watchpoints** (`PW64_WATCH=<addr>`, crash.rs: DR0/DR7 armed by editing the CONTEXT RECORD of a raised benign exception) do not fire on machines with VBS/HVCI enabled (it filters user-context debug-register edits — verified: no hits even on a definitely-written global); kept for machines without virtualization-based security. Stack-slot map: boot -1→0x80800000, kernel 1→0x80900000, render 0→0x80a00000, sched 4→0x80b00000, app 6→0x80c00000, audio 3→0x80d00000 (created last, after `alInit`). (Old 1 MB slots; slots are now 0x110000 apart, stack = slot + 64 KB: `PW64_TRACE_OS` logs the ranges.)
- **Crash tooling** (`crates/pw64-platform/src/os/crash.rs`): installed at `os::boot`, prints `[crash] exception 0x... at RIP/RSP` + the read/write address to stderr and crash.log (stack buffer + raw Win32 writes, no allocation, once per process), then CONTINUE_SEARCH. Only real crashes: a VEH sees every thread's first-chance exceptions (drivers/overlays/CPU probes handle their own AVs/illegal instructions), so (a) `SetUnhandledExceptionFilter` reports anything unhandled on normal stacks, and (b) the VEH reports only error-severity codes (not 0xE06D7363) with RIP in the exe or game module (our code has no SEH handlers) or RSP on a coroutine stack. (b) is needed because the UEF is never reached from a coroutine: corosensei's unwind info crosses to the parent stack for backtraces only, the SEH dispatcher stops at the TEB stack limits (the coroutine's) and the second chance kills the process without the filter. Symbol resolution: set `PW64_MAP=<path>` when building — pw64-game's build.rs appends `/MAP:<path>` to the exported link-arg list (it must flow through `low_base_link_args`, since plain `cargo:rustc-link-arg` doesn't reach the exe), then match RIP `< base 0xC0000000` against the map's function list. Faster than gdb: works through coroutines and needs no extra tooling at the crash site. Also added a `[swap]` trace line to `pw64_swap` (PW64_TRACE_OS) showing the last layout/count before a fault.

## 9. Window, threading, scripted input (`crates/pw64`: `window.rs`, `main.rs`, `input.rs`)

- **Threads:** winit's event loop owns the main thread (Windows requirement). The OS core is thread-local (`KERNEL`, hooks), so the whole game (`run_game`: hooks + `os::boot` + `os::run`) runs on one `pw64-game` host thread, spawned from `resumed` after the window + wgpu device exist. `pw64-input` (gilrs) is a third thread; the audio output (cpal) has its own.
- **Present:** gfx task → `Frame` stored per target fb (`fb & 0x1FFFFFFF`); the retrace (or present tick, `PW64_FPS`) that latches that fb moves it to "shown" and into the `FrameSink` → `Arc<Mutex<Option<Arc<Frame>>>>` slot + `EventLoopProxy::send_event` → main thread `request_redraw` → its own `pw64_gfx::Renderer` renders into the surface (AutoVsync, Mailbox with a present tick; non-sRGB format, 4:3 letterbox done by the renderer). The game thread never blocks on the swapchain (only on a full op queue, renderer.md "Framebuffer persistence"). The game thread's `Hle` has a second renderer (same device) only for `PW64_DUMP_FRAMES`.
- **Exit:** window close → `QUIT`; the game thread's retrace hook sees it, sets `PARKED` and parks forever (no C or GPU work in flight); main waits ≤1 s for `PARKED`, then `process::exit` without dropping GPU state (NVIDIA teardown crash, §8). If `os::run` returns (deadlock) the game thread posts `Stopped(code)` and parks (TLS destructors would drop the OS core/GPU).
- **Headless** = `PW64_HEADLESS` or `PW64_MAX_RETRACES` set: runs on the main thread, no window, no PNGs unless `PW64_DUMP_FRAMES=<n>|<r1,r2,..>` (→ `tmp/frame_<retrace>.png`, 640×480). `PW64_GFX_SHOTS`/`PW64_FB_SHOTS` now default 0.
- **Scripted runs** (`PW64_INPUT_SCRIPT`, input.md): use `PW64_NO_THROTTLE=1 PW64_NO_INPUT=1` for speed/determinism. `crates/birdman64/scripts/fly_hang_glider.txt` reaches Beginner Hang Glider test 1 and flies (steering works); crashing into the cliff reaches the results screen (~retrace 3150). 4200 retraces clean.
- **Fixed on the way:**
  1. `test_summary.c` `func_8030C6A0`: `s16 sp5C[3]` but `textFmtInt` writes `dst[length]`, `dst[length+1]` → /GS cookie `__fastfail` (0xC0000409, no VEH report; found with msys2 gdb + `/MAP`). Patch: array `[3 + 2]`, length stays 3. Other `textFmtInt` callers' buffers are big enough.
  2. `hangGliderMovementFrame` uninitialised `sp5B` → `-ftrivial-auto-var-init=zero` (§5).
  3. `uvaSeqStop` (audio_seq.c) spins ≤2 s on `alSeqpGetState` expecting the audio thread to preempt; the cooperative scheduler never ran it → 3 × 2 s stalls per menu→level transition. Patch: `osYieldThread()` in the loop (checkpoint + switch to the higher-priority audio thread). **General rule:** any C busy-wait on another thread's progress needs a yield natively. Also snap.c's three 0.1 s `uvClkGetSec` spins (gfx task must finish before `uvCopyFrameBuf`; unyielded they also starved audio ~1 s per photo flow: 8 underruns → 0). title_screen.c:327 (`func_803434E8`, 1.0 s spin, no caller) yields too. Audit: no other clock/flag busy-waits in decomp app/kernel (libultra io/debug spins are replaced natively).
- **Crash triage recipe:** `PW64_MAP=<abs path> cargo build --release -p birdman64` (delete `target/release/build/pw64-game-*` + `.fingerprint/pw64-game-*` first if C didn't rebuild), run under `/c/msys64/mingw64/bin/gdb -batch -ex run -ex bt`, map RIPs `>= 0xC0000000` to the nearest preceding `Rva+Base` symbol in the map. Poor man's profiler: `gdb -batch -p <pid> -ex "thread apply all bt"` a few times. `0x406D1388` (SetThreadName) is filtered by the VEH.
- **Known renderer gap seen in flight:** a large black polygon under/behind the hang glider in chase view (likely a shadow/near-clipped primitive) — renderer.md.

## 10. Linux port (x86_64); macOS not possible as is

The whole workspace builds, lints and tests natively on x86_64 Linux (CI job
`linux`); `pw64` starts and gets to the ROM load (CI has no ROM, so a full game
run on Linux is not covered there). Build deps: zig 0.16.0 (get_zig example, needs
`tar` + xz-utils; legacy `PW64_CC=clang`: clang ≥16), `libasound2-dev`
(cpal), `libudev-dev` (gilrs), `libdbus-1-dev` (btleplug/BlueZ; BLE stays off by
default), `pkg-config`. Windows is unchanged (same C types, flags, link args).

- **Address space** (§2 option C, same map as Windows): RDRAM window and the
  coroutine stacks are `mmap(MAP_FIXED_NOREPLACE)` (+ address check: pre-4.17
  kernels take it as a hint); `pw64_game::memmap::image_range` = linker symbols
  `__executable_start`..`_end`.
- **Exe at 0xC0000000**: rustc links PIE and the kernel ignores a PIE's base, so
  the exe must be ET_EXEC. Link args (pw64-game build.rs, exported to
  pw64/build.rs like the PE ones): `-Wl,--no-pie -Wl,--image-base=0xC0000000`.
  Gotchas: `-no-pie` to the `cc` driver swaps in the non-PIC `crtbegin.o`, whose
  `R_X86_64_32S` relocs can't reach > 2 GB (link error) — so `--no-pie` goes to
  lld only and the PIC crt objects stay. rust-lld (rustc's default linker for
  x86_64-linux-gnu) rejects `-Ttext-segment`; GNU ld has no ELF `--image-base`
  (a GNU-ld user needs `-Ttext-segment=0xC0000000` instead). Same reason, the C
  is compiled `-fPIE` (not `-fno-pic`): non-PIC small-model code uses
  `R_X86_64_32S` absolute addresses, invalid above 2 GB; RIP-relative code
  works at any base. `readelf -h target/debug/birdman64` → `EXEC`, LOAD at 0xc0000000.
- **Host heap/main stack are high on Linux** (glibc brk is randomised up to 1 GB
  past the image; main-thread stack 0x7ff…), so `check_low_4gb` asserts them
  on Windows only. Nothing C-visible lives there: the C allocates from the
  window/arena, Rust passes it no heap buffers (its C-visible statics are in
  the image), and the C only runs on the low coroutine stacks.
- **C build** (`Flavor::Gnu` in build.rs): `zig clang` + `zig_flags(Os::Linux)`
  (`--target=x86_64-unknown-linux-gnu -fPIE -O3`, ELF objects, `zig ar`).
  Legacy `PW64_CC=clang`: `clang --target=x86_64-unknown-linux-gnu`
  (`PW64_CLANG`, else `clang` on PATH), GNU-spelled twins of every clang-cl flag
  (`GNU_FLAGS` ↔ `MSVC_FLAGS`, shared `WARN_FLAGS`, `-include` for `-FI`); cc's
  `-w` and `-fPIC` are filtered. Result: same 8 warnings, 0 pointer-width.
- **LP64 `long`**: `PW64_LONG32` (pw64_native.h) = `long` if `__SIZEOF_LONG__`
  is 4 (Windows: types unchanged) else `int`; used by
  `patches/include/libultra/PR/ultratypes.h.patch` (u32/s32/vu32/vs32) and
  gbi.h.patch (`Mtx_t`, `Hilite` alignment array, `Gsetcolor.color`, `TexRect`).
  Every other decomp `long` is in unused headers (ramrom.h, dbgproto.h, IDO
  stdarg/stdlib). `ldiv` (utils.c RANLUX): shadow stdlib.h maps it to a 32-bit
  inline on LP64. `size_t`/`intptr_t`/`ptrdiff_t` use `__SIZE_TYPE__` etc.
- **Layout audit (bitfields and all)**: every TU compiled for both targets with
  `-fsyntax-only -Xclang -fdump-record-layouts-simple` (flags from each
  `c-flags.txt`) and all records diffed (size, alignment, field *bit* offsets).
  The only difference: snap.h `Unk8033F050` (MSVC 0x1C, SysV 0x18 — GCC packs
  bitfields across declared types like IDO, still LSB-first); snap.c.patch
  already converts it field by field, so only `pw64_layout_checks.c` needed an
  `#ifdef _MSC_VER`. Gfx (gbi.h.patch), `UnkPartStruct_Unk8` (reversed u16
  bitfields: same-type units, so identical LSB-first layout), Acmd, OSTask: same.
  `va_list` is a struct array on SysV; all decomp uses are local (string.c,
  dobj.c), so no by-value hand-off hazard. Re-run the diff after adding
  bitfield structs.
- **Crash tracer**: `os/crash_linux.rs` — SIGSEGV/BUS/ILL/FPE handler
  (`SA_ONSTACK`), prints `[crash] signal N at RIP/RSP, fault address`, restores
  the previous handlers and returns (the fault re-raises into Rust's
  stack-overflow reporter or the default action). No `PW64_WATCH`.
  `PW64_MAP` → `-Wl,-Map=<path>`.
- **Unwinding**: stub panics unwind through the C frames (clang emits
  `.eh_frame` by default on x86_64 Linux; don't add `-fno-asynchronous-unwind-tables`).
  Pinned by pw64-game test `boot_without_rom_unwinds_through_c`: real `bootproc`
  → Kernel → App thread → `uvSysInit` → first `osPiStartDma` panics "ROM not
  loaded" out of `os::run` (runs on both targets, no ROM needed).
- **macOS/arm64**: `__PAGEZERO` forbids mappings below 4 GB, so neither the
  window nor a low image works: needs option A (widened Gfx) or i686 (option D)
  first — §2. build.rs / stack.rs / memmap.rs fail with a clear message there.

- **EEPROM robustness (headless.rs)**: a non-NotFound read failure of
  pw64.eep (sharing violation, offline OneDrive placeholder, permissions)
  latches `EEP_READ_FAILED` for the session: EEPROM writes still update
  memory and return success (the game must not save-retry-loop) but never
  touch the file, so an unreadable save cannot be wiped by a zero-image
  rewrite. First file write of a session copies the on-disk image to
  `<eep>.bak` (overwrite). `write_atomic` now `sync_all`s the temp file
  before the rename and retries rename failures up to 10 times over ~2 s
  (PermissionDenied / raw OS errors 32, 33); every EEPROM write persists the
  full image, so the save self-heals after a failed write. Windows test
  trick: hold the target with `OpenOptions::share_mode(0)` to force a
  sharing violation; on Unix renames over open files always succeed.
- **Platform toasts**: `pw64_platform::headless::notify_toast(String)` queues
  player messages (save problems); `take_toasts() -> Vec<String>` drains
  them, oldest first; the pw64 window drains it per frame into its toast UI
  (settings.md).
- **Clock stall absorption (time.rs)**: `now()` compares the wall count
  against the previous read (`last_read` atomics; `now` stays `&self` so
  `interrupt_due(&self)` keeps compiling) and absorbs any jump over
  `STALL_COUNTS` (250 ms) into `stalled`, subtracted like `paused`. The
  baseline must also refresh in the frozen branch of `now()` AND in
  `unfreeze()`: the unfreeze recompute (`paused = wall + skipped - f -
  stalled`) already absorbs the paused/stalled time, and without the
  baseline refresh the next read would absorb it a second time and move the
  count backwards. `skip()` must NOT refresh the baseline (wall did not
  move). Sub-threshold slow frames (< 250 ms) are still real time; a host
  that cannot deliver a frame in 250 ms starves the count by design.
