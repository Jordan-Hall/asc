g init
mkdir s
echo 1 > a
echo 1 > b
echo 1 > s/c
g add .
at 1
g commit -m base
echo 2 > a
echo 2 > b
echo 2 > s/c
g add b
echo 3 > b
echo 3 > s/c
g stash push -- a s
g status --short
g log -n1 --format='%H %P' refs/stash
g ls-tree -r refs/stash
g ls-tree -r refs/stash^2
g stash pop
g status --short
g stash push -k
g status --short
g ls-tree -r refs/stash
g stash list
git_unused=1
g stash push -- nonexist
g stash push -m
g stash drop -q
g reset --hard
rm a
echo n > new1
g add new1
g stash push -- a
g status --short
g stash pop
g status --short
g stash push --keep-index -m "keep"
g status --short
g stash list
g reset --hard
g stash clear
mkdir sub
echo k > sub/keep
g add sub/keep
at 3
g commit -m sub
echo z > sub/z
g add sub/z
echo kk >> sub/keep
cd sub
g stash push -- .
cd ..
g status --short
g stash list
