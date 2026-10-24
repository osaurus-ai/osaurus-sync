#!/usr/bin/env bash
# Builds OsaurusSyncFFI.xcframework (macOS arm64, iOS device, iOS simulator arm64) and
# regenerates the Swift bindings in bindings/swift.
#
#   scripts/build-xcframework.sh            # release
#   PROFILE=dev scripts/build-xcframework.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PROFILE="${PROFILE:-release}"
OUT_DIR=$([ "$PROFILE" = "dev" ] && echo debug || echo "$PROFILE")
SWIFT_DIR="$ROOT/bindings/swift"
GEN_DIR="$ROOT/target/uniffi-swift"
XCFRAMEWORK="$SWIFT_DIR/OsaurusSyncFFI.xcframework"
LIB=libosaurus_sync_ffi

export MACOSX_DEPLOYMENT_TARGET=14.0
export IPHONEOS_DEPLOYMENT_TARGET=17.0

TARGETS=(aarch64-apple-darwin aarch64-apple-ios aarch64-apple-ios-sim)
for t in "${TARGETS[@]}"; do
  echo "==> building $t ($PROFILE)"
  if [ "$t" = aarch64-apple-darwin ]; then
    # The dylib is only used to read the UniFFI metadata.
    cargo build -p osaurus-sync-ffi --lib --profile "$PROFILE" --target "$t"
  else
    cargo rustc -p osaurus-sync-ffi --lib --profile "$PROFILE" --target "$t" --crate-type staticlib
  fi
done

echo "==> generating Swift bindings"
rm -rf "$GEN_DIR"
cargo run -q -p osaurus-sync-ffi --features cli --bin uniffi-bindgen -- generate \
  --library "target/aarch64-apple-darwin/$OUT_DIR/$LIB.dylib" \
  --language swift --out-dir "$GEN_DIR"

HEADERS="$GEN_DIR/headers"
mkdir -p "$HEADERS"
cp "$GEN_DIR/OsaurusSyncFFI.h" "$HEADERS/"
cp "$GEN_DIR/OsaurusSyncFFI.modulemap" "$HEADERS/module.modulemap"

echo "==> creating $XCFRAMEWORK"
rm -rf "$XCFRAMEWORK"
args=()
for t in "${TARGETS[@]}"; do
  args+=(-library "target/$t/$OUT_DIR/$LIB.a" -headers "$HEADERS")
done
xcodebuild -create-xcframework "${args[@]}" -output "$XCFRAMEWORK" >/dev/null

mkdir -p "$SWIFT_DIR/Sources/OsaurusSync/Generated"
cp "$GEN_DIR/OsaurusSync.swift" "$SWIFT_DIR/Sources/OsaurusSync/Generated/OsaurusSync.swift"

echo "==> done"
du -sh "$XCFRAMEWORK"
