#!/usr/bin/env bash
# Writes a distributable Swift package: bindings/swift with the generated sources included.
#
#   scripts/make-swift-package.sh OUT_DIR XCFRAMEWORK_URL CHECKSUM
#   scripts/make-swift-package.sh --local OUT_DIR
#
# The first form references the XCFramework by URL and checksum; the release workflow pushes it
# to the distribution repository, so apps can depend on it by version. CHECKSUM is
# `swift package compute-checksum OsaurusSyncFFI.xcframework.zip`. The --local form copies the
# XCFramework into OUT_DIR and references it by path, so the package works as a local package
# without access to the release assets. The generated Swift bindings (and, for --local, the
# XCFramework) must already be in bindings/swift (scripts/build-xcframework.sh).
set -euo pipefail

usage() {
  sed -n '2,12p' "$0" >&2
  exit 2
}

LOCAL=false
if [ "${1:-}" = --local ]; then
  [ $# -eq 2 ] || usage
  LOCAL=true OUT="$2"
else
  [ $# -eq 3 ] || usage
  OUT="$1" URL="$2" CHECKSUM="$3"
fi
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SWIFT_DIR="$ROOT/bindings/swift"

if [ ! -f "$SWIFT_DIR/Sources/OsaurusSync/Generated/OsaurusSync.swift" ]; then
  echo "missing generated Swift bindings; run scripts/build-xcframework.sh first" >&2
  exit 1
fi
if $LOCAL; then
  if [ ! -d "$SWIFT_DIR/OsaurusSyncFFI.xcframework" ]; then
    echo "missing OsaurusSyncFFI.xcframework; run scripts/build-xcframework.sh first" >&2
    exit 1
  fi
elif ! [[ "$CHECKSUM" =~ ^[0-9a-f]{64}$ ]]; then
  echo "checksum must be 64 hex characters" >&2
  exit 1
fi

rm -rf "$OUT/Sources" "$OUT/OsaurusSyncFFI.xcframework"
mkdir -p "$OUT"
cp -R "$SWIFT_DIR/Sources" "$OUT/Sources"
cp "$SWIFT_DIR/README.md" "$OUT/README.md"
for f in LICENSE THIRD_PARTY_NOTICES.md; do
  if [ -f "$ROOT/$f" ]; then cp "$ROOT/$f" "$OUT/$f"; fi
done

# The release manifest: the binary target (remote, or bundled for --local), and no test target
# (the tests need the repository's fixtures and run in CI instead).
if $LOCAL; then
  cp -R "$SWIFT_DIR/OsaurusSyncFFI.xcframework" "$OUT/OsaurusSyncFFI.xcframework"
  BINARY='.binaryTarget(name: "OsaurusSyncFFI", path: "OsaurusSyncFFI.xcframework")'
else
  BINARY=".binaryTarget(name: \"OsaurusSyncFFI\", url: \"$URL\", checksum: \"$CHECKSUM\")"
fi
BINARY="$BINARY" perl -0pe '
  s{\.binaryTarget\(name: "OsaurusSyncFFI", path: "[^"]+"\)}{$ENV{BINARY}} or die "binaryTarget not found\n";
  s{\n\s*// Built by scripts/build-xcframework\.sh\.}{};
  s{,\n\s*\.testTarget\(.*?\n\s*\),\n}{,\n}s or die "testTarget not found\n";
' "$SWIFT_DIR/Package.swift" >"$OUT/Package.swift"

echo "wrote $OUT"
