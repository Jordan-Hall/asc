# `push --mirror` as git does it: every local ref goes to the remote with force, a remote ref with
# no local counterpart is deleted, the report lists the remote's refs and then the new ones, the
# options that choose refs exclude each other, and `remote.<name>.mirror` makes a bare push a
# mirror.
rg() { "${ASC_REAL_GIT:-git}" "$@"; }
rg init -q --bare ../up.git
g init
g remote add origin ../up.git
echo one > a.txt
g add a.txt
at 1
g commit -q -m one
g branch old
g push -q origin master old
g branch -D old
g branch feat
g tag v1

# The options that choose refs exclude each other, and a mirror takes no refspec.
g push --all --mirror origin
g push --mirror --tags origin
g push --mirror -d origin feat
g push --mirror origin master

g push --mirror origin
g push --mirror origin
g push --porcelain --mirror origin

# A rewritten branch is forced; a deleted tag goes from the remote too.
echo two > b.txt
g add b.txt
at 2
g commit -q --amend -m one-amended
g tag -d v1
g push --mirror origin
g branch -r
rg --git-dir=../up.git for-each-ref --format='%(refname)'

# remote.origin.mirror=true: a bare push mirrors, and a refspec is refused.
g config remote.origin.mirror true
g branch extra
g push origin
g push origin master
rg --git-dir=../up.git for-each-ref --format='%(refname)'
