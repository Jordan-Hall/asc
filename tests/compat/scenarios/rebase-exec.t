g init
hooks="$PWD/.hooks"
mkdir -p "$hooks"
g config core.hooksPath "$(wpwd)/.hooks"
printf '#!/bin/sh\necho "hook post-rewrite: $1" >&2\ncat >&2\n' > "$hooks/post-rewrite"
chmod +x "$hooks/post-rewrite"
printf 'a\nb\nc\n' > f
g add .
at 1
g commit -m base
g branch root
g switch -c side
echo 1 > m1
g add m1
at 2
g commit -m m1
echo 2 > m2
g add m2
at 3
g commit -m m2
g switch master
echo x > x
g add x
at 4
g commit -m "main x"
g switch side
g tag orig
g rebase --exec "test -f m1" master
g log --oneline
g reset --hard orig
g rebase --exec "test -f nothing" master
gd="$(git rev-parse --git-dir)"
cat "$gd/rebase-merge/done" "$gd/rebase-merge/git-rebase-todo" "$gd/rebase-merge/msgnum" "$gd/rebase-merge/end"
g rebase --continue
g rebase --continue
g log --oneline
g reset --hard orig
g switch -c dup master
echo m1 > m1
g add m1
at 5
g commit -m "m1 again"
g switch side
g rebase dup
g log --oneline
