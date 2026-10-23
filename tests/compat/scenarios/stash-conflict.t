g init
printf 'a\nb\nc\n' > f
echo 1 > g1
echo 1 > h
g add .
at 1
g commit -m base
printf 'a\nS\nc\n' > f
echo S > g1
g stash
printf 'a\nT\nc\n' > f
at 2
g commit -am T
g stash pop
g status --short
cat f
g stash list
g reset --hard
g stash pop
g stash list
echo 9 > h
g stash apply
echo 1 > h
g checkout -q -- .
printf 'a\nS\nc\n' > f
g stash apply
g checkout -q -- f
g status --short
g reset --hard
g stash drop
echo staged > g1
g add g1
echo wt >> g1
g stash
g stash apply --index
g status --short
g reset --hard
g stash apply
g status --short
g stash pop -q
g stash list
