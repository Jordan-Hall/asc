g init
mkdir -p .husky/_
printf '#!/bin/sh\nn=$(basename "$0")\ns=$(dirname "$(dirname "$0")")/$n\n[ -f "$s" ] || exit 0\n. "$s"\n' > .husky/_/h
printf '#!/bin/sh\n. "$(dirname "$0")/h"\n' > .husky/_/commit-msg
chmod +x .husky/_/h .husky/_/commit-msg
# A commitlint stand-in: reads the message file named by $1 and enforces "type: subject".
cat > .husky/commit-msg <<'HOOK'
#!/bin/sh
echo "commitlint reads $(basename "$1")" >&2
if head -n 1 "$1" | grep -Eq '^(feat|fix|chore): .+'; then
  echo "commitlint ok" >&2
else
  echo "commitlint: subject may not be empty, type may not be empty" >&2
  exit 1
fi
HOOK
g config core.hooksPath .husky/_
echo one > a.txt
g add a.txt
at 1
g commit -m "bad subject"
g log --oneline
g status --short
at 2
g commit -m "feat: add a"
g log --oneline
echo two >> a.txt
at 3
g commit -a -m "bad again" --no-verify
g log --oneline
