g init
printf 'a\nb\nc\n' > f
g add .
at 1
g commit -m base
g tag base
g switch -c side
printf 'a\nb\nside\n' > f
at 2
g commit -am "side edit"
printf 'a\nb\nside2\n' > f
at 3
g commit -am "side edit 2"
g switch master
printf 'a\nb\nmain\n' > f
at 4
g commit -am "main edit"
g cherry-pick -X theirs side~1
cat f
g log -n1 --format=%s
g reset --hard base
printf 'a\nb\nmain\n' > f
at 5
g commit -am "main edit"
g config merge.conflictStyle zdiff3
g cherry-pick side
cat f
g cherry-pick --abort
g config --unset merge.conflictStyle
g revert -X ours side
cat f
g revert --abort
g cherry-pick side~1..side
g cherry-pick --abort
g revert side~1 side
g status
g revert --abort
g status
g cherry-pick -n side~1
g status
g reset --hard
g status
