# First-run build ("build on first run")

The release zip holds only our code, a bundled compiler (zig) and licence
notices. On first start the exe downloads the upstream decomp at a pinned,
hash-checked commit, applies our patches, compiles the C on the player's
machine into a **game module** (DLL/.so) and loads it. Later starts load the
cached module. Dev builds are unchanged (`cargo run -p birdman64` = static link).

Spike (2026-09-30, Windows) proved it end to end: see "Spike results".

## Decision: game module at a fixed base, no imports (option 2 + thunks)

The exe (Rust: pw64-platform/gfx/audio/frontend; **no pw64-kernel**) stays
linked at 0xC0000000. The C is compiled locally into `pw64game.dll` /
`libpw64game.so`, linked at **0xB0000000** (bit 31 set, < 4 GB, between the
coroutine stacks ending at 0xA0000000 and the exe), loaded with
`LoadLibraryW` / `dlopen`, entry points via `GetProcAddress` / `dlsym`.

- **The module imports nothing from the exe.** Every Rust function the C calls
  (51 names, `crates/pw64-game/dylib/pw64_dll_imports.h`) is defined in the
  module by `pw64_dll_shim.c` as a jump thunk `name: jmp *pw64_imp_name(%rip)`;
  the exe fills the slots via the exported `pw64_dll_bind(lookup)` from a
  build.rs-generated table (`$OUT_DIR/dll_imports.rs`). So: no import lib naming
  `birdman64.exe` (renaming the exe is harmless), no `/EXPORT`/`-rdynamic` on the
  Rust exe, no data imports (the MSVC dllimport problem), identical code for
  PE and ELF. Tail jumps build no frame: unwinding is unaffected (tested).
- Data the static build takes from pw64-platform (`osTvType`, `osMemSize`,
  `osViModeTable`, ucode symbols) is defined in the shim; nothing Rust-side reads
  it. Keep values in sync (os/misc.rs, stubs.rs).
- MSVC-target bits the CRT would supply: `_fltused`, `__security_cookie` +
  `__security_check_cookie` (/GS kept for parity) in the shim. Module links no
  CRT (`-noentry -nodefaultlib`). The spike imported `memcpy memset strlen
  ldiv powf sqrtf` from `ucrtbase.dll` (Win) / the exe's libc (Linux); they
  now go through thunks to Rust too (`C(name)` entries), so the module has zero
  imports and uses the exe's CRT (same `powf` on every machine; Linux: no
  `--as-needed` risk of libm being dropped from the exe).
- `/FIXED` (no .reloc): if 0xB0000000 is taken, LoadLibrary fails instead of
  relocating; `dylib::load` also checks the base. Linux: glibc passes the
  first PT_LOAD vaddr as the mmap hint (current glibc: always; 2.31: only when
  the main exe is non-PIE, which ours is; verified in WSL 2.31).
- Exe side (`pw64-game` feature `dylib`, `src/dylib.rs`): `bootproc` is a Rust
  trampoline with the same name/type, so `pw64` main.rs needs no change;
  `memmap::image_range()` covers module + exe (hle.rs/audio.rs map it 1:1).
  ABI check: module exports `pw64_dll_abi()`, must equal `dylib::DLL_ABI`.

Why not the others:
- **(1) Link the whole exe on the player's machine** (Rust shipped as a
  staticlib): needs the MSVC CRT/SDK import libs (not redistributable) or a
  switch of the release to `x86_64-pc-windows-gnullvm` (second Windows target
  to keep green: wgpu/cpal/gilrs/btleplug/rfd on mingw, libunwind vs SEH,
  raw-dylib inside a staticlib); Linux would need `-lasound -ludev -ldbus`
  dev symlinks or stub .so files at link time. Bigger download (unstripped
  staticlib), slower link, more failure modes. Rejected.
- **(2) plain DLL importing from the exe** (import lib + `/EXPORT`): works for
  functions, but C data refs to exe data need `__declspec(dllimport)` (no
  auto-import outside mingw), and the import names the exe file. Thunks
  remove both problems at the cost of one indirect jump per platform call.
- **Own ELF/PE loader:** only needed if a platform ever ignores the base hint.
  Kept as the Linux fallback idea (the module has no symbol relocs).

## Toolchain: zig 0.16.0 (MIT; bundles clang/LLD 21)

- Only `zig.exe` is needed (no `lib/` dir): hidden subcommands `zig clang`
  (plain clang driver, not `zig cc`: no zig defaults, no VS/SDK probing, no
  cache), `zig lld-link`, `zig ld.lld`, `zig dlltool`. We use `-nostdinc`
  and link no libc, so no headers/libs are needed.
- Win: https://ziglang.org/download/0.16.0/zig-x86_64-windows-0.16.0.zip,
  sha256 `68659eb5f1e4eb1437a722f1dd889c5a322c9954607f5edcf337bc3684a75a7e`,
  97 MB; zig.exe 177 MB, 55 MB deflated, 37 MB xz.
  Linux: `zig-x86_64-linux-0.16.0.tar.xz`, sha256
  `70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00`, 55 MB;
  zig 173 MB, 36.6 MB xz.
- Why not LLVM's own binaries: clang.exe 110 MB + lld 75 MB > zig.exe alone.
- `zig cc` gotchas (why we don't use it): keeps frame pointers, drops
  `-fstack-protector-strong` for msvc, probes the VS install, `-shared`
  ignores `--image-base`, rejects `-Map`.

## Flags (must equal the static build: `c-flags.txt`)

Windows = clang-cl `-MD -O2` (cc1 `-O3`, /GS strong, /Gy, /Oy) in GNU spelling,
checked by diffing `-###` cc1 args (only diffs: `_MSC_VER` minor version,
cosmetic flags):
`zig clang --target=x86_64-pc-windows-msvc -D_MT -D_DLL -fstack-protector-strong
-ffunction-sections -fomit-frame-pointer -nostdinc -std=gnu11 -funsigned-char
-fwrapv -fno-strict-aliasing -ffp-contract=off -ftrivial-auto-var-init=zero
-fgnuc-version=4.2.1 -O3` + WARN_FLAGS + `-include native/pw64_native.h` +
DEFINES + includes (build.rs lists). Linux: `--target=x86_64-unknown-linux-gnu
-fPIE` + the same COMMON (identical to the static Linux build's code).
No `-g`: builds are deterministic (`-Brepro`), so a dev can rebuild the exact
module to symbolize a crash; the builder keeps the `.map` anyway.
**ported.txt is ignored**: the release compiles the C originals (pw64-kernel
ports are decomp translations and are not shipped). Dev builds keep the ports
(`static` feature); kernel_diff proves them bit-identical, so behaviour matches.

Link (Windows): `zig dlltool -m i386:x86-64 -d ucrtbase.def -l ucrtbase.lib`;
`zig lld-link -dll -noentry -nodefaultlib -machine:x64 -Brepro -base:0xB0000000
-fixed -dynamicbase:no -highentropyva:no -opt:ref -opt:icf -export:bootproc
-export:pw64_widescreen_aspect,DATA -export:D_802B892C,DATA -map:… -out:… *.obj
ucrtbase.lib` (shim exports via `__declspec(dllexport)`). Linux: `zig ld.lld
-shared -Bsymbolic --version-script=<exports> --image-base=0xB0000000 -z
noexecstack -Map=… *.o` (no libc linked; `-Bsymbolic` + `local: *` let the
`-fPIE` objects link into a .so). Reference script: `dylib/spike_build.sh`.

Gotchas: Git Bash rewrites `/FLAG` args to paths (use `-flag`); zig on
Windows wants `C:/…` paths; use a response file for the flags (quoted,
forward slashes); spaces + non-ASCII dirs tested OK (rsp + lld-link). A
future >4 KB stack frame makes clang emit `__chkstk` (MSVC CRT): provide it
in the shim if the link ever reports it. GNU `patch` rejects some of our
patches (asymmetric context): always use our applier.

## Decomp download

- `https://codeload.github.com/gcsmith/Pilotwings64Decomp/zip/<commit>` (what
  `github.com/…/archive/<commit>.zip` redirects to), commit = submodule
  `2529dabfb5f5776818e78d06f7976631d1823fbe`, ~1 MB, 607 entries, one symlink
  (`tools/diff.py`: skip symlinks). Archive content = submodule (CR-stripped).
- Pin **per-file SHA-256** (manifest of the files we use, CR stripped), not the
  zip hash: GitHub regenerates archives and their bytes are not guaranteed.
- Extract only `src/{kernel,app,libultra/audio,libultra/sp}` + `include/`.

## Patches: legal note

Our 73 `.patch` files (3471 lines) carry **1853 context + 277 removed decomp
lines**, i.e. ~2100 lines of decomp text: too much to call "our code". Ship
them in a **context-free "ops" form** generated at build time: per file, a
list of `=n` (keep n old lines), `-n` (drop n), `+text` (insert). The pristine
file is pinned by the manifest hash, so no context is needed for safety. Only
`+` lines remain (902, ours; some restate a decomp statement with a cast or
reordered bitfields: minimal, unavoidable, listed by the kit test). Also ours
and shipped: `native/` (shadow headers incl. `segment_symbols.h` = ROM
offsets/addresses, facts), `dylib/`. The pw64-platform libultra functions are
API reimplementations (`mirrors osSendMesg` = behaviour, not translated
code); pw64-formats/pw64-audio-data parsers: format knowledge. See "Legal
audit" below.

## Layouts

Release zip (`Birdman64-windows-x64.zip`, ~60 MB; ~42 MB if zig ships as xz):
```
Birdman64/
  Birdman64.exe            # first-run feature, gui-subsystem, crt-static; kit embedded
  READ ME FIRST.txt        # player quick start (wording matches paths.rs)
  toolchain/zig/zig.exe    # zig 0.16.0, unmodified
  toolchain/zig/LICENSE    # zig (MIT)
  licenses/LLVM-LICENSE.txt  # zig.exe contains LLVM/clang/LLD (Apache-2.0 WITH LLVM-exception)
  LICENSE  README.md  CHANGELOG.md  THIRD_PARTY_LICENSES.html   # ours; cargo-about for Rust crates
```
Build kit embedded in the exe: `native/**`, `dylib/*`, ops patches,
manifest. Not shipped: decomp licence (it arrives with the download).

Cache (compiled module = cache, not user data): portable `<exe dir>/cache/` if
writable, else `%LOCALAPPDATA%\Birdman64\cache` / `$XDG_CACHE_HOME/birdman64`
(via `crates/birdman64/src/paths.rs`):
```
cache/decomp/<commit>/{src,include}/…, .complete     # verified extract, 4 MB
cache/build/<key>.tmp-<pid>/{tree,obj,flags.rsp,build.log}  # deleted on success
cache/game/<key>/pw64game.dll|libpw64game.so, pw64game.map, c-warnings.log, build.json
cache/zig-cache/                                     # ZIG_*_CACHE_DIR (zig clang writes none)
```
`key` = `DLL_ABI` (= exe version + hash of imports list/shim/kit) + zig
version + decomp commit. Finished module moved into place atomically; old
keys deleted after a successful build.

## Steps + UX

1. ROM found/verified (rom_setup.rs) → 2. module for `key` present? load, done
→ else 3. progress window: "Preparing the game (first start only)": find
zig (0%), download (5-15%), verify + extract (20%), apply patches (25%),
compile 204 files (25-95%, per file), link (97%), load (100%) → game starts in
the same window. Child processes: `CREATE_NO_WINDOW` (else a console flashes
per compile under gui-subsystem), below-normal priority, jobs =
available_parallelism. Headless/CI: `--build-game` (or `PW64_BUILD_ONLY=1`).

Failure → message box (rfd), log path in every message:
- network: "Couldn't download the game source from GitHub. Check your
  internet connection." [Retry] [Quit]; 3 automatic retries (1 s, 4 s), 60 s
  timeout, honours HTTPS_PROXY; OS root store (corporate TLS proxies).
- hash mismatch: "The downloaded source doesn't match the expected version
  (<file>). Retry, or report it with the log."
- zig missing/blocked: "The bundled compiler (toolchain/zig/zig.exe) is
  missing or was blocked, often by antivirus. Re-extract the zip or allow it."
- compile/link error: "Building the game failed. Please report it with the
  log: <path>."
- base taken / load error: "Couldn't load the game module at its fixed
  address (0xB0000000): another program's overlay may be in the way."
- no writable cache dir / disk full: say which dir and how much space (~20 MB
  + zig 177 MB already in the install dir).
No admin rights needed (user dirs only). Antivirus: the fresh unsigned DLL
may be scanned on load (fine); SmartScreen doesn't apply to CreateProcess'd
zig or a LoadLibrary'd DLL.

## Linux (AppImage + tar.gz)

zig in the AppDir (`usr/lib/birdman64/toolchain/zig/zig`, squashfs-compressed),
module in `$XDG_CACHE_HOME`. dlopen needs the cache on an exec-mountable fs
(error message if `noexec`). AppImage needs libfuse2 (missing on Ubuntu 22.04+:
document `--appimage-extract-and-run`; the tar.gz is the fallback). Build the
exe on an old-glibc runner. The .so needs no libc symbols at all.

## Android (out of scope)

Nothing here blocks it: the runtime/module split and thunk binding are OS
neutral; Android would need its own way to obtain the module (on-device
compiler or ROM recompile).

## Spike results (16 threads, Win 10)

- Download 1 MB < 1 s; patch overlay 1 s; compile 204 files 6-8 s parallel
  (22 s serial); link < 1 s; **total ~8 s**. Same 8 warnings as the static
  build. DLL 903 KB (+ 415 KB map), imports only ucrtbase; exports bootproc,
  pw64_dll_abi/bind/range, 2 data. Linux (WSL, zig ld.lld): 2 s compile,
  1.1 MB .so at 0xB0000000.
- `birdman64.exe` with `--no-default-features --features first-run`: 11.8 MB, 4.8 MB
  zipped; PDB publics: no `uvVec3*`/`uvMat4*`/`func_802*`, only our
  `dylib::bootproc` trampoline.
- `PW64_GAME_DLL=… PW64_INPUT_SCRIPT=…fly_hang_glider.txt PW64_MAX_RETRACES=2700`:
  runs clean (threads at 0xb018…), frames 600/2700 correct (island, flight
  HUD). Pixel diff vs the static build is within the static-vs-static run
  variance (headless not deterministic, sweep.md), same scenes.
- `PW64_GAME_DLL=… cargo test --release -p pw64-game --no-default-features
  --features dylib`: boots the module with no ROM and the Rust panic unwinds
  through the DLL's C frames (thunks) and out of `os::run`.
- Risk: LLVM version drift (zig clang 21 vs dev clang-cl 23). The decomp has UB
  that clang versions exploit differently (cf. `customFxParams`, native-build
  §8.6): a release-only bug is possible. Resolved: dev builds use the same
  zig ("zig as the dev compiler" below).

## Components

"Flight check" (used throughout) = build the module, then `PW64_GAME_DLL=<module>
PW64_NO_AUDIO=1 PW64_INPUT_SCRIPT=crates/birdman64/scripts/fly_hang_glider.txt
PW64_MAX_RETRACES=2700 PW64_NO_THROTTLE=1 PW64_NO_INPUT=1 PW64_DUMP_FRAMES=600,2700`
with the first-run exe, and look at both PNGs (island; hang glider + HUD).
CI also runs `cargo clippy -p birdman64 -p pw64-game --no-default-features
--features pw64/first-run,pw64-game/dylib --all-targets` (the default
workspace clippy does not compile the dylib feature).

- **CRT via thunks**: `C(name)` entries in `pw64_dll_imports.h` for `memcpy
  memset strlen ldiv powf sqrtf`, defined in `pw64-platform/src/crt.rs`
  (`ldiv` = Windows LLP64 `long`, `#[repr(C)] {quot, rem}: i32`; unused on
  Linux, where the shadow stdlib.h inlines it). `llvm-objdump -p pw64game.dll`
  shows no "DLL Name"; Linux `nm -D --undefined-only` is empty.
- **`pw64-cbuild`** (lib): the C source lists, defines, flag sets
  (`MSVC_FLAGS`/`GNU_FLAGS`/`zig_flags`), patch applier, ops format, decomp
  manifest, fetch (feature `fetch`), module builder and embedded kit; used by
  pw64-game's build.rs (static build) and the launcher (first-run build).
- **Ops patch format** (`ops.rs`): `Keep(n)`/`Drop(n)`/`Insert(text)`,
  serialised as `file <path>` headers + `=n`/`-n`/`+text` lines.
- **Decomp manifest** (`decomp-manifest.txt`): `commit`, `url`, then
  `<sha256> <path>` per file under the source dirs + `include/` (CR stripped).
  Regenerate with `cargo run -p pw64-cbuild --example mkmanifest`; the test
  `manifest_matches_submodule` checks it against `decomp/`.
- **Builder**: `build_module(&Opts, progress) -> Result<Module, BuildError>`;
  example `build_module -- --zig <p> --decomp-dir decomp --out <dir>` (run from
  the repo root; also what CI's `firstrun` job runs on every push, then the
  dylib test). A new libultra import in the C shows up as an lld "undefined
  symbol": add `X(name)` to `pw64_dll_imports.h`.
- **Launcher** (`crates/birdman64/src/firstrun.rs`, feature `first-run`): load or
  build after the ROM step, egui progress screen in window.rs's event loop (one
  winit EventLoop per process), error dialogs, `--build-game`.
- **Crash symbolization**: crash handlers print `pw64game+0x<off>` for RIPs in
  the module and the `.map` path.

## Implementation notes

### Flags
- c-flags.txt diffs: normalise the `pw64-game-<hash>` OUT_DIR dirname cargo
  embeds in the -I lines (it changes whenever build.rs/Cargo.toml change). The
  build script re-runs every invocation; "no C object recompiled" (obj mtimes)
  is the signal for an unchanged build.
- `zig_flags(Os::Windows | Os::Linux)` = spike TGT + COMMON + WARN +
  `-include <native>/pw64_native.h` + DEFINES + `-I<native>/include` + one
  `-I<tree>/<DECOMP_INCLUDES entry>` each; `<native>`/`<tree>` are placeholders
  the builder substitutes. Twin-table unit test pins MSVC/GNU spellings 1:1
  (-J = -funsigned-char; /Gy//Oy come out of cc's -O2 baseline, so the Windows
  TGT spells -ffunction-sections/-fomit-frame-pointer; -fPIE is GNU-only).

### Thunk table
- Import table entries are `Sym(*const c_void)` wrappers (raw pointers are not
  `Sync`; fn items are, but one mixed table needs one type); CRT entries point
  at `pw64_platform::crt::pw64_crt_*` (generated code references them by path,
  which keeps the definitions in the exe). Gotcha: `fn as usize` fails const
  eval ("pointers cannot be cast to integers"); fn -> fn-pointer casts work.
- 57 imports bind (51 `X` + 6 `C`); exports: bootproc, pw64_dll_abi/bind/range,
  pw64_widescreen_aspect, D_802B892C.

### Crash symbolization
- os/crash.rs + crash_linux.rs: `set_game_module(range, map)` + a pure
  `write_rip`: RIPs in the module image print as `pw64game+0x<off>` (offset =
  `.map` address, module linked at 0xB0000000). Windows appends a `module map:
  <path>` line to stderr and crash.log; Linux writes it into the fixed buffer
  (path as raw OsStr bytes: no allocation, async-signal-safe).
- dylib.rs calls it right after loading: map = sibling `pw64game.map` of the
  module. Static builds never call it (range (0,0) matches nothing).

### Ops format and manifest
- `ops.rs`: `Op` = Keep(n)/Drop(n)/Insert,
  `from_unified_diff` mirrors `apply_unified_diff`'s hunk parsing (same
  `@@`/`--- `/`diff ` break conditions, `\ No newline` skip) but keeps only
  structure: the gap before every hunk becomes a leading Keep, runs of
  same-kind ops coalesce (a trailing context run merges into the inter-hunk
  gap Keep). `apply` mirrors the EOL handling (per-line CR strip, join with
  the original's dominant EOL) and errors if a Keep/Drop runs past EOF;
  lines after the last op are kept implicitly. `to_text`/`from_text`:
  `file <path>` headers + `=n`/`-n`/`+text` lines; unambiguous round trip
  (insert text always follows the `+` tag, so even `=5`/`file x.c` texts
  survive; counts must be positive or parse fails).
- `tests/ops_vs_patches.rs`: for all 73 patches, ops apply == unified-diff
  apply on the decomp file (CR stripped), text round trip, and serialized ops
  contain only `file `/`=n`/`-n`/`+` lines (no decomp content). Skips when
  decomp/ is absent.
- `manifest.rs`: 442 files at commit 2529dabf...1823fbe; `hash_cr_stripped`,
  `is_safe_path`. The local decomp checkout is CRLF (autocrlf) while the
  codeload zip is LF, hence CR-stripped hashing. `tests/manifest.rs` recomputes
  every hash from the submodule and compares entry lists both ways.

### Fetch
- `src/fetch.rs` (feature `fetch` = ureq 3 + rustls platform
  verifier + workspace `zip`): `decomp_tree(manifest, cache_root, progress)`
  = PW64_DECOMP_DIR tree (hash-checked) > cache hit (`.complete`) >
  PW64_DECOMP_ZIP > network. Download: agent with 60 s global timeout +
  `RootCerts::PlatformVerifier` (OS root store, HTTPS_PROXY honoured by
  ureq's default config), `User-Agent` Birdman64/<ver>, 3 attempts with 1 s
  / 4 s sleeps, 32 MB cap, `read_to_vec` (10 MB body limit fine for ~1 MB).
  `verify_and_extract`: per zip entry, name must split into top-dir +
  `/`-relative rel; dir entries (`rel` ending in `/`) traversal-checked then
  skipped; every OTHER entry (even non-manifest ones) must pass
  `is_safe_path` (rejects `..`/absolute/backslash before anything else);
  manifest paths must be present, non-symlink, and match the manifest
  CR-stripped sha256 (mismatch = HashMismatch with path+both hashes);
  missing files = Archive error. Files are written CR-stripped (cache tree
  = archive's LF content), `.complete` marker written last. `verify_tree`
  re-hashes an existing tree (dev override path). Errors: Network /
  HashMismatch / Archive / Io, Display = player-facing sentences.
- Tests use a synthetic in-memory zip (no network). zip 8 gotcha: the writer
  masks unix_mode to 0o777 (no S_IFLNK), so the symlink-skip branch is tested
  indirectly (the real archive's symlink is a non-manifest path anyway).
- Real codeload zips contain directory entries (`<top>/src/`) whose names
  `is_safe_path` rejects (trailing slash = empty component): dir entries are
  traversal-checked, then skipped.

### Builder
- `src/build.rs`: `build_module(&Opts, &mut dyn FnMut(Progress) +
  Send) -> Result<Module, BuildError>`; steps = `zig version` (Toolchain
  error = the "missing or blocked" message) > clear+create
  `out_root/build/<key>.tmp-<pid>/{tree,obj,game}` > copy src/+include/
  (create_dir_all first: include/ has no top-level files on disk, a plain
  copy of the dir errored) > apply ops (paths must be src/ or include/) >
  flags.rsp > parallel `zig clang @rsp -c -o` (jobs =
  available_parallelism min 8) > shim (`zig_tgt_common` + ABI define +
  -I dylib) > link (Windows lld-link flags == spike, `-map:` INCLUDED ->
  pw64game.map; Linux ld.lld + exports.ver) > per-obj logs concatenated to
  game/c-warnings.log, build.log, build.json (key/key_extra/zig_version/
  commit/abi/files/warnings) > delete+rename game/ -> out_root/game/<key>
  (atomic move; failed partial installs of the same key are deleted first)
  > tmp dir deleted. Module paths are recomputed for the FINAL game/<key>
  location (the rename would otherwise leave the struct pointing into the
  deleted tmp dir).
- Build key: `build_key` joins its parts with `-`, non [A-Za-z0-9._-] chars
  -> `_` (parts: see "ABI and build key").
- zig 0.16 gotchas: the rsp file is clang format
  (newline-separated quoted args, NO trailing commas); zig subcommands need
  the `clang` prefix (plain `zig @rsp -c` = "unknown command
  --target=..."); `zig version` must be spawned with NO args otherwise it
  prints the help text non-zero (0.16 ignores the extra arg and exits 1).
- Windows spawning: `CommandExt::creation_flags(CREATE_NO_WINDOW |
  CREATE_BELOW_NORMAL_PRIORITY_CLASS)` on every zig invocation (0x08000000 +
  0x4000); constants cfg(windows)-gated.
- The link command line (204 objects under `cache/build/<long key>.tmp-<pid>/
  obj/`) exceeds Windows' 32 K limit (os error 206): everything after
  `lld-link`/`ld.lld` goes in `link.rsp` (`@file`).
- Output matches the spike: 902656-byte DLL, 203 C files, 8 warnings, PE
  ImageBase B0000000, relocations stripped, no entry point. Dylib test:
  `PW64_GAME_DLL=<dll> cargo test --release -p pw64-game --no-default-features
  --features dylib` (cargo runs tests with the crate dir as cwd: pass an
  absolute path).
- Progress: `Step` = FindZig/Download/VerifyExtract/ApplyPatches/Compile/Link/
  Load, `Progress { step, fraction }` (whole-build fraction: FindZig 0,
  download 5-15, verify 20, patches 25, compile 25-95 monotonic via a
  max-basis-points atomic, link 97). `sha2` is ungated (build.rs hashes the
  key parts).

### Embedded kit
- `src/kit.rs` + pw64-cbuild build.rs: one `kit.rs` compiles twice
  (library + build script via `#[path]`, also `ops.rs`), so the packer and
  the reader cannot drift. Container = `PW64KIT1` magic + one
  `(u32 path len, path, u32 data len, data)` record per entry, LE, entries
  sorted by path, no timestamps -> byte-determinism (the ABI hash and
  cache key need it). Entries: `native/**`, `dylib/*` (spike_build.sh kept,
  dev reference only) + `ops.txt` = every patch as `ops::to_text` blocks.
  pw64-cbuild build.rs writes `$OUT_DIR/kit.bin`; lib.rs
  `pub static KIT = include_bytes!`. The builder parses the kit
  (`BuildError::Kit`), extracts native/** + dylib/** to `<tmp>/kit/` and takes
  ops from `ops.txt`; the example has an `--abi` override for tests.
- Legal regression test (`tests/kit.rs`): entry paths must be `native/`,
  `dylib/` or `ops.txt`; then every context/removed patch line (trimmed,
  >= 12 chars, not only braces/punctuation) must be absent from every kit
  entry's text: checked 1445 lines. Two data-driven exemptions, both
  counted: (1) the exact text is also a `+` line of some patch, possibly
  with a leading qualifier (`static s32 customFxParams[] = {` fixes the
  block-scope-array UB: the removed line is a substring of the inserted
  one; also reordered bitfields in uv_filesystem.h) - the notes' "minimal,
  unavoidable" case; (2) the line also sits in our own committed
  native/dylib sources (`#include <uv_memory.h>` in pw64_audio_load.c):
  interface facts, covered by the legal audit, not this test.
- Kit test gotcha: the raw kit bytes are not text (binary length fields);
  parse it and search per entry. Patch set facts: 25 context/removed lines
  are duplicated as `+` lines somewhere (substring match needed for the
  qualifier case). A kit determinism test checks regenerated == embedded bytes.

### ABI and build key
- pw64-game build.rs `dylib_abi()` prints `cargo:rustc-env=DLL_ABI=` =
  `kit::abi(pkg version, KIT)` = `<version>-<first 16 hex of
  sha256(pw64_dll_imports.h + pw64_dll_shim.c + kit)>`; `dylib::DLL_ABI =
  env!("DLL_ABI")`. The same value goes to the module (`-DPW64_DLL_ABI`) and
  the cache key, both computed from the same embedded kit, so exe and module
  always agree. A stale module is refused: "was built for another version
  (<old>, need <new>); delete it to rebuild" (dylib.rs `load`).

### Release packaging and CI
- Files: `release.yml` (windows + linux jobs), `ci.yml` job `firstrun`,
  `about.toml` + `about.hbs` at the repo root, `packaging/`.
  - zig: the windows zip's top dir is `zig-x86_64-windows-0.16.0/` with
    `zig.exe` + `LICENSE` (MIT) inside; the linux tarball likewise. zig.exe
    runs standalone without lib/.
  - LLVM licence: llvm-project tag `llvmorg-21.1.0`, `llvm/LICENSE.TXT`
    ("Apache License v2.0 with LLVM Exceptions"); release.yml fetches it.
  - cargo-about 0.9.2 (latest 0.9.x line) needs `--features cli` now (its
    default features no longer build the binary; plain `cargo install
    cargo-about` fails with "bin cargo-about requires the features: cli").
    Config schema changed too: top-level `accepted = [...]` (NOT
    `[licences] allow`), `targets = [...]`, `workarounds = [...]`,
    `private = { ignore = ... }` (see about.toml). Local
    `cargo about generate about.hbs` renders 87 licence sections /
    353 crate links; add "OFL-1.1" + "Ubuntu-font-1.0" to accepted
    (epaint_default_fonts needs them) or the run errors.
  - zip staging: `Compress-Archive -Path dist/Birdman64` produces the
    layout above (extracts to a `Birdman64/` root); the pwsh SHA256SUMS
    snippet emits sha256sum-format lines.
- release.yml shape: windows job (RUSTFLAGS=-Ctarget-feature=+crt-static in
  the job env) builds `--no-default-features --features
  first-run,gui-subsystem`, downloads + hash-checks zig into
  `toolchain/zig/`, fetches the LLVM licence, runs cargo-about, zips
  `Birdman64/` + uploads the PDB as its own artifact/release asset, smoke
  `--build-game` with `PW64_DATA_DIR=<workspace>/tmp/first-run-data`
  (clean dir => exercises the real network download + build; exits 0),
  then the dylib test against `tmp/first-run-data/cache/game/*/pw64game.dll`.
  linux job: same build without gui-subsystem, then the AppImage: AppDir
  root gets AppRun + .desktop + icon; zig at
  `usr/lib/birdman64/toolchain/zig/zig` (find_zig candidate 2 resolves
  `usr/bin/../lib/...`); the smoke runs the AppDir's AppRun with
  PW64_DATA_DIR UNSET, so paths.rs rule 1 gets AppRun's per-user default
  and the cache lands in `$HOME/.cache/birdman64/game/` (asserted with
  test -d); the dylib test uses that module. appimagetool 1.9.1
  (`--comp zstd`; NOT `--compilation`: that flag does not exist in
  appimagetool.c, the option is `{ "comp", ... }`) pinned by URL + sha256
  (ed4ce84f...).
- AppRun: `PW64_DATA_DIR` export only when unset (the CI smoke with the
  var set bypasses it, so the linux job also asserts the unset case);
  APPDIR falls back to the script's own dir so a direct AppDir run works
  (the launcher normally exports APPDIR itself).
- ci.yml `firstrun` job (windows + ubuntu matrix): zig cached by
  `actions/cache@v4` key `zig-0.16.0-<runner os>` into `tmp/zig/`
  (pwsh-only download script; pwsh is preinstalled on ubuntu-latest, so
  both legs share one script with $IsWindows branches); module via the
  example + `--decomp-dir decomp` (also re-validates the manifest on
  every push); dylib test with an absolute PW64_GAME_DLL (globbed from
  `tmp/firstrun/game/*/pw64game.dll|libpw64game.so`). The linux dylib test
  needs no libasound/udev/dbus (pw64-game + pw64-platform only).
- Windows release smoke (crt-static exe, clean PW64_DATA_DIR): download +
  verify + patch + compile + link + load 5.7 s, module loaded at 0xb0000000.

### Launcher
- `crates/birdman64/src/firstrun.rs` (feature `first-run` pulls
  `pw64-cbuild/fetch`). Flow: main maps RDRAM (`memmap::init` loads nothing
  in dylib builds) ->
  `--build-game`/`PW64_BUILD_ONLY` (before the ROM step: CI has no ROM) ->
  ROM -> headless: `ensure_blocking` (stderr steps, exit 1 + message on
  failure) / windowed: cached module loaded before the window, else
  `window::run(setup=true)`: `App::start` makes window + GPU, then
  `firstrun::Screen` (egui, worker thread, `UserEvent::Setup` wakes) instead
  of the game thread; done -> `firstrun::load` (dylib::load +
  `check_low_4gb`) -> `spawn_game()` in the same window. No settings overlay
  during setup; window close then sets PARKED (no game thread to wait for).
- Cache hit WITHOUT zig: `build::find_cached(root, DLL_ABI, commit)` matches
  `game/<abi>-*-<commit>/` with the library present (the zig version is only
  known by running the 177 MB zig; must not happen per launch, and a later
  start must work if antivirus removed zig). `remove_other_keys` after a
  successful build. Cache dir: `paths::cache_dir()` = `<data dir>/cache`,
  per-user data dir -> `%LOCALAPPDATA%\Birdman64\cache` / XDG cache.
- zig: `PW64_ZIG` > `<exe dir>/toolchain/zig/zig[.exe]` > (Linux AppImage)
  `<exe dir>/../lib/birdman64/toolchain/zig/zig`. `PW64_GAME_DLL` still wins
  over everything (dev: load that module, no build).
- Progress is made monotonic in the launcher (the builder restarts at
  FindZig 0% after the fetch; download retries re-report 5%).
- `cache/build.log` is the launcher's log (steps with timings, the full
  error text incl. compiler output); the builder's own build.log lives in
  the tmp dir, deleted on success, kept on failure.
- Error texts (firstrun.rs `fetch_failure`/`build_failure`): network ->
  check connection; hash/archive -> retry/report; zig missing -> re-extract
  or allow in antivirus; I/O -> names the cache dir + ~200 MB; compile/link ->
  report with log. Every one shows the log path and Retry/Quit
  (Enter/Esc); headless prints `error: game setup failed: ...` + exit 1.
- `settings.rs` `raw_input`/`paint` are shared egui helpers (settings
  overlay + setup screen).
- Dev capture: with `PW64_WIN_SHOT` set, the setup screen writes
  `tmp/win_setup_<Step>.png` once per step (Compile at >= 60%) and
  `tmp/win_setup_error.png`.
- Timings (Windows, network): `--build-game` from an empty PW64_DATA_DIR
  6.4 s (download 0.4 s, compile 4.4 s); second run 0.13 s (cache hit). A
  cached windowed start works with PW64_ZIG pointing nowhere; a corrupt
  PW64_DECOMP_ZIP gives exit 1 + message headless, the error screen windowed.

### Legal audit
- Release tree (`cargo tree -p birdman64
  --no-default-features --features first-run,gui-subsystem -e normal`): no
  pw64-kernel; release PDB has no uvVec3/uvMat4/func_802/pw64_kernel names;
  decomp identifiers in the exe bytes sit only inside the embedded kit
  (our `+` lines/comments). No ROM tables in the exe (resampler LUT read from
  the ROM at run time; no large const arrays). `+` lines that equal a line of
  the pristine decomp file are single statements/bitfields (sched.c below).
  `cargo test -p pw64-cbuild --test kit -- --nocapture` lists the exempt
  restatements.

### Robustness
- Decomp extract goes to `decomp/<commit>.tmp-<pid>` then
  rename (cache hit re-hashes the tree); 16 MB inflated cap per zip entry;
  module files fsynced (write handle: FlushFileBuffers needs it) before the
  `game/<key>` rename; an existing `game/<key>` that can't be removed (loaded
  by another instance) or a lost rename race = use that module; stale
  `build/*.tmp-*` (> 1 day) pruned after a build; compile-worker panics become
  BuildError (catch_unwind, poison-tolerant locks, no `eprintln!`); a refused
  module is FreeLibrary'd/dlclosed (Retry can rebuild + reload); a cached
  module that fails to load is rebuilt (windowed); crt memcpy = memmove.
  Two concurrent `--build-game` from an empty cache: both exit 0.
- README facts for players: ~1 MB download, 10 to 60 s build, ~200 MB disk,
  internet once; the `toolchain` folder must stay next to the exe; deleting
  the cache only costs one rebuild.
- `.gitattributes`: `* text=auto`, eol=lf for *.sh, packaging/linux/AppRun,
  *.desktop, *.yml, *.toml, *.rs, *.md, *.c, *.h, *.patch,
  decomp-manifest.txt (hashed/parsed: bytes must be identical on all OSes);
  binary: *.png *.ico *.zip.

### zig as the dev compiler
- **zig is the default compiler of the static
  build** (pw64-game build.rs `zig_cc`): `zig clang` + `pw64_cbuild::zig_flags`
  verbatim (placeholders: `<native>` -> crates/pw64-game/native, `-I<tree>/X` ->
  the patched include mirror for `include*`, decomp/ otherwise) +
  per-file ported.txt renames + `-g` (`-gcodeview` on Windows) when cargo's
  DEBUG is on. So debug builds compile the C at -O3 too (was clang-cl -O0):
  dev, tests, CI and players share the codegen. Objects archived with `zig lib`
  (llvm-lib, COFF archive; MSVC link.exe links it fine) / `zig ar rcs` (Linux),
  args via `archive.rsp`, then `rustc-link-lib=static:+whole-archive=pw64game`.
- zig lookup: `PW64_ZIG` > `<repo>/tools/zig/zig[.exe]` > panic naming the fix.
  No PATH lookup; `zig version` must equal `pw64_cbuild::ZIG_VERSION` and is part
  of the objects' rebuild key. Download: `cargo run -p pw64-cbuild --example
  get_zig --features fetch` (ureq, sha256 vs `ZIG_WINDOWS`/`ZIG_LINUX` in
  pw64-cbuild lib.rs; Windows unzips zig.exe + LICENSE with the zip crate,
  Linux runs `tar -xJf --strip-components=1`; no-op when already 0.16.0).
  `tools/zig/` is gitignored. Legacy LLVM kept as `PW64_CC=clang-cl` / `clang`
  (unchanged code path, cc baseline incl. profile opt level).
- Why it links: zig clang with `--target=x86_64-pc-windows-msvc` emits
  MSVC-ABI COFF with **no** `/DEFAULTLIB` directives (clang-cl -MD adds
  msvcrt+oldnames; `-###` shows none for zig), so the CRT (`__security_cookie`,
  memcpy, sqrtf...) comes from what rustc links; `-D_MT -D_DLL` only matter to
  headers (we use -nostdinc). Function sections = `-ffunction-sections` (/Gy).
  Linux: ELF objects, `-fPIE`, rust-lld non-PIE exe as before.
- A/B (release pw64, fly_hang_glider, frames 600/2700, 8 runs each): all run
  clean; same scenes; PSNR vs one clang-cl run: clang-cl 49-74 dB (600) /
  35-55 dB (2700), zig 49-76 / 40-55, one zig outlier at 600 (27 dB: same view,
  camera a few frames shifted; its 2700 frame matches) = the known headless
  timing nondeterminism, not a codegen difference. c-warnings: 8 both, identical
  text. `cargo test --workspace` (debug) and `--release --test kernel_diff`:
  green (18 ports bit-identical vs the zig -O3 C). Cross-check
  `cargo clippy --target x86_64-unknown-linux-gnu -p pw64-game` from Windows
  compiles + archives the Linux objects with the Windows zig (ELF); Linux
  link/run is left to CI.
- Build time (clean pw64-game, 16 threads): debug clang-cl -O0 7.5 s vs zig
  -O3 6.4 s; release `cargo build --release -p birdman64` after `cargo clean
  --release -p pw64-game`: 54.7 s vs 49.6 s (dominated by the Rust link).
- CI: test/linux/firstrun jobs share `actions/cache` `tools/zig` (key
  `zig-0.16.0-tools-<os>`) + the get_zig example on a miss; the linux job
  installs no clang; firstrun passes `--zig tools/zig/zig[.exe]`.
  release.yml needs none (its first-run exe has no C).

### sched.c present path (legal audit follow-up)
- sched.c.patch `_uvScHandlePresent` is written so no decomp statement is
  restated. Static `pw64_sc_present`;
  `_uvScHandlePresent` = flag 1, `_uvScHandleRetrace()`, flag 0 (single
  scheduler thread, no re-entry risk). `_uvScHandleRetrace` gains only our
  lines: a `goto pw64_shared` past the retrace-only gates (clock update,
  RSP/RDP timeout counters + checks, audio-busy gate), a flip condition
  `(D_802B9C68 != 0) && ((pw64_sc_present != 0) || !pw64_present_active())`
  (the flag skips `pw64_present_active()`: the tick already latched the swap),
  and a present-only block after the shared cmdQ drain that starts gfx only
  when the RSP is idle, then returns (audio start/yield/clients stay
  retrace-only). Behaviour is identical in both paths by construction
  (statements untouched, only our lines inserted).
- Only `_uvScHandleRetrace();` (the wrapper's own call) remains as a
  restatement in sched.c; `+` lines >= 12 chars equal to a pristine decomp
  line, over all patches: 19. Gotcha: the kit test only checks
  context/removed lines *inside hunks*; a copy outside every hunk is caught
  only by the audit's `+`-lines view. Both tests: `cargo test -p pw64-cbuild`.
- Flight check at 60 fps and `PW64_FPS=144` gives the same flight (TIME
  00'18"49 vs 00'18"52, within the framerate.md drift tolerance).

### Dependencies
- zip features `deflate` -> `deflate-flate2-zlib-rs`
  (zip 8's `deflate` composite silently includes the zopfli WRITE-side
  compressor; we only read ROM/decomp zips): zopfli gone from
  Cargo.lock/-graph (crate counts: pw64 Linux 360 -> 358, Windows
  234 -> 232; per-target unique-crate totals). cargo-machete is clean.
- rfd: kept the default xdg-portal backend. Measured
  alternative `gtk3` (default-features off + features = ["gtk3"]): 269
  Linux crates vs 358, but it adds a hard gtk3-devel build dependency
  (pkg-config + ~15 *-sys crates with system libs) on every Linux dev
  machine + CI runner and needs libgtk3 at RUNTIME on players' machines
  (not guaranteed on non-GNOME desktops); the portal needs only D-Bus
  (every desktop has it), picks the right native dialog per DE, and rfd
  falls back to zenity when no portal backend exists. rfd 0.15 has no
  lighter portal option: the portal requires an async-runtime feature
  (tokio or async-std) for ashpd even for sync dialogs. Decision written
  in the workspace Cargo.toml comment. Decision criteria: a player-facing
  file dialog must work on a normal Linux desktop without extra installs.
- PowerShell gotcha: piping clippy/test output to Select-String makes the
  pipeline report exit 1 with no error lines; use `Out-File` +
  `$LASTEXITCODE` to read the real code.

### App icon and version resource
- Exe resource = `crates/birdman64/pw64.rc`
  (icon `1 ICON ../../assets/icon/birdman64.ico` + VERSIONINFO) embedded by
  pw64 build.rs via `embed_resource::compile("pw64.rc", &macros)
  .manifest_optional()` in a `windows_resource()` fn gated on
  `CARGO_CFG_TARGET_OS == "windows"` (NOT `cfg!(windows)`: build scripts
  compile for the host, the env var carries the *target*; Linux-target
  clippy/cross builds must not emit link args). Macros
  `VER_MAJOR/MINOR/PATCH` (+ numeric FILEVERSION) + `VER_VERSION` from
  CARGO_PKG_VERSION. FileDescription/ProductName Birdman64, CompanyName
  "Birdman64 contributors", OriginalFilename Birdman64.exe, LegalCopyright
  "MIT licence; not affiliated with Nintendo".
- rc gotchas (SDK rc.exe 10.0.19041):
  plain macros do NOT expand inside quoted strings - need the two-level
  stringize `VER_STR(VER_VERSION)`; `#x` stringize works in rc. rc.exe
  needs winver.h for VOS_NT_WINDOWS32/VFT_APP: `#include <winver.h>` +
  the SDK Include dir (embed-resource sets %INCLUDE% itself). No `."
  string concat in rc VALUE strings (RC2104).
- embed-resource behaviour checked in its source (3.0.11): on a windows-msvc
  HOST it only looks for rc.exe (registry KitsRoot10/vswhom, `RC` env
  override; NO llvm-rc fallback - llvm-rc is the default only for
  non-Windows hosts cross-compiling). `manifest_optional()` degrades a
  missing rc.exe to "no icon" instead of failing the build (winresource
  would hard-fail; chosen embed-resource over it for that). CI
  windows-latest has the SDK.
- Window icon: `assets/icon/birdman64-64.png` (64x64 RGBA, generated by
  render.py), decoded in
  window.rs `window_icon()` (include_bytes! + png crate, rejects
  non-RGBA8) and passed as `with_window_icon` at the one create_window
  site. `Icon::from_rgba(buf, w, h)`. Headless never reaches window::run
  (main.rs), so the decode cannot affect the smoke path.
- Linux packaging: .desktop has `Icon=birdman64`; release.yml copies the
  256 px png to the AppDir root, `.DirIcon` and `hicolor/256x256/apps`.
- Linux cross-clippy of `-p birdman64` from Windows needs libdbus-sys (rfd portal
  dep, pkg-config); pw64-rom + pw64-cbuild cross-check fine.

## Linux unwind fix

- Symptom: dylib test on the zig-built `libpw64game.so` aborted with
  `failed to initiate panic, error 5` (_URC_END_OF_STACK).
- Cause: we call `zig ld.lld` directly, and plain ld.lld does NOT emit
  `.eh_frame_hdr`/`PT_GNU_EH_FRAME` unless given `--eh-frame-hdr` (the
  gcc/clang driver always passes it). The objects had `.eh_frame` (zig clang
  defaults to async unwind tables on x86_64-linux-gnu, no flag needed), but
  libgcc's `_Unwind_Find_FDE` finds FDEs of a dlopen'ed module only via
  `dl_iterate_phdr` + PT_GNU_EH_FRAME (no crtbegin `__register_frame`: the
  module links no CRT). The fixed base 0xB0000000 was not involved.
- Fix: `--eh-frame-hdr` in the Linux link args (pw64-cbuild build.rs) and in
  `dylib/spike_build.sh`. Windows unchanged (SEH `.pdata`). Kit/ABI hash
  changes (the spike script is in the kit), so old cached modules rebuild.
- Check: `readelf -lW libpw64game.so | grep GNU_EH_FRAME` must show a row.
  Verified in WSL Ubuntu 20.04 (rustup + Linux zig 0.16.0): test aborts
  before, passes after.
- Static Linux path is fine: rustc's cc-driven link adds the header, and
  `tests::boot_without_rom_unwinds_through_c` (static feature) does unwind
  through C and passes in the CI `linux` job.
- The build key once did not cover pw64-cbuild's own flag/link-arg
  constants (a change there alone reused a stale module). Now
  `pw64_cbuild::build::flags_hash(os)` = first
  16 hex of sha256 over the whole `zig_flags(os)` line list (placeholders
  intact) + the linker argument list + the Linux exports script text.
  `link_args` holds the constant part only (map/out/version-script path args
  stay runtime); the Linux version script's text is hashed with it
  (`hash_link_list`: the exports are part of the module). The hash is a
  key part of its own: `build_key` = `<key_extra>-<flags>-<zig>-<commit>`,
  `find_cached(out, key_extra, flags, commit)` matches
  `<abi>-<flags>-*<commit>` (flags is known without running zig, so a
  flags-only change can never reuse the older module; firstrun.rs passes
  `flags_hash_host()`). build.json records `flags`. Tests:
  `flags_hash_changes_with_the_flag_list`, `build_key`, `find_cached`
  (different flags = no match).

## README screenshots

- Headless dumps at `PW64_DUMP_HEIGHT=1080` (width follows the output
  aspect, fixed per run so `PW64_DUMP_PIXEL` coordinates stay put): glider
  frames 1850 (hero) + 1520 (briefing) from fly_hang_glider.txt, rocket belt
  1950 (21:9) from fly_rocket_belt.txt. Hosted as release assets, not in
  the repo.
- Filter comparison (PW64_FILTER=n64 vs bilinear) not shipped: mean pixel
  diff 0.6/255 at the same retrace, visible only zoomed in.
- No title screen, no Nintendo/Paradigm logo or wordmark in any image
  (gameplay, briefing and our own UI only).

## Data dir, zip guard, ROM diagnosis

- Data dir (paths.rs): `decide` takes a `marker` param (exe dir holds
  `portable.txt` / `pw64.toml` / `pw64.eep`); portable = marker && writable.
  The write probe only runs when a marker is present (plain installs skip the
  create/delete). Older portable installs keep working: their pw64.toml /
  pw64.eep are themselves markers.
- Zip guard (main.rs): `in_zip_temp(exe, temp)` = exe strictly below
  temp and some component after it starts `temp1_` or ends `.zip`
  (case-insensitive via to_ascii_lowercase over components). Windows-only call
  before the ROM step, `fatal(...)` with the "Extract All..." message. Tests
  use forward-slash paths so they run on Linux too (backslash paths are one
  component there). Temp detection matches components starting "temp1_",
  "7zo", "rar$ex" (case-insensitive) or ending ".zip", anywhere under a
  Temp/TMP dir.
- ROM diagnosis: `pw64_rom::diagnose(bytes) -> RomProblem` normalizes the
  first 0x40 bytes to .z64 (magic match, 2/4-byte swap), reads title 0x20..0x34
  + country 0x3E. "Pilotwings64-ish" = lowercased, spaces/underscores stripped,
  contains "pilotwings64". PW title + 'E' → Modified (only called after a
  failed verify, no SHA recompute); 'P'/'J'/other → Region("European" |
  "Japanese" | "non-US"); other title → OtherGame; bad magic/short → NotN64.
- rom_setup.rs: welcome OkCancel box before every first picker round (both
  Prompt and stale-Remembered reach it); welcome Cancel and picker Cancel both
  go through `no_rom_chosen()` (Info box "No game file chosen..." + exit 0).
  The picker error box and the CLI/env `Use` error (`friendly_use_error`,
  prepended to the fatal message) use `diagnose_message(path)`; zip files
  return None there (from_zip's member error is already specific), so a zip
  with a wrong-region ROM inside still shows the detailed SHA error: known
  gap, would need a pw64-rom zip-member diagnose helper. The picker box is
  cross-platform (rfd works on Linux).
- Unit tests cover the pure parts (diagnose headers, diagnose_message on
  temp files, in_zip_temp, decide); dialog behaviour on screen is manual.

## Release notes and READ ME FIRST

- `packaging/release-notes.md` is the release body (4-item Quick start;
  "other files (debug symbols, checksums)" line); both `github release`
  steps pass `body_path`. Pdb asset: `Birdman64-windows-x64-debug-symbols.pdb`.
  The Linux tar.gz keeps README.md, so the same body covers both platforms.
- `packaging/READ ME FIRST.txt` (25 lines) is copied into the Windows zip
  root and the Linux dist + AppDir. Save-folder text matches paths.rs:
  default per-user
  `%APPDATA%\Birdman64` + `%LOCALAPPDATA%\Birdman64\cache`, portable only
  with `portable.txt` (or an existing pw64.toml/pw64.eep) next to the exe,
  Linux per-user/cache paths in parentheses. Keys line matches the README
  controls table (Esc/F10 settings, pad Select/Create/Capture). Line-ending
  check: plain ASCII, no tabs, git eol=lf does not apply (`*.txt` is not
  listed, text=auto handles it).

## Error handling details

- ureq 3.4.2 surfaces 4xx/5xx as `ureq::Error::StatusCode(u16)` (default
  `http_status_as_error = true`): mapped to `FetchError::Http` in
  `download()` with no retry; every other variant stays a retryable
  `FetchError::Network`. Player text includes the HTTPS_PROXY hint.
- Shared `retry_fs`/`transient_io` in pw64-cbuild build.rs wraps
  rename/remove_dir_all at the fetch extract and build publish/delete sites:
  PermissionDenied or os error 32/33 (Windows sharing violation) retries with
  a 0.25 s doubling backoff to 2 s (~8 s deadline). Antivirus scanning a
  freshly written module is the common cause.
  - Transient is per platform (pure `transient_kind`):
    Windows PermissionDenied + raw 32/33; Linux only EBUSY/ETXTBSY/EAGAIN
    (`ResourceBusy`/`ExecutableFileBusy`/`WouldBlock`). Everything else
    (NotFound, Linux EACCES, disk full) fails at once, no 8 s wait.
- Compile failures name the file: `run_zig` returns `Zig`/`Io` whose text
  only says "zig clang", so `compile_one` rewraps `Zig` as
  `BuildError::Compile { file, log }` (and writes `<obj>.log`) and prefixes
  `Io` with "compiling <file>: ". `Compile`'s Display (what build.log gets)
  caps the compiler output at 30 lines + "(N more lines)". The setup
  screen's message stays the generic "Building the game failed" text.
- `run_zig` classifies its own output: "No space left", "not enough
  space", "Access is denied", "being used by another process" become
  `BuildError::Io` (the player-fixable disk message); any other non-zero zig
  exit is `BuildError::Zig(captured output)` and reports to the issue tracker.
  Callers don't re-wrap. Note: adding a BuildError variant breaks the
  match in pw64 firstrun.rs build_failure (exhaustive).
- Cache prune keeps the current key + the 2 newest other keys by mtime via pure
  `prunable(entries, already, keep_newest)`. Clippy: `sort_by_key` with
  `std::cmp::Reverse`, not `sort_by(|a, b| b.1.cmp(&a.1))`.
- ROM loading: pi.rs stat gate first (skip raw files > 64 MiB before reading);
  pw64-rom `Rom::load` opens the File first and probes 4 bytes: zip magic
  routes to `from_zip(path, file)` with a File-backed ZipArchive (zip v8
  ZipArchive::new is generic over Read + Seek, so no whole-zip reads).
- load() classifies dylib error strings via `load_problem`:
  "address range taken" and "could not load ..." + not found/denied variants
  set `Failure::reload_only`, and the setup screen's Retry then re-issues
  `load` (Screen stores the module path from poll and wakes the Setup event)
  instead of spawn_worker. Linux dlopen cancel is `Ok(None)` inside rfd, so
  rfd `pick_file() -> None` on Linux always means "no dialog backend worked".
- rfd 0.15.4 xdg_desktop_portal.rs: every portal error (including the service
  being absent AND the player cancelling, which ashpd reports as Err) falls
  back to zenity; zenity missing => None. So None + no zenity + no kdialog is
  the "no dialog works" case (stderr instructions, exit 1); kdialog runs
  `--getopenfilename . "<filters>"` with Qt filter syntax
  ("desc (*.z64 *.n64)\nAll files (*)").
- `Rom::load` IO errors carry `std::io::Error` as root cause
  (`e.root_cause().is::<std::io::Error>()`), which rom_setup uses to show
  "Birdman64 couldn't read {file}: {e}" instead of the wrong-content box,
  for picked files and the remembered ROM alike. A gone remembered ROM also
  switches the welcome text.
- Windows file-type filters are case-sensitive: the picker filter needs
  the uppercase extensions too, plus an "All files" (*) filter (portal maps
  "*" to a glob, Windows to *.*).

