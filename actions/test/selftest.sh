#!/usr/bin/env bash
# Self test of the ASC Actions (SPEC 12.7). One command: bash actions/test/selftest.sh
#
# Default (offline, starts nothing): static contract checks of every action.yml, a bash syntax
# check and injection-hardening test of every embedded script, the workflow patch applies
# cleanly to a scratch copy, and the runner units are well formed.
#
# --live: additionally runs the actions' commands against an asc serve that YOU started
#   (ASC_SERVER_URL, ASC_TOKEN, ASC_BIN) inside a container with a read-only host cache mount,
#   and asserts the synthetic .git, TURBO_SCM_BASE and outputs. This script never starts a server.
# --mirror <url>: also checks the asc/actions mirror serves every action.yml over https.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
actions="$(cd "$here/.." && pwd)"
repo="$(cd "$actions/.." && pwd)"
live=0
mirror=""
while [ $# -gt 0 ]; do
  case "$1" in
    --live) live=1 ;;
    --mirror) mirror="${2:?--mirror needs a url}"; shift ;;
    *) echo "usage: selftest.sh [--live] [--mirror <url>]" >&2; exit 2 ;;
  esac
  shift
done

py=""
for c in python3 python; do
  if command -v "$c" >/dev/null 2>&1 && "$c" -I -c 'import yaml' 2>/dev/null; then py="$c"; break; fi
done
[ -n "$py" ] || { echo "selftest: needs python with PyYAML" >&2; exit 1; }

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
pass=0
ok() { pass=$((pass + 1)); echo "ok  $1"; }

# 1. Contract checks.
"$py" -I "$here/check_actions.py" "$actions" >/dev/null
ok "action contracts"

# 2. Extract each run body, syntax check it, and assert hostile input stays data.
"$py" -I - "$actions" "$scratch" <<'PY'
import pathlib, sys, yaml
root, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
n = 0
for f in sorted(root.glob("**/action.yml")):
    doc = yaml.safe_load(f.read_text(encoding="utf-8"))
    for i, s in enumerate(doc["runs"]["steps"]):
        if "run" in s:
            name = f.parent.relative_to(root).as_posix().replace("/", "_")
            (out / f"{name}.{i}.sh").write_text(s["run"], encoding="utf-8")
            n += 1
print(n)
PY
for s in "$scratch"/*.sh; do bash -n "$s"; done
ok "embedded scripts parse"

# turbo-cache: exports the documented variables, masks the token, PR events are read-only remote.
run_turbo() {
  local event="$1" server="$2" repo_in="$3" env_file="$scratch/genv.$RANDOM"
  : >"$env_file"
  IN_SERVER="$server" IN_REPO="$repo_in" IN_TOKEN="tok123" IN_EVENT="$event" GITHUB_ENV="$env_file" \
    bash "$scratch/asc_turbo-cache.0.sh" >"$scratch/out" 2>&1 || return 1
  cat "$env_file"
}
env_out="$(run_turbo push https://asc.example.com asc/dds)"
grep -qx 'TURBO_API=https://asc.example.com' <<<"$env_out"
grep -qx 'TURBO_TEAM=asc.dds' <<<"$env_out"
grep -qx 'TURBO_TOKEN=tok123' <<<"$env_out"
grep -qx 'TURBO_CACHE=local:rw,remote:rw' <<<"$env_out"
grep -q '^::add-mask::tok123' "$scratch/out"
env_out="$(run_turbo pull_request https://asc.example.com asc/dds)"
grep -qx 'TURBO_CACHE=local:rw,remote:r' <<<"$env_out"
if run_turbo push http://evil.example.com asc/dds; then echo "plain http accepted" >&2; exit 1; fi
if run_turbo push https://asc.example.com 'no-slash'; then echo "bad repo accepted" >&2; exit 1; fi
if run_turbo push https://asc.example.com $'asc/dds\nTURBO_API=x'; then echo "newline accepted" >&2; exit 1; fi
ok "asc/turbo-cache exports and rejects hostile input"

# setup: refuses non-https, bad digests and bad versions before touching the network.
run_setup() {
  IN_VERSION="$1" IN_URL="$2" IN_SHA="$3" RUNNER_TEMP="$scratch" RUNNER_TOOL_CACHE="$scratch/tc" \
    GITHUB_PATH="$scratch/gpath" bash "$scratch/asc_setup.0.sh" >/dev/null 2>&1
}
sha64="$(printf 'a%.0s' $(seq 64))"
if run_setup 1.0.0 http://x/asc.tgz "$sha64"; then echo "http accepted" >&2; exit 1; fi
if run_setup 1.0.0 https://x/asc.tgz abc; then echo "short sha accepted" >&2; exit 1; fi
if run_setup '../x' https://x/asc.tgz "$sha64"; then echo "path version accepted" >&2; exit 1; fi
ok "asc/setup refuses unverified installs"

# checkout: refuses traversal, non owner/repo and plain http before any work.
run_checkout() {
  IN_REPO="$1" IN_REF="${2:-abc}" IN_BASE="" IN_PATH="${3:-.}" IN_SERVER="${4:-https://asc.example.com}" \
    IN_MODE=plain IN_SPARSE="" ASC_TOKEN=t GITHUB_OUTPUT="$scratch/gout" bash "$scratch/checkout.0.sh" >/dev/null 2>&1
}
if run_checkout noslash; then echo "bad repo accepted" >&2; exit 1; fi
if run_checkout asc/dds abc ../escape; then echo "traversal accepted" >&2; exit 1; fi
if run_checkout asc/dds abc . http://evil.example.com; then echo "plain http accepted" >&2; exit 1; fi
ok "checkout refuses hostile input"

# 3. The workflow patch is well formed (applies to the files it names, if monorepo is present).
patch_file="$repo/docs/ci/asc-workflows.patch"
[ -s "$patch_file" ]
grep -q '^+.*asc/affected\|^+.*asc ci affected' "$patch_file"
if grep -q '^+.*fetch-depth: 0\|^+.*submodules: recursive' "$patch_file"; then
  echo "patch adds fetch-depth: 0 or submodules: recursive" >&2; exit 1
fi
asc="${ASC_DIR:-/d/dev/monorepo}"
if [ -d "$asc/.forgejo/workflows" ] && command -v git >/dev/null 2>&1; then
  mkdir -p "$scratch/asc/.forgejo/workflows"
  cp "$asc"/.forgejo/workflows/*.yml "$scratch/asc/.forgejo/workflows/"
  # patch(1) semantics through git apply outside any repository.
  (cd "$scratch/asc" && git apply --check "$patch_file")
  ok "workflow patch applies"
else
  ok "workflow patch present (monorepo not found, apply check skipped)"
fi

# 4. Runner units.
for f in asc-warm.service asc-warm.timer runner-config-snippet.yaml; do [ -s "$repo/deploy/runner/$f" ]; done
grep -q 'NoNewPrivileges=true' "$repo/deploy/runner/asc-warm.service"
grep -q ':ro' "$repo/deploy/runner/runner-config-snippet.yaml"
"$py" -I -c 'import sys,yaml; yaml.safe_load(open(sys.argv[1]))' "$repo/deploy/runner/runner-config-snippet.yaml"
ok "runner units"

# 5. Optional mirror check.
if [ -n "$mirror" ]; then
  case "$mirror" in https://*) ;; *) echo "mirror must be https" >&2; exit 1 ;; esac
  for a in checkout cache upload-artifact download-artifact asc/setup asc/changed asc/affected asc/turbo-cache; do
    curl -fsS --proto '=https' --max-time 20 -o /dev/null "${mirror%/}/$a/action.yml"
  done
  ok "mirror serves every action"
fi

# 6. Optional live run against a server the caller started.
if [ "$live" -eq 1 ]; then
  : "${ASC_SERVER_URL:?--live needs ASC_SERVER_URL}" "${ASC_TOKEN:?--live needs ASC_TOKEN}" "${ASC_BIN:?--live needs ASC_BIN}"
  command -v docker >/dev/null 2>&1 || { echo "--live needs docker" >&2; exit 1; }
  host_cache="$scratch/hostcache"; mkdir -p "$host_cache"
  out="$scratch/live.out"
  docker run --rm --network host -v "$host_cache:/var/cache/asc/cache:ro" \
    -v "$ASC_BIN:/usr/local/bin/asc:ro" -e ASC_SERVER_URL -e ASC_TOKEN \
    -e GITHUB_ENV=/tmp/genv -e GITHUB_PATH=/tmp/gpath -e RUNNER_TEMP=/tmp/rt \
    "${ASC_SELFTEST_IMAGE:-debian:stable-slim}" bash -c '
      set -euo pipefail
      mkdir -p /tmp/rt /w && cd /w
      asc ci checkout --repo "${ASC_REPO:-asc/dds}" --rev master --dir . --server "$ASC_SERVER_URL" \
        --host-cache /var/cache/asc/cache --runner-temp /tmp/rt
      test -e .git
      grep -q "^TURBO_SCM_BASE=" /tmp/genv
      grep -q "^TURBO_API=" /tmp/genv
    ' >"$out" 2>&1 || { cat "$out" >&2; exit 1; }
  ok "live checkout (synthetic .git, TURBO_SCM_BASE, TURBO_API)"
fi

echo "selftest: $pass checks passed"
