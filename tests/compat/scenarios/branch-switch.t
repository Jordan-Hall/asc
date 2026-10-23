g init
echo one > a.txt
g add a.txt
at 1
g commit -m one
g branch
g branch feature
g branch -v
g branch --list
g branch -a
g switch feature
g status
echo two >> a.txt
at 2
g commit -am two
g switch master
g checkout -b topic
g branch
at 3
g commit --allow-empty -m empty
g switch -
g switch -
g checkout master
g checkout feature
g branch -m feature renamed
g branch
g branch --show-current
g switch -c other master
g branch -vv
g switch --detach HEAD
g status
g branch --show-current
g switch master
g branch -d topic
g branch -D topic
g branch -d renamed
g branch --merged
g branch --no-merged
g branch -f other HEAD
g switch nosuch
g branch -d nosuch
g checkout -b other
g branch -c other copy
g branch
