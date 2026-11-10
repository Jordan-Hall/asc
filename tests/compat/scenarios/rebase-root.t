# rebase --root: every commit from the root up, on git's squash-onto commit or with --onto.
g init
for i in 1 2 3; do
  echo "$i" > "f$i"
  g add "f$i"
  at "$i"
  g commit -m "c$i"
done
g tag orig
# Without -f the commits fast-forward over the squash-onto commit and nothing changes.
at 4
g rebase --root
g log --format="%h %p %s"
g rebase -f --root
g log --format="%h %p %s"
g reset --hard orig
# The todo list names the squash-onto commit as the new base.
export GIT_SEQUENCE_EDITOR="cat"
g rebase -i --root
export GIT_SEQUENCE_EDITOR="sed -i -e '1{h;d}' -e '2{G}'"
g rebase -i --root
g log --format="%s %p" 
g ls-files
g reset --hard orig
unset GIT_SEQUENCE_EDITOR
# --onto: the commits the new base lacks, with a branch argument.
g switch -c side master~2
echo s > s
g add s
at 5
g commit -m s
g rebase --root --onto side master
g log --oneline --graph --all
# A fixup on the squash-onto commit is put back on the list.
g switch master
g reset --hard orig
export GIT_SEQUENCE_EDITOR="printf 'label x\nfixup orig~1\n' >"
g rebase -i --root
g status
g rebase --abort
# A merge into [new root] fast-forwards to the merged commit.
export GIT_SEQUENCE_EDITOR="printf 'reset [new root]\nmerge -C orig side\npick orig\n' >"
g rebase -i --root
g log --format="%s %p"
unset GIT_SEQUENCE_EDITOR
# Refusals.
g rebase --root master side
g rebase --keep-base --root
g rebase --root nope
g rebase --root --onto nope
# --rebase-merges: the root branch starts from [new root], or from onto with --onto.
g switch -c merged orig~2
echo b > b
g add b
at 6
g commit -m b
at 7
g merge -q --no-edit side
export GIT_SEQUENCE_EDITOR="cat"
g rebase -i -r --root
g log --oneline --graph
g rebase -i -r --root --onto merged~1
g log --oneline --graph
g rebase -f -r --root
g log --oneline --graph
