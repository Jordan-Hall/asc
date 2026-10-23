g init
printf 'one\n' > a.txt
mkdir d
printf 'two\n' > d/b.txt
g add .
at 1
g commit -m base
tree=$(git rev-parse 'HEAD^{tree}')
base=$(git rev-parse HEAD)
at 2
g commit-tree "$tree" -m "root commit"
g commit-tree "$tree" -p "$base" -m "child"
g commit-tree "$tree" -p HEAD -m "first" -m "second paragraph"
printf 'from stdin\n\nbody\n' | git commit-tree "$tree" -p "$base"
printf 'file message\nsecond line\n' > msg.txt
g commit-tree "$tree" -p "$base" -F msg.txt
g commit-tree "$tree" -p "$base" -m "head" -F msg.txt -m "tail"
printf 'no newline' | git commit-tree "$tree"
c=$(git commit-tree "$tree" -p "$base" -m "kept")
g cat-file -p "$c"
g log --format='%H %s' "$c"
g commit-tree "$tree" -p "$base" -p "$base" -m "dup parent"
g commit-tree -p "$base" "$tree" -m "options first"
g commit-tree
g commit-tree "$base" -m "not a tree"
g commit-tree nosuch -m x
g commit-tree "$tree" -p nosuch -m x
g commit-tree "$tree" "$tree" -m x
g commit-tree "$tree" -m
