g init
seq 1 30 | sed 's/^/line /' > f
g add f
at 1
g commit -m base
g branch topic1
g checkout -q -b old topic1
sed -i 's/^line 5$/line five/' f
sed -i 's/^line 25$/line 25 old/' f
at 2
g commit -a -m "feature" -m "details of the feature"
printf 'x\n' > g
g add g
at 3
g commit -m "add g"
printf '#!/bin/sh\n' > run.sh
g add run.sh
at 4
g commit -m "add script"
g checkout -q -b new topic1
sed -i 's/^line 5$/line five/' f
sed -i 's/^line 25$/line 25 new/' f
at 5
g commit -a -m "feature (v2)" -m "details of the feature"
printf 'x\ny\n' > g
g add g
at 6
g commit -m "add g"
printf '#!/bin/sh\n' > run.sh
g add run.sh
at 7
g commit -m "add script"
git rm -q run.sh
at 8
g commit -m "remove script"
g range-diff topic1 old new
g range-diff topic1..old topic1..new
g range-diff old...new
g range-diff --no-patch topic1 old new
g range-diff -s topic1 old new
g range-diff --left-only topic1 old new
g range-diff --right-only topic1 old new
g range-diff --creation-factor=200 topic1 old new
g range-diff --creation-factor=10 topic1 old new
g range-diff --no-dual-color topic1 old new
g range-diff topic1 new new
g range-diff topic1 old topic1
g range-diff topic1 topic1 new
g range-diff topic1 old
g range-diff nosuch old new
g range-diff --left-only --right-only topic1 old new
