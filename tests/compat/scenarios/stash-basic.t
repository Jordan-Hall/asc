g init
printf 'a\nb\nc\n' > f
echo x > g1
g add .
at 1
g commit -m base
g stash
g stash list
g stash create
g stash pop
g stash drop
g stash show
printf 'a\nB\nc\n' > f
g add f
printf 'a\nB\nc\nd\n' > f
echo new > n.txt
g add n.txt
echo u > u.txt
at 2
g stash push -m "my msg"
g status --short
g stash list
g rev-parse refs/stash
g log -n1 --format='%H %P%n%B' refs/stash
g log -n1 --format=%B refs/stash^2
g stash show
g stash show -p
g stash show stash@{0}
at 3
g stash apply
g status --short
g stash apply
g reset --hard
g stash pop
g status
g stash list
g stash pop
g stash -q
g stash list
g stash save legacy message
g stash list
printf 'one\n' > g1
at 4
g stash
printf 'two\n' > g1
at 5
g stash
g stash list --format=%s
g stash list --pretty=%gd:%H -z
g stash drop stash@{1}
g stash list
g rev-parse refs/stash
g stash drop stash@{5}
g stash drop nonsense
g stash apply nonsense
g stash weird
g stash clear
g stash list
g stash clear
g stash pop
g stash apply
g stash drop
g rev-parse --verify refs/stash
