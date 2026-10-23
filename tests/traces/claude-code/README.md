# Claude Code traces

Claude Code runs git for its session context, for the commands the model sends through its Bash
tool, and for `EnterWorktree` / `ExitWorktree`. These traces pin the invocations the native table
(docs/SPEC.md §11.5) and the compat gate (§17.1) must serve.

| Trace | Driver |
|---|---|
| `session-context` | Session start (branch, default branch, user, status, last commits) and the `/commit` skill's context block. |
| `edit-commit-push` | Edit, review (`status`, `diff`, `log`), stage, multi-line commit, push with an upstream to a bare origin. |
| `worktree-default` | `EnterWorktree` / `ExitWorktree` with no hook configured: Claude Code runs `git worktree add` and `git worktree remove` itself. |
| `worktree-git-hooks` | The same flow in a repository with a `post-checkout` hook, which git runs in the new work tree. |
| `worktree-hook-commands` | `WorktreeCreate` and `WorktreeRemove` hooks configured: Claude Code feeds the hook JSON on stdin and reads the path from stdout; a hook script makes the git calls. |

## Versions

Claude Code 2.1.296 (`claude --version`), git 2.55. The pin is in `../pins.toml`.

## Provenance

All five traces are **authored**, not recorded. Driving Claude Code needs a model session, which
the recorder does not start. The argv come from the git argument templates in the 2.1.296 build
(for example `git worktree add --no-track -B <branch> <path> <base>` with `LC_ALL=C`, and
`git --no-optional-locks status --porcelain`). One constant that the build appends to its
`status --short` call is not known and is left out.

Spike S7 (docs/SPEC.md §19) replaces them: run the pinned build under `GIT_TRACE2_EVENT` with the
conventions of `record/README.md`, confirm the `WorktreeCreate` / `WorktreeRemove` JSON field
names and the stdout contract, and change `provenance` to `recorded`. The two `hook_event` steps
in `worktree-hook-commands` are the contract unit U54w implements; the git runner skips them.
