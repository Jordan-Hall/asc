#!/usr/bin/env bash
# Local asc release (SPEC 16.2): build, package, checksum, sign and upload without CI.
# Mirrors .forgejo/workflows/release.yml: the same targets, file names and signature trusted comments.
#
#   build/release/local-release.sh [--dry-run] [--only linux|windows] [--no-build] [--no-upload]
#                                  [--latest-only] [--out <dir>] [--version <x.y.z>]
#
# Linux artefacts (zip, tar.gz) build on Linux or in WSL; Windows artefacts (zip, MSI) build on Windows
# in Git Bash. On Windows the Linux half runs through `wsl` automatically. Defaults build what the host can.
#
# Environment:
#   ASC_RELEASE_KEY_FILE    path to the minisign secret key; signs when set (never printed or copied)
#   ASC_RELEASE_PUBKEY      minisign public key text baked into install.sh (install.sh is skipped without it)
#   ASC_RELEASE_TOKEN       Forgejo token with package write access; needed to upload (header file, not argv)
#   ASC_RELEASES_URL        default https://forge.example.org/api/packages/asc/generic/asc
#   ASC_WSL_DISTRO          default Ubuntu
#   ASC_RELEASE_TARGET_DIR  cargo target dir (default E:/asct/release-target; ~/.cache/asc-release/target on Linux,
#                          which is on E: in the WSL distro)
# All build output goes to E:.
set -euo pipefail

TOOLCHAIN=1.95.0
WIN_TARGET=x86_64-pc-windows-msvc
LIN_TARGET=x86_64-unknown-linux-gnu
RELEASES_URL=${ASC_RELEASES_URL:-https://forge.example.org/api/packages/asc/generic/asc}
DISTRO=${ASC_WSL_DISTRO:-Ubuntu}

dry=0; only=""; build=1; upload=1; latest_only=0; out=""; ver=""
die() { echo "local-release: $*" >&2; exit 1; }
say() { echo "local-release: $*"; }
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) dry=1; shift ;;
    --only) [ $# -ge 2 ] || die "--only needs linux or windows"; only=$2; shift 2 ;;
    --no-build) build=0; shift ;;
    --no-upload) upload=0; shift ;;
    --latest-only) latest_only=1; build=0; shift ;;
    --out) [ $# -ge 2 ] || die "--out needs a directory"; out=$2; shift 2 ;;
    --version) [ $# -ge 2 ] || die "--version needs a value"; ver=$2; shift 2 ;;
    -h|--help) sed -n 2,19p "$0"; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done
case "$only" in ""|linux|windows) ;; *) die "--only must be linux or windows" ;; esac

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) host=windows ;;
  Linux) host=linux ;;
  *) die "unsupported host $(uname -s)" ;;
esac
in_wsl=0
if [ "$host" = linux ] && grep -qi microsoft /proc/version 2>/dev/null; then in_wsl=1; fi

want_linux=1; want_windows=1
[ "$only" = linux ] && want_windows=0
[ "$only" = windows ] && want_linux=0
if [ "$host" = linux ]; then
  [ "$only" = windows ] && die "Windows artefacts cannot be built on Linux"
  want_windows=0
fi

if [ -z "$ver" ]; then
  ver=$(cd "$root" && cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="asc"))') \
    || die "cannot read the asc crate version"
fi
printf '%s' "$ver" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "bad version: $ver"

if [ -z "$out" ]; then
  if [ "$host" = windows ]; then out=/e/asct/release/$ver
  elif [ "$in_wsl" = 1 ]; then out=/mnt/e/asct/release/$ver
  else out=${XDG_CACHE_HOME:-$HOME/.cache}/asc-release/out/$ver; fi
fi
if [ -n "${ASC_RELEASE_TARGET_DIR:-}" ]; then tdir=$ASC_RELEASE_TARGET_DIR
elif [ "$host" = windows ]; then tdir=E:/asct/release-target
else tdir=$HOME/.cache/asc-release/target; fi
[ -n "$out" ] && [ -n "$tdir" ] || die "empty output directory"

pick_python() {
  local p
  for p in python3 python py; do
    if command -v "$p" >/dev/null 2>&1 && "$p" -c 'import zipfile' >/dev/null 2>&1; then echo "$p"; return; fi
  done
  die "no working python for zip packaging"
}

sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }
wsl_path() { wsl -d "$DISTRO" -- wslpath -u "$1" | tr -d '\r'; }

say "version $ver  host $host  out $out  target-dir $tdir  dry-run $dry"
say "linux=$want_linux windows=$want_windows build=$build upload=$upload latest-only=$latest_only"

build_target() { # <target> <exe>
  local t=$1 exe=$2 bin py
  say "build $t with Rust $TOOLCHAIN"
  if [ "$dry" = 1 ]; then
    echo "DRY-RUN: CARGO_TARGET_DIR=$tdir cargo +$TOOLCHAIN build --release --locked -p asc --target $t"
    echo "DRY-RUN: zip $out/asc-$ver-$t.zip  ($exe)"
    case "$t" in *linux*) echo "DRY-RUN: tar $out/asc-$ver-$t.tar.gz" ;; esac
    return
  fi
  py=$(pick_python)
  rustup toolchain list | grep -q "^$TOOLCHAIN" || rustup toolchain install "$TOOLCHAIN" --profile minimal -t "$t"
  (cd "$root" && CARGO_TARGET_DIR=$tdir cargo "+$TOOLCHAIN" build --release --locked -p asc --target "$t")
  mkdir -p "$out"
  bin=$tdir/$t/release/$exe
  [ -f "$bin" ] || die "missing build output $bin"
  # The upgrade artefact: a flat zip with the executable.
  "$py" -c "import sys,zipfile; z=zipfile.ZipFile(sys.argv[1],'w',zipfile.ZIP_DEFLATED); z.write(sys.argv[2], sys.argv[3]); z.close()" \
    "$out/asc-$ver-$t.zip" "$bin" "$exe"
  case "$t" in *linux*) tar -czf "$out/asc-$ver-$t.tar.gz" -C "$tdir/$t/release" asc ;; esac
}

build_msi() {
  say "MSI with cargo-wix 0.3.9"
  if [ "$dry" = 1 ]; then
    echo "DRY-RUN: cargo install cargo-wix --locked --version 0.3.9   (if missing)"
    echo "DRY-RUN: cargo wix --nocapture --package asc --no-build --target $WIN_TARGET --output $out/asc-$ver-$WIN_TARGET.msi packaging/windows/main.wxs"
    return
  fi
  command -v cargo-wix >/dev/null 2>&1 || cargo install cargo-wix --locked --version 0.3.9
  (cd "$root" && CARGO_TARGET_DIR=$tdir cargo wix --nocapture --package asc --no-build --target "$WIN_TARGET" \
      --output "$(cygpath -w "$out/asc-$ver-$WIN_TARGET.msi")" packaging/windows/main.wxs)
}

if [ "$build" = 1 ]; then
  if [ "$want_linux" = 1 ]; then
    if [ "$host" = windows ]; then
      say "the Linux half runs in WSL ($DISTRO)"
      if [ "$dry" = 1 ]; then
        echo "DRY-RUN: wsl -d $DISTRO -- bash <wsl path of repo>/build/release/local-release.sh --only linux --no-upload --version $ver --out <wsl path of $out>"
      else
        wroot=$(wsl_path "$(cygpath -m "$root")"); mkdir -p "$out"; wout=$(wsl_path "$(cygpath -m "$out")")
        wsl -d "$DISTRO" -- bash "$wroot/build/release/local-release.sh" --only linux --no-upload --version "$ver" --out "$wout"
      fi
    else
      build_target "$LIN_TARGET" asc
    fi
  fi
  if [ "$want_windows" = 1 ]; then
    build_target "$WIN_TARGET" asc.exe
    build_msi
  fi
fi

# A Linux-only run (the WSL half, or a Linux host with no key and no upload) stops after the build.
if [ "$host" = linux ] && [ "$upload" = 0 ] && [ -z "${ASC_RELEASE_KEY_FILE:-}" ]; then say "Linux half done"; exit 0; fi

if [ "$dry" = 0 ] && [ ! -d "$out" ]; then die "no output directory $out"; fi

list_files() { (cd "$out" && find . -type f ! -name '*.minisig' ! -name SHA256SUMS | sed 's|^\./||' | LC_ALL=C sort); }

if [ "$latest_only" = 0 ]; then
  # Winget manifests and the Linux installer, as in the publish job.
  msi=$out/asc-$ver-$WIN_TARGET.msi
  if [ "$dry" = 1 ]; then
    say "would write winget manifests and, with ASC_RELEASE_PUBKEY set, install.sh and asc-daemon.service"
  else
    if [ -f "$msi" ]; then
      shaw=$(sha256 "$msi" | tr a-f A-F)
      mkdir -p "$out/winget"
      for f in "$root"/packaging/winget/*.yaml; do
        sed -e "s|@VERSION@|$ver|g" -e "s|@SHA256@|$shaw|g" -e "s|@RELEASES_URL@|$RELEASES_URL|g" "$f" > "$out/winget/$(basename "$f")"
      done
      say "winget manifests written"
    fi
    if [ -f "$out/asc-$ver-$LIN_TARGET.tar.gz" ]; then
      if [ -n "${ASC_RELEASE_PUBKEY:-}" ]; then
        sed "s|@RELEASE_PUBKEY@|$ASC_RELEASE_PUBKEY|" "$root/packaging/linux/install.sh" > "$out/install.sh"
        cp "$root/packaging/linux/asc-daemon.service" "$out/asc-daemon.service"
        say "install.sh written"
      else
        say "ASC_RELEASE_PUBKEY is unset: install.sh is not produced (it would refuse to run)"
      fi
    fi
  fi

  if [ "$dry" = 1 ]; then
    echo "DRY-RUN: write $out/SHA256SUMS (sha256 of every artefact, sorted)"
  else
    list_files | while read -r f; do printf '%s  %s\n' "$(sha256 "$out/$f")" "$f"; done > "$out/SHA256SUMS"
    say "wrote $out/SHA256SUMS"
  fi

  # Signing: minisign reads the key file; its content is never echoed or copied.
  if [ -n "${ASC_RELEASE_KEY_FILE:-}" ]; then
    msign() { # <file>; the trusted comment names the file, which `asc upgrade` checks
      local f=$1 b
      b=$(basename "$f")
      if command -v minisign >/dev/null 2>&1; then
        minisign -S -s "$ASC_RELEASE_KEY_FILE" -t "file:$b version:$ver" -m "$f"
      elif [ "$host" = windows ]; then
        wsl -d "$DISTRO" -- minisign -S -s "$(wsl_path "$(cygpath -m "$ASC_RELEASE_KEY_FILE")")" -t "file:$b version:$ver" -m "$(wsl_path "$(cygpath -m "$f")")"
      else die "minisign is not installed"; fi
    }
    if [ "$dry" = 1 ]; then
      echo "DRY-RUN: minisign -S -s <ASC_RELEASE_KEY_FILE> -t file:<name>,version:$ver -m <each artefact and SHA256SUMS>"
    else
      [ -f "$ASC_RELEASE_KEY_FILE" ] || die "ASC_RELEASE_KEY_FILE does not exist"
      { list_files; echo SHA256SUMS; } | while read -r f; do
        rm -f "${out:?}/${f:?}.minisig"
        msign "$out/$f" </dev/null
      done
    fi
  else
    say "ASC_RELEASE_KEY_FILE is unset: artefacts are NOT signed (asc upgrade and install.sh refuse unsigned files)"
  fi
fi

# Upload. The token goes into a 0600 header file that curl reads; it never appears in argv.
if [ "$upload" = 1 ]; then
  hdr=""; vfile=""
  cleanup() { [ -z "$hdr" ] || rm -f "$hdr"; [ -z "$vfile" ] || rm -f "$vfile"; }
  trap cleanup EXIT
  if [ "$dry" = 0 ]; then
    [ -n "${ASC_RELEASE_TOKEN:-}" ] || die "ASC_RELEASE_TOKEN is not set (use --no-upload to stop before uploading)"
    hdr=$(umask 077; mktemp)
    chmod 600 "$hdr"
    printf 'Authorization: token %s\n' "$ASC_RELEASE_TOKEN" > "$hdr"
  fi
  put() { # <local file> <remote path>; a re-run replaces the file
    if [ "$dry" = 1 ]; then echo "DRY-RUN: PUT $1 -> $RELEASES_URL/$2"; return; fi
    curl -sS --proto '=https' -X DELETE -H "@$hdr" "$RELEASES_URL/$2" >/dev/null || true
    curl -fsS --proto '=https' -X PUT -H "@$hdr" --upload-file "$1" "$RELEASES_URL/$2"
  }
  if [ "$latest_only" = 0 ]; then
    if [ "$dry" = 1 ]; then
      echo "DRY-RUN: PUT every file under $out (artefacts, .minisig, SHA256SUMS, winget/, install.sh) -> $RELEASES_URL/$ver/<file>"
    else
      (cd "$out" && find . -type f | sed 's|^\./||' | LC_ALL=C sort) | while read -r f; do put "$out/$f" "$ver/$f"; done
    fi
  fi
  # latest/version moves last, and only when both the Windows and the Linux zip exist.
  have_both=1
  if [ "$dry" = 1 ]; then
    [ -z "$only" ] && [ "$host" = windows ] || have_both=0
  else
    for t in "$WIN_TARGET" "$LIN_TARGET"; do [ -f "$out/asc-$ver-$t.zip" ] || have_both=0; done
  fi
  if [ "$have_both" = 1 ]; then
    if [ "$dry" = 1 ]; then
      echo "DRY-RUN: PUT latest/version = $ver (last, after the files above)"
    else
      vfile=$(mktemp); printf '%s\n' "$ver" > "$vfile"; put "$vfile" latest/version
    fi
  else
    say "latest/version NOT moved: both the Windows and the Linux zip must exist in $out"
  fi
else
  say "upload skipped (--no-upload)"
fi
say "done"
