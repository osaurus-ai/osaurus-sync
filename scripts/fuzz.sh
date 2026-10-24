#!/usr/bin/env bash
# Runs every fuzz target for a while. Needs a nightly toolchain and cargo-fuzz.
#
#   scripts/fuzz.sh [SECONDS_PER_TARGET] [TARGET...]    # default 60 seconds, all targets
#
# The envelope corpus is seeded with the compatibility fixtures. Crashes are written to
# fuzz/artifacts/<target>/; reproduce one with `cargo +nightly fuzz run <target> <file>`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SECS="${1:-60}"
shift || true
cd "$ROOT/fuzz"
TARGETS=("$@")
if [ ${#TARGETS[@]} -eq 0 ]; then
  while IFS= read -r t; do TARGETS+=("$t"); done < <(cargo +nightly fuzz list)
fi

mkdir -p corpus/envelope
cp "$ROOT"/tests/fixtures/*.bin corpus/envelope/

cargo +nightly fuzz build -O
failed=()
for t in "${TARGETS[@]}"; do
  echo "==> $t (${SECS}s)"
  if ! cargo +nightly fuzz run -O "$t" -- -max_total_time="$SECS" -max_len=65536 -rss_limit_mb=2048; then
    failed+=("$t")
  fi
done
if [ ${#failed[@]} -gt 0 ]; then
  echo "failed: ${failed[*]}" >&2
  exit 1
fi
