#!/usr/bin/env bash
# SPEC 11.4 / 9.4.1 lint. Usage: no-raw-git.sh | no-raw-git.sh --self-test
#  1. `Command::new("git")` is allowed only in crates/asc-git/src/realgit.rs and on lines
#     carrying `lint:allow-raw-git` (test-only fixtures in crates below asc-git).
#  2. Under crates/asc/src/ (except refs_guard.rs, the one file that writes refs), no `.update(` on
#     the local `Refs` (use `Ctx::update_local_refs`, SPEC 9.4.1).
set -u
cd "$(dirname "$0")/../.."

RAW_GIT='Command::new\(\s*"git(\.exe)?"'
REFS_UPDATE='\b[A-Za-z_]*refs[A-Za-z_]*(\(\))?\s*\.update\('

# hits <pattern> <path>...: matching lines minus the marker and fixtures.
hits() {
  local pat=$1; shift
  grep -rnHI --include='*.rs' -P -e "$pat" -- "$@" 2>/dev/null \
    | grep -v -e '^build/lint/fixtures/' -e 'lint:allow-raw-git'
}

# check_raw_git <path>...: nonzero if any offending line.
check_raw_git() { ! hits "$RAW_GIT" "$@" | grep -v -e '^crates/asc-git/src/realgit\.rs:' | grep -q .; }
# check_refs_update <path>...
check_refs_update() { ! hits "$REFS_UPDATE" "$@" | grep -v -e '^crates/asc/src/refs_guard.rs:' | grep -q .; }

if [ "${1:-}" = "--self-test" ]; then
  fx=build/lint/fixtures/no-raw-git
  fail=0
  expect() { # expect <fail|pass> <desc> <cmd...>
    local want=$1 desc=$2; shift 2
    if "$@" >/dev/null; then got=pass; else got=fail; fi
    [ "$got" = "$want" ] || { echo "self-test: $desc: expected $want, got $got"; fail=1; }
  }
  # The fixtures live under build/lint/fixtures, so test the patterns on them directly.
  has_hit() { grep -P -e "$1" "$2" | grep -qv 'lint:allow-raw-git'; }
  expect pass "raw git fixture is a hit" has_hit "$RAW_GIT" "$fx/bad/raw.rs"
  expect fail "marked raw git passes" has_hit "$RAW_GIT" "$fx/allowed/raw.rs"
  expect fail "non-git command passes" has_hit "$RAW_GIT" "$fx/good/other.rs"
  expect pass "refs update fixture is a hit" has_hit "$REFS_UPDATE" "$fx/bad/refs.rs"
  expect fail "update_local_refs passes" has_hit "$REFS_UPDATE" "$fx/good/refs.rs"
  # The real tree must pass.
  expect pass "tree: raw git" check_raw_git crates tests bench deploy actions packaging
  expect pass "tree: refs update" check_refs_update crates/asc/src
  [ "$fail" -eq 0 ] && echo "no-raw-git self-test ok"
  exit "$fail"
fi

status=0
paths=()
for p in crates tests bench deploy actions packaging; do [ -e "$p" ] && paths+=("$p"); done
if ! check_raw_git "${paths[@]}"; then
  hits "$RAW_GIT" "${paths[@]}" | grep -v -e '^crates/asc-git/src/realgit\.rs:'
  echo 'no-raw-git: use RealGit::command(); test fixtures below asc-git carry lint:allow-raw-git (SPEC 11.4)' >&2
  status=1
fi
if [ -d crates/asc/src ] && ! check_refs_update crates/asc/src; then
  hits "$REFS_UPDATE" crates/asc/src | grep -v -e '^crates/asc/src/refs_guard.rs:'
  echo 'no-raw-git: use Ctx::update_local_refs (SPEC 9.4.1)' >&2
  status=1
fi
exit "$status"
