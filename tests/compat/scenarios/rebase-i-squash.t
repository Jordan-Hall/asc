# rebase -i: squash and fixup chains, fixup -C, --autosquash, the combined message.
g init
gd="$(git rev-parse --git-dir)"
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
# The editor of the final squash step: keep the message, drop git's comments.
export GIT_EDITOR="sed -i -e '/^#/d'"
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^pick/squash/' -e '3s/^pick/fixup/'"
g rebase -i master
g log --format='%h %s%n%b---'
cat "$gd/COMMIT_EDITMSG"
g reset --hard orig
# Only fixups: no editor, the first message stays.
export GIT_EDITOR=false
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^pick/fixup/' -e '3s/^pick/fixup/'"
g rebase -i master
g log --format='%h %s%n%b---'
g reset --hard orig
# A fixup -C takes the message of the folded commit.
export GIT_EDITOR=false
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^pick/fixup -C/'"
g rebase -i master
g log --format='%h %s%n%b---'
g reset --hard orig
# Two chains, an edit in the middle of the list.
export GIT_EDITOR="sed -i -e 's/^m1/combined one/' -e '/^#/d'"
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^pick/squash/' -e '4s/^pick/squash/'"
g rebase -i master
g log --format='%h %s%n%b---'
g reset --hard orig
# An empty message aborts the commit and stops the rebase; --continue finishes it.
export GIT_EDITOR="sed -i -e '/./d'"
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^pick/squash/'"
g rebase -i master
ls "$gd/rebase-merge" 2>/dev/null || echo "no rebase-merge"
cat "$gd/rebase-merge/current-fixups" "$gd/rebase-merge/message-squash"
export GIT_EDITOR="sed -i -e '/^#/d'"
g rebase --continue
g log --format='%h %s%n%b---'
# --autosquash moves the fixup! and squash! commits.
g reset --hard orig
echo fix > m1
g add m1
at 9
g commit --fixup HEAD~3
echo sq > m2
g add m2
at 10
g commit --squash HEAD~3
unset GIT_SEQUENCE_EDITOR
export GIT_EDITOR="sed -i -e '/^#/d'"
g rebase -i --autosquash master
g log --format='%h %s%n%b---'
g show --stat HEAD~3
