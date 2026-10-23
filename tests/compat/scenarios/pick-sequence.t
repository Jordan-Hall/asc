g init
echo base > f
g add .
at 1
g commit -m base
g tag base
g switch -c side
echo 1 > m1
g add m1
at 2
g commit -m m1
echo 2 > m2
g add m2
at 3
g commit -m m2
echo 3 > m3
g add m3
at 4
g commit -m m3
g switch master
echo conflict > m2
g add m2
at 5
g commit -m "main m2"
g cherry-pick side~2 side~1 side
g status
gd="$(git rev-parse --git-dir)"
ls "$gd/sequencer"
cat "$gd/sequencer/todo"
cat "$gd/sequencer/head"
cat "$gd/sequencer/abort-safety"
echo resolved > m2
g add m2
at 6
g cherry-pick --continue
g log --oneline
g reset --hard base
echo conflict > m2
g add m2
at 7
g commit -m "main m2 again"
g cherry-pick side~2..side
g cherry-pick --skip
g log --oneline
g status
g reset --hard base
echo conflict > m2
g add m2
at 8
g commit -m "main m2 third"
g cherry-pick -x side~2 side~1
g cherry-pick --abort
g log --oneline
g cherry-pick side~1
g cherry-pick side
