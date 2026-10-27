#!/bin/sh
# asc installer for Linux (SPEC 16.2). Never interactive.
#   install.sh --tarball asc-<ver>-<target>.tar.gz [--sig <file>] [--prefix <dir>] [--shim] [--no-service]
# Layout: <prefix>/<ver>/{asc,shim/}, <prefix>/current -> <ver>, ~/.local/bin/asc -> current/asc.
# The tarball is verified with minisign against the release key before anything is unpacked.
set -eu

RELEASE_PUBKEY='@RELEASE_PUBKEY@'
prefix="${XDG_DATA_HOME:-$HOME/.local/share}/asc-install"
tarball=''; sig=''; shim=0; service=1

die() { echo "install.sh: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --tarball) [ $# -ge 2 ] || die "--tarball needs a file"; tarball=$2; shift 2 ;;
    --sig) [ $# -ge 2 ] || die "--sig needs a file"; sig=$2; shift 2 ;;
    --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; prefix=$2; shift 2 ;;
    --shim) shim=1; shift ;;
    --no-service) service=0; shift ;;
    *) die "unknown option: $1" ;;
  esac
done

[ -n "$tarball" ] || die "usage: install.sh --tarball <file> [--sig <file>] [--prefix <dir>] [--shim] [--no-service]"
[ -f "$tarball" ] || die "no such file: $tarball"
[ "${RELEASE_PUBKEY#@}" != "$RELEASE_PUBKEY" ] && die "this installer has no release key; use the one published with a release"
command -v minisign >/dev/null 2>&1 || die "minisign is required to verify the download"
sig=${sig:-$tarball.minisig}
[ -f "$sig" ] || die "no signature file: $sig"
minisign -V -P "$RELEASE_PUBKEY" -x "$sig" -m "$tarball" >/dev/null || die "signature check failed, nothing was installed"

# The name is asc-<ver>-<target>.tar.gz; the version names a directory, so it is validated.
base=$(basename "$tarball")
ver=${base#asc-}; ver=${ver%%-*}
case "$ver" in
  ''|*[!0-9.]*|.*|*.|*..*) die "bad version in $base" ;;
esac
[ "$(printf '%s' "$ver" | tr -cd . | wc -c)" -eq 2 ] || die "bad version in $base"
# The signed trusted comment names the file and version the signature was made for; a validly
# signed older archive renamed to look newer is refused.
comment=$(sed -n 's/^trusted comment: //p' "$sig" | head -n 1 | tr '	' ' ')
[ "$comment" = "file:$base version:$ver" ] || die "the signature is for another file or version ($comment), nothing was installed"

dest="$prefix/$ver"
[ -e "$dest" ] && die "$dest already exists"
mkdir -p "$prefix"
stage=$(mktemp -d "$prefix/.$ver.new.XXXXXX")
trap 'rm -rf "$stage"' EXIT INT TERM
# Only a flat `asc`; no path in the tarball is trusted.
tar -xzf "$tarball" -C "$stage" --no-same-owner --no-same-permissions asc
[ -f "$stage/asc" ] && [ ! -L "$stage/asc" ] || die "the tarball has no asc"
chmod 755 "$stage/asc"
mkdir -p "$stage/shim"
[ "$shim" -eq 1 ] && ln -s "$dest/asc" "$stage/shim/git"
mv "$stage" "$dest"
trap - EXIT INT TERM

# Switch current atomically, then expose asc on PATH.
ln -s "$ver" "$prefix/.current.$$"
mv -T "$prefix/.current.$$" "$prefix/current"
mkdir -p "$HOME/.local/bin"
ln -sfn "$prefix/current/asc" "$HOME/.local/bin/asc"

if [ "$service" -eq 1 ] && command -v systemctl >/dev/null 2>&1; then
  unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  mkdir -p "$unit_dir"
  src=$(dirname "$0")/asc-daemon.service
  if [ -f "$src" ]; then
    sed "s|@PREFIX@|$prefix|g" "$src" > "$unit_dir/asc-daemon.service"
    systemctl --user daemon-reload >/dev/null 2>&1 || true
    systemctl --user enable asc-daemon.service >/dev/null 2>&1 || true
  fi
fi

echo "asc $ver installed in $dest"
[ "$shim" -eq 1 ] && echo "To use asc for git commands: export PATH=\"$prefix/current/shim:\$PATH\""
exit 0
