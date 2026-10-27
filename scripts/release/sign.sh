#!/bin/sh
# Signs every file of a release directory with minisign, one <file>.minisig per file.
#
#   MINISIGN_KEY_FILE=/path/to/minisign.key sign.sh <release dir> <version>
#
# The trusted comment of each signature is `file:<name> version:<version>`. `asc upgrade` and
# `asc self-update` refuse a signature whose trusted comment does not name the archive they asked
# for, and install.sh and verify.py check both fields, so a validly signed older file cannot be
# replayed as another file or version.
#
# The key is read by minisign only and must be an unencrypted key (CI writes it from a secret into
# a mode 0600 file). Nothing is printed about its content.
set -eu

die() { echo "sign.sh: $*" >&2; exit 1; }

[ $# -eq 2 ] || die "usage: MINISIGN_KEY_FILE=<key> sign.sh <release dir> <version>"
dir=$1
version=$2
key=${MINISIGN_KEY_FILE:-}

case "$version" in
  ''|*[!0-9.]*|.*|*.|*..*) die "bad version: $version" ;;
esac
[ -d "$dir" ] || die "not a directory: $dir"
[ -n "$key" ] || die "MINISIGN_KEY_FILE is not set; refusing to publish unsigned files"
[ -f "$key" ] || die "no such key file"
command -v minisign >/dev/null 2>&1 || die "minisign is not installed"
[ -f "$dir/SHA256SUMS" ] || die "$dir/SHA256SUMS is missing; assemble the release first"

count=0
# One file per line; names were checked by assemble.py and contain no whitespace.
list=$(mktemp)
trap 'rm -f "$list"' EXIT INT TERM
find "$dir" -type f ! -name '*.minisig' | LC_ALL=C sort > "$list"
while IFS= read -r file; do
  name=$(basename "$file")
  minisign -S -s "$key" -t "file:$name version:$version" -m "$file" -x "$file.minisig" </dev/null >/dev/null \
    || die "minisign failed for $name"
  count=$((count + 1))
done < "$list"
[ "$count" -gt 0 ] || die "nothing to sign in $dir"
echo "sign.sh: signed $count files"
