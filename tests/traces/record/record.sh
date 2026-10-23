#!/bin/sh
# Records one scenario into a .trace file.
#   ASC_TRACE_TOOLS=<dir holding node_modules with the pinned tools> \
#     tests/traces/record/record.sh <scenario.sh> <out.trace>
# See README.md. The fixture lives in a temporary directory that is removed at the end
# (ASC_TRACE_KEEP=1 keeps it). Real git runs under GIT_TRACE2_EVENT; no wrapper is needed, so the
# tools can spawn git by any means (cross-spawn, execa, Rust).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
: "${ASC_TRACE_TOOLS:?set ASC_TRACE_TOOLS to a directory whose node_modules holds the pinned tools}"
scenario=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
out=$2
case "$out" in /*|?:*) ;; *) out=$(pwd)/$out ;; esac
tmp=$(mktemp -d)
trap '[ -n "${ASC_TRACE_KEEP:-}" ] || rm -rf "$tmp"' EXIT
mkdir -p "$tmp/home/.config" "$tmp/home/AppData/Roaming" "$tmp/home/AppData/Local" "$tmp/work" "$tmp/acts"
cp "$here/../../compat/home/.gitconfig" "$tmp/home/.gitconfig"

# The same fixed environment as the compat runner (tests/compat/src/lib.rs `fixed_env`), so the
# object ids of a recording equal those of a replay.
tmpw=$(cd "$tmp" && { pwd -W 2>/dev/null || pwd; })
export HOME="$tmpw/home" USERPROFILE="$tmpw/home" XDG_CONFIG_HOME="$tmpw/home/.config"
export GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 GIT_EDITOR=true GIT_PAGER=cat PAGER=cat
export GIT_AUTHOR_NAME="A U Thor" GIT_AUTHOR_EMAIL=author@example.com
export GIT_COMMITTER_NAME="C O Mitter" GIT_COMMITTER_EMAIL=committer@example.com
export GIT_AUTHOR_DATE="1700000000 +0000" GIT_COMMITTER_DATE="1700000000 +0000"
export TZ=UTC LC_ALL=C NO_COLOR=1
export PATH="$here/bin:$ASC_TRACE_TOOLS/node_modules/.bin:$PATH"
export CI=1 HUSKY= npm_config_update_notifier=false TURBO_TELEMETRY_DISABLED=1 DO_NOT_TRACK=1
export ASC_TRACE_REC="$here" ASC_TRACE_ACTS="$tmp/acts" ASC_TRACE_TOOLS
export ASC_TRACE_SH=$(command -v sh)
export GIT_TRACE2_EVENT="$tmp/trace2.jsonl"
export GIT_TRACE2_ENV_VARS=GIT_INDEX_FILE,GIT_DIR,GIT_WORK_TREE,GIT_PREFIX,GIT_EDITOR,GIT_REFLOG_ACTION,GIT_EXEC_PATH,GIT_NAMESPACE
: > "$GIT_TRACE2_EVENT"
cd "$tmp/work"
export ROOT=$(pwd -W 2>/dev/null || pwd)
. "$here/prelude.sh"
. "$scenario"
node "$here/to-trace.mjs" --trace2 "$GIT_TRACE2_EVENT" --acts "$tmp/acts" --root "$ROOT" \
  --scenario "$scenario" --out "$out"
