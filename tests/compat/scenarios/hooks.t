g init
hooks="$PWD/.hooks"
mkdir -p "$hooks"
g config core.hooksPath "$(wpwd)/.hooks"
for h in pre-commit prepare-commit-msg commit-msg post-commit; do
  printf '#!/bin/sh\necho "hook %s: args=$# $*" >&2\n' "$h" > "$hooks/$h"
  chmod +x "$hooks/$h"
done
printf '#!/bin/sh\necho "hook commit-msg sees: $(head -n 1 "$1")" >&2\nexit 0\n' > "$hooks/commit-msg"
printf '#!/bin/sh\necho "hook pre-commit index: $GIT_INDEX_FILE" >&2\necho "hook pre-commit editor: $GIT_EDITOR" >&2\n' > "$hooks/pre-commit"
echo one > a.txt
g add a.txt
at 1
g commit -m first
echo two >> a.txt
printf '#!/bin/sh\necho "pre-commit rejects" >&2\nexit 1\n' > "$hooks/pre-commit"
at 2
g commit -a -m rejected
g log --oneline
g commit -a -m skipped --no-verify
g log --oneline
printf '#!/bin/sh\necho "post-checkout: $1 $2 $3" >&2\n' > "$hooks/post-checkout"
chmod +x "$hooks/post-checkout"
g branch other
g switch other
g switch master
g checkout -b third
