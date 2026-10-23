# rebase -i --update-refs: branches on the rebased commits follow them.
g init
gd="$(git rev-parse --git-dir)"
echo base > f
g add .
at 1
g commit -m base
g switch -c side
for i in 1 2 3 4; do
  echo "$i" > "m$i"
  g add "m$i"
  at "$((i + 1))"
  g commit -m "m$i"
done
g branch mid side~2
g branch low side~3
g branch same side
g switch master
echo x > x
g add x
at 7
g commit -m "main x"
g switch side
g tag orig
export GIT_EDITOR=true
unset GIT_SEQUENCE_EDITOR
g rebase -i --update-refs master
g for-each-ref
g log --oneline --decorate
# A stop in the middle keeps the list of refs.
g reset --hard orig
g branch -f mid orig~2
g branch -f low orig~3
export GIT_SEQUENCE_EDITOR="sed -i -e '/ # m3$/s/^pick/edit/'"
g rebase -i --update-refs master
cat "$gd/rebase-merge/update-refs"
cat "$gd/rebase-merge/done"
g for-each-ref
g rebase --continue
g for-each-ref
# The configuration turns it on for -i, --no-update-refs turns it off.
g reset --hard orig
g branch -f mid orig~2
g branch -f low orig~3
g config rebase.updateRefs true
unset GIT_SEQUENCE_EDITOR
g rebase -i --no-update-refs master
g for-each-ref
g reset --hard orig
g branch -f mid orig~2
g branch -f low orig~3
g rebase -i master
g for-each-ref
# With --exec the update-ref lines follow the exec lines.
g reset --hard orig
g branch -f mid orig~2
g branch -f low orig~3
g rebase -i --exec "echo ok" master
g for-each-ref
# A branch that moved meanwhile is left alone.
g reset --hard orig
g branch -f mid orig~2
g branch -f low orig~3
export GIT_SEQUENCE_EDITOR="sed -i -e '/ # m3$/s/^pick/edit/'"
g rebase -i master
g update-ref refs/heads/low master
g rebase --continue
g for-each-ref
