# asc cheat sheet

Common tasks and the asc command for each. `asc --help` lists everything; [`CLI.md`](CLI.md) has an
example for every command. Output, `--porcelain`, `-z` and `--format` are stable for scripts.

## Everyday tasks

| Task | Command |
|---|---|
| Get a repository | `asc clone <url>`. Instant and virtual: no files are downloaded up front. `--mode plain` writes real files. `--reference <old clone>` borrows an existing clone's objects. |
| Another working copy | `asc worktree add <path>`. Instant and virtual. `--ephemeral --ttl 7d` makes a throwaway worktree that the daemon removes when it is clean and expired. |
| Update | `asc fetch` / `asc pull`. Downloads no files, only what is missing when a path is read. |
| Record work | `asc commit`, or `asc create -m "..."` for a change in a stack. One cheap local row; the daemon uploads new work in the background. |
| Share work | `asc push`. In a branchless repo (the default) `asc push origin feat` also submits the stack as draft changes. `asc submit -s` sends the whole stack. |
| Land work | `asc push --land` (or `-o land`): the server rebases onto `master` when the paths do not conflict. |
| Branches | In a branchless repo a branch is a local label. Only `master`, release branches and tags are shared refs. |
| Undo | `asc undo`; `asc op log` shows the operation log. |
| Nested repositories | Composed read-only from the nested repository; nothing to update by hand. |
| Large files | Plain blobs. No extra tool is needed. |
| Repository upkeep | None: there are no packfiles to maintain. |
| Editor integration | ASC's VS Code and JetBrains plugins use the asc API. |

## asc-only commands

| Command | Use |
|---|---|
| `asc hide` | Hide a draft change. |
| `asc affected` | List the projects and targets a change touches. |
| `asc projects`, `asc graph` | The project graph. |
| `asc landed` | Whether a change has landed on `master`. |
| `asc doctor` | Check the install; `asc doctor --fix` repairs what it can. |
| `asc doctor --report-reclaimable` | Show disk an old `.git`, old worktrees and cruft packs hold. It deletes nothing. |
| `asc install-shim` | Optional: put a `git` shim beside `asc` and print the PATH line. |
| `asc help git` | Print the compatibility command map. |

## Rules of thumb

- The default branch is `master`, never `main`.
- A flag asc does not serve fails with usage exit code 129; it is never ignored.
- Nothing prompts: credentials come from your credential helpers or `ASC_TOKEN`.

## Git compatibility

Existing tools and CI that still speak git keep working: aliases, hooks, `.gitignore` and
`.gitattributes` are honoured, and with the optional shim (`asc install-shim`) `git` is asc inside an
asc workspace and real git everywhere else. `asc help git` prints the command map.
