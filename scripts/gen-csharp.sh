#!/usr/bin/env bash
# Generates the C# bindings and copies the host native library into runtimes/<rid>/native.
#
#   cargo install uniffi-bindgen-cs --git https://github.com/NordSecurity/uniffi-bindgen-cs --tag v0.11.1+v0.31.0
#   scripts/gen-csharp.sh                # release, host target
#   PROFILE=dev TARGET=x86_64-pc-windows-msvc scripts/gen-csharp.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PROFILE="${PROFILE:-release}"
OUT_DIR=$([ "$PROFILE" = "dev" ] && echo debug || echo "$PROFILE")
TARGET="${TARGET:-$(rustc -vV | sed -n 's/host: //p')}"
CS_DIR="$ROOT/bindings/csharp/OsaurusSync"

case "$TARGET" in
  x86_64-pc-windows-msvc)  RID=win-x64;   LIB=osaurus_sync_ffi.dll ;;
  aarch64-pc-windows-msvc) RID=win-arm64; LIB=osaurus_sync_ffi.dll ;;
  aarch64-apple-darwin)    RID=osx-arm64; LIB=libosaurus_sync_ffi.dylib ;;
  x86_64-unknown-linux-gnu) RID=linux-x64; LIB=libosaurus_sync_ffi.so ;;
  *) echo "unsupported target $TARGET" >&2; exit 1 ;;
esac

cargo build -p osaurus-sync-ffi --lib --profile "$PROFILE" --target "$TARGET"
NATIVE="target/$TARGET/$OUT_DIR/$LIB"

mkdir -p "$CS_DIR/Generated"
uniffi-bindgen-cs --library "$NATIVE" --config crates/osaurus-sync-ffi/uniffi.toml --out-dir "$CS_DIR/Generated"

mkdir -p "$CS_DIR/runtimes/$RID/native"
cp "$NATIVE" "$CS_DIR/runtimes/$RID/native/"
echo "generated bindings; native library in runtimes/$RID/native"
