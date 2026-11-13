# Read-only git view

Status: design, not built. Roadmap row 26 (`docs/ROADMAP.md`), decided 2026-11-13: build it,
design first. This document changes two recorded decisions: SPEC §9.6 ("real git never operates on
an asc workspace") and the out-of-scope entry for a full `.git` projection (SPEC §0 and the note in
the IDE section). Nothing here changes object identity, the wire protocol or asc's own git
compatibility through the shim.

## Why

Inside an asc workspace the `git` command is asc itself: the shim (SPEC §11.4) serves every git
builtin natively from the native table (SPEC §11.5). That covers every tool that finds `git` on
`PATH` with the shim first. It does not cover three groups:

1. **Real git found some other way.** An editor or GUI that bundles git or has an absolute
   `git.path`, a build image whose only git is `/usr/bin/git`, a sandboxed coding agent started with
   a fixed `PATH`, a machine `PATH` on which the installer could not put the shim first (a
   non-elevated install on Windows cannot shadow the machine `PATH`, SPEC §11.4).
2. **Libraries that read `.git` directly.** libgit2, gix, go-git, JGit, isomorphic-git, dulwich,
   and the many build scripts that read `.git/HEAD` and `packed-refs` by hand. Shell prompts and
   version-stamping build steps are the common case. The shim never runs for them.
3. **Scripts that read the commit hash.** `git rev-parse HEAD` and `git describe` from either of the
   groups above.

Today all three see the synthetic gitdir of SPEC §9.4, which has no objects and no refs, and fail
with `fatal: not a git repository`. The goal is that `git status`, `git diff`, `git log` and the
other read commands listed in [Command coverage](#command-coverage) work for them, with the same
answers the shim gives, while every write is refused before it changes anything.

Non-goals: writes through real git, a second source of truth, importing anything real git does,
serving the view to other machines, and SHA-256 repositories (asc's `GitOid` is SHA-1 only).

## What exists to reuse

Identity and object rendering (crate `asc-core`, `asc-engine`):

- `crates/asc-core/src/git.rs:75` `object_oid`, `:133` `render_tree`, `:156` `tree_oid`,
  `:285` `render_commit`, `:352` `export_commit` (raw `git.commit` bytes when present, verified,
  else the canonical render), `:565` `export_tag`. These are the exact bytes `asc export --git`
  writes (SPEC §14.2).
- `crates/asc-core/src/object.rs:142` tree `Entry` carries `size: u64` and `git: GitOid` for every
  leaf, so a tree renders to git and a blob's loose size is known without reading its content.
- `crates/asc-git/src/tree_override.rs` verifies stored `GitTreeOverride` bodies, so mirrored trees
  keep their original oids.
- `crates/asc-engine/src/derive.rs:160` `Engine::git_commit_of` and
  `crates/asc-engine/src/derive/gitcommit.rs:16` `GitCommit`: the memoised git commit id of a
  changeset (SPEC §5, `gitcommit(cs)`).
- `crates/asc-engine/src/derive/sink.rs:15` `GitMapRow`, `:23` `GitMapSink`, `:32`
  `index_tree_git`: emits `(git oid, kind) -> asc id` rows for a tree and its entries; ingest calls
  it along the changed spines.
- `crates/asc-state/src/state/tables.rs:121` module `git_map`: `:151` `insert_many`, `:168`
  `git_to_asc`, `:187` `git_prefix` (abbreviated lookup), `:242` `asc_to_git`. Blob, tree, commit
  and tag kinds are already modelled.

The synthetic `.git` and its index (crate `asc-git`, module `projection`):

- `crates/asc-git/src/projection/layout.rs:79` `Projection`, `:88` `create`, `:121` `open`,
  `:193` `write_head`; `crates/asc-git/src/projection/mod.rs` `ProjectionError`.
- `crates/asc-git/src/projection/config.rs:79` `load_with` (the merged config with `includeIf`
  evaluated against `<root>/.git`), `:151` `add_remote`.
- `crates/asc-git/src/projection/index_export.rs:33` `BASE_FILE`, `:55` `TurboIndex`, `:334`
  `render_head`, `:369` `patch`, `:414` `update`: the index rendered from HEAD's tree metadata plus
  stage set 0, patched by changed paths only.
- `crates/asc-git/src/index.rs:179` `EntryFlags` (with `skip_worktree`), `:324` `IndexFile`, `:460`
  `IndexLock`, `:639` the sparse-cone `skip_worktree` option of the renderer.
- `crates/asc/src/index_sink.rs:147` `sync`: keeps `<gitdir>/index` equal to HEAD plus set 0 after
  every ref move and stage change.
- `crates/asc/src/refs_guard.rs:58` `apply_in_tx`: the one local-ref chokepoint (SPEC §9.4.1).
  Every ref move passes here, so it is the one place to signal the view.

The shim and real git (crate `asc`, `asc-git`):

- `crates/asc/src/shim.rs:20` `MARKER` (`asc-workspace`), `:282` `find_workspace`, `:313`
  `read_gitfile`: discovery and the trusted-workspace rule (SPEC §11.4).
- `crates/asc/src/native.rs:13` `NativeSpec`, `:34` `route`: the native table.
- `crates/asc-git/src/realgit.rs:17` `MIN_VERSION` (2.40), `:80` `Resolver`, `:235` `RealGit`:
  the only way asc starts real git; the view's tests use it.
- `crates/asc-git/src/compat/view.rs:54` `VIEW_CONFIG_TEMPLATE`, `:324` `GitView`: the server-side
  derived bare repository of SPEC §25. It proves the identity rule (same export, same ids) but is a
  full on-disk export per repo and is not reused as storage here (see
  [Approaches](#approaches-compared)).

Working copy and virtual file systems (crates `asc-wc`, `asc-vfs`):

- `crates/asc-wc/src/status.rs:423` `compute` (plain status; uses the index as its stat cache) and
  `:103` `Status` (including `shadowed`, the directories whose `.gitignore` differs from
  `.ascignore`).
- `crates/asc-wc/src/vstatus.rs:77` `DiskState`, `:96` `Input`, `:110` `compute`: virtual status
  from journal hints; "paths that were never materialised are clean by construction".
- `crates/asc-wc/src/journal.rs:102` `Journal`, `:201` `since`.
- `crates/asc-vfs/src/core.rs:639` `Provider`, `:203` `CoreFs`; the FUSE front end
  `crates/asc-vfs/src/fuse/fs.rs` with passthrough in `fuse/passthrough.rs`; the NFSv3 loopback
  server `crates/asc-vfs/src/nfs/` (the macOS default, also tested on Linux); ProjFS in
  `crates/asc-vfs/src/projfs/`. The providers already skip `.git` (`projfs/provider.rs:191`,
  `projfs/reconcile.rs:122`), so the workspace root's gitfile is an ordinary on-disk file in every
  mode.
- `crates/asc/tests/compat_modes.rs` and the harness in `tests/compat/src/lib.rs`: scenario scripts
  run against real git and against asc, compared up to `tests/compat/allowed.toml`. The view's suite
  reuses the harness with a third side: real git run against the view.

`crates/asc/src/cli/doctor.rs:100` `Check` and the `includeIf` drift check near `:1049` are the
pattern for the new doctor check.

## Approaches compared

Costs use the SPEC §17 workloads: small (about 4.5k files), medium (about 80k files and 1M
commits) and HUGE (at least 400k files, a 25 GB working copy). Figures marked "est." are reasoned
from object counts and typical sizes, not measured; unit 10 measures them.

### a. A real `.git` directory kept in step

asc writes `objects/`, `refs/`, `packed-refs` and `index` on disk and updates them on every ref
move.

- *Full objects:* the medium history undeltified (the delta principle of the roadmap forbids deltas
  on read paths) is tens of GB (est.). Rejected outright.
- *Shallow, depth 1, with blobs:* one copy of HEAD's blobs, zlib-compressed: about a third of the
  working copy (est. 8 GB for HUGE). This duplicates every file, which the "no duplicate files"
  goal forbids, and in a virtual worktree it forces every blob to be fetched.
- *Partial (`blob:none`) with promisor lazy fetch:* commits and trees only (est. 40 MB for HUGE's
  HEAD trees, plus about 6 KB per further commit). Missing blobs are fetched by git spawning
  `git fetch` against the promisor remote. That needs a transport: a remote helper on `PATH` (the
  withdrawn `git-remote-asc`, and the same `PATH` problem the shim has), `ext::` (disabled by git's
  protocol policy, and enabling it is an injection risk), or a local repository that already holds
  the objects (circular). libgit2, go-git and JGit do not lazy-fetch at all, so the library group
  fails on the first `git diff`.
- *Read-only:* not achievable. Files on disk are writable by the user. `git checkout <branch>` takes
  `index.lock`, rewrites the work tree, then the index, then `HEAD`, and all of it succeeds; asc's
  own state then disagrees with the files. Read-only file modes do not stop a rename into a writable
  directory, and toggling directory permissions around asc's own writes races real git.

Loses on disk, on lazy blobs for libraries, and on read-only.

### b. A `git` shim first on `PATH`

Already built (SPEC §11.4, §11.5, `shim.rs`, `native.rs`). Zero disk, clean asc messages, and
writes work natively. It reaches only callers that resolve `git` through `PATH` with the shim first.
It can never reach libraries or an absolute git path. On its own it does not meet row 26; it
stays the primary interface.

### c. A `.git` file pointing at a projected git directory served by the daemon

The gitfile names a directory that the asc daemon serves through a read-only mount. Every file in
it is rendered on read from asc's state: `HEAD`, `index` and in-progress state pass through from the
existing gitdir; `config` and `packed-refs` are rendered; `objects/xx/<38 hex>` are rendered on
lookup from the asc object the git oid maps to.

- *Disk:* none on Linux and macOS beyond `git_map` rows (est. 30 MB of SQLite for HUGE after a full
  HEAD tree walk; most rows already exist from ingest). On Windows, ProjFS keeps hydrated files on
  disk, bounded by an eviction cap.
- *Lazy blobs:* every reader, library or binary, opens loose objects by path, so laziness works for
  all of them with no transport.
- *Read-only:* a read-only mount returns `EROFS` (Linux, macOS) to every create, rename and write,
  so git fails at its first lock file, before it touches the work tree.
- *Freshness:* rendered at read time, so there is nothing to keep in step on switch.
- *Cost:* needs the daemon and a mount; each object read is a kernel round trip (est. 20 to 100
  microseconds), so a full-history walk is slower than git on a packed repository.

### d. A combination (chosen)

**b + c:** the shim stays first for `PATH` callers and serves reads and writes natively; the
projected read-only view serves everything else. Specifically:

- The gitfile points at the view only when the view is on. asc's own gitdir
  (`$ASC_HOME/wc/<ws>/git`) is unchanged and stays asc's writable state; the shim and every native
  command keep using it.
- Objects are rendered lazily (from c), never exported to disk (unlike a).
- No promisor, no remote helper and no `ext::` transport.

Why the others lose, in one line each: (a) cannot be made read-only and either duplicates the work
tree or breaks libraries on blobs; (b) cannot reach libraries or absolute git paths; (c) alone would
need its own write path for `PATH` callers, which the shim already provides better.

## Layout

With `asc.gitView = true` the workspace root's gitfile reads:

```
<root>/.git                 gitdir: $ASC_HOME/repos/<repo-id>/view/worktrees/<ws>
```

`$ASC_HOME/repos/<repo-id>/view` is the mount point of one read-only mount per repository, shared by
all its workspaces on the machine. Its content:

```
view/
  config                    rendered: fixed template plus allow-listed keys (Security)
  packed-refs               rendered from asc refs, sorted and peeled
  refs/heads/ refs/tags/ refs/remotes/     empty directories
  objects/<2 hex>/<38 hex>  loose objects, rendered on lookup
  objects/info/ objects/pack/              empty directories
  info/exclude              the common dir's info/exclude, passed through
  hooks/                    empty
  worktrees/<ws>/
    HEAD                    passed through from $ASC_HOME/wc/<ws>/git/HEAD
    commondir               "../..\n"
    gitdir                  "<root>/.git\n"
    index                   asc's index plus the overlay (Index), or passed through
    asc-workspace           the marker (the workspace id)
    info/sparse-checkout    the sparse cone, for sparse and virtual worktrees
    ORIG_HEAD MERGE_HEAD MERGE_MSG CHERRY_PICK_HEAD REVERT_HEAD
    rebase-merge/ rebase-apply/ sequencer/  passed through when present
```

Notes:

- The admin directory is named `worktrees/<ws>` because that is where git looks for linked
  worktrees, so `git worktree list` shows every asc workspace of the repository with its root and
  `HEAD`.
- The parent of the common dir is still `$ASC_HOME/repos/<repo-id>`, so turbo's cache location
  (SPEC §9.4) does not move.
- `asc-commit-index` and `COMMIT_EDITMSG` are not passed through: they are asc's temporary files.
- Native `rev-parse --git-dir` keeps printing `$ASC_HOME/wc/<ws>/git`, the writable gitdir, so a
  script driven through the shim that writes into the gitdir keeps working. Only real git prints
  the view path.

When the daemon is not running, the mount point is an empty directory. Real git then fails with
`fatal: not a git repository: $ASC_HOME/repos/<repo-id>/view/worktrees/<ws>`. asc itself is not
affected: the shim maps a view gitfile to `$ASC_HOME/wc/<ws>/git` from the path text alone (it never
reads the mount) and then applies the existing trust rule of `find_workspace`.

## Command coverage

The table is the target for real git 2.40 or later against the view. Unit 10 pins every row as a
compatibility case; an invocation that does not pass moves to the "refused" list before release.
"Same as native" means byte-identical to the shim's output for the same workspace, up to
`tests/compat/allowed.toml`.

### Works (read-only)

Each item gives the invocation, then the result.

- `git rev-parse HEAD`: `gitcommit(base_cs)`; same as native and as export.
- `git rev-parse --short HEAD`: prefix of length `core.abbrev` (Identity).
- `git rev-parse --abbrev-ref HEAD`, `--show-toplevel`, `--is-inside-work-tree`: same as native.
- `git rev-parse --git-dir`, `--git-common-dir`: the view paths (differs from native, by design).
- `git describe`, `--tags`, `--always`, `--long`, `--abbrev=<n>`, `--match`: same as native.
- `git describe --dirty`: same as native; git's index refresh is not saved (read-only).
- `git status`, `-s`, `-b`, `--porcelain`, `--porcelain=v2`, `-z`, `--branch`, `-uno`: same as
  native, see note 1.
- `git status --porcelain=v2 --branch`: `# branch.oid`, `.head`, `.upstream` and `.ab` lines, from
  packed-refs and the `branch.*` config.
- `git diff`, `--cached`, `HEAD`, `<rev>`, `<rev> <rev>`, `--stat`, `--name-only`, `--name-status`,
  `-M`, `-U<n>`, `--` pathspecs: same as native, see note 2.
- `git log`, any `--format`, `--oneline`, `--graph`, `-n`, `-p`, `--stat`, `--follow`, pathspecs,
  ranges: same as native; cost in Cost.
- `git show <rev>`, `<rev>:<path>`, `--stat`, `--name-only`, annotated tags: same as native.
- `git blame <path>`, `-L`, `-w`, `-M`, `-C`, `--porcelain`: same as native; cost grows with
  history depth.
- `git ls-files`, `-s`, `--stage`, `-z`, `--others`, `--exclude-standard`, `--deleted`,
  `--modified`: same as native, see notes 1 and 3.
- `git ls-tree`, `cat-file -p`, `-t`, `-s`, `-e`, `--batch`, `--batch-check`: same as native.
- `git rev-list`, `merge-base`, `name-rev`, `for-each-ref`, `show-ref`, `symbolic-ref HEAD`: same
  as native.
- `git branch` (list), `-a`, `-v`, `--show-current`; `git tag` (list), `-l`: same as native.
- `git remote`, `-v`, `get-url <name>`: the user's URL, as SPEC §9.4 requires.
- `git config --get`, `--list` (read): the view's config plus global and system.
- `git grep`, `shortlog`, `diff-tree`, `diff-index --cached`, `archive` (local): same as native.
- `git worktree list`: every workspace of the repo on this machine.
- `git fsck`: reads every rendered object; no repair.

Notes:

1. Untracked files follow `.gitignore` and `info/exclude` only. Where a directory's `.ascignore`
   shadows its `.gitignore` (the `shadowed` list of `asc-wc/src/status.rs:103`), real git may list
   files the shim hides. `asc doctor` reports these directories (Open question 5).
2. `git diff` without `--cached` compares the work tree with the index, whose entries are HEAD
   overlaid with stage set 0. That is asc's staged-versus-unstaged split, so the hunks match
   native `git diff`. Staged blobs are resolvable because staging records their `git_map` rows
   (unit 2).
3. In a virtual worktree, never-materialised paths carry `skip-worktree` in the served index
   (Index). They are clean by construction, so `git status` and `git diff` do not report them, and
   `git ls-files` still lists them.

### Refused

Every refusal is git's own error from the read-only mount, raised at the first lock file or object
write, before git changes the work tree, the index, a ref or the config. Paths shown are the view
paths. On Linux and macOS the reason text is `Read-only file system`; on Windows it is
`Permission denied` (subject to unit 5).

Each item gives the invocations, then the first line of the message.

- `git add`, `rm`, `mv`, `commit`, `checkout`, `switch`, `restore`, `reset`, `merge`, `rebase`,
  `cherry-pick`, `revert`, `am`, `stash`, `apply --index`, `sparse-checkout set`, `update-index`:
  `fatal: Unable to create '<view>/worktrees/<ws>/index.lock': Read-only file system`.
- `git branch <new>`, `branch -d`, `tag <name>`, `update-ref`, `symbolic-ref HEAD <ref>`:
  `fatal: cannot lock ref '<ref>': Unable to create '<view>/<ref>.lock': Read-only file system`.
  git's wording varies by command; the cause is the same.
- `git tag -a`, `notes add`, `commit-tree`, `hash-object -w`, `mktree`:
  `error: insufficient permission for adding an object to repository database <view>/objects`.
- `git config <key> <value>` (local):
  `error: could not lock config file <view>/config: Read-only file system`.
- `git fetch`, `pull`: fails at its first write (`FETCH_HEAD` or the incoming pack) with the same
  cause.
- `git push` (configured remote):
  `fatal: 'the-asc-git-view-is-read-only--use-asc-push' does not appear to be a git repository`
  (see Keeping it read-only).
- `git gc`, `repack`, `prune`, `maintenance run`, `pack-refs`: fails at its first write with the
  same cause.
- `git worktree add`, `remove`, `prune`: fails at its first write with the same cause.
- `git submodule update`, `init`: fails at its first write with the same cause.

Work-tree-only commands that write no repository state, such as `git apply` without `--index` and
`git clean`, edit files like any editor does. asc sees those edits as ordinary changes.

Library writers (libgit2 `git_commit_create`, gix, go-git) fail the same way at their first
create in the view.

## Identity

A changeset's git commit hash in the view is `gitcommit(cs)` (SPEC §5): `Engine::git_commit_of`
(`derive.rs:160`), memoised as `GitCommit` (`derive/gitcommit.rs:16`). It is the SHA-1 of the bytes
`export_commit` (`git.rs:352`) returns: the stored raw `git.commit` body when the changeset was
imported from a non-canonical git commit (verified against the changeset by `verify_raw_commit`),
otherwise `render_commit` (`git.rs:285`) over the root tree's git oid and the parents' git commit
ids.

**The hash equals what a git export gives.** `asc export --git` (SPEC §14.2, `export.rs:128`
`Exporter`) and the server's derived view (SPEC §25.3, `compat/view.rs`) call the same functions
with the same inputs, and tree oids come from the same `tree_oid` or verified override. So the view,
an export, a `git clone` from `asc serve` and native `git rev-parse` all print the same id for the
same changeset, and a commit exported and re-imported keeps its changeset id (SPEC §0 decision 2).
Annotated tags use `export_tag` (`git.rs:565`) the same way.

Stability rules, all inherited from asc and none new:

- An id is stable for the life of the changeset. A land rewrite makes a new changeset and drops
  `git.commit` (SPEC §8, step 5a), so the rewritten commit has a new id, as a rebase would in git.
- The canonical render includes the `asc` header only when the changeset records copies or is a
  snapshot. git accepts unknown commit headers, so readers ignore it; it is part of the hash, as it
  is in an export.
- `HEAD` is the workspace's `base_cs`. asc keeps uncommitted work in the stage set and the work
  tree, never in a commit, so there is no hidden working-copy commit for the view to expose. An
  unborn branch gives git's "No commits yet".

**Abbreviations.** git picks the default abbreviation length from the number of objects it can
count; the view has no packs, so unconfigured git would choose 7 where a full clone of the medium
repository chooses 12. The view's config therefore sets `core.abbrev` to git's own rule applied to
the repository's object count from asc's store (`max(7, ceil((floor(log2(n)) + 1) / 2) + 1)`,
the computation in git's `repo_default_abbrev_len`). The function lives once, in
`asc-git/src/view/abbrev.rs`, and native `rev-parse --short` uses it too. Uniqueness is checked
against the objects the view can list (`git_prefix`, `tables.rs:187`). A prefix that is unique
locally but not on the server can only make the local abbreviation shorter than a full clone's
abbreviation would need. The length rule prevents this up to git's usual tolerance.

## Keeping it read-only

### Writes

The mount is read-only at the file-system level:

- **Linux:** FUSE, mounted with `ro` and without `allow_other`. The kernel rejects every create,
  write, rename, unlink, mkdir, link and setattr with `EROFS` before the daemon sees the request.
- **macOS:** the existing NFSv3 loopback server (`asc-vfs/src/nfs/`), mounted with `-o ro`, and the
  server also answers every mutating procedure with `NFS3ERR_ROFS`. FSKit, where present, exposes
  the same read-only namespace.
- **Windows:** ProjFS for content. The protection is the open item of unit 5: the view root gets a
  protected DACL that grants the user read, execute and list only (no `FILE_ADD_FILE`,
  `FILE_ADD_SUBDIRECTORY`, `FILE_WRITE_DATA`, `DELETE`, `FILE_DELETE_CHILD`). The provider also
  denies `PRE_DELETE`, `PRE_RENAME`, `PRE_SET_HARDLINK` and `FILE_PRE_CONVERT_TO_FULL`
  notifications. Unit 5 must prove that ProjFS can still create placeholders and hydrate under that
  DACL and that real git's `index.lock` create fails. If it cannot, the view is not offered on
  Windows (Open question 2).

git takes `index.lock` before it changes the work tree in every index-writing command (checkout,
switch, reset, merge, restore, stash, commit, add), so the refusal comes before any file changes.
`git commit` takes the lock in `prepare_index`, before it runs `pre-commit`, so no hook runs.

`git push` reads only, but it would bypass asc's push pipeline (stacks, policy, agent branches).
The rendered config therefore sets `remote.<name>.pushurl` to the non-existent relative path
`the-asc-git-view-is-read-only--use-asc-push`, so git stops with the message in the table. An
explicit `git push <url>` is the user's own choice and is not blocked. It can only send objects
the user can already read.

Nothing real git does reaches asc. asc never reads the view: it renders it. The shim never imports
anything from it.

### Hooks never run

- The rendered config sets `core.hooksPath` to the view's own `hooks/`, an empty read-only
  directory. A repository-local value overrides a global `core.hooksPath`, so the user's global
  hooks do not run either. The only way to run a hook is to pass `-c core.hooksPath=...` on the
  command line, which is the user's explicit choice.
- The user's local settings (for example husky's `core.hooksPath`) stay in asc's own common config
  `$ASC_HOME/repos/<repo-id>/.git/config`, which only asc reads. They are not copied into the view.
- `core.fsmonitor = false` overrides a global fsmonitor setting, so no fsmonitor hook or daemon
  starts, and the built-in daemon does not try to create its IPC socket in the gitdir.
- Hooks that git runs on read-only paths: `pre-push` would run from `core.hooksPath`, which is
  empty. `post-index-change` does not run because the index cannot be written. No hook runs on
  `status`, `diff`, `log`, `show`, `blame` or `ls-files`.

### Index freshness without a full scan

The served index is asc's existing synthetic index. When the view is on, `asc.turboIndex` is
forced on for that workspace, so the file always exists:

- It is already updated by patching only the changed paths (`index_export::patch`, `:369`, and
  `update`, `:414`), from `index_sink::sync` (`index_sink.rs:147`), on every ref move through the
  chokepoint (`refs_guard.rs:58`) and every stage change. A one-path commit reads the trees along
  that path only.
- In a plain workspace each entry carries the real stat, refreshed by asc's plain status. After the
  user edits a file, git's own stat comparison finds it and hashes that file only. git's attempt to
  save refreshed stat data fails quietly on the read-only mount (git uses a non-fatal lock for
  this), which changes nothing but repeated hashing of the same racy files.
- In a virtual worktree the daemon adds an overlay at read time: every path that the provider's
  `DiskState` (`vstatus.rs:77`) reports as never materialised gets `skip-worktree`. These paths are
  clean by construction, so git neither lstats nor reports them. The rendered config sets
  `core.sparseCheckout = true` and `sparse.expectFilesOutsideOfPatterns = true`, so git does not
  lstat skip-worktree paths to look for present files. The overlay result is cached per index
  generation and per materialisation epoch. The journal (`journal.rs:102`) bumps the epoch when a
  path is materialised for the first time, so the next read re-renders the flags and the SHA-1
  trailer (est. 60 ms for HUGE's 40 MB index, once per epoch).
- Without the overlay (plain workspaces), the index file is served by passthrough
  (`fuse/passthrough.rs` on Linux), with no copy.

The view never writes the on-disk index, so asc's use of it as the plain-status stat cache
(`status.rs:423`) cannot be poisoned by a foreign writer.

### Refs and HEAD freshness

`packed-refs` is rendered at read time from asc refs: local branches, tags (with `^` peeled lines
for annotated tags), and remote-tracking refs. The header line is
`# pack-refs with: peeled fully-peeled sorted`. The rendered bytes are cached by the refs
generation that the chokepoint bumps. On Linux the daemon invalidates the kernel's cached inode for
`packed-refs` and `HEAD` with a FUSE inode notification; NFS clients revalidate on open
(close-to-open consistency). `HEAD` and the in-progress files are passed through from asc's gitdir,
which `Projection::write_head` (`layout.rs:193`) already keeps current.

## Objects

A lookup of `objects/<2 hex>/<38 hex>` accepts only lowercase hex names of exactly that shape.
Anything else is `ENOENT`. The resolver:

1. maps the oid to `(kind, asc id)` with `git_map::git_to_asc` (`tables.rs:168`);
2. renders the body: blob bytes from `Objects`; trees with `render_tree` or the verified override;
   commits with `export_commit`; tags with `export_tag`;
3. frames it as a loose object: `<kind> <len>\0<body>`, zlib-wrapped with stored (level 0) deflate
   blocks. The framed size is a pure function of the body length (2 header bytes, 5 bytes per block
   of at most 65,535 bytes, 4 Adler-32 bytes), so `getattr` answers from the tree entry's `size`
   without reading a blob. Byte-range reads compute block boundaries directly. The Adler-32 trailer
   is computed on the pass that reaches the last block and is cached in memory per oid.

Stored deflate costs no compression CPU. Nothing is stored on disk on Linux and macOS, so the
larger framing costs only page cache.

**Row coverage.** git reaches an object only through a ref or a parent object, so every oid it asks
for has been named by something the view already rendered. When the resolver renders a tree, it
records rows for the tree's entries through `index_tree_git` (`derive/sink.rs:32`), which ingest
already calls along changed spines. Commits named by refs get their row from `GitCommit`
derivation. Staged blobs get their row when they are staged (unit 2). An oid typed by hand that no
rendered object names (a blob id copied from elsewhere) is `ENOENT`, and git prints
`fatal: bad object <oid>`.

**Listing.** `readdir` of `objects/<2 hex>/` returns the known oids with that prefix
(`git_map::git_prefix`, `tables.rs:187`), which is what git uses to resolve and disambiguate
abbreviations. `objects/pack/` and `objects/info/` are empty: no packs, no alternates, no
commit-graph in v1 (Open question 4).

## Cost

| Item | small | medium | HUGE |
|---|---|---|---|
| Disk, Linux and macOS | rows only | rows only | rows only (est. 30 MB) |
| Disk, Windows (ProjFS cache) | at most the cap | at most the cap | at most the cap |
| Index file (exists with `turboIndex`) | est. 450 KB | est. 8 MB | est. 40 MB |
| Added to clone or first enable | est. under 50 ms | est. 0.3 s | est. 1 to 2 s, once |
| Added to switch, commit, checkout | est. under 5 ms | same | same |
| Real `git status`, plain | as plain git | as plain git | as plain git (400k lstat) |
| Real `git status`, virtual | materialised paths | same | same, plus untracked walk |
| `git log -n 20` | est. under 10 ms | same | same |
| `git rev-list --count HEAD` | est. under 1 s | est. 50 s (1M reads) | depends on history |

- **Disk.** The view stores nothing on Linux and macOS. `git_map` rows are the only growth, and most
  exist from ingest. On Windows, hydrated view files (the index copy and every object git read)
  occupy disk until the daemon dehydrates them. The cap is `asc.gitView.cacheBytes`, default
  256 MiB, evicted least recently used through the provider's `dehydrate` (`core.rs:639`).
- **Checkout and switch.** No objects are exported, so a switch adds only the invalidation of
  `HEAD`, `packed-refs` and `index` (est. under 5 ms). The index patch already runs for turbo. In a
  workspace where `turboIndex` was off, first enable renders the index once in O(files).
- **Sparse worktrees.** asc's sparse cone is written to `worktrees/<ws>/info/sparse-checkout` in
  cone mode, and the index already marks out-of-cone paths `skip-worktree` (`index.rs:639`), so
  git behaves as in its own sparse checkout.
- **Virtual worktrees.** The overlay stops lstat of never-materialised paths. git still walks
  directories for untracked files, and on a virtual mount each `readdir` is served by the provider
  without hydration but is not free (est. several seconds over HUGE's 40k directories on first
  run). Unit 12 adds a sparse index: directories with nothing materialised below them collapse to
  one sparse-directory entry (`index.sparse = true`), so git walks only materialised directories.
  Until unit 12 lands, the view config sets `status.showUntrackedFiles = no` in virtual worktrees,
  and `asc doctor` says so.
- **Large histories.** A full-history walk costs one kernel round trip per commit, so it is far
  slower than git on a packed repository with a commit-graph. Short walks (`log -n`, `describe`,
  prompts) are unaffected. The native shim remains the fast path. A rendered commit-graph is Open
  question 4.
- **Daemon load.** Object renders are bounded per request (one object) and served from asc's object
  cache. A blob read streams. Concurrency is the provider's existing worker pool.

## Security

A `.git` directory is an attack surface: whoever controls its config or hooks runs code in every
tool that opens it. The view's answer is that no one but the daemon can write it, and the daemon
writes nothing executable.

**Never written** into the view's `config` or any view file, whatever the user's asc config holds:
`core.hooksPath` (except the fixed empty directory), `core.fsmonitor` (except `false`),
`core.sshCommand`, `core.gitProxy`, `core.askPass`, `core.editor`, `core.pager`, `core.worktree`,
`credential.*`, `alias.*`, `filter.*`, `diff.*.command`, `diff.*.textconv`, `diff.external`,
`merge.*.driver`, `sequence.editor`, `gpg.*`, `include.*`, `includeIf.*`, `url.*.insteadOf`,
`url.*.pushInsteadOf`, `protocol.*`, `http.*`, `uploadpack.*`, `receive.*`, `safe.*`. No file is
ever written to `hooks/`.

**Written**, from a fixed template plus allow-listed keys, serialised by `gix-config` so values are
escaped:

- `core`: `repositoryformatversion = 0`, `bare = true` (Open question 3), `filemode`, `symlinks`,
  `ignorecase` (as SPEC §9.4), `logallrefupdates = false`, `hooksPath = <view>/hooks`,
  `fsmonitor = false`, `untrackedCache = false`, `splitIndex = false`, `abbrev = <n>`,
  `sparseCheckout`, `sparseCheckoutCone`.
- `sparse.expectFilesOutsideOfPatterns = true`; `index.sparse` (unit 12).
- `gc.auto = 0`, `maintenance.auto = false`, `fetch.writeCommitGraph = false`.
- `remote.<name>.url` and `remote.<name>.fetch`, copied from asc's config, with names checked by
  `RemoteName::new` (`layout.rs:25`), and URLs rejected if they contain a control character.
  `remote.<name>.pushurl` is set to the guard path.
- `branch.<b>.remote` and `branch.<b>.merge`, with branch names checked as refs.
- `status.showUntrackedFiles = no` in virtual worktrees until unit 12.

**Content from the tree** (`.gitattributes`, `.gitignore`) can name a filter or diff driver, but a
driver's command can come only from the user's own global or system config. This is git's normal
model for any clone, and the view adds no driver.

**Mount access.** FUSE mounts omit `allow_other`, so only the user's processes see the view. The NFS
mount uses the loopback restrictions of the virtual worktree mount (SPEC §10.3b). The Windows root
gets the protected DACL of unit 5. Lookup names are validated, so no `..` and no path outside the
namespace can be reached.

**safe.directory.** git 2.35.2 and later checks that the work tree, gitdir and common dir belong
to the current user. The view reports the user as the owner on every backend (FUSE and NFS
attributes carry the user's uid; ProjFS files are created under the user), so no `safe.directory`
entry is needed. asc never writes `safe.directory` into any config, global or local. `asc doctor`
runs real git `rev-parse HEAD` in the view and fails the check if git reports dubious ownership.

**Object integrity.** Rendered objects come from verified asc objects (`export_commit` verifies raw
bodies, `tree_override.rs` verifies overrides), so git reads correct content. git does not re-hash
loose objects on read, so correctness rests on the renderer, which unit 1 tests against real git's
`cat-file` and `fsck`.

**Discovery.** The shim's trust rule is unchanged in substance. A gitfile naming
`<home>/repos/<id>/view/worktrees/<ws>` is mapped lexically to `<home>/wc/<ws>/git`, and that
directory must pass every existing check of `find_workspace`, including the marker and the
canonical-path rule. A view path whose `<id>` and `<ws>` do not match the workspace's registered
repo is not an asc workspace.

## CLI and configuration

- **Opt-in.** `asc.gitView` (git-format, local or global, read like `asc.turboIndex`), default
  `false`. When it is on, the workspace's gitfile points at the view, and `asc.turboIndex` is forced
  on for that workspace.
- **Verb.** `asc git-view on|off|status` (an asc-only verb, SPEC §11.2). `on` sets the key, asks
  the daemon to mount the repository's view, and rewrites the gitfile atomically, after the mount
  answers. `off` rewrites the gitfile to `$ASC_HOME/wc/<ws>/git` first and then unmounts if no
  other workspace of the repo uses the view. `status` prints the mount state, the backend, the
  gitfile target, and the result of a real-git probe.
- **Cache cap.** `asc.gitView.cacheBytes` (Windows only), default 256 MiB.
- **Daemon.** The daemon mounts every repository's view at start for each workspace with the key
  on. A failed mount leaves the gitfile pointing at the view, so real git fails with "not a git
  repository", and the daemon logs the failure.
- **`asc doctor` check "git view"**, only when the key is on:
  - the gitfile points at the view and the mount is live;
  - the backend is read-only: creating `index.lock` in the view fails;
  - `RealGit` (`realgit.rs:235`) `rev-parse HEAD` in the view prints the same id as native
    `rev-parse HEAD`, with no dubious-ownership error;
  - the rendered config has none of the never-written keys;
  - warnings: directories in `Status::shadowed` (note 1), `status.showUntrackedFiles = no` in
    effect, a Windows build without the unit 5 protection;
  - a fix action `asc git-view off` when the mount cannot be made.

## Implementation units

Each unit has its tests and its documentation change, and passes its acceptance command and the
crate's oxlint-equivalent checks (`cargo clippy` as CI runs it) before it is done.

1. **Loose-object renderer.** New `crates/asc-git/src/view/mod.rs` and `view/object.rs`: stored-
   deflate zlib framing, framed size from body length, byte-range reads, Adler-32. Pure; no I/O.
   Tests write rendered blobs, trees, commits and tags into a temporary `objects/` directory and
   check them with `RealGit` `cat-file -p` and `fsck --strict` (including a raw `git.commit`, an
   override tree, a 70,000-byte blob crossing a block boundary and an empty blob). Docs: module
   docs. Acceptance: `cargo test -q -j 4 -p asc-git view_object`.
2. **Resolver and row coverage.** New `crates/asc-git/src/view/resolve.rs`: oid to `(kind, asc id)`
   through `git_map`, child rows on tree render through `index_tree_git`, prefix listing. Edit
   `crates/asc-state/src/state/stage.rs` so staging a blob records its `git_map` row. Tests: every
   object reachable from a ref resolves after a cold start; an unknown oid is `None`; prefix
   listing matches `git_prefix`. Docs: SPEC §5 note on staged rows. Acceptance:
   `cargo test -q -j 4 -p asc-git view_resolve`.
3. **Namespace and config.** New `crates/asc-git/src/view/ns.rs` (the read-only namespace:
   `lookup`, `getattr`, `read_dir`, `read` over a `ViewSource` trait), `view/config.rs` (template,
   allow-list, push guard), `view/refs.rs` (`packed-refs`), `view/abbrev.rs`. Tests: a property test
   that no never-written key appears for any input config; `packed-refs` is sorted and peeled;
   passthrough names are exactly the allow-list; invalid object names are refused. Docs: this
   document's tables become `docs/git-view.md` for users. Acceptance:
   `cargo test -q -j 4 -p asc-git view_ns`.
4. **Read-only front ends, Linux and macOS.** New `crates/asc-vfs/src/viewfs/mod.rs` (a
   `ReadOnlyNs` trait, so `asc-vfs` does not depend on `asc-git`), `viewfs/fuse.rs` (mount `ro`, no
   `allow_other`, inode invalidation), `viewfs/nfs.rs` (export over the existing server, every
   mutating procedure answers `NFS3ERR_ROFS`). Tests on Linux mount an in-memory namespace and check
   reads, `EROFS` on create, rename and unlink, and invalidation. Docs: SPEC §10.3 and §10.3b
   paragraphs. Acceptance: `cargo test -q -j 4 -p asc-vfs --features fuse,nfs viewfs`.
5. **Windows front end and protection spike.** New `crates/asc-vfs/src/viewfs/projfs.rs`: a second
   virtualization root, the protected DACL, denial of the pre-notifications, and LRU dehydration
   under the cap. The test that decides the platform: under the DACL, placeholders and hydration
   work and real git's `index.lock` create fails. If the test cannot pass, the unit records that,
   and unit 9 reports the view as unavailable on Windows. Docs: SPEC §10.2 paragraph. Acceptance:
   `cargo test -q -j 4 -p asc-vfs --features projfs viewfs_projfs`.
6. **Index overlay.** Edit `crates/asc-git/src/projection/index_export.rs` (an overlay entry point
   that sets `skip_worktree` from a predicate and recomputes the trailer) and
   `crates/asc/src/index_sink.rs` (force `turboIndex` when the view is on; expose the index
   generation). Tests: the overlay flags exactly the never-materialised paths; real git `status`
   on a fixture with an overlaid index lstats only materialised paths (counted through a recording
   `DiskState`); the on-disk index is never changed by the overlay.
   Docs: SPEC §9.4 index paragraph.
   Acceptance: `cargo test -q -j 4 -p asc-git index_export_overlay`.
7. **Layout and discovery.** Edit `crates/asc-git/src/projection/layout.rs` (`Projection::open`
   accepts the view gitfile; `set_view(bool)` rewrites the gitfile atomically) and
   `crates/asc/src/shim.rs` (lexical mapping of a view gitdir to `wc/<ws>/git` before the trust
   checks). Tests: discovery with the mount absent; a view path naming another repo's workspace is
   refused; native `rev-parse --git-dir` prints the writable gitdir. Docs: SPEC §9.4 layout block
   and §11.4 discovery bullet. Acceptance: `cargo test -q -j 4 -p asc shim_view` and
   `cargo test -q -j 4 -p asc-git projection_view`.
8. **Daemon wiring.** New `crates/asc/src/daemon/gitview.rs`: a `ViewSource` over the state db,
   the engine and the workspace's gitdir; one mount per repo; invalidation driven by the chokepoint
   (`refs_guard.rs:58`) and the index sink; materialisation epochs from the journal. Tests (Linux):
   a commit through the shim is visible to real git `log -1` in the view without a remount; two
   workspaces share one mount. Docs: SPEC §9.1 daemon duties. Acceptance:
   `cargo test -q -j 4 -p asc daemon_gitview`.
9. **CLI, config and doctor.** New `crates/asc/src/cli/git_view.rs` (`on|off|status`); edit
   `crates/asc/src/cli/doctor.rs` (the "git view" check) and `crates/asc/src/cli/help_git.rs`.
   Tests: `on` then `off` restores the original gitfile byte for byte; doctor fails on a planted
   forbidden key and on a writable view. Docs: SPEC §11.2 verb list, `docs/CHEATSHEET.md`.
   Acceptance: `cargo test -q -j 4 -p asc git_view_cli`.
10. **Real-git compatibility suite and measurements.** New `crates/asc/tests/git_view.rs`, reusing
    `tests/compat/src/lib.rs`: every "Works" row runs through `RealGit` in the view and is compared
    with the shim's output and with real git on `asc export --git` of the same refs. Every
    "Refused" row is checked to fail with the listed cause and to leave the work tree, the index,
    `HEAD` and the refs byte-identical. The test also records object reads and wall time for the
    Cost table on the small fixture. Docs: replace "est." figures here with measured ones.
    Acceptance: `cargo test -q -j 4 -p asc --test git_view`.
11. **Library readers.** New `crates/asc-git/tests/view_gix.rs`: gix (already a dependency) opens
    the view, reads `HEAD`, walks 100 commits, diffs index against work tree, and fails to commit.
    Docs: list of verified readers in `docs/git-view.md`. Acceptance:
    `cargo test -q -j 4 -p asc-git --test view_gix`.
12. **Sparse index for virtual worktrees.** Edit `crates/asc-git/src/index.rs` (sparse-directory
    entries and the `sdir` extension, since `gix-index` cannot write them; spliced as the TREE
    extension already is) and the overlay of unit 6 (collapse directories with nothing
    materialised). Then remove the `status.showUntrackedFiles = no` default. Tests: real git
    `status` on a collapsed index matches native status; `git ls-files --sparse` shows the collapsed
    directories. Docs: SPEC §9.4. Acceptance: `cargo test -q -j 4 -p asc-git sparse_index`.

Order: 1, 2, 3 in parallel; then 4 and 5; then 6 and 7; then 8, 9, 10, 11; 12 last.

## Open questions

1. **Opt-in or default?** Recommended: opt-in (`asc.gitView = false`) for the first release. Make it
   the default only after unit 10 has passed in CI on Linux, macOS and Windows for two releases and
   unit 12 has landed.
2. **Windows, if the unit 5 spike fails.** Recommended: do not offer the view on Windows. Do not
   use a writable on-disk fallback, because it would break the read-only guarantee (approach a).
   The shim stays the Windows answer, and `asc doctor` explains how to point an editor's git path
   at it.
3. **`core.bare = true` in the view's common config.** It makes `git worktree list` show the asc
   workspaces correctly instead of a non-existent main worktree. Recommended: yes, gated on turbo's
   contract test (SPEC §9.4, U61p) still placing the cache in `$ASC_HOME/repos/<repo-id>`.
4. **A rendered commit-graph** (`objects/info/commit-graph`) from asc's generation numbers
   (`cs_gen`) would make long walks and `merge-base` fast. Recommended: not in v1. Add it as a
   follow-up unit if unit 10's measurements show full-history walks in real use.
5. **Translate `.ascignore` into `info/exclude`** so untracked output matches native everywhere.
   Recommended: no. Per-directory rules with negation do not translate exactly. Report the
   shadowed directories in `asc doctor` instead.
6. **Turbo and the skip-worktree overlay.** turbo reads the index with gix-index. Recommended: keep
   the overlay on in virtual worktrees, and add a turbo contract case. If turbo mishandles
   skip-worktree entries, serve the index without the overlay when the root tree has `turbo.json`.
7. **Block `git push` through the view.** Recommended: yes, with the `pushurl` guard. It keeps one
   push pipeline and costs nothing for readers.
8. **A Cargo feature for the view.** Recommended: no new feature. The view builds wherever a VFS
   backend feature (`fuse`, `nfs`, `projfs`) is built, and the runtime key keeps it off.
9. **Containers that mount only the work tree.** The gitfile names a path outside the container.
   Recommended: out of scope. `asc export --git` or a `git clone` from `asc serve` is the answer for
   images that need a real repository.
10. **Serving the view without the daemon.** Recommended: no. The daemon already owns the mounts of
    virtual worktrees. A second always-on process would add a supervisor and a lock protocol for
    no new reader.
