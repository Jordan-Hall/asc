g init
c() {
  n=$((n + 1))
  at "$n"
  git commit -q --allow-empty -m "$1"
}
n=0
c c1
g tag v1
c c2
c c3
g tag -a v2 -m "release two"
c c4
g checkout -q -b side HEAD~2
c s1
c s2
g checkout -q master
c c5
g merge -q --no-ff side -m merge
c c6
g branch feat HEAD~1
g tag -a t6 -m "tag six"
g tag lw6
c c7
c c8
g branch br HEAD~1
for r in HEAD HEAD~1 HEAD~2 HEAD~3 HEAD~4 HEAD~5 HEAD~6 HEAD~7 HEAD~8 HEAD~9 HEAD~3^2 HEAD~3^2~1 side feat master br v1 v2 t6 lw6 v1^0 v2^0; do
  g name-rev "$r"
done
g name-rev HEAD HEAD~1 HEAD~2
g name-rev --name-only HEAD HEAD~2 HEAD~4
g name-rev --tags HEAD HEAD~1 HEAD~3 HEAD~6 v1 v2
g name-rev --tags --name-only HEAD~3 HEAD~6
g name-rev --refs='refs/heads/*' HEAD~2 v1
g name-rev --refs=feat HEAD~2
g name-rev --refs='refs/tags/*' --exclude='*6' HEAD~2 HEAD~3
g name-rev --exclude='refs/heads/*' HEAD~2
g name-rev --always HEAD~2
g name-rev --tags --always HEAD
g name-rev --tags HEAD
g name-rev --tags --no-undefined HEAD
g name-rev nosuchrev
g name-rev HEAD nosuchrev HEAD~1
g name-rev
git log --format='%H commit %h' -4 | git name-rev --annotate-stdin
git log --format='%H commit %h' -4 | git name-rev --annotate-stdin --name-only
git log --format='%H' -2 | git name-rev --tags --annotate-stdin
