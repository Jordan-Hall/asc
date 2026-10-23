# rebase -i --rebase-merges: label, reset and merge recreate the topology.
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
echo 2 > m2
g add m2
at 3
g commit -m m2
g switch -c topic side~1
echo t > t
g add t
at 4
g commit -m "topic work"
g switch side
at 5
g merge --no-ff -m "Merge branch 'topic' into side" topic
echo z > z
g add z
at 6
g commit -m "after merge"
g switch master
echo x > x
g add x
at 7
g commit -m "main x"
g switch side
g tag orig
export GIT_EDITOR=true
unset GIT_SEQUENCE_EDITOR
g rebase -i --rebase-merges master
g log --oneline --graph
g for-each-ref
cat "$gd/rebase-merge/git-rebase-todo.backup" 2>/dev/null
# Linear history with -r.
g reset --hard orig
g rebase -i -r master~0 side~3
g log --oneline --graph
# A stop in the middle shows the labels.
g reset --hard orig
export GIT_SEQUENCE_EDITOR="sed -i -e '/pick.*m2/s/^pick/edit/'"
g rebase -i -r master
cat "$gd/rebase-merge/done" "$gd/rebase-merge/git-rebase-todo" "$gd/rebase-merge/refs-to-delete"
g for-each-ref
g rebase --continue
g log --oneline --graph
g for-each-ref
g reset --hard orig
export GIT_SEQUENCE_EDITOR="sed -i -e '/pick.*m2/s/^pick/edit/'"
g rebase -i -r master
g rebase --abort
g for-each-ref
