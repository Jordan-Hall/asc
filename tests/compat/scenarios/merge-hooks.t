g init
hooks="$PWD/.hooks"
mkdir -p "$hooks"
g config core.hooksPath "$(wpwd)/.hooks"
for h in pre-merge-commit prepare-commit-msg commit-msg post-merge post-commit; do
  printf '#!/bin/sh\necho "hook %s: args=$(basename "${1:-none}") ${2:-} msg=$(head -n 1 "$1" 2>/dev/null)" >&2\n' "$h" > "$hooks/$h"
  chmod +x "$hooks/$h"
done
echo one > a.txt
echo base > b.txt
g add .
at 1
g commit -m first
g switch -c side
echo side > s.txt
g add s.txt
at 2
g commit -m "side file"
g switch master
echo main > m.txt
g add m.txt
at 3
g commit -m "main file"
g merge side
g log --oneline --graph
g reset --hard HEAD~1
g merge --no-verify side
g reset --hard HEAD~1
printf '#!/bin/sh\necho "pre-merge-commit rejects" >&2\nexit 1\n' > "$hooks/pre-merge-commit"
g merge side
g status
g merge --abort
g merge --no-verify side
g reset --hard HEAD~1
g merge --abort
g reset --hard HEAD
g log --oneline
