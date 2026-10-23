#@ tool: commitlint
#@ version: 21.2.4
#@ provenance: recorded
#@ driver: A CI step lints the commits of a branch with commitlint --from and --to, then the last commit with --last. The branch holds one valid and one invalid message.
g init -q
act 'printf "one\n" > a.txt
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
g checkout -q -b topic
act 'printf "two\n" >> a.txt'
g commit -qam "feat: second change"
act 'printf "three\n" >> a.txt'
g commit -qam "third change"
tool commitlint commitlint --from master --to topic
tool commitlint commitlint --last
