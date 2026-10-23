#!/usr/bin/env bash
# Runs every fuzz target for N seconds (default 60). Non-zero on a crash or a build failure.
# Usage: bash fuzz/run-all.sh [seconds] [target...]
# Needs cargo-fuzz and a nightly toolchain (FUZZ_TOOLCHAIN, default nightly).
set -euo pipefail
cd "$(dirname "$0")"
secs="${1:-60}"
shift || true
toolchain="${FUZZ_TOOLCHAIN:-nightly}"
if [ "$#" -gt 0 ]; then
  targets=("$@")
else
  mapfile -t targets < <(sed -n 's/^name = "\(.*\)"$/\1/p' Cargo.toml | grep -v '^asc-fuzz$')
fi
fail=0
for t in "${targets[@]}"; do
  echo "== $t (${secs}s)"
  mkdir -p "corpus/$t" "regressions/$t"
  if ! cargo "+$toolchain" fuzz run "$t" "corpus/$t" "regressions/$t" -- \
      -max_total_time="$secs" -rss_limit_mb=2048 -max_len=65536; then
    echo "FAILED: $t (crash input is under fuzz/artifacts/$t; commit it to fuzz/regressions/$t/)"
    fail=1
  fi
done
exit "$fail"
