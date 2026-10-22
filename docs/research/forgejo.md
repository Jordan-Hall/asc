# Forgejo internals: research for full storage replacement by asc

Status: research, 2026-10-22. Author: research agent for the asc project.
Source studied: `D:\dev\forgejo` (blobless partial clone of `codeberg.org/forgejo/forgejo`), checked out at tag **v16.0.6** (commit `1aa139441`, "2026-10-12 security patches").

## 0. Facts first

| Item | Value |
|---|---|
| ASC server version | `16.0.6+gitea-1.22.0` (from `https://forge.example.org/api/v1/version`) |
| Fork base | tag `v16.0.6`. Upstream already has `v17.0.0` and `origin/forgejo` is 764 commits past `v16.0.0` |
| Upstream churn v16.0.6 -> main | 1667 files, +77k/-38k lines. In `modules/git` + `modules/gitrepo`: 28 files, +998/-667. In pull/repository/private/githttp/serv: 42 files, +1064/-390 |
| Go module | `forgejo.org`, go 1.26 |
| Non-test Go LOC (excluding `tests/`) | ~362k |
| go-git | Removed upstream (`a21128a73 [CHORE] Drop go-git support`). All git access is the `git` CLI via `modules/git` (good: one implementation to replace, not two) |

## 1. How Forgejo talks to git today

Everything is the `git` executable run against a bare repository at
`setting.RepoRootPath/<owner>/<name>.git` (wiki: `<name>.wiki.git`).
There is no storage interface. The path is the interface.

### 1.1 `modules/git` (the git wrapper, ~16k LOC non-test)

- `repo_base.go`: `type Repository struct { Path string; batch, check *Batch; Ctx; LastCommitCache; objectFormat }`. `OpenRepository(ctx, path)` only checks the directory exists. Reads go through two long-lived `git cat-file --batch` / `--batch-check` processes (`batch.go`, `batch_reader.go`, `CatFileBatch`, `CatFileBatchCheck`).
- Object types: `Commit` (`commit.go`, `commit_reader.go`), `Tree` (`tree.go`, has `repo *Repository`), `TreeEntry` (`tree_entry.go`, `tree_entry_mode.go`), `Blob` (`blob.go`, has `repo *Repository`), `Tag` (`tag.go`), `Signature`, `ObjectID` / `ObjectFormat` (`object_id.go`, `object_format.go`: sha1 and sha256).
- Refs: `ref.go` (`RefName`, `BranchPrefix`, `TagPrefix`, `PullPrefix`), `repo_ref.go`, `repo_branch.go`, `repo_tag.go`, `foreachref/` (parser for `for-each-ref --format`).
- History: `repo_commit.go` (679 LOC: `CommitsByFileAndRange`, `CommitsBetween*`, `FileCommitsCount`, `GetCommitByPath`, `IsCommitInBranch`), `log_name_status.go` (`WalkGitLog`, used to find the last commit for each entry of a directory), `commit_info.go` + `last_commit_cache.go` (directory listing "latest commit" column), `repo_commitgraph.go`.
- Diff/compare: `diff.go` (`GetRawDiff`, `GetRepoRawDiffForFile`, `GetAffectedFiles`, hunk utilities), `repo_compare.go` (`GetCompareInfo`, `GetMergeBase`, `GetDiffNumChangedFiles`, `GetFilesChangedBetween`, `GetPatch`).
- Blame: `blame.go` (`CreateBlameReader(ctx, objectFormat, repoPath, commit, file, ...)` streams `git blame --porcelain`), `repo_blame.go` (`LineBlame`, `ReverseLineBlame` used for review comment line tracking).
- Grep: `grep.go` (`GrepSearch` runs `git grep`).
- Archive: `repo_archive.go` (`CreateArchive` runs `git archive`; `BUNDLE` too).
- Index/write primitives: `repo_index.go` (`ReadTreeToIndex`, `AddObjectToIndex`, `WriteTree`), `repo_tree.go` (`CommitTree`), `repo_object.go` (`HashObject`).
- Attributes / language stats: `repo_attribute.go` (`git check-attr` long-running process), `repo_language_stats.go` (walks every blob with cat-file + go-enry).
- Misc: `notes.go`, `submodule.go`, `repo_gpg.go`, `signature.go`, `hook.go` + `hook_generate.go` (writes server-side hook scripts into each bare repo), `remote.go`, `fetch.go`, `command.go` (`NewCommand`, `RunOpts{Dir, Env, Stdin, Stdout}`, trusted-argument typing), `git.go` (version checks; `SupportGitMergeTree` needs git >= 2.38).
- `pipeline/`: `catfile.go`, `revlist.go`, `namerev.go`, `lfs.go` (scans history for LFS pointers by piping rev-list into cat-file).

Exported surface (non-test): **504** exported funcs/methods in `modules/git` (+pipeline, foreachref). **104** methods on `*Repository` (listed in Appendix A). About **130** `NewCommand(` call sites inside `modules/git`.

### 1.2 `modules/gitrepo` (the upstream "hide the path" layer)

`gitrepo.go`: `type Repository interface { GetName(); GetOwnerName() }`, `OpenRepository`, `OpenWikiRepository`, `RepositoryFromContextOrOpen`; `branch.go` (default branch get/set), `walk.go` (`WalkReferences`). This is where upstream Gitea started moving path knowledge out of callers (`5f82ead13 Simplify how git repositories are opened (#28937)`, `3d9afe881 Move get/set default branch ... to hide repopath (#29126)`). The effort stalled. 70 files import `gitrepo`. **This package is the natural place to choose a backend per repository.**

### 1.3 Consumers (who imports `modules/git`)

211 non-test files import `forgejo.org/modules/git`:

| Area | Files |
|---|---|
| routers/web (mostly `routers/web/repo`) | 30 |
| services/repository | 27 |
| services/pull | 15 |
| routers/api (mostly `routers/api/v1/repo`) | 15 |
| services/webhook | 12 |
| models/gitea_migrations | 7 |
| services/actions, modules/repository, modules/indexer | 5 each |
| services/notify, migrations, issue, convert, models/repo, models/issues | 4 each |
| other (f3, doctor, private, asymkey, wiki, mirror, cron, context, markup, ...) | rest |

Most-used identifiers outside `modules/git`: `git.NewCommand` 132, `git.RunOpts` 114, `git.RefName` 111, `git.Repository` 109, `git.Commit` 90, `git.IsErrNotExist` 63, `git.BranchPrefix` 51, `git.OpenRepository` 34, `git.TreeEntry` 28, `git.Blob` 16, `git.Tag` 15. 153 distinct identifiers in total.

Other counts:

| Metric | Count |
|---|---|
| `git.NewCommand` calls outside `modules/git` (raw git CLI against a path) | **132** in 47 files |
| `.RepoPath()` / `.WikiPath()` calls | **158** in ~60 files |
| `setting.RepoRootPath` uses | 46 |
| `git.OpenRepository` / `gitrepo.Open*` / `RepositoryFromContextOrOpen` calls | 124 |
| `ctx.Repo.GitRepo` uses (per-request opened repo, `services/context/repo.go:707,976`) | 167 |
| Method calls on git objects in `routers/web/repo` / `routers/api/v1/repo` | ~203 / ~78 |

Raw `git.NewCommand` hot spots (outside `modules/git`):
`services/pull/patch.go` 11, `services/repository/files/temp_repo.go` 10, `services/pull/merge_prepare.go` 10, `services/mirror/mirror_push.go` 8, `services/mirror/mirror_pull.go` 6, `services/pull/temp_repo.go` 5, `services/pull/merge_rebase.go` 5, `modules/indexer/code/git.go` 5, `services/repository/migrate.go` 4, `services/migrations/{gitea_uploader,dump}.go` 4+4, `services/asymkey/sign.go` 4, `routers/web/repo/githttp.go` 4, `services/repository/init.go` 3, `services/doctor/heads.go` 3, `routers/private/hook_verification.go` 3, then 1-2 each in fork, create, update_rebase, merge_squash, merge, check, agit, wiki, gitgraph, generate, contributors_graph, release, gitdiff, indexers, `cmd/hook.go`.

### 1.4 The write path: every write is "temp clone + git push"

This is the single most important finding. Forgejo never writes objects into the real repo directly from the web process. Every write follows the same shape:

1. Make a temporary clone (`git clone -s` with alternates, or `git init` + alternates) in a temp dir.
2. Build the change with index plumbing (`read-tree`, `update-index`, `hash-object -w`, `write-tree`, `commit-tree`) or porcelain (`merge`, `rebase`, `am`, `apply`).
3. `git push` back into the bare repo with `FullPushingEnvironment(...)` (`modules/repository/env.go`), which triggers the bare repo's hooks.
4. Hooks (`cmd/hook.go`) call the internal HTTP API: `/api/internal/hook/pre-receive`, `/hook/post-receive`, `/hook/proc-receive` (`routers/private/internal.go:63-66`).
5. `HookPostReceive` -> `repo_service.PushUpdates` (`services/repository/push.go`) -> branch table sync, tags/releases, notifications, webhooks, Actions triggers, PR re-test.

Writers that follow this pattern:

| Feature | Code |
|---|---|
| Web editor, upload, delete, patch apply, cherry-pick/revert from UI, API `contents` | `services/repository/files/temp_repo.go` (`TemporaryUploadRepository`: Clone, SetDefaultIndex, LsFiles, RemoveFilesFromIndex, HashObject, AddObjectToIndex, WriteTree, CommitTree, Push, DiffIndex), `update.go`, `upload.go`, `patch.go`, `cherry_pick.go`, `commit.go` |
| PR merge (merge, rebase, rebase-merge, squash, ff-only, manual) | `services/pull/merge.go` (`doMergeAndPush`), `merge_prepare.go`, `merge_merge.go`, `merge_rebase.go`, `merge_squash.go`, `merge_ff_only.go`, `merge_manual.go`, `temp_repo.go` (`createTemporaryRepoForPR`) |
| PR "update branch" | `services/pull/update.go`, `update_rebase.go` |
| PR conflict check / mergeability | `services/pull/patch.go` (`testPatch`, `checkConflicts`, `MergeTree` via `git merge-tree --write-tree`, fallback `read-tree -m` + `merge-file`), `patch_unmerged.go`, `check.go` |
| Protected-file checks | `services/pull/patch.go` `CheckFileProtection`, `CheckUnprotectedFiles` (also called from pre-receive) |
| Wiki | `services/wiki/wiki.go` (`git.Clone` of `.wiki.git`, index plumbing, `git.Push`) |
| Repo init with README/license/gitignore | `services/repository/init.go` (`add --all`, `commit`, `push origin`) |
| Templates / generate | `services/repository/generate.go`, `generate_repo_commit.go` |
| Branch create from UI / sync fork | `routers/web/repo/branch.go:133`, `services/repository/branch.go:354`, `services/repository/sync_fork.go:31` (all `git.Push` between bare repos) |
| Fork | `services/repository/fork.go:144` (`git clone --bare`) |
| AGit flow (`refs/for/*`) | `services/agit/agit.go` via `proc-receive` hook |
| Mirrors | `services/mirror/mirror_pull.go` (`remote update --prune`), `mirror_push.go` (`remote add --mirror=push`, `git.Push`) |
| Migrations | `services/repository/migrate.go`, `services/migrations/gitea_uploader.go` (`git clone --mirror`) |

Consequence: the write side is a *small set of high-level operations* (create commit from file ops, merge with style, rebase, cherry-pick/revert, apply patch, update refs, init, fork, delete) disguised as hundreds of lines of git plumbing. In the Scion model these are exactly the operations the **server** owns. We replace temp repos with RPCs, not with a faster temp repo.

### 1.5 Hooks and the internal API (already an RPC seam)

- `cmd/hook.go`: `pre-receive`, `update`, `post-receive`, `proc-receive` subcommands. They read stdin, batch refs, and POST to Forgejo.
- `modules/private/hook.go`: `HookOptions { OldCommitIDs, NewCommitIDs, RefFullNames, UserID, UserName, GitObjectDirectory, GitAlternativeObjectDirectories, GitQuarantinePath, GitPushOptions, PullRequestID, PushTrigger, DeployKeyID, IsWiki, ActionPerm }`.
- `routers/private/hook_pre_receive.go`: branch protection (force push, required approvals, signed commits via `hook_verification.go` which runs `rev-list`, protected files via `pull.CheckFileProtection`, quota, push options). It reads objects from the quarantine (`GIT_QUARANTINE_PATH`).
- `routers/private/hook_post_receive.go`: calls `repo_service.PushUpdates`, sets default branch on first push, PR auto-merge handling, push-option repo settings.
- `routers/private/hook_proc_receive.go` -> `services/agit`.

The hook protocol is plain HTTP + JSON keyed by `(owner, repo, []RefUpdate)`. **An asc server can call these same endpoints** before and after its ref compare-and-swap. Policy code stays in Forgejo, unchanged.

### 1.6 Serving git protocol

- Smart HTTP: `routers/web/repo/githttp.go`. `httpBase` does auth (basic, token, Actions task token, LFS JWT), repo redirects, push-to-create (`repo_service.PushCreateRepo`), then `serviceRPC` execs `git upload-pack|receive-pack --stateless-rpc <repoDir>`. Also dumb HTTP (`GetInfoRefs`, `GetInfoPacks`, `GetLooseObject`, `GetPackFile`, `GetIdxFile`) reading files straight from the repo directory.
- SSH: `cmd/serv.go` (OpenSSH `AuthorizedKeysCommand` mode) and the built-in Go SSH server (`modules/ssh`) both call `private.ServCommand` (`routers/private/serv.go`) for authz, then exec `git-upload-pack` / `git-receive-pack` on the repo path. `git-lfs-authenticate` is handled in `cmd/serv.go:263-300` by minting an LFS JWT (`lfs.Claims`). That pattern is reusable for an `asc-authenticate` verb.

### 1.7 LFS

- `modules/lfs` (pointer parsing, `ContentStore` over `storage.LFS`, HTTP client for migrations), `services/lfs/server.go` (batch API, locks), `models/git/lfs.go` (`LFSMetaObject` rows per repo), `modules/git/pipeline/lfs.go` (history scan), `services/pull/lfs.go` and `services/repository/lfs.go` (copy LFS meta on merge/fork, GC).
- `modules/storage` already has a pluggable `ObjectStorage` interface (`Open/Save/Stat/Delete/URL/IterateObjects`) with `local.go` and `minio.go` (S3-compatible) implementations, used for LFS, attachments, avatars, archives, packages, Actions logs and artifacts. **Forgejo already knows how to put bytes in S3. It does not do it for git objects.**

In asc, large files are ordinary content-addressed blobs (chunked). LFS becomes a compatibility view: the LFS batch API can answer from the asc blob store (an LFS OID is a sha256 of content, which asc can index as an alias). ASC's ~7.6 GB LFS can be imported as normal asc blobs.

### 1.8 Actions (CI)

- Workflow discovery reads the tree of the pushed commit: `modules/actions/workflows.go` (`ListWorkflows(commit)` uses `commit.SubTree(".forgejo/workflows")`, `GetContentFromEntry`, `DetectWorkflows`, `matchPushEvent` uses `commit.GetFilesChangedSinceCommit(before)` for `paths:` filters, `matchPullRequestEvent` uses `gitRepo.GetCommit`). Triggered from `services/actions/notifier.go`, `notifier_helper.go`.
- Runner context: `services/actions/context.go` (`server_url`, `repository`, `sha`, `ref`), `services/actions/task.go:173` (`token`), `services/actions/auth.go` (OIDC claims incl. `sha`). The runner (forgejo-runner, act-based, separate repo `code.forgejo.org/forgejo/runner`) runs `actions/checkout`, which does a normal `git fetch` over smart HTTP with the task token. Forgejo itself does not check code out for the runner.
- ASC's workflows: 17 `actions/checkout` steps across 7 workflow files, all with `submodules: recursive`; `deploy-affected.yml` uses `fetch-depth: 0` to compute affected packages from full history. Every job pays a full checkout + LFS + submodule clone.

### 1.9 Code indexer, stats, misc readers

- `modules/indexer/code/git.go`: `show-ref`, `merge-base`, `ls-tree -r -l`, `diff --name-status` between last indexed commit and HEAD. `bleve.go`, `elasticsearch.go`, `zoekt/zoekt.go` read blobs with `cat-file`. All of this is "changed paths between two changesets" + "read blob" - a perfect fit for asc derived data.
- `modules/indexer/stats` (language stats via `repo_language_stats.go`).
- `services/repository/contributors_graph.go`, `services/repository/gitgraph/graph.go` (`git log --graph`), `routers/web/repo/recent_commits.go`, `modules/git/repo_stats.go` (code activity).
- `services/repository/archiver/archiver.go` (`git archive` into `storage.RepoArchives`).
- `services/gitdiff/gitdiff.go:1129` runs `git diff` and parses unified diff text (`ParsePatch`); review comments, CSV diff, highlighting and "load more" depend on that text format.
- `services/asymkey/sign.go` and `models/asymkey` (commit signature verification and server-side signing of merges / web edits).
- `services/repository/check.go` (`git gc` cron), `services/doctor/*` (fsck, HEAD, config), `modules/repository/create.go getDirectorySize` (repo size by walking the directory).
- Database caches git facts as strings: `models/git/branch.go` (`Branch{CommitID, CommitMessage, CommitTime}`), `commit_status.go` (`SHA VARCHAR(64)`), `models/issues/pull.go` (`MergeBase`, `MergedCommitID VARCHAR(64)`), `comment.go` (`CommitSHA VARCHAR(64)`), Actions runs `CommitSHA`, release `Sha1`, indexer status. After the SHA-256 work, all current columns are 64 chars wide.

### 1.10 Object format: an existing per-repo switch

`modules/git/object_format.go` defines `ObjectFormat` (`Name, EmptyObjectID, EmptyTree, FullLength, IsValid, MustID, ComputeHash`) with sha1 and sha256 implementations. `models/repo/repo.go:182` stores `ObjectFormatName VARCHAR(6)` per repo, and ID parsing everywhere goes through it. A third format, `"asc"` (64-hex BLAKE3 changeset/tree/blob IDs), fits the columns and the parsing code. **This is the per-repo feature flag that already exists.** (A separate `storage_backend` column is cleaner and is proposed below; `object_format_name = "asc"` is the cheap fallback.)

## 2. Narrowest seam

Goal: asc is the source of truth. No bare git repo is authoritative. Touch as few of the 211 importing files as possible, and make each fork patch small and rebase-friendly.

Three seams, from narrowest to widest.

### Seam A - read backend inside `modules/git` (keeps the 211 importers unchanged)

Keep `git.Repository`, `Commit`, `Tree`, `TreeEntry`, `Blob`, `Tag` as **concrete types with the same exported API**. Add one private field to `Repository`: `backend Backend`. Every method that today runs `NewCommand` or `CatFileBatch` dispatches to the backend. The current code moves, mostly verbatim, into `modules/git/gitcli` (backend for legacy repos and tests). A new `modules/git/ascclient` package implements the same interface against the asc server.

Callers keep calling `gitRepo.GetCommit(id)`, `commit.GetTreeEntryByPath(p)`, `entry.Blob().DataAsync()`. They never learn the backend changed. That is the reason for putting the seam *below* the existing types and not above them: changing `*git.Repository` into an interface would touch 109 type references, 90 `git.Commit` references and every test.

Sketch (Go):

```go
package git

// Backend is everything modules/git needs from a repository store.
// Implementations: gitcli (git binary on a bare repo), ascclient (asc server RPC).
type Backend interface {
    ObjectFormat() ObjectFormat

    // Objects (immutable, content-addressed: cache freely)
    ReadCommit(ctx context.Context, id ObjectID) (*CommitData, error)
    ReadTree(ctx context.Context, id ObjectID) ([]TreeEntryData, error)
    ReadTreeRecursive(ctx context.Context, id ObjectID, opt TreeWalkOpts) iter.Seq2[TreeEntryData, error]
    OpenBlob(ctx context.Context, id ObjectID, limit int64) (io.ReadCloser, int64, error)
    StatObjects(ctx context.Context, ids []ObjectID) ([]ObjectInfo, error)
    ReadTag(ctx context.Context, id ObjectID) (*TagData, error)
    Resolve(ctx context.Context, rev string) (ObjectID, ObjectType, error) // rev = full ref, short id, "id^", "id~n"

    // Refs (single-row compare-and-swap on the server)
    ListRefs(ctx context.Context, q RefQuery) ([]Reference, error) // prefix, pattern, sort, limit, contains
    Head(ctx context.Context) (RefName, error)
    SetHead(ctx context.Context, branch RefName) error

    // History and derived data (server-side, Scion style)
    Log(ctx context.Context, q LogQuery) iter.Seq2[*CommitData, error] // from/not, paths, follow, skip/limit, author, since/until, grep
    Count(ctx context.Context, q LogQuery) (int64, error)
    MergeBase(ctx context.Context, a, b ObjectID) (ObjectID, error)
    IsAncestor(ctx context.Context, ancestor, desc ObjectID) (bool, error)
    LastCommits(ctx context.Context, at ObjectID, dir string, names []string) (map[string]ObjectID, error) // replaces WalkGitLog + LastCommitCache
    Blame(ctx context.Context, at ObjectID, path string, opt BlameOpts) iter.Seq2[BlamePart, error]

    // Diff (server-side; git-compatible unified text so services/gitdiff keeps working)
    ChangedPaths(ctx context.Context, base, head ObjectID, opt DiffOpts) ([]PathChange, error) // name-status + rename/copy
    UnifiedDiff(ctx context.Context, base, head ObjectID, opt DiffOpts, w io.Writer) error   // "diff --git" format, binary/patch/raw modes
    DiffStat(ctx context.Context, base, head ObjectID) (DiffStat, error)

    // Bulk readers
    Archive(ctx context.Context, at ObjectID, format ArchiveType, prefix string, w io.Writer) error
    Grep(ctx context.Context, at ObjectID, q GrepOptions) ([]*GrepResult, error)
    Attributes(ctx context.Context, at ObjectID, paths, names []string) (map[string]map[string]string, error)
    LanguageStats(ctx context.Context, at ObjectID) (map[string]int64, error)

    Close() error
}
```

About 30 methods. The 104 `*Repository` methods and the `Commit`/`Tree`/`Blob` methods are rewritten on top of them (most become 3-10 line adapters). Functions that take a `repoPath string` today (`CreateBlameReader`, `GetReverseRawDiff`, `LogNameStatusRepo`, `NewLogNameStatusRepoParser`, `GetAffectedFiles(env)`) change signature to take `*Repository`. That is a handful of callers.

Backend selection lives in **one** place: `modules/gitrepo.OpenRepository(ctx, repo)`. Extend `gitrepo.Repository` with `GetStorageBackend() string` (implemented by `repo_model.Repository`), and the function picks `gitcli` or `ascclient`. Callers that still use `git.OpenRepository(ctx, repo.RepoPath())` (34 sites) are converted to `gitrepo.OpenRepository`. That conversion is a pure upstream-friendly refactor.

### Seam B - write operations (`services/repository/store` or `modules/repostore`)

Replace all temp-repo code with a small mutation interface. Both backends implement it. The `gitcli` implementation is the existing temp-repo code moved behind the interface. The `asc` implementation is one RPC per operation, executed on the asc server with no working copy (Scion: server does commits, merges, rebases).

```go
package repostore

type Writer interface {
    // Web editor, upload, delete, API contents, wiki pages, repo init/README, generate-from-template
    CreateCommit(ctx context.Context, req CommitRequest) (git.ObjectID, error)
    // req: Parents, Author, Committer, Message, Sign bool, Ops []FileOp{Put{Path, Mode, Blob|Bytes}, Delete, Move, Chmod}

    // PR merge in all styles + PR update-branch; also the mergeability check (DryRun)
    Merge(ctx context.Context, req MergeRequest) (MergeResult, error)
    // req: Style (merge|rebase|rebase-merge|squash|ff-only|fast-forward-only), Base, Head, MergeBase,
    //      Message, Author, Committer, Sign, DryRun, ProtectedPatterns
    // res: Commit, Conflicts []string, ChangedProtectedFiles []string, IsEmpty

    CherryPick(ctx context.Context, req PickRequest) (git.ObjectID, error) // also Revert
    ApplyPatch(ctx context.Context, req PatchRequest) (git.ObjectID, error)

    // Every ref change goes through here: atomic multi-ref CAS, runs Forgejo pre/post hooks
    UpdateRefs(ctx context.Context, upd []RefUpdate, opt PushOptions) error // RefUpdate{Name, Old, New}

    // Lifecycle
    Init(ctx context.Context, repo RepoKey, opt InitOptions) error
    Fork(ctx context.Context, from, to RepoKey) error   // asc: metadata copy, objects are shared
    Delete(ctx context.Context, repo RepoKey) error
    Size(ctx context.Context, repo RepoKey) (int64, error)
}
```

What this removes from the fork's diff burden: `services/repository/files/temp_repo.go`, `services/pull/temp_repo.go`, `merge_*.go`, `update_rebase.go`, the conflict-check half of `patch.go`, the wiki clone/push code, `init.go`, `fork.go` clone, and `branch.go`/`sync_fork.go` pushes. For asc repos, each of these becomes one call. For git repos, the same code runs as today behind `gitcli`.

Note: `Merge` with `DryRun` is also the PR mergeability check. Scion computes this server-side. Forgejo already does it with `git merge-tree --write-tree` (`services/pull/patch.go:401`), so semantics are known.

### Seam C - transport, auth and hooks (outside the Go process)

- **asc protocol**: asc clients and the asc daemon talk to the asc server directly (gRPC/HTTP2), not through Forgejo's HTTP stack. Forgejo stays the identity and permission authority:
  - HTTPS: asc server accepts Forgejo personal access tokens, OAuth tokens and Actions task tokens by calling a new internal endpoint `/api/internal/asc/authz/{owner}/{repo}?mode=` (modelled on `/api/internal/serv/command`), with a short positive cache.
  - SSH: add an `asc-authenticate` verb next to `git-lfs-authenticate` in `cmd/serv.go` and `modules/ssh`. It mints a short-lived JWT (same mechanism as `lfs.Claims`) that the asc client then presents to the asc server.
- **Hooks**: on push, the asc server uploads objects first (immutable, unreferenced objects are harmless, so no quarantine), then calls `POST /api/internal/hook/pre-receive/{owner}/{repo}` with `HookOptions` (`GitQuarantinePath` empty, `IsWiki`, `PushTrigger`), does the ref CAS, then calls `/hook/post-receive`. `routers/private/hook_pre_receive.go` reads the new commits through Seam A, so branch protection, protected files, signed-commit rules and quota keep working without change. `/hook/proc-receive` maps to asc's "push to `refs/for/<branch>`" for AGit-style PR creation.
- **Events**: `PushUpdates` (`services/repository/push.go`) stays the single fan-out point for webhooks, Actions, branch table and PR re-test.

### What must keep working through a git-protocol bridge

The user decided asc changesets are the model and git is derived data. These consumers still speak git and need a bridge:

| Consumer | Need | Bridge behaviour |
|---|---|---|
| Legacy `git clone/fetch` users, IDEs, scripts | smart HTTP + SSH upload-pack | Serve from the derived git mirror |
| `git push` from legacy users (transition) | receive-pack | Receive into a quarantine, import commits to changesets, call `UpdateRefs` (hooks run once, from asc) |
| `actions/checkout` in existing workflows | upload-pack + LFS | Serve from the mirror, until workflows switch to `asc-checkout` |
| Pull mirrors (`services/mirror/mirror_pull.go`) and migrations | `git fetch` from external remotes | Fetch into a bridge workspace, import, `UpdateRefs` |
| Push mirrors (`mirror_push.go`) | `git push` to external remotes | Export, then push from the mirror |
| Submodule consumers (ASC: `packages/soma/sdk`) | Other repos referencing this repo by git SHA | Export must be **deterministic**, so a changeset always maps to the same git SHA |
| GPG/SSH signed commits | Signature covers git commit bytes | Store the original git commit header and signature on import, so export reproduces the byte-identical commit. asc-native commits get asc signatures; the export can be signed by the server key (Forgejo already signs merges, `services/asymkey/sign.go`) |
| REST API `/git/commits|trees|blobs|refs` (`routers/api/v1/repo/git_ref.go`, `tree.go`, `blob.go`) | IDs | Return asc IDs for asc repos; add `?format=git` to return mapped git SHAs |

Bridge design (simple version): a **derived bare git repo per asc repo**, built incrementally by `asc-git-bridge` from changesets (changeset -> git commit, tree -> git tree, blob -> git blob, with a persisted `asc_id <-> git_sha` mapping table in the asc metadata store). It is a cache. It can live on cheap disk, be deleted and be rebuilt, and is enabled per repo (`git_bridge = off | lazy | eager`). Serving then uses the stock `git upload-pack` code in `githttp.go` / `cmd/serv.go` against the mirror path. This keeps `githttp.go`, `cmd/serv.go` and `services/mirror` almost unchanged: they operate on `bridgePath(repo)` instead of `RepoPath()`.

## 3. Actions and Turborepo CI

Forgejo side (small changes):
1. Add `asc` fields to the Actions context (`services/actions/context.go`): `asc_server_url`, `changeset` (asc ID), and keep `sha` as the mapped git SHA while the bridge is on.
2. Ship an `asc-checkout` action (composite or Go/JS) that installs `asc` and runs `asc clone --virtual --at <changeset>` (FUSE on Linux runners). Files are fetched on first access from the machine-local shared cache and the asc server. No submodule clone, no LFS smudge.
3. `paths:` filters in `modules/actions/workflows.go` already call `GetFilesChangedSinceCommit`. Through Seam A they become one `ChangedPaths` RPC, with no history walk.
4. New API (asc server, proxied or linked from Forgejo): `GET /repos/{o}/{r}/asc/changed?base=&head=` and `GET .../asc/merge-base`. A small `asc affected --turbo` command maps changed paths to workspace packages using the package graph (read from `package.json`/`turbo.json`/`Cargo.toml`/`pyproject.toml` blobs only), so ASC's `deploy-affected.yml` stops needing `fetch-depth: 0`.
5. Turbo hashing without hydrating: asc trees already store content hashes per file. `asc turbo-hash <package>` can produce inputs for `turbo run --filter` / a turbo remote cache keyed by asc tree IDs. The turbo remote cache can live in the same object store (Forgejo `storage.Actions` or the asc bucket).

CI details belong in the CI research. The Forgejo fork only needs items 1-4.

## 4. Fork plan

### 4.1 Principles

- Fork from `v16.0.6` (what production runs) and move to `v17.x` once the seams exist. Keep the fork as a **patch stack**, not a long-lived divergent branch: `asc/v16` = upstream tag + N ordered commits, rebased on each upstream release with `git rebase --onto`.
- Order the patches so the early ones are **upstreamable refactors** (pure moves, no behaviour change). Upstream Gitea/Forgejo already started "hide repo path" (#28937, #29126). Whether upstream accepts a full backend interface is not verified. Each refactor upstream shrinks the patch stack.
- New code lives in **new packages** (`modules/git/ascclient`, `modules/repostore`, `services/ascbridge`, `routers/private/asc.go`). New packages never conflict on rebase.
- Feature flag per repository: new column `repository.storage_backend` (`git` default, `asc`). Global switch `[repository] DEFAULT_STORAGE_BACKEND` and `[asc] SERVER_URL, TOKEN, BRIDGE_DEFAULT`. Fallback alternative with zero schema change: `object_format_name = 'asc'`.

### 4.2 Order of work

| Phase | Work | Packages touched | Risk |
|---|---|---|---|
| 0 | Fork repo on ASC Forgejo, CI for the fork (`make test-backend`, sqlite integration tests). Record baseline test pass list | none | low |
| 1 | **Path hiding refactor.** Route all 34 `git.OpenRepository(ctx, path)` callers through `gitrepo.OpenRepository(ctx, repo)`. Replace `RepoPath()` uses that only open a repo. Change `repoPath string` signatures in `blame.go`, `diff.go`, `log_name_status.go` to `*Repository` | modules/gitrepo, modules/git, ~40 caller files | low, upstreamable |
| 2 | **Seam A.** Add `Backend` interface; move current implementation into `modules/git/gitcli`; `Repository` dispatches to `backend`. No behaviour change; all tests must still pass | modules/git only | medium (big diff, but in one package) |
| 3 | **Seam B.** Introduce `repostore.Writer`; move temp-repo code behind its `gitcli` implementation: web editor (`services/repository/files`), merge/rebase/squash/update (`services/pull`), conflict check, wiki, init/generate, fork, branch create/sync | services/repository/files, services/pull, services/wiki, services/repository | medium |
| 4 | Remove remaining raw `git.NewCommand` outside `modules/git` (indexer, gitgraph, contributors graph, release, asymkey, doctor, gitdiff): add the missing Backend methods or move the call into `gitcli` | modules/indexer/code, services/* | low-medium |
| 5 | **asc backend.** `modules/git/ascclient` (read RPCs, aggressive caching by immutable ID) and `repostore` asc implementation. Add `storage_backend` column + migration. `gitrepo.OpenRepository` selects backend. Integration test suite runs twice (git + asc) | new packages, models/repo, modules/gitrepo | high (the real work, mostly in Rust server) |
| 6 | **Hooks + auth for asc.** `/api/internal/asc/authz`, `asc-authenticate` SSH verb, asc server -> `/hook/pre-receive|post-receive`. `HookOptions` gains `Changesets` if needed | routers/private, cmd/serv.go, modules/ssh, modules/private | medium |
| 7 | **Git bridge.** `services/ascbridge`: derived mirror, mapping table, upload-pack serving via existing `githttp.go`/`cmd/serv.go` against `bridgePath(repo)`, receive-pack import path, mirrors and migrations through the bridge. LFS API served from asc blobs | routers/web/repo/githttp.go, cmd/serv.go, services/mirror, services/migrations, services/lfs | medium-high |
| 8 | **Repo lifecycle.** create/fork/transfer/rename/delete/adopt/size/gc/doctor for asc repos (`services/repository/{create,fork,transfer,delete,adopt,check}.go`, `modules/repository/create.go getDirectorySize`, `services/doctor`). Fork becomes O(1) | services/repository, services/doctor | low-medium |
| 9 | **Actions.** asc context fields, `asc-checkout` action, `paths:` via `ChangedPaths`, affected API | services/actions, modules/actions | low |
| 10 | **Migrate asc/dds.** Import git history -> changesets (incl. LFS -> blobs, submodule as gitlink entry), bridge on (eager) so nothing breaks, flip `storage_backend=asc`, move workflows to `asc-checkout`, then set bridge to `lazy` | ops | medium |

Phases 1-4 change no behaviour and can ship to production first. They de-risk everything after them because the test suite proves the seams are complete.

### 4.3 What stays per-repo flaggable

- `storage_backend`: `git` | `asc` (read and write path).
- `git_bridge`: `off` | `lazy` (build on first git request) | `eager` (update on every push). Needed while legacy clients, `actions/checkout`, mirrors or submodule consumers exist.
- `lfs_compat`: serve LFS batch API from asc blobs (default on when bridge on).
- `server_side_merge_only`: reject bridge pushes to protected branches (trunk-based, Scion style).
- Mirrors (pull or push) force `git_bridge != off` for that repo.

### 4.4 Keeping the fork rebasable

- Keep fork commits grouped by seam. One commit per phase where possible, each with tests.
- `modules/git` will conflict most (upstream churn since v16.0.6: 28 files, ~1.6k changed lines). Mitigation: Phase 2 moves code mostly verbatim into `gitcli`, so upstream changes to `modules/git/*.go` are re-applied mechanically to `gitcli/*.go`. Script that check: for each upstream change in `modules/git/X.go`, show the matching `gitcli/X.go`.
- Run upstream's integration tests (`tests/integration`, 370 files, ~57 git-related) against both backends in the fork's CI. A failing upstream test on `asc` is the signal for a missing seam method.
- Try to upstream phases 1, 2 and 4 to Forgejo as "storage backend interface" (it also helps Gitaly-style deployments). If accepted, the remaining fork is new packages plus a few wiring lines.

## 5. Risks and open questions

1. **IDs shown to users.** asc changeset IDs replace git SHAs in UI, API, commit statuses and Actions `sha` for asc repos. External tools that expect 40-hex SHAs (Renovate, some CI integrations) need the bridge's `?format=git` or the mapped SHA. Decide: does `commit_status.sha` store the asc ID (recommended) with the git SHA as an alias lookup?
2. **Deterministic export.** Needed for submodules and signed commits. The import must keep the original git commit bytes (headers, signatures, encoding, mergetag) as changeset metadata.
3. **Unified diff text.** `services/gitdiff` parses `git diff` output. The asc server must produce byte-compatible unified diffs (including rename/copy headers, binary markers, `\ No newline at end of file`). This is deliberate: it keeps review comments and highlight code untouched.
4. **gitattributes / linguist / LFS pointer semantics** must be implemented in the asc server (`Attributes`, `LanguageStats`).
5. **Code search** (bleve/zoekt) reads blobs in bulk; give it a streaming `ChangedPaths + OpenBlob` path or a server-side index.
6. **Repo size and quota** (`modules/repository/create.go`, quota checks in pre-receive): with shared content-addressed storage, "size" becomes logical size; define how quota counts deduplicated data.
7. **Wiki** is a second repository per repo (`.wiki.git`). Model it as a second asc repo, `owner/name.wiki`.
8. **Performance of N+1 RPCs** from Seam A (for example `GetCommitsInfo` per tree entry). Mitigate with batch methods (`LastCommits`, `StatObjects`) and an in-process LRU keyed by immutable IDs.

## Appendix A - `*git.Repository` exported methods (v16.0.6)

AddLastCommitCache AddObjectToIndex AddObjectsToIndex AddRemote CatFileBatch CatFileBatchCheck CheckIfDiffDiffers Close CommitTree CommitsBetween CommitsBetweenIDs CommitsBetweenLimit CommitsBetweenNotBase CommitsByFileAndRange CommitsCountBetween ConvertToGitID CreateAnnotatedTag CreateArchive CreateBranch CreateBundle CreateTag DeleteBranch EmptyIndex ExpandRef Fetch FileChangedBetweenCommits FileCommitsCount FilesCountBetween GetAllCommitsCount GetAnnotatedTag GetBlob GetBranch GetBranchCommit GetBranchCommitID GetBranchNames GetBranches GetCodeActivityStats GetCommit GetCommitByPath GetCommitShortStat GetCommitsFromIDs GetCompareInfo GetDefaultPublicGPGKey GetDiff GetDiffBinary GetDiffFromMergeBase GetDiffNumChangedFiles GetDiffOrPatch GetFilesChangedBetween GetHEADBranch GetHook GetLanguageStats GetLatestCommitTime GetMergeBase GetMergeBaseSimple GetObjectFormat GetPatch GetRefCommitID GetRefType GetRefs GetRefsBySha GetRefsFiltered GetShortStat GetTag GetTagCommit GetTagCommitID GetTagID GetTagInfos GetTagNameBySHA GetTagType GetTagWithID GetTags GetTree GetTreePathLatestCommit GitAttributeChecker GitAttributeFirst GitAttributes HashObject Hooks IsBranchExist IsCommitExist IsCommitInBranch IsEmpty IsObjectExist IsReferenceExist IsTagExist LineBlame ListOccurrences LsFiles LsTree NewBatch NewBatchCheck ReadPatchCommit ReadTreeToIndex ReadTreeToTemporaryIndex RemoveFilesFromIndex RemoveReference RemoveRemote RenameBranch ResolveReference ReverseLineBlame SetReference WalkReferences WriteTree

Classification:
- **Read (Seam A)**: Get*/Is*/Commits*/LsTree/LsFiles/LineBlame/ReverseLineBlame/GitAttribute*/GetLanguageStats/GetCodeActivityStats/CreateArchive/CreateBundle/ListOccurrences/WalkReferences/ResolveReference/ExpandRef/ConvertToGitID.
- **Write (Seam B)**: CreateBranch, DeleteBranch, RenameBranch, CreateTag, CreateAnnotatedTag, SetReference, RemoveReference (-> `UpdateRefs`); index plumbing AddObjectToIndex(es), ReadTreeToIndex, ReadTreeToTemporaryIndex, RemoveFilesFromIndex, EmptyIndex, WriteTree, CommitTree, HashObject (-> `CreateCommit`; not exposed by the asc backend).
- **git-only (bridge or gitcli only)**: AddRemote, RemoveRemote, Fetch, Hooks/GetHook, CatFileBatch/CatFileBatchCheck/NewBatch/NewBatchCheck (become private).

`*git.Commit` exported methods: CacheCommit CommitsBefore CommitsBeforeLimit CommitsBeforeUntil CommitsByRange CommitsCount FileChangedSinceCommit GetAllBranches GetBranchName GetCommitByPath GetFileContent GetFilesChangedSinceCommit GetRepositoryDefaultPublicGPGKey GetSubmodule HasFile HasPreviousCommit IsForcePush Message Parent ParentCount ParentID SearchCommits Summary.

`Tree` / `TreeEntry` / `Blob`: GetBlobByFoldedPath GetBlobByPath GetTreeEntryByPath ListEntries ListEntriesRecursiveFast ListEntriesRecursiveWithSize SubTree; Blob FollowLink(s) GetSubJumpablePathName IsDir IsExecutable IsLink IsRegular IsSubmodule LinkTarget Mode Name Path Size Tree Type; Blob DataAsync GetContentBase GuessContentType Name NewReader NewTruncatedReader Size; Entries CustomSort GetCommitsInfo Sort.

## Appendix B - key file index

| Concern | Path (relative to `D:\dev\forgejo`) |
|---|---|
| Repo handle, cat-file batch | `modules/git/repo_base.go`, `batch.go`, `batch_reader.go` |
| Backend selection point | `modules/gitrepo/gitrepo.go` |
| Object format (per-repo flag precedent) | `modules/git/object_format.go`, `models/repo/repo.go:182` |
| Per-request repo open | `services/context/repo.go:707,976` |
| Web editor temp repo | `services/repository/files/temp_repo.go` |
| PR temp repo, merge styles | `services/pull/temp_repo.go`, `merge.go`, `merge_prepare.go`, `merge_*.go` |
| Conflict check, protected files | `services/pull/patch.go`, `patch_unmerged.go`, `check.go` |
| Wiki | `services/wiki/wiki.go` |
| Push event fan-out | `services/repository/push.go`, `modules/repository/push.go` |
| Pushing env for internal pushes | `modules/repository/env.go` |
| Hook client / server | `cmd/hook.go`, `modules/private/hook.go`, `routers/private/hook_*.go`, `routers/private/internal.go` |
| Smart/dumb HTTP | `routers/web/repo/githttp.go` |
| SSH | `cmd/serv.go`, `modules/ssh`, `routers/private/serv.go` |
| LFS | `modules/lfs/content_store.go`, `services/lfs/server.go`, `models/git/lfs.go`, `modules/git/pipeline/lfs.go` |
| Object storage (S3/local) | `modules/storage/storage.go`, `local.go`, `minio.go` |
| Actions workflow detection | `modules/actions/workflows.go`, `services/actions/notifier_helper.go` |
| Actions context/token | `services/actions/context.go`, `task.go`, `auth.go` |
| Code indexer | `modules/indexer/code/git.go`, `bleve/`, `elasticsearch/`, `zoekt/` |
| Diff rendering | `services/gitdiff/gitdiff.go` |
| Blame | `modules/git/blame.go`, `repo_blame.go`, `routers/web/repo/blame.go` |
| Archives | `modules/git/repo_archive.go`, `services/repository/archiver/archiver.go` |
| Mirrors | `services/mirror/mirror_pull.go`, `mirror_push.go` |
| Migrations | `services/repository/migrate.go`, `services/migrations/gitea_uploader.go` |
| Fork / create / init | `services/repository/fork.go`, `create.go`, `init.go`, `generate.go` |
| AGit | `services/agit/agit.go`, `routers/private/hook_proc_receive.go` |
| Signing | `services/asymkey/sign.go`, `models/asymkey/` |
| Branch DB cache | `models/git/branch.go` |
| REST git endpoints | `routers/api/v1/repo/git_ref.go`, `tree.go`, `blob.go`, `commits.go`, `file.go`, `compare.go` |
