g init
echo one > a.txt
g add a.txt
at 1
g commit -m first
g branch other
g worktree add ../w1
g worktree add -b feat ../w2
g worktree add --detach ../w3
g worktree add ../w4 master
g worktree add ../w5 other
g worktree add ../w1
g worktree add -b feat ../w6
g worktree add -q ../w7 -b quiet
g worktree add ../w8 nosuchref
g worktree list
g worktree list -v
g worktree list --porcelain
g -C ../w2 branch --show-current
g -C ../w2 status
g -C ../w2 switch master
g -C ../w3 rev-parse --abbrev-ref HEAD
echo two > ../w2/b.txt
g -C ../w2 add b.txt
at 2
g -C ../w2 commit -m second
g -C ../w2 log --oneline
g log --oneline
g branch -v
g branch -d feat
g worktree add -B feat ../w9 other
g worktree list
