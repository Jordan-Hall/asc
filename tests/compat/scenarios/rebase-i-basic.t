# rebase -i: the todo list through the editor: untouched, reordered, dropped, with --exec, noop.
g init
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
# The editor falls back to GIT_EDITOR (true): the list runs as generated.
g rebase -i master
g log --oneline
g reset --hard orig
# Swap the first two commits and drop the last.
export GIT_SEQUENCE_EDITOR="sed -i -e '1{h;d}' -e '2{G}' -e '4s/^pick/drop/'"
g rebase -i master
g log --oneline
g reset --hard orig
# Short names and the long ones are the same commands.
export GIT_SEQUENCE_EDITOR="sed -i -e '1s/^pick/p/' -e '2s/^pick/d/' -e '3s/^pick/p/'"
g rebase -i master
g log --oneline
g reset --hard orig
# --exec lines are in the list the editor sees.
export GIT_SEQUENCE_EDITOR="sed -i -e '/^exec/d'"
g rebase -i --exec "echo hi" master
g log --oneline
g reset --hard orig
unset GIT_SEQUENCE_EDITOR
g rebase -i --exec "echo ran-$(git rev-parse --short HEAD >/dev/null 2>&1; echo x)" master
g log --oneline
g reset --hard orig
# Nothing to replay is a noop; so is an already rebased branch.
g rebase -i side
g rebase -i master
g rebase -i master
g log --oneline
# sequence.editor beats GIT_EDITOR; the todo file is the argument.
g reset --hard orig
g config sequence.editor "sed -i -e '1s/^pick/drop/'"
g rebase -i master
g log --oneline
# The options that need -i or a todo list.
g reset --hard orig
g -c sequence.editor=true rebase -i --no-keep-empty --keep-empty master
g log --oneline
