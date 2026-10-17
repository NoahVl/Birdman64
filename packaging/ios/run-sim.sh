#!/usr/bin/env bash
# Runs the Birdman64 simulator app on an iPhone simulator (macOS only) and,
# when a ROM is given, copies it into the app's Documents/rom folder first.
#
# Usage: packaging/ios/run-sim.sh [--app <Birdman64.app or .zip>] [<rom file>]
#
#   --app  the app bundle to install: a .app directory, or a .zip (unpacked
#          with ditto -x -k into a temp dir, quarantine attrs cleared with
#          xattr). Without it, the simulator app is built with
#          `packaging/ios/build-ipa.sh --sim` first.
#   rom    a game file (.z64/.n64/.v64, or a .zip containing one).
#
# Click with the mouse = touch. The app's log lands in the app container's
# Documents/birdman64.log (printed below; birdman64-previous.log holds the
# previous run). Debugging the CI smoke run instead: artifacts ios-sim*.
set -euo pipefail
[ "$(uname -s)" = Darwin ] || { echo "error: this script needs macOS (xcrun/simctl)" >&2; exit 1; }
cd "$(dirname "$0")/../.."

APP=
TMP=
ROM=
while [ $# -gt 0 ]; do
    case "$1" in
        --app)
            [ $# -ge 2 ] || { echo "error: --app needs a path" >&2; exit 1; }
            APP=$2; shift 2 ;;
        *) ROM=$1; shift ;;
    esac
done

if [ -z "$APP" ]; then
    bash packaging/ios/build-ipa.sh --sim
    APP=target/ios/sim/Payload/Birdman64.app
fi
trap '[ -z "$TMP" ] || rm -rf "$TMP"' EXIT
if [ "${APP##*.}" = zip ]; then
    TMP=$(mktemp -d /tmp/birdman64-sim.XXXXXX)
    ditto -x -k "$APP" "$TMP"
    APP="$TMP/Birdman64.app"
    [ -d "$APP" ] || APP=$(find "$TMP" -type d -name '*.app' | head -n 1)
    [ -d "$APP" ] || { echo "error: no .app bundle in the zip" >&2; exit 1; }
fi
# Simulators refuse quarantined bundles.
xattr -cr "$APP"

UDID=$(xcrun simctl list devices available -j \
    | python3 -c 'import json,sys;d=json.load(sys.stdin)["devices"];print(next(x["udid"] for k in d if k.startswith("com.apple.CoreSimulator.SimRuntime.iOS") for x in d[k] if x["name"].startswith("iPhone")))') \
    || { echo "no available iOS simulator"; exit 1; }
if ! xcrun simctl list devices booted | grep -q "$UDID"; then
    xcrun simctl boot "$UDID"
fi
xcrun simctl bootstatus "$UDID" -b
open -a Simulator

xcrun simctl install "$UDID" "$APP"
# `data` is the container root; the app's data dir is its Documents folder.
DOCS="$(xcrun simctl get_app_container "$UDID" app.birdman64 data)/Documents"
if [ -n "$ROM" ]; then
    mkdir -p "$DOCS/rom"
    cp "$ROM" "$DOCS/rom/"
fi
xcrun simctl launch "$UDID" app.birdman64
echo "App log: $DOCS/birdman64.log (previous run: birdman64-previous.log)"
echo "Show it in the Files app or: open \"$DOCS\""
echo "Click with the mouse = touch."
