# compat: asc-only
# A hook child reaches asc for every `git`, with no user shim installed (SPEC §9.7, §11.4).
set -e
git init
mkdir -p .husky/_ .hooks
printf '#!/bin/sh\n. "$(dirname "$0")/h"\n' > .husky/_/pre-commit
printf '#!/bin/sh\ns=$(dirname "$(dirname "$0")")/$(basename "$0")\n. "$s"\n' > .husky/_/h
chmod +x .husky/_/pre-commit .husky/_/h
cat > .husky/pre-commit <<'HOOK'
#!/bin/sh
git rev-parse --git-dir > "$PWD/hook-gitdir.txt" || exit 1
git diff --cached --name-only > "$PWD/hook-staged.txt" || exit 1
if command -v node >/dev/null 2>&1; then
  node -e 'const {execFileSync:e}=require("child_process");e("git",["status","--short"],{stdio:"ignore"})' || exit 1
fi
# lint-staged style: reformat a staged file and `git add` it; the commit must carry the result.
if [ -f fmt.txt ]; then
  tr a-z A-Z < fmt.txt > fmt.tmp && mv fmt.tmp fmt.txt
  git add fmt.txt
fi
HOOK
chmod +x .husky/pre-commit
git config core.hooksPath .husky/_
echo hello > fmt.txt
echo other > o.txt
git add fmt.txt o.txt
git commit -m first
test -s hook-gitdir.txt
grep -q "fmt.txt" hook-staged.txt && grep -q "o.txt" hook-staged.txt
test "$(git show HEAD:fmt.txt)" = "HELLO"
# the committed blob equals git's blob of the formatted bytes
want=$(printf 'HELLO\n' | git hash-object --stdin)
test "$(git rev-parse HEAD:fmt.txt)" = "$want"
