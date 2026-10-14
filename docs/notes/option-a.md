# Option A: C-visible memory anywhere (no fixed low-4 GB window)

Goal: drop the fixed RDRAM window (0x80000000), fixed exe base (0xC0000000) and module
base (0xB0000000), so RDRAM, stacks and the image may live anywhere (macOS arm64,
Android). Game output must stay bit-identical to the fixed-window build at every step.

## Decisions

- **Keep 32-bit addresses ("vaddr") in the C; translate at the conversions.** Every
  int↔pointer conversion is already explicit (`PW64_U32`/`PW64_PTR`; the build has
  `-Werror` on int/pointer casts and `shorten-64-to-32`). These now call Rust
  `pw64_to_vaddr`/`pw64_to_host` (pw64-platform `vaddr.rs`), which map registered
  host blocks to vaddr ranges. No region registered = identity = today's build.
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
  model. Planned env `PW64_RELOC=alias|strict` (step S4/S5) selects relocation.
  Remove the fixed window/base code once strict mode passes the full sweep
  (sweep.md) on Windows + Linux and has shipped in one release.

## Inventory (2026-10, before S2)

Routed by the layer already (occurrences of `PW64_U32|PW64_PTR|K0` macros in the
patched tree): graphics.c 18, memory.c 17, gbi.h 15, geometry.c 12, reverb.c 10,
load.c 9, filesystem.c 7, texture.c 6, os_convert.h 6, system.c 4, bnkf.c 3,
audio_manager.c 3, toys.c 3, resample.c 2, env.c 2, R4300.h 2, pw64_arena.c 2,
1 each: save.c heapinit.c cseq.c sched.c font.c debug.c snow.c game.c
pw64_rom_structs.c pw64_audio_load.c. No per-site work unless strict mode trips.

**Hidden sites** (int *constant* → pointer: clang doesn't flag these):
- graphics.c 105-107 `gGfxFbPtrs`, `D_80299278` (3) — **done (S1 template)**.
- user_file.c 33 (1), sched.c 232 `osViSwapBuffer((void*)0x80100000)` (1),
  system.c 227/231 `bootproc` clear loops (4 literals).
- memory.c 41-42 `D_803805E0` / `kernel_TEXT_START` defines, 72/77/81 clear loops.
  The `D_802B8830[]` / `D_802B892C` / `osMemSize + 0x80000000` u32 literals stay.
- debug.c 634/654 (2); audio_manager.c 41 `gAudioHeapStart` (static initializer:
  `PW64_PTR` is a call, so assign it in the init function before line 63).

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
- S3a: pw64-platform consumers: `osVirtualToPhysical` = `to_vaddr(p) - K0`;
  pi.rs 265 = `to_host(to_vaddr(p))` (normalises sign-extended s32 and host);
  mio0 bound via `to_vaddr(src)`; vi fb → keep host, convert at the consumer.
- S3b: hle.rs/audio.rs: `map()` returns the vaddr (`raw | K0` if bit 31 clear), reads
  call `to_host`; `data_ptr`/`framebuffer` via `to_vaddr`; audio `to_host(addr + K0)`.
- S4: `PW64_RELOC=alias`: one shared-memory RDRAM block mapped twice (low at
  0x80000000 for unconverted sites, high above 4 GB registered as the region;
  Windows `CreateFileMapping` + 2 views, Linux `memfd_create` + 2 `mmap`);
  `hle_writable` uses the registered range. Gate: identity in alias mode too.
- S5: `PW64_RELOC=strict`: low view reserved no-access; every straggler faults
  (crash handler symbolises it). Gate: identity + all sweep scripts.
- S6: stacks block anywhere above 4 GB, region 0x80800000. S7: image regions;
  drop `/BASE`, fixed module base and their checks. S8: make relocation the default,
  delete the window/reservation code.

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
- Not in static initializers (it is a call): assign at init instead.
- Rust side: `pw64_platform::vaddr::{to_host, to_vaddr}`; never `as u32` a host
  address or `as usize` a C address.
