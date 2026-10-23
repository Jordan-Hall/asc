# rebase -i: reword opens the message editor; edit and the editor's failure modes.
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
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^pick/reword/'"
export GIT_EDITOR="sed -i -e '1s/.*/m2 reworded/' -e '/^#/d'"
g rebase -i master
g log --format='%h %an %s'
cat "$gd/COMMIT_EDITMSG"
# Reword the first commit, which can be fast-forwarded when the base is its parent.
g reset --hard orig
export GIT_SEQUENCE_EDITOR="sed -i -e '1s/^pick/reword/'"
g rebase -i master~1
g log --format='%h %s'
# An editor that fails stops the rebase with the message unchanged.
g reset --hard orig
export GIT_EDITOR=false
export GIT_SEQUENCE_EDITOR="sed -i -e '3s/^pick/reword/'"
g rebase -i master
ls "$gd/rebase-merge" 2>/dev/null || echo "no rebase-merge"
cat "$gd/rebase-merge/done"
g rebase --continue
g log --format='%h %s'
