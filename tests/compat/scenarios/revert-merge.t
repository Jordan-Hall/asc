g init
echo base > f
g add .
at 1
g commit -m base
g switch -c side
echo side > s.txt
g add s.txt
at 2
g commit -m "side file"
g switch master
echo main > m.txt
g add m.txt
at 3
g commit -m "main file"
g merge --no-ff -m "merge side" side
g revert HEAD
g revert -m 1 HEAD
g log --oneline
g log -n1 --format=%B
g revert -m 3 HEAD~1
g cherry-pick -m 1 HEAD~2
g cherry-pick HEAD~2
g cherry-pick -m 1 side
