#!/bin/sh
# Builds the .deb and .rpm from a release directory with nfpm.
#   sh linux-packages.sh --release DIR --out DIR
# It verifies each Linux archive against DIR/SHA256SUMS first, as install.sh does. When
# ASC_RELEASE_PUBKEY is set it also requires a valid minisign signature on SHA256SUMS
# (SHA256SUMS.minisig) and stops without minisign; otherwise it says that only the checksum was checked.
# Needs nfpm on PATH. The rendered nfpm-*.yaml files in DIR say which archive each package takes.
set -eu

die() { echo "linux-packages.sh: $*" >&2; exit 1; }

rel=''; out=''
while [ $# -gt 0 ]; do
  case "$1" in
    --release) [ $# -ge 2 ] || die "--release needs a directory"; rel=$2; shift 2 ;;
    --out) [ $# -ge 2 ] || die "--out needs a directory"; out=$2; shift 2 ;;
    *) die "unknown option: $1" ;;
  esac
done
[ -n "$rel" ] && [ -n "$out" ] || die "usage: linux-packages.sh --release DIR --out DIR"
[ -f "$rel/SHA256SUMS" ] || die "no SHA256SUMS in $rel"
command -v nfpm >/dev/null 2>&1 || die "nfpm is required"
command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required"

if [ -n "${ASC_RELEASE_PUBKEY:-}" ]; then
  command -v minisign >/dev/null 2>&1 || die "minisign is required when ASC_RELEASE_PUBKEY is set"
  [ -f "$rel/SHA256SUMS.minisig" ] || die "no SHA256SUMS.minisig in $rel"
  minisign -V -P "$ASC_RELEASE_PUBKEY" -m "$rel/SHA256SUMS" -x "$rel/SHA256SUMS.minisig" >/dev/null ||
    die "the signature on SHA256SUMS is not valid"
else
  echo "linux-packages.sh: ASC_RELEASE_PUBKEY is not set; the checksum only proves the archive matches SHA256SUMS" >&2
fi

mkdir -p "$out"
rel=$(cd "$rel" && pwd)
out=$(cd "$out" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/asc-pkgs.XXXXXX") || die "cannot make a temporary directory"
trap 'rm -rf "$work"' EXIT INT TERM

for cfg in "$rel"/nfpm-*.yaml; do
  [ -f "$cfg" ] || die "no nfpm-*.yaml in $rel"
  tarball=$(sed -n 's|^#   archive: .*/\(asc-[^/]*\.tar\.gz\)$|\1|p' "$cfg")
  [ -n "$tarball" ] || die "$cfg names no archive"
  [ -f "$rel/$tarball" ] || die "missing $rel/$tarball"
  want=$(awk -v f="$tarball" '$2 == f { print $1 }' "$rel/SHA256SUMS")
  got=$(sha256sum "$rel/$tarball" | cut -d' ' -f1)
  [ -n "$want" ] && [ "$want" = "$got" ] || die "$tarball does not match SHA256SUMS"
  d="$work/$(basename "$cfg" .yaml)"
  mkdir -p "$d"
  tar -xzf "$rel/$tarball" -C "$d"
  (cd "$d" && nfpm package --config "$cfg" --packager deb --target "$out/" &&
    nfpm package --config "$cfg" --packager rpm --target "$out/") || die "nfpm failed for $cfg"
done
