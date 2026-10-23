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
