# compat: branchless=both
# fidelity: D-5
# Pushing tags in both modes (SPEC 21.4.2, 21.1 BD7; divergence D-5). The remote is a bare git
# repository, which has no ref policy, so here both modes must match git byte for byte; the rejection
# of a tag outside `shared_refs` is a server policy (B08a) and its client mapping (B12) adds the
# `allowed.toml` entry for D-5 that covers the push lines. The local commands are identical in both
# modes, and `asc.branchless=false` is git's behaviour.
rg() { "${ASC_REAL_GIT:-git}" "$@"; }
rg init -q --bare ../up.git
g init
g remote add origin ../up.git
echo one > a.txt
g add a.txt
at 1
g commit -m one
g push origin master
g tag v1
g tag -a v2 -m "release two"
g push origin v1
g push origin v2
g push origin v1
g tag v3 HEAD
g push origin --tags
g push origin :refs/tags/v1
g tag -l
g ls-remote --tags origin
rg --git-dir=../up.git for-each-ref
