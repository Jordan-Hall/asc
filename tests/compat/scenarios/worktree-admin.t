g init
echo one > a.txt
g add a.txt
at 1
g commit -m first
g worktree add ../w1
g worktree add ../w2 -b second
g worktree add --lock --reason busy ../w3 -b third
g worktree list -v
g worktree lock ../w1
g worktree lock ../w1
g worktree unlock ../w2
g worktree unlock ../w1
g worktree lock ../work
g worktree unlock ../work
g worktree remove ../work
g worktree remove ../w3
g worktree remove ../nosuch
echo dirty > ../w1/x.txt
g worktree remove ../w1
echo changed >> ../w1/a.txt
rm ../w1/x.txt
g worktree remove ../w1
g worktree remove --force ../w1
g worktree list
g worktree move ../w2 ../w2b
g worktree move ../w3 ../w3b
g worktree move ../w3 ../w3b -f
g worktree move ../w3 ../w3b -f -f
g worktree list
g -C ../w2b status
g worktree prune -v
g worktree add ../w4 -b fourth
rm -rf ../w4
g worktree prune -n -v
g worktree list
g worktree prune -v
g worktree list
