g init
printf 'a\nb\nc\nd\ne\n' > f
printf 'x\n' > g
g add .
at 1
g commit -m base
basec=$(git rev-parse HEAD)
g branch side
printf 'a\nb2\nc\nd\ne\n' > f
printf 'z\n' > h
g add h
at 2
g commit -am main
g checkout -q side
printf 'a\nb\nc\nd\ne\nf\n' > f
printf 'y\n' > k
g add k
at 3
g commit -am side
g merge-tree --write-tree master side
g merge-tree --write-tree --name-only master side
g checkout -q -b clash master
printf 'a\nB\nc\nd\ne\n' > f
at 4
g commit -am clash
g merge-tree --write-tree master clash
g merge-tree --write-tree --name-only master clash
g merge-tree --write-tree --no-messages master clash
g merge-tree --write-tree --quiet master clash
g merge-tree --write-tree --quiet master side
g merge-tree --write-tree --messages master side
g merge-tree --write-tree clash master
g merge-tree --write-tree -z master clash
t=$(git merge-tree --write-tree master side)
g ls-tree -r "$t"
g cat-file -p "$t:f"
g merge-tree --write-tree --merge-base="$basec" master side
g merge-tree --write-tree master
g merge-tree --write-tree master nosuch
g checkout -q -b gone master
g rm -q g
at 5
g commit -m "delete g"
g checkout -q -b chg master
printf 'x2\n' > g
at 6
g commit -am "change g"
g merge-tree --write-tree gone chg
g merge-tree --write-tree chg gone
