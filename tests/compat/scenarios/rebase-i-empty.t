# rebase -i: a commit that turns empty stops the rebase (--empty=stop), is dropped
# (--empty=drop) or kept (--empty=keep); commits that start empty are kept.
g init
gd="$(git rev-parse --git-dir)"
echo base > f
g add .
at 1
g commit -m base
g switch -c side
echo 1 > m1
g add m1
at 2
g commit -m m1
at 3
g commit --allow-empty -m "empty one"
echo 2 > m2
g add m2
at 4
g commit -m m2
g switch master
echo 1 > m1
echo e > extra
g add .
at 5
g commit -m "main m1 and extra"
g switch side
g tag orig
export GIT_EDITOR=true
unset GIT_SEQUENCE_EDITOR
g rebase -i master
cat "$gd/rebase-merge/done"
for n in CHERRY_PICK_HEAD REBASE_HEAD MERGE_MSG AUTO_MERGE; do test -e "$gd/$n" && echo "has $n"; done
cat "$gd/CHERRY_PICK_HEAD" "$gd/REBASE_HEAD" "$gd/MERGE_MSG"
g rebase --skip
g log --oneline
g reset --hard orig
g rebase -i --empty=drop master
g log --oneline
cat "$gd/rebase-merge/drop_redundant_commits" 2>/dev/null
g reset --hard orig
g rebase -i --empty=keep master
g log --oneline
g reset --hard orig
g rebase -i --no-keep-empty --empty=drop master
g log --oneline
g reset --hard orig
g rebase --empty=keep master
g log --oneline
