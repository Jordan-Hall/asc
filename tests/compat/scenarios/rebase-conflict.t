g init
printf 'a\nb\nc\n' > f
echo base > k
g add .
at 1
g commit -m base
g switch -c side
printf 'a\nside\nc\n' > f
g add f
at 2
g commit -m "side f"
echo more > s.txt
g add s.txt
at 3
g commit -m "side s"
echo again >> s.txt
g add s.txt
at 4
g commit -m "side s2"
g switch master
printf 'a\nmain\nc\n' > f
g add f
at 5
g commit -m "main f"
g switch side
g rebase master
gd="$(git rev-parse --git-dir)"
for n in head-name onto orig-head stopped-sha author-script message end msgnum done git-rebase-todo; do
  echo "== $n"
  cat "$gd/rebase-merge/$n"
done
head -n 4 "$gd/rebase-merge/git-rebase-todo.backup"
cat "$gd/rebase-merge/patch"
cat "$gd/REBASE_HEAD" "$gd/ORIG_HEAD"
cat f
g rebase --continue
printf 'a\nres\nc\n' > f
g rebase --continue
g add f
at 6
g rebase --continue
g log --oneline
test -d "$gd/rebase-merge" && echo in-progress || echo finished
g reset --hard side
g rebase master
g rebase --skip
g log --oneline
g reset --hard side
g rebase master
g rebase --abort
g log --oneline
g rebase --abort
g rebase --continue
g rebase --skip
g rebase master
g rebase --quit
g rebase --quit
g log --oneline
# git writes its editor template (with status text) to COMMIT_EDITMSG on --continue; that text
# belongs to the status unit, so the file is not compared here.
rm -f "$gd/COMMIT_EDITMSG"
