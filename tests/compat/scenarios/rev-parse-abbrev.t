# An abbreviated commit id names its commit in rev-parse, log, switch and rebase todo lists, as
# in git; a ref of the same name wins over the id, and a prefix of nothing is unknown.
g init
for i in 1 2 3; do
  echo "$i" > "f$i"
  g add "f$i"
  at "$i"
  g commit -m "c$i"
done
short=$(git rev-parse --short=7 HEAD~1)
g rev-parse "$short"
g rev-parse "$short~1" "$short^"
g log -1 --format=%s "$short"
g cat-file -t "$short"
g switch -q --detach "$short"
g log -1 --format=%s
g switch -q master
four=$(git rev-parse --short=4 HEAD)
g branch "$four" HEAD~2
g rev-parse "$four"
g rev-parse 0000000
