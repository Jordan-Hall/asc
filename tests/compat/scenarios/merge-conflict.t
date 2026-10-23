g init
printf 'a\nb\nc\n' > f
echo keep > k
g add .
at 1
g commit -m base
g switch -c side
printf 'a\nb\nside\n' > f
echo s > s.txt
g rm -q k
g add -A
at 2
g commit -m "side change"
g switch master
printf 'a\nb\nmain\n' > f
echo k2 >> k
at 3
g commit -am "main change"
g merge side
g status
g ls-files -s
cat f
g merge side
g merge --abort
g status
g merge side
g merge --abort
g merge --abort
g merge side
printf 'a\nb\nresolved\n' > f
g add f
g rm -q k
g commit -m "resolved merge"
g log --oneline --graph
g reset --hard HEAD~1
g merge -X theirs side
printf 'resolved\n' > k
g add k
g merge --continue
g log -n1 --format=%B
g reset --hard HEAD~1
g config merge.conflictStyle diff3
g merge side
cat f
g merge --quit
g reset --hard HEAD
g config --unset merge.conflictStyle
g merge --continue
g merge -X ours side
g status
