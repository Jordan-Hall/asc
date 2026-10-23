g init
printf 'a\nb\nc\n' > f
g add .
at 1
g commit -m base
g stash create
printf 'a\nB\nc\n' > f
at 2
g stash create
g stash create "created msg" > /dev/null
id=$(git stash create)
g stash store
g stash store -m "stored" "$id"
g stash store "$id"
g stash store HEAD
g stash list
g status --short
g stash branch nb
g status --short
g branch
g stash list
git_unused=1
printf 'a\nC\nc\n' > f
at 3
g stash
g checkout -q master
g stash branch nb2 stash@{0}
g log --oneline
g stash branch
