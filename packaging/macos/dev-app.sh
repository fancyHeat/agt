#!/bin/sh
# Builds AGT.app for local development and testing on macOS (the release build uses `vpk pack`,
# see .github/workflows/release.yml).
#
#   packaging/macos/dev-app.sh [output dir]        # default: target/release
#
# Set AGT_SIGN_IDENTITY to a code-signing identity from `security find-identity -v -p codesigning`
# (e.g. "Developer ID Application: Name (TEAMID)"). macOS keys the Microphone and Accessibility
# permissions to the app's signature: with a real identity (even a self-signed one) they survive
# rebuilds; ad hoc signing (no identity) gets a new signature every build, so macOS asks again.
set -eu
cd "$(dirname "$0")/../.."
OUT=${1:-target/release}
VERSION=$(cargo pkgid -p agt | sed 's/.*[@#]//')

cargo build --release --locked -p agt

APP="$OUT/AGT.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/agt "$APP/Contents/MacOS/agt"
sed "s/__VERSION__/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"

# Same as vpk: hardened runtime and our entitlements only.
codesign --force --options runtime --timestamp=none \
    --entitlements packaging/macos/agt.entitlements \
    --sign "${AGT_SIGN_IDENTITY:--}" "$APP"
codesign --verify --strict "$APP"
echo "built $APP ($VERSION, signed by ${AGT_SIGN_IDENTITY:-ad hoc})"
