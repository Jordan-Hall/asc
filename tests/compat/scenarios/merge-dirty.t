g init
printf 'a\nb\nc\n' > f
g add .
at 1
g commit -m base
g switch -c side
printf 'a\nb\nside\n' > f
echo s > s.txt
g add -A
at 2
g commit -m "side change"
g switch master
echo m > m.txt
g add m.txt
at 3
g commit -m "main change"
echo staged > staged.txt
g add staged.txt
g merge side
g reset -q staged.txt
rm staged.txt
echo dirty >> f
g merge side
g checkout -- f
echo untracked > s.txt
g merge side
rm s.txt
g merge side
g status
