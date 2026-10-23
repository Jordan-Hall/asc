# compat: asc-only
# A plain fetch skips agent branches (the clone-written ^refs/heads/agent/* refspec, SPEC 6.2, 11.2)
# and downloads no changeset for them; naming them fetches them; --prune drops a deleted branch.
set -e
p="$(wpwd)/bucket"
case "$p" in /*) url="file://$p" ;; *) url="file:///$p" ;; esac
git init -q --store "$url" a
cd a
echo base > f.txt
git add f.txt
at 1
git commit -q -m base
git push -q origin master
for n in 1 2 3; do
  git switch -q -c "agent/task$n"
  echo "agent $n" > "agent$n.txt"
  git add "agent$n.txt"
  at $((10 + n))
  git commit -q -m "agent $n"
  git push -q origin "agent/task$n"
  git switch -q master
done
git switch -q -c feature
echo feat > feat.txt
git add feat.txt
at 20
git commit -q -m feature
git push -q origin feature
git switch -q master
cd ..
git clone -q "$url" b
cd b
git branch -r > ../remote-branches.txt
grep -q "origin/master" ../remote-branches.txt
grep -q "origin/feature" ../remote-branches.txt
! grep -q "agent" ../remote-branches.txt
git fetch origin 2> ../fetch-plain.err
test ! -s ../fetch-plain.err
test -z "$(git for-each-ref refs/remotes/origin/agent)"
# naming them fetches them
git fetch origin 'refs/heads/agent/*:refs/remotes/origin/agent/*' 2> ../fetch-agent.err
grep -c "\[new branch\]" ../fetch-agent.err | grep -q "^3$"
git rev-parse --verify -q refs/remotes/origin/agent/task2 >/dev/null
# a branch deleted on the remote is pruned on request
cd ../a
git push -q origin --delete feature
cd ../b
git fetch origin 2> ../fetch-keep.err
git rev-parse --verify -q refs/remotes/origin/feature >/dev/null
git fetch --prune origin 2> ../fetch-prune.err
grep -q "^ - \[deleted\]         (none)     -> origin/feature$" ../fetch-prune.err
! git rev-parse --verify -q refs/remotes/origin/feature >/dev/null 2>&1
