g init
printf 'a\nb\nc\n' > f
echo keep > k
g add .
at 1
g commit -m base
g branch ahead
g switch -c side
printf 'a\nb\nside\n' > f
echo s > s.txt
g rm -q k
g add -A
at 2
g commit -m "side change"
echo more >> s.txt
at 3
g commit -am "side two"
g switch master
g merge side
g log --oneline
g merge side
g merge ahead
g switch -c other ahead
echo o > o.txt
g add o.txt
at 4
g commit -m other
g merge --ff-only side
g merge --no-ff -m "merge with message" side
g log --oneline --graph
g switch master
g reset --hard ahead
g merge --no-ff side
g log -n1 --format=%B
g status
g reset --hard ahead
g merge --squash side
g status
g commit -m squashed
g reset --hard ahead
g merge --no-commit --no-ff side
g status
g commit -m "committed by hand"
g log --oneline --graph
g reset --hard ahead
g merge --squash --no-commit side
g reset --hard ahead
g merge -q side
g log --oneline
g merge nosuch
g merge
