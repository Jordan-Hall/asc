# asc onboarding

How to install asc, set up your editor, and fix what `asc doctor` reports. Common tasks and their
commands are in `docs/CHEATSHEET.md`. SPEC wins where this page disagrees.

## 1. Install

- Windows: the MSI installs `asc.exe`, a private shim directory, the `PATH` entries, the
  `Client-ProjFS` feature and Defender exclusions for `$ASC_HOME`, and registers the daemon task.
- Linux: `packaging/linux/install.sh` (shipped with each release as `install.sh`); the daemon runs as a user service. Linux and macOS also install with `scripts/install.sh`; see `docs/RELEASE.md`.
- Step-by-step setup and sign-in: `docs/GETTING-STARTED.md`.
- The compatibility layer and import need git 2.40 or later installed. asc finds it from
  `ASC_REAL_GIT`, then `asc.realGit`, then the first other `git` on `PATH`, then (Windows) Git for Windows.
- Run `asc doctor` after installing.

## 2. Git compatibility: the optional `git` shim

Only needed for existing tools, scripts and CI that still run `git`. `asc install-shim` puts `git[.exe]` in `<asc dir>/shim` and prints the `PATH` line. Inside an asc
workspace `git` is asc; anywhere else it is real git, with identical arguments, environment and
stdio. asc children (hooks, editors) get the right `PATH` without any of this.

| Surface | What to do |
|---|---|
| cmd.exe and PowerShell | The shim directory must come before Git for Windows on `PATH`. An elevated MSI inserts it into the machine `PATH`; a non-elevated install cannot shadow git and says so. |
| Git Bash (what Claude Code uses on Windows) | Git Bash puts `/mingw64/bin` first. The installer adds `export PATH="<shimdir>:$PATH"` to `~/.bashrc` inside a `# asc shim` block. Add it yourself if you installed by hand. |
| Turborepo and other tools | They run `git` from their own environment. Check with `asc doctor`; its `git@turbo (this environment)` line shows what they will see. |
| Linux and macOS | Put the shim directory first in `PATH` in your shell profile. |

IDE git panels are replaced: ASC's VS Code and JetBrains plugins use the asc API
instead.

## 3. First clone and reclaiming disk

```
asc clone https://forge.example/Owner/repo D:\work\repo
asc clone https://forge.example/Owner/repo D:\work\repo --reference D:\old\repo\.git
asc doctor --report-reclaimable
```

`--reference` borrows objects from an existing clone's directory so the first fetch is small.
`asc doctor --report-reclaimable` only measures an old `.git`, `worktrees/`, `modules/`, `lfs/`
and cruft packs. It never deletes: delete them yourself once you trust the new clone.

## 4. Editor settings

Large trees full of build output slow editors down. Write these once at workspace scope and
check them in:

- VS Code: `files.watcherExclude`, `search.exclude` and `files.exclude` for `**/node_modules`,
  `**/dist`, `**/.turbo`, `**/target`, `**/.next` and `**/.asc`, and `search.followSymlinks: false`.
- JetBrains: mark the same folders as excluded (`DirectoryIndexExcludePolicy`); `.ascignore` is
  honoured too.
- Keep source roots indexed: language servers and indexers hydrate sources on demand, and a
  clone or worktree prefetches the usual source set.

## 5. Branches in asc: the branch-scope model

Local branches and tags are rows on your machine, shared by all workspaces on it, and reach the
server only through `push`. In a branchless repo (the default) the only shared refs are `master`,
release branches and tags. Other work is draft changes: new commits upload in the background,
`asc push origin feat` also submits the stack as draft changes and the pushed branch becomes
your own user ref, and `asc hide` hides a draft. Agent branches (`agent/<name>`) expire.
As in git, a branch checked out in one worktree cannot be checked out in another. Per-repo and
per-user switches turn the background upload off; local-only mode (`asc init`) works with no
server.

## 6. Compatibility notes

- Attributes come from `.ascattributes` if present, else `.gitattributes`; the files are never
  merged. `text`, `text=auto` and `eol=*` normalise CRLF to LF on `add` as git does. `filter=`
  drivers (clean/smudge), `ident` and `working-tree-encoding` are not supported and are reported
  by `asc doctor` (`gitattributes`).
- Ignore files: `.ascignore` first, else `.gitignore`.
- `clone --depth` and `--filter` are accepted but cloning is always lazy.
- No LFS, no `submodule update`, no packfile maintenance (see the cheat sheet).
- The default branch is `master`.
- Under the name `git`, an asc-only verb runs only where git would say "not a git command"; an
  alias or a `git-<name>` program wins.

## 7. Claude Code agents

See `docs/agents/claude-code-worktrees.md` for the `WorktreeCreate` and `WorktreeRemove` hooks
and the `.claude/settings.json` snippet.

## 8. Troubleshooting by `asc doctor` finding

Each line is `ok|info|warn|fail <name>: <detail>`; only `fail` makes the exit code 1. Run
`asc doctor --fix` for what can be repaired without administrator rights.

| Finding | Meaning and fix |
|---|---|
| `real-git` | No usable real git. Install git 2.40 or later, or set `asc.realGit` or `ASC_REAL_GIT`. |
| `shim-dir` | The shim directory is missing or dangling. `asc doctor --fix` or `asc install-shim`. |
| `git@cmd`, `git@powershell`, `git@git-bash`, `git@machine+user PATH`, `git@turbo (this environment)` | `git` resolves to something other than the shim there. Put the shim directory first (section 2). |
| `daemon` | The daemon is stopped or unreachable. `asc doctor --fix` starts it; `asc daemon` shows more. |
| `asc-home-space` | Low free space under `$ASC_HOME`. Free space or move `ASC_HOME`. |
| `projfs` (Windows) | `Client-ProjFS` is off. Enable the Windows feature and reboot. |
| `fuse` (Linux) | FUSE is missing or not usable. Install it, or use `--mode plain`. |
| `long-paths` (Windows) | Long paths are off. Enable `LongPathsEnabled` or use short workspace paths. |
| `defender`, `dev-drive` | Information about antivirus exclusions and Dev Drive. Excluding `$ASC_HOME` speeds up work. |
| `autocrlf`, `gitattributes` | Line-ending or attribute settings git handles and asc does not. See section 6. |
| `remote`, `store` | The remote or bucket is offline or fails the create-only probe. Check the URL and credentials. |
| credentials | Lists credential sources by name only (never values). asc uses the system credential helpers. |
