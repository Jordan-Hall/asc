#@ tool: husky
#@ version: 9.1.7
#@ provenance: recorded
#@ driver: A user installs husky (the prepare script), adds a pre-commit hook that runs lint-staged, a commit-msg hook that runs commitlint, a post-checkout hook that looks for a changed lockfile and a pre-push hook that refuses a dirty work tree. They commit, commit with a rejected message, switch to a new branch, commit a partially staged file and push to a bare remote.
g init -q
act 'printf "line one\nline two\nline three\nline four\nline five\nline six\nline seven\nline eight\n" > a.js
printf "{\"name\":\"fx\",\"private\":true,\"scripts\":{\"prepare\":\"husky\"}}\n" > package.json
printf "bun.lock stub\n" > bun.lock
printf ".husky/_/\n" > .gitignore
cat > .lintstagedrc.json <<"CFG"
{"*.js": "asc-act --file .semi.sh"}
CFG
cat > .semi.sh <<"TASK"
for f in "$@"; do sed -i "1s/\$/;/" "$f"; done
TASK
cat > commitlint.config.mjs <<"CFG"
export default {
  rules: {
    "type-empty": [2, "never"],
    "type-enum": [2, "always", ["feat", "fix", "chore", "docs"]],
    "subject-empty": [2, "never"],
    "header-max-length": [2, "always", 72],
  },
};
CFG'
tool husky husky
act 'mkdir -p .husky
cat > .husky/pre-commit <<"H"
lint-staged
H
cat > .husky/commit-msg <<"H"
commitlint --edit "$1"
H
cat > .husky/post-checkout <<"H"
git diff --name-only "$1" "$2" -- bun.lock
H
cat > .husky/pre-push <<"H"
git diff --quiet || { echo "dirty work tree"; exit 1; }
git rev-parse --abbrev-ref HEAD
H'
g add -A
g commit -qm "chore: baseline"
act 'sed -i "1s/one/ONE/" a.js'
g add a.js
g commit -qm "update a.js"
g checkout -q -b feature
act 'sed -i "8s/eight/EIGHT/" a.js'
g add a.js
act 'sed -i "4s/four/FOUR/" a.js'
g commit -qm "feat: change line eight"
g status --porcelain=v1
g init -q --bare ../remote.git
g remote add origin ../remote.git
g checkout -- a.js
g push -u origin feature
