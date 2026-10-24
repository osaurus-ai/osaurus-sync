#!/usr/bin/env bash
# Packs the Osaurus.Sync NuGet package from native libraries built per runtime identifier.
#
#   scripts/pack-nuget.sh [ARTIFACTS_DIR]
#
# ARTIFACTS_DIR (default dist/native) holds one directory per RID, as uploaded by CI:
#   dist/native/win-x64/osaurus_sync_ffi.dll
#   dist/native/win-arm64/osaurus_sync_ffi.dll
#   dist/native/osx-arm64/libosaurus_sync_ffi.dylib   (optional)
# The generated bindings must already be in bindings/csharp/OsaurusSync/Generated
# (scripts/gen-csharp.sh, or the CI artifact). Set ALLOW_PARTIAL=1 to pack without
# both Windows libraries, e.g. for a local smoke test.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARTIFACTS="${1:-$ROOT/dist/native}"
CS_DIR="$ROOT/bindings/csharp/OsaurusSync"
OUT="$ROOT/dist/nuget"
DOTNET="${DOTNET:-$(command -v dotnet || echo "$HOME/.dotnet/dotnet")}"

if [ ! -f "$CS_DIR/Generated/osaurus_sync_ffi.cs" ]; then
  echo "missing generated bindings; run scripts/gen-csharp.sh first" >&2
  exit 1
fi

found=()
for rid in win-x64 win-arm64 osx-arm64; do
  case "$rid" in
    win-*) lib=osaurus_sync_ffi.dll ;;
    osx-*) lib=libosaurus_sync_ffi.dylib ;;
  esac
  src="$ARTIFACTS/$rid/$lib"
  if [ -f "$src" ]; then
    mkdir -p "$CS_DIR/runtimes/$rid/native"
    cp "$src" "$CS_DIR/runtimes/$rid/native/$lib"
    found+=("$rid")
  elif [ -f "$CS_DIR/runtimes/$rid/native/$lib" ]; then
    found+=("$rid")
  elif [[ "$rid" == win-* && "${ALLOW_PARTIAL:-0}" != 1 ]]; then
    echo "missing $src" >&2
    exit 1
  fi
done

rm -rf "$OUT"
"$DOTNET" pack "$CS_DIR/OsaurusSync.csproj" -c Release -o "$OUT" ${VERSION:+-p:Version=$VERSION}
echo "packed $(ls "$OUT"/*.nupkg) with runtimes: ${found[*]}"
