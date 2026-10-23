# compat: asc-only
# Bucket remotes (SPEC 3.4, 6.2, 6.3): two clones under one ASC_HOME exchange commits through one
# file:// bucket, a clone under a second ASC_HOME is refused, pre-push can abort, a fetch shows
# its remote-tracking updates to reference-transaction, and old revisions are fetched lazily.
set -e
p="$(wpwd)/bucket"
case "$p" in /*) url="file://$p" ;; *) url="file:///$p" ;; esac

git init -q --store "$url" a
cd a
test "$(git config remote.origin.url)" = "$url"
test "$(git config remote.origin.ascRefs)" = "local"
git remote -v | grep -q "refs local to this machine"
echo v1 > f.txt
git add f.txt
at 1
git commit -q -m one
echo v2 > f.txt
git add f.txt
at 2
git commit -q -m two
git push -u origin master 2> ../push-a.err
grep -q "^To " ../push-a.err
grep -q "\[new branch\]      master -> master" ../push-a.err
git rev-parse --verify -q refs/remotes/origin/master >/dev/null
test "$(git config branch.master.remote)" = "origin"
git push origin master 2> ../push-a2.err
grep -q "Everything up-to-date" ../push-a2.err
cd ..

git clone -q "$url" b
test "$(cat b/f.txt)" = "v2"
test "$(git -C b config branch.master.merge)" = "refs/heads/master"
test "$(git -C b config --get-all remote.origin.fetch | tail -n 1)" = "^refs/heads/agent/*"
# an older revision's file arrives on demand from the bucket
test "$(git -C b show HEAD~1:f.txt)" = "v1"

# a second machine (another ASC_HOME) cannot use a --refs local bucket
rc=0
ASC_HOME="$(wpwd)/other-home" git clone "$url" c 2> c.err || rc=$?
test "$rc" = 128
grep -q "refs for .* are local to another machine; use an asc server or --refs shared" c.err
test ! -e c/.git

# b pushes, a fetches and fast-forwards
cd b
echo three > g.txt
git add g.txt
at 3
git commit -q -m three
git push origin master 2> ../push-b.err
grep -Eq "^   [0-9a-f]{7}\.\.[0-9a-f]{7}  master -> master" ../push-b.err
cd ../a
hooks="$PWD/.hooks"
mkdir -p "$hooks"
printf '#!/bin/sh\nif [ "$1" = committed ]; then cat >> "%s/ref-txn.log"; fi\n' "$(wpwd)/.hooks" > "$hooks/reference-transaction"
chmod +x "$hooks/reference-transaction"
git config core.hooksPath "$(wpwd)/.hooks"
git fetch origin 2> ../fetch-a.err
grep -q "^From " ../fetch-a.err
grep -Eq "^   [0-9a-f]{7}\.\.[0-9a-f]{7}  master     -> origin/master" ../fetch-a.err
grep -q "refs/remotes/origin/master" "$hooks/ref-txn.log"
git merge -q --ff-only origin/master
test "$(cat g.txt)" = "three"
git fetch origin 2> ../fetch-a2.err
test ! -s ../fetch-a2.err

# pre-push can abort a push
printf '#!/bin/sh\ncat > "%s/pre-push.in"\necho "pre-push $1" >&2\nexit 1\n' "$(wpwd)/.hooks" > "$hooks/pre-push"
chmod +x "$hooks/pre-push"
echo four > h.txt
git add h.txt
at 4
git commit -q -m four
rc=0
git push origin master 2> ../push-abort.err || rc=$?
test "$rc" = 1
grep -q "^pre-push origin" ../push-abort.err
grep -q "error: failed to push some refs" ../push-abort.err
grep -Eq "^refs/heads/master [0-9a-f]{40} refs/heads/master [0-9a-f]{40}$" "$hooks/pre-push.in"
git push --no-verify origin master 2> ../push-ok.err
grep -q "master -> master" ../push-ok.err
