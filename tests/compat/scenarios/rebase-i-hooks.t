# rebase -i: hooks (pre-rebase, prepare-commit-msg, commit-msg, post-commit, post-rewrite).
g init
gd="$(git rev-parse --git-dir)"
hooks="$PWD/.hooks"
mkdir -p "$hooks"
g config core.hooksPath "$(wpwd)/.hooks"
printf '#!/bin/sh\necho "hook pre-rebase: $*" >&2\n' > "$hooks/pre-rebase"
printf '#!/bin/sh\necho "hook post-rewrite: $1" >&2\ncat >&2\n' > "$hooks/post-rewrite"
printf '#!/bin/sh\necho "hook post-commit" >&2\n' > "$hooks/post-commit"
printf '#!/bin/sh\necho "hook prepare-commit-msg: $(basename "$1") $2" >&2\n' > "$hooks/prepare-commit-msg"
printf '#!/bin/sh\necho "hook commit-msg: $(basename "$1")" >&2\n' > "$hooks/commit-msg"
printf '#!/bin/sh\necho "hook pre-commit" >&2\n' > "$hooks/pre-commit"
chmod +x "$hooks/pre-rebase" "$hooks/post-rewrite" "$hooks/post-commit" "$hooks/prepare-commit-msg" "$hooks/commit-msg" "$hooks/pre-commit"
echo base > f
g add .
at 1
g commit -m base
g switch -c side
for i in 1 2 3 4; do
  echo "$i" > "m$i"
  g add "m$i"
  at "$((i + 1))"
  g commit -m "m$i"
done
g switch master
echo x > x
g add x
at 7
g commit -m "main x"
g switch side
g tag orig
export GIT_EDITOR=true
unset GIT_SEQUENCE_EDITOR
g rebase -i master
g reset --hard orig
# squash and fixup: one post-rewrite pair per folded commit, all mapped to the result.
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^pick/squash/' -e '3s/^pick/fixup/'"
g rebase -i master
g log --oneline
g reset --hard orig
# reword runs commit-msg; the squash chain does not.
export GIT_SEQUENCE_EDITOR="sed -i -e '1s/^pick/reword/'"
g rebase -i master
g reset --hard orig
g rebase -i --no-verify master
g log --oneline
# edit then amend: post-rewrite sees the edited commit.
g reset --hard orig
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^pick/edit/'"
g rebase -i master
echo more >> m2
g add m2
at 9
g commit --amend --no-edit
g rebase --continue
g log --oneline
