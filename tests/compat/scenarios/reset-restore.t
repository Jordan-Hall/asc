g init
echo one > a.txt
echo bee > b.txt
g add .
at 1
g commit -m one
echo two >> a.txt
g add a.txt
at 2
g commit -m two
echo three >> a.txt
echo cee > c.txt
g add a.txt c.txt
at 3
g commit -m three
g reset --soft HEAD~1
g status
g log --oneline
g diff --cached --stat
g commit -m three-again
g reset HEAD~1
g status
g log --oneline
g add -A
g commit -m three-mixed
g reset --hard HEAD~1
g status
g log --oneline
cat a.txt
echo scratch >> a.txt
g restore a.txt
cat a.txt
echo staged > a.txt
g add a.txt
g restore --staged a.txt
g status -s
g restore a.txt
g status -s
g restore --source=HEAD~1 a.txt
cat a.txt
g status -s
g restore --staged --worktree --source=HEAD a.txt
g status -s
rm b.txt
g restore b.txt
cat b.txt
echo added > new.txt
g add new.txt
g reset new.txt
g status -s
g reset --mixed HEAD
g reset -q
g reset --hard
git_rc=$?
g reset nosuch
g restore nosuch.txt
