g init
mkdir -p .hooks
g config core.hooksPath "$(wpwd)/.hooks"
printf 'a\nb\nc\n' > f.txt
printf 'x\n' > g.txt
g add f.txt g.txt
at 1
g commit -m base
# Partially staged: f.txt has a staged change and a further unstaged change; g.txt is fully staged.
printf 'a\nB\nc\n' > f.txt
g add f.txt
printf 'a\nB\nc\nunstaged\n' > f.txt
printf 'y\n' > g.txt
g add g.txt
# A lint-staged stand-in: lists staged files, formats staged content in a scratch copy
# (blob from the index, as lint-staged does after hiding unstaged changes) and runs `git add`.
cat > .hooks/pre-commit <<'HOOK'
#!/bin/sh
echo "lint-staged index=$(basename "$GIT_INDEX_FILE")" >&2
for f in $(git diff --cached --name-only --diff-filter=ACMR); do
  echo "staged: $f" >&2
done
echo "unstaged diff: $(git diff --name-only | tr '\n' ' ')" >&2
y=$(cat g.txt)
printf '%s!\n' "$y" > g.txt
git add g.txt
HOOK
chmod +x .hooks/pre-commit
at 2
g commit -m formatted
g show HEAD:g.txt
g show HEAD:f.txt
g diff --name-only
g diff --cached --name-only
g status --short
g diff --binary
