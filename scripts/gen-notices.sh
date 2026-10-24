#!/usr/bin/env bash
# Writes THIRD_PARTY_NOTICES.md for the dependencies compiled into the native library, using
# cargo-about with about.toml. The release workflow runs it and ships the file in the Swift
# and NuGet packages.
#
#   scripts/gen-notices.sh [OUT]     # default: THIRD_PARTY_NOTICES.md at the repo root
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/THIRD_PARTY_NOTICES.md}"
cd "$ROOT"
cargo about generate --locked --fail -c about.toml -m crates/osaurus-sync-ffi/Cargo.toml \
  scripts/about/notices.hbs -o "$OUT"
echo "wrote $OUT ($(grep -c '^- \[' "$OUT") component entries)"
