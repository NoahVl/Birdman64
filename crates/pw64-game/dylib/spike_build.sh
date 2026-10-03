#!/usr/bin/env bash
# SPIKE (2026-09-30), docs/notes/first-run-build.md: the player-side build of
# the game module with zig, by hand. The real implementation is the Rust
# builder (task list in the notes); this script is the reference for its
# exact steps and flags, and CI can use it until then.
#
# usage: spike_build.sh <decomp tree (archive extract)> <out dir> <zig.exe|zig>
# Windows (Git Bash): builds <out>/pw64game.dll at 0xB0000000.
# Linux:              builds <out>/libpw64game.so at 0xB0000000.
set -euo pipefail
DECOMP=$(cd "$1" && pwd); OUT=$2; ZIG=$3
HERE=$(cd "$(dirname "$0")" && pwd)
GAME=$(cd "$HERE/.." && pwd)            # crates/pw64-game
ABI=${PW64_DLL_ABI:-pw64-dll-spike}
mkdir -p "$OUT"; OUT=$(cd "$OUT" && pwd)
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) WIN=1 ;; *) WIN=0 ;; esac
# zig is a native Windows program: give it C:/... paths, not /c/...
if [ $WIN = 1 ]; then
  DECOMP=$(cygpath -m "$DECOMP"); OUT=$(cygpath -m "$OUT"); HERE=$(cygpath -m "$HERE"); GAME=$(cygpath -m "$GAME")
  PATCHED=$(cygpath -m "${PATCHED:-}")
fi

t0=$(date +%s)
# 1. Source tree = pristine archive copy + our patches. GNU patch rejects
#    some of them (asymmetric context, whitespace-only context lines), so
#    the spike overlays the files build.rs's strict applier produced
#    ($PATCHED = target/<p>/build/pw64-game-*/out/patched); the real builder
#    runs that applier itself (task list).
: "${PATCHED:?set PATCHED to a pw64-game OUT_DIR/patched}"
rm -rf "$OUT/tree"; mkdir -p "$OUT/tree"
cp -r "$DECOMP/src" "$DECOMP/include" "$OUT/tree/"
cp -r "$PATCHED/." "$OUT/tree/"
T=$OUT/tree

# 2. Flags: the static build's (pw64-game build.rs, c-flags.txt) in GNU
#    spelling, minus ported.txt renames (the Rust ports are not shipped: the
#    C originals are compiled instead).
COMMON=(-nostdinc -std=gnu11 -funsigned-char -fwrapv -fno-strict-aliasing
  -ffp-contract=off -ftrivial-auto-var-init=zero -fgnuc-version=4.2.1 -O3)
WARN=(-Wno-multichar -Wno-incompatible-library-redeclaration
  -Werror=int-conversion -Werror=pointer-to-int-cast -Werror=int-to-pointer-cast
  -Werror=void-pointer-to-int-cast -Werror=int-to-void-pointer-cast
  -Werror=shorten-64-to-32 -Wno-error=incompatible-function-pointer-types
  -Wno-error=incompatible-pointer-types -Wno-error=implicit-function-declaration
  -Wno-error=implicit-int -Wno-error=return-type)
DEFS=(-D_LANGUAGE_C -DVERSION_US -DBUILD_VERSION=VERSION_D -D_FINALROM -DNDEBUG
  -DTARGET_N64 -DNON_MATCHING -DAVOID_UB -DRECOMP_BUILD)
INCS=(-I"$GAME/native/include" -I"$T" -I"$T/src" -I"$T/include" -I"$T/include/kernel"
  -I"$T/include/libultra" -I"$T/include/libultra/PR" -I"$T/include/libultra/compiler")
if [ $WIN = 1 ]; then
  # = clang-cl -MD -O2 (cc1 -O3, /GS strong, /Gy, /Oy); checked with -### diff.
  TGT=(--target=x86_64-pc-windows-msvc -D_MT -D_DLL -fstack-protector-strong
    -ffunction-sections -fomit-frame-pointer)
  EXT=obj
else
  TGT=(--target=x86_64-unknown-linux-gnu -fPIE)
  EXT=o
fi
FLAGS=("${TGT[@]}" "${COMMON[@]}" "${WARN[@]}" -include "$GAME/native/pw64_native.h" "${DEFS[@]}" "${INCS[@]}")
# Response file (forward slashes, quoted: spaces/non-ASCII in the user path).
for f in "${FLAGS[@]}"; do printf '"%s"\n' "${f//\\//}"; done > "$OUT/flags.rsp"

# 3. Compile (parallel, one process per file).
mkdir -p "$OUT/obj"
{
  for d in src/kernel src/app src/libultra/audio src/libultra/sp; do ls "$T/$d"/*.c; done
  ls "$GAME"/native/src/*.c
} > "$OUT/sources.txt"
t1=$(date +%s)
export ZIG OUT EXT
compile_one() {
  local s=$1 n
  n=$(echo "$s" | sed 's#.*/tree/src/##;s#.*/native/src/#native_#;s#/#_#g;s#\.c$##')
  "$ZIG" clang @"$OUT/flags.rsp" -c "$s" -o "$OUT/obj/$n.$EXT" > "$OUT/obj/$n.log" 2>&1 \
    || { cat "$OUT/obj/$n.log" >&2; echo "FAILED $s" >&2; return 255; }
}
export -f compile_one
xargs -P "$(nproc)" -I{} bash -c 'compile_one "$@"' _ {} < "$OUT/sources.txt"
cat "$OUT"/obj/*.log > "$OUT/c-warnings.log"
# The shim (ours, no decomp headers).
"$ZIG" clang "${TGT[@]}" "${COMMON[@]}" -DPW64_DLL_ABI="\"$ABI\"" -I"$HERE" \
  -c "$HERE/pw64_dll_shim.c" -o "$OUT/obj/pw64_dll_shim.$EXT"
t2=$(date +%s)
echo "patch: $((t1 - t0)) s, compile: $((t2 - t1)) s, $(ls "$OUT"/obj/*.$EXT | wc -l) objects, $(grep -c ': warning:' "$OUT/c-warnings.log") warnings"

# 4. Link at the fixed base (0xB0000000: between the coroutine stacks, which
#    end at 0xA0000000, and the exe at 0xC0000000).
EXPORTS=(bootproc pw64_widescreen_aspect pw64_fill_screen D_802B892C)
if [ $WIN = 1 ]; then
  # /FIXED: no relocations, so LoadLibrary fails instead of relocating when
  # the range is taken (the exe checks the base too). /NOENTRY: no CRT init.
  # (dash spelling: Git Bash would rewrite /FLAG arguments as paths)
  "$ZIG" lld-link -nologo -dll -noentry -nodefaultlib -machine:x64 -Brepro     -base:0xB0000000 -fixed -dynamicbase:no -highentropyva:no -opt:ref -opt:icf     -export:bootproc -export:pw64_widescreen_aspect,DATA -export:pw64_fill_screen,DATA -export:D_802B892C,DATA     -out:"$OUT/pw64game.dll" -map:"$OUT/pw64game.map" "$OUT"/obj/*.obj
  MOD=$OUT/pw64game.dll
else
  { echo "{ global: pw64_dll_bind; pw64_dll_abi; pw64_dll_range;"; for e in "${EXPORTS[@]}"; do echo " $e;"; done; echo " local: *; };"; } > "$OUT/exports.ver"
  # -Bsymbolic + version script: every C symbol binds locally, so the -fPIE
  # objects (identical code to the static Linux build) link into a .so.
  # Plain ld.lld (zig cc ignores --image-base for -shared and can't -Map):
  # no libc linked and (T1) nothing stays undefined: the CRT thunks bind to
  # the exe's pw64_crt_* via pw64_dll_bind like every other import.
  # --eh-frame-hdr: plain ld.lld omits PT_GNU_EH_FRAME (the cc driver adds it);
  # without it a Rust panic cannot unwind through the module (error 5).
  "$ZIG" ld.lld -shared -Bsymbolic --eh-frame-hdr --version-script="$OUT/exports.ver"     --image-base=0xB0000000 -z noexecstack -Map="$OUT/pw64game.map"     -o "$OUT/libpw64game.so" "$OUT"/obj/*.o
  MOD=$OUT/libpw64game.so
fi
t3=$(date +%s)
echo "link: $((t3 - t2)) s -> $MOD ($(stat -c %s "$MOD") bytes); total $((t3 - t0)) s"
