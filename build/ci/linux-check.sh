#!/usr/bin/env bash
# Linux gate: the #[cfg(unix)] code is never compiled on Windows, so every change must pass here too.
# Needs git 2.55 on PATH (the tests compare against it); Ubuntu LTS ships 2.43, so use ppa:git-core/ppa.
# Usage: build/ci/linux-check.sh   (run from any directory on a Linux host; prefer an ext4 clone, not DrvFs)
set -euo pipefail
cd "$(dirname "$0")/../.."

if [ "$(uname -s)" != "Linux" ]; then
  echo "linux-check.sh must run on Linux (uname: $(uname -s))" >&2
  exit 2
fi

cargo check --workspace --all-targets
cargo clippy --workspace --all-targets --no-deps -- -D warnings
cargo test --workspace --no-fail-fast
