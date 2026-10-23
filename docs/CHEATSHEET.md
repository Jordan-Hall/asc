# asc cheat sheet: git commands in asc

Keep using your git commands. With the shim installed (`asc install-shim`), `git` is asc inside a
asc workspace and real git everywhere else. `asc <cmd>` does the same thing. Output, flags, exit
codes, `--porcelain`, `-z` and `--format` match git.

## What is different

| You type | What happens in asc |
|---|---|
| `git clone <url>` | Instant and virtual: no files are downloaded up front. `--mode plain` writes real files. `--reference <git-dir>` borrows an old clone's objects. |
| `git worktree add <path>` | Instant and virtual. `--ephemeral --ttl 7d` makes a throwaway worktree that the daemon removes when it is clean and expired. |
| `git fetch` / `git pull` | Downloads no files, only what is missing when a path is read. |
| `git push` | Exactly git's push. In a branchless repo (the default) `git push origin feat` also submits the stack as draft changes. |
| `git push --land` (or `-o land`) | The server rebases onto `master` when the paths do not conflict. |
| `git commit` | One cheap local row. The daemon uploads new commits in the background. |
| `git branch` | In a branchless repo a branch is a local label. Only `master`, release branches and tags are shared refs. |
| `git submodule update` | Not needed: submodules are composed read-only from the nested repository. |
| `git lfs ...` | Not needed and not supported: big files are plain blobs. |
| `git gc`, `repack`, `prune`, `maintenance` | No-ops: there are no packfiles to maintain. |
| IDE git panels | Not supported. ASC's VS Code and JetBrains plugins use the asc API. |

## asc-only commands

| Command | Use |
|---|---|
| `asc hide` | Hide a draft change. |
| `asc affected` | List the projects and targets a change touches. |
| `asc projects`, `asc graph` | The project graph. |
| `asc landed` | Whether a change has landed on `master`. |
| `asc doctor` | Check the install; `asc doctor --fix` repairs what it can. |
| `asc doctor --report-reclaimable` | Show disk an old `.git`, old worktrees and cruft packs hold. It deletes nothing. |
| `asc install-shim` | Put `git` beside `asc` and print the PATH line. |
| `asc help git` | Print this sheet. |

## Rules of thumb

- The default branch is `master`, never `main`.
- Aliases (`alias.*`), `git-<name>` programs, hooks, `.gitignore`, `.gitattributes`, git config
  and credential helpers keep working.
- A flag asc does not serve fails with git's usage exit code 129; it is never ignored.
- Nothing prompts: credentials come from your git credential helpers, as with git.
