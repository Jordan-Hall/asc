#@ tool: lint-staged
#@ version: 17.6.0
#@ provenance: recorded
#@ driver: A user edits a.js in two places, stages the whole file, then edits a third line without staging it. They run lint-staged, whose task appends a semicolon to the first line of each staged file. lint-staged hides the unstaged hunk, runs the task on the staged content, stages the result and brings the unstaged hunk back.
# The task goes through asc-act (record/bin), so the trace carries its effect on the files as a
# step. The task script and the config are committed files with no host paths.
g init -q
act 'printf "line one\nline two\nline three\nline four\nline five\nline six\nline seven\nline eight\n" > a.js
cat > .lintstagedrc.json <<"CFG"
{"*.js": "asc-act --file .semi.sh"}
CFG
cat > .semi.sh <<"TASK"
for f in "$@"; do sed -i "1s/\$/;/" "$f"; done
TASK'
g add -A
g commit -qm "chore: baseline"
act 'sed -i "1s/one/ONE/;8s/eight/EIGHT/" a.js'
g add a.js
act 'sed -i "4s/four/FOUR/" a.js'
tool lint-staged lint-staged
g status --porcelain=v1
g diff --cached
g diff
