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
#        packaging/android/build-apk.sh --sign-only <unsigned.apk>
#
# Default: dev APK → target/android-apk/birdman64.apk: game C linked into
# libbirdman64.so, debuggable, local debug key (never distribute).
# --release-flavour: no game C (feature first-run); the app builds the game
# module on the device with the bundled zig, shipped as
# lib/arm64-v8a/libzig.so (get_zig --android → tools/zig-android/). Not
# debuggable, debug info stripped, licences under assets/licenses/ (needs
# cargo-about 0.9.2 and network for the LLVM licence). Never touches a
# release key: → target/android-apk/Birdman64-android-arm64-unsigned.apk
# (aligned) + Birdman64-android-arm64-debugsigned.apk (local debug key; test
# builds only: a phone can't update it in place to a release-signed APK).
# --sign-only: signs an unsigned APK with the release key from
# ANDROID_KEYSTORE (path) or ANDROID_KEYSTORE_B64 (base64), with
# ANDROID_KEYSTORE_PASSWORD (store = key password) and ANDROID_KEY_ALIAS →
# Birdman64-android-arm64.apk next to it; without them the debug key →
# *-debugsigned.apk. Needs only the SDK, so CI hands the secrets to that
# step alone (no cargo/build scripts see them).
set -euo pipefail
cd "$(dirname "$0")/../.."

PROFILE=release
CARGO_PROFILE=(--release)
FEATURES=()
FLAVOUR=dev
SIGN_ONLY=
while [ $# -gt 0 ]; do
    case "$1" in
        --debug) PROFILE=debug CARGO_PROFILE=() ;;
        --release-flavour)
            FLAVOUR=release
            FEATURES=(--no-default-features --features first-run)
            ;;
        --sign-only)
            SIGN_ONLY="${2:?--sign-only needs the unsigned APK}"
            shift
            ;;
        *) echo "error: unknown argument $1" >&2; exit 1 ;;
    esac
    shift
done
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
[ -d "$SDK" ] || { echo "error: set ANDROID_HOME to the Android SDK" >&2; exit 1; }
# = minSdkVersion in AndroidManifest.xml and pw64-cbuild's zig target.
API=26

case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) HOST=windows-x86_64 EXE=.exe CMD=.cmd BAT=.bat ;;
    Darwin) HOST=darwin-x86_64 EXE= CMD= BAT= ;;
    *) HOST=linux-x86_64 EXE= CMD= BAT= ;;
esac
BT=$(ls -d "$SDK"/build-tools/* | sort -V | tail -n 1)
OUT=target/android-apk

# Signs aligned APK $1 into $2 with the debug key ($3 = debug) or the release
# key from the environment ($3 = release). The decoded keystore lives in a
# temp file outside the repo, removed on exit (trap set before decoding).
KS_TMP=
trap '[ -z "$KS_TMP" ] || rm -f "$KS_TMP"' EXIT
sign_apk() {
    local in="$1" out="$2" key=()
    if [ "$3" = release ]; then
        : "${ANDROID_KEYSTORE_PASSWORD:?set ANDROID_KEYSTORE_PASSWORD}"
        : "${ANDROID_KEY_ALIAS:?set ANDROID_KEY_ALIAS}"
        local ks="${ANDROID_KEYSTORE:-}"
        if [ -n "${ANDROID_KEYSTORE_B64:-}" ]; then
            KS_TMP=$(mktemp "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/birdman64-ks.XXXXXX")
            printf '%s' "$ANDROID_KEYSTORE_B64" | base64 -d > "$KS_TMP"
            ks="$KS_TMP"
        fi
        if command -v cygpath > /dev/null; then ks=$(cygpath -w "$ks"); fi
        key=(--ks "$ks" --ks-key-alias "$ANDROID_KEY_ALIAS"
            --ks-pass env:ANDROID_KEYSTORE_PASSWORD --key-pass env:ANDROID_KEYSTORE_PASSWORD)
    else
        local dks="$OUT/debug.keystore"
        if [ ! -f "$dks" ]; then
            keytool -genkeypair -keystore "$dks" -storepass android -keypass android \
                -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 \
                -dname "CN=Android Debug,O=Android,C=US" > /dev/null
        fi
        key=(--ks "$dks" --ks-pass pass:android --key-pass pass:android)
    fi
    rm -f "$out"
    "$BT/apksigner$BAT" sign "${key[@]}" \
        --v1-signing-enabled false --v2-signing-enabled true --v3-signing-enabled true \
        --v4-signing-enabled false --out "$out" "$in"
    "$BT/apksigner$BAT" verify "$out"
    # ANDROID_CERT_SHA256 (release.yml): the expected signer certificate's
    # SHA-256 (colons/case ignored). A wrong key would produce an APK that
    # phones refuse to install over the previous release, so fail instead.
    if [ "$3" = release ] && [ -n "${ANDROID_CERT_SHA256:-}" ]; then
        local want got
        want=$(printf '%s' "$ANDROID_CERT_SHA256" | tr -d ': ' | tr 'A-F' 'a-f')
        got=$("$BT/apksigner$BAT" verify --print-certs "$out" |
            sed -n 's/^Signer #1 certificate SHA-256 digest: *//p' | tr -d ': \r' | tr 'A-F' 'a-f')
        if [ "$got" != "$want" ]; then
            echo "error: APK signer certificate SHA-256 is '$got', expected '$want'" >&2
            rm -f "$out"
            exit 1
        fi
        echo "signer certificate SHA-256 matches the pinned fingerprint"
    fi
    echo "$out"
}

if [ -n "$SIGN_ONLY" ]; then
    [ -f "$SIGN_ONLY" ] || { echo "error: $SIGN_ONLY not found" >&2; exit 1; }
    mkdir -p "$OUT"
    # apksigner after zipalign (16 KB for the .so pages); idempotent.
    "$BT/zipalign$EXE" -P 16 -f 4 "$SIGN_ONLY" "$OUT/sign-aligned.apk"
    DIR=$(dirname "$SIGN_ONLY")
    if [ -n "${ANDROID_KEYSTORE_B64:-}${ANDROID_KEYSTORE:-}" ]; then
        sign_apk "$OUT/sign-aligned.apk" "$DIR/Birdman64-android-arm64.apk" release
    else
        echo "WARNING: no release keystore (ANDROID_KEYSTORE[_B64]): signing with the" >&2
        echo "WARNING: local DEBUG key. Never publish the result." >&2
        sign_apk "$OUT/sign-aligned.apk" "$DIR/Birdman64-android-arm64-debugsigned.apk" debug
    fi
    rm -f "$OUT/sign-aligned.apk"
    exit 0
fi

NDK="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
[ -d "$NDK" ] || { echo "error: set ANDROID_NDK_HOME to the Android NDK" >&2; exit 1; }
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
if [ "$FLAVOUR" = release ]; then
    # Panic/file paths in the shipped library without the builder's user dir:
    # home, then the more specific workspace and cargo home (last match wins).
    winpath() { if command -v cygpath > /dev/null; then cygpath -w "$1"; else echo "$1"; fi; }
    for p in "$HOME=~" "$PWD=." "${CARGO_HOME:-$HOME/.cargo}=cargo"; do
        CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS+=" --remap-path-prefix=$(winpath "${p%=*}")=${p##*=}"
    done
fi

cargo rustc -p birdman64 --lib --crate-type cdylib --target aarch64-linux-android \
    ${CARGO_PROFILE[@]+"${CARGO_PROFILE[@]}"} ${FEATURES[@]+"${FEATURES[@]}"}
if [ "$FLAVOUR" = release ]; then
    # This shell's GNU tar (xz support); Windows' tar.exe hangs on the .xz.
    TAR=$(command -v tar)
    if command -v cygpath > /dev/null; then TAR=$(cygpath -w "$TAR"); fi
    PW64_TAR="$TAR" cargo run -p pw64-cbuild --example get_zig --features fetch -- --android
    # Same LLVM licence as release.yml (LLVM_LICENSE_URL): zig bundles
    # clang/LLD 21.
    LLVM_LICENSE=tools/zig-android/LLVM-LICENSE.txt
    if [ ! -s "$LLVM_LICENSE" ]; then
        curl -fsSL -o "$LLVM_LICENSE" \
            https://raw.githubusercontent.com/llvm/llvm-project/llvmorg-21.1.0/llvm/LICENSE.TXT
    fi
fi

PLATFORM=$(ls -d "$SDK"/platforms/android-* | sort -V | tail -n 1)
VERSION=$(grep -m 1 '^version' Cargo.toml | cut -d '"' -f 2)
# versionCode from the semver (1.2.3 → 10203): every release must be higher
# than the installed one for an in-place update.
IFS=. read -r V_MAJ V_MIN V_PAT <<< "${VERSION%%-*}"
VERSION_CODE=$((V_MAJ * 10000 + V_MIN * 100 + V_PAT))
rm -rf "$OUT/stage" "$OUT/unsigned.apk" "$OUT/aligned.apk"
mkdir -p "$OUT/stage/lib/arm64-v8a"
cp "target/aarch64-linux-android/$PROFILE/libbirdman64.so" "$OUT/stage/lib/arm64-v8a/"
LIBS=(lib/arm64-v8a/libbirdman64.so)
LINK_FLAGS=()
if [ "$FLAVOUR" = release ]; then
    # Debug info is ~3/4 of the library; crash reports symbolize offline.
    "$BIN/llvm-strip$EXE" --strip-debug "$OUT/stage/lib/arm64-v8a/libbirdman64.so"
    # zig (static aarch64-linux executable, unmodified) under a lib*.so name:
    # the installer extracts it to nativeLibraryDir, the one app-reachable
    # place Android 10+ lets an app exec from.
    cp tools/zig-android/zig "$OUT/stage/lib/arm64-v8a/libzig.so"
    LIBS+=(lib/arm64-v8a/libzig.so)
    # What the desktop release zip ships (release.yml), under assets/licenses/.
    L="$OUT/stage/assets/licenses"
    mkdir -p "$L"
    cp tools/zig-android/LICENSE "$L/zig-LICENSE.txt"
    cp tools/zig-android/LLVM-LICENSE.txt "$L/LLVM-LICENSE.txt"
    cp LICENSE "$L/LICENSE.txt"
    cp packaging/DECOMP-LICENSE-NOTE.txt "$L/DECOMP-LICENSE-NOTE.txt"
    if ! cargo about --version > /dev/null 2>&1; then
        echo "error: needs cargo-about: cargo install cargo-about --locked --version 0.9.2 --features cli" >&2
        exit 1
    fi
    cargo about generate about.hbs --output-file "$L/THIRD_PARTY_LICENSES.html"
    for f in zig-LICENSE.txt LLVM-LICENSE.txt LICENSE.txt DECOMP-LICENSE-NOTE.txt \
        THIRD_PARTY_LICENSES.html; do
        [ -s "$L/$f" ] || { echo "error: licence file $f missing" >&2; exit 1; }
        LIBS+=("assets/licenses/$f")
    done
else
    # android:debuggable (adb run-as, logs): dev APK only.
    LINK_FLAGS=(--debug-mode)
fi

# Launcher icon (packaging/android/res, generated by assets/icon/render.py).
rm -f "$OUT/res.zip"
"$BT/aapt2$EXE" compile --dir packaging/android/res -o "$OUT/res.zip"
"$BT/aapt2$EXE" link -o "$OUT/unsigned.apk" \
    --manifest packaging/android/AndroidManifest.xml -R "$OUT/res.zip" \
    -I "$PLATFORM/android.jar" --version-name "$VERSION" --version-code "$VERSION_CODE" \
    ${LINK_FLAGS[@]+"${LINK_FLAGS[@]}"}
(cd "$OUT/stage" && "$BT/aapt$EXE" add ../unsigned.apk "${LIBS[@]}" > /dev/null)
"$BT/zipalign$EXE" -P 16 -f 4 "$OUT/unsigned.apk" "$OUT/aligned.apk"

# The build never signs with the release key (only --sign-only does): a
# stable key is what lets a phone update the app in place; losing it means
# players must uninstall (and lose their saves) to update.
if [ "$FLAVOUR" = release ]; then
    cp "$OUT/aligned.apk" "$OUT/Birdman64-android-arm64-unsigned.apk"
    echo "$OUT/Birdman64-android-arm64-unsigned.apk"
    echo "WARNING: test copy signed with the local DEBUG key; release-sign with --sign-only." >&2
    sign_apk "$OUT/aligned.apk" "$OUT/Birdman64-android-arm64-debugsigned.apk" debug
else
    sign_apk "$OUT/aligned.apk" "$OUT/birdman64.apk" debug
fi
