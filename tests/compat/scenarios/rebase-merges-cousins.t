# rebase --rebase-merges modes: a branch forked before the merge base resets to its own base by
# default and to `onto` with rebase-cousins; rebase.rebaseMerges switches and names the mode, a bare
# -r keeps the configured mode, --no-rebase-merges wins, and an unknown mode is fatal.
g init
echo old > old
g add old
at 1
g commit -q -m old
g switch -q -c side
echo s > s
g add s
at 2
g commit -q -m s1
g switch -q master
echo b > b
g add b
at 3
g commit -q -m base
echo m > m
g add m
at 4
g commit -q -m m1
g switch -q -c topic master~1
echo t > t
g add t
at 5
g commit -q -m t1
at 6
g merge -q --no-ff -m "Merge branch 'side' into topic" side
g tag orig
export GIT_EDITOR=true
export GIT_SEQUENCE_EDITOR="sed -n -e '/^[a-z]/p' -e 'w /dev/stderr'"
g rebase -i -r master
g log --oneline --graph
g reset -q --hard orig
g rebase -i --rebase-merges=rebase-cousins master
g log --oneline --graph
g reset -q --hard orig
g rebase -i --rebase-merges=no-rebase-cousins master
g reset -q --hard orig
g -c rebase.rebaseMerges=rebase-cousins rebase -i master
g reset -q --hard orig
g -c rebase.rebaseMerges=rebase-cousins rebase -i -r master
g reset -q --hard orig
g -c rebase.rebaseMerges=rebase-cousins rebase -i --rebase-merges=no-rebase-cousins master
g reset -q --hard orig
g -c rebase.rebaseMerges=true rebase -i master
g reset -q --hard orig
g -c rebase.rebaseMerges=rebase-cousins rebase -i --no-rebase-merges master
g log --oneline --graph
g reset -q --hard orig
g -c rebase.rebaseMerges=false rebase -i master
g reset -q --hard orig
g rebase -i --rebase-merges= master
g reset -q --hard orig
g rebase --rebase-merges=bogus master
g -c rebase.rebaseMerges=bogus rebase master
g log --oneline --graph
