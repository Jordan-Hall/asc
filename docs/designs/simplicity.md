# asc: simplicity-first architecture

Status: design proposal, 2026-10-22. Angle: **the smallest number of crates, services and moving parts that still meets every fixed requirement**, with an MVP that ships cheap worktrees, lazy checkout and CI affected-detection in weeks.

Inputs: `docs/research/{forgejo,prior-art,turbo-ci,rust-stack,asc-profile}.md`. Where this design deviates from a research recommendation, the deviation and its reason are stated inline and marked **[simplification]**.

---

## 0. The design on one page

**Three deployables, seven crates, one proto package (`asc.v1`).**

| Deployable | What it is | Where it runs |
|---|---|---|
| `asc` | A single Rust binary. It contains the CLI, the per-machine daemon (`asc daemon`), the server (`asc serve`) and the git remote helper (`git-remote-asc`, chosen by argv[0]). | Dev machines, CI runners, the forge host |
| Forgejo fork | Upstream v16.0.6 plus a patch stack: an authz endpoint, hook glue, then the read backend (Seam A) and the writer (Seam B). | Forge host |
| `asc-checkout` action | A composite action that runs `asc clone` in CI. | Forgejo Actions |

**The eight decisions that keep it small**

1. **A remote is a store or a server, nothing else.** `Objects` (immutable, content-addressed) plus `Refs` (compare-and-swap). The same `Engine` runs over a local directory, an S3 bucket, or a gRPC client to `asc serve`. Local-only mode, serverless-bucket mode and client/server mode are therefore one code path with three constructors.
2. **The machine cache is a directory, not a service.** All worktrees, the CLI and the daemon share `$ASC_HOME/cache` through the filesystem: atomic rename-into-place, content-addressed names, no locking protocol. The daemon exists only because a virtual filesystem needs a long-lived process. CI's plain mode needs no daemon at all.
3. **Every working copy has a real `.git` projection, and the git index *is* asc's staging area.** Turbo, Biome, IDEs and `git status` work unchanged. There is one staging area, not two. `asc config` reads and writes git config files. Credentials come from `git credential fill` (Git Credential Manager).
4. **Tree entries carry the git blob SHA-1 and the file size, and trees carry their own git tree SHA-1.** This makes git export, the projection index, turbo hashing and index-vs-HEAD comparison pure metadata operations with no content reads.
5. **A changeset holds both its change list (Bonsai) and its root tree id.** **[simplification]** Mononoke derives the root tree. Hashing it in removes the "warm ref before checkout" step and the most common derived type, and the server validates `apply(p1.root, changes) == root` on ingest. The change list is kept because it makes path-filtered log, pushrebase conflict checks and path history O(change) without tree walks.
6. **Windows ProjFS is the first virtual filesystem; CI starts with plain full checkouts from a warm host cache.** **[simplification]** ASC's tree is 4,544 files / 119 MiB. A reflink-or-copy from the runner's shared cache is ~1–2 s with no FUSE privileges, no daemon in the container, and turbo works natively. FUSE (Linux) follows as a later unit; macOS ships with plain mode (APFS clones).
7. **The MVP does not need the Forgejo fork.** `asc serve` first runs in *mirror mode*: Forgejo's git repo stays the source of truth, the asc server imports every push (incremental, deterministic), and `asc push` exports changesets to byte-identical git commits and pushes them with the user's own token. Developers and CI get lazy worktrees and server-side diffs immediately. The truth then flips to asc (Stage 2) and Forgejo moves to the asc read/write backends (Stages 3–4).
8. **Forgejo stays the identity authority, with no token minting.** Clients send a Forgejo PAT, OAuth token or Actions task token. `asc serve` asks Forgejo's internal API "can this token read/write this repo" and caches the answer for 60 s.

**What we explicitly do not build in v1** (each is a later, additive unit if measurement asks for it): packfile-style segment packing of small objects, zstd dictionaries, skip lists (an in-memory commit graph with generation numbers suffices at 1M commits), regional caches, JWT/SSH `asc-authenticate`, jj-style first-class conflicts and op-log, workspace sync, macOS NFS/FSKit, `node_modules` snapshots, a server-run `turbo ls` package graph, and bucket-only ref CAS for multi-writer serverless.

---

## 1. Requirements traceability

| Fixed requirement | Where it is met |
|---|---|
| New changeset model, content-addressed, git as derived data | §2 data model; §5 derived data (`GitCommit`); §11 import |
| Full storage replacement in Forgejo | §12 stages 2–4 (asc is truth from Stage 2; Seam A/B remove git reads/writes) |
| Rust client, daemon, server | One Rust binary `asc` (§3) |
| `asc` mirrors git commands and flags | §9 command map |
| S3-compatible and local storage; works without a server | §3 `Objects`/`Refs`; §10 local-only mode |
| Lazy materialisation on checkout/switch/branch | §7 ProjFS/FUSE; switch updates placeholders from metadata |
| Diffs server-side or from metadata | §4 `Diff` RPC; tree diff with hash pruning |
| Cheap worktrees | §6 workspaces: one index file + a registry row per worktree |
| Faster Turborepo CI | §8: projection, `asc diff --remote` plan job, turbo remote cache on the same store, warm-cache checkouts |

---

## 2. Data model (crate `asc-core`, pure, no I/O, no async)

### 2.1 Identifiers

- BLAKE3, 32 bytes, **keyed per kind** with `blake3::Hasher::new_derive_key("asc v1 <kind>")`. Kinds: `file`, `chunk`, `tree`, `changeset`, `tag`.
- Typed newtypes, no shared base type: `FileId`, `ChunkId`, `TreeId`, `ChangesetId`, `TagId`, plus `GitOid([u8; 20])`. Displayed as 64 lowercase hex; abbreviations of 12 or more characters are accepted, as with git.
- A `FileId` is always BLAKE3 over the **whole file content**, whatever its size. Chunking is a storage detail below the id, so large and small files are the same kind of thing to every caller.

### 2.2 Encoding

- Canonical encoding: `postcard` 1.x with a leading format-version byte. Only structs and sorted `Vec`s are allowed (no maps with nondeterministic order; `BTreeMap` is allowed).
- The bytes hashed are the bytes stored and the bytes sent. Protobuf wraps them as opaque `bytes` on the wire.
- Property test: `decode(encode(x)) == x` and `encode(decode(b)) == b` for arbitrary values.

### 2.3 File content

```rust
enum FileBody {                       // stored under o/f/<FileId>
    Inline(Bytes),                    // files <= 4 MiB
    Chunked(Vec<(ChunkId, u32 /*len*/)>), // files > 4 MiB, FastCDC v2020 512K/1M/4M
}
```

- FastCDC chunking replaces git LFS natively: rewritten JSONL training sets dedupe across versions, and hydration can stream by chunk.
- Bytes are stored exactly as committed. There is no EOL or filter conversion in v1. ASC's files are all LF and its `.gitattributes` is comments only.

### 2.4 Trees

```rust
struct Tree {                     // o/t/<TreeId>
    git: GitOid,                  // this tree's git tree SHA-1 (pure function of entries)
    entries: Vec<Entry>,          // sorted by name bytes, unique
}
struct Entry {
    name: Name,                   // validated: no '/', NUL, ".", "..", ".git"
    kind: Kind,                   // File | Exec | Symlink | Dir | Gitlink
    id: [u8; 32],                 // FileId for File/Exec/Symlink, TreeId for Dir, zero for Gitlink
    size: u64,                    // file size; for Dir the total bytes beneath it
    git: GitOid,                  // git blob SHA, git tree SHA, or gitlink commit SHA
}
```

- The tree id depends only on content, never on path or history, so identical directories share one object everywhere.
- `git` fields are pure functions of content, so including them keeps ids deterministic. They make three things metadata-only: git export, writing the projection index, and turbo's file hashes.
- Directories are not split in v1 (ASC's largest directory is small). A split format is a future format version.
- **Case collisions** (`A.ts` and `a.ts` in one directory) are rejected at ingest for repos marked `windows_safe` (the default). ProjFS is case-insensitive.

### 2.5 Changesets

```rust
struct Changeset {                // o/c/<ChangesetId>
    parents: Vec<ChangesetId>,    // first parent first
    root: TreeId,                 // [simplification] hashed in; validated on ingest
    author: Signature,            // name, email, unix time, tz offset
    committer: Signature,
    message: String,
    extra: BTreeMap<String, Bytes>, // e.g. "git.commit" = original raw git commit bytes (import)
    changes: Vec<(RepoPath, Change)>, // sorted; relative to parents[0]
}
enum Change {
    Write { file: FileId, kind: Kind, size: u64, git: GitOid, copy_from: Option<RepoPath> },
    Gitlink { git: GitOid },
    Delete,
}
```

- A root changeset has `changes` relative to the empty tree.
- A merge changeset records the resolved paths relative to `parents[0]`, as Bonsai does.
- **Ingest validation** (server and local engine): every referenced object exists, `apply(parents[0].root, changes) == root`, and paths are valid.
- `copy_from` is set only by `asc mv`. Import does not guess renames.

### 2.6 Large files and the end of LFS

- Files over 4 MiB are chunked (2.3). There are no pointer files and no LFS server for asc-native repos.
- **Import of historical LFS files:** content is resolved from Forgejo's LFS store and stored as normal chunked content. The tree entry's `git` field keeps the SHA-1 of the *pointer blob* that git saw, so exported trees stay byte-identical. The pointer bytes are stored as a derived override object `d/gitblob/<sha1>`. The bridge emits the pointer; asc clients see the real file.

### 2.7 Submodules

- The data model has a `Gitlink` kind so historical commits import faithfully and export byte-identically.
- **Migration absorbs `packages/soma/sdk` into the monorepo** at cut-over: one changeset replaces the gitlink with the sdk's tree at the pinned commit. The sdk's own history is imported as a separate asc repo `asc/soma-sdk`, which is a cheap metadata-only import because objects dedupe within one namespace (§3.3).
- `asc` never recurses into gitlinks. It shows them as empty directories with a one-line warning. This removes `submodules: recursive` from all 17 CI checkouts.

---

## 3. Storage (crate `asc-store`)

### 3.1 Two traits

```rust
#[async_trait]
trait Objects: Send + Sync {
    async fn get(&self, key: &ObjKey) -> Result<Option<Bytes>>;
    async fn put(&self, key: &ObjKey, bytes: Bytes) -> Result<()>;   // create-if-absent, idempotent
    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>>;
}

#[async_trait]
trait Refs: Send + Sync {
    async fn get(&self, repo: &RepoName, name: &RefName) -> Result<Option<ChangesetId>>;
    async fn list(&self, repo: &RepoName, prefix: &str) -> Result<Vec<(RefName, ChangesetId)>>;
    /// Atomic: all updates apply or none. `expected = None` means "must not exist".
    async fn update(&self, repo: &RepoName, ups: &[RefUpdate]) -> Result<(), RefConflict>;
    async fn log(&self, repo: &RepoName, name: &RefName, limit: u32) -> Result<Vec<RefLogEntry>>;
}
```

`ObjKey` is a typed enum (`File(FileId) | Chunk(ChunkId) | Tree(TreeId) | Changeset(ChangesetId) | Tag(TagId) | Derived(&'static str, [u8;32]) | GitBlobOverride(GitOid)`), never a free string.

### 3.2 Implementations (all of them)

| Impl | Backing | Used for |
|---|---|---|
| `BucketObjects` | `object_store` 0.14 (`LocalFileSystem`, `AmazonS3` for S3/MinIO/R2/Garage/Ceph, `InMemory` for tests) | Server storage, local-only repos, serverless remotes. Values are zstd level 3 frames, no dictionaries in v1. |
| `CacheDir` | Plain files under `$ASC_HOME/cache` | The per-machine shared cache. File content is stored **uncompressed** so ProjFS can copy it, FUSE can pass it through and plain mode can reflink it. Metadata objects are stored as their canonical bytes. |
| `Tiered<A, B>` | Read A, on miss read B and fill A; write-through to both | Client view: `Tiered<CacheDir, Remote>` |
| `SqlRefs` | `sqlx` 0.9 with SQLite or Postgres (same SQL, two URLs) | Server refs (Postgres, which Forgejo already runs) and machine-local refs (SQLite) |

Refs in a bucket using `If-Match` (`BucketRefs`) are a **later unit**. Garage does not document conditional writes, and local-only plus client/server already cover "works without a server". `asc init --store s3://...` therefore keeps refs in a local SQLite file: one writer, objects in the bucket.

### 3.3 Bucket layout

```
<prefix>/ns/<ns>/f/<hh>/<FileId>          FileBody (zstd)
<prefix>/ns/<ns>/k/<hh>/<ChunkId>         chunk bytes (zstd)
<prefix>/ns/<ns>/t/<hh>/<TreeId>          Tree
<prefix>/ns/<ns>/c/<hh>/<ChangesetId>     Changeset
<prefix>/ns/<ns>/d/<name>.<ver>/<hh>/<key> derived data (recomputable)
<prefix>/turbo/<repo>/<scope>/<hash>      turbo remote cache artefacts
```

- `<ns>` is a **storage namespace** shared by a repo and its forks. Forks cost nothing, and an object id learned in one unrelated repo cannot be read from another.
- There are no packfiles. Every object is independently fetchable, so transfers resume by asking again for what is missing.

### 3.4 Machine layout (`$ASC_HOME`, default `%LOCALAPPDATA%\asc` / `~/.local/share/asc`, configurable to `D:\asc`)

```
cache/                content-addressed objects (evictable copies of remote objects)
store/                objects that exist only here (local-only repos, unpushed commits) - never evicted
state.db              SQLite: repos, local refs, workspaces, journal, git-map cache, cache atime
git/<repo>/objects/   shared git projection objects (commits + trees, plus seeded blobs), alternates target
wc/<ws-id>/git/       per-workspace gitdir (HEAD, index, config, refs)
redirect/<ws-id>/     backing dirs for node_modules, dist, .turbo, target, .venv (§7.4)
daemon.log, asc.sock / \\.\pipe\asc-<user>
```

- Cache GC: LRU by atime with a size cap (default 20 GiB). It evicts only from `cache/`. `asc push` copies `store/` objects upstream, and a later GC removes them once the server confirms it has them.

---

## 4. Engine and server

### 4.1 Engine (crate `asc-engine`)

`Engine<O: Objects, R: Refs>` is the one implementation of repository logic. It runs embedded in the CLI (local-only repos, serverless remotes, and all local operations) and inside `asc serve`.

| Module | Contents |
|---|---|
| `tree` | `apply(root, changes) -> TreeId` (rebuilds only the touched spine), `diff(a, b, pathspec)` with **hash pruning** (equal subtree ids are skipped), lookup and glob walk |
| `commit` | Build and validate changesets, revspec resolution (`HEAD~3`, `^2`, `A..B`, `A...B`, `@{u}`, `rev:path`, id prefixes, git SHAs through the git map) |
| `graph` | In-memory commit graph per repo (`id -> (gen, parents)`), loaded lazily from SQL on the server or derived locally. `merge_base`, `is_ancestor`, `log` (rev ranges, `--first-parent`, path filters through `changes`) |
| `derive` | Derived-data framework (§5) |
| `diff` | `imara-diff` line diff. Output: name-only, name-status, stat, numstat and unified patch, **byte-identical to git** (golden tests) |
| `merge` | Three-way tree merge with hash pruning. Only paths changed on both sides get a content merge, via `gix-merge`'s builtin text driver (pinned with `gix`), which produces git's conflict markers. Also `cherry_pick`, `revert`, `rebase(stack, onto)` and `land` (pushrebase) |
| `blame` | Walks the file's history using `changes` lists, line-diffs successive versions, memoised as derived data per (changeset, path) |

`graph` stays in memory: 1M commits at about 80 bytes each is about 80 MB per hot repo. Generation numbers plus a priority-queue walk give `merge_base` without skip lists. **[simplification]** Skip lists can be added later as a derived type without changing anything else.

### 4.2 Server: `asc serve`

- One process, one port. `tonic` gRPC and `axum` HTTP share a router.
  - gRPC: the `asc.v1.Repo` service below.
  - HTTP: turbo remote cache `/v8/*`, `/healthz`, `/metrics`, `/.well-known/asc`.
- Config file `asc-serve.toml`: storage URL (`file:///srv/asc` or `s3://bucket/prefix` plus credentials), refs DB URL (`postgres://…` or `sqlite://…`), Forgejo base URL and internal token, and per-repo mode (`mirror` or `primary`).
- Stateless apart from caches, so it can scale horizontally behind a load balancer: every ref change is a single-row compare-and-swap in Postgres. The in-memory graph is refreshed from SQL on a CAS miss.

### 4.3 RPC surface (`proto/asc/v1/repo.proto`, one service)

| Group | RPC | Notes |
|---|---|---|
| Objects | `HasObjects(keys) -> bitmap` | Batch existence check, used for resumable upload |
| | `GetObjects(stream keys) -> stream (key, bytes)` | Batched (256 keys or 16 MiB), clients pipeline up to 8 streams. Chunked files arrive chunk by chunk |
| | `PutObjects(stream (key, bytes)) -> summary` | The server re-hashes and validates every object |
| Refs | `ListRefs(repo, prefix)` | |
| | `UpdateRefs(repo, updates[], land: bool) -> new ids` | Atomic multi-ref CAS. Runs Forgejo pre-receive before the CAS and post-receive after it. `land=true` does pushrebase on a non-fast-forward when changed paths don't overlap (§4.4) |
| Resolve | `Resolve(repo, revspecs[]) -> ids` | Includes git SHA → asc id |
| | `GitMap(repo, ids[] or shas[])` | Both directions |
| History | `Log(repo, range, pathspec, first_parent, limit, skip)` | Returns decoded headers, not postcard |
| | `MergeBase(repo, a, b)`, `IsAncestor(repo, a, b)` | |
| | `Blame(repo, rev, path, line_range)` | |
| Diff | `Diff(repo, a, b, merge_base: bool, pathspec, format)` | `format`: `NAMES`, `STATUS`, `STAT`, `PATCH`. Used by CI with no checkout, by Forgejo PR pages and by `asc diff --remote` |
| Read (for Go) | `ReadChangeset`, `ReadTree(repo, rev, path, recursive)`, `ReadFile(repo, rev, path, range) -> stream`, `Stat(repo, rev, paths[])` | Protobuf-decoded so the Go fork never parses postcard |
| Writes (server-side) | `Commit(repo, parent, file_ops[], message, author, expected_ref)` | Forgejo web editor, uploads, contents API |
| | `Merge(repo, base_ref, head, style, message, dry_run)` | `style`: merge, squash, rebase, rebase-merge, fast-forward-only. `dry_run` is the mergeability check |
| | `Apply(repo, onto, changeset, mode)` | `mode`: cherry-pick or revert |
| Bulk | `Archive(repo, rev, format, pathspec) -> stream` | Added with Seam A. Grep, language stats and attributes come later in the same service |

Negotiation follows Scion and needs no special RPC. The client fetches the root tree, then each needed level of trees in parallel batches, then the file bodies it actually needs. Branch switch diffs trees locally with hash pruning, and the trees are tiny and cached. **[simplification]** No `TreeDiff` or `Prefetch` RPCs until measurement asks for them.

### 4.4 Land (pushrebase)

`UpdateRefs(land=true)` with a stack based on an old trunk tip:

1. Compute the paths changed on the server between the stack's base and the current tip: the union of the `changes` lists.
2. If they don't intersect the stack's changed paths, rebase each changeset onto the tip (new changesets, same `changes`, new `root` via `apply`), then CAS the ref.
3. On intersection, reject with the conflicting paths. The client runs `asc pull --rebase`.

This removes push races for 20 agents committing to one trunk (bursts of 50 commits a minute). Enabled per branch with server config `land = ["master"]`. Elsewhere, `asc push` keeps git's fast-forward rule.

---

## 5. Derived data

```rust
#[async_trait]
trait Derived: Serialize + DeserializeOwned + Send + Sync + Sized {
    const NAME: &'static str;     // "gen", "gitcommit", "blame", ...
    const VERSION: u32;
    async fn derive(e: &Engine, cs: &Changeset, parents: &[Self]) -> Result<Self>;
}
async fn get<D: Derived>(e: &Engine, id: ChangesetId) -> Result<D>;
```

- `get` finds the underived ancestors iteratively (topological order, no recursion), derives oldest first, and memoises each result at `d/<NAME>.<VERSION>/<id>`. Bumping `VERSION` recomputes lazily.
- **Eager types**, derived before `UpdateRefs` moves a ref (the "warm ref" rule):
  - `gen`: generation number and parents, also written to the SQL graph table.
  - `gitcommit`: the git commit SHA-1. Bytes come from `extra["git.commit"]` when imported, otherwise from canonical rendering: tree = `root.git`, parents = parents' `gitcommit`, author and committer verbatim. The `asc_id <-> git_sha` pair goes into the SQL map table.
- **Lazy types:** `blame` per (changeset, path), and later `pathhistory`, `copies` and `turbo-graph`.
- A new type is one `impl Derived` plus registration. The write path never changes.

---

## 6. Daemon, workspaces and worktrees

### 6.1 The daemon is small

`asc daemon` is one per user per machine. The CLI starts it on first need. On Windows it registers as a per-user logon task; on Linux it is a systemd user unit.

| It does | It does not |
|---|---|
| Hosts each virtual workspace (ProjFS root or FUSE mount) | Proxy history RPCs. The CLI calls the server directly |
| Keeps each workspace's modified-paths journal | Own the cache. The cache is a shared directory |
| Prefetches in the background and GCs the cache | Act as a turbo cache proxy (later unit) |
| Dehydrates idle hydrated files on Windows (§7.1) | |

IPC is a small `asc.v1.Workspace` gRPC service over `interprocess` local sockets: a named pipe with a current-user DACL on Windows, a mode-0600 socket on Unix. Methods: `Mount`, `Unmount`, `Switch(ws, from, to)`, `Journal(ws) -> paths`, `ClearJournal(ws, paths)`, `Dehydrate(ws, pathspec)` and `Stats(ws)` (hydration counters, used by the turbo contract tests).

VFS callbacks never cross IPC. They read trees from an in-process `quick_cache` over `CacheDir` and fetch misses through `Tiered`.

### 6.2 Workspace = registry row + gitdir + mount

```
state.db: workspaces(id, repo, path, mode{virtual,plain}, base_cs, branch, sparse, created)
$ASC_HOME/wc/<id>/git/            the projection gitdir (§8.1)
<path>/.git                      gitfile: "gitdir: $ASC_HOME/wc/<id>/git"
```

- `asc clone URL dir`: register the repo, fetch refs, fetch the head changeset and root tree, create the workspace, write the projection, mount. It downloads O(1) data, with a target under 2 s.
- `asc worktree add <path> [-b <branch>] [<rev>]`: the same, with no network if the revision is cached. Cost per worktree: one index file (about 0.5 MB for 4.5k entries) plus one row. This replaces about 91 MB per agent worktree today and 27 GB of per-worktree submodule gitdirs.
- `asc worktree list | remove | prune | lock | move` mirror git.
- Every workspace, of every repo, shares `cache/` and `git/<repo>/objects`. Content is stored once per machine.

### 6.3 Status: O(changed files)

- **Virtual mode:** dirty set = journal ∪ (index entries whose `git` differs from HEAD's tree entry). Journal paths are confirmed against disk (ProjFS on-disk state; FUSE overlay presence) and only those files are hashed. Unmaterialised directories cannot hold untracked files, so there is no tree walk.
- **Plain mode:** a git-index-style stat check over the materialised files, kept warm by `notify` while the daemon runs, and by a full stat pass otherwise (4.5k files: milliseconds). `.gitignore` handling uses the `ignore` crate.
- The journal drops paths under redirections and `.gitignore`d build-output paths, so `bun install` doesn't flood it.

---

## 7. Virtual filesystem (crate `asc-vfs`, three modules behind cfg)

### 7.1 Windows: ProjFS (first, because ASC's developers are on Windows)

- Our own thin layer over `windows` 0.62 bindings. `ProjectedFSLib.dll` is **loaded at runtime**, so `asc.exe` still starts where the optional feature is off. `asc doctor` prints the one-time admin command: `Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart`.
- Callbacks:
  - enumeration and placeholder info are answered from tree metadata (size, timestamps, attributes);
  - `ContentID` = kind byte + 32-byte id; `ProviderID` = workspace epoch;
  - **file data is served by the `ContentID` recorded in the placeholder, never the current HEAD**;
  - slow fetches use `ERROR_IO_PENDING` + `PrjCompleteCommand`.
- Notifications (created, overwritten, closed-modified, deleted, renamed, hardlink, pre-convert-to-full) append paths to the journal. **They are treated as hints:** at daemon start each workspace is reconciled by checking `PrjGetOnDiskFileState` for journal paths and for Full or Dirty files found by a bounded scan of hydrated directories.
- **Switch:** `diff(old.root, new.root)` from metadata. For each changed path not in the journal: `PrjUpdateFileIfNeeded`, or `PrjDeleteFile` for removals. Unmaterialised paths are skipped. Paths in the journal follow git's switch rules: refuse, or carry the local change with `--merge`. Then bump the epoch and clear the negative path cache. No downloads.
- **Timestamps:** every placeholder reports `mtime = ctime = committer time of the changeset that last wrote it`, rounded to whole seconds. That value is what the projection index records (§8.1).
- **Disk on NTFS:** a hydrated file is a full copy. The daemon dehydrates clean hydrated files in workspaces that have been idle for more than 7 days, and `asc sparse dehydrate <pathspec>` does it on demand. A Dev Drive (ReFS block clone) spike may remove the copy altogether.
- Symlinks use `PrjFillDirEntryBuffer2`, which needs Developer Mode. Otherwise they are materialised as text files with a warning, as git does with `core.symlinks=false`.

### 7.2 Linux: FUSE (later unit; for Linux developers and optional virtual CI)

- `fuser` 0.18. Inode = 64-bit hash of the path, which is stable across restarts with no inode table **[simplification]**. Collisions are resolved by an in-memory side map.
- Writes copy up into an overlay directory per workspace, so the dirty set is exact and needs no notifications.
- Clean files already in `cache/` are served with **FUSE passthrough** (kernel ≥ 6.9): zero copies, zero extra disk.

### 7.3 Plain mode (everywhere, and the CI default)

- Materialises the tree, or a sparse cone (`asc sparse set <globs>`), from `cache/` using `reflink-copy`. Where the filesystem has no reflinks it does a plain copy. Never hardlinks: an editor writing through a hardlink would corrupt the shared cache.
- macOS v1 is plain mode with APFS `clonefile`, which makes worktrees nearly free anyway.

### 7.4 Redirections

Directories named in `.asc/redirections` are kept off the virtual tree. The checked-in default is `node_modules`, `dist`, `.turbo`, `target`, `.venv`, `.next`.

- **ProjFS:** these paths are already plain NTFS once created. asc registers notification mappings with no notifications for each `<package>/<name>` (package dirs are enumerated from the tree at mount and on switch), so they cost nothing in the journal.
- **FUSE:** the name is served as a symlink to `$ASC_HOME/redirect/<ws>/<path>`, which is created on first lookup.

---

## 8. Turborepo and CI

### 8.1 The git projection (hashing without hydration)

The design follows `turbo-ci.md` §2.3, with these specifics:

- `wc/<id>/git/config` sets `core.worktree`, `core.fsmonitor` (an asc hook that returns the journal), `core.hooksPath` (asc shims), `extensions.partialClone=asc`, and `remote.asc.url=asc::<repo>` with `promisor=true`.
- **`index`** is written by asc from tree metadata only. It is a v2 index with a TREE extension built from `Tree.git`. Each entry has the path, mode, `git` SHA-1, size, and mtime/ctime exactly as the VFS reports them (§7.1). On FUSE it also has the path-hash inode. The index timestamp is `max(mtime) + 2 s`, so no entry is racy, and there are no skip-worktree bits. Turbo's `gix-index` stat check then matches every clean file, and **no file is read**.
- **Commits and trees** for the git view are rendered **locally** from asc metadata, since export is a pure function (§5 `gitcommit`). They are written to the shared `git/<repo>/objects` (alternates). Developers get the first-parent chain, generated once per machine. CI gets `merge_base..head` plus a `shallow` file.
- **Blobs:**
  - asc seeds loose objects for `**/package.json`, `turbo.json` and `bun.lock` at HEAD and at the CI base, which are exactly what turbo reads.
  - Everything else is fetched on demand by `git-remote-asc`, a promisor helper that speaks protocol v2 `fetch` for explicit OIDs only and returns an undeltified pack. It maps git SHA-1 → `FileId` through tree metadata and the git map.
- **Staging area:** `asc add`, `asc reset` and `asc restore --staged` edit this index with `gix-index`, and `git add` works too. `asc commit` builds `changes` as index-vs-HEAD (compare `git` fields), maps each staged SHA-1 to a `FileId` (from the local git-map cache; unknown SHA-1s are ingested from the working file), and stores the new file bodies in `store/`.
- **Hooks:** the `pre-commit` and `pre-push` shims refuse `git commit` and `git push` with "use `asc commit` / `asc push`". When `ASC_COMMIT=1` is set by `asc commit`, they hand over to the repo's own hooks (`asc.hooksPath`, default `.husky` or `.githooks` if present).

Contract tests (unit U29) pin the behaviour. On a clean virtual workspace, `turbo run build --dry=json` must produce task hashes equal to a real git clone with **0 hydrations** beyond manifests. They run on every turbo minor release.

### 8.2 CI flow

**Plan job: no checkout, seconds.**

```yaml
plan:
  steps:
    - run: asc diff --remote "$ASC_REPO" --name-only --merge-base "$BASE" "$HEAD" > changed.txt
    - run: bun scripts/ci/plan.ts changed.txt >> "$GITHUB_OUTPUT"
```

- `asc diff --remote` is a single `Diff(format=NAMES)` call.
- **[simplification]** asc does not implement the package graph. ASC's existing `deploy-affected.ts` style script (which already asks turbo for the graph) maps paths to the `any / rust / images / e2e` switches.
- A server-side `turbo-graph` derived type is a later unit if the plan job needs dependents without an install.

**Build and test jobs: `asc-checkout` then turbo.**

```yaml
- uses: asc/asc-checkout@v1     # mode: plain (default) | virtual
  with: { base: ${{ env.BASE }} }
- run: bun install --frozen-lockfile        # host bun cache bind-mounted
- run: turbo run build check-types test --affected
```

- `asc-checkout` runs `asc clone --mode=plain --rev $HEAD --base $BASE`.
- The runner host mounts `/var/cache/asc` (the shared `$ASC_HOME/cache`) into job containers, so a warm checkout is a local copy (reflink where available) of 119 MiB.
- It writes the projection with the base refs, a shallow boundary at the merge base, and `TURBO_SCM_BASE` / `TURBO_SCM_HEAD`. Turbo's own `--affected` is then exact.
- `mode: virtual` (FUSE, needs `/dev/fuse`) follows once U24 lands. Measure first, because plain mode may already be enough at ASC's size.

**Turbo remote cache on the same store.** `asc serve` implements `/v8/artifacts` (`GET`/`HEAD`/`PUT`/`POST` and `/status`, `/events`):

- objects at `turbo/<repo>/<scope>/<hash>`;
- `<scope>` is `trusted` (protected branches) or `pr-<n>`; reads fall back from `pr-<n>` to `trusted`, writes go only to the caller's scope;
- auth uses the same Forgejo tokens;
- signatures are on (`TURBO_REMOTE_CACHE_SIGNATURE_KEY`);
- retention is 14 days for `pr-*` and 60 for `trusted`.

Developers point `TURBO_API` at the same server, which gives them one cache across all their worktrees.

**Do now, independent of asc** (from `turbo-ci.md`): bind-mount the bun cache, merge the duplicate workflow, add `check-types` to every package, add cargo tasks to turbo, delete the comment-only `.gitattributes`, and remove `"env": ["*"]` from `build`.

---

## 9. CLI command map

Legend:

- **L**: local only (CLI + cache + workspace).
- **L+f**: local, fetching missing metadata or content from the remote as needed.
- **S**: server RPC when a server remote exists. Otherwise the same engine code runs locally against the store (local-only or serverless).

Flags and output formats (porcelain v1/v2, `-z`, `--name-only`, `--format`) follow git. Unsupported flags fail loudly with "asc: flag not supported yet", never silently.

| git command | asc behaviour | Where |
|---|---|---|
| `init` | New local-only repo in `store/` with SQLite refs. `--store s3://…` keeps objects in a bucket | L |
| `clone` | Register the repo, fetch refs and the head changeset, create a virtual workspace (plain where unavailable; `--mode`), write the projection. `--sparse`, `--branch` and `--no-checkout` are supported | L+f |
| `status` | Journal plus index-vs-HEAD (§6.3); `--porcelain[=v2]`, `-s`, `-z`, `-uno` | L |
| `add`, `rm`, `mv` | Edit the git index; `mv` records `copy_from` | L |
| `restore`, `reset` | Index and working copy from the tree; `--soft/--mixed/--hard`; `--hard` updates placeholders (virtual) | L+f |
| `commit` | Changeset from index vs HEAD, stored in `store/`, local branch moved. `-m`, `-a`, `--amend`, `--fixup`, `-S` (signs into `extra`). Runs repo hooks | L |
| `switch`, `checkout` | Metadata diff plus placeholder updates; no content download. `-c/-b`, `--detach`, `-- <path>` restores files | L+f |
| `branch` | Local refs in `state.db`; `-r/-a` show remote-tracking; `-d/-D/-m`, `--set-upstream-to` | L |
| `tag` | Lightweight tags are refs to a changeset. Annotated tags (`-a`, `-m`, `-s`) are refs to a small `TagRecord` object (target, tagger, message, signature), stored like a changeset | L (pushed with `push --tags`) |
| `diff` | Working copy or index vs tree: L. Rev vs rev: S (with `--remote` it needs no checkout). All git output formats | L+f / S |
| `log` | S when online (server graph, path filters, `--follow` via `copy_from`); local graph for unpushed commits and local-only repos | S |
| `show` | Changeset header plus diff | S / L+f |
| `blame` | Server `Blame` (derived data); local engine in local-only mode | S |
| `fetch` | Refs plus changeset headers only; trees and files stay lazy. `--prune`, `--all` | L+f |
| `pull` | fetch plus merge, or `--rebase` (local engine; content only for paths changed on both sides) | L+f |
| `push` | `PutObjects` (missing only, resumable) then `UpdateRefs` CAS; land on configured trunks; `--force-with-lease`, `-u`, `--delete`, `--tags` | S |
| `merge` | Local engine three-way merge; conflict markers written to files and stages 1/2/3 to the index, git-compatible | L+f |
| `rebase` | Local engine replay. `--onto`, `--continue/--abort/--skip`, `-i` (todo file, same format as git), `--autosquash` | L+f |
| `cherry-pick`, `revert` | Local engine | L+f |
| `stash` | Snapshot changeset on `refs/stash` with a reflog; `push/pop/apply/list/drop/show` | L |
| `worktree` | Virtual or plain workspace (§6.2) | L |
| `remote` | `add/remove/set-url/-v`. URLs: `https://forge/…` (server via `/.well-known/asc`), `asc+https://…`, `s3://bucket/prefix`, `file:///path` | L |
| `config` | Same files as git (`gix-config`): repo = projection gitdir config, global = `~/.gitconfig`. asc keys live under `asc.*` | L |
| `ls-files`, `ls-tree`, `cat-file`, `rev-parse`, `merge-base`, `rev-list`, `show-ref`, `for-each-ref`, `symbolic-ref`, `hash-object` | Plumbing equivalents. `ls-tree --git-oid` prints git SHA-1s from metadata | L+f / S |
| `grep` | `asc grep` over hydrated files plus server `Grep` (later; until then, local with a warning that it hydrates) | L+f |
| `clean` | Untracked from journal (virtual) or a walk (plain) | L |
| `reflog` | Local `state.db` ref log; `--remote` shows the server log | L |
| `bisect` | Local driver over the server graph | L+S |
| `describe` | Server graph | S |
| `archive` | Server `Archive` | S |
| `sparse-checkout` | Alias of `asc sparse` (plain mode cone; virtual mode prefetch or dehydrate scope) | L |
| `gc`, `maintenance`, `fsck`, `prune` | Cache GC; `fsck` verifies hashes in `cache/` and `store/` | L |
| `submodule` | Prints the absorbed-monorepo notice; `status` lists gitlinks | L |
| `format-patch`, `am`, `apply` | `apply` and `format-patch` locally; `am` later | L |
| `notes`, `filter-branch`, `replace`, `svn`, `p4` | Not supported | – |

asc-only commands, operational only (no new workflow verbs to learn):

- `asc daemon` and `asc serve`;
- `asc import <git-dir|url>` and `asc export --git <dir>`;
- `asc auth login|status`;
- `asc doctor`, `asc sparse dehydrate`, `asc debug stats`.

`git-remote-asc` is the same binary invoked through a hardlink or copy named `git-remote-asc`.

---

## 10. Offline and local-only behaviour

- **Always-online by design, graceful offline.** These work offline:
  - `status`, `add`, `commit`, `diff` against HEAD, `stash`, `branch`;
  - switching between revisions whose trees are cached;
  - reading files already in `cache/`.
- These fail fast with `asc: offline (server unreachable); this needs <RPC>`:
  - `push` and `fetch`;
  - `log` and `blame` beyond local history;
  - reads of unfetched files. On ProjFS the read returns `STATUS_IO_DEVICE_ERROR` and the daemon log names the path.
- **Local-only repos** (`asc init`) have no server. The engine runs in-process over `store/` + SQLite. Every command in §9 works, including log, blame and merge, using the same code the server runs.
- **Serverless remotes** (`s3://…`, `file://…`): push and fetch move objects with `HasObjects`/`PutObjects` semantics straight against the bucket, and refs use `SqlRefs` on a local file (single writer). Multi-writer serverless needs `BucketRefs` (later unit, MinIO/S3/R2/Ceph only).

---

## 11. Migration and import (crate `asc-git`)

### 11.1 Import

`asc import --git <path-or-url> --repo asc/dds [--incremental]`:

1. `gix` walks commits in topological order. For each commit it diffs the git trees against the first parent's, giving `changes`.
2. File bodies are ingested (BLAKE3 + chunking, SHA-1 kept). LFS pointers are resolved through Forgejo's LFS API (§2.6). Gitlinks become `Change::Gitlink`.
3. The changeset is built with `extra["git.commit"]` = the raw commit bytes, which covers GPG signatures and odd headers.
4. **Round-trip check per commit:** the export of the changeset must equal the original git SHA, otherwise the import aborts. The rare tree with non-canonical modes (for example `100664`) gets its raw tree bytes stored as a `GitBlobOverride`-style override.
5. The `asc_id <-> git_sha` map is written.

Scale: 5,502 commits and 104,800 objects (311 MiB packed) plus 26 GB of historical LFS. History import is minutes. LFS content goes in a resumable background pass.

### 11.2 Export (the git bridge)

- `asc export --git <bare-dir> [--refs]` streams `git fast-import` input (blobs, trees, commits, in dependency order), skipping objects the target already has according to the git map.
- **Deterministic:** imported changesets reproduce their original bytes, and native changesets use canonical rendering.
- In mirror mode and Stage 2 the bridge is a **derived bare repo**: deletable, rebuildable, never the source of truth.

### 11.3 Cut-over plan for asc/dds

1. Clean up today, optionally: drop the 4.6 GB unreachable pack and prune worktree submodule copies. This is independent of asc.
2. **Mirror mode:** `asc serve` imports asc/dds and keeps it current from a Forgejo push webhook (`git fetch` into the server's mirror dir, then incremental import). Developers move to `asc clone` / `asc worktree`. `asc push` exports and pushes to Forgejo with the user's token and `--force-with-lease` semantics, and moves the asc ref only when Forgejo accepts.
3. **Absorb the soma sdk submodule** (one changeset). Import `asc/soma-sdk` separately.
4. **Flip the truth (Stage 2):** asc refs become authoritative. Forgejo's bare repo becomes the derived bridge, kept `eager`.
5. Stage 3–4 Forgejo backends, then set the bridge to `lazy`.

---

## 12. Forgejo fork (Go, `D:\dev\forgejo`, patch stack on v16.0.6)

Stages are ordered so each one ships alone. The Go client is generated from the same `repo.proto` (`buf generate`) into `modules/ascclient`.

| Stage | Change | Source of truth |
|---|---|---|
| **1 (no fork)** | Mirror mode (§11.3) | Forgejo git |
| **2 Truth flip** | (a) `routers/private/asc.go`: `GET /api/internal/asc/authz/{owner}/{repo}?mode=read\|write` with a bearer token → `{user_id, allowed}`, modelled on `/serv/command`. (b) For repos with `storage_backend=asc`, the bare repo's **pre-receive** hook calls `asc serve` `ImportPush(repo, old, new, bridge_path)`: import the new commits (they are already in the bridge's quarantine), then `UpdateRefs` CAS on asc. A CAS failure rejects the push. So the web editor, merges, wiki and legacy `git push` all funnel into asc truth without code changes. (c) `asc push` → `UpdateRefs` → asc calls Forgejo pre-receive (branch protection), CAS, exports to the bridge (`update-ref`, no hooks), then calls Forgejo **post-receive**, which fans out webhooks, Actions and PR re-tests. (d) Migration adds the `storage_backend` column | **asc** (bare repo = derived bridge) |
| **3 Seam A (reads)** | Phase 1 of `forgejo.md`: hide repo paths behind `gitrepo`. Phase 2: move `modules/git` internals to `modules/git/gitcli` behind a private `backend` field. New `modules/git/ascbackend` implements about 30 read methods with `ReadTree`/`ReadFile`/`Log`/`Diff`/`Blame`/`MergeBase`/`Archive`, plus an LRU cache keyed by immutable id. The selection is made in one place, `gitrepo.OpenRepository`. `Diff(PATCH)` output is byte-identical to git, so `services/gitdiff` is unchanged | asc; Forgejo **reads** through asc |
| **4 Seam B (writes)** | `modules/repostore.Writer` (`CreateCommit`, `Merge`, `Apply`, `UpdateRefs`, `Init`, `Fork`, `Delete`, `Size`). The gitcli implementation wraps today's temp-repo code; the asc implementation is one RPC each. Then remove the remaining raw `NewCommand` users (indexer, gitgraph, contributors, release, asymkey, doctor) through Seam A/B. Bridge → `lazy` (built on first legacy git request) | asc; Forgejo reads **and writes** through asc. **Full replacement done** |
| **5 Actions** | `paths:` filters call `Diff(NAMES)`. Add `asc_repo`, `asc_sha` and `asc_base` to the Actions context. Publish the `asc-checkout` action | – |

Commit ids in the Forgejo UI and API are asc ids (64 hex, shown as 12) for asc repos, with the git SHA available via `?format=git` and shown as "git: abc1234" on commit pages, so existing links and statuses keep resolving through the git map. The wiki is a second asc repo (`<repo>.wiki`), as it is a second bare repo today.

---

## 13. Security and auth

- **Identity:** Forgejo only. `asc auth login` stores a Forgejo PAT through `git credential approve`, the same store developers already use. CI uses the Actions task token. The server checks every repo-scoped call through the authz endpoint (Stage 2) and caches `(token hash, repo, mode)` for 60 s. In mirror mode it validates the token against Forgejo's public `/api/v1/repos/{owner}/{repo}` and checks `permissions`.
- **Transport:** TLS on `asc serve`. The local socket is restricted to the current user (pipe DACL / 0600).
- **Integrity:** the server re-hashes every uploaded object and validates changesets (§2.5) and paths (no `..`, `.git`, NUL, Windows-reserved names or case collisions) before anything is referenced. Refs only move by CAS after the Forgejo pre-receive check.
- **Isolation:** per-network storage namespaces (§3.3). Object reads require read access to a repo in that namespace.
- **Signing:** `asc commit -S` stores an SSH or GPG signature over the canonical changeset bytes in `extra`. Imported git signatures stay valid on the bridge because the bytes are byte-identical. Turbo artefacts are HMAC-signed.
- **Supply chain:** pinned crate versions, `cargo deny` (licences: no GPL code; Sapling, Mononoke and EdenFS are ideas only), `cargo audit` in CI.

---

## 14. Observability

- `tracing` everywhere: JSON logs to `daemon.log` and server stdout, optional OTLP export (`ASC_OTLP_ENDPOINT`).
- Server `/metrics` (Prometheus): RPC latency and error histograms per method, bytes in and out, object store latency, ref CAS conflicts, derived-data lag, turbo cache hit ratio.
- Client: `asc debug stats [--json]` reports cache size and hit ratio, per-workspace hydration count and bytes, journal size, and daemon uptime. `ASC_TRACE=1` prints RPC timings for a single command.
- `asc doctor` checks ProjFS/FUSE availability, daemon health, clock skew, cache disk space, credential presence, server reachability, and the S3 conditional-write probe for `s3://` remotes.

---

## 15. Testing strategy

| Layer | How |
|---|---|
| `asc-core` | Unit and `proptest` tests: encoding round-trip and determinism, id stability (frozen golden vectors), git object encoding vs `git hash-object` / `git mktree` outputs |
| `asc-store` | Every `Objects`/`Refs` impl runs one shared conformance suite: `InMemory` and `LocalFileSystem` always; MinIO via `docker compose` under `--features it-s3`; Postgres under `--features it-pg`. Concurrent CAS race test (100 tasks, exactly one winner per round) |
| `asc-engine` | Tree apply and diff vs a naive full-walk oracle (proptest). Merge vs `git merge-file` goldens. `diff` byte-identical to `git diff` over the fixture corpus. merge_base vs `git merge-base` on generated DAGs |
| `asc-git` | Import → export round-trip equality on fixture repos (signed commits, odd modes, LFS, gitlinks). The `#[ignore]` test `asc_roundtrip` imports `D:\dev\monorepo` read-only and asserts all 5,502 SHAs |
| `asc-vfs` | Per OS: mount, enumerate, read (hydration counter), write → journal, switch with dirty files, daemon restart reconcile, case-insensitivity. ProjFS tests run on a Windows runner with ProjFS enabled; FUSE tests in Linux CI with `/dev/fuse` |
| CLI | `tests/cli/*.t`: snapshot tests that run the same script under `git` and `asc` against identical repos and diff the outputs (status, log, diff, branch, merge conflicts) |
| Turbo contract | `tests/turbo/`: pinned turbo version, golden hash equality with a real clone, 0 extra hydrations, `--affected` equality (§8.1) |
| Server | In-process `asc serve` over `InMemory` + SQLite. gRPC tests for every RPC, auth-denied paths, land conflicts |
| Forgejo | Its integration suite runs against both `storage_backend` values (about 57 git-related test files) |

---

## 16. Phased roadmap

| Phase | Weeks | Ships | Value to ASC |
|---|---|---|---|
| **P0 Foundations** | 1–2 | U1–U14: core, store, engine (tree, commit, graph, derive, diff), local-only `asc init/add/commit/log/diff` in plain mode | Proves the model; `asc import` of ASC round-trips |
| **P1 Lazy worktrees on Windows (MVP-1)** | 3–5 | U17–U19, U21–U23, U25–U27: import, export, projection, rpc, ProjFS, daemon, CLI core verbs, `asc serve` in mirror mode | The 50 worktrees become metadata; the 58 GB `.git` and 27 GB of submodule gitdirs go away for asc users; turbo hashes without hydration |
| **P2 CI (MVP-2)** | 5–7 | U28 turbo cache endpoint, U29 contract tests, U30 `asc-checkout` (plain mode), plan job via `asc diff --remote` | PR runtime from about 15–25 min wall to single-digit minutes (with the non-asc fixes); no `fetch-depth: 0`, no submodule checkouts |
| **P3 asc is the truth** | 7–9 | U31–U33: Go client, authz endpoint, pre-receive import hook; land/pushrebase (U15); soma sdk absorbed | Agents push without races; Forgejo bare repo is a rebuildable cache |
| **P4 Forgejo reads through asc** | 9–13 | U34–U36 Seam A | Forgejo no longer reads git |
| **P5 Forgejo writes through asc** | 13–17 | U37 Seam B, U38 Actions; bridge set to `lazy` | **Full storage replacement** |
| **P6 Breadth** | 17+ | U16 blame derived data, U20 promisor helper hardening, U24 FUSE + virtual CI, macOS, `BucketRefs`, zstd dictionaries, small-object packing, regional cache (`asc serve --upstream`), turbo-graph derived type, `node_modules` snapshots | Driven by measurement |

Spikes, in order, each a day or two inside P0–P1:

1. Enable ProjFS here and run a 1k-file provider.
2. Check that turbo's gix-index stat check accepts ProjFS-reported times (the critical risk).
3. Round-trip-import ASC.
4. MinIO and Garage conditional-write behaviour (for `asc doctor`).
5. Dev Drive + ProjFS block clone.
6. FUSE passthrough in WSL.

---

## 17. Implementation units

Each unit has its owned files, one acceptance command, and its dependencies. They are ordered so that each is verifiable before the next. Rust units live in `D:\dev\source-control`. Go units live in `D:\dev\forgejo` on branch `asc/main`.

**Workspace:**

```
Cargo.toml (workspace, edition 2024, rust 1.95, pinned [workspace.dependencies])
proto/asc/v1/{repo,workspace}.proto
crates/asc-core  crates/asc-store  crates/asc-engine  crates/asc-git
crates/asc-rpc   crates/asc-vfs    crates/asc   (binary)
tests/{cli,turbo}/   actions/asc-checkout/
```

| # | Unit | Owned files | Acceptance command | Depends on |
|---|---|---|---|---|
| U1 | Workspace skeleton and CI (fmt, clippy, deny) | `Cargo.toml`, `rust-toolchain.toml`, `deny.toml`, `.forgejo/workflows/asc.yml`, empty crate stubs | `cargo clippy --workspace --all-targets -- -D warnings` | – |
| U2 | `asc-core::id`: typed ids, keyed BLAKE3, hex/abbrev, `GitOid` | `crates/asc-core/src/id.rs` | `cargo test -p asc-core id::` | U1 |
| U3 | `asc-core::path`: `RepoPath`, `Name` validation, case-fold collision check | `crates/asc-core/src/path.rs` | `cargo test -p asc-core path::` | U1 |
| U4 | `asc-core::object`: `Tree`, `Entry`, `Changeset`, `Change`, `Signature`, `FileBody`; postcard canonical codec with version byte; golden vectors | `crates/asc-core/src/object.rs`, `codec.rs`, `tests/golden/` | `cargo test -p asc-core object::` | U2, U3 |
| U5 | `asc-core::git`: git blob/tree/commit byte rendering and SHA-1 | `crates/asc-core/src/git.rs` | `cargo test -p asc-core git::` | U4 |
| U6 | `asc-core::content`: FastCDC chunking, `FileId` over the whole file, zstd codec | `crates/asc-core/src/content.rs` | `cargo test -p asc-core content::` | U2 |
| U7 | `asc-store::objects`: `Objects` trait, `ObjKey` layout, `BucketObjects` (memory/local/S3), conformance suite | `crates/asc-store/src/{objects,layout}.rs`, `tests/objects.rs`, `docker-compose.it.yml` | `cargo test -p asc-store --test objects` (MinIO: `--features it-s3`) | U4, U6 |
| U8 | `asc-store::refs`: `Refs` trait, `SqlRefs` (SQLite/Postgres), atomic multi-ref CAS, reflog, race test | `crates/asc-store/src/refs.rs`, `migrations/`, `tests/refs.rs` | `cargo test -p asc-store --test refs` | U2 |
| U9 | `asc-store::cache`: `CacheDir` (uncompressed content, atomic rename), `Tiered`, LRU GC, `store/` vs `cache/` | `crates/asc-store/src/{cache,tiered,gc}.rs` | `cargo test -p asc-store cache::` | U7 |
| U10 | `asc-engine::tree`: apply, pruned diff, lookup and glob, naive oracle proptest | `crates/asc-engine/src/tree.rs` | `cargo test -p asc-engine tree::` | U4, U9 |
| U11 | `asc-engine::commit`: build and validate changesets, revspec resolution | `crates/asc-engine/src/{commit,revspec}.rs` | `cargo test -p asc-engine commit::` | U8, U10 |
| U12 | `asc-engine::graph`: in-memory graph, generation, merge_base, is_ancestor, log with pathspec | `crates/asc-engine/src/graph.rs` | `cargo test -p asc-engine graph::` | U11 |
| U13 | `asc-engine::derive`: framework plus `gen` and `gitcommit` | `crates/asc-engine/src/derive/{mod,gen,gitcommit}.rs` | `cargo test -p asc-engine derive::` | U5, U12 |
| U14 | `asc-engine::diff`: line diff and all git output formats, byte-identical goldens | `crates/asc-engine/src/diff.rs`, `tests/diff_golden/` | `cargo test -p asc-engine diff::` | U10 |
| U15 | `asc-engine::merge`: three-way tree and text merge (gix-merge), cherry-pick, revert, rebase, land | `crates/asc-engine/src/merge.rs`, `land.rs` | `cargo test -p asc-engine merge::` | U12, U14 |
| U16 | `asc-engine::blame`: derived blame | `crates/asc-engine/src/blame.rs` | `cargo test -p asc-engine blame::` | U13, U14 |
| U17 | `asc-git::import`: gix walk, LFS resolve, gitlinks, raw-commit extras, git map, per-commit round-trip assert | `crates/asc-git/src/import.rs`, `tests/fixtures/` | `cargo test -p asc-git import::` (ASC: `cargo test -p asc-git asc_roundtrip -- --ignored`) | U13 |
| U18 | `asc-git::export`: fast-import stream, incremental via git map, push to git remote | `crates/asc-git/src/export.rs` | `cargo test -p asc-git export::` | U17 |
| U19 | `asc-git::projection`: gitdir, gitfile, config, index writer/reader (gix-index), local commit/tree rendering, blob seeding, hook shims | `crates/asc-git/src/projection/{mod,index,objects,hooks}.rs` | `cargo test -p asc-git projection::` (asserts `git status --porcelain` is empty and `git ls-files -s` equals the tree) | U13, U9 |
| U20 | `asc-git::remote_helper`: `git-remote-asc` protocol v2 fetch for explicit OIDs, undeltified pack | `crates/asc-git/src/remote_helper.rs` | `cargo test -p asc-git remote_helper::` (`git show HEAD:x` in a projected repo fetches only x) | U19 |
| U21 | `asc-rpc`: protos, tonic server over `Engine`, client implementing `Objects`+`Refs` and the history calls, auth interceptor, local-socket transport | `proto/asc/v1/*.proto`, `crates/asc-rpc/src/{server,client,auth,ipc}.rs`, `build.rs` | `cargo test -p asc-rpc` | U11–U15 |
| U22 | `asc-vfs::plain`: materialise or sparse via reflink/copy, stat-cache status, notify | `crates/asc-vfs/src/plain.rs` | `cargo test -p asc-vfs plain::` | U9, U10 |
| U23 | `asc-vfs::projfs`: dynamic load, callbacks, async completion, journal, switch, dehydrate, reconcile | `crates/asc-vfs/src/projfs/*.rs` | `cargo test -p asc-vfs --features projfs projfs::` (Windows with ProjFS enabled) | U9, U10 |
| U24 | `asc-vfs::fuse`: fuser, path-hash inodes, overlay, passthrough, redirections | `crates/asc-vfs/src/fuse/*.rs` | `cargo test -p asc-vfs --features fuse fuse::` (Linux/WSL with `/dev/fuse`) | U9, U10 |
| U25 | `asc::workspace`: `state.db` schema, workspace registry, journal store, status, switch orchestration, worktree add/remove | `crates/asc/src/workspace/*.rs` | `cargo test -p asc workspace::` (plain-mode backend) | U19, U22 |
| U26 | `asc::cli`: clap command tree mirroring git (§9), pager, `-C`/`-c`, aliases, revspec, output formats | `crates/asc/src/cli/*.rs`, `tests/cli/*.t` | `cargo test -p asc --test cli` (runs each script under git and asc and diffs the outputs) | U21, U25, U15 |
| U27 | `asc::daemon`: process lifecycle, autostart, `Workspace` IPC service, mount host, GC and dehydrate scheduler | `crates/asc/src/daemon/*.rs` | `cargo test -p asc daemon::` (spawns the daemon, mounts a plain or ProjFS workspace, kill/restart reconcile) | U23 or U24, U25 |
| U28 | `asc::serve`: config, repo registry, mirror mode (webhook → fetch → import; push → export → git push), authz client, `/v8` turbo cache, metrics, `/.well-known/asc` | `crates/asc/src/serve/{mod,mirror,authz,turbo,http}.rs` | `cargo test -p asc serve::` (in-process server over InMemory + SQLite, fake Forgejo authz) | U18, U21 |
| U29 | Turbo contract tests | `tests/turbo/{fixture/,run.rs}` | `cargo test -p asc --test turbo -- --ignored` (needs bun and turbo 2.10.11) | U26, U27 |
| U30 | `asc-checkout` action plus ASC workflow changes | `actions/asc-checkout/{action.yml,run.sh}`, `D:\dev\monorepo\.forgejo\workflows\*` (as a PR) | `bash actions/asc-checkout/test.sh` (clone into a temp dir from a local `asc serve`; asserts the projection and `TURBO_SCM_BASE`) | U26, U28 |
| U31 | Forgejo: generated Go client | `modules/ascclient/{gen/,client.go}`, `buf.gen.yaml` | `go test ./modules/ascclient/...` | U21 |
| U32 | Forgejo: internal authz endpoint | `routers/private/asc.go`, `routers/private/internal.go` (route), `modules/private/asc.go` | `go test ./routers/private/... -run TestAscAuthz` | – |
| U33 | Forgejo: `storage_backend` column and pre-receive import hook (Stage 2) | `models/repo/repo.go`, `models/migrations/v*_asc.go`, `routers/private/hook_pre_receive.go` (asc branch), `services/ascsync/` | `go test ./services/ascsync/... ./routers/private/...` | U31, U32, U28 |
| U34 | Forgejo: hide repo paths behind `gitrepo` (forgejo.md Phase 1) | the 34 `git.OpenRepository(path)` call sites, blame/diff/log-name-status signatures | `go test ./modules/gitrepo/... ./services/...` | – |
| U35 | Forgejo: Seam A split (`gitcli` backend, unchanged behaviour) | `modules/git/backend.go`, `modules/git/gitcli/*` | `go test ./modules/git/...` and `make test-sqlite#TestGit` | U34 |
| U36 | Forgejo: asc read backend | `modules/git/ascbackend/*` | `go test ./modules/git/ascbackend/...` plus the integration suite with `STORAGE_BACKEND=asc` | U31, U35 |
| U37 | Forgejo: Seam B writer (gitcli and asc) | `modules/repostore/{writer.go,gitcli/,asc/}`, call sites in `services/{repository/files,pull,wiki}` | `go test ./modules/repostore/... ./services/pull/...` | U36 |
| U38 | Forgejo: Actions `paths:` via `Diff`, asc context fields | `modules/actions/workflows.go`, `services/actions/context.go` | `go test ./modules/actions/... ./services/actions/...` | U36 |

Critical path to MVP-1: U1→U2/U3→U4→U5/U6→U7/U8→U9→U10→U11→U12→U13→U14→U17→U18→U19→U21→U23→U25→U26→U27→U28. U15 (merge/rebase) is needed by U26's `pull`/`merge`/`rebase` verbs, so it lands before U26 as well.

MVP-2 adds U29 and U30. U16 (blame), U20 and U24 slot in wherever capacity allows.

---

## 18. Risks, and the simplest mitigation for each

| Risk | Mitigation |
|---|---|
| ProjFS timestamps or attributes drift and turbo sees every file as dirty (hydrating everything) | Spike 2 before building on it; contract test U29; the fallback is to write the index from post-hydration stat for hydrated files |
| ProjFS notifications are lossy | Journal = hints; reconcile on start; `asc status --full` escape hatch |
| NTFS hydration still costs disk per worktree | Dehydration policy; Dev Drive spike; most agent worktrees read a small fraction of 119 MiB |
| Mirror-mode double write (git and asc) races | Forgejo git is the single truth in Stage 1, and asc refs only move after Forgejo accepts |
| Unified diff not byte-identical to git (Forgejo parser) | Golden corpus from ASC's history in U14 |
| Garage lacks conditional writes | Not needed: server refs are in SQL, and bucket refs are a later unit gated by `asc doctor` |
| Turbo changes its git touchpoints | Contract tests on every turbo minor; upstream proposal `TURBO_SCM_CHANGED_FILES` |
| Many small object GETs on S3 | Client batching and pipelining plus the shared machine cache; small-object packing is a later unit, done only if cost or latency data says so |
| One-binary bloat (server pulls VFS code and vice versa) | Cargo features `serve`, `vfs-projfs`, `vfs-fuse`. The default dev build includes everything; the server container builds `--no-default-features --features serve` |
