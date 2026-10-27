#!/bin/sh
# Checks a freshly built asc executable before it is packaged.
#
#   smoke.sh <target triple> <executable> <version> <40-hex commit>
#
# * `asc --version` runs and says `asc <version> (...commit <first 12 of the commit>...)`. The
#   Linux aarch64 build runs under qemu-user; the macOS x86_64 build, built on an arm64 runner, is
#   checked with lipo and run only if Rosetta is there.
# * With ASC_SMOKE_GLIBC_MAX=2.35 a *-linux-gnu build may not need a newer glibc symbol version
#   than that (OBJDUMP names the objdump for the target; default objdump).
# * The executable is stripped: it has no symbol table.
set -eu

die() { echo "smoke.sh: $*" >&2; exit 1; }

[ $# -eq 4 ] || die "usage: smoke.sh <target> <executable> <version> <commit>"
target=$1; bin=$2; version=$3; commit=$4
[ -f "$bin" ] || die "no such file: $bin"
case "$commit" in
  *[!0-9a-f]*|'') die "bad commit" ;;
esac
[ "${#commit}" -eq 40 ] || die "bad commit"
short=$(printf '%s' "$commit" | cut -c1-12)

expect_version() {
  case "$1" in
    "asc $version ("*"commit $short)") echo "smoke.sh: $target: $1" ;;
    *) die "$target: unexpected --version output: $1" ;;
  esac
}

case "$target" in
  aarch64-unknown-linux-gnu)
    command -v qemu-aarch64 >/dev/null 2>&1 || die "qemu-aarch64 is required to run $target"
    expect_version "$(qemu-aarch64 -L "${QEMU_LD_PREFIX:-/usr/aarch64-linux-gnu}" "$bin" --version)"
    ;;
  x86_64-apple-darwin)
    [ "$(lipo -archs "$bin")" = x86_64 ] || die "$target: not an x86_64 executable"
    if arch -x86_64 /usr/bin/true >/dev/null 2>&1; then
      expect_version "$(arch -x86_64 "$bin" --version)"
    else
      echo "smoke.sh: $target: Rosetta is not available; architecture checked, not run"
    fi
    ;;
  x86_64-unknown-linux-gnu|aarch64-apple-darwin|x86_64-pc-windows-msvc)
    expect_version "$("$bin" --version)"
    ;;
  *) die "unknown target $target" ;;
esac

case "$target" in
  *-linux-gnu)
    if [ -n "${ASC_SMOKE_GLIBC_MAX:-}" ]; then
      objdump=${OBJDUMP:-objdump}
      needed=$("$objdump" -T "$bin" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -Vu | tail -n 1) \
        || die "$target: cannot read the dynamic symbols"
      [ -n "$needed" ] || die "$target: no glibc symbols found; is this a dynamic executable?"
      newest=$(printf '%s\n%s\n' "$needed" "$ASC_SMOKE_GLIBC_MAX" | sort -V | tail -n 1)
      [ "$newest" = "$ASC_SMOKE_GLIBC_MAX" ] || die "$target: needs glibc $needed, baseline is $ASC_SMOKE_GLIBC_MAX"
      echo "smoke.sh: $target: needs glibc $needed or older (baseline $ASC_SMOKE_GLIBC_MAX)"
    fi
    if "${OBJDUMP:-objdump}" -t "$bin" 2>/dev/null | grep -q ' F \.text'; then
      die "$target: the executable still has a symbol table; it was not stripped"
    fi
    ;;
esac
