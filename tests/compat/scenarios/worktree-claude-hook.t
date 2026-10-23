# compat: asc-only
# The Claude Code WorktreeCreate and WorktreeRemove hooks (SPEC §9.3).
set -e
git init
echo one > a.txt
git add a.txt
git commit -m first
here="$(wpwd)"
printf '{"session_id":"s1","hook_event_name":"WorktreeCreate","cwd":"%s","name":"bold-fox"}' "$here" > create.json
asc internal claude-hook worktree-create < create.json > out.txt 2> err.txt
test ! -s err.txt
test "$(wc -l < out.txt | tr -d ' ')" = "1"
wt="$(cat out.txt)"
case "$wt" in *.claude?worktrees?bold-fox) ;; *) echo "bad path: $wt" >&2; exit 1;; esac
test -f "$wt/a.txt"
git -C "$wt" rev-parse --abbrev-ref HEAD | grep -qx agent/bold-fox
git branch --list agent/bold-fox | grep -q agent/bold-fox
# The parent ignores .claude/worktrees/ (git init adds it to the exclude file) ...
git status --porcelain | grep -v '^?? ' && exit 1
! git status --porcelain | grep -q '\.claude'
# ... and, with that line gone, sees the worktree as one untracked entry.
: > "$(git rev-parse --git-common-dir)/info/exclude"
git status --porcelain | grep -qx '?? \.claude/'
# A hostile name is refused before anything is made.
printf '{"cwd":"%s","name":"../evil"}' "$here" > bad.json
if asc internal claude-hook worktree-create < bad.json > bad-out.txt 2> bad-err.txt; then exit 1; fi
test ! -s bad-out.txt
test -s bad-err.txt
# Remove with the path from the hook JSON; the branch stays.
wtj="$(printf '%s' "$wt" | tr '\' '/')"
printf '{"hook_event_name":"WorktreeRemove","cwd":"%s","worktree_path":"%s"}' "$here" "$wtj" > remove.json
asc internal claude-hook worktree-remove < remove.json > rm-out.txt 2> rm-err.txt
test ! -s rm-out.txt
test ! -e "$wt"
git branch --list agent/bold-fox | grep -q agent/bold-fox
# A path that is not a worktree is an error, and nothing outside is touched.
mkdir keep && echo x > keep/f
printf '{"cwd":"%s","worktree_path":"%s/keep"}' "$here" "$here" > evil.json
if asc internal claude-hook worktree-remove < evil.json 2> evil-err.txt; then exit 1; fi
test -f keep/f
# --- The hook input is untrusted: only a worktree the create hook made is force-removed. ---
hook_remove() {
  printf '{"hook_event_name":"WorktreeRemove","cwd":"%s","worktree_path":"%s"}' "$2" "$1" \
    | asc internal claude-hook worktree-remove > hook-out.txt 2> hook-err.txt
}
hook_create() {
  printf '{"hook_event_name":"WorktreeCreate","cwd":"%s","name":"%s"}' "$here" "$1" > c.json
  asc internal claude-hook worktree-create < c.json | tr '\' '/'
}
# A dirty, hand-made worktree in the agent directory survives (no force), as does its work.
git worktree add .claude/worktrees/hand -b hand-made
echo precious > .claude/worktrees/hand/work.txt
if hook_remove "$here/.claude/worktrees/hand" "$here"; then exit 1; fi
test "$(cat .claude/worktrees/hand/work.txt)" = "precious"
git worktree list | grep -q 'worktrees/hand'
# ... but a clean hand-made one there may go, without force.
rm .claude/worktrees/hand/work.txt
hook_remove "$here/.claude/worktrees/hand" "$here"
test ! -e .claude/worktrees/hand
# A worktree outside .claude/worktrees is refused, even one marked ephemeral and clean.
git worktree add --ephemeral ../outside-wt -b outside-b
outside="$(cd ../outside-wt && wpwd)"
echo precious > ../outside-wt/work.txt
if hook_remove "$outside" "$here"; then exit 1; fi
grep -q 'not directly under' hook-err.txt
test "$(cat ../outside-wt/work.txt)" = "precious"
rm ../outside-wt/work.txt
if hook_remove "$outside" "$here"; then exit 1; fi
test -f ../outside-wt/a.txt
git worktree list | grep -q 'outside-wt'
# A cwd from another repository is ignored.
mkdir ../other-repo
(cd ../other-repo && git init)
there="$(cd ../other-repo && wpwd)"
far="$(hook_create far-fox)"
hook_remove "$far" "$there"
test ! -e "$far"
# So is one that is not an ancestor of the worktree path.
near="$(hook_create near-fox)"
hook_remove "$near" "$here/keep"
test ! -e "$near"
# A gitfile an agent repointed at another repository is refused: nothing runs there, and the
# worktree and its work stay.
echo o > ../other-repo/o.txt
git -C ../other-repo add o.txt
git -C ../other-repo commit -m other
echo more > ../other-repo/untracked.txt
test -f ../other-repo/.git
mkdir ../other-hooks
for h in post-commit post-checkout; do
  printf '#!/bin/sh
echo ran > "%s/other-ran-%s.txt"
' "$here" "$h" > ../other-hooks/$h
  chmod +x ../other-hooks/$h
done
git -C ../other-repo config core.hooksPath "$(cd ../other-hooks && wpwd)"
evil="$(hook_create evil-fox)"
echo mine > "$evil/work.txt"
cp "$evil/.git" evil-git.bak
cp ../other-repo/.git "$evil/.git"
if hook_remove "$evil" "$here"; then exit 1; fi
test "$(cat "$evil/work.txt")" = "mine"
test ! -e other-ran-post-commit.txt
test ! -e other-ran-post-checkout.txt
test -z "$(git -C ../other-repo for-each-ref refs/asc/)"
# With the gitfile put back, the same request works.
cp evil-git.bak "$evil/.git"
hook_remove "$evil" "$here"
test ! -e "$evil"
git worktree remove --force ../outside-wt
# A dirty hook-created worktree is saved on refs/asc/agent-trash before it is force-removed.
dirty="$(hook_create dirty-fox)"
echo mine > "$dirty/work.txt"
echo changed > "$dirty/a.txt"
mkdir -p .hooks
for h in post-commit post-checkout; do
  printf '#!/bin/sh
echo ran > "%s/snap-ran-%s.txt"
' "$here" "$h" > .hooks/$h
  chmod +x .hooks/$h
done
git config core.hooksPath "$here/.hooks"
hook_remove "$dirty" "$here"
test ! -e "$dirty"
grep -q 'refs/asc/agent-trash/dirty-fox/' hook-err.txt
snap="$(git for-each-ref --format='%(refname)' refs/asc/agent-trash/dirty-fox/)"
test -n "$snap"
test "$(git show "$snap:work.txt")" = "mine"
test "$(git show "$snap:a.txt")" = "changed"
test "$(git show "agent/dirty-fox:a.txt")" = "one"
# No repository hook runs during the snapshot.
test ! -e snap-ran-post-commit.txt
test ! -e snap-ran-post-checkout.txt
