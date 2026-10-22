#!/usr/bin/env bash
# Regenerates scenario/expected/* with git from scenario/c1 and scenario/c2.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)/scenario
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
git init -q -b master .
git config user.name t; git config user.email t@x
snap() { # <dir> <tag>
  git rm -rq --cached . 2>/dev/null || true
  find . -path ./.git -prune -o -type f -print0 | xargs -0 -r rm -f
  (cd "$here/$1" && find . -type f -print0) | while IFS= read -r -d '' f; do
    mkdir -p "$(dirname "$f")"; cp "$here/$1/$f" "$f"
  done
  git add -A
  grep "^$1 " "$here/modes.txt" | while read -r _ path mode; do
    git update-index --chmod=+x -- "$path"
  done || true
  git commit -qm "$1" --allow-empty
  git tag "$1"
}
snap c1; snap c2
mkdir -p "$here/expected"
d() { git -c core.abbrev=7 diff -M "$@" c1 c2; }
d >"$here/expected/patch"
d --stat >"$here/expected/stat"
d --stat=60 >"$here/expected/stat60"
d --numstat >"$here/expected/numstat"
d --shortstat >"$here/expected/shortstat"
d --raw >"$here/expected/raw"
d --name-status >"$here/expected/name-status"
d --name-only >"$here/expected/name-only"
d --summary >"$here/expected/summary"
d -z --name-status >"$here/expected/name-status-z"
d -z --numstat >"$here/expected/numstat-z"
d -U1 >"$here/expected/patch-u1"
d --word-diff=plain >"$here/expected/word-diff"
d --no-renames >"$here/expected/patch-no-renames"
d --histogram >"$here/expected/patch-histogram"
d --stat=40 >"$here/expected/stat40"
d --stat=80,20 >"$here/expected/stat-name20"
d --stat=80,40,5 >"$here/expected/stat-graph5"
d --no-prefix >"$here/expected/patch-noprefix"
d --relative=docs >"$here/expected/patch-relative"
d --relative=docs --name-only >"$here/expected/name-only-relative"
d --abbrev=10 --raw >"$here/expected/raw-abbrev10"
d --patience >"$here/expected/patch-patience"
d -U0 >"$here/expected/patch-u0"
