# cherry-pick, rebase and merge on a branch with no commit yet (an unborn HEAD) do what git does.
g init
echo one > f.txt
g add f.txt
at 1
g commit -q -m c1
echo two > g.txt
g add g.txt
at 2
g commit -q -m c2
g checkout -q --orphan fresh
g rm -rq --cached .
rm -f f.txt g.txt
at 3
g cherry-pick master
g log --format='%s %P' fresh
g ls-files
g checkout -q --orphan fresh2
g rm -rq --cached .
rm -f f.txt g.txt
at 4
g cherry-pick master~1 master
g log --format=%s fresh2
g checkout -q --orphan fresh3
g rm -rq --cached .
rm -f f.txt g.txt
g rebase master
g status --short --branch
g checkout -q --orphan fresh4
g rm -rq --cached .
rm -f f.txt g.txt
at 5
g merge -q master
g log --format=%s
g checkout -q --orphan fresh5
g rm -rq --cached .
rm -f f.txt g.txt
g rebase --root
g cherry-pick --abort
g checkout -q master
g checkout -q -b clash
echo clash > g.txt
g add g.txt
at 6
g commit -q -m clash
g cherry-pick master
g checkout -f master
g status --short --branch
g checkout -q clash
g cherry-pick master~1 master
g checkout -f master
g status --short --branch
g cherry-pick --continue
