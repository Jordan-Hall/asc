# compat: asc-only
# A requested path inside another workspace is mounted under ASC_HOME/wt and reached through a
# link; the parent sees one untracked entry (SPEC §9.3).
set -e
git init
echo one > a.txt
git add a.txt
git commit -m first
git worktree add nested/wt -b nested-b
test -f nested/wt/a.txt
test -f nested/wt/.git
git -C nested/wt rev-parse --abbrev-ref HEAD | grep -qx nested-b
test "$(git status --porcelain)" = "?? nested/"
git worktree list | grep -q 'nested/wt'
echo two > nested/wt/b.txt
git -C nested/wt add b.txt
git -C nested/wt commit -m second
git rev-parse nested-b > /dev/null
# The worktree's own status does not leak into the parent's.
test "$(git -C nested/wt status --porcelain)" = ""
# Removing a worktree with committed work works; the link and the mount go.
git worktree remove nested/wt
test ! -e nested/wt
git branch --list nested-b | grep -q nested-b
git worktree list | grep -vq 'nested/wt'
# Dirty worktrees are refused without --force.
git worktree add nested/two -b nested-two
echo x > nested/two/untracked.txt
if git worktree remove nested/two 2>err.txt; then exit 1; fi
grep -q 'contains modified or untracked files' err.txt
test -f nested/two/untracked.txt
git worktree remove --force nested/two
test ! -e nested/two
rm -f err.txt
