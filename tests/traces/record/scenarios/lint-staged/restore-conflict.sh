#@ tool: lint-staged
#@ version: 17.6.0
#@ provenance: recorded
#@ driver: A user stages a change to line one and leaves an unstaged change to line four. The lint-staged task appends a semicolon to every line, so the hidden unstaged hunk no longer applies to the staged result. lint-staged retries the apply with --3way, and when that fails too it resets to HEAD and restores the backup stash.
g init -q
act 'printf "line one\nline two\nline three\nline four\nline five\n" > a.js
cat > .lintstagedrc.json <<"CFG"
{"*.js": "asc-act --file .semi.sh"}
CFG
cat > .semi.sh <<"TASK"
for f in "$@"; do sed -i "s/\$/;/" "$f"; done
TASK'
g add -A
g commit -qm "chore: baseline"
act 'sed -i "1s/one/ONE/" a.js'
g add a.js
act 'sed -i "4s/four/FOUR/" a.js'
tool lint-staged lint-staged
g status --porcelain=v1
g diff --cached
g diff
g stash list
