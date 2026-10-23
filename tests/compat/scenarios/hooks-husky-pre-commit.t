husky_setup() {
  mkdir -p .husky/_
  cat > .husky/_/h <<'EOF'
#!/bin/sh
n=$(basename "$0")
s=$(dirname "$(dirname "$0")")/$n
[ -f "$s" ] || exit 0
. "$s"
EOF
  for h in pre-commit commit-msg pre-push post-checkout; do
    printf '#!/bin/sh\n. "$(dirname "$0")/h"\n' > ".husky/_/$h"
    chmod +x ".husky/_/$h"
  done
  g config core.hooksPath .husky/_
}
g init
husky_setup
printf '#!/bin/sh\necho "husky pre-commit: staged=$(git diff --cached --name-only | tr "\n" " ")" >&2\nexit 0\n' > .husky/pre-commit
printf '#!/bin/sh\necho "husky post-checkout: $3" >&2\nbun install --frozen-lockfile 2>/dev/null || echo "bun install stub" >&2\n' > .husky/post-checkout
echo one > a.txt
g add a.txt
at 1
g commit -m first
echo two > b.txt
g add b.txt
at 2
g commit -m second
g branch feature
g switch feature
g switch master
printf '#!/bin/sh\necho "husky pre-commit rejects" >&2\nexit 1\n' > .husky/pre-commit
echo three > c.txt
g add c.txt
at 3
g commit -m rejected
g log --oneline
g status --short
