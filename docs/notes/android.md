# Android port (arm64): feasibility and plan

Status: not started. Design notes only. Target is **arm64-v8a**. Distribution is an
APK on GitHub releases, not Google Play, so Play's rule against downloaded or locally
built executable code doesn't apply. Goal: one codebase. Android-specific code sits
behind `cfg(target_os = "android")`, the same way the Windows and Linux splits work.

## What carries over

- **Game C:** arm64 is little-endian and LP64, the same as x86_64 Linux, so the
  pointer, `long` and layout patches apply unchanged. `-funsigned-char` and
  `-ffp-contract=off` are already in the shared flags (`pw64-cbuild`). Without them,
  arm64 clang would treat `char` as signed differently and fuse mul-add, which changes
  float results. Re-run the record-layout diff (native-build.md §10) for
  `aarch64-linux-android`.
- **Rust:** wgpu (Vulkan, GLES fallback), winit 0.30 (android-activity), cpal (AAudio),
  egui and corosensei all support aarch64 Android. pw64-gfx, pw64-audio, the formats
  code and the OS core have no target-specific code.
- **Linux platform code:** `mmap(MAP_FIXED_NOREPLACE)` stacks and window, the signal
  crash tracer (register names need arm64 variants: PC/SP instead of RIP/RSP).

## Address space

The C needs every C-visible address to be < 4 GB with bit 31 set (native-build.md §2
option C). This comes from the C's data model, not from linking: display lists, ROM
structs, the allocator and the filesystem hold addresses in 32-bit fields, there are
39 literal `0x80xxxxxx` addresses, and `(u32)p & 0x80000000` decides RAM vs ROM
offset. The heap, stacks and framebuffers are bound by this as much as code and
globals, so dynamic linking alone doesn't remove it.

Two ways forward:

| | Reserved low range (option C on Android) | 64-bit pointers (option A) |
|---|---|---|
| Idea | Keep the current model; reserve 0x80000000..0xC0000000 at startup, load the module into it | C uses real pointers; RDRAM is a block allocated anywhere |
| Work | Small: arm64 thunks, reservation, `android_dlopen_ext` | A few weeks (list below) |
| Risk | ART keeps its heap/boot image in the low 4 GB; the range (1 GB of address space, not RAM) may be taken on some devices or versions | Hidden pointer-in-u32 sites the compiler can't flag |
| Unlocks | Android only, maybe | Android, macOS, iOS; drops the fixed exe base, reservations and module base on every platform |

**Recommendation: option A**, done first on desktop, where today's build is a
bit-identical reference (frame dumps, sweeps). A low reservation in a process ART
already manages is too fragile to rely on across devices.

### Option A work list (from the §2 hazard inventory)

1. ~160 compiler-flagged int↔pointer sites (now zero-extending `PW64_U32`/`PW64_PTR`)
   → real `uintptr_t`/pointers.
2. 39 literal RDRAM addresses → `rdram_base + offset` (one host allocation, anywhere).
3. `_uvMemAlloc` (returns `s32`), filesystem `dataInfo->address`, the memory map,
   `gGfxFbPtrs`, `uvFileReadHeader(s32)` → 64-bit.
4. ROM vs RAM bit-31 test (memory.c, filesystem.c) → explicit flag or range check.
5. Display lists: widen `Gfx` to 16 bytes (SM64-port style) or keep 32-bit
   **offsets** that the renderer adds a base to. Convert ROM dlists on load (UVTX
   `setimg` patching, `OS_PHYSICAL_TO_K0` sites in texture/geometry/sprite.c).
6. ROM structs with pointer slots (ParsedUVEN, UnkUVMD_24, font `Bitmap[]`, audio
   `alBnkfNew`/`alSeqFileNew` in-place patching) → layout-aware loaders, which mostly
   exist already for endianness.
7. Rust side: the SP/audio emulation (`hle.rs`, Acmd addresses), the `pw64-gfx`
   `Memory` mapping and `OSTask` reads → base translation. Re-check `pw64-kernel`
   ports that touch addresses.
8. Verification: frame-dump and audio-WAV identity against the option C build,
   full gameplay sweep. Raw `sizeof` copies and arithmetic on stored addresses are
   the sites to expect bugs in.

With option A, Android still loads the game as a module (first-run layout), but with
a plain `dlopen` at any address. The rest of this section describes the option C
path, kept for reference.

**Dynamic linking under option C:** yes, but the loader must not choose the address.
- `android_dlopen_ext` with `ANDROID_DLEXT_RESERVED_ADDRESS` (API 21+) loads a
  normal PIC `.so` into a range we `mmap`-reserve first, e.g. 0xB0000000. It's still
  ordinary dynamic loading: the bionic linker applies relocations and
  `dlsym` works. Android rejects text relocations (API 23+), so the module is linked
  PIC/shared, not `/FIXED`-style. The base then comes from the reservation, not the
  link address. Check the loaded base, as `dylib::load` does.
- A plain `dlopen` would place the module high, so C globals would get addresses
  ≥ 4 GB and truncate. That only becomes possible after widening Gfx and the
  allocator (§2 option A), the same work macOS would need.
- The module could import symbols straight from `libmain.so` (ELF data imports go
  through the 64-bit GOT, so the MSVC dllimport problem doesn't exist here). Keep the
  thunk table anyway: it's shared with Windows and Linux, and the module stays
  import-free. The thunks (`pw64_dll_shim.c`, `jmp *pw64_imp_x(%rip)`) need an arm64
  version: `adrp x16, slot; ldr x16, [x16, :lo12:slot]; br x16`. x16/x17 are the
  AAPCS64 veneer registers, so the thunk clobbers nothing the callee expects.
- **Audit:** nothing Rust passes to C may live in `libmain.so` statics or the Rust
  heap. Both are high on Android, and they were only safe on Windows because the exe
  was low. The module design already defines the C-read data in the shim. Check every
  thunk argument and return value that is a pointer.
- **Risk (check first, on a real device):** ART puts its heap and boot image in the
  low 4 GB for compressed references. Reserve 0x80000000..0xC0000000 (RDRAM window,
  coroutine stacks, module) as early as possible in `android_main`, and check whether
  that range is free on a few devices and Android versions. If it isn't, the window,
  stacks and module would need another < 4 GB range with bit 31 set, and that range
  only spans 0x80000000..0xFFFFFFFF.

## Building the game module

- **On device:** zig ships `aarch64-linux` builds (~37 MB xz). Android 10+ blocks
  `exec()` of files in app data, so ship zig in the APK as `lib/arm64-v8a/libzig.so`
  and run it from `nativeLibraryDir`. Check that `dlopen` of a module written to app
  storage still works on current Android versions. Use the same download, hash check,
  patch and cache flow as the desktop. Compile time on a phone is unmeasured.
- **Fallback:** the desktop exe builds an Android module with
  `--target=aarch64-linux-android`, and the player copies it over. Simpler and no
  zig in the APK, but a worse experience for beginners.

## Frontend work (new code)

- **Gamepads:** gilrs has no Android backend. Read key and motion events from
  android-activity and feed pad slots in the same place gilrs does. BLE pads: skip
  (btleplug needs Java glue).
- **Touch controls:** on-screen stick + A/B/Z/R/C/Start overlay (egui or a dedicated
  pass), hidden while a pad is connected. Needed for phones without a controller.
- **ROM / files:** no rfd. Pick the ROM through the Storage Access Framework
  (`ACTION_OPEN_DOCUMENT`, JNI), copy it into app storage. Data dir = internal files
  dir. No display modes, F11 or Alt+Enter.
- **Lifecycle:** on `Suspended`, pause the OS core (settings pause model), drop the
  surface and flush the EEPROM (the process may be killed). On `Resumed`, recreate
  the surface. Handle rotation (landscape lock) and display cutouts (fill-screen
  safe area).
- **Packaging/CI:** GameActivity (or NativeActivity) + cargo-ndk + Gradle (or xbuild).
  A CI job that builds and lints `aarch64-linux-android`. Debug-signed APK as a
  release asset.

## Order

1. **Option A on desktop** (Windows/Linux x86_64), verified bit-identical against the
   current build.
2. **aarch64 Linux** (Raspberry Pi 5 / ARM CI runner): proves the C on arm64, the
   coroutine stacks and the module loader with no Android tooling. All of it is reused
   on Android.
3. **Android, pad only, module built on desktop:** proves wgpu, cpal and the lifecycle.
4. **On-device build, touch controls, SAF ROM picker:** usable by beginners.
