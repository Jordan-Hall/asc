g init
hooks="$PWD/.hooks"
mkdir -p "$hooks"
g config core.hooksPath "$(wpwd)/.hooks"
printf '#!/bin/sh\necho "hook pre-rebase: $*" >&2\n' > "$hooks/pre-rebase"
printf '#!/bin/sh\necho "hook post-rewrite: $1" >&2\ncat >&2\n' > "$hooks/post-rewrite"
chmod +x "$hooks/pre-rebase" "$hooks/post-rewrite"
echo base > f
g add .
at 1
g commit -m base
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
g rebase master
g reset --hard HEAD~2
g rebase --no-verify master
g log --oneline
g switch -c other side
g rebase master side
g rebase master other
printf '#!/bin/sh\nexit 1\n' > "$hooks/pre-rebase"
g switch -c third master
echo t > t
g add t
at 5
g commit -m third
g rebase master side
g rebase --no-verify master side
g log --oneline
