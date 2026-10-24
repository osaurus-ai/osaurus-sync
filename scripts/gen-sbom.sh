#!/usr/bin/env bash
# Writes a CycloneDX 1.5 SBOM of the native library for each shipped target, using
# cargo-cyclonedx. Build-only dependencies are left out.
#
#   scripts/gen-sbom.sh [OUT_DIR]    # default: dist/sbom
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/dist/sbom}"
cd "$ROOT"
mkdir -p "$OUT"
for t in aarch64-apple-darwin aarch64-apple-ios aarch64-apple-ios-sim x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
  cargo cyclonedx --manifest-path crates/osaurus-sync-ffi/Cargo.toml -f json --spec-version 1.5 \
    --no-build-deps --target "$t" --target-in-filename --license-accept-named UNLICENSED -q
  # cargo-cyclonedx writes one file per workspace member; only the library's is the product.
  mv "crates/osaurus-sync-ffi/osaurus-sync-ffi_$t.cdx.json" "$OUT/osaurus-sync-ffi_$t.cdx.json"
  find crates -name "*_$t.cdx.json" -delete
done
ls "$OUT"
