#!/usr/bin/env bash
# Tests assemble.py, sign.sh, verify.py and publish-to-hub.sh end to end on fake binaries.
#   bash scripts/release/test/test-release.sh
# The signature cases run when `minisign` is installed and are reported as skipped otherwise.
set -u
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

work=$(mktemp -d "${TMPDIR:-/tmp}/asc-release-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
SH=${TEST_SH:-sh}
PUBLISH="$REL/publish-to-hub.sh"

have_minisign=0
command -v minisign >/dev/null 2>&1 && have_minisign=1

check "scripts parse under sh" bash -c "for f in '$REL/sign.sh' '$REL/publish-to-hub.sh' '$REL/linux-packages.sh' '$ROOT/scripts/install.sh'; do $SH -n \"\$f\" || exit 1; done"
if command -v shellcheck >/dev/null 2>&1; then
  check "shellcheck is clean on the release scripts" shellcheck --severity=warning --shell=sh "$REL/sign.sh" "$REL/publish-to-hub.sh" "$REL/linux-packages.sh" "$REL/smoke.sh"
else
  echo "skip shellcheck (not installed)"
fi

# ---- assemble and verify -----------------------------------------------------------------------

make_release "$work/r123" 1.2.3 || { echo "assemble failed" >&2; exit 1; }
check "a fresh release verifies" "$PY" "$REL/verify.py" "$work/r123"
make_release "$work/r123b" 1.2.3 "$work/r123.bins" || exit 1
if cmp -s "$work/r123/SHA256SUMS" "$work/r123b/SHA256SUMS"; then ok "two assemblies of one input are byte identical"; else bad "two assemblies of one input are byte identical"; fi
refuse "assemble refuses a non-empty output directory" make_release "$work/r123" 1.2.3 "$work/r123.bins"

# ---- linux packages ----------------------------------------------------------------------------

mkdir -p "$work/fakebin"
cat > "$work/fakebin/nfpm" <<'FAKE'
#!/bin/sh
# Stand-in for nfpm: checks the config and the unpacked files, then writes the package.
cfg=''; pk=''; tgt=''
while [ $# -gt 0 ]; do case "$1" in --config) cfg=$2; shift 2 ;; --packager) pk=$2; shift 2 ;; --target) tgt=$2; shift 2 ;; *) shift ;; esac; done
[ -x ./asc ] && [ -f ./LICENSE-MIT.txt ] || exit 1
grep -q '^version: "1.2.3"$' "$cfg" || exit 1
echo package > "${tgt}asc_1.2.3_$(sed -n 's/^arch: //p' "$cfg").$pk"
FAKE
chmod +x "$work/fakebin/nfpm"
check "linux packages build from a verified release" env PATH="$work/fakebin:$PATH" "$SH" "$REL/linux-packages.sh" --release "$work/r123" --out "$work/pkgs"
[ -f "$work/pkgs/asc_1.2.3_amd64.deb" ] && [ -f "$work/pkgs/asc_1.2.3_arm64.rpm" ] && ok "a deb and an rpm exist for each architecture" || bad "a deb and an rpm exist for each architecture"
cp -R "$work/r123" "$work/tampered"
printf 'x' >> "$work/tampered/asc-1.2.3-x86_64-unknown-linux-gnu.tar.gz"
refuse "a tampered archive builds no package" env PATH="$work/fakebin:$PATH" "$SH" "$REL/linux-packages.sh" --release "$work/tampered" --out "$work/pkgs2"
contains "the refusal names the checksum" "$out" "SHA256SUMS"
cp -R "$work/r123" "$work/cfgtamper"
printf 'scripts:
  postinstall: /tmp/x
' >> "$work/cfgtamper/nfpm-amd64.yaml"
refuse "a modified nfpm config builds no package" env PATH="$work/fakebin:$PATH" "$SH" "$REL/linux-packages.sh" --release "$work/cfgtamper" --out "$work/pkgs4"
contains "the refusal names the config" "$out" "nfpm-amd64.yaml"
cp -R "$work/r123" "$work/cfgextra"
rm "$work/cfgextra/nfpm-arm64.yaml"
refuse "a missing nfpm config builds no package" env PATH="$work/fakebin:$PATH" "$SH" "$REL/linux-packages.sh" --release "$work/cfgextra" --out "$work/pkgs5"
refuse "a key without a signature file is refused" env PATH="$work/fakebin:$PATH" ASC_RELEASE_PUBKEY=RWQexample "$SH" "$REL/linux-packages.sh" --release "$work/r123" --out "$work/pkgs3"

# ---- rendered manifests parse where a tool exists ------------------------------------------------

check "the PKGBUILD parses as bash" bash -n "$work/r123/PKGBUILD"
if command -v ruby >/dev/null 2>&1; then
  check "the Homebrew formula parses as ruby" ruby -c "$work/r123/asc.rb"
else
  echo "skip ruby syntax (ruby not installed)"
fi
if "$PY" -c 'import yaml' 2>/dev/null; then
  check "winget and nfpm files parse as yaml" "$PY" -c 'import sys, yaml
for f in sys.argv[1:]: yaml.safe_load(open(f, encoding="utf-8"))' "$work"/r123/winget-*.yaml "$work"/r123/nfpm-*.yaml
else
  echo "skip yaml syntax (PyYAML not installed)"
fi

# ---- publish: refusals leave the hub untouched -------------------------------------------------

mkdir -p "$work/hub"
refuse "publish needs a key or an explicit skip" "$SH" "$PUBLISH" --from "$work/r123" --to "$work/hub"
contains "the refusal names the key" "$out" "ASC_RELEASE_PUBKEY"
refuse "publish needs an existing download directory" "$SH" "$PUBLISH" --from "$work/r123" --to "$work/nope" --skip-signature-check
refuse "publish rejects unknown options" "$SH" "$PUBLISH" --from "$work/r123" --to "$work/hub" --frob
[ -z "$(ls -A "$work/hub")" ] && ok "the hub is still empty" || bad "the hub is still empty"

cp -R "$work/r123" "$work/bad"
printf 'x' >> "$work/bad/asc-1.2.3-x86_64-unknown-linux-gnu.zip"
refuse "a tampered release is not published" "$SH" "$PUBLISH" --from "$work/bad" --to "$work/hub" --skip-signature-check
[ -z "$(ls -A "$work/hub")" ] && ok "nothing was copied from the tampered release" || bad "nothing was copied from the tampered release"

# ---- publish: layout, immutability, latest -----------------------------------------------------

out=$("$SH" "$PUBLISH" --from "$work/r123" --to "$work/hub" --skip-signature-check --dry-run 2>&1)
contains "a dry run says what it would do" "$out" "dry run: would copy"
[ -z "$(ls -A "$work/hub")" ] && ok "a dry run changes nothing" || bad "a dry run changes nothing"

check "publish copies a release" "$SH" "$PUBLISH" --from "$work/r123" --to "$work/hub" --skip-signature-check
[ -f "$work/hub/1.2.3/manifest.json" ] && [ -f "$work/hub/1.2.3/asc-1.2.3-aarch64-apple-darwin.tar.gz" ] && ok "the version directory has the archives and the manifest" || bad "the version directory has the archives and the manifest"
[ "$(cat "$work/hub/latest/version")" = "1.2.3" ] && ok "latest/version names the version" || bad "latest/version names the version"
cmp -s "$work/hub/latest/manifest.json" "$work/r123/manifest.json" && ok "latest/manifest.json is the manifest" || bad "latest/manifest.json is the manifest"
check "the published copy verifies" "$PY" "$REL/verify.py" "$work/hub/1.2.3"
[ -z "$(find "$work/hub" -name '.*' -not -path "$work/hub" | head -1)" ] && ok "no staging files are left behind" || bad "no staging files are left behind"
refuse "a published version is immutable" "$SH" "$PUBLISH" --from "$work/r123" --to "$work/hub" --skip-signature-check
contains "the refusal says releases are immutable" "$out" "immutable"

make_release "$work/r124" 1.2.4 || exit 1
check "a newer version publishes and moves latest" "$SH" "$PUBLISH" --from "$work/r124" --to "$work/hub" --skip-signature-check
[ "$(cat "$work/hub/latest/version")" = "1.2.4" ] && ok "latest moved to 1.2.4" || bad "latest moved to 1.2.4"

make_release "$work/r122" 1.2.2 || exit 1
refuse "latest does not move backwards" "$SH" "$PUBLISH" --from "$work/r122" --to "$work/hub" --skip-signature-check
contains "the refusal names the newer version" "$out" "1.2.4"
[ ! -e "$work/hub/1.2.2" ] && ok "nothing of the older release was copied" || bad "nothing of the older release was copied"
check "a back-port publishes with --no-latest" "$SH" "$PUBLISH" --from "$work/r122" --to "$work/hub" --skip-signature-check --no-latest
[ "$(cat "$work/hub/latest/version")" = "1.2.4" ] && ok "latest stayed at 1.2.4" || bad "latest stayed at 1.2.4"

printf 'junk\n' > "$work/hub/latest/version"
refuse "a damaged latest/version is refused" "$SH" "$PUBLISH" --from "$work/r124" --to "$work/hub" --skip-signature-check --force
printf '1.2.4\n' > "$work/hub/latest/version"

# --force replaces a version (a re-run after a bad upload) and may move latest.
check "--force replaces a published version" "$SH" "$PUBLISH" --from "$work/r124" --to "$work/hub" --skip-signature-check --force
check "the replaced version verifies" "$PY" "$REL/verify.py" "$work/hub/1.2.4"

# ---- smoke checks ------------------------------------------------------------------------------

SMOKE="$REL/smoke.sh"
mkdir -p "$work/smoke"
printf '#!/bin/sh\necho "asc 1.2.3 (git 2.55.0, commit %s)"\n' "$(printf '%s' "$COMMIT" | cut -c1-12)" > "$work/smoke/asc"
chmod 755 "$work/smoke/asc"
check "smoke accepts the expected version line" "$SH" "$SMOKE" x86_64-unknown-linux-gnu "$work/smoke/asc" 1.2.3 "$COMMIT"
refuse "smoke refuses another version" "$SH" "$SMOKE" x86_64-unknown-linux-gnu "$work/smoke/asc" 1.2.4 "$COMMIT"
refuse "smoke refuses another commit" "$SH" "$SMOKE" x86_64-unknown-linux-gnu "$work/smoke/asc" 1.2.3 "ffffffffffffffffffffffffffffffffffffffff"
refuse "smoke refuses a short commit" "$SH" "$SMOKE" x86_64-unknown-linux-gnu "$work/smoke/asc" 1.2.3 abc123
refuse "smoke refuses an unknown target" "$SH" "$SMOKE" riscv64-unknown-linux-gnu "$work/smoke/asc" 1.2.3 "$COMMIT"

# A stand-in objdump: -T lists dynamic symbols, -t the symbol table.
cat > "$work/smoke/objdump" <<'EOF'
#!/bin/sh
case "$1" in
  -T) printf '0000 g DF *UND* 0000 GLIBC_2.2.5 memcpy\n0000 g DF *UND* 0000 (GLIBC_2.34) pthread_create\n0000 g DF *UND* 0000 GLIBC_2.9 dup3\n' ;;
  -t) [ -n "${FAKE_SYMBOLS:-}" ] && printf '0000 g F .text 0000 main\n' ;;
esac
exit 0
EOF
chmod 755 "$work/smoke/objdump"
check "smoke accepts a glibc within the baseline" env OBJDUMP="$work/smoke/objdump" ASC_SMOKE_GLIBC_MAX=2.35 "$SH" "$SMOKE" x86_64-unknown-linux-gnu "$work/smoke/asc" 1.2.3 "$COMMIT"
check "smoke accepts a glibc equal to the baseline" env OBJDUMP="$work/smoke/objdump" ASC_SMOKE_GLIBC_MAX=2.34 "$SH" "$SMOKE" x86_64-unknown-linux-gnu "$work/smoke/asc" 1.2.3 "$COMMIT"
refuse "smoke refuses a glibc newer than the baseline" env OBJDUMP="$work/smoke/objdump" ASC_SMOKE_GLIBC_MAX=2.17 "$SH" "$SMOKE" x86_64-unknown-linux-gnu "$work/smoke/asc" 1.2.3 "$COMMIT"
contains "the refusal names the glibc version" "$out" "needs glibc 2.34"
refuse "smoke refuses an executable that was not stripped" env FAKE_SYMBOLS=1 OBJDUMP="$work/smoke/objdump" "$SH" "$SMOKE" x86_64-unknown-linux-gnu "$work/smoke/asc" 1.2.3 "$COMMIT"
refuse "smoke fails closed when the symbols cannot be read" env OBJDUMP="$work/smoke/nonexistent" ASC_SMOKE_GLIBC_MAX=2.35 "$SH" "$SMOKE" x86_64-unknown-linux-gnu "$work/smoke/asc" 1.2.3 "$COMMIT"

# ---- signatures --------------------------------------------------------------------------------

refuse "sign.sh refuses without a key" env -u MINISIGN_KEY_FILE "$SH" "$REL/sign.sh" "$work/r123" 1.2.3
contains "the refusal says it will not publish unsigned" "$out" "unsigned"
refuse "sign.sh refuses a bad version" env MINISIGN_KEY_FILE=/nonexistent "$SH" "$REL/sign.sh" "$work/r123" "1.2"

if [ $have_minisign -eq 1 ]; then
  mkdir -p "$work/keys"
  pubkey=$(new_key "$work/keys")
  make_release "$work/s130" 1.3.0 || exit 1
  MINISIGN_KEY_FILE="$work/keys/key" "$SH" "$REL/sign.sh" "$work/s130" 1.3.0 >/dev/null && ok "sign.sh signs a release" || bad "sign.sh signs a release"
  check "a signed release verifies with the key" "$PY" "$REL/verify.py" "$work/s130" --pubkey "$pubkey"
  [ -f "$work/s130/SHA256SUMS.minisig" ] && [ -f "$work/s130/manifest.json.minisig" ] && [ -f "$work/s130/winget-asc.asc.yaml.minisig" ] && ok "every file has a signature" || bad "every file has a signature"
  grep -q "file:asc-1.3.0-aarch64-apple-darwin.zip version:1.3.0" "$work/s130/asc-1.3.0-aarch64-apple-darwin.zip.minisig" && ok "the trusted comment names file and version" || bad "the trusted comment names file and version"

  other=$(mkdir -p "$work/keys2" && new_key "$work/keys2")
  refuse "another key does not verify" "$PY" "$REL/verify.py" "$work/s130" --pubkey "$other"

  cp -R "$work/s130" "$work/s130-missing"
  rm "$work/s130-missing/asc-1.3.0-x86_64-apple-darwin.zip.minisig"
  refuse "a missing signature is refused" "$PY" "$REL/verify.py" "$work/s130-missing" --pubkey "$pubkey"

  cp -R "$work/s130" "$work/s130-swap"
  cp "$work/s130-swap/asc-1.3.0-aarch64-apple-darwin.zip.minisig" "$work/s130-swap/asc-1.3.0-x86_64-apple-darwin.zip.minisig"
  refuse "a signature of another file is refused" "$PY" "$REL/verify.py" "$work/s130-swap" --pubkey "$pubkey"

  refuse "publish with the wrong key is refused" "$SH" "$PUBLISH" --from "$work/s130" --to "$work/hub" --pubkey "$other"
  [ ! -e "$work/hub/1.3.0" ] && ok "nothing was published with the wrong key" || bad "nothing was published with the wrong key"
  check "the key from the environment is used" env ASC_RELEASE_PUBKEY="$pubkey" "$SH" "$PUBLISH" --from "$work/s130" --to "$work/hub" --dry-run
  check "publish with the right key works" "$SH" "$PUBLISH" --from "$work/s130" --to "$work/hub" --pubkey "$pubkey"
  [ "$(cat "$work/hub/latest/version")" = "1.3.0" ] && ok "latest is 1.3.0" || bad "latest is 1.3.0"
  [ -f "$work/hub/1.3.0/SHA256SUMS.minisig" ] && ok "the signatures are published with the files" || bad "the signatures are published with the files"
else
  echo "skip signature cases (minisign is not installed)"
fi

finish
