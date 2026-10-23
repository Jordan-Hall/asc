g init
echo base > f
echo k > k
g add .
at 1
g commit -m base
g branch old
g switch -c side
echo 1 > m1
g add m1
at 2
g commit -m m1
echo 2 > m2
g add m2
at 3
g commit -m m2
g switch master
echo x > x
g add x
at 4
g commit -m "main x"
echo y > y
g add y
at 5
g commit -m "main y"
g switch side
g rebase master
g log --oneline --graph
g rebase master
g rebase -f master
g log --oneline
g switch old
g rebase master
g log --oneline
g switch side
g rebase --onto old master
g log --oneline
g rebase --onto master old
g log --oneline
g rebase --exec "echo hi" old
g log --oneline
g rebase nope
g rebase master side
g rebase master old
g switch --detach side
g rebase master
g log --oneline --all --graph
