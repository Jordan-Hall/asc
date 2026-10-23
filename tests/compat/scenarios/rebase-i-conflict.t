# rebase -i: a conflicting pick stops the rebase; --continue, --skip and --abort; a conflicting
# squash; the stop files.
g init
gd="$(git rev-parse --git-dir)"
printf 'a\nb\nc\n' > f
g add .
at 1
g commit -m base
g switch -c side
printf 'a\nB\nc\n' > f
g add f
at 2
g commit -m "side f"
echo 2 > m2
g add m2
at 3
g commit -m m2
echo 3 > m3
g add m3
at 4
g commit -m m3
g switch master
printf 'a\nM\nc\n' > f
g add f
at 5
g commit -m "main f"
g switch side
g tag orig
export GIT_EDITOR=true
g rebase -i master
cat "$gd/rebase-merge/done" "$gd/rebase-merge/git-rebase-todo" "$gd/rebase-merge/stopped-sha" "$gd/rebase-merge/msgnum"
cat "$gd/rebase-merge/message" "$gd/rebase-merge/author-script" "$gd/REBASE_HEAD" "$gd/MERGE_MSG"
cat f
g status --short
printf 'a\nR\nc\n' > f
g add f
at 6
g rebase --continue
g log --oneline
cat f
# --skip drops the conflicting commit.
g reset --hard orig
g rebase -i master
g rebase --skip
g log --oneline
# --abort returns to the branch.
g reset --hard orig
g rebase -i master
g rebase --abort
g log --oneline
# The continue of an unresolved conflict is refused.
g rebase -i master
g rebase --continue
g rebase --abort
# A squash that conflicts: m2 first, then "side f" folded into it.
g reset --hard orig
export GIT_SEQUENCE_EDITOR='f() { sed -i -e "1{h;d}" -e "2{G}" "$1"; sed -i -e "2s/^pick/squash/" "$1"; }; f'
g rebase -i master
cat "$gd/rebase-merge/current-fixups"
cat "$gd/rebase-merge/message-squash"
printf 'a\nR\nc\n' > f
g add f
export GIT_EDITOR="sed -i -e '/^#/d'"
at 7
g rebase --continue
g log --format='%h %s%n%b---'
g show --stat HEAD~1
# --edit-todo changes the commands left while the rebase is stopped.
g reset --hard orig
export GIT_EDITOR=true
unset GIT_SEQUENCE_EDITOR
g rebase -i master
export GIT_SEQUENCE_EDITOR="sed -i -e '1s/^pick/drop/'"
g rebase --edit-todo
cat "$gd/rebase-merge/git-rebase-todo" "$gd/rebase-merge/end"
g rebase --skip
g log --oneline
