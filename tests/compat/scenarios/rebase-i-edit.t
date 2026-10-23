# rebase -i: stops at edit, break and a failing exec; the state files; --continue after an
# amend or with staged changes; --skip and --abort.
g init
gd="$(git rev-parse --git-dir)"
echo base > f
g add .
at 1
g commit -m base
g switch -c side
for i in 1 2 3; do
  echo "$i" > "m$i"
  g add "m$i"
  at "$((i + 1))"
  g commit -m "m$i"
done
g switch master
echo x > x
g add x
at 6
g commit -m "main x"
g switch side
g tag orig
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^pick/edit/'"
g rebase -i master
cat "$gd/rebase-merge/done" "$gd/rebase-merge/git-rebase-todo" "$gd/rebase-merge/stopped-sha" "$gd/rebase-merge/amend" "$gd/rebase-merge/message" "$gd/rebase-merge/msgnum" "$gd/rebase-merge/end"
cat "$gd/rebase-merge/author-script" "$gd/rebase-merge/patch" "$gd/rebase-merge/rewritten-list"
g log --oneline
# Amend the commit, then continue.
echo more >> m2
g add m2
at 8
g commit --amend --no-edit
g rebase --continue
g log --oneline
test -d "$gd/rebase-merge" && echo in-progress || echo finished
# Staged changes at an edit stop become the amended commit on --continue.
g reset --hard orig
g rebase -i master
echo staged >> m2
g add m2
g rebase --continue
g log --oneline
g show --stat HEAD~1
# break stops without a commit; staged changes there need a commit of their own.
g reset --hard orig
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^pick/break\npick/'"
g rebase -i master
cat "$gd/rebase-merge/done" "$gd/rebase-merge/git-rebase-todo"
echo extra > extra
g add extra
g rebase --continue
g commit -m extra
g rebase --continue
g log --oneline
# A failing exec stops the rebase.
g reset --hard orig
unset GIT_SEQUENCE_EDITOR
g rebase -i --exec "test ! -f m2" master
cat "$gd/rebase-merge/done" "$gd/rebase-merge/git-rebase-todo"
g rebase --continue
g log --oneline
g rebase --abort
g log --oneline
# --skip at an edit stop moves on; --abort returns.
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^pick/edit/'"
g rebase -i master
g rebase --skip
g log --oneline
g reset --hard orig
g rebase -i master
echo scratch > scratch
g add scratch
g rebase --abort
g log --oneline
g status --short
g rebase --continue
g rebase --edit-todo
