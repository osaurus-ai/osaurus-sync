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
STATIC_PROFILE=$([ "$PROFILE" = "release" ] && echo staticlib || echo "$PROFILE")
STATIC_DIR=$([ "$STATIC_PROFILE" = "dev" ] && echo debug || echo "$STATIC_PROFILE")
SWIFT_DIR="$ROOT/bindings/swift"
GEN_DIR="$ROOT/target/uniffi-swift"
XCFRAMEWORK="$SWIFT_DIR/OsaurusSyncFFI.xcframework"
LIB=libosaurus_sync_ffi

export MACOSX_DEPLOYMENT_TARGET=14.0
export IPHONEOS_DEPLOYMENT_TARGET=17.0

TARGETS=(aarch64-apple-darwin aarch64-apple-ios aarch64-apple-ios-sim)
# The dylib is only used to read the UniFFI metadata.
echo "==> building the metadata dylib ($PROFILE)"
cargo build -p osaurus-sync-ffi --lib --profile "$PROFILE" --target aarch64-apple-darwin

# Each static library is pre-linked into one object with only the UniFFI symbols global. The
# library bundles its own SQLCipher, Rust standard library and dependencies; left global, they
# would collide with (or silently bind to) an app's own SQLite and Rust code.
SYMBOLS="$ROOT/target/uniffi-exports.txt"
printf '%s\n' '_uniffi_osaurus_sync_ffi_*' '_ffi_osaurus_sync_ffi_*' > "$SYMBOLS"
for t in "${TARGETS[@]}"; do
  echo "==> building $t ($STATIC_PROFILE)"
  cargo rustc -p osaurus-sync-ffi --lib --profile "$STATIC_PROFILE" --target "$t" --crate-type staticlib
  case "$t" in
    aarch64-apple-darwin) platform="macos $MACOSX_DEPLOYMENT_TARGET" ;;
    aarch64-apple-ios) platform="ios $IPHONEOS_DEPLOYMENT_TARGET" ;;
    aarch64-apple-ios-sim) platform="ios-simulator $IPHONEOS_DEPLOYMENT_TARGET" ;;
  esac
  dir="target/$t/$STATIC_DIR"
  # shellcheck disable=SC2086
  xcrun ld -r -arch arm64 -platform_version $platform ${platform#* } -force_load "$dir/$LIB.a" \
    -exported_symbols_list "$SYMBOLS" -o "$dir/$LIB-prelinked.o"
  mkdir -p "$dir/prelinked"
  rm -f "$dir/prelinked/$LIB.a"
  xcrun libtool -static -o "$dir/prelinked/$LIB.a" "$dir/$LIB-prelinked.o" 2>/dev/null
  if xcrun nm --no-llvm-bc -gU "$dir/prelinked/$LIB.a" | awk 'NF == 3 { print $3 }' | grep -Ev '^_(uniffi|ffi)_osaurus_sync_ffi_' | grep -q .; then
    echo "unexpected global symbols in $t" >&2
    exit 1
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
  args+=(-library "target/$t/$STATIC_DIR/prelinked/$LIB.a" -headers "$HEADERS")
done
xcodebuild -create-xcframework "${args[@]}" -output "$XCFRAMEWORK" >/dev/null

mkdir -p "$SWIFT_DIR/Sources/OsaurusSync/Generated"
cp "$GEN_DIR/OsaurusSync.swift" "$SWIFT_DIR/Sources/OsaurusSync/Generated/OsaurusSync.swift"

echo "==> done"
du -sh "$XCFRAMEWORK"
