# `log --follow <path>` walks as git does: every commit in git's order, one path followed and
# moved to the source of a rename (found by content when git's mv recorded none), merges shown
# only with --first-parent, --skip counted over walked commits; --graph keeps real parents and
# leaves git's `...` where hidden commits passed.
g init
echo one > f.txt
g add f.txt
at 1
g commit -q -m c1
g checkout -q -b side
echo s > s.txt
g add s.txt
at 2
g commit -q -m s1
echo f-side > f.txt
at 3
g commit -q -a -m s2
g checkout -q master
echo two > other.txt
g add other.txt
at 4
g commit -q -m o1
at 5
g merge -q --no-ff -m merge-side side
g mv f.txt g.txt
at 6
g commit -q -m mv
echo three > g.txt
at 7
g commit -q -a -m c3
echo four > other.txt
at 8
g commit -q -a -m o2
# A rename made with git's mv and an edit: found by content, as git's -M50% finds it.
printf 'l1\nl2\nl3\nl4\n' > g.txt
at 9
g commit -q -a -m c4
g mv g.txt h.txt
printf 'l1\nl2\nl3\nl5\n' > h.txt
g add h.txt
at 10
g commit -q -m mv-edit

g log --oneline --graph --follow -- g.txt
g log --graph --format=%s --follow -- g.txt
g log --oneline --graph --follow --first-parent -- g.txt
g log --oneline --graph --follow --date-order -- g.txt
g log --oneline --graph --follow -n 2 -- g.txt
g log --oneline --graph --follow --skip 1 -- g.txt
g log --oneline --graph --follow -- f.txt
g log --oneline --graph --follow side -- f.txt
g log --oneline --graph --follow -- h.txt

# The same walks without the graph.
g log --oneline --follow -- g.txt
g log --oneline --follow --first-parent -- g.txt
g log --oneline --follow --skip 1 -- g.txt
g log --oneline --follow -- f.txt
g log --oneline --follow --first-parent -- f.txt
g log --oneline --follow -- h.txt
g log --format=%s --name-status --follow -- h.txt
