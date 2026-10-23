g init
echo one > a.txt
mkdir sub
echo two > sub/b.txt
g status
g add a.txt sub
g status
g status --short
at 1
g commit -m "first"
g log --format=%H%n%an%n%s
g status
echo three >> a.txt
g commit -a -m "second"
g log --oneline
