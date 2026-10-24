#!/usr/bin/env bash
# Writes a distributable Swift package: bindings/swift with the generated sources included and
# the XCFramework referenced by URL and checksum instead of a local path.
#
#   scripts/make-swift-package.sh OUT_DIR XCFRAMEWORK_URL CHECKSUM
#
# CHECKSUM is `swift package compute-checksum OsaurusSyncFFI.xcframework.zip`. The generated
# Swift bindings must already be in bindings/swift/Sources/OsaurusSync/Generated
# (scripts/build-xcframework.sh). The release workflow pushes OUT_DIR to the distribution
# repository, so apps can depend on it by version.
set -euo pipefail

if [ $# -ne 3 ]; then
  sed -n '2,10p' "$0" >&2
  exit 2
fi
OUT="$1" URL="$2" CHECKSUM="$3"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SWIFT_DIR="$ROOT/bindings/swift"

if [ ! -f "$SWIFT_DIR/Sources/OsaurusSync/Generated/OsaurusSync.swift" ]; then
  echo "missing generated Swift bindings; run scripts/build-xcframework.sh first" >&2
  exit 1
fi
if ! [[ "$CHECKSUM" =~ ^[0-9a-f]{64}$ ]]; then
  echo "checksum must be 64 hex characters" >&2
  exit 1
fi

rm -rf "$OUT/Sources"
mkdir -p "$OUT"
cp -R "$SWIFT_DIR/Sources" "$OUT/Sources"
cp "$SWIFT_DIR/README.md" "$OUT/README.md"
for f in LICENSE THIRD_PARTY_NOTICES.md; do
  if [ -f "$ROOT/$f" ]; then cp "$ROOT/$f" "$OUT/$f"; fi
done

# The release manifest: the remote binary target, and no test target (the tests need the
# repository's fixtures and run in CI instead).
URL="$URL" CHECKSUM="$CHECKSUM" perl -0pe '
  s{\.binaryTarget\(name: "OsaurusSyncFFI", path: "[^"]+"\)}{.binaryTarget(name: "OsaurusSyncFFI", url: "$ENV{URL}", checksum: "$ENV{CHECKSUM}")} or die "binaryTarget not found\n";
  s{\n\s*// Built by scripts/build-xcframework\.sh\.}{};
  s{,\n\s*\.testTarget\(.*?\n\s*\),\n}{,\n}s or die "testTarget not found\n";
' "$SWIFT_DIR/Package.swift" >"$OUT/Package.swift"

echo "wrote $OUT"
