#!/usr/bin/env bash
# Runs the Swift XCTests inside an iOS Simulator without an Xcode project, for machines where
# `xcodebuild test` cannot target iOS (for example when only older simulator runtimes are
# installed). CI uses `xcodebuild test` instead.
#
#   scripts/build-xcframework.sh && scripts/test-ios-sim.sh [simulator-udid]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SWIFT_DIR="$ROOT/bindings/swift"
BUILD="$ROOT/target/swift-ios-sim"
SDK="$(xcrun --sdk iphonesimulator --show-sdk-path)"
AGENT="$(xcode-select -p)/Platforms/iPhoneSimulator.platform/Developer/Library/Xcode/Agents/xctest"

UDID="${1:-}"
if [ -z "$UDID" ]; then
  RUNTIME="$(xcrun simctl list runtimes -j | python3 -c 'import json,sys; r=[x for x in json.load(sys.stdin)["runtimes"] if x["platform"]=="iOS" and x["isAvailable"]]; print(r[-1]["identifier"])')"
  UDID="$(xcrun simctl create osaurus-sync-test com.apple.CoreSimulator.SimDeviceType.iPhone-16 "$RUNTIME")"
  CREATED=1
fi

(cd "$SWIFT_DIR" && swift build --build-tests --triple arm64-apple-ios17.0-simulator --sdk "$SDK" --scratch-path "$BUILD")

# SwiftPM lays the bundle out macOS-style; the simulator expects a flat iOS bundle.
SRC="$BUILD/arm64-apple-ios-simulator/debug/OsaurusSyncPackageTests.xctest"
DST="$BUILD/flat/OsaurusSyncPackageTests.xctest"
rm -rf "$BUILD/flat" && mkdir -p "$DST"
cp "$SRC/Contents/MacOS/OsaurusSyncPackageTests" "$DST/"
cat > "$DST/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>OsaurusSyncPackageTests</string>
<key>CFBundleIdentifier</key><string>ai.osaurus.sync.tests</string>
<key>CFBundlePackageType</key><string>BNDL</string>
<key>CFBundleSupportedPlatforms</key><array><string>iPhoneSimulator</string></array>
</dict></plist>
EOF

xcrun simctl boot "$UDID" 2>/dev/null || true
xcrun simctl bootstatus "$UDID" -b >/dev/null
set +e
xcrun simctl spawn "$UDID" "$AGENT" "$DST"
STATUS=$?
set -e
xcrun simctl shutdown "$UDID" || true
if [ "${CREATED:-0}" = 1 ]; then xcrun simctl delete "$UDID"; fi
exit $STATUS
