g init
printf 'line1\nline2\nline3\nline4\nline5\nline6\n' > old.txt
mkdir dir
echo keep > dir/keep.txt
g add .
at 1
g commit -m base
g mv old.txt new.txt
g status
g status -s
g diff --cached -M --name-status
g diff --cached --stat
g diff --cached -M --summary
at 2
g commit -m rename
g log --stat --oneline
g log -M --name-status --oneline
mv new.txt moved.txt
g add -A
g status -s
g diff --cached --name-status
g mv dir other
g status -s
g mv nosuch x
g mv -n moved.txt again.txt
g mv moved.txt dir/
g status -s
at 3
g commit -m moves
g diff HEAD~2 HEAD --name-status -M
g diff HEAD~2 HEAD --stat -M
g log --name-status --oneline -M
