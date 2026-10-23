g init
printf 'a\nb\nc\n' > f
echo keep > k
g add .
at 1
g commit -m base
g tag base
g switch -c side
echo one > m1
g add m1
at 2
g commit -m "m1 subject

body of m1"
echo two > m2
g add m2
at 3
g commit -m "m2"
printf 'a\nb\nside\n' > f
g rm -q k
g add -A
at 4
g commit -m "side change"
g switch master
g cherry-pick side~2
g log -n1 --format='%an %ad %cn %B'
g cherry-pick -x side~1
g log -n1 --format=%B
g reset --hard base
g cherry-pick -n side~1
g status
cat "$(git rev-parse --git-dir)/MERGE_MSG"
g commit -m "from -n"
g log --oneline
g reset --hard base
printf 'a\nb\nmain\n' > f
echo k2 >> k
at 5
g commit -am "main change"
g cherry-pick side
g status
g ls-files -s
cat f
g cherry-pick side
g cherry-pick --abort
g status
g cherry-pick side
printf 'a\nb\nresolved\n' > f
g add f
g rm -q k
at 6
g cherry-pick --continue
g log --format='%h %an %s'
g status
g cherry-pick nosuch
g cherry-pick
g cherry-pick --continue
