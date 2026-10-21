#!/usr/bin/env bash
# Builds an iOS .app bundle / unsigned .ipa of Birdman64: the birdman64 binary
# behind a hand-rolled Payload/Birdman64.app (Info.plist from Info.plist.in,
# icons resized with sips from assets/icon/birdman64-source.png — the source
# the Android launcher icons come from). No Xcode project.
#
# Needs macOS with Xcode (the iOS SDK) and rustup; the decomp submodule, the
# Rust target and zig are fetched on first use. No Mac: the "Build iOS app"
# workflow (.github/workflows/ios-ipa.yml) runs this in your own private copy.
#
# Usage: packaging/ios/build-ipa.sh [--sim] [--debug]
#
# Default: release device build, zipped as target/ios/Birdman64.ipa
# (unsigned; sign with Xcode/ideviceinstaller or AltStore to install).
# --sim: aarch64-apple-ios-sim → target/ios/<sim|device>/... .app left
# unzipped, ad-hoc signed (codesign -s -), as the simulator requires.
# --debug: cargo debug profile instead of release.
set -euo pipefail
cd "$(dirname "$0")/../.."

if [ "$(uname -s)" != Darwin ]; then
    echo "error: this script needs macOS (Xcode toolchain, plutil, sips, codesign)" >&2
    exit 1
fi

SIM=
PROFILE=release
CARGO_PROFILE=(--release)
while [ $# -gt 0 ]; do
    case "$1" in
        --sim) SIM=1 ;;
        --debug) PROFILE=debug CARGO_PROFILE=() ;;
        *) echo "error: unknown argument $1" >&2; exit 1 ;;
    esac
    shift
done

TARGET=aarch64-apple-ios
OUT=target/ios/device
[ -n "$SIM" ] && TARGET=aarch64-apple-ios-sim OUT=target/ios/sim

# First-time setup, so a fresh clone builds with this one command: the decomp
# sources (submodule), the Rust target and zig (compiles the game C).
[ -f decomp/Makefile ] || git submodule update --init decomp
rustup target add "$TARGET"
if [ -z "${PW64_ZIG:-}" ] && [ ! -x tools/zig/zig ]; then
    cargo run -q -p pw64-cbuild --example get_zig --features fetch
fi

cargo build -p birdman64 --bin birdman64 --target "$TARGET" \
    ${CARGO_PROFILE[@]+"${CARGO_PROFILE[@]}"}
VERSION=$(grep -m 1 '^version' Cargo.toml | cut -d '"' -f 2)

APP="$OUT/Payload/Birdman64.app"
rm -rf "$APP"
mkdir -p "$APP"
BIN="target/$TARGET/$PROFILE/birdman64"
cp "$BIN" "$APP/Birdman64"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@EXECUTABLE@/Birdman64/g" \
    packaging/ios/Info.plist.in > "$APP/Info.plist"
plutil -lint "$APP/Info.plist"

# Icons: iPhone home-screen slots (CFBundleIconFiles AppIcon60x60) at 2x/3x.
sips -z 120 120 assets/icon/birdman64-source.png --out "$APP/AppIcon60x60@2x.png" > /dev/null
sips -z 180 180 assets/icon/birdman64-source.png --out "$APP/AppIcon60x60@3x.png" > /dev/null
cp LICENSE README.md "$APP/"

if [ -n "$SIM" ]; then
    # The simulator refuses unsigned bundles; no profile/signing identity needed.
    codesign -s - --force "$APP"
    echo "$APP"
else
    IPA=target/ios/Birdman64.ipa
    rm -f "$IPA"
    (cd "$OUT" && zip -qr ../Birdman64.ipa Payload)
    echo "$IPA"
fi
