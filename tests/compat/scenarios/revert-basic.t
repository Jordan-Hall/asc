g init
printf 'a\nb\nc\n' > f
g add .
at 1
g commit -m base
printf 'a\nb\nc\nd\n' > f
echo new > n.txt
g add -A
at 2
g commit -m "add d and n"
echo later > later.txt
g add later.txt
at 3
g commit -m later
g revert HEAD~1
g log -n1 --format=%B
g log --oneline
g revert HEAD
g log -n1 --format=%B
g reset --hard HEAD~2
g revert -n HEAD~1
g status
cat "$(git rev-parse --git-dir)/MERGE_MSG"
g commit -m "by hand"
g reset --hard HEAD~1
printf 'a\nb\nc2\nd\n' > f
at 4
g commit -am "edit c"
printf 'a\nb\nc3\nd\n' > f
at 5
g commit -am "edit c again"
g revert HEAD~1
g status
cat f
g revert --abort
g status
g revert HEAD~1
printf 'a\nb\nresolved\nd\n' > f
g add f
at 6
g revert --continue
g log --oneline
g status
g revert --skip
g revert
g revert HEAD~2
g revert --quit
g reset --hard HEAD
g status
