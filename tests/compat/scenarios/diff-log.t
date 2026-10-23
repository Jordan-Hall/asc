g init
printf 'one\ntwo\nthree\n' > a.txt
printf 'x\n' > b.txt
g add a.txt b.txt
at 1
g commit -m "first commit"
printf 'one\nTWO\nthree\nfour\n' > a.txt
g diff
g diff --stat
g diff --numstat
g diff --shortstat
g diff --name-only
g diff --name-status
g diff -U1
g diff --cached
g add a.txt
g diff
g diff --cached
g diff --cached --stat
g diff HEAD
at 2
g commit -m "second commit" -m "with a body"
rm b.txt
mkdir d
printf 'new\n' > d/c.txt
g add d/c.txt
at 3
g commit -a -m "third commit"
g diff HEAD~1 HEAD
g diff HEAD~2 HEAD --stat
g diff HEAD~2..HEAD --name-status
g diff HEAD~1 HEAD -- d
g log
g log --oneline
g log -n 2
g log -1 --format=%H
g log --format='%h %an <%ae> %ad %s'
g log --format=%B -n 1
g log --stat
g log --name-status
g log --name-only --oneline
g log -p
g log --oneline --graph
g log --oneline -- a.txt
g log --author=Thor --oneline
g log --grep=second --oneline
g log --reverse --oneline
g log --pretty=fuller -n 1
g log --date=iso -n 1
g log HEAD~1..HEAD --oneline
g show
g show HEAD~1 --stat
g show --name-only --format=%s HEAD~2
g diff --no-index a.txt d/c.txt
