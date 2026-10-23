g init
echo one > a.txt
echo two > b.txt
g add .
at 1
g commit -m base
g branch other
echo dirty >> a.txt
g reset --hard
cat a.txt
echo dirty >> a.txt
echo staged > c.txt
g add c.txt
g reset --hard HEAD
g status -s
echo dirty >> a.txt
g checkout -f master
cat a.txt
echo dirty >> a.txt
g switch --discard-changes other
g status -s
echo dirty >> a.txt
g checkout -- a.txt
cat a.txt
echo dirty >> a.txt
g restore a.txt
cat a.txt
echo scratch > scratch.txt
g clean -n
g clean -f
g status -s
echo three > a.txt
g add a.txt
at 2
g commit -m second
echo four > a.txt
g add a.txt
at 3
g commit --amend -m second-amended
g log --oneline
g switch -c side HEAD~1
echo side > s.txt
g add s.txt
at 4
g commit -m side
g rebase master
g log --oneline --graph
g stash list
