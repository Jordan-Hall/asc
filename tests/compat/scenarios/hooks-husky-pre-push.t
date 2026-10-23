# compat: asc-only
# Remotes and push land with the remote units; here the pre-push hook is reached through the husky
# dispatcher the way git runs it (argv: remote name and url, refs on stdin), with the hooks path set.
set -e
git init
mkdir -p .husky/_
printf '#!/bin/sh\nn=$(basename "$0")\ns=$(dirname "$(dirname "$0")")/$n\n[ -f "$s" ] || exit 0\n. "$s"\n' > .husky/_/h
printf '#!/bin/sh\n. "$(dirname "$0")/h"\n' > .husky/_/pre-push
chmod +x .husky/_/h .husky/_/pre-push
printf '#!/bin/sh\necho "pre-push $1 $2"\nwhile read l; do echo "ref $l"; done\ngit rev-parse --git-dir >/dev/null\nexit 0\n' > .husky/pre-push
git config core.hooksPath .husky/_
test "$(git config core.hooksPath)" = ".husky/_"
out=$(echo "refs/heads/master a refs/heads/master b" | sh .husky/_/pre-push origin url)
echo "$out" | grep -q "pre-push origin url"
echo "$out" | grep -q "ref refs/heads/master a"
printf '#!/bin/sh\nexit 3\n' > .husky/pre-push
rc=0; sh .husky/_/pre-push origin url </dev/null || rc=$?
test "$rc" = 3
