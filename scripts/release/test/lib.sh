# Shared helpers for the release script tests. Sourced by bash; not meant to be run.
# shellcheck shell=bash

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
REL="$ROOT/scripts/release"
PY=${PYTHON:-python3}
passed=0
failed=0

ok() { passed=$((passed + 1)); echo "ok   $1"; }
bad() { failed=$((failed + 1)); echo "FAIL $1" >&2; [ -n "${2:-}" ] && printf '%s\n' "$2" | sed 's/^/     | /' >&2; return 0; }

# check <description> <command...>: passes when the command succeeds.
check() {
  local desc=$1; shift
  if out=$("$@" 2>&1); then ok "$desc"; else bad "$desc" "$out"; fi
}

# refuse <description> <command...>: passes when the command fails. Output stays in $out.
refuse() {
  local desc=$1; shift
  if out=$("$@" 2>&1); then bad "$desc (it succeeded)" "$out"; else ok "$desc"; fi
}

# contains <description> <text> <needle>
contains() {
  case "$2" in *"$3"*) ok "$1" ;; *) bad "$1 (missing '$3')" "$2" ;; esac
}

lacks() {
  case "$2" in *"$3"*) bad "$1 (found '$3')" "$2" ;; *) ok "$1" ;; esac
}

finish() {
  echo "$passed passed, $failed failed"
  [ "$failed" -eq 0 ]
}

# fake_bins <dir> <version>: a stand-in executable for every target.
fake_bins() {
  local d=$1 v=$2 t
  for t in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
    mkdir -p "$d/$t"
    printf '#!/bin/sh\necho "asc %s (commit fixture)"\n' "$v" > "$d/$t/asc"
    chmod 755 "$d/$t/asc"
  done
  mkdir -p "$d/x86_64-pc-windows-msvc"
  printf 'MZ fake executable %s\n' "$v" > "$d/x86_64-pc-windows-msvc/asc.exe"
}

COMMIT=0123456789abcdef0123456789abcdef01234567
EPOCH=1700000000

# make_release <out dir> <version> [bins dir]: assemble.py on the fake binaries and the real
# Cargo.lock, so the SBOM code runs on the real dependency graph.
make_release() {
  local out=$1 v=$2 bins=${3:-}
  if [ -z "$bins" ]; then bins="$out.bins"; fake_bins "$bins" "$v"; fi
  "$PY" "$REL/assemble.py" --version "$v" --commit "$COMMIT" --epoch "$EPOCH" \
    --bins "$bins" --cargo-lock "$ROOT/Cargo.lock" \
    --license "$ROOT/LICENSE-APACHE" --license "$ROOT/LICENSE-MIT" \
    --release-url "https://downloads.example.invalid/asc/$v" --homepage "https://example.invalid/asc" \
    --root "$ROOT" --out "$out"
}

# new_key <dir>: a throwaway unencrypted minisign key pair in <dir>/{key,key.pub}; prints the public key text.
new_key() {
  minisign -G -W -f -p "$1/key.pub" -s "$1/key" >/dev/null
  sed -n 2p "$1/key.pub"
}

# serve <dir>: starts the fixture server; sets SERVER_PID, BASE and LOG.
serve() {
  local dir=$1 portfile=$2.port
  LOG=$2.log
  rm -f "$portfile"
  "$PY" "$REL/test/fixture_server.py" "$dir" "$portfile" "$LOG" &
  SERVER_PID=$!
  local i=0
  while [ ! -s "$portfile" ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
  [ -s "$portfile" ] || { echo "the fixture server did not start" >&2; exit 1; }
  # shellcheck disable=SC2034 # read by the scripts that source this file
  BASE="http://127.0.0.1:$(cat "$portfile")"
}

stop_server() { [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null; SERVER_PID=; return 0; }
