# Claude Code worktrees with asc

Claude Code makes a worktree for each agent. asc makes that cheap: a worktree is a few database
rows, and a virtual one writes no files until a path is read.

## With the hooks (recommended)

Add this to the repository's `.claude/settings.json`:

```json
{
  "hooks": {
    "WorktreeCreate": [
      { "hooks": [{ "type": "command", "command": "asc internal claude-hook worktree-create" }] }
    ],
    "WorktreeRemove": [
      { "hooks": [{ "type": "command", "command": "asc internal claude-hook worktree-remove" }] }
    ]
  }
}
```

- `worktree-create` reads the hook JSON from stdin (the worktree `name` and `cwd`), creates
  `<repo-root>/.claude/worktrees/<name>` on the branch `agent/<name>` from the current `HEAD`,
  ephemeral for seven days, and prints the absolute path as the only line of stdout. Errors go to
  stderr with a non-zero exit.
- `worktree-remove` reads `worktree_path` and removes that worktree (`worktree remove --force`)
  when the hook made it. Unpushed commits stay reachable from `refs/heads/agent/<name>`.
- The JSON is untrusted: it is size-limited, the name must be one plain path component (letters,
  digits, `.`, `_`, `-`), and only a registered worktree is ever removed.
- Spike S7 confirms the JSON field names against the pinned Claude Code version. The hook accepts
  `name` or `worktree_name`, and `worktree_path` or `path`.

## Without the hooks

Claude Code runs `git worktree add` and `git worktree remove`. With the shim first on `PATH`
(see `docs/ONBOARDING.md`) those reach asc and are native. Because `.claude/worktrees/<name>`
lies inside the main workspace, asc mounts the new workspace under `$ASC_HOME/wt/<ws-id>` and puts
a directory symlink or junction at the requested path.

## Housekeeping

- Agent worktrees are ephemeral: the daemon removes expired clean ones hourly. A dirty one is
  kept and `asc status` warns.
- Agent branches are `agent/<name>`; they push with `-o asc.ttl=<dur>` and expire.
- On Windows Claude Code uses Git Bash, so the `# asc shim` block the installer adds to `~/.bashrc` matters.
- Run `asc doctor` if a hook fails: it reports where `git` resolves in each shell.
