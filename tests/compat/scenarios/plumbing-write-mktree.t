g init
printf 'one\n' > a.txt
printf 'two\n' > b.txt
mkdir d
printf 'three\n' > d/c.txt
printf '#!/bin/sh\n' > run.sh
chmod +x run.sh
g add .
at 1
g commit -m base
g ls-tree HEAD
git ls-tree HEAD | git mktree
git ls-tree -r HEAD | git mktree
git ls-tree HEAD:d | git mktree
tree=$(git ls-tree HEAD | git mktree)
g cat-file -p "$tree"
g rev-parse 'HEAD^{tree}'
a=$(git rev-parse HEAD:a.txt)
d=$(git rev-parse HEAD:d)
printf '100644 blob %s\tz.txt\n040000 tree %s\tdir\n100644 blob %s\ta.txt\n' "$a" "$d" "$a" | git mktree
printf '100644 blob %s\tz.txt\n040000 tree %s\tdir\n100644 blob %s\ta.txt\n' "$a" "$d" "$a" | git mktree > out.txt
g cat-file -p "$(cat out.txt)"
printf '100755 blob %s\tx\n120000 blob %s\tlink\n' "$a" "$a" | git mktree
printf '100644 blob %s\tq\n\n100644 blob %s\tr\n' "$a" "$a" | git mktree --batch
printf '100644 blob %s\tf\0' "$a" | git mktree -z
printf '' | git mktree
printf '100644 blob %s\t"tab\there"\n' "$a" | git mktree
printf '100644 blob %s\ta/b\n' "$a" | git mktree
printf '100644 tree %s\tbad\n' "$a" | git mktree
printf '040000 blob %s\tbad\n' "$a" | git mktree
printf '100644 blob %s bad\n' "$a" | git mktree
printf '100644 blob 0000000000000000000000000000000000000001\tgone\n' | git mktree
printf 'garbage\n' | git mktree
g mktree extra
