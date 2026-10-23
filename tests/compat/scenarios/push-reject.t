# compat: asc-only
# A push while behind is rejected as git rejects it (SPEC 6.3, 11.2): the table, the error line and the
# advice, byte for byte, and the rejection is final until the remote work is fetched and merged.
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
cd ..
git clone -q "$url" b
cd b
echo b > b.txt
git add b.txt
at 2
git commit -q -m "b work"
git push -q origin master
cd ../a
echo a > a.txt
git add a.txt
at 3
git commit -q -m "a work"
rc=0
git push origin master 2> ../reject.err || rc=$?
test "$rc" = 1
cat > ../reject.want <<WANT
To $url
 ! [rejected]        master -> master (fetch first)
error: failed to push some refs to '$url'
hint: Updates were rejected because the remote contains work that you do not
hint: have locally. This is usually caused by another repository pushing to
hint: the same ref. If you want to integrate the remote changes, use
hint: 'git pull' before pushing again.
hint: See the 'Note about fast-forwards' in 'git push --help' for details.
WANT
sed 's/asc pull/git pull/; s/asc push/git push/' ../reject.err > ../reject.got
cmp ../reject.want ../reject.got
git push --porcelain origin master > ../porcelain.out 2>/dev/null || true
grep -q "^!	refs/heads/master:refs/heads/master	\[rejected\] (fetch first)" ../porcelain.out
tail -n 1 ../porcelain.out | grep -q "^Done$"
# after a fetch the same push is a non-fast-forward
git fetch -q origin
rc=0
git push origin master 2> ../reject2.err || rc=$?
test "$rc" = 1
grep -q "^ ! \[rejected\]        master -> master (non-fast-forward)$" ../reject2.err
grep -q "the tip of your current branch is behind" ../reject2.err
# pull merges, then the push goes through
git pull -q --no-rebase origin master
git push -q origin master
git push --dry-run origin master 2>&1 | grep -q "Everything up-to-date"
# --force-with-lease with a stale expectation is rejected as stale info
cd ../b
git pull -q --no-rebase origin master
echo again >> b.txt
git add b.txt
at 5
git commit -q -m again
git push -q origin master
cd ../a
echo more >> a.txt
git add a.txt
at 6
git commit -q -m more
rc=0
git push --force-with-lease=master:HEAD~1 origin master 2> ../stale.err || rc=$?
test "$rc" = 1
grep -q "(stale info)" ../stale.err
