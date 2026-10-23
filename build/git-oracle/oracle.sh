#!/usr/bin/env bash
# The pinned git oracle (docs/git-fidelity.md, "The pinned version"; PLAN Q02).
#
# Usage: oracle.sh fetch | verify | path | env | selftest
#   fetch     Linux: download git-<GIT_VERSION>.tar.xz over https, check its sha256 BEFORE unpacking,
#             build it. Windows: download the MinGit zip over https, check its sha256, unpack it.
#             Both end by writing <root>/.oracle-hash. A repeat run is a no-op while `verify` passes.
#   verify    Re-check the record, the pin and the binary's sha256, and that `git --version` prints
#             the pinned version. Exit 0 and one "oracle ok" line, or exit 1 and say why.
#   path      Print the pinned git binary (a native path on Windows).
#   env       Print `ASC_GIT_ORACLE=<path>` and `ASC_REAL_GIT=<path>`, one per line, so a shell can
#             `export` them or CI can append them to its environment file.
#   selftest  Check that a tampered download (one byte changed, a local fixture) is refused and
#             deleted, and an intact one is accepted. Needs no network.
#
# Pins are in pins.env next to this script. The cache is ${ASC_ORACLE_CACHE:-$HOME/.cache/asc-oracle}.
# Linux needs a C compiler, make, tar, xz, curl, and the libcurl and zlib headers (git-remote-http
# is built so that the HTTP auth tests have a client). Windows needs curl and unzip (Git Bash has
# both). Only https is used for downloads, and nothing downloaded is used before its sha256 matches.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=pins.env
. "$here/pins.env"

die() {
  echo "oracle.sh: $*" >&2
  exit 1
}

case "$(uname -s)" in
  Linux) os=linux ;;
  MINGW* | MSYS* | CYGWIN*) os=windows ;;
  *)
    echo "oracle.sh: unsupported platform $(uname -s) (Linux and Windows only)" >&2
    exit 2
    ;;
esac

cache="${ASC_ORACLE_CACHE:-$HOME/.cache/asc-oracle}"
if [ "$os" = linux ]; then
  root="$cache/git-$GIT_VERSION"
  bin="$root/bin/git"
  want_version="$GIT_VERSION"
  want_source_sha="$GIT_TARBALL_SHA256"
else
  root="$cache/mingit-$GFW_VERSION"
  bin="$root/cmd/git.exe"
  want_version="$GFW_VERSION"
  want_source_sha="$GFW_SHA256"
fi
record="$root/.oracle-hash"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -- "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -- "$1" | cut -d' ' -f1
  else
    die "no sha256sum or shasum"
  fi
}

# verify_download <file> <sha256>: succeeds when the digest matches. On a mismatch the file is
# deleted (a bad download must never be reused) and the status is 1.
verify_download() {
  local file="$1" want="$2" got
  got="$(sha256_of "$file")"
  if [ "$got" != "$want" ]; then
    rm -f -- "$file"
    echo "oracle.sh: sha256 mismatch for $(basename "$file"): expected $want, got $got; file deleted" >&2
    return 1
  fi
}

# download <url> <dest> <sha256>: https only (redirects too), verified, atomic. A cached file that
# still matches is kept; one that does not is deleted and fetched again.
download() {
  local url="$1" dest="$2" want="$3"
  case "$url" in https://*) ;; *) die "refusing a download that is not https: $url" ;; esac
  if [ -f "$dest" ]; then
    if verify_download "$dest" "$want" 2>/dev/null; then
      return 0
    fi
  fi
  command -v curl >/dev/null 2>&1 || die "curl is required"
  mkdir -p "$(dirname "$dest")"
  rm -f -- "$dest.part"
  echo "oracle.sh: downloading $url" >&2
  curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --silent --show-error --location \
    --retry 3 --output "$dest.part" "$url" || {
    rm -f -- "$dest.part"
    die "download failed: $url"
  }
  verify_download "$dest.part" "$want" || exit 1
  mv -f -- "$dest.part" "$dest"
}

# record_get <key>: the value of key=value in the record, or nothing.
record_get() {
  sed -n "s/^$1=//p" "$record" 2>/dev/null | head -n 1 | tr -d '\r'
}

write_record() {
  {
    printf 'version=%s\n' "$want_version"
    printf 'source_sha256=%s\n' "$want_source_sha"
    printf 'git_sha256=%s\n' "$(sha256_of "$bin")"
  } >"$record"
}

native() {
  if [ "$os" = windows ] && command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s\n' "$1"
  fi
}

# check: quiet verification; prints the problem and returns 1.
check() {
  [ -f "$record" ] || {
    echo "oracle.sh: no pinned git at $root (run: bash build/git-oracle/oracle.sh fetch)" >&2
    return 1
  }
  [ -f "$bin" ] || {
    echo "oracle.sh: $bin is missing" >&2
    return 1
  }
  [ "$(record_get version)" = "$want_version" ] || {
    echo "oracle.sh: record version '$(record_get version)' is not the pin $want_version" >&2
    return 1
  }
  [ "$(record_get source_sha256)" = "$want_source_sha" ] || {
    echo "oracle.sh: record source sha256 is not the pin in pins.env" >&2
    return 1
  }
  local got
  got="$(sha256_of "$bin")"
  [ "$(record_get git_sha256)" = "$got" ] || {
    echo "oracle.sh: $bin sha256 $got differs from the record $(record_get git_sha256)" >&2
    return 1
  }
  local out
  out="$("$bin" --version 2>&1 | tr -d '\r')" || true
  [ "$out" = "git version $want_version" ] || {
    echo "oracle.sh: '$bin --version' printed '$out', expected 'git version $want_version'" >&2
    return 1
  }
}

fetch_linux() {
  local tool
  for tool in cc make tar xz curl; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required to build git"
  done
  printf '#include <curl/curl.h>\n#include <zlib.h>\n' | cc -E -x c - >/dev/null 2>&1 ||
    die "libcurl and zlib headers are required (Debian and Ubuntu: apt-get install libcurl4-openssl-dev zlib1g-dev)"
  download "$GIT_TARBALL_URL" "$cache/$GIT_TARBALL" "$GIT_TARBALL_SHA256"
  local src="$cache/src-$GIT_VERSION" jobs="${ASC_ORACLE_JOBS:-}"
  if [ -z "$jobs" ]; then
    jobs="$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)"
  fi
  rm -rf -- "$src" "$root"
  mkdir -p "$src"
  tar -xJf "$cache/$GIT_TARBALL" -C "$src" --strip-components=1
  # NO_RUST=1: git 2.55 builds some hashing helpers in Rust unless told not to; the oracle must
  # build with a C toolchain alone, and the Rust parts give the same results.
  (
    cd "$src"
    make prefix="$root" NO_TCLTK=1 NO_GETTEXT=1 NO_PERL=1 NO_PYTHON=1 NO_EXPAT=1 NO_RUST=1 -j"$jobs" install
  ) || {
    rm -rf -- "$src" "$root"
    die "building git $GIT_VERSION failed"
  }
  rm -rf -- "$src"
  [ -x "$bin" ] || die "build finished but $bin is missing"
  write_record
}

fetch_windows() {
  download "$GFW_URL" "$cache/$GFW_ASSET" "$GFW_SHA256"
  rm -rf -- "$root"
  mkdir -p "$root"
  if command -v unzip >/dev/null 2>&1; then
    unzip -q -o "$cache/$GFW_ASSET" -d "$root"
  else
    tar -xf "$cache/$GFW_ASSET" -C "$root"
  fi
  [ -f "$bin" ] || {
    rm -rf -- "$root"
    die "the MinGit archive has no cmd/git.exe"
  }
  write_record
}

cmd_fetch() {
  if check 2>/dev/null; then
    echo "oracle.sh: pinned git $want_version already present at $root" >&2
    return 0
  fi
  mkdir -p "$cache"
  if [ "$os" = linux ]; then fetch_linux; else fetch_windows; fi
  check
  echo "oracle.sh: pinned git $want_version ready at $root" >&2
}

cmd_verify() {
  check || exit 1
  echo "oracle ok: $(native "$bin") (git version $want_version, sha256 $(record_get git_sha256))"
}

cmd_path() {
  check || exit 1
  native "$bin"
}

cmd_env() {
  check || exit 1
  local p
  p="$(native "$bin")"
  printf 'ASC_GIT_ORACLE=%s\n' "$p"
  printf 'ASC_REAL_GIT=%s\n' "$p"
}

cmd_selftest() {
  local t good
  t="$(mktemp -d)"
  trap "rm -rf -- '$t'" EXIT
  head -c 4096 /dev/zero >"$t/fixture"
  good="$(sha256_of "$t/fixture")"
  # An intact download is accepted and kept.
  verify_download "$t/fixture" "$good" || die "selftest: an intact fixture was refused"
  [ -f "$t/fixture" ] || die "selftest: an intact fixture was deleted"
  # One byte changed: refused, and the file is gone.
  {
    head -c 100 "$t/fixture"
    printf '\001'
    tail -c +102 "$t/fixture"
  } >"$t/tampered"
  [ "$(wc -c <"$t/tampered" | tr -d ' ')" = 4096 ] || die "selftest: the tampered fixture has the wrong size"
  if verify_download "$t/tampered" "$good" 2>"$t/err"; then
    die "selftest: a tampered fixture was accepted"
  fi
  [ ! -e "$t/tampered" ] || die "selftest: a tampered fixture was not deleted"
  grep -q 'sha256 mismatch' "$t/err" || die "selftest: no mismatch message"
  # A download that is not https is refused before any request.
  if (download "http://example.invalid/x" "$t/x" "$good") 2>/dev/null; then
    die "selftest: an http url was accepted"
  fi
  echo "oracle selftest ok"
}

case "${1:-}" in
  fetch) cmd_fetch ;;
  verify) cmd_verify ;;
  path) cmd_path ;;
  env) cmd_env ;;
  selftest) cmd_selftest ;;
  *)
    echo "usage: oracle.sh fetch | verify | path | env | selftest" >&2
    exit 2
    ;;
esac
