export GIT_AUTHOR_NAME="Rena Mer" GIT_AUTHOR_EMAIL=rena@example.com
g init
printf 'alpha\nbeta\ngamma\ndelta\nepsilon\n' > f.txt
g add f.txt
at 1
g commit -m base
g mv f.txt sub.txt
at 2
g commit -m rename
export GIT_AUTHOR_NAME="Ed Itor" GIT_AUTHOR_EMAIL=ed@example.com
printf 'alpha\nBETA\ngamma\ndelta\nepsilon\nzeta\n' > sub.txt
at 3
g commit -am edit
g blame sub.txt
g blame --porcelain sub.txt
g blame --line-porcelain -L 1,2 sub.txt
g blame --incremental sub.txt
g blame HEAD~1 sub.txt
g blame HEAD~2 sub.txt
printf 'alpha\n  BETA  \ngamma\ndelta\nepsilon\nzeta\n' > sub.txt
at 4
g commit -am whitespace
g blame sub.txt
g blame -w sub.txt
g blame -w --porcelain -L 2,2 sub.txt
printf 'alpha\n\tBETA\ngamma\nDELTA\nepsilon\nzeta\n' > sub.txt
at 5
g commit -am more
g blame -w sub.txt
g blame -w -L 2,4 HEAD~1 sub.txt
g blame --reverse HEAD~4 -- sub.txt
g blame --reverse HEAD~4..HEAD~1 -- sub.txt
g blame --reverse HEAD~4..HEAD -- f.txt
g blame --reverse HEAD~3 -- sub.txt
g blame --reverse HEAD~4 --porcelain -- sub.txt
g blame --reverse -- sub.txt
g blame --reverse HEAD~4 -- nosuch.txt
printf 'p
q
r
s
t
u
' > m.txt
g add m.txt
at 6
g commit -m m
g mv m.txt n.txt
printf 'p
q
r
s
t
U
v
' > n.txt
g add n.txt
at 7
g commit -m mn
g blame n.txt
g blame --porcelain n.txt
g blame --reverse HEAD~1 -- m.txt
