#@ tool: lint-staged
#@ version: 17.6.0
#@ provenance: recorded
#@ driver: A user stages a new file, a deleted file and a renamed file together with a modified one, and runs lint-staged. Its task touches only the files it is given, so a deletion is never passed to it and the rename shows as an added path.
g init -q
act 'mkdir -p src; printf "old one\n" > src/old.js; printf "gone\n" > src/gone.js; printf "mod\nmod two\n" > src/mod.js
cat > .lintstagedrc.json <<"CFG"
{"*.js": "asc-act --file .semi.sh"}
CFG
cat > .semi.sh <<"TASK"
for f in "$@"; do sed -i "1s/\$/;/" "$f"; done
TASK'
g add -A
g commit -qm "chore: baseline"
act 'printf "fresh\n" > src/new.js; sed -i "1s/mod/MOD/" src/mod.js'
g mv src/old.js src/renamed.js
g rm -q src/gone.js
g add src/new.js src/mod.js
tool lint-staged lint-staged
g status --porcelain=v1
g diff --cached --name-status
