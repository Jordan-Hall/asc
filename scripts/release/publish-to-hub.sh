#!/bin/sh
# Copies a verified release directory into the download directory a hub serves.
#
#   publish-to-hub.sh --from <release dir> --to <hub download dir>
#                     [--pubkey <minisign public key> | --skip-signature-check]
#                     [--no-latest] [--force] [--dry-run]
#
# Layout written under --to (the directory ASC_DOWNLOAD_BASE_URL points at):
#   <ver>/                 every file of the release: archives, .minisig files, SHA256SUMS,
#                          manifest.json, the SBOM, the VS Code and JetBrains packages, packaging
#   latest/version         the newest version, one line (read by installers and `asc self-update`)
#   latest/manifest.json   a copy of that version's manifest.json (read by the hub download page)
#
# What it does, in order, and stops at the first failure:
#   1. verify.py checks the release directory: SHA256SUMS, manifest.json, archive contents and, with
#      the public key, every signature. A key is required unless --skip-signature-check says not.
#   2. <ver>/ is copied to a staging directory beside its destination, verified again there, and
#      renamed into place. A version that already exists is never overwritten without --force.
#   3. latest/ moves last, so a reader never sees a version whose files are missing. It does not move
#      to an older version without --force. --no-latest leaves it alone (a back-port or a rehearsal).
#
# The key comes from --pubkey or ASC_RELEASE_PUBKEY (the minisign public key text, the second line
# of the .pub file). --dry-run verifies and prints the plan, and changes nothing.
set -eu

die() { echo "publish-to-hub.sh: $*" >&2; exit 1; }

here=$(cd "$(dirname "$0")" && pwd)
from=''; to=''; pubkey=${ASC_RELEASE_PUBKEY:-}; skip_sig=0; latest=1; force=0; dry=0

while [ $# -gt 0 ]; do
  case "$1" in
    --from) [ $# -ge 2 ] || die "--from needs a directory"; from=$2; shift 2 ;;
    --to) [ $# -ge 2 ] || die "--to needs a directory"; to=$2; shift 2 ;;
    --pubkey) [ $# -ge 2 ] || die "--pubkey needs the key text"; pubkey=$2; shift 2 ;;
    --skip-signature-check) skip_sig=1; shift ;;
    --no-latest) latest=0; shift ;;
    --force) force=1; shift ;;
    --dry-run) dry=1; shift ;;
    -h|--help) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

[ -n "$from" ] && [ -n "$to" ] || die "usage: publish-to-hub.sh --from <release dir> --to <hub download dir>"
[ -d "$from" ] || die "no such release directory: $from"
[ -d "$to" ] || die "no such download directory: $to (create it first, so a typo does not make a new tree)"
command -v python3 >/dev/null 2>&1 || die "python3 is required"

if [ "$skip_sig" -eq 1 ]; then
  echo "publish-to-hub.sh: WARNING: signatures are not checked" >&2
  python3 "$here/verify.py" "$from" || die "the release did not verify"
else
  [ -n "$pubkey" ] || die "no public key: pass --pubkey or set ASC_RELEASE_PUBKEY (or --skip-signature-check)"
  python3 "$here/verify.py" "$from" --pubkey "$pubkey" || die "the release did not verify"
fi

version=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["version"])' "$from/manifest.json") \
  || die "cannot read the version from manifest.json"
case "$version" in
  ''|*[!0-9.]*|.*|*.|*..*) die "bad version in manifest.json: $version" ;;
esac

dest="$to/$version"
if [ -e "$dest" ] && [ "$force" -ne 1 ]; then
  die "$dest already exists; releases are immutable (use --force to replace it)"
fi

# newer <a> <b>: success when version a is strictly newer than b (both MAJOR.MINOR.PATCH).
newer() {
  python3 -I -c 'import sys
a, b = ([int(x) for x in v.split(".")] for v in sys.argv[1:3])
sys.exit(0 if a > b else 1)' "$1" "$2"
}

move_latest=$latest
if [ "$latest" -eq 1 ] && [ -f "$to/latest/version" ]; then
  current=$(head -n 1 "$to/latest/version" | tr -d '\r')
  case "$current" in
    ''|*[!0-9.]*|.*|*.|*..*) die "$to/latest/version is not a version: $current" ;;
  esac
  if [ "$current" != "$version" ] && ! newer "$version" "$current" && [ "$force" -ne 1 ]; then
    die "latest is $current, which is newer than $version; use --no-latest for a back-port, or --force"
  fi
fi

if [ "$dry" -eq 1 ]; then
  echo "publish-to-hub.sh: dry run: would copy $from to $dest"
  [ "$move_latest" -eq 1 ] && echo "publish-to-hub.sh: dry run: would set $to/latest/version to $version"
  exit 0
fi

stage="$to/.$version.new.$$"
old="$to/.$version.old.$$"
trap 'rm -rf "$stage" "$old"' EXIT INT TERM
rm -rf "$stage"
cp -R "$from" "$stage"
find "$stage" -type d -exec chmod 755 {} +
find "$stage" -type f -exec chmod 644 {} +
python3 "$here/verify.py" "$stage" >/dev/null || die "the staged copy did not verify"

if [ -e "$dest" ]; then mv "$dest" "$old"; fi
mv "$stage" "$dest"
rm -rf "$old"
echo "publish-to-hub.sh: $version is in $dest"

if [ "$move_latest" -eq 1 ]; then
  mkdir -p "$to/latest"
  printf '%s\n' "$version" > "$to/latest/.version.$$"
  cp "$dest/manifest.json" "$to/latest/.manifest.json.$$"
  chmod 644 "$to/latest/.version.$$" "$to/latest/.manifest.json.$$"
  mv -f "$to/latest/.manifest.json.$$" "$to/latest/manifest.json"
  mv -f "$to/latest/.version.$$" "$to/latest/version"
  echo "publish-to-hub.sh: latest is $version"
fi
