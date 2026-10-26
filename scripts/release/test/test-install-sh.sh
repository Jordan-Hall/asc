#!/usr/bin/env bash
# Tests scripts/install.sh against a local fixture release host (no network beyond 127.0.0.1).
#   bash scripts/release/test/test-install-sh.sh
# The installer runs under `sh` (dash on Debian and Ubuntu), the way a user pipes it.
# Signature cases run when `minisign` is installed and are reported as skipped otherwise.
set -u
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

case "$(uname -s)" in
  Linux|Darwin) ;;
  *) echo "skip: install.sh runs on Linux and macOS (this is $(uname -s))"; exit 0 ;;
esac

work=$(mktemp -d "${TMPDIR:-/tmp}/asc-install-test.XXXXXX")
trap 'stop_server; rm -rf "$work"' EXIT
INSTALL="$ROOT/scripts/install.sh"
SH=${TEST_SH:-sh}

case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) echo "skip: no fixture target for $(uname -m)"; exit 0 ;;
esac
case "$(uname -s)" in
  Linux) target="$arch-unknown-linux-gnu" ;;
  Darwin) target="$arch-apple-darwin" ;;
esac

# ---- a release host: 1.2.3 (latest) and 1.2.4 ---------------------------------------------------

have_minisign=0
command -v minisign >/dev/null 2>&1 && have_minisign=1
pubkey=''
mkdir -p "$work/keys"
if [ $have_minisign -eq 1 ]; then pubkey=$(new_key "$work/keys"); fi

publish() {
  local v=$1 rel; rel="$work/rel-$v"
  make_release "$rel" "$v" || { echo "make_release $v failed" >&2; exit 1; }
  if [ $have_minisign -eq 1 ]; then
    MINISIGN_KEY_FILE="$work/keys/key" sh "$REL/sign.sh" "$rel" "$v" >/dev/null || exit 1
    sh "$REL/publish-to-hub.sh" --from "$rel" --to "$work/host" --pubkey "$pubkey" >/dev/null || exit 1
  else
    sh "$REL/publish-to-hub.sh" --from "$rel" --to "$work/host" --skip-signature-check >/dev/null 2>&1 || exit 1
  fi
}
mkdir -p "$work/host"
publish 1.2.3
publish 1.2.4
# 1.2.4 is published but latest stays at 1.2.3 for the first tests.
printf '1.2.3\n' > "$work/host/latest/version"
serve "$work/host" "$work/srv"

new_home() { rm -rf "$work/h"; mkdir -p "$work/h/tmp"; }
# inst_args <args...>: install.sh with a clean environment and the fixture host.
inst_args() { env -i PATH="$PATH" HOME="$work/h" TMPDIR="$work/h/tmp" ASC_ALLOW_LOOPBACK_HTTP=1 ASC_DOWNLOAD_BASE_URL="$BASE" "$SH" "$INSTALL" "$@"; }
prefix="$work/h/.local/share/asc"
bindir="$work/h/.local/bin"

# ---- syntax and help ---------------------------------------------------------------------------

check "install.sh parses under sh" "$SH" -n "$INSTALL"
if command -v shellcheck >/dev/null 2>&1; then
  check "shellcheck is clean on install.sh" shellcheck --severity=warning --shell=sh "$INSTALL"
else
  echo "skip shellcheck (not installed)"
fi
out=$(inst_args --help 2>&1); contains "--help names the base URL variable" "$out" "ASC_DOWNLOAD_BASE_URL"

# ---- dry run -----------------------------------------------------------------------------------

new_home
: > "$LOG"
out=$(inst_args --dry-run 2>&1); rc=$?
[ $rc -eq 0 ] && ok "dry run succeeds" || bad "dry run succeeds" "$out"
contains "dry run names the version" "$out" "asc 1.2.3 for $target"
contains "dry run names the archive url" "$out" "$BASE/1.2.3/asc-1.2.3-$target.tar.gz"
contains "dry run says nothing was written" "$out" "nothing was downloaded or written"
[ ! -e "$prefix" ] && [ ! -e "$bindir" ] && ok "dry run writes nothing" || bad "dry run writes nothing"
lacks "dry run downloads no archive" "$(cat "$LOG")" ".tar.gz"

# ---- install latest ----------------------------------------------------------------------------

new_home
out=$(inst_args 2>&1); rc=$?
[ $rc -eq 0 ] && ok "install succeeds" || bad "install succeeds" "$out"
[ -x "$prefix/1.2.3/asc" ] && ok "the version directory holds asc" || bad "the version directory holds asc"
[ -d "$prefix/1.2.3/shim" ] && ok "the shim directory exists for self-update" || bad "the shim directory exists for self-update"
[ "$(readlink "$prefix/current")" = "1.2.3" ] && ok "current points at the version" || bad "current points at the version"
[ "$(readlink "$bindir/asc")" = "$prefix/current/asc" ] && ok "the bin link goes through current" || bad "the bin link goes through current"
contains "the installed asc runs" "$("$bindir/asc" --version)" "asc 1.2.3"
contains "a missing PATH entry gets a hint" "$out" "is not on your PATH"
lacks "no secret or base url credentials printed" "$out" "hunter2"
req=$(cat "$LOG")
contains "latest/version was read" "$req" "/latest/version"
contains "SHA256SUMS was read" "$req" "/1.2.3/SHA256SUMS"
lacks "no other version was fetched" "$req" "/1.2.4/"

# PATH containing the bin dir: no hint.
out=$(env -i PATH="$bindir:$PATH" HOME="$work/h" TMPDIR="$work/h/tmp" ASC_ALLOW_LOOPBACK_HTTP=1 ASC_DOWNLOAD_BASE_URL="$BASE" "$SH" "$INSTALL" 2>&1)
lacks "no hint when the bin dir is on PATH" "$out" "is not on your PATH"
contains "installing the same version again is a no-op" "$out" "was already installed"

# ---- upgrade side by side ----------------------------------------------------------------------

out=$(inst_args --version 1.2.4 2>&1); rc=$?
[ $rc -eq 0 ] && ok "a second version installs" || bad "a second version installs" "$out"
[ "$(readlink "$prefix/current")" = "1.2.4" ] && ok "current moved" || bad "current moved"
[ -x "$prefix/1.2.3/asc" ] && ok "the old version is untouched" || bad "the old version is untouched"
contains "the link now runs the new version" "$("$bindir/asc" --version)" "asc 1.2.4"

# ---- failures leave nothing behind -------------------------------------------------------------

new_home
refuse "an unpublished version is refused" inst_args --version 9.9.9
contains "the unpublished-version message names the version" "$out" "9.9.9"
[ ! -e "$prefix" ] && ok "nothing was installed for the unpublished version" || bad "nothing was installed for the unpublished version"

for v in 1.2 1.2.3.4 01.2.3 1.2.x '../1.2.3' '1.2.3-rc1'; do
  refuse "version '$v' is refused" inst_args --version "$v"
done

new_home
refuse "no base URL is refused" env -i PATH="$PATH" HOME="$work/h" TMPDIR="$work/h/tmp" "$SH" "$INSTALL"
contains "no base URL says how to set one" "$out" "ASC_DOWNLOAD_BASE_URL"
refuse "plain http to a remote host is refused" env -i PATH="$PATH" HOME="$work/h" ASC_ALLOW_LOOPBACK_HTTP=1 "$SH" "$INSTALL" --base-url http://releases.example.invalid/asc
refuse "loopback http needs the opt-in" env -i PATH="$PATH" HOME="$work/h" "$SH" "$INSTALL" --base-url "$BASE"
contains "the loopback message names the opt-in" "$out" "ASC_ALLOW_LOOPBACK_HTTP=1"
refuse "credentials in the base URL are refused" inst_args --base-url "https://user:hunter2@releases.example.invalid/asc"
lacks "the credentials are not echoed" "$out" "hunter2"
refuse "a query in the base URL is refused" inst_args --base-url "https://releases.example.invalid/asc?token=abc"
refuse "other schemes are refused" inst_args --base-url "ftp://releases.example.invalid/asc"
refuse "an unknown option is refused" inst_args --frobnicate

# A tampered archive: the checksum stops it and nothing is installed.
new_home
cp "$work/host/1.2.3/asc-1.2.3-$target.tar.gz" "$work/orig.tar.gz"
printf 'tampered' >> "$work/host/1.2.3/asc-1.2.3-$target.tar.gz"
refuse "a tampered archive is refused" inst_args --version 1.2.3
contains "the mismatch is reported" "$out" "checksum mismatch"
[ ! -e "$prefix/1.2.3" ] && [ ! -e "$bindir/asc" ] && ok "nothing was installed from the tampered archive" || bad "nothing was installed from the tampered archive"
cp "$work/orig.tar.gz" "$work/host/1.2.3/asc-1.2.3-$target.tar.gz"

# SHA256SUMS without an entry for this platform.
new_home
cp "$work/host/1.2.3/SHA256SUMS" "$work/orig.sums"
grep -v "asc-1.2.3-$target.tar.gz" "$work/orig.sums" > "$work/host/1.2.3/SHA256SUMS"
refuse "a missing SHA256SUMS entry is refused" inst_args --version 1.2.3
contains "the missing entry is reported" "$out" "no single entry"
# Two entries for one file are ambiguous.
grep "asc-1.2.3-$target.tar.gz" "$work/orig.sums" >> "$work/host/1.2.3/SHA256SUMS"
grep "asc-1.2.3-$target.tar.gz" "$work/orig.sums" >> "$work/host/1.2.3/SHA256SUMS"
refuse "a duplicated SHA256SUMS entry is refused" inst_args --version 1.2.3
cp "$work/orig.sums" "$work/host/1.2.3/SHA256SUMS"

# An archive whose asc is a symbolic link, with matching sums (a compromised publisher).
new_home
"$PY" -I - "$work/host/1.2.3" "$target" <<'PYEOF'
import hashlib, io, sys, tarfile
d, target = sys.argv[1], sys.argv[2]
name = f"asc-1.2.3-{target}.tar.gz"
buf = io.BytesIO()
with tarfile.open(fileobj=buf, mode="w:gz") as t:
    i = tarfile.TarInfo("asc"); i.type = tarfile.SYMTYPE; i.linkname = "/etc/passwd"; t.addfile(i)
data = buf.getvalue()
open(f"{d}/{name}", "wb").write(data)
lines = [l for l in open(f"{d}/SHA256SUMS", encoding="utf-8") if not l.endswith("  " + name + "\n")]
lines.append(f"{hashlib.sha256(data).hexdigest()}  {name}\n")
open(f"{d}/SHA256SUMS", "w", encoding="utf-8").write("".join(lines))
PYEOF
refuse "an archive with a linked asc is refused" inst_args --version 1.2.3
[ ! -e "$prefix/1.2.3" ] && ok "nothing was installed from the linked archive" || bad "nothing was installed from the linked archive"
cp "$work/orig.tar.gz" "$work/host/1.2.3/asc-1.2.3-$target.tar.gz"
cp "$work/orig.sums" "$work/host/1.2.3/SHA256SUMS"

# A file in the bin dir that is not ours is never replaced.
new_home
mkdir -p "$bindir"; printf 'mine\n' > "$bindir/asc"
refuse "a foreign bin/asc is not overwritten" inst_args --version 1.2.3
[ "$(cat "$bindir/asc")" = mine ] && ok "the foreign file is unchanged" || bad "the foreign file is unchanged"
[ ! -e "$prefix/1.2.3" ] && ok "nothing was installed beside the foreign file" || bad "nothing was installed beside the foreign file"

# A version directory with other content is not overwritten.
new_home
mkdir -p "$prefix/1.2.3"; printf 'other' > "$prefix/1.2.3/asc"
refuse "an existing different version directory is refused" inst_args --version 1.2.3

# Custom prefix and bin dir.
new_home
out=$(inst_args --version 1.2.3 --prefix "$work/h/p" --bin-dir "$work/h/b" 2>&1) \
  && [ -x "$work/h/p/1.2.3/asc" ] && [ -L "$work/h/b/asc" ] && ok "--prefix and --bin-dir are honoured" || bad "--prefix and --bin-dir are honoured" "$out"

# ---- signatures --------------------------------------------------------------------------------

if [ $have_minisign -eq 1 ]; then
  printf '1.2.4\n' > "$work/host/latest/version"
  new_home
  out=$(inst_args --version 1.2.3 2>&1) && ok "unsigned mode still installs a signed release" || bad "unsigned mode still installs a signed release" "$out"
  contains "unsigned mode says what the checksum proves" "$out" "checksum only"

  new_home
  out=$(env -i PATH="$PATH" HOME="$work/h" TMPDIR="$work/h/tmp" ASC_ALLOW_LOOPBACK_HTTP=1 ASC_DOWNLOAD_BASE_URL="$BASE" ASC_RELEASE_PUBKEY="$pubkey" "$SH" "$INSTALL" 2>&1) \
    && ok "a valid signature installs" || bad "a valid signature installs" "$out"
  contains "the plan says it is signed" "$out" "signed by the configured key"

  new_home
  other=$(mkdir -p "$work/keys2" && new_key "$work/keys2")
  refuse "another key's signature is refused" env -i PATH="$PATH" HOME="$work/h" TMPDIR="$work/h/tmp" ASC_ALLOW_LOOPBACK_HTTP=1 ASC_DOWNLOAD_BASE_URL="$BASE" ASC_RELEASE_PUBKEY="$other" "$SH" "$INSTALL"
  [ ! -e "$prefix" ] && ok "nothing was installed for the wrong key" || bad "nothing was installed for the wrong key"

  # A tampered SHA256SUMS fails the signature even though the archive hash inside is consistent.
  new_home
  cp "$work/host/1.2.4/SHA256SUMS" "$work/orig.sums4"
  printf '%s  extra\n' "$(printf x | sha256sum 2>/dev/null | cut -d' ' -f1)" >> "$work/host/1.2.4/SHA256SUMS"
  refuse "a modified SHA256SUMS fails the signature" env -i PATH="$PATH" HOME="$work/h" TMPDIR="$work/h/tmp" ASC_ALLOW_LOOPBACK_HTTP=1 ASC_DOWNLOAD_BASE_URL="$BASE" ASC_RELEASE_PUBKEY="$pubkey" "$SH" "$INSTALL"
  cp "$work/orig.sums4" "$work/host/1.2.4/SHA256SUMS"

  # A signature made for another version's SHA256SUMS cannot be replayed.
  new_home
  cp "$work/host/1.2.4/SHA256SUMS.minisig" "$work/orig.sig4"
  cp "$work/host/1.2.3/SHA256SUMS.minisig" "$work/host/1.2.4/SHA256SUMS.minisig"
  refuse "a replayed signature is refused" env -i PATH="$PATH" HOME="$work/h" TMPDIR="$work/h/tmp" ASC_ALLOW_LOOPBACK_HTTP=1 ASC_DOWNLOAD_BASE_URL="$BASE" ASC_RELEASE_PUBKEY="$pubkey" "$SH" "$INSTALL"
  cp "$work/orig.sig4" "$work/host/1.2.4/SHA256SUMS.minisig"

  # A configured key with no minisign installed stops the install.
  new_home
  mkdir -p "$work/nobin"
  for c in sh uname curl wget tar awk sed grep head tr cut wc mktemp rm mkdir mv cp ln chmod cat readlink sha256sum shasum openssl dirname basename sysctl; do
    p=$(command -v "$c" 2>/dev/null) && [ -x "$p" ] && ln -sf "$p" "$work/nobin/$c"
  done
  refuse "a key without minisign stops the install" env -i PATH="$work/nobin" HOME="$work/h" TMPDIR="$work/h/tmp" ASC_ALLOW_LOOPBACK_HTTP=1 ASC_DOWNLOAD_BASE_URL="$BASE" ASC_RELEASE_PUBKEY="$pubkey" "$work/nobin/sh" "$INSTALL"
  contains "the message names minisign" "$out" "minisign"
  [ ! -e "$prefix" ] && ok "nothing was installed without minisign" || bad "nothing was installed without minisign"
else
  echo "skip signature cases (minisign is not installed)"
fi

finish
