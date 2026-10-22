#!/usr/bin/env bash
# Work tree access lint. Usage: no-raw-worktree-fs.sh | no-raw-worktree-fs.sh --self-test
# A tracked path under a symlinked or reparse-point ancestor is outside the work tree: it is never
# read, written or deleted (git's has_symlink_leading_path). Every path of a work tree file therefore
# comes from `asc_wc::SafeRoot::resolve` (or the SafeRoot helpers built on it). Under crates/asc/src
# (outside `#[cfg(test)] mod` sections) this fails on
#  1. any use of `fs_path`, `try_fs_path`, `fs_path_of` or `fs_path_raw`: they do not look at ancestors;
#  2. `root.join(` and `toplevel().join(`, which build a work tree path by hand (the metadata
#     names `.asc` and `.git` are exempt);
#  3. `read_dir(`, a walk that must not follow a link into a subtree.
# A line carrying the marker `lint:allow-worktree-fs` is exempt; say why on the line or above it.
set -u
cd "$(dirname "$0")/../.."

MARK='lint:allow-worktree-fs'
RAW_PATH='\b(try_)?fs_path\w*'
RAW_JOIN='\b(root|toplevel\(\))\s*\.join\((?!\s*"\.(asc|git)\b)'
RAW_WALK='\bread_dir\('
ANY="(?:$RAW_PATH)|(?:$RAW_JOIN)|(?:$RAW_WALK)"

# code_lines <file>...: "file:line:text" for each line outside test modules and comments.
code_lines() {
  local f
  for f in "$@"; do
    awk -v f="$f" '
      pend { pend = 0; if ($0 ~ /^[ \t]*(pub(\([a-z]+\))? )?mod /) exit }
      /^[ \t]*#\[cfg\(test\)\][ \t]*$/ { pend = 1; next }
      /^[ \t]*\/\// { next }
      { print f ":" NR ":" $0 }
    ' "$f"
  done
}

# hits <path>...: offending lines among the .rs files below the paths.
hits() {
  local files
  files=$(find "$@" -name '*.rs' -not -path 'build/lint/fixtures/*' 2>/dev/null | sort)
  [ -n "$files" ] || return 0
  # shellcheck disable=SC2086
  code_lines $files | grep -P -e "$ANY" | grep -v -e "$MARK"
}

if [ "${1:-}" = "--self-test" ]; then
  fx=build/lint/fixtures/no-raw-worktree-fs
  fail=0
  expect() { # expect <fail|pass> <desc> <cmd...>
    local want=$1 desc=$2; shift 2
    if "$@" >/dev/null; then got=pass; else got=fail; fi
    [ "$got" = "$want" ] || { echo "self-test: $desc: expected $want, got $got"; fail=1; }
  }
  # has_hit <file>: the file has an offending line (fixtures are skipped by hits, so scan directly).
  has_hit() { code_lines "$1" | grep -P -e "$ANY" | grep -qv -e "$MARK"; }
  for f in "$fx"/bad/*.rs; do expect pass "bad fixture is a hit: $f" has_hit "$f"; done
  for f in "$fx"/good/*.rs "$fx"/allowed/*.rs; do expect fail "fixture passes: $f" has_hit "$f"; done
  # The real tree must pass.
  expect fail "tree: no raw work tree access" hits crates/asc/src
  [ "$fail" -eq 0 ] && echo "no-raw-worktree-fs self-test ok"
  exit "$fail"
fi

if [ -d crates/asc/src ] && out=$(hits crates/asc/src) && [ -n "$out" ]; then
  printf '%s\n' "$out"
  echo 'no-raw-worktree-fs: take work tree paths from asc_wc::SafeRoot::resolve (a path under a symlinked ancestor is outside the work tree); justified exceptions carry lint:allow-worktree-fs' >&2
  exit 1
fi
exit 0
