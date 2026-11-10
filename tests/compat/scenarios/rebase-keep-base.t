g init
echo base > f
g add f
at 1
g commit -m base
g switch -c topic
echo 1 > t1
g add t1
at 2
g commit -m t1
echo 2 > t2
g add t2
at 3
g commit -m t2
g switch master
echo m > m1
g add m1
at 4
g commit -m m1
g switch topic
g rebase --keep-base master
at 6
g rebase --keep-base -f master
g log --oneline --graph --all
g rebase --keep-base --onto master master
g rebase --keep-base
g branch --set-upstream-to=master
g rebase --keep-base -f
g log --oneline
g switch --detach topic
at 6
g rebase --keep-base -f master
g log --oneline
g switch --orphan lone
echo l > l
g add l
at 5
g commit -m lone
g rebase --keep-base master
g switch master
g rebase --keep-base master master
g log --oneline --all
