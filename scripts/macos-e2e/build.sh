#!/bin/sh
# Builds "AGT Test Driver.app" (see README.md) into the output dir (default: target/macos-e2e).
# Set AGT_SIGN_IDENTITY to a code-signing identity so its Accessibility grant survives rebuilds.
set -eu
cd "$(dirname "$0")"
OUT=${1:-../../target/macos-e2e}
APP="$OUT/AGT Test Driver.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
swiftc -O main.swift -o "$APP/Contents/MacOS/AGTTestDriver"
cat > "$APP/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>io.notics.agt.testdriver</string>
    <key>CFBundleName</key><string>AGT Test Driver</string>
    <key>CFBundleExecutable</key><string>AGTTestDriver</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>1.0</string>
    <key>LSMinimumSystemVersion</key><string>13.4</string>
</dict>
</plist>
EOF
codesign --force --options runtime --timestamp=none --sign "${AGT_SIGN_IDENTITY:--}" "$APP"
echo "built $APP"
