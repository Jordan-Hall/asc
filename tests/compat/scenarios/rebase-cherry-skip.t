# rebase leaves out a commit whose patch upstream already holds: the warning and the hint, -q,
# advice.skippedCherryPicks, --reapply-cherry-picks (the commit replays and drops as empty),
# --keep-base (reapplies by default) with and without --no-reapply-cherry-picks, --onto,
# --rebase-merges and --root.
g init
echo a > a
g add a
at 1
g commit -q -m base
g switch -q -c topic
echo x > x
g add x
at 2
g commit -q -m X
echo y > y
g add y
at 3
g commit -q -m Y
g switch -q master
echo m > m
g add m
at 4
g commit -q -m M
at 5
g cherry-pick topic~1
g switch -q topic
g tag orig
export GIT_EDITOR=true
export GIT_SEQUENCE_EDITOR="sed -n -e '/^[a-z]/p' -e 'w /dev/stderr'"
g rebase master
g log --oneline --graph
g reset -q --hard orig
g rebase -i master
g reset -q --hard orig
g rebase -q master
g reset -q --hard orig
g -c advice.skippedCherryPicks=false rebase master
g reset -q --hard orig
g rebase --reapply-cherry-picks master
g log --oneline --graph
g reset -q --hard orig
g rebase -i --keep-base master
g reset -q --hard orig
g rebase -i --keep-base --no-reapply-cherry-picks master
g reset -q --hard orig
g rebase -i --onto master~1 master
g reset -q --hard orig
g rebase -i -r master
g reset -q --hard orig
g rebase -i --root
g reset -q --hard orig
g rebase -i master~2
g log --oneline --graph
