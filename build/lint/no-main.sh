#!/usr/bin/env bash
# SPEC 6.5 lint (GNU grep -P, same PCRE as the SPEC rg form): fails on branch-name uses of `main` (the default branch is `master`).
# Usage: no-main.sh | no-main.sh --self-test
# A line carrying the marker `lint:allow-main` is exempt.
set -u
cd "$(dirname "$0")/../.."

# The exact branch-name regex from SPEC 6.5 (not `rg -w main`).
PATTERN='(refs/(heads|remotes/[^/\s]+)/main\b|\b(origin|upstream)[/ ]main\b|\bpush\b.*\S:main\b|(\s-[bBcCmM]|--branch|--initial-branch|--base|--head|--ref|--onto|--rev)[ =]main\b|(?i:default_?branch)["\x27]?\s*[:=]\s*["\x27]?main\b|\b(ref|base|branch|branches)\s*:\s*\[?\s*["\x27]?main\b|^\s*-\s*["\x27]?main["\x27]?\s*(#.*)?$|\bmain\.\.|\.\.\.?main\b|\b(switch|checkout|merge|rebase|pull|fetch|reset|diff|log|branch)\s+(-\S+\s+)*main\b|\b(switch|checkout)\s+-[bBcC]\s+\S+\s+main\b|\bworktree\s+add\s+(-\S+\s+)*\S+\s+main\b)'

# scan <path>...: print offending lines (marker lines exempt); status 1 if any.
scan() {
  local hits
  hits=$(grep -rnHIP -e "$PATTERN" -- "$@" | grep -v -e '^build/lint/fixtures/' -e 'lint:allow-main')
  if [ -n "$hits" ]; then
    printf '%s\n' "$hits"
    return 1
  fi
  return 0
}

if [ "${1:-}" = "--self-test" ]; then
  fx=build/lint/fixtures/no-main
  fail=0
  for f in "$fx"/bad/*; do
    # The scan glob skips fixtures, so run the pattern directly on the file.
    if ! grep -P -e "$PATTERN" "$f" | grep -qv 'lint:allow-main'; then
      echo "self-test: expected failure, got pass: $f"; fail=1
    fi
  done
  for f in "$fx"/allowed/* "$fx"/good/*; do
    if grep -P -e "$PATTERN" "$f" | grep -qv 'lint:allow-main'; then
      echo "self-test: expected pass, got failure: $f"; fail=1
    fi
  done
  for f in docs/SPEC.md docs/PLAN.md README.md; do
    if [ -f "$f" ] && ! scan "$f" >/dev/null; then
      echo "self-test: $f must pass"; fail=1
    fi
  done
  [ "$fail" -eq 0 ] && echo "no-main self-test ok"
  exit "$fail"
fi

paths=()
for p in docs/*.md README.md crates tests deploy actions .forgejo bench packaging proto build; do
  [ -e "$p" ] && paths+=("$p")
done
scan "${paths[@]}" && exit 0
echo "no-main: use \`master\` (SPEC 6.5), or add lint:allow-main to a line that quotes a forbidden form" >&2
exit 1
