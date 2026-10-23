g init
printf 'one\n' > a.txt
g add a.txt
at 1
g commit -m one
printf 'two\n' >> a.txt
at 2
g commit -am two
one=$(git rev-parse HEAD~1)
two=$(git rev-parse HEAD)
# Notes written with plumbing: a tree of <object id> files under refs/notes/commits.
n1=$(printf 'note for one\n' | git hash-object -w --stdin)
n2=$(printf 'note for two\nsecond line\n' | git hash-object -w --stdin)
tree=$(printf '100644 blob %s\t%s\n100644 blob %s\t%s\n' "$n1" "$one" "$n2" "$two" | git mktree)
at 3
nc=$(git commit-tree "$tree" -m "Notes added by 'git notes add'")
git update-ref refs/notes/commits "$nc"
g notes show
g notes show HEAD
g notes show HEAD~1
g notes show "$one"
g notes --ref commits show HEAD
g notes --ref=refs/notes/commits show HEAD~1
g notes show nosuchrev
g notes --ref nonesuch show HEAD
g notes list
g notes list HEAD
g notes
at 4
printf 'three\n' >> a.txt
g commit -am three
g notes show
g notes show HEAD~1
