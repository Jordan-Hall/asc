#!/bin/sh
# U83: the build without the git protocol layer (SPEC 25.9, 25.10). The Cargo feature `git-compat`
# is off, so `asc serve` answers the disabled stub on the three git paths and no git protocol
# code is compiled. Run from the repository root. Both builds must pass in CI.
set -eu

cd "$(dirname "$0")/../.."

echo "== build without default features"
cargo build -p asc -p asc-serve --no-default-features

echo "== asc-git and asc-serve tests without default features"
cargo test -p asc-git -p asc-serve --no-default-features

echo "== the feature tree has no git-compat"
tree=$(cargo tree -e features -p asc --no-default-features)
if printf '%s\n' "$tree" | grep -q 'git-compat'; then
    echo "error: a git-compat line is in 'cargo tree -e features -p asc --no-default-features':" >&2
    printf '%s\n' "$tree" | grep 'git-compat' >&2
    exit 1
fi

echo "== a server from that build answers the stub for info/refs, git-upload-pack and git-receive-pack"
# `serve` is the one feature added back: it is the server, the stub is what it answers.
cargo test -p asc --no-default-features --features serve --test git_compat -- the_build_without_git_compat

echo "== no SSH library in the feature-off tree (U88)"
if cargo tree -p asc --no-default-features | grep -q russh; then
    echo "error: russh is in the dependency tree of asc --no-default-features" >&2
    exit 1
fi
if printf '%s\n' "$tree" | grep -q 'git-ssh'; then
    echo "error: a git-ssh line is in the feature tree of asc --no-default-features" >&2
    exit 1
fi

echo "== a git-compat build without git-ssh binds no SSH port (U88)"
cargo test -p asc --no-default-features --features serve,git-compat --test git_ssh -- a_git_compat_build_without_git_ssh

echo "no-git-compat: ok"
