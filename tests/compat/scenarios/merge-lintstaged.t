g init
printf 'a\nb\nc\n' > f
g add .
at 1
g commit -m base
g switch -c side
printf 'a\nb\nside\n' > f
g commit -am "side change"
g switch master
printf 'a\nb\nmain\n' > f
at 3
g commit -am "main change"
g merge side
gd="$(git rev-parse --git-dir)"
mkdir -p backup
for n in MERGE_HEAD MERGE_MODE MERGE_MSG; do
  if [ -f "$gd/$n" ]; then cp "$gd/$n" "backup/$n"; fi
done
rm -f "$gd/MERGE_HEAD" "$gd/MERGE_MODE" "$gd/MERGE_MSG"
printf 'a\nb\nresolved\n' > f
g add f
for n in MERGE_HEAD MERGE_MODE MERGE_MSG; do
  if [ -f "backup/$n" ]; then cp "backup/$n" "$gd/$n"; fi
done
rm -rf backup
g status
at 4
g commit --no-edit
g log --oneline --graph
g cat-file -p HEAD
