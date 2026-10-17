#!/usr/bin/env bash
# Builds a debug-signed arm64-v8a APK of Birdman64: the birdman64 library as
# a cdylib (libbirdman64.so) behind a NativeActivity manifest. No Gradle, no
# Java code: only the SDK build-tools (aapt2, aapt, zipalign, apksigner) and
# keytool. See docs/notes/android.md.
#
# Needs: `rustup target add aarch64-linux-android`; zig in tools/zig (get_zig
# example); ANDROID_NDK_HOME (NDK r27+); ANDROID_HOME (build-tools + one
# platform). Runs on Linux and Git Bash on Windows.
#
# Usage: packaging/android/build-apk.sh [--debug] [--release-flavour]
#   → target/android-apk/birdman64.apk
#
# Default: dev APK, game C linked into libbirdman64.so (never distribute).
# --release-flavour: no game C (feature first-run); the app builds the game
# module on the device with the bundled zig, shipped as
# lib/arm64-v8a/libzig.so (get_zig --android → tools/zig-android/).
set -euo pipefail
cd "$(dirname "$0")/../.."

PROFILE=release
CARGO_PROFILE=(--release)
FEATURES=()
FLAVOUR=dev
for arg in "$@"; do
    case "$arg" in
        --debug) PROFILE=debug CARGO_PROFILE=() ;;
        --release-flavour)
            FLAVOUR=release
            FEATURES=(--no-default-features --features first-run)
            ;;
        *) echo "error: unknown argument $arg" >&2; exit 1 ;;
    esac
done
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
NDK="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
[ -d "$SDK" ] || { echo "error: set ANDROID_HOME to the Android SDK" >&2; exit 1; }
[ -d "$NDK" ] || { echo "error: set ANDROID_NDK_HOME to the Android NDK" >&2; exit 1; }
# = minSdkVersion in AndroidManifest.xml and pw64-cbuild's zig target.
API=26

case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) HOST=windows-x86_64 EXE=.exe CMD=.cmd BAT=.bat ;;
    Darwin) HOST=darwin-x86_64 EXE= CMD= BAT= ;;
    *) HOST=linux-x86_64 EXE= CMD= BAT= ;;
esac
# Git Bash: Windows-form paths (cc-rs runs the NDK's .cmd wrappers through
# cmd.exe, which can't resolve /c/... paths).
if command -v cygpath > /dev/null; then NDK=$(cygpath -m "$NDK"); fi
BIN="$NDK/toolchains/llvm/prebuilt/$HOST/bin"
# rustc links the app library with the NDK clang (bionic, liblog,
# libandroid, libaaudio); the decomp C itself is compiled by zig.
export CC_aarch64_linux_android="$BIN/aarch64-linux-android$API-clang$CMD"
export AR_aarch64_linux_android="$BIN/llvm-ar$EXE"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CC_aarch64_linux_android"
# 16 KB page devices (Android 15+) need 16 KB-aligned LOAD segments.
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384"

cargo rustc -p birdman64 --lib --crate-type cdylib --target aarch64-linux-android \
    ${CARGO_PROFILE[@]+"${CARGO_PROFILE[@]}"} ${FEATURES[@]+"${FEATURES[@]}"}
if [ "$FLAVOUR" = release ]; then
    # This shell's GNU tar (xz support); Windows' tar.exe hangs on the .xz.
    TAR=$(command -v tar)
    if command -v cygpath > /dev/null; then TAR=$(cygpath -w "$TAR"); fi
    PW64_TAR="$TAR" cargo run -p pw64-cbuild --example get_zig --features fetch -- --android
fi

BT=$(ls -d "$SDK"/build-tools/* | sort -V | tail -n 1)
PLATFORM=$(ls -d "$SDK"/platforms/android-* | sort -V | tail -n 1)
VERSION=$(grep -m 1 '^version' Cargo.toml | cut -d '"' -f 2)
OUT=target/android-apk
rm -rf "$OUT/stage" "$OUT/unsigned.apk" "$OUT/aligned.apk"
mkdir -p "$OUT/stage/lib/arm64-v8a"
cp "target/aarch64-linux-android/$PROFILE/libbirdman64.so" "$OUT/stage/lib/arm64-v8a/"
LIBS=(lib/arm64-v8a/libbirdman64.so)
if [ "$FLAVOUR" = release ]; then
    # zig (static aarch64-linux executable, unmodified) under a lib*.so name:
    # the installer extracts it to nativeLibraryDir, the one app-reachable
    # place Android 10+ lets an app exec from. Its MIT licence rides along.
    cp tools/zig-android/zig "$OUT/stage/lib/arm64-v8a/libzig.so"
    mkdir -p "$OUT/stage/assets/licenses"
    cp tools/zig-android/LICENSE "$OUT/stage/assets/licenses/zig-LICENSE.txt"
    LIBS+=(lib/arm64-v8a/libzig.so assets/licenses/zig-LICENSE.txt)
fi

# --debug-mode: android:debuggable (adb run-as, logs); this is a dev APK.
"$BT/aapt2$EXE" link -o "$OUT/unsigned.apk" \
    --manifest packaging/android/AndroidManifest.xml \
    -I "$PLATFORM/android.jar" --version-name "$VERSION" --debug-mode
(cd "$OUT/stage" && "$BT/aapt$EXE" add ../unsigned.apk "${LIBS[@]}" > /dev/null)
"$BT/zipalign$EXE" -P 16 -f 4 "$OUT/unsigned.apk" "$OUT/aligned.apk"

KS="$OUT/debug.keystore"
if [ ! -f "$KS" ]; then
    keytool -genkeypair -keystore "$KS" -storepass android -keypass android \
        -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 \
        -dname "CN=Android Debug,O=Android,C=US" > /dev/null
fi
"$BT/apksigner$BAT" sign --ks "$KS" --ks-pass pass:android --key-pass pass:android \
    --out "$OUT/birdman64.apk" "$OUT/aligned.apk"
echo "$OUT/birdman64.apk"
