g init
echo one > a.txt
g add a.txt
at 1
g commit -m first
g config remote.origin.url ../origin
g config remote.origin.fetch +refs/heads/*:refs/remotes/origin/*
g update-ref refs/remotes/origin/feat HEAD
g worktree add ../w1 -b t1 origin/feat
g worktree add --no-track ../w2 -b t2 origin/feat
g worktree add --track ../w3 -b t3 master
g worktree add --track ../w4 -b t4 origin/feat
g worktree add -q ../w5 -b t5 origin/feat
g worktree add --no-track ../w6 -b t6
g config --get-regexp ^branch\.
g branch -vv
