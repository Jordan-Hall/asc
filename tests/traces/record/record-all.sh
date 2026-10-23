#!/bin/sh
# Records every scenario under record/scenarios into tests/traces/<tool>/<case>.trace.
#   ASC_TRACE_TOOLS=<dir with node_modules> tests/traces/record/record-all.sh [<tool>/<case>...]
# ASC_TRACE_TOOLS is record/tools after `npm ci` there. With no arguments every scenario is recorded.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
traces=$(cd "$here/.." && pwd)
if [ "$#" -eq 0 ]; then
  set -- $(cd "$here/scenarios" && find . -name '*.sh' | sed 's|^\./||; s|\.sh$||' | sort)
fi
for name in "$@"; do
  mkdir -p "$traces/$(dirname "$name")"
  echo "recording $name"
  sh "$here/record.sh" "$here/scenarios/$name.sh" "$traces/$name.trace" > /dev/null 2>&1 || {
    echo "record.sh failed for $name" >&2
    exit 1
  }
done
