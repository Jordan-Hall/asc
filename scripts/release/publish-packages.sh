#!/bin/sh
# Publishes the package-manager manifests of an assembled release (assemble.py) to where the package managers
# read them, through the GitHub API (`gh api`, no clone):
#
#   winget    winget-asc.asc*.yaml -> a pull request to $WINGET_UPSTREAM (default microsoft/winget-pkgs) from a
#             branch of the fork $WINGET_FORK, at manifests/a/asc/asc/<version>/asc.asc*.yaml
#   scoop     asc.scoop.json       -> bucket/asc.json in the bucket repository $SCOOP_BUCKET
#   homebrew  asc.rb               -> Formula/asc.rb in the tap repository $HOMEBREW_TAP
#
#   sh scripts/release/publish-packages.sh <release-dir> <version> [winget|scoop|homebrew ...]
#
# With no target named, every target whose repository variable is set is published and the others are reported
# as skipped. GH_TOKEN must be able to write to those repositories (and to open the winget pull request). A file
# that already holds the same bytes is left alone, so a re-run publishes nothing twice. ASC_GH replaces `gh`
# (the tests use a stand-in).
set -eu

GH=${ASC_GH:-gh}
WINGET_UPSTREAM=${WINGET_UPSTREAM:-microsoft/winget-pkgs}

die() { echo "publish-packages: $*" >&2; exit 1; }
say() { echo "publish-packages: $*"; }

[ $# -ge 2 ] || die "usage: publish-packages.sh <release-dir> <version> [winget|scoop|homebrew ...]"
dir=$1
version=$2
shift 2
printf '%s' "$version" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' || die "bad version: $version"
[ -d "$dir" ] || die "no release directory: $dir"

repo_ok() { printf '%s' "$1" | grep -Eq '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$'; }

b64() { base64 < "$1" | tr -d '\n'; }

# put <repo> <branch> <path> <file> <message>: creates or replaces one file on a branch; nothing when the bytes
# are already there.
put() {
  repo=$1 branch=$2 path=$3 file=$4 message=$5
  want=$(b64 "$file")
  sha=$("$GH" api "repos/$repo/contents/$path?ref=$branch" --jq .sha 2>/dev/null || true)
  if [ -n "$sha" ]; then
    have=$("$GH" api "repos/$repo/contents/$path?ref=$branch" --jq .content | tr -d '\n')
    if [ "$have" = "$want" ]; then
      say "$repo:$path is current"
      return 0
    fi
    "$GH" api -X PUT "repos/$repo/contents/$path" -f message="$message" -f content="$want" -f branch="$branch" -f sha="$sha" >/dev/null
  else
    "$GH" api -X PUT "repos/$repo/contents/$path" -f message="$message" -f content="$want" -f branch="$branch" >/dev/null
  fi
  say "wrote $repo:$path on $branch"
}

need() { [ -f "$dir/$1" ] || die "the release has no $1"; }

publish_scoop() {
  need asc.scoop.json
  branch=$("$GH" api "repos/$SCOOP_BUCKET" --jq .default_branch)
  put "$SCOOP_BUCKET" "$branch" bucket/asc.json "$dir/asc.scoop.json" "asc $version"
}

publish_homebrew() {
  need asc.rb
  branch=$("$GH" api "repos/$HOMEBREW_TAP" --jq .default_branch)
  put "$HOMEBREW_TAP" "$branch" Formula/asc.rb "$dir/asc.rb" "asc $version"
}

publish_winget() {
  for f in winget-asc.asc.yaml winget-asc.asc.installer.yaml winget-asc.asc.locale.en-GB.yaml; do need "$f"; done
  upstream_branch=$("$GH" api "repos/$WINGET_UPSTREAM" --jq .default_branch)
  head=$("$GH" api "repos/$WINGET_UPSTREAM/git/ref/heads/$upstream_branch" --jq .object.sha)
  branch="asc.asc-$version"
  owner=${WINGET_FORK%%/*}
  open=$("$GH" api "repos/$WINGET_UPSTREAM/pulls?head=$owner:$branch&state=open" --jq length)
  if [ "$open" != 0 ]; then
    say "the winget pull request for $version is already open"
    return 0
  fi
  # The branch starts at the upstream tip, so the pull request carries the three files only.
  if "$GH" api "repos/$WINGET_FORK/git/ref/heads/$branch" --jq .object.sha >/dev/null 2>&1; then
    "$GH" api -X PATCH "repos/$WINGET_FORK/git/refs/heads/$branch" -f sha="$head" -F force=true >/dev/null
  else
    "$GH" api -X POST "repos/$WINGET_FORK/git/refs" -f ref="refs/heads/$branch" -f sha="$head" >/dev/null
  fi
  base="manifests/a/asc/asc/$version"
  for f in asc.asc.yaml asc.asc.installer.yaml asc.asc.locale.en-GB.yaml; do
    put "$WINGET_FORK" "$branch" "$base/$f" "$dir/winget-$f" "New version: asc.asc version $version"
  done
  "$GH" api -X POST "repos/$WINGET_UPSTREAM/pulls" -f title="New version: asc.asc version $version" \
    -f head="$owner:$branch" -f base="$upstream_branch" \
    -f body="asc $version: the portable zip of the signed release. Manifests made by the asc release workflow." >/dev/null
  say "opened the winget pull request for $version"
}

targets=$*
[ -n "$targets" ] || targets="winget scoop homebrew"
for t in $targets; do
  case "$t" in
    winget) var=${WINGET_FORK:-} name=WINGET_FORK ;;
    scoop) var=${SCOOP_BUCKET:-} name=SCOOP_BUCKET ;;
    homebrew) var=${HOMEBREW_TAP:-} name=HOMEBREW_TAP ;;
    *) die "unknown target: $t" ;;
  esac
  if [ -z "$var" ]; then
    [ $# -gt 0 ] && die "$name is not set"
    say "skip $t ($name is not set)"
    continue
  fi
  repo_ok "$var" || die "$name is not owner/name: $var"
  "publish_$t"
done
