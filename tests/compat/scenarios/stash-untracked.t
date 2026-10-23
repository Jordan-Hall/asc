g init
printf 'a\nb\nc\n' > f
g add .
at 1
g commit -m base
mkdir -p d/e
echo u > d/e/u.txt
echo v > v.txt
printf 'ignored.log\n' > .gitignore
g add .gitignore
at 2
g commit -m ignore
echo i > ignored.log
printf 'a\nB\nc\n' > f
g stash
g status --short
g stash pop
g stash -u
g status --short
ls
g log -n1 --format='%H %P' refs/stash
g log -n1 --format=%B refs/stash^3
g stash show --stat
g stash pop
g status --short
ls
g stash push -u -m "with untracked"
g stash list
echo other > v.txt
g stash apply
cat v.txt
ls d/e
g status --short
rm v.txt
g clean -fdq
g stash pop
g status --short
g stash push -u -- d
g status --short
g stash pop --index
g status --short
