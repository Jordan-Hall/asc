#!/bin/sh
# asc installer for Linux and macOS. POSIX sh, never interactive, changes nothing outside the
# install prefix and the bin directory, and never edits a shell profile.
#
#   ASC_DOWNLOAD_BASE_URL=https://downloads.example.org/asc sh install.sh
#   sh install.sh --base-url https://downloads.example.org/asc --version 1.2.3
#
# The base URL is the directory a release host serves (scripts/release/publish-to-hub.sh writes
# it): <base>/latest/version, <base>/<version>/SHA256SUMS and <base>/<version>/asc-<version>-<target>.tar.gz.
# There is no default host; the installer is neutral about where the files come from.
#
# Options (each also has an environment variable):
#   --base-url URL   ASC_DOWNLOAD_BASE_URL   where releases are served (https; see below)
#   --version X.Y.Z  ASC_VERSION             install this version instead of latest/version
#   --prefix DIR     ASC_INSTALL_PREFIX      versions live in DIR/<version>, DIR/current points at one
#                                            (default: ${XDG_DATA_HOME:-$HOME/.local/share}/asc)
#   --bin-dir DIR    ASC_BIN_DIR             where the `asc` link goes (default: $HOME/.local/bin)
#   --dry-run                                resolve and print the plan; download no archive, write nothing
#   -h, --help
#
# Environment:
#   ASC_RELEASE_PUBKEY        minisign public key text. When set, SHA256SUMS must carry a valid
#                             signature (SHA256SUMS.minisig) from that key, and minisign must be
#                             installed; otherwise the install stops. When unset the checksum only
#                             proves the download matches what the host published, and a notice says so.
#   ASC_ALLOW_LOOPBACK_HTTP=1 allow http:// for 127.0.0.1, localhost and [::1] (tests and local mirrors).
#
# Layout, which `asc self-update` continues: DIR/<version>/asc, DIR/current -> <version>,
# BIN/asc -> DIR/current/asc.
set -eu

die() { echo "install.sh: $*" >&2; exit 1; }
note() { echo "install.sh: $*" >&2; }

base=${ASC_DOWNLOAD_BASE_URL:-}
want=${ASC_VERSION:-}
prefix=${ASC_INSTALL_PREFIX:-}
bindir=${ASC_BIN_DIR:-}
pubkey=${ASC_RELEASE_PUBKEY:-}
dry=0

while [ $# -gt 0 ]; do
  case "$1" in
    --base-url) [ $# -ge 2 ] || die "--base-url needs a URL"; base=$2; shift 2 ;;
    --version) [ $# -ge 2 ] || die "--version needs X.Y.Z"; want=$2; shift 2 ;;
    --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; prefix=$2; shift 2 ;;
    --bin-dir) [ $# -ge 2 ] || die "--bin-dir needs a directory"; bindir=$2; shift 2 ;;
    --dry-run) dry=1; shift ;;
    -h|--help) sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
done

[ -n "${HOME:-}" ] || die "HOME is not set"
[ -n "$prefix" ] || prefix="${XDG_DATA_HOME:-$HOME/.local/share}/asc"
[ -n "$bindir" ] || bindir="$HOME/.local/bin"

# ---- the base URL ----------------------------------------------------------------------------

[ -n "$base" ] || die "no download location: set ASC_DOWNLOAD_BASE_URL or pass --base-url (see --help)"
base=${base%/}
case "$base" in
  https://*) proto=https ;;
  http://127.0.0.1|http://127.0.0.1[:/]*|http://localhost|http://localhost[:/]*|http://\[::1\]|http://\[::1\][:/]*)
    [ "${ASC_ALLOW_LOOPBACK_HTTP:-}" = 1 ] ||
      die "plain http is refused; use https (loopback http needs ASC_ALLOW_LOOPBACK_HTTP=1)"
    proto=http ;;
  http://*) die "plain http is refused; use an https base URL" ;;
  *) die "the base URL must start with https://" ;;
esac
# No credentials in the URL: they would end up in logs and process listings.
authority=${base#*://}; authority=${authority%%/*}
case "$authority" in *@*) die "the base URL must not contain credentials" ;; esac
case "$base" in *\?*|*#*) die "the base URL must not have a query or fragment" ;; esac
case "$base" in
  *[!]A-Za-z0-9._~:/%+,=[-]*) die "the base URL has characters that are not allowed" ;;
esac

# ---- platform --------------------------------------------------------------------------------

os=$(uname -s)
cpu=$(uname -m)
case "$cpu" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) die "no asc build for the processor '$cpu' (builds: x86_64, aarch64)" ;;
esac
case "$os" in
  Linux) target="$arch-unknown-linux-gnu" ;;
  Darwin)
    # A shell running under Rosetta reports x86_64 on an Apple silicon Mac; install the native build.
    if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
      arch=aarch64
    fi
    target="$arch-apple-darwin" ;;
  *) die "no asc build for '$os' here; on Windows use install.ps1" ;;
esac

# ---- downloads -------------------------------------------------------------------------------

if command -v curl >/dev/null 2>&1; then
  dl=curl
elif command -v wget >/dev/null 2>&1; then
  dl=wget
else
  die "curl or wget is required"
fi

# fetch <url> <file> <max bytes>: a failed or oversized download leaves no file behind.
fetch() {
  rm -f "$2"
  if [ "$dl" = curl ]; then
    curl --fail --silent --show-error --location --proto "=$proto" --proto-redir "=$proto" \
      --max-redirs 5 --connect-timeout 20 --max-time 600 --max-filesize "$3" \
      --output "$2" "$1" || { rm -f "$2"; return 1; }
  else
    # wget has no size cap: the size is checked after the download.
    wget --quiet --max-redirect=5 --timeout=60 --output-document="$2" "$1" || { rm -f "$2"; return 1; }
  fi
  size=$(wc -c < "$2" | tr -d ' ')
  if [ "$size" -gt "$3" ]; then rm -f "$2"; return 1; fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 "$1" | sed 's/^.*= //'
  else die "sha256sum, shasum or openssl is required to verify the download"
  fi
}

valid_version() {
  case "$1" in
    ''|*[!0-9.]*|.*|*.|*..*) return 1 ;;
  esac
  [ "$(printf '%s' "$1" | tr -cd . | wc -c | tr -d ' ')" -eq 2 ] || return 1
  # No leading zeros: the same rule `asc self-update` applies.
  case ".$1" in *.0[0-9]*) return 1 ;; esac
  [ "${#1}" -le 32 ]
}

work=$(mktemp -d "${TMPDIR:-/tmp}/asc-install.XXXXXX") || die "cannot make a temporary directory"
trap 'rm -rf "$work"' EXIT INT TERM

if [ -z "$want" ]; then
  fetch "$base/latest/version" "$work/latest" 64 || die "cannot read $base/latest/version"
  want=$(head -n 1 "$work/latest" | tr -d '\r ')
fi
valid_version "$want" || die "not a version: '$want' (expected MAJOR.MINOR.PATCH)"
ver=$want

archive="asc-$ver-$target.tar.gz"
rel="$base/$ver"
fetch "$rel/SHA256SUMS" "$work/SHA256SUMS" 1048576 || die "cannot read $rel/SHA256SUMS (is $ver published for $target?)"

# The signature over SHA256SUMS, when a key is configured. The sums then carry the signature's
# authority to the archive.
if [ -n "$pubkey" ]; then
  command -v minisign >/dev/null 2>&1 || die "ASC_RELEASE_PUBKEY is set but minisign is not installed; refusing to continue"
  fetch "$rel/SHA256SUMS.minisig" "$work/SHA256SUMS.minisig" 4096 || die "cannot read $rel/SHA256SUMS.minisig"
  minisign -V -q -P "$pubkey" -x "$work/SHA256SUMS.minisig" -m "$work/SHA256SUMS" >/dev/null 2>&1 \
    || die "the signature of SHA256SUMS is not valid for the configured key; nothing was installed"
  comment=$(sed -n 's/^trusted comment: //p' "$work/SHA256SUMS.minisig" | head -n 1 | tr '\t' ' ')
  case " $comment " in
    *" file:SHA256SUMS "*) ;;
    *) die "the signature is for another file; nothing was installed" ;;
  esac
  case " $comment " in
    *" version:$ver "*) ;;
    *) die "the signature is for another version; nothing was installed" ;;
  esac
  trust="signed by the configured key"
else
  trust="checksum only (set ASC_RELEASE_PUBKEY to verify the publisher's signature)"
fi

# The expected hash: the one line for this archive, exactly "<64 hex>  <name>".
expected=$(awk -v f="$archive" 'NF == 2 && $2 == f && $1 ~ /^[0-9a-f]+$/ && length($1) == 64 { print $1 }' "$work/SHA256SUMS")
[ "$(printf '%s\n' "$expected" | grep -c .)" -eq 1 ] || die "SHA256SUMS has no single entry for $archive"

bin_link="$bindir/asc"
current="$prefix/current"
dest="$prefix/$ver"

echo "asc $ver for $target"
echo "  from     $rel/$archive"
echo "  sha256   $expected"
echo "  trust    $trust"
echo "  into     $dest/asc"
echo "  link     $bin_link -> $current/asc"

if [ "$dry" -eq 1 ]; then
  echo "dry run: nothing was downloaded or written"
  exit 0
fi

[ -n "$pubkey" ] || note "notice: the checksum comes from the same host as the archive; it does not prove who published it"

fetch "$rel/$archive" "$work/$archive" 268435456 || die "cannot download $rel/$archive"
got=$(sha256 "$work/$archive")
[ "$got" = "$expected" ] || die "checksum mismatch for $archive (expected $expected, got $got); nothing was installed"

# Only a flat `asc`; no path in the archive is trusted.
stage="$work/stage"
mkdir "$stage"
tar -xzf "$work/$archive" -C "$stage" asc 2>/dev/null || die "the archive has no asc"
[ -f "$stage/asc" ] && [ ! -L "$stage/asc" ] || die "the archive's asc is not a regular file"
chmod 755 "$stage/asc"
new_hash=$(sha256 "$stage/asc")

# Never replace a file that is not ours.
if [ -e "$bin_link" ] || [ -L "$bin_link" ]; then
  if [ -L "$bin_link" ]; then
    case "$(readlink "$bin_link")" in
      "$current/asc"|"$prefix"/*) ;;
      *) die "$bin_link is a link to somewhere else; remove it first" ;;
    esac
  else
    die "$bin_link exists and was not made by this installer; remove it first"
  fi
fi

mkdir -p "$prefix" "$bindir"
if [ -e "$dest" ]; then
  [ -f "$dest/asc" ] && [ "$(sha256 "$dest/asc")" = "$new_hash" ] \
    || die "$dest exists with different content; remove it first"
  echo "asc $ver was already installed"
else
  tmp="$prefix/.$ver.new.$$"
  rm -rf "$tmp"
  mkdir "$tmp"
  # The directory `asc self-update` expects beside the executable.
  mkdir "$tmp/shim"
  cp "$stage/asc" "$tmp/asc"
  mv "$tmp" "$dest" || { rm -rf "$tmp"; die "cannot create $dest"; }
fi

[ ! -e "$current" ] || [ -L "$current" ] || die "$current exists and is not a link; remove it first"

# Switch `current`, then expose asc. ln -sfn replaces an existing link rather than entering it.
ln -sfn "$ver" "$current"
ln -sfn "$current/asc" "$bin_link"

echo "installed asc $ver in $dest"
"$bin_link" --version 2>/dev/null || note "warning: the installed asc did not run here"

case ":${PATH:-}:" in
  *":$bindir:"*) ;;
  *)
    echo
    echo "$bindir is not on your PATH. Add it for this shell and your profile:"
    echo "  export PATH=\"$bindir:\$PATH\"        # bash, zsh, sh"
    echo "  fish_add_path \"$bindir\"             # fish"
    ;;
esac
exit 0
