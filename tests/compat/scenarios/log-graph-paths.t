# `log --graph -- <path>` simplifies history as git's default does: commits with no change under
# the paths are left out, a merge with no change compared with one parent follows only that
# parent, and each shown commit is drawn joined to its nearest shown ancestors.
g init
echo base > other.txt
g add other.txt
at 1
g commit -q -m root-other
echo a1 > a.txt
g add a.txt
at 2
g commit -q -m a1
g checkout -q -b side
echo s1 > a.txt
at 3
g commit -q -a -m side-a
echo o2 > other.txt
at 4
g commit -q -a -m side-other
g checkout -q master
mkdir dir
echo d1 > dir/d.txt
g add dir/d.txt
at 5
g commit -q -m main-dir
at 6
g merge -q --no-ff -m merge-side side
echo a2 > a.txt
at 7
g commit -q -a -m a2

# A branch that changes only other paths: its merge brings nothing under a.txt.
g checkout -q -b other-only
echo o3 > other.txt
at 8
g commit -q -a -m other-only
g checkout -q master
echo d2 > dir/d.txt
at 9
g commit -q -a -m main-dir2
at 10
g merge -q --no-ff -m merge-other-only other-only

# The same change on both sides: the merge has no change compared with its first parent.
g checkout -q -b twin
echo same > a.txt
at 11
g commit -q -a -m twin-a
g checkout -q master
echo same > a.txt
at 12
g commit -q -a -m main-a
at 13
g merge -q --no-ff -m merge-twin twin

g log --oneline --graph -- a.txt
g log --oneline --graph -- dir
g log --oneline --graph -- other.txt
g log --oneline --graph -- a.txt dir
g log --oneline --graph --first-parent -- a.txt
g log --oneline --graph HEAD~3..HEAD -- a.txt
g log --oneline --graph side..master -- a.txt
g log --graph --format=%s -- missing.txt
g log --oneline --graph --date-order -- other.txt dir
