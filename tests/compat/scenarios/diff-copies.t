g init
printf 'line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\n' > src.txt
printf 'gone 1\ngone 2\ngone 3\ngone 4\ngone 5\ngone 6\n' > old.txt
g add .
at 1
g commit -m base
cp src.txt copy.txt
printf 'line 11\n' >> src.txt
g add .
g diff --cached --name-status
g diff --cached -C --name-status
g diff --cached --find-copies --summary
g diff --cached -C --stat
g diff --cached -C
g diff --cached -C90% --name-status
g config diff.renames copies
g diff --cached --name-status
g config diff.renames true
at 2
g commit -m copy
cp old.txt a.txt
mv old.txt b.txt
g add -A
g diff --cached -C --name-status
g diff --cached -M --name-status
at 3
g commit -m split
g log -C --name-status --oneline
g show -C --name-status --oneline HEAD~1
