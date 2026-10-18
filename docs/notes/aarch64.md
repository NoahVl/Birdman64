# aarch64 Linux (arm64)

Status: the workspace builds and lints for `aarch64-unknown-linux-gnu`; the tests pass under
qemu-user, cross-built on x86_64 Linux. Not yet run on arm64 hardware or with a ROM.

- **Relocatable only.** pw64-game build.rs sets the cfg `pw64_reloc` when the `reloc` feature is
  on or the target is aarch64. That means `RELOC_BUILD`, a PIE exe, no low-base link args,
  `PW64_RELOC` defaulting to `on`, and `off` refused. rustc links aarch64 with the system
  `cc`/GNU ld, which has no `--image-base`.
- **pw64-cbuild `Os::LinuxArm64`:** `--target=aarch64-unknown-linux-gnu -fPIE` plus the same
  COMMON flags. The module link is the Linux one, always without `--image-base`, and gets its own
  flags hash and cache key. get_zig knows the aarch64-linux tarball.
- **Shim thunk:** `adrp x16, slot; ldr x16, [x16, :lo12:slot]; br x16`. x16 (IP0) is free to
  clobber at a call. A test link of the module had no text relocations, only RELATIVE ones.
- **Registers:** crash_linux.rs reads PC/SP from `uc_mcontext.pc/.sp`; `pw64_rsp` uses `mov sp`.
- **39-bit VA kernels** (user space ends at 512 GB): the RDRAM/stack candidates
  (0x7_2000_0000 / 0x7_4000_0000 + k·4 GB, all below 96 GB) fit. The strict-mode `high_heap`
  falls back to 128..384 GB when the 1 TB+ candidates fail.
- **Layout diff x86_64 vs aarch64** (207 TUs, 10221 records): only the builtin `va_list` record
  differs (audio_event.c, dobj.c, string.c, all local uses).
- `c_char` is u8 on aarch64, so `c_char as u8` trips clippy `unnecessary_cast` there.
- **Cross-test recipe** (x86_64 Linux, no root):
  - Linker: the full zig tarball's `zig cc -target aarch64-linux-gnu.2.31`. The binary-only
    tools/zig copy can't link. Wrap it to expand `@rsp`, drop `-fuse-ld=*`, `-B*` and
    `--fix-cortex-a53-843419`, rewrite `-Wl,<x>.a` → `<x>.a` and `-lgcc_s` → `-lunwind`.
  - Point `CC_aarch64_unknown_linux_gnu` at the same wrapper (`ring` in `first-run`), and set
    `PKG_CONFIG_ALLOW_CROSS=1`.
  - Libraries: arm64 alsa/udev/dbus (+ systemd, lzma, lz4, gcrypt, gpg-error) `.deb`s from
    ports.ubuntu.com, unpacked with `dpkg -x`; `-L` to `lib*.so` symlinks. glibc comes from
    `libc6-arm64-cross`.
  - Runner: `qemu-aarch64-static -B 0x100000000000 -L <sysroot>`.
- **qemu-user artifacts** (these are not arm64 bugs):
  - Without `-B`, qemu's own heap (~0x7b000000) grows into the low stack window, so strict
    reservations fail.
  - `os::tests::gfx_task_finishes_before_the_next_retrace` reads the wall clock and fails when
    run in parallel (it passes with `--test-threads=1`).
  - `restart_now_failure_keeps_the_banner` fails because qemu reports a spawn of a missing exe
    as a success.

- The arm64 thunks have no `bti c` landing pad: building the exe or module with `-mbranch-protection` (BTI) would make `br x16` fault; add the pad first if that is ever enabled.

## Darwin (macOS/iOS)

- Compile-checked only from Windows (`cargo clippy --target aarch64-apple-{darwin,ios,ios-sim}` works: zig compiles the C to Mach-O, no SDK needed for check). Never run yet: the macOS CI job is the first real test.
- `pw64_reloc` is forced on for every apple target (build.rs); `PW64_RELOC` other than `on` is refused (`Reloc::parse`): no low windows (`__PAGEZERO` = low 4 GB), no memfd, so off/alias/strict can't exist.
- Darwin has no `MAP_FIXED_NOREPLACE` and `MAP_FIXED` clobbers: `pw64_platform::MAP_FIXED_NOREPLACE` is 0 there, so every "fresh reservation" is an mmap hint plus the existing `address == hint` check. Re-mapping a slot inside our own reservation still uses `MAP_FIXED`.
- iOS has only a few GB above 4 GB: when the fixed RDRAM/stack candidates (0x7_2000_0000+) fail, `pw64_platform::mmap_anywhere` takes a kernel-chosen block (64 KB aligned by over-reserving and trimming). The vaddr layer needs only host > 4 GB, no particular base.
- `LowStack::new` on apple lazily relocates the window (kernel-chosen) if memmap didn't, so the OS-core unit tests run on a Mac without init.
- Exe image range = Mach-O load-command walk from `_mh_execute_header` (`os/crash_apple.rs::image_extent`, skips `__PAGEZERO`/`__LINKEDIT`); the crash tracer there is a no-op.
- pw64-cbuild: `Os::{MacArm64, Ios, IosSim}` (`aarch64-apple-macos11`, `-ios15`, `-ios15-simulator`, + `-mno-outline-atomics`); `Os::from_target_abi` picks the simulator by `CARGO_CFG_TARGET_ABI=sim`. The first-run module (`build_module`, `dylib` feature) refuses apple until the module path is ported to Mach-O (shim thunks).
- Still to verify on a Mac: the layout diff vs aarch64 Linux (only `va_list` may differ), `cargo test --workspace`, and that `_mh_execute_header` resolves in test binaries.
