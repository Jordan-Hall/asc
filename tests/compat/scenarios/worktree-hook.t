g init
hooks="$PWD/.hooks"
mkdir -p "$hooks"
g config core.hooksPath "$(wpwd)/.hooks"
printf '#!/bin/sh\necho "hook post-checkout: $1 $2 $3" >&2\n' > "$hooks/post-checkout"
chmod +x "$hooks/post-checkout"
echo one > a.txt
g add a.txt
at 1
g commit -m first
g worktree add ../hooked -b hooked
g worktree add --detach ../hooked2
printf '#!/bin/sh\necho "hook post-checkout fails" >&2\nexit 3\n' > "$hooks/post-checkout"
g worktree add ../hooked3
g worktree list
