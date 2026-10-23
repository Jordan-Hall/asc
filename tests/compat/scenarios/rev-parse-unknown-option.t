# fidelity: Q-2
# rev-parse echoes a token it does not know instead of rejecting it: scripts split "flags meant
# for rev-list" from revisions that way. Pinned git prints the token and exits 0; so must asc.
g init
echo one > a.txt
g add a.txt
at 1
g commit -m one
g rev-parse --no-such-flag
g rev-parse HEAD --no-such-flag
g rev-parse --no-such-flag HEAD
g rev-parse --no-such-flag=value -x
