#!/usr/bin/env bash
# Linux gate: the #[cfg(unix)] code is never compiled on Windows, so every change must pass here too.
# Needs git 2.55 on PATH (the tests compare against it); Ubuntu LTS ships 2.43, so use ppa:git-core/ppa.
# Usage: build/ci/linux-check.sh   (run from any directory on a Linux host; prefer an ext4 clone, not DrvFs)
#
# Runs check, clippy and test in turn, never stopping at the first failure, and ends with one line per
# step. The exit status is non-zero when any step failed.
#
# Nothing here may wait for a person. A test that spawns git or ssh must not reach a terminal, so the
# tests run with stdin from /dev/null, in their own session (setsid: no controlling terminal, /dev/tty
# cannot be opened), with GIT_TERMINAL_PROMPT=0 and no askpass or display. A prompt that slips through
# then fails at once. A test that still hangs is killed:
#   ASC_TEST_TIMEOUT   seconds one test may run when cargo-nextest is installed (default 120; killed after
#                     three periods, so a hung test costs at most 3x this)
#   ASC_STEP_TIMEOUT   seconds the whole test step may run (default 3600), the only limit without nextest
set -uo pipefail
cd "$(dirname "$0")/../.."

if [ "$(uname -s)" != "Linux" ]; then
  echo "linux-check.sh must run on Linux (uname: $(uname -s))" >&2
  exit 2
fi

ASC_TEST_TIMEOUT="${ASC_TEST_TIMEOUT:-120}"
ASC_STEP_TIMEOUT="${ASC_STEP_TIMEOUT:-3600}"

names=()
codes=()
failed=0

# step <name> <command...>: runs the command, records its exit status and carries on.
step() {
  local name="$1"
  shift
  echo "==> $name: $*"
  "$@"
  local code=$?
  names+=("$name")
  codes+=("$code")
  if [ "$code" -ne 0 ]; then
    failed=1
  fi
  return 0
}

# noprompt <command...>: no terminal, no stdin, no askpass, no display; the step fails instead of waiting.
noprompt() {
  env -u DISPLAY -u WAYLAND_DISPLAY -u SSH_ASKPASS -u GIT_ASKPASS -u SSH_ASKPASS_REQUIRE \
    GIT_TERMINAL_PROMPT=0 \
    setsid -w timeout --kill-after=30 "$ASC_STEP_TIMEOUT" "$@" </dev/null
}

run_tests() {
  if command -v cargo-nextest >/dev/null 2>&1; then
    local conf
    conf="$(mktemp)"
    cat >"$conf" <<EOF
[profile.default]
slow-timeout = { period = "${ASC_TEST_TIMEOUT}s", terminate-after = 3 }
EOF
    noprompt cargo nextest run --workspace --no-fail-fast --config-file "$conf" &&
      noprompt cargo test --workspace --doc --no-fail-fast
    local code=$?
    rm -f "$conf"
    return "$code"
  fi
  echo "cargo-nextest not installed: no per-test timeout, only the ${ASC_STEP_TIMEOUT}s step limit" >&2
  noprompt cargo test --workspace --no-fail-fast
}

step check cargo check --workspace --all-targets
step clippy cargo clippy --workspace --all-targets --no-deps -- -D warnings
step test run_tests

echo
echo "linux-check summary"
for i in "${!names[@]}"; do
  if [ "${codes[$i]}" -eq 0 ]; then
    echo "  ${names[$i]}: ok"
  else
    echo "  ${names[$i]}: FAILED (exit ${codes[$i]})"
  fi
done
exit "$failed"
