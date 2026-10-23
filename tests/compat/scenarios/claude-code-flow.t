g init
echo one > a.txt
g add a.txt
at 1
g commit -m first
# The commands Claude Code runs before and after an edit.
g status
g status --porcelain
g status --short --branch
g branch --show-current
g rev-parse --abbrev-ref HEAD
g diff --stat
echo two >> a.txt
echo new > b.txt
g status --porcelain
g diff
g diff HEAD --stat
g log --oneline -n 5
g add -A
g diff --cached
at 2
g commit -m "second"
g log --format=%s -n 3
g show --stat HEAD
g ls-files
