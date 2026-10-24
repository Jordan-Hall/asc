#!/bin/sh
# U88: nothing we ship may weaken ssh host-key checking (SPEC 15.6.1, 15.6.4 item 8, 25.11.9).
# The Rust sources, scripts and generated config under the scanned directories must not name
# StrictHostKeyChecking, UserKnownHostsFile, CheckHostIP or VerifyHostKeyDNS, except the files
# below: they are the SSH test files that assert the options are absent from what asc runs (and
# pin a test host key through an explicit known_hosts file, which is the one safe use), and
# this script. Run from the repository root; NO_SSH_WEAKENING_ROOT scans another tree (the
# script's own test plants an option there and expects a failure).
set -eu

cd "$(dirname "$0")/../.."
root="${NO_SSH_WEAKENING_ROOT:-.}"

allow='^(\./)?(crates/asc/tests/git_ssh/(main|openssh|migrate)\.rs|scripts/ci/no-ssh-weakening\.sh)$'
words='StrictHostKeyChecking|UserKnownHostsFile|CheckHostIP|VerifyHostKeyDNS'

found=""
for dir in crates scripts deploy actions .forgejo .cargo; do
    [ -d "$root/$dir" ] || continue
    hits=$(grep -rIlE --exclude-dir=target --exclude-dir=node_modules --exclude-dir=.git \
        "$words" "$root/$dir" 2>/dev/null || true)
    for f in $hits; do
        rel=${f#"$root"/}
        if ! printf '%s\n' "$rel" | grep -Eq "$allow"; then
            found="$found$rel
"
        fi
    done
done

if [ -n "$found" ]; then
    echo "error: ssh host-key options are named outside the allow-list:" >&2
    printf '%s' "$found" | while IFS= read -r f; do
        grep -nE "$words" "$root/$f" | sed "s|^|  $f:|" >&2
    done
    exit 1
fi
echo "no-ssh-weakening: ok"
