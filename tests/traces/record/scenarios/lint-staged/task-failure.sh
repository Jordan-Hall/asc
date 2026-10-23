#@ tool: lint-staged
#@ version: 17.6.0
#@ provenance: recorded
#@ driver: A user stages a change next to an unstaged one, and the lint-staged task rewrites the file and then fails. lint-staged restores the index and the work tree from its backup stash, so the user's staged and unstaged edits both come back unchanged.
g init -q
act 'printf "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n" > a.js; printf "keep\n" > b.js
cat > .lintstagedrc.json <<"CFG"
{"*.js": "asc-act --file .fail.sh"}
CFG
cat > .fail.sh <<"TASK"
echo broken >> "$1"
exit 1
TASK'
g add -A
g commit -qm "chore: baseline"
act 'sed -i "1s/alpha/ALPHA/" a.js; printf "new line\n" >> b.js'
g add a.js b.js
act 'sed -i "8s/theta/THETA/" a.js'
tool lint-staged lint-staged
g status --porcelain=v1
g diff --cached --stat
g diff --stat
g stash list
