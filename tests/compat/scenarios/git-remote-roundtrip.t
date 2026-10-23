# compat: asc-only
# Git-remote mode (SPEC 6.3): a local bare repo stands in for Forgejo. Clone, commit, push, a
# second clone and a fetch see the same ids, a stale push is refused, tags travel, and
# `--reference` seeds the private bare repo without an alternates link. Real git runs only
# through $ASC_REAL_GIT (never the shim), so the runner's fall-through check also asserts that
# asc's own git children do not recurse into the shim.
set -e
rg() { "${ASC_REAL_GIT:-git}" "$@"; }
p="$(wpwd)"

rg init -q --bare upstream.git
rg init -q seed
cd seed
echo one > f.txt
rg add f.txt
at 1
rg commit -q -m one
rg push -q ../upstream.git master
cd ..

git clone -q "$p/upstream.git" a
cd a
test "$(git rev-parse HEAD)" = "$(rg -C ../seed rev-parse HEAD)"
test "$(git rev-parse origin/master)" = "$(rg -C ../seed rev-parse HEAD)"
echo two > g.txt
git add g.txt
at 2
git commit -q -m two
git push -q origin master
test "$(git rev-parse HEAD)" = "$(rg --git-dir=../upstream.git rev-parse master)"
test "$(git rev-parse origin/master)" = "$(git rev-parse HEAD)"
git branch topic
git push -q origin topic
test "$(rg --git-dir=../upstream.git rev-parse topic)" = "$(git rev-parse HEAD)"
git tag v1
git push -q origin v1
test "$(rg --git-dir=../upstream.git rev-parse v1)" = "$(git rev-parse HEAD)"
cd ..

# a second machine
git clone -q "$p/upstream.git" b
test "$(git -C b rev-parse HEAD)" = "$(git -C a rev-parse HEAD)"
test "$(git -C b rev-parse origin/topic)" = "$(git -C a rev-parse HEAD)"

# a commit lands on the upstream; the second clone fetches it
cd seed
rg pull -q --ff-only ../upstream.git master
echo three > h.txt
rg add h.txt
at 3
rg commit -q -m three
rg push -q ../upstream.git master
cd ..
git -C b fetch -q origin
test "$(git -C b rev-parse origin/master)" = "$(rg -C seed rev-parse HEAD)"

# a push of a diverged branch is refused and the upstream keeps its value
cd a
echo four > i.txt
git add i.txt
at 4
git commit -q -m four
before="$(rg --git-dir=../upstream.git rev-parse master)"
if git push -q origin master 2> ../push-refused.err; then
  echo "diverged push was accepted" >&2
  exit 1
fi
test "$(rg --git-dir=../upstream.git rev-parse master)" = "$before"
cd ..

# --reference copies objects from an existing checkout, with no alternates link
git clone -q --reference "$p/seed/.git" "$p/upstream.git" c
test "$(git -C c rev-parse HEAD)" = "$(rg -C seed rev-parse HEAD)"
for d in "$ASC_HOME"/repos/*/git-remote.git; do
  test -d "$d/objects"
  test ! -e "$d/objects/info/alternates"
done
