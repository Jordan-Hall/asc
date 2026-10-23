#@ tool: commitlint
#@ version: 21.2.4
#@ provenance: recorded
#@ driver: A user checks a commit message file with commitlint --edit, once with a message that passes and once with a message that breaks the type and subject rules, the way a commit-msg hook calls it.
g init -q
act 'printf "tracked\n" > a.txt
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
g add -A
g commit -qm "chore: baseline"
act 'printf "feat: add a feature\n\nA body line.\n" > COMMIT_MSG.txt'
tool commitlint commitlint --edit COMMIT_MSG.txt
act 'printf "added stuff\n" > COMMIT_MSG.txt'
tool commitlint commitlint --edit COMMIT_MSG.txt
