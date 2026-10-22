#!/usr/bin/env bash
# Regenerates the diff golden corpus: modified text files taken from a repository's history, and
# git's own output for each pair. Usage: extract.sh <git-repo> [count]
#
# Layout written next to this script:
#   pairs/NNN.old, pairs/NNN.new, pairs/NNN.src  the two versions and where they came from
#   expected/NNN.<algorithm>.patch               git diff --no-index --no-prefix
#   expected/NNN.<algorithm>.numstat             git diff --no-index --numstat
set -euo pipefail

repo=${1:?usage: extract.sh <git-repo> [count]}
count=${2:-200}
here=$(cd "$(dirname "$0")" && pwd)
max_bytes=6000
min_bytes=200
algorithms="myers minimal patience histogram"

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1
rm -rf "$here/pairs" "$here/expected"
mkdir -p "$here/pairs" "$here/expected"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

n=0
# One candidate file per commit, newest first, skipping every other commit for variety.
skip=0
while read -r commit; do
  skip=$((skip + 1))
  [ $((skip % 2)) -eq 0 ] && continue
  while read -r _ _ old new status path; do
    [ "$status" = "M" ] || continue
    case "$path" in
      *.ts|*.tsx|*.js|*.mjs|*.rs|*.go|*.py|*.md|*.json|*.css|*.yml|*.yaml|*.sh|*.toml|*.html|*.sql) ;;
      *) continue ;;
    esac
    osize=$(git -C "$repo" cat-file -s "$old")
    nsize=$(git -C "$repo" cat-file -s "$new")
    [ "$osize" -ge $min_bytes ] && [ "$osize" -le $max_bytes ] || continue
    [ "$nsize" -ge $min_bytes ] && [ "$nsize" -le $max_bytes ] || continue
    id=$(printf '%03d' "$n")
    git -C "$repo" cat-file blob "$old" >"$here/pairs/$id.old"
    git -C "$repo" cat-file blob "$new" >"$here/pairs/$id.new"
    if grep -qP '\x00' "$here/pairs/$id.old" "$here/pairs/$id.new"; then
      rm -f "$here/pairs/$id.old" "$here/pairs/$id.new"
      continue
    fi
    printf '%s:%s\n' "$commit" "$path" >"$here/pairs/$id.src"
    break
  done < <(git -C "$repo" diff-tree --no-commit-id -r --raw --no-abbrev "$commit" | tr '\t' ' ' | sed 's/^://')
  if [ -f "$here/pairs/$(printf '%03d' "$n").old" ]; then
    n=$((n + 1))
  fi
  [ "$n" -ge "$count" ] && break
done < <(git -C "$repo" log --no-merges --format=%H -n 20000)

for oldf in "$here"/pairs/*.old; do
  id=$(basename "$oldf" .old)
  rm -rf "$work/a" "$work/b"
  mkdir -p "$work/a" "$work/b"
  cp "$oldf" "$work/a/file"
  cp "$here/pairs/$id.new" "$work/b/file"
  for alg in $algorithms; do
    (cd "$work" && git diff --no-index --no-prefix --diff-algorithm="$alg" a/file b/file || true) \
      >"$here/expected/$id.$alg.patch"
    (cd "$work" && git diff --no-index --no-prefix --diff-algorithm="$alg" --numstat a/file b/file || true) \
      | sed "s#{a => b}/file#file#" >"$here/expected/$id.$alg.numstat"
  done
done
echo "wrote $n pairs"
