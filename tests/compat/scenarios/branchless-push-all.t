# compat: branchless=both
# fidelity: D-6
# `push --all` in both modes (SPEC 21.4.2, 21.1 BD7; divergence D-6). The remote is a bare git
# repository, so here both modes must match git byte for byte; that branchless `--all` pushes user
# refs and submits no stacks (the `remote: asc: --all does not submit stacks` line) is a server
# policy (B08a) and its client mapping (B12) adds the `allowed.toml` entry for D-6. The local
# commands are identical in both modes, and `asc.branchless=false` is git's behaviour.
rg() { "${ASC_REAL_GIT:-git}" "$@"; }
rg init -q --bare ../up.git
g init
g remote add origin ../up.git
echo one > a.txt
g add a.txt
at 1
g commit -m one
g branch feat-a
g branch feat-b
g switch feat-a
echo two > b.txt
g add b.txt
at 2
g commit -m two
g switch master
g push --all origin
g push --all origin
g switch feat-b
echo three > c.txt
g add c.txt
at 3
g commit -m three
g push --all origin
g branch -vv
g branch -r
rg --git-dir=../up.git for-each-ref
