# rebase -i: todo lists that do not parse, an empty list, wrong options.
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
export GIT_EDITOR=true
# An unknown command.
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^pick/bogus/'"
g rebase -i master
ls "$gd/rebase-merge" 2>/dev/null || echo "no rebase-merge"
g log --oneline
g rebase --continue
# Fix it with --edit-todo.
export GIT_SEQUENCE_EDITOR="sed -i -e 's/^bogus/pick/'"
g rebase --edit-todo
g rebase --continue
g log --oneline
# A fixup with nothing before it, a commit that does not exist, a missing argument.
g reset --hard orig
export GIT_SEQUENCE_EDITOR="sed -i -e '1s/^pick/fixup/'"
g rebase -i master
g rebase --abort
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/ [0-9a-f]* #/ zzzzzzz #/'"
g rebase -i master
g rebase --abort
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^pick.*/pick/' -e '3s/^pick.*/exec/'"
g rebase -i master
g rebase --abort
# An empty list is nothing to do and leaves nothing behind.
export GIT_SEQUENCE_EDITOR="sed -i -e '/^pick/d'"
g rebase -i master
ls "$gd/rebase-merge" 2>/dev/null || echo "no rebase-merge"
g log --oneline
# The editor failing stops before anything moved.
export GIT_SEQUENCE_EDITOR=false
g rebase -i master
ls "$gd/rebase-merge" 2>/dev/null || echo "no rebase-merge"
g log --oneline
# break takes no argument, not even a comment.
export GIT_SEQUENCE_EDITOR="sed -i -e '2s/^/break # stop here\n/'"
g rebase -i master
g rebase --abort
# Comments and blank lines are fine.
export GIT_SEQUENCE_EDITOR="sed -i -e '1s/^/# a comment\n\n/' -e '2s/^/break\n/'"
g rebase -i master
cat "$gd/rebase-merge/done" "$gd/rebase-merge/git-rebase-todo"
g rebase --continue
g log --oneline
# Options that do not combine, and a rebase that is not running.
g rebase --continue
g rebase --edit-todo
g rebase --abort
g rebase --continue --abort
g rebase -i --empty=bogus master
