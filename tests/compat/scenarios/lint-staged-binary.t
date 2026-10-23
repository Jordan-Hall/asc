g init
mkdir -p .hooks
g config core.hooksPath "$(wpwd)/.hooks"
printf 'one\n' > a.txt
g add a.txt
at 1
g commit -m base
printf '\000\001\002bin\n' > blob.bin
printf 'two\n' >> a.txt
g add blob.bin a.txt
cat > .hooks/pre-commit <<'HOOK'
#!/bin/sh
git diff --cached --no-color --no-ext-diff --name-only >&2
git diff --cached --numstat >&2
HOOK
chmod +x .hooks/pre-commit
at 2
g commit -m binary
g diff --binary HEAD~1 HEAD
g ls-files -s
