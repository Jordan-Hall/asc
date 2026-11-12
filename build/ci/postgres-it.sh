#!/usr/bin/env bash
# Runs the asc-serve tests that take ASC_TEST_POSTGRES_URL (the server migrations, the AI keys and features, the
# agent runs that use them) against a real PostgreSQL, no Docker.
# Usage: build/ci/postgres-it.sh   (Linux, with the PostgreSQL server binaries; PG_BIN overrides where they are)
# Starts a throwaway cluster on loopback (a temp directory, a random password), runs the tests and stops it on exit.
# Env: PG_BIN (default: the newest /usr/lib/postgresql/*/bin), ASC_PG_PORT (55432), ASC_PG_WORK (default ~/postgres-it),
#      CARGO_TARGET_DIR (default ~/asct/postgres), ASC_PG_FILTER (test filter, default every lib test).
set -euo pipefail
cd "$(dirname "$0")/../.."
REPO=$(pwd)

if [ "$(uname -s)" != "Linux" ]; then
  echo "postgres-it.sh must run on Linux (uname: $(uname -s))" >&2
  exit 2
fi
if [ -z "${PG_BIN:-}" ]; then
  PG_BIN=$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -n1 || true)
fi
[ -n "$PG_BIN" ] && [ -x "$PG_BIN/initdb" ] || { echo "no PostgreSQL server binaries; set PG_BIN" >&2; exit 2; }

PORT=${ASC_PG_PORT:-55432}
WORK=${ASC_PG_WORK:-$HOME/postgres-it}/run-$$
rm -rf "$WORK"
mkdir -p "$WORK"
DATA=$WORK/data
LOG=$WORK/postgres.log
PASS=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
printf '%s\n' "$PASS" >"$WORK/pass"

cleanup() {
  status=$?
  if [ -f "$DATA/postmaster.pid" ]; then "$PG_BIN/pg_ctl" -D "$DATA" -m immediate stop >/dev/null 2>&1 || true; fi
  if [ "$status" -ne 0 ] && [ -f "$LOG" ]; then
    # Every error the server logged (a failing test may have hit one long before the end), then the tail.
    echo "--- postgres errors ---" >&2
    grep -E -A2 ' (ERROR|FATAL|PANIC): ' "$LOG" | tail -n 200 >&2 || true
    echo "--- postgres log (tail) ---" >&2
    tail -n 15 "$LOG" >&2
  fi
  rm -rf "$WORK"
  exit "$status"
}
trap cleanup EXIT INT TERM

"$PG_BIN/postgres" --version
"$PG_BIN/initdb" -D "$DATA" -U postgres --auth=scram-sha-256 --pwfile="$WORK/pass" -E UTF8 --locale=C.UTF-8 >/dev/null
# Every test makes a database of its own and pools of its own, so the default 100 connections are too few.
"$PG_BIN/pg_ctl" -D "$DATA" -l "$LOG" -w -t 60 \
  -o "-c listen_addresses=127.0.0.1 -c port=$PORT -c unix_socket_directories=$WORK -c max_connections=1000 -c fsync=off" start >/dev/null
echo "postgres ready on 127.0.0.1:$PORT"

export ASC_TEST_POSTGRES_URL="postgres://postgres:$PASS@127.0.0.1:$PORT/postgres"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$HOME/asct/postgres}
cd "$REPO"
# shellcheck disable=SC2086 # the filter is a list of test name parts
cargo test -p asc-serve --lib -- ${ASC_PG_FILTER:-}
