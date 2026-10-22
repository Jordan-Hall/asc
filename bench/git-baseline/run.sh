#!/usr/bin/env bash
# One-command entry point: bash bench/git-baseline/run.sh [--sizes small,medium,asc] [--runs N] [--out DIR]
# See README.md. Needs git and node on PATH. Never interactive; never pushes; never touches the ASC source.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
exec node "$here/bench.mjs" "$@"
