g init
printf 'a\nb\nc\n' > f
printf 'x\n' > d.txt
printf 'y\n' > u.txt
printf 'd.txt merge=mine\nu.txt merge=union\nf merge=binary\n' > .gitattributes
g config merge.mine.driver "echo driven > %A; exit 0"
g add .
at 1
g commit -m base
g switch -c side
printf 'a\nb\nside\n' > f
printf 'x1\n' > d.txt
printf 'y\nside\n' > u.txt
at 2
g commit -am side
g switch master
printf 'main\nb\nc\n' > f
printf 'x2\n' > d.txt
printf 'main\ny\n' > u.txt
at 3
g commit -am main
g merge side
g status
cat d.txt
cat u.txt
cat f
g merge --abort
g config merge.mine.driver "echo conflicted > %A; exit 1"
g merge side
g status
cat d.txt
