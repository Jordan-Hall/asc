# asc architecture: integration-first design

Status: design proposal, 2026-10-22. Angle: **integration-first**. The Forgejo storage replacement, Turborepo, Forgejo Actions, git-compatible UX and migration from the existing git history (LFS and submodule included) must be smooth, reversible and low risk. Every phase ships something usable and can be rolled back.

Inputs: `docs/research/{forgejo,prior-art,rust-stack,turbo-ci,asc-profile}.md`. Fixed user decisions (Bonsai-style changesets, full storage replacement in a Forgejo fork, Rust, git-shaped `asc` CLI, S3-compatible and local storage, local-only and client/server modes, lazy materialisation, cheap worktrees, faster Turborepo CI) are not reopened here.

---

## 0. Decisions on one page

| # | Decision | Why it helps integration |
|---|---|---|
| D1 | **asc IDs are the identity. The git SHA-1 is eager, deterministic derived data. Import is lossless, so imported commits keep their original git SHAs.** | Every SHA in Forgejo's database, commit statuses, PR records, issue text, Actions runs and chat links stays valid. The git bridge is a cache, not a second source of truth. |
| D2 | **Two independent per-repo switches in Forgejo: `storage_backend` (`git`/`asc`) and the existing `object_format_name` (`sha1`/`asc`).** Cut over storage first while still showing git SHAs. Change the displayed IDs later, as a separate step. | Two risky changes become two small ones. Each can be rolled back on its own. |
| D3 | **The client works against a plain git remote ("git-remote mode") before any asc server exists.** | Developers and agents get virtual worktrees, and the disk relief, in the first milestone, with no server or Forgejo changes. Later they switch the remote to `asc-server` with `asc remote set-url`. Changeset IDs do not change, because import is deterministic. |
| D4 | **Each working copy has a real, blobless `.git` ("git projection"). Its git index *is* asc's staging area.** | Turborepo, Biome, IDEs, lint-staged and scripts run the unmodified `git` binary with zero hydration. There is one staging area, not two. |
| D5 | **asc reads and writes git's own config files, with asc settings in an `asc.*` namespace.** | `user.name`, aliases, `core.editor`, credential helpers and hooks work on day one. There is one config system. |
| D6 | **Derived data lives in the object store under deterministic keys. SQL (or redb) holds only refs, the ref log, the repo registry and work queues.** | The same derivation code runs in local-only, serverless-bucket and server modes. No mapping database has to be migrated. |
| D7 | **Commits are created locally as draft changesets, as in git. History, blame, server diffs, PR merges and landing run on the server.** | Git muscle memory (commit offline, push later) is kept, together with Scion's API-first server. |
| D8 | **One `RepoApi` Rust trait at every tier: embedded engine, daemon cache, regional cache, server.** | Local-only mode is the embedded engine. Tiers are decorators. One conformance suite tests them all. |
| D9 | **Shadow mode before cutover. At cutover, the existing bare repo becomes the bridge mirror.** | Rollback means flipping one column back. The mirror already holds byte-identical SHAs. |
| D10 | **CI affected detection runs turbo's own `turbo ls --affected` over a manifest-only checkout plus the projection. No turbo logic is reimplemented in Rust.** | The plan job's answer is exactly turbo's, and it cannot drift. |
| D11 | **Read-only git commands that `asc` does not implement are delegated to the real `git` on the projection. Commands that would create commits through git are refused.** | Broad compatibility from day one, with no false promises. |
| D12 | **LFS: imported history keeps LFS pointer entries, resolved lazily from the sha256. New large files are native chunked content.** | Changeset IDs stay deterministic whether or not LFS bytes were downloaded, and git SHAs stay byte-identical. |
| D13 | **The submodule stays a gitlink entry. The daemon composes the nested repo's tree at that path: no nested `.git`, no per-worktree LFS store.** | Removes the 27 GB of per-worktree submodule git directories without forcing a repo merge. |
| D14 | **Forgejo stays the authority for identity, permissions and policy. The asc server calls Forgejo's existing internal hook API around every ref compare-and-swap.** | Branch protection, protected files, webhooks, Actions triggers and PR re-tests keep their single code path (`PushUpdates`). |

---

## 1. Goals, non-goals, constraints

**Goals (measurable):**

| Item | Target |
|---|---|
| New working copy or worktree, asc/dds | < 1 s, < 50 MiB on disk before builds |
| Per-machine store replacing 58 GB `.git` | < 2 GiB (live history plus hydrated content) |
| `asc status` | O(dirty paths): < 50 ms on a clean tree |
| `asc switch` | O(changed paths), no content downloads |
| CI checkout | 1–2 s mount. No history and no submodule clone. |
| `deploy-affected` | about 5 s, with no checkout |
| Turbo task hashes on a virtual checkout | identical to a full git clone, with 0 reads of clean files |
| Scale headroom | 100k files, 5 GiB snapshot, 1M changesets, 50k-file changesets, 50 ref updates/min on one branch, 200 working copies per machine |

**Non-goals for v1:** Mercurial or jj interop, cross-repo sync, macOS virtual filesystem (macOS uses non-virtual mode first), patch-theory merges, a git-protocol server written from scratch (stock `git upload-pack` serves the bridge mirror).

**Constraints:** Windows 11 developers on NTFS (no Dev Drive yet; ProjFS staged but disabled); Linux CI runners on forgejo-runner; Forgejo 16.0.6; Rust 1.95; no GPL code copied from Sapling, Mononoke or EdenFS (ideas only); asc is licensed MIT OR Apache-2.0 and stays a separate program from the GPLv3 Forgejo fork, which talks to it over RPC.

---

## 2. Data model

### 2.1 IDs and hashing

- All IDs are 32-byte BLAKE3 hashes with **domain separation per kind** (`blake3::Hasher::new_derive_key("asc/v1/<kind>")`). They are distinct Rust newtypes in `asc-types`: `ContentId`, `ChunkId`, `TreeId`, `ChangesetId`, `TagId`. An `ObjectId` enum wraps them for transport. A blob ID can never be used as a tree ID.
- `GitOid` (20 bytes, SHA-1) is a separate newtype. It is **never** an identity inside asc, only an attribute and a mapping key.
- Display: lowercase hex, 12 characters abbreviated. Revision input accepts asc hex (64 characters or a prefix), git hex (40 characters or a prefix), or the explicit `asc:`/`git:` prefixes. An ambiguous prefix is an error that lists the candidates.

### 2.2 Content

- `ContentId = BLAKE3(full file bytes)`, whatever the file size, so identity never depends on chunking parameters.
- **Small content (< 4 MiB):** one object, zstd level 3, optional trained dictionary per file class (`.ts/.tsx`, `.json`, `.rs`, `.py`, lockfiles). A 1-byte header names the codec and dictionary.
- **Large content (≥ 4 MiB):** the object under `ContentId` is a `ChunkList { chunks: Vec<(ChunkId, u32 len)> }`, made by FastCDC (min/avg/max 512 KiB / 1 MiB / 4 MiB). Chunks are deduplicated across versions, so appends to ASC's JSONL corpora cost only a few chunks. Reads by byte range fetch only the chunks they touch. Bao range verification is a later option.
- **Content metadata** (derived at ingest, stored once per content): `size`, `git_oid` (SHA-1 of `"blob <len>\0" + bytes`), `sha256` (needed for the LFS and artefact aliases), `is_binary`, `line_count`.

### 2.3 Changeset (Bonsai-style, hashed)

```rust
pub struct Changeset {                       // envelope: b"asc" | kind=Changeset | version=1 | postcard
    pub parents: Vec<ChangesetId>,           // first parent first
    pub author: Signature,                   // { name: BString, email: BString, time: i64, tz_min: i16 }
    pub committer: Signature,
    pub message: BString,                    // raw bytes
    pub extra: BTreeMap<String, BString>,    // namespaced: "git.*" holds lossless git headers (2.6)
    pub changes: BTreeMap<RepoPath, Change>, // relative to FIRST parent, sorted
}

pub enum Change {
    Write(FileWrite),
    Gitlink { commit: GitOid },             // submodule pin (2.7)
    Delete,
}

pub struct FileWrite {
    pub kind: FileKind,                      // Regular | Executable | Symlink
    pub content: ContentId,
    pub size: u64,
    pub git_oid: GitOid,                     // what git sees at this path (the pointer blob for LFS entries)
    pub lfs: Option<LfsRef>,                 // Some => git holds a pointer; working copies show the resolved bytes
    pub copy_from: Option<(RepoPath, ChangesetId)>,
}
```

- **Merges** record their changes against the first parent too (Scion), so `tree(cs) = tree(p1) + changes`. Tree building never needs the other parents.
- The **root tree is not in the hashed changeset.** It is required derived data (section 4). Pushing a changeset costs O(change), and the tree format can evolve.
- `git_oid` is included so that building trees and git commits never needs the content bytes. The server re-checks it when it receives content.
- `RepoPath` is normalised UTF-8 or raw bytes, `/`-separated, with no `.`/`..` and no trailing slash. It also carries a case-fold key, used to reject paths that differ only in case on new commits (Windows ProjFS is case-insensitive). History is imported as it is, and the VFS logs a warning for any collision.
- **Snapshot changesets** (`extra["asc.snapshot"]`) may include untracked files. Stash, workspace sync and agent hand-off use them. They are never allowed on protected refs.

### 2.4 Trees (derived, "fsnode v1")

```rust
pub struct Tree { pub entries: Vec<TreeEntry> }   // sorted by name bytes; one node per directory
pub struct TreeEntry { pub name: PathComponent, pub node: Node }
pub enum Node {
    Dir     { tree: TreeId, git_oid: GitOid, files: u32, bytes: u64 },
    File    { kind: FileKind, content: ContentId, size: u64, git_oid: GitOid, lfs: Option<LfsRef> },
    Gitlink { commit: GitOid },
}
```

- Trees hold content only, never history, so identical content always has the same `TreeId` on every branch and in every repo.
- `git_oid` on directories is the git tree SHA-1. It is computed when the tree is built, from the child entries in git's sort order. Exporting a git tree is therefore a re-encoding, with no content read.
- Directories with more than 4,096 entries split into a sharded trie node (`Node::Shard`) in v2. ASC's largest directory is far below that, so v1 rejects the case with a clear error. The format version leaves room for it.
- `files` and `bytes` summaries give `du`, `--stat` totals and VFS directory sizes without walking the tree.

### 2.5 Tags and refs

- A lightweight tag is a ref. An annotated tag is a `TagObject { target: ObjectId, name, tagger, message, extra }` (with `git.*` extras for lossless export).
- Ref names follow git: `refs/heads/*`, `refs/tags/*`, `refs/pull/<n>/head` (Forgejo), `refs/for/*` (AGit), `refs/asc/snapshots/*`. Values are `ChangesetId` or `TagId`.

### 2.6 Git mapping and lossless round trip

- `git_commit(cs)` is derived: it re-encodes the changeset as a git commit (tree = `git_oid` of the root tree, parents mapped through `git_commit`, author and committer lines, message), followed by the extra headers.
- **Import keeps everything git hashes.** `extra["git.author"]` and `extra["git.committer"]` hold the raw lines when they don't round-trip through `Signature` (odd time zones, non-UTF-8). Other extras: `git.encoding`, `git.gpgsig`, `git.mergetag`, `git.headers` (any other header, in order), and `git.message` when the message bytes are not canonical. So `export(import(c)) == c` byte for byte, and the SHA is identical.
- **Non-canonical git trees** (odd modes such as `040000` vs `40000`, unsorted entries, duplicate names) can't be rebuilt from an fsnode. The importer stores the raw tree bytes under `derived/git_tree_override/<TreeId>`, keyed by the exact tree. The round-trip verifier (section 13) proves no other case exists in ASC's history. The bridge rejects pushes whose objects can't be represented losslessly, with an error message.
- Native asc commits export deterministically: the same changeset always gives the same git SHA. A git signature is supported: the signer signs the git bytes without `gpgsig` and stores the signature in `extra["git.gpgsig"]`.

### 2.7 Submodules

- `Change::Gitlink { commit }` and `Node::Gitlink` keep the git SHA pin. This is lossless, so the git tree SHA is unchanged.
- The repo config `asc.submodule.<path>.repo = asc/soma` names the asc repo that serves it (the default is derived from `.gitmodules` `url = ../soma.git`).
- The daemon **composes** trees: at a gitlink path, the VFS projects `root_tree(git→asc(commit))` of the nested repo, which is lazy and comes from the same shared cache. There is no nested `.git` and no separate LFS store. Writes under the path are journaled against the nested repo. `asc status` reports the submodule as modified. `asc -C packages/soma/sdk commit` commits in the nested repo, and `asc add packages/soma/sdk` updates the pin.
- Folding `soma` into `dds` as a subtree is an optional later step (section 19).

### 2.8 Encoding and versioning

- Canonical encoding: envelope `b"asc" | kind u8 | version u8 | postcard(struct)`. Only sorted `Vec`s and `BTreeMap`s are used, never `HashMap`. The hash covers the full envelope.
- Protobuf exists only at the RPC boundary. Objects travel as opaque `bytes`, so the bytes hashed, stored and sent are the same bytes.
- A new format version is a new kind/version pair, never a reinterpretation. Derived types are versioned in their key (`derived/root_tree.v1/...`) and can be re-derived side by side.

---

## 3. Storage

### 3.1 BlobStore and layout

```rust
pub trait BlobStore: Send + Sync {
    async fn get(&self, key: &Key) -> Result<Option<Bytes>>;
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Option<Bytes>>;
    async fn put_if_absent(&self, key: &Key, bytes: Bytes) -> Result<()>;   // AlreadyExists == Ok
    async fn exists_many(&self, keys: &[Key]) -> Result<Vec<bool>>;
    fn list(&self, prefix: &KeyPrefix) -> BoxStream<'_, Result<Key>>;
}
```

- The implementation is `object_store` 0.14 (S3, MinIO, R2, Ceph RGW, Garage, local filesystem, in-memory). Decorators are stacked from config: `prefix` (pool), `verify` (BLAKE3 check on every read), `cache` (memory LRU), `metrics`, `readonly`, and `chaos` in tests. **Blob writes never need conditional-write support**, so every S3-compatible store works for objects.
- Layout, inside a pool:

```
<pool>/c/<hh>/<content-id>        content (zstd) or ChunkList
<pool>/k/<hh>/<chunk-id>          large-file chunks
<pool>/t/<hh>/<tree-id>           trees
<pool>/s/<hh>/<changeset-id>      changesets
<pool>/g/<hh>/<tag-id>            tag objects
<pool>/m/<hh>/<content-id>        content metadata (size, git_oid, sha256, binary, lines)
<pool>/d/<kind.vN>/<key>          derived data (deterministic, recomputable)
<pool>/x/git/<sha1>               alias: git oid -> asc ObjectId (commit, tree and blob)
<pool>/x/sha256/<hex>             alias: sha256 -> ContentId (LFS and artefacts)
<pool>/r/<refname>                refs (serverless-bucket mode only)
turbo/<repo>/<scope>/<hash>[.meta] turbo remote cache (section 10.3)
```

- No packfiles and no repack. Every object can be fetched alone and is idempotent, so transfers resume per object. Segment packing of small objects (ranged GETs, one index object per segment) is a later `pack` decorator. ASC has about 105k reachable objects, so it is not needed for v1.

### 3.2 RefStore

```rust
pub trait RefStore: Send + Sync {
    async fn get(&self, repo: RepoId, name: &RefName) -> Result<Option<RefValue>>;
    async fn list(&self, repo: RepoId, prefix: &RefPrefix) -> Result<Vec<(RefName, RefValue)>>;
    /// Atomic across all updates. `expected = None` means "must not exist".
    async fn cas(&self, repo: RepoId, updates: &[RefCas], log: RefLogEntry) -> Result<CasOutcome>;
}
```

| Implementation | Mode | Notes |
|---|---|---|
| `redb` | local-only, and the daemon's local refs | one file, MVCC, used by VFS callbacks too |
| `sql` (sqlx: Postgres or SQLite) | server | `UPDATE refs SET v=$new, ver=ver+1 WHERE repo=$r AND name=$n AND ver=$old`, many rows in one transaction, plus a `ref_log` insert (this is also the audit log) |
| `s3cas` | serverless shared bucket | `r/<ref>` object holding `{value, ver}` with `If-Match`. Only enabled after `asc init` probes the bucket (create twice, then update with a stale ETag). A retried create that returns 412 counts as success when the stored value equals the value we wrote. Atomicity covers **one** ref at a time, so multi-ref pushes fall back to one ref per call. Garage: assume not supported. |

### 3.3 Deployment modes

| Mode | Objects | Refs and registry | Derivation runs in | Use |
|---|---|---|---|---|
| **local-only** | local directory (`~/.asc/store` or `D:\asc\store`) | redb | the daemon (or the CLI when there is no daemon) | solo work, tests, offline |
| **git-remote** | local directory | redb, plus remote-tracking refs from the git remote | the daemon | phase 1 for ASC developers (13.1) |
| **serverless bucket** | S3 bucket | `s3cas` | each client (output is deterministic, so concurrent derivation is harmless) | small teams and disaster recovery, where the bucket supports CAS |
| **client/server** (ASC) | S3 (Garage/MinIO) or local disk | Postgres (shared with Forgejo's database server, separate `asc` schema) or SQLite | `asc-server` (on push, or deferred through a queue) | production |

Every machine always has a **local durable store** (the same format as local-only mode) for drafts that have not been pushed, the staging area's new content and snapshots. The **shared cache** holds only objects that exist upstream and can be evicted. This split is the rule that makes eviction safe.

### 3.4 Pools, dedup, retention

- A **pool** is a storage namespace. Each Forgejo *fork network* is one pool, so forks share objects and `Fork` is an O(1) metadata copy, while unrelated repos stay isolated. The repo registry maps `repo → pool`.
- Server GC: none in v1. Objects are append-only and ASC's live data is about 0.4 GB. v2 adds mark-and-sweep per pool from refs, retained snapshots and the turbo scopes, with a grace period.
- Quota (Forgejo): the "size" of an asc repo is the **logical** size, meaning the bytes reachable from its refs, computed as derived data per root tree (`bytes` summary) plus history. It is reported as both logical size and pool-unique size. Section 19 has the open question.

---

## 4. Derived data

```rust
pub trait Derivable: Sized + Send + Sync {
    const KIND: &'static str;            // "root_tree.v1"
    const REQUIRED: bool;                // must exist before a ref may point at the changeset (warm-ref rule)
    async fn derive(ctx: &DeriveCtx, cs: &Changeset, parents: &[Self]) -> Result<Self>;
    fn encode(&self) -> Bytes; fn decode(b: &[u8]) -> Result<Self>;
}
```

- The value is stored at `d/<KIND>/<changeset-id>` using `put_if_absent`. Derivation is a pure function, so racing derivers write identical bytes. Batch backfill derives in topological order with parallel slices.

| Type | Required | What it is | Used by |
|---|---|---|---|
| `root_tree.v1` | yes | the root `TreeId`: parent root tree plus changes. New tree nodes are written along changed paths only. | everything |
| `graph.v1` | yes | generation number plus a skew-binary skip pointer | merge-base, is-ancestor, log ranges, `--contains`, Forgejo "commits between" |
| `git_commit.v1` | yes when the repo's bridge or ID mode needs git (always, for ASC) | git SHA-1, plus the `x/git/<sha>` reverse aliases for the commit and every new tree and blob | Forgejo ID mode `sha1`, the bridge, the projection, Actions |
| `content_meta` | on ingest | section 2.2 | everything |
| `last_change.v1` | lazy (phase 7) | per directory: entry name → last changeset that changed it, derived from the parent's map | Forgejo directory listings (`LastCommits`), `log -- path` |
| `blame.v1` | lazy | per (path, changeset): line → origin changeset, from the parent's blame plus this changeset's diff | `asc blame`, Forgejo blame |
| `inferred_copy.v1` | lazy | renames for imported history (exact hash, then same basename, then similarity limited to the changed set) | `log --follow`, merge rename handling |
| `lang_stats.v1` | lazy per root tree | linguist-style language bytes, honouring `.gitattributes` | Forgejo language bar |

- Until `last_change` and `blame` exist, v1 walks first-parent changesets and checks their `changes` keys. Changesets are small and cached, and ASC has 5.5k commits. The derived types are the scale path to 1M changesets.
- **Warm-ref rule:** `UpdateRefs` derives every required type for the new changesets, synchronously (each is O(change)), before the compare-and-swap. Readers never wait for derivation.

---

## 5. Server (`asc-server`)

### 5.1 RPC surface (gRPC, `proto/asc/v1/*.proto`; the same services at every tier)

Every request carries `repo` and `id_mode` (`ASC` or `GIT`). In `GIT` mode the server translates IDs at the boundary through the `x/git` aliases, so the Go client never maps IDs itself.

| Service | Methods | Notes |
|---|---|---|
| `Objects` | `FindMissing(ids) → ids`, `GetObjects(stream ids) → stream (id, bytes)`, `PutObjects(stream) → ack`, `GetTrees(root, prefixes, depth)`, `TreeDiff(old, new, prefixes)`, `Prefetch(root, cone, globs, max_bytes)`, `ReadRange(content, range)` | Want-driven, batched per tree level, resumable by asking again for whatever is still missing. No session state. |
| `Refs` | `ListRefs(prefix, pattern, contains?)`, `UpdateRefs(updates[], opts)`, `WatchRefs(prefix) → stream` | Multi-ref atomic CAS. `opts`: `pusher`, `push_options`, `mode = FastForward | Force | Land`, `hooks = Run | SkipPre(bridge)`. |
| `History` | `Resolve(rev)`, `Log(LogQuery) → stream`, `Count`, `MergeBase(a,b)`, `IsAncestor`, `LastCommits(at, dir, names)`, `Blame(at, path, opts) → stream`, `ChangedPaths(base, head, opts)`, `UnifiedDiff(base, head, opts) → stream`, `DiffStat`, `Grep(at, query)` | API-first. CI and Forgejo never need a checkout. |
| `Mutations` | `CreateChangeset(parents, author, msg, ops[])`, `Merge(style, base, head, dry_run)`, `Rebase`, `CherryPick`, `Revert`, `ApplyPatch`, `Land(stack, onto)` | Forgejo's web editor, PR merges and update-branch, with no working copy. |
| `Repo` | `Init`, `Fork`, `Delete`, `Rename`, `Size`, `Archive(at, fmt, prefix) → stream`, `Attributes(at, paths, names)`, `LanguageStats(at)` | Repository lifecycle and Forgejo's bulk readers. |
| `GitBridge` | `MapIds(git↔asc)`, `GitObjects(oids) → stream raw git objects`, `ImportGitPack(stream pack, ref updates)`, `ExportStatus` | Feeds the mirror, the projection and bridge pushes. |
| `Admin` | `ImportGit(path or url)`, `VerifyImport`, `Rederive(kind, range)`, `Health` | Operations. |

Plain HTTP on the same port (`axum`): `/v8/artifacts*` (turbo remote cache), `/healthz`, `/metrics`, and presigned-redirect endpoints for large content.

### 5.2 Write path: `UpdateRefs`

1. **Authorise.** Validate the token with Forgejo (section 14). A write needs push permission on the repo.
2. **Check closure.** Every new changeset's parents, and the contents its changes reference, must exist. Clients upload changesets and contents only. The server derives trees itself and treats any client-built trees as cache.
3. **Derive** the required types for the new changesets (warm-ref rule).
4. **Policy:** a fast-forward check through `graph`. With `mode=Land`, pushrebase instead (5.3).
5. **Pre-receive:** `POST {forgejo}/api/internal/hook/pre-receive/{owner}/{repo}` with `HookOptions` (old and new IDs in the repo's ID mode, an empty `GitQuarantinePath`, push options, pusher). Forgejo reads the new commits through Seam A. Branch protection, protected files, signed-commit rules and quota run unchanged. This step is skipped when `hooks=SkipPre`, which only Forgejo's internal token may set, for bridge pushes that already ran pre-receive.
6. **One SQL transaction:** CAS every ref, insert `ref_log` rows, and insert `outbox` rows (`post_receive`, `bridge_export`).
7. Return. An outbox worker delivers `post-receive` (at least once, with retries; `PushUpdates` is idempotent per ref and new ID) and runs the eager bridge export.

Order of writes: blobs, then changesets, then derived data, then the ref. Orphaned objects are harmless, so nothing is ever rolled back.

### 5.3 Server-side mutations

- **Tree merge** is the union of two changed-path sets. Subtrees unchanged on one side are taken whole from the other side by ID. Only paths changed on both sides get a content 3-way merge (a port of `gix-merge`'s text driver, producing git-identical markers and `merge`/`diff3`/`zdiff3` styles). Renames come from `copy_from` and `inferred_copy`, and are detected only among changed paths.
- **Conflicts** are `Merge<T>` values (the jj algebra, ported with attribution). `Merge(dry_run=true)` returns conflicting paths and changed protected files. That is Forgejo's mergeability check.
- **Merge styles** for Forgejo: `merge`, `rebase`, `rebase-merge`, `squash`, `fast-forward-only`, `manual`. Each produces one changeset, or a stack, followed by `UpdateRefs`. That way Forgejo's own hooks run, as they do today.
- **Land (pushrebase):** opt-in per branch (`asc.land.<branch>=pushrebase`) or with `asc land`. The server rebases the stack onto the current tip when the paths changed by the stack and the paths changed on the tip since the stack's base are disjoint (including file-vs-directory prefix and case-fold conflicts). It then CASes, retrying up to 50 times, and returns the old→new mapping. Plain `asc push` keeps git's reject-non-fast-forward behaviour, so nothing surprises a git user.

### 5.4 Tiers

- `asc-server --mode=server`: authoritative. Postgres or SQLite plus the object store.
- `asc-server --mode=cache --upstream=<url>`: a regional cache, using the same binary, the same RPCs and a memory-plus-disk LRU. Objects are immutable, so the cache never invalidates anything. Ref and history calls pass through. Any HTTP cache in front of the presigned content URLs also works as a tier.
- `ascd`: the per-machine tier (section 6).

---

## 6. Daemon (`ascd`)

```
 asc CLI ──IPC (named pipe / UDS, gRPC)──► ascd ─────────────► upstream tier (regional cache / asc-server / git remote)
 git, turbo, IDE ──► .git projection ─┐    │ ├ LocalEngine (RepoApi) over local durable store + redb
 editors, tools ───► virtual WC ──────┴──► │ ├ SharedCache (RepoApi decorator): content files + redb index + LRU
                                           │ ├ VFS providers (ProjFS / FUSE), one mount per working copy
                                           │ ├ Journal + status, git projection writer, git-remote-asc helper backend
                                           │ ├ Turbo cache tier on 127.0.0.1 (/v8)
                                           │ └ Prefetcher, dehydrator, derive worker (local modes)
```

- **One daemon per user per machine** (Windows: started on demand by `asc` and by a logon task; Linux: a systemd user unit; CI host: a system service with one socket per runner user).
- **Shared cache:** content bodies are stored as files (`<cache>/c/<hh>/<id>`), which FUSE passthrough needs. A redb index maps `id → (size, path, last_access, pins)`, and `quick_cache` keeps decoded trees in memory. Default budgets are 20 GiB for small content and trees and 50 GiB for large content, kept separate so large chunks never evict trees. Eviction is LRU. Pinned objects are never evicted.
- **Local durable store:** drafts, staged content, snapshots and the op log. It is never evicted. After a push succeeds, pushed objects are demoted to the cache.
- **IPC:** `interprocess` local sockets carrying the same gRPC. On Windows the pipe DACL allows only the current user's SID; on Unix the socket is mode 0600 in `$XDG_RUNTIME_DIR`. VFS callbacks run inside `ascd` and never cross IPC.
- **No daemon:** in non-virtual mode, `asc` can embed `LocalEngine` and `SharedCache` directly (`ASC_NO_DAEMON=1`). This is used by CI sparse mode and by tests.
- **Restart safety:** mounts and inode tables persist in redb. On start, a filesystem check reconciles each Windows working copy against disk, because ProjFS lets files change while the provider is down. The CLI auto-restarts the daemon. Upgrades unmount cleanly; a FUSE fd hand-off is later work.

---

## 7. Working copies and the virtual filesystem

### 7.1 Anatomy of a working copy

```
W/                                   the mount (ProjFS root / FUSE mountpoint / plain dir in non-virtual mode)
W/.git                               gitfile: "gitdir: <ascdata>/wc/<wc-id>/git"
W/node_modules -> <ascdata>/redirect/<wc-id>/node_modules     (junction or symlink, section 7.8)
<ascdata>/wc/<wc-id>/
  git/                               per-worktree git dir: HEAD, index, commondir -> ../../repos/<repo>/git
  asc.toml                            repo id, base changeset, mode, sparse profile, redirections
  journal.redb                       modified-paths journal, sequence numbers, placeholder versions
  overlay/                           Linux FUSE only: modified file bodies
<ascdata>/repos/<repo>/git/           shared git common dir: config, refs mirror, objects (commits and trees only), hooks
```

- `<ascdata>` defaults to `%LOCALAPPDATA%\asc` or `~/.local/share/asc`. On ASC machines it is set to `D:\asc` (the same volume as the bun cache, so redirected `node_modules` keeps hard links).
- asc finds its state through the `.git` gitfile. There is no `.asc` marker in the tree.

### 7.2 The git projection (D4)

- **Common dir** per repo per machine: `config` (git's file, which holds the `asc.*` keys too), a refs mirror (git SHAs), `objects/` holding synthesized commits and trees (deterministic, from the bridge mapping) and blobs fetched on demand, and `hooks/`. `extensions.partialclone=asc`, `remote.asc.promisor=true`, `remote.asc.url=asc::<repo>`.
- **Commits** are written eagerly for the first-parent history the daemon knows (5.5k commits are tiny). **Trees** are written for HEAD and base, and the rest arrive lazily through the promisor. **Blobs** are fetched only on demand by `git-remote-asc` (`stateless-connect`, protocol v2 `fetch` with explicit wants and no negotiation, answering whole objects with no deltas from the daemon cache).
- **Index = staging area.** asc writes `index` v2 on `clone`, `switch`, `checkout <rev>` and `reset`. Each entry has the path, mode, `git_oid` and the **exact stat the VFS reports** for that placeholder (section 7.3). The index file is written at least 2 s after the newest placeholder mtime, so nothing is racy. A TREE (cache-tree) extension is included, and no sparse or skip-worktree flags in virtual mode. `git add` and `asc add` both edit the same index. `asc add` also stores the content in the local durable store and records `git_oid → ContentId`. `asc commit` turns the index into changes against HEAD: entries whose `git_oid` matches the base tree reuse its `ContentId`, and unknown oids are read from git's loose objects (written by `git add`) and ingested.
- **Hooks installed in the common dir:**
  - `pre-commit` and `pre-push` (when run by git) refuse: "this is an asc working copy: use `asc commit` / `asc push`". `asc commit` runs the user's own hooks from `core.hooksPath` (husky and lint-staged keep working).
  - `reference-transaction` imports git-side ref edits (`git branch x`) into asc's refs.
  - `post-checkout` reconciles when a tool ran `git checkout`: the new HEAD is adopted as the base, and files git rewrote are marked for dehydration.
- `core.fsmonitor` points at `asc internal fsmonitor` (protocol v2, token = journal sequence). `git status` then never stats clean directories.
- **Delegation (D11).** Read-only git commands that `asc` lacks run `git <cmd>` against the projection (`GIT_DIR`, `GIT_WORK_TREE`). Write commands that create commits are refused with the asc equivalent named.

### 7.3 Windows: ProjFS (`asc-vfs-projfs`)

- Our own thin wrapper over `windows` 0.62 `Win32_Storage_ProjectedFileSystem`. `ProjectedFSLib.dll` is **loaded at runtime** with `libloading`, so `ascd.exe` still starts where ProjFS is disabled and falls back to non-virtual mode. `asc doctor --fix` enables `Client-ProjFS` through an elevated prompt (admin is needed once).
- Each placeholder records `VersionInfo.ContentID = kind ‖ ObjectId` and `ProviderID = base epoch`. **`GetFileData` reads by the placeholder's ContentID, never by the current HEAD.** Slow fetches return `ERROR_IO_PENDING` and finish later with `PrjCompleteCommand`. Large content streams by chunk range.
- **Stat fidelity** (turbo depends on this): `CreationTime = ChangeTime = LastWriteTime = wc_epoch` for every placeholder. Hydration never changes timestamps. A spike test must prove this (section 16).
- Notifications (`NEW_FILE_CREATED`, `FILE_OVERWRITTEN`, `PRE_DELETE`, `PRE_RENAME`, `FILE_RENAMED`, `HARDLINK_CREATED`, `FILE_HANDLE_CLOSED_FILE_MODIFIED/DELETED`, `FILE_PRE_CONVERT_TO_FULL`) only append *hints* to the journal. Status re-stats and reconciles them with `PrjGetOnDiskFileState`. Notifications are masked off under redirections and the projection gitdir.
- EdenFS's rules apply: no ProjFS calls from inside a callback that holds a lock, a stable instance GUID for the life of the working copy, serve renamed files from the committed tree, a filesystem check on start, and retry or report when invalidation hits a file that is held open.
- **Dehydration:** `asc dehydrate [paths]` and an idle job that converts clean, not-recently-read hydrated files back into placeholders. Hydrated bytes are a full NTFS copy until a Dev Drive with block clone is available.

### 7.4 Linux: FUSE (`asc-vfs-fuse`)

- `fuser` 0.18 with **passthrough**. Once content is in the shared cache, the kernel reads the cache file directly, so each worktree costs zero extra bytes for files it reads. Inodes are stable and persisted in redb, and timestamps follow the same `wc_epoch` rule.
- Writes copy the file up into `overlay/`, so the dirty set is exact with no notifications. Kernel caches are invalidated (`INVAL_ENTRY`/`INVAL_INODE`) on switch.
- CI: the daemon runs on the runner host and the job sees the mount through a bind mount (section 12). In containers without `/dev/fuse`, non-virtual sparse mode is used.

### 7.5 macOS and non-virtual mode (`asc-wc` materialiser)

- **Non-virtual mode** works everywhere and is the first macOS target. It materialises a **cone-only sparse** set (turbo packages are directories). Files are reflinked or cloned from the shared cache (APFS `clonefile`, btrfs/XFS, ReFS Dev Drive) or copied. They are never hard-linked, because an edit would then corrupt the cache. Change detection uses a stat cache in redb plus `notify`. Non-materialised entries carry skip-worktree in the projection index, so git and turbo don't see them as deleted.
- An NFSv3 loopback mount (`nfs3_server`) or FSKit comes later.

### 7.6 Status and the journal

- Dirty candidates are the journal's paths since the base, plus the FUSE overlay. Each candidate is re-checked: size, then BLAKE3 against the base entry. A file whose content matches again leaves the dirty set.
- **Untracked files can only exist in materialised directories.** asc walks only those, with `.gitignore` rules (`ignore` crate). There is no full-tree untracked walk.
- Staged vs HEAD comes from the index against the HEAD tree, pruned by cache-tree.
- `asc status --porcelain=v1|v2 -z` produces git's exact formats. `asc internal changed-since <token>` serves fsmonitor and watchers, with a fresh-instance flag after a restart or journal truncation.

### 7.7 Switch and checkout

1. `TreeDiff(old_root, new_root)`, locally from cached trees or by RPC. It recurses only into directories whose IDs differ.
2. Any changed path that is in the dirty set is a conflict, handled by git's rules: refuse, or carry the change with `--merge`.
3. For each other changed path: update the placeholder (`PrjUpdateFileIfNeeded`, `PrjDeleteFile`) or invalidate the FUSE inode. Placeholders that were never materialised just get the new version. Finally clear the negative-path cache and bump the epoch.
4. Rewrite the projection index and HEAD. Cost is O(changed paths), with no content downloads.

### 7.8 Worktrees, redirections, workspaces

- `asc worktree add <path> [-b <branch>] [<rev>]` writes the state and the projection gitdir, then mounts. It takes milliseconds whatever the repo size. Worktrees share the common dir, the shared cache and the local durable store.
- `--ephemeral --ttl 7d` creates auto-expiring worktrees and agent branches (`refs/heads/agent/*`, server-side TTL). These are meant for ASC's roughly 20 parallel agents.
- **Redirections:** the repo's `.asc/redirections` file (checked in; the default list is `node_modules`, `.turbo`, `dist`, `.next`, `target`, `.venv`, `__pycache__`). Each entry is a junction or symlink to `<ascdata>/redirect/<wc-id>/...` on plain local disk, so it is outside the VFS and outside the journal. Shared per-lockfile `node_modules` snapshots, mounted read-only, come later (phase 7).
- **Workspace sync** (phase 7): `asc workspace push` uploads the dirty overlay as a snapshot changeset. Another machine or a CI job can mount it without anything being pushed to a branch.

---

## 8. CLI

### 8.1 Principles

- Same verbs, flags, revision syntax (`HEAD~2`, `A..B`, `A...B`, `@{u}`, `:/msg`, `<rev>:<path>`), exit codes and output formats as git, including `--porcelain`, `-z`, `--format` and `--name-status`. Unsupported flags fail with a clear error that names the alternative. They are never silently ignored.
- Global `-C <dir>` and `-c key=value`, `alias.*` expansion, `asc-<name>` plugins on `PATH`, git's pager rules (`GIT_PAGER`/`PAGER`/`less -FRX`) and `NO_COLOR`.
- **Credentials** use git's credential helper protocol (`git credential fill`), so Git Credential Manager on Windows works unchanged.
- `asc` is a multi-call binary: `asc`, `git-remote-asc` and `asc-fsmonitor` are hard links to the same file.

### 8.2 Command map

"Local" means the daemon or embedded engine with no network. "Server" means an RPC to the upstream tier, which on a cache hit may be answered by the daemon.

| git command | asc behaviour | Runs |
|---|---|---|
| `init` | create a local-only repo (`--store <dir|s3://…>`). `asc init --remote <url>` registers an existing server repo. | local |
| `clone <url> [dir]` | register the repo, fetch refs and the root tree, mount a virtual working copy, write the projection. `--no-virtual`, `--sparse <cone>`. `--depth` and `--filter` are accepted and ignored (always lazy). Submodules are composed by default. | server (metadata only) |
| `worktree add/list/remove/move/lock/prune` | metadata plus a mount. `--ephemeral --ttl` | local |
| `status` | journal-based. `-s`, `-b`, `--porcelain=v1|v2`, `-z`, `--ignored`, `-uno` | local |
| `add`, `rm`, `mv` | edit the projection index. `mv` records `copy_from`. `add -p` works (hunks of the file against the index). | local |
| `restore`, `checkout -- <path>` | restore from the index or `--source`. Content comes from cache or server. | local (+ fetch) |
| `commit` | draft changeset in the local engine. `-m`, `-a`, `--amend`, `--fixup`, `--no-verify`, `-S` (phase 7). Runs the user's hooks. | local |
| `switch`, `checkout <rev>` | section 7.7. `-c`, `-C`, `--detach`, `--merge`, `--orphan` | local (+ TreeDiff RPC) |
| `branch` | local refs. `-d`, `-D`, `-m`, `-u`, `-r`, `-a`, `--list <pattern>`. `--contains` uses `graph`. | local / server |
| `tag` | refs and `TagObject`. `-a`, `-m`, `-d`, `-l` | local |
| `fetch` | update remote-tracking refs **without objects**. `--prune`, `--tags`. `--prefetch <cone>` warms the cache. | server |
| `pull` | fetch, then merge or rebase per `pull.rebase` / `pull.ff`, with git's defaults | server + local |
| `push` | `FindMissing`, upload, then `UpdateRefs` with `old` = the remote-tracking value. `-u`, `--force-with-lease`, `--force`, `--delete`, `--tags`, `-o <push-option>`, `refs/for/<b>` (AGit PR) | server |
| `merge` | local engine: tree merge from metadata. Only content for paths changed on both sides is fetched. Markers per `merge.conflictStyle`. `--no-ff`, `--squash`, `--abort`, `--continue` | local |
| `rebase` | local engine for draft stacks. `--onto`, `--continue`, `--abort`, `--autosquash`. `-i` (todo editor) in phase 7. | local |
| `cherry-pick`, `revert` | local engine | local |
| `reset [--soft|--mixed|--hard] [<rev>] [-- paths]` | moves HEAD and the index. `--hard` updates placeholders (7.7). | local |
| `stash push/pop/apply/list/show/drop` | snapshot changesets on `refs/stash` in the local store | local |
| `clean -fdx` | walks materialised directories only | local |
| `log` | server history merged with local drafts. `--oneline`, `--graph`, `--format`, `-p`/`--stat` (server diff), `--follow`, `-- <path>`, `--since`, `--author`, `-n` | server + local |
| `show <rev>`, `show <rev>:<path>` | changeset plus server diff, or content | server |
| `diff` | working copy vs index vs HEAD for dirty paths only (local). `<a> <b>` and `<a>...<b>` run on the server. `--stat`, `--name-only`, `--name-status`, `--cached`, `-M`, `--word-diff`, `--no-index` | local / server |
| `blame` | server derived blame, adjusted locally when the file is dirty | server |
| `grep` | server grep at base for clean files, plus local grep over dirty files. **No hydration.** | server + local |
| `remote add/remove/set-url/-v` | stored in git config. Remote types: `asc+https://`, `asc+ssh://`, or `git+https://…` (git-remote mode, 13.1) | local |
| `config` | git's config files. `asc.*` keys | local |
| `rev-parse`, `rev-list`, `merge-base`, `describe`, `name-rev`, `shortlog`, `for-each-ref`, `show-ref`, `symbolic-ref`, `update-ref` | native implementations. `rev-parse --git <rev>` and `--asc <rev>` print either ID | local / server |
| `ls-files`, `ls-tree`, `cat-file`, `diff-tree`, `diff-index`, `hash-object`, `check-ignore`, `check-attr` | native, from metadata. `ls-tree --format` can print `git_oid` and `asc` IDs | local |
| `archive` | server `Archive` | server |
| `format-patch`, `am`, `apply` | local engine | local |
| `bisect` | the server's log plus skip pointers choose the midpoints. The working copy switches each step. | server + local |
| `reflog` | asc op log, filtered per ref. Also `asc undo` (extension) | local |
| `submodule status/update/init/sync/foreach` | composed nested repos. `update` changes the mounted pin. | local |
| `sparse-checkout` | non-virtual mode: the cone. Virtual mode: the prefetch profile. | local |
| `gc`, `maintenance`, `prune`, `repack`, `count-objects` | `gc` = cache eviction and dehydration. The others print "not needed in asc" and exit 0. | local |
| `fsck` | verify the cache, the local store and working copies (`asc doctor`) | local |
| `lfs *` | not needed: native large files. `asc lfs ls-files` is delegated for history views. | — |
| `notes`, `replace`, `bundle`, `filter-branch`, `filter-repo`, `svn`, `p4`, `send-email`, `instaweb`, `daemon` | read-only commands are delegated to git on the projection (D11). Rewriting commands fail with a pointer to `asc-server admin`. | — |

Extensions (no git equivalent): `asc land`, `asc affected`, `asc prefetch`, `asc dehydrate`, `asc undo`, `asc adopt <git-dir>` (convert an existing git checkout, keeping uncommitted work), `asc import`/`asc export`, `asc workspace`, `asc daemon`, `asc doctor`, `asc ci checkout|affected|selftest`, `asc debug`.

### 8.3 IDs in output

`asc.displayIds = asc | git | both`, default `asc`. While Forgejo still shows git SHAs (ID mode `sha1`, section 11.2), ASC sets `git` so terminal and web agree. Every input accepts both kinds of ID.

---

## 9. Offline and local-only behaviour

- **Every machine is a local repo plus an optional upstream** (3.3). Commit, branch, stash, switch between cached revisions, merge or rebase of cached content, status and diff of dirty files all work offline.
- Offline limits are explicit. Reads of content that is not cached fail with `E_OFFLINE <path>`. A virtual file that is not cached returns a read error, and the path is logged so `asc prefetch` can warm it next time. `log` and `blame` fall back to whatever local history exists and print `history truncated (offline)`. `push`, `fetch` and server diffs fail fast.
- `asc prefetch --cone <globs> [--history N]` warms the cache before going offline.
- **Local-only mode** is `LocalEngine` over a local directory or a CAS-capable bucket. The full RPC surface is served in-process, including derived data, blame and the turbo cache. A local-only repo can later be pushed to a server with `asc remote add` and `asc push --all`, and IDs do not change.

---

## 10. Turborepo integration

### 10.1 Hashing without hydration

- Turbo 2.10 reads `.git/index` with `gix-index`, stat-compares every entry and trusts the stored SHA-1 for any file whose stat matches. The projection index (7.2) supplies the exact stat and the `git_oid` for each placeholder. So `turbo run` hashes a clean virtual checkout **with zero content reads**, and the hashes equal a real git clone's, which keeps today's cache keys valid.
- Turbo runs the unmodified `git` for `rev-parse`, `ls-files --others`, `diff-tree` and `show base:bun.lock`. The projection answers all of them. The only content fetched is `bun.lock` at base, and only when it changed.
- ASC hygiene, independent of asc: delete the comment-only `.gitattributes` (it disables turbo's clean-tree shortcut), replace `"env": ["*"]` on `build` with an explicit list, and point `TURBO_CACHE_DIR` at a machine-wide directory or rely on the daemon tier.

### 10.2 Affected detection

- **Inside a job:** `turbo run <tasks> --affected`. `asc ci checkout --base <rev>` writes `refs/remotes/origin/<base>`, a `shallow` boundary at `merge_base(base, head)`, and the bridge commits and trees for `merge-base..head`. It then exports `TURBO_SCM_BASE=<merge-base git sha>` and `TURBO_SCM_HEAD=HEAD`. Turbo never falls back to "everything changed".
- **Plan job (D10):** `asc ci affected --base B --head H --rules .asc/ci.toml` runs these steps:
  1. a sparse checkout of manifests only: `**/package.json`, `turbo.json`, `bun.lock`, `Cargo.toml`, `pyproject.toml` and the workspace config, fetched in one `Prefetch`, about 2 MB;
  2. `turbo ls --affected --output=json` with the projection as above;
  3. a map from packages to job switches (`any`, `rust`, `images`, `e2e`, `deploy_targets`) using rules in `.asc/ci.toml`;
  4. outputs written in the `GITHUB_OUTPUT` format.

  It takes seconds, with no install and no build. A server-only fast path, `asc ci changed` (the `ChangedPaths` RPC plus glob rules), serves the Forgejo `paths:` filters and docs-only short-circuits.

### 10.3 Remote cache (`asc-turbo-cache`)

- Implements `/v8/artifacts/status`, `HEAD|GET|PUT /v8/artifacts/{hash}`, `POST /v8/artifacts` and `POST /v8/artifacts/events` over `BlobStore`, so it works on S3, Garage and local disk, and in local-only mode.
- **Scopes:** `trusted` (written only by protected-branch jobs) and `pr-<n>`. Reads try the job's own scope, then `trusted`. Writes go only to the job's own scope. `remoteCache.signature: true` adds HMAC tags, which are stored opaquely, end to end.
- **Auth:** the same bearer tokens as asc (PATs, OAuth, Actions task tokens). `teamId`/`slug` map to the Forgejo owner.
- **Tiers:** the daemon serves `/v8` on `127.0.0.1`. Reads go daemon → regional → server → store. Writes are acknowledged once the bytes are in the local cache, then uploaded in the background. Artefacts are immutable, so this is safe. Optional `OPTIONS` preflight answers with presigned S3 URLs for large artefacts.
- **Retention:** LRU by last access, 14 days for `pr-*` and 60 days for `trusted`, with a size cap per repo. `x-artifact-sha` is stored as the changeset.

### 10.4 Rust and Python

Add `cargo-test` and `cargo-check` turbo tasks to the existing `package.json` wrappers (inputs `**/*.rs`, `**/Cargo.toml`, `Cargo.lock`), so Rust takes part in `--affected` and the remote cache. `target/` and `.venv` are redirections. A shared `CARGO_TARGET_DIR` per toolchain and `UV_LINK_MODE=hardlink` are recommended. `experimentalCargoSccache` stays optional until there is a root cargo workspace.

---

## 11. Forgejo fork

Base: `v16.0.6` (`D:\dev\forgejo`), kept as a patch stack on branch `asc/v16`, one commit per seam, rebased on each upstream release. New code goes in new packages.

### 11.1 Seams (from `forgejo.md`, unchanged in shape)

- **Seam A, reads:** a private `backend Backend` field inside `git.Repository`. The concrete `Repository`, `Commit`, `Tree`, `TreeEntry`, `Blob` and `Tag` types keep their APIs, so the 211 importing files do not change. Today's code moves into `modules/git/gitcli`. The new `modules/git/ascclient` calls asc-server, with an LRU keyed by immutable IDs plus the batch calls `LastCommits` and `StatObjects`.
- **Seam B, writes:** `modules/repostore.Writer` (`CreateCommit`, `Merge` including dry run, `CherryPick`/`Revert`, `ApplyPatch`, `UpdateRefs`, `Init`, `Fork`, `Delete`, `Size`). The temp-repo code becomes the `gitcli` implementation. The `asc` implementation is one RPC per operation.
- **Seam C, transport and auth:** asc clients talk to asc-server directly. Forgejo authorises (11.3).
- **Backend selection** happens in one place, `modules/gitrepo.OpenRepository`, from the `repository.storage_backend` column. A third backend, `dual`, exists for shadow mode (13.2): it serves from `gitcli` and calls `ascclient` on a sample of requests, comparing results and recording mismatches in metrics and logs. It never changes a response.

### 11.2 ID modes (D2)

| `storage_backend` | `object_format_name` | Behaviour |
|---|---|---|
| `git` | `sha1` | upstream behaviour |
| `dual` | `sha1` | shadow: git serves, asc is compared |
| `asc` | `sha1` | **cutover state.** asc is the storage. Forgejo sees git SHAs: `ascclient` sends `id_mode=GIT` and asc-server maps at its boundary. No database rows change. |
| `asc` | `asc` | final state. Forgejo shows asc IDs, which fit `VARCHAR(64)`. A one-off migration rewrites stored SHAs for the repo (commit statuses, PR merge base and merged commit, comment `CommitSHA`, Actions runs, branch table, releases) through `MapIds`. Every input (URLs, API, search, statuses posted by CI) accepts both forms and normalises to the asc ID. Commit pages also show the git SHA. |

### 11.3 Hooks and auth

- New internal endpoint `POST /api/internal/asc/authz` takes `{token | jwt, owner, repo, mode}` and returns `{user_id, perm, scopes, is_actions_task, deploy_key_id, expires}`. asc-server caches positive answers for 60 s and negative answers for 5 s.
- SSH: an `asc-authenticate <owner>/<repo> <read|write>` verb next to `git-lfs-authenticate` in `cmd/serv.go` and `modules/ssh`. It returns a short-lived Ed25519-signed JWT. asc-server holds only the public key.
- Hooks: asc-server calls the existing `/api/internal/hook/{pre,post}-receive` (5.2). `proc-receive` maps to `refs/for/<branch>` pushes (AGit), which create or update a PR. `PushUpdates` stays the single fan-out point.
- Forgejo calls asc-server with its internal token over mTLS or a private network. asc-server calls Forgejo with Forgejo's `INTERNAL_TOKEN`.

### 11.4 Git bridge

- **Mirror:** per asc repo, a derived bare git repo (`git_bridge = off | lazy | eager`). The asc-server outbox worker exports new changesets with `asc-git export`, which writes objects and refs deterministically. `githttp.go`, `cmd/serv.go`, `services/mirror` and migrations operate on `bridgePath(repo)` instead of `RepoPath()`. Stock `git upload-pack` serves legacy clones and `actions/checkout`.
- **Legacy pushes** (transition only): `git receive-pack` runs into the mirror's quarantine as today. Forgejo's pre-receive handler, for asc repos, then:
  1. runs its policy checks as today;
  2. streams `git pack-objects --revs` of the new objects to `ImportGitPack`;
  3. calls `UpdateRefs(hooks=SkipPre)`.

  If the CAS fails, the push is rejected. Git then updates the mirror refs itself, and they agree with asc because export is deterministic. Post-receive runs from the git hook as today, and asc-server skips its own call for these updates. `server_side_merge_only` rejects bridge pushes to protected branches.
- Pull and push mirrors force `git_bridge != off`. Pull mirrors fetch into the mirror, then call `ImportGitPack` and `UpdateRefs`.
- At cutover, **the existing bare repo is the mirror** (D9). It is already byte-identical.

### 11.5 LFS

- For asc repos, the LFS batch API is served from asc content through the `x/sha256/<hex>` alias. Download URLs are presigned URLs to the content, or a streaming endpoint for chunked content. Uploads (bridge pushes) are ingested as asc content.
- Imported LFS history keeps pointer entries (D12). The VFS and `asc show` resolve them through the alias. Content for the 113 historic objects (25.6 GiB) is copied server-side from Forgejo's LFS storage into the pool during import, as a background job, and stays readable from the old store until that copy finishes.

### 11.6 Lifecycle and readers

- `Init`, `Fork` (O(1), same pool), `Transfer`/`Rename` (registry update), `Delete` (refs dropped; objects left to pool GC), `Size` (logical), `Adopt` (import), doctor checks (`VerifyImport`, bridge freshness).
- Readers: archives come from `Archive`. The code indexer uses `ChangedPaths` plus `GetObjects`. Language stats and attributes are server derived. `services/gitdiff` parses asc's `UnifiedDiff`, which is in git's format. Byte-identical hunking is **not** required: review comments anchor on line numbers, and patches are checked with `git apply --check` in tests.
- Wiki: a second asc repo `owner/name.wiki` in the same pool.

### 11.7 Patch stack

| # | Commit | Behaviour change |
|---|---|---|
| 1 | Path-hiding refactor (34 `git.OpenRepository(path)` callers; `repoPath string` signatures) | none (upstreamable) |
| 2 | Seam A: `Backend` and `gitcli` | none (upstreamable) |
| 3 | Seam B: `repostore` and its `gitcli` implementation | none |
| 4 | Remove raw `git.NewCommand` outside `gitcli` and `ascbridge` | none (upstreamable) |
| 5 | `modules/ascproto` (generated by buf from `source-control/proto`, committed) | none |
| 6 | `ascclient`, the `repostore` asc implementation, `storage_backend` column, `dual` backend | behind the flag |
| 7 | `routers/private/asc.go` (authz), `asc-authenticate`, hook changes | behind the flag |
| 8 | `services/ascbridge`, LFS from asc, mirror wiring | behind the flag |
| 9 | Actions: asc context fields, `paths:` filters through `ChangedPaths` | behind the flag |
| 10 | ID mode `asc`: normalisation and migration | behind the flag |

---

## 12. Forgejo Actions and CI

**Runner topology:** `ascd` runs on each persistent runner host as a system service, with the shared cache and the turbo tier. Job containers get the working copy through a bind mount (`rshared`) plus the daemon socket. FUSE runs on the host, so jobs need no privileges. Hosts without a daemon fall back to non-virtual sparse mode inside the job. The forgejo-runner config (`valid_volumes`, `options`) must be confirmed with whoever operates the runners.

**Forgejo context** (patch 9): adds `asc_server_url`, `asc_changeset` and `asc_base_merge_base`. In ID mode `sha1`, `github.sha` stays the git SHA, so `actions/checkout` and the act heuristics keep working. `paths:` filters call `ChangedPaths` instead of walking history.

**Actions** (repo `asc/asc-actions`, thin composite wrappers around `asc ci ...`, so all logic is Rust and tested by cargo):

```yaml
- uses: asc/asc-actions/checkout@v1
  with:
    mode: virtual            # virtual | sparse | full
    base: ${{ github.base_ref || github.event.before }}
    sparse: ${{ needs.plan.outputs.cone }}   # optional
    prefetch: |
      **/package.json
      bun.lock
      turbo.json
- uses: asc/asc-actions/affected@v1    # plan job; no checkout of sources
```

`checkout` takes these steps:

1. authenticate with the task token;
2. `asc clone --at <changeset>` in the chosen mode;
3. prefetch the globs in one RPC;
4. write the projection with the merge-base refs and shallow boundary;
5. export `TURBO_SCM_BASE`, `TURBO_SCM_HEAD`, `TURBO_API=http://127.0.0.1:<port>`, `TURBO_TOKEN`, `TURBO_TEAM`, and `TURBO_CACHE=local:rw,remote:rw` (or `remote:r` for untrusted PRs).

It does no submodule clone and no LFS smudge.

**Workflow shape** (from `turbo-ci.md` 4.6): `plan` (no checkout, seconds), then `verify` (`turbo run build check-types test lint --affected`), `rust` (only if `plan.rust`), `images` (only listed services), `e2e`. `deploy-affected.yml` drops `fetch-depth: 0` and runs from the plan outputs.

**Do now, without asc:** keep bun's download cache on the host, merge `ci-intelligence.yml` into `ci.yml`, add `check-types` aliases, add cargo turbo tasks, delete `.gitattributes`, fix `env: ["*"]`, and run any spec-compliant `/v8` cache on Garage. Later the asc cache replaces it with no workflow change beyond `TURBO_API`.

---

## 13. Migration

### 13.1 Phase A: developers and agents first, in git-remote mode (no server changes)

1. Install `asc` and `ascd`. Enable ProjFS once (admin). Set `<ascdata>=D:\asc`.
2. Run `asc adopt D:\dev\monorepo`. It imports from the existing local `.git` (5.5k commits; LFS pointers kept as pointers; the submodule is imported from `D:\dev\soma`), registers `origin` as `git+https://forge.example.org/asc/dds.git`, and converts the checkout into a virtual working copy. Uncommitted and untracked files are kept: they become overlay or untracked files.
3. The daemon keeps **one** shared bare git cache per remote per machine (about 0.4 GB). `asc fetch` = `git fetch` into it, then incremental import. `asc push` = deterministic export of new changesets into it, then `git push`, so Forgejo receives the same SHAs that asc-server will later compute.
4. Agents use `asc worktree add --ephemeral`. Open item: whether Claude Code's worktree hooks can call `asc worktree add` (section 19).
5. The user decides when to delete the old `.git` (58 GB), the 49 git worktrees and their submodule git dirs. asc never deletes them itself. `asc doctor --report-reclaimable` lists what could go.

Exit: worktree add < 1 s, clean `turbo run --dry` on a virtual worktree gives the same hashes as git with 0 content hydrations, and `asc push` produces SHAs Forgejo accepts.

### 13.2 Phase B: server import and shadow

1. Deploy asc-server (Postgres schema `asc`, Garage bucket `asc`, or local disk). Import `asc/soma` then `asc/dds` with `asc-server admin import-git` from mirror clones. The import is deterministic, so the changeset IDs equal the developers' local ones.
2. Run `VerifyImport`: for every commit, tree and tag, `export(import(x))` must be byte-identical to git. Any mismatch blocks progress.
3. Forgejo post-receive (git backend) enqueues incremental `ImportGitPack` for every push. Set `storage_backend=dual`. The shadow comparisons cover commits, trees, refs, `ChangedPaths`, merge-base, log pages, blame and `Merge(dry_run)` against `git merge-tree`, for ASC's 467 branches.
4. Turn on the asc `/v8` turbo cache, and the plan job using `ChangedPaths` through the server.

Exit: a week of zero shadow mismatches above noise. The Forgejo integration suite passes on the `asc` backend (F13).

### 13.3 Phase C: cutover and rollback

1. Freeze pushes for minutes. Run a final incremental import and `VerifyImport` (incremental).
2. Set `storage_backend=asc`, `object_format_name=sha1`, `git_bridge=eager`. The old bare repo is now the mirror.
3. Developers run `asc remote set-url origin asc+https://forge.example.org/asc/dds`. IDs are unchanged, so nothing is re-fetched.
4. **Rollback:** set `storage_backend=git`. The mirror is current, because eager export is part of every ref update's outbox, and SHAs are identical. Developers switch the remote back to `git+https`.
5. Then move workflows to `asc-actions` (sparse mode first, virtual once the runner hosts have `ascd`), set `git_bridge=lazy`, and later optionally ID mode `asc`.

### 13.4 LFS, submodule and artefacts

- LFS history: pointers stay in history. Content is copied into the pool in the background (11.5). Forgejo's old LFS objects can be removed after `VerifyImport --lfs`.
- Submodule: `asc/soma` becomes an asc repo, and `packages/soma/sdk` is composed (2.7). CI drops `submodules: recursive`. Optional later: fold it in as a subtree (one changeset), if no other repo consumes `soma`.
- Artefacts: `ops/artefacts/manifest.json` (578 items, 102 GiB, sha256-addressed in Garage) can later be committed as asc large files. Ingest the Garage objects by alias to avoid copying, then delete `artefacts.ts`. This is optional, but it is where about 200 GiB of local disk lives (section 19).

---

## 14. Security and auth

- **Identity and permissions:** Forgejo, always (11.3). asc-server stores no users.
- **Tokens:** PATs, OAuth and Actions task tokens over HTTPS, and short-lived JWTs from `asc-authenticate` over SSH. The client takes credentials from git credential helpers and never stores secrets itself.
- **Object access:** every request names a repo, and authorisation is per repo. Objects are fetched only within the repo's pool, so content shared across fork networks is not reachable. Content IDs are 256-bit, so they can't be guessed.
- **Integrity:** BLAKE3 is verified on every read at every tier, so a poisoned cache or bucket can't inject content. Git SHA-1 is never trusted for identity. Turbo artefacts can carry HMAC signatures, and the PR scopes isolate them.
- **Daemon:** a per-user pipe DACL or 0600 socket. On a shared CI host, each job session is bound to its token's repo, and reads outside it are refused even when the object is cached.
- **Server-to-server:** mTLS or a private network between Forgejo and asc-server, plus internal tokens both ways. `hooks=SkipPre` is accepted only with Forgejo's internal credential.
- **Policy** stays in Forgejo's pre-receive: protected branches and files, required signatures, quota. Clients can't skip it, because the asc server is the only path that moves a ref.
- **Data at rest:** bucket SSE. Server logs carry no content. Licence hygiene: `cargo deny` blocks GPL dependencies in asc crates.

---

## 15. Observability

- `tracing` with OpenTelemetry export. The W3C `traceparent` header is propagated from Forgejo (Go OTel) through gRPC to asc-server and into storage calls, so one PR page can be traced end to end.
- **Prometheus `/metrics`** (on the server, and on the daemon at localhost):
  - RPC latency and errors per method;
  - cache hit ratio per tier;
  - bytes fetched per tier;
  - **hydrations per working copy, by reason and by process image** (ProjFS reports the triggering PID);
  - derive queue depth and lag;
  - ref CAS conflicts and land retries;
  - hook latency and failures, and outbox lag (post-receive and bridge export);
  - shadow mismatch counts per method;
  - turbo cache hits and misses per scope;
  - store latency per backend.
- **Audit:** `ref_log` records who moved which ref, from what, to what, and through which path (asc push, Forgejo merge, bridge).
- **Client:** `asc debug hydrations [--since]`, `asc debug trace <command>` (a span tree), `asc doctor` (ProjFS state, daemon, cache budget, projection consistency, case collisions, reclaimable git debris), and `asc status --timing`. There is no remote telemetry from clients.

---

## 16. Testing strategy

1. **Unit and property tests** in each crate: determinism of encodings (proptest: encode → decode → encode), tree build and diff laws (`diff(a, apply(a, d)) == d`), `Merge<T>` simplification laws, path normalisation and case-fold. Fuzz targets for every decoder: envelope, git objects, proto, index.
2. **Differential tests against git**, the integration-first core, using real repos (asc/dds and soma mirrors, plus generated torture repos):
   - import, then export: every object byte-identical;
   - `ChangedPaths`/`diff --name-status` against `git diff-tree -r -M`;
   - `merge-base`, `rev-list --count`, `log --format` against git;
   - `Merge(dry_run)` conflict sets against `git merge-tree --write-tree` for every branch pair in a sample;
   - unified diffs pass `git apply --check`;
   - `blame` line origins against `git blame --porcelain` (allowing tie differences);
   - `asc status --porcelain=v2 -z` and `ls-files` against git on materialised copies.
3. **RepoApi conformance suite** (`asc-testkit`): one test suite run against `LocalEngine`, `SharedCache<LocalEngine>`, `RemoteRepoApi → asc-server` and `cache-mode → server`. This proves "same RPCs at every tier".
4. **Storage matrix:** InMemory, local filesystem, MinIO and Garage (testcontainers), plus R2 and Ceph when credentials are available. Ref CAS races under load (409 vs 412 handling) and the capability probe.
5. **VFS tests:**
   - Windows ProjFS, on a machine with ProjFS enabled: hydration counter, **stat stability across hydration**, async completion, switch with open files, dehydration, filesystem check after a provider kill.
   - Linux FUSE in WSL 6.18: passthrough, overlay exactness, inode stability across restart.
   - Chaos: kill `ascd` mid-hydration and mid-switch.
6. **Turbo contract tests** (`asc-it`, pinned turbo version, re-run on every turbo minor release):
   - `turbo run build --dry=json` on a clean virtual checkout: task hashes equal a real clone's, and the hydration counter covers manifests only;
   - modifying one file changes only its tasks;
   - `turbo ls --affected` equals a full clone's, with at most `bun.lock` fetched;
   - a racy run (within 1 s of checkout) reports `verified entries = 0`;
   - `git status`, `log`, `ls-files` and `diff HEAD` work with no hydration of clean files.
7. **Forgejo:** the upstream unit and integration suites run in CI three times: `git`, `dual` (the shadow comparator must report zero mismatches) and `asc`. Each phase's patch must keep `git` green.
8. **End to end:** `deploy/compose.yml` brings up Garage or MinIO, Postgres, asc-server, the Forgejo fork and a forgejo-runner with host `ascd`. Scenarios: clone, push, PR, merge, Actions plan job, cached turbo build, bridge `git clone`, bridge push, rollback flip.
9. **Benchmarks:** ASC targets from section 1, plus synthetic 100k-file and 1M-changeset repos, run nightly with regression thresholds.

---

## 17. Roadmap

| Phase | Delivers | Units | Exit criteria |
|---|---|---|---|
| 0 | CI hygiene in monorepo (no asc) and a stand-in `/v8` cache on Garage. Forgejo fork patches 1–4 start in parallel. | F01–F05 | Turbo cache hits in CI. Fork CI green on `git`. |
| 1 | **Data model proven on ASC:** types, store, tree, derive, git import and export | U01–U08 | `roundtrip` on asc/dds and soma: 0 mismatches. Import < 10 min. |
| 2 | **Engine and local-only asc:** diff, merge, engine, config, auth, proto and RPC, turbo cache, IPC, cache, working copy, projection (non-virtual mode), git-remote mode, VFS core, daemon, CLI core | U09–U23, U26–U27 | `asc` local-only passes the CLI-vs-git scenario suite. Turbo contract passes on a materialised copy. |
| 3 | **Virtual worktrees and git-remote mode for developers and agents** (disk relief) | U24, U25, U28 | 13.1 exit criteria on Windows (ProjFS). FUSE in WSL. |
| 4 | **asc-server, turbo cache on the server, shadow** | U29, U30, F06–F10 | 13.2 exit criteria. |
| 5 | **Cutover** asc/dds and soma: `storage_backend=asc`, ID mode `sha1`, bridge eager | F11–F13, M01 | 13.3. Rollback drill done once on staging. |
| 6 | **CI on asc:** plan job, `asc-actions/checkout` sparse, then virtual with host `ascd`. Bridge set to `lazy`. | U31, A01 | Typical PR at 4–8 runner-minutes. `deploy-affected` about 5 s. |
| 7 | **Scale and polish:** derived `last_change` and `blame`, `Land`/pushrebase, `rebase -i`, workspace sync, `node_modules` snapshots, ID mode `asc`, signing, segment packing, macOS NFS, artefacts into asc | later units | per feature |

Phases 1–3 need nothing from Forgejo, and phase 0's fork work needs nothing from asc. The two tracks meet in phase 4.

---

## 18. Implementation units

Rust workspace at `D:\dev\source-control` (edition 2024, toolchain 1.95, `Cargo.lock` committed). Go units are in `D:\dev\forgejo` on branch `asc/v16`. Every unit has **one acceptance command**. A unit is done when that command passes on a clean checkout. Commands marked `[it]` need Docker, Windows ProjFS, WSL or turbo, as noted. The order below is the build order: each unit is verifiable before the next one starts.

| ID | Unit | Owned files | Acceptance command | Depends on |
|---|---|---|---|---|
| U01 | Workspace skeleton | `Cargo.toml`, `rust-toolchain.toml`, `deny.toml`, `.cargo/config.toml`, `proto/buf.yaml` | `cargo check --workspace` | — |
| U02 | `asc-types`: IDs, `RepoPath`, `Changeset`, `Tree`, `Change`, envelope encoding, domain hashing, `Merge<T>` | `crates/asc-types/**` | `cargo test -p asc-types` | U01 |
| U03 | `asc-gitobj`: git blob, tree, commit and tag encode/parse/hash; lossless header capture; LFS pointer parse | `crates/asc-gitobj/**` | `cargo test -p asc-gitobj` (golden fixtures produced by `git hash-object`, `mktree`, `commit-tree`) | U02 |
| U04 | `asc-store`: `BlobStore`, object_store backends, decorators, zstd and dictionaries, FastCDC `ChunkList`, content metadata | `crates/asc-store/**` | `cargo test -p asc-store` (memory and local); `[it]` `cargo test -p asc-store --features it -- --ignored` (MinIO and Garage) | U02 |
| U05 | `asc-refs`: `RefStore`, redb, sql (sqlite and pg), s3cas, capability probe | `crates/asc-refs/**`, `crates/asc-refs/migrations/**` | `cargo test -p asc-refs`; `[it]` `--features it -- --ignored` (Postgres and MinIO) | U02, U04 |
| U06 | `asc-tree`: tree build from changes, hash-pruned `TreeDiff`, walk, git tree oid, composition hook for gitlinks | `crates/asc-tree/**` | `cargo test -p asc-tree` | U03, U04 |
| U07 | `asc-derive`: `Derivable` framework, `root_tree`, `graph`, `git_commit` and aliases, `content_meta`, warm check, backfill | `crates/asc-derive/**` | `cargo test -p asc-derive` | U06 |
| U08 | `asc-git`: gix importer (history, extras, tree overrides, LFS pointers, gitlinks), deterministic exporter, round-trip verifier | `crates/asc-git/**` | `cargo test -p asc-git`; milestone: `cargo run -p asc-git --release --example roundtrip -- D:\dev\monorepo` prints `mismatches: 0` | U07 |
| U09 | `asc-diff`: imara-diff line diff, git-format unified printer, name-status and stat, rename detection among changed paths | `crates/asc-diff/**` | `cargo test -p asc-diff` (fixtures: `git apply --check` and `git diff --name-status -M` goldens) | U06 |
| U10 | `asc-merge`: ported gix-merge text driver, path-set tree merge, rebase, cherry-pick and revert planners, pushrebase conflict check | `crates/asc-merge/**`, `NOTICE` (attributions) | `cargo test -p asc-merge` (goldens against `git merge-file` and `git merge-tree --write-tree`) | U09 |
| U11 | `asc-engine`: `RepoApi` trait plus `LocalEngine` (commit, refs CAS, log, merge-base, diff, blame v1, merge styles, land, grep, archive) | `crates/asc-engine/**` | `cargo test -p asc-engine` | U05, U07, U10 |
| U12 | `asc-testkit`: in-memory fixtures, generated repos, the **RepoApi conformance suite**, git differential harness | `crates/asc-testkit/**` | `cargo test -p asc-testkit` (runs the conformance suite against `LocalEngine`) | U11, U08 |
| U13 | `asc-config`: git config files (gix-config), `asc.*` keys, layering, `-c`, aliases | `crates/asc-config/**` | `cargo test -p asc-config` | U01 |
| U14 | `asc-auth`: Forgejo authz client and cache, Ed25519 JWT verify, git credential helper client | `crates/asc-auth/**` | `cargo test -p asc-auth` | U01 |
| U15 | `asc-proto`: `.proto` files, tonic-prost codegen, conversions to asc-types (prost types never leak past this crate) | `proto/asc/v1/*.proto`, `crates/asc-proto/**` | `cargo test -p asc-proto` | U02 |
| U16 | `asc-rpc`: tonic services over `RepoApi`, `RemoteRepoApi` client, ID-mode codec (git↔asc), resumable streaming | `crates/asc-rpc/**` | `cargo test -p asc-rpc` (conformance suite over an in-process server) | U11, U12, U14, U15 |
| U17 | `asc-turbo-cache`: `/v8` handlers over `BlobStore`, scopes, retention, presign, events | `crates/asc-turbo-cache/**` | `cargo test -p asc-turbo-cache`; `[it, turbo]` `--features it -- --ignored` (real turbo against it) | U04, U14 |
| U18 | `asc-ipc`: local socket ↔ tonic adapters, DACL and permissions | `crates/asc-ipc/**` | `cargo test -p asc-ipc` | U01 |
| U19 | `asc-cache`: `SharedCache<Upstream>` decorator, local durable store split, budgets, LRU, pins, prefetch | `crates/asc-cache/**` | `cargo test -p asc-cache` (conformance suite through the cache) | U11, U12 |
| U20 | `asc-wc`: working-copy state, journal, status, switch planner, non-virtual sparse materialiser (reflink), redirections, op log | `crates/asc-wc/**` | `cargo test -p asc-wc` | U19 |
| U21 | `asc-gitproj`: projection writer (common dir, gitdir, index v2 with exact stat and cache-tree, refs mirror, hooks), fsmonitor v2, `git-remote-asc` protocol | `crates/asc-gitproj/**` | `cargo test -p asc-gitproj` (runs real `git status/ls-files/diff/log/show` on a projected, materialised copy) | U20, U03 |
| U22 | `asc-gitremote`: git-remote mode (shared bare cache per remote, incremental import on fetch, export plus `git push` on push) | `crates/asc-gitremote/**` | `cargo test -p asc-gitremote` (against a local bare repo standing in for Forgejo) | U08, U19 |
| U23 | `asc-vfs`: OS-neutral provider core (placeholder versions, inode/path table, read-through, composition, epochs) | `crates/asc-vfs/**` | `cargo test -p asc-vfs` (in-memory OS harness) | U20 |
| U24 | `asc-vfs-projfs` | `crates/asc-vfs-projfs/**` | `[it, Windows + ProjFS]` `cargo test -p asc-vfs-projfs -- --ignored` | U23 |
| U25 | `asc-vfs-fuse` | `crates/asc-vfs-fuse/**` | `[it, WSL]` `cargo test -p asc-vfs-fuse -- --ignored` | U23 |
| U26 | `ascd` binary: IPC server, mounts, daemon services, turbo tier, dehydrator, filesystem check on start | `bins/ascd/**` | `cargo test -p ascd` (non-virtual mode; virtual tests behind `--features projfs,fuse -- --ignored`) | U17, U18, U20–U23 |
| U27 | `asc` CLI binary (multi-call: `asc`, `git-remote-asc`, `asc-fsmonitor`) | `bins/asc/**`, `bins/asc/tests/scenarios/**` | `cargo test -p asc-cli` (trycmd scenarios: each runs the same script under git and asc and compares porcelain output) | U26, U13, U22 |
| U28 | `asc-it`: cross-component tests (turbo contract, git differential on ASC mirrors, chaos) | `tests/asc-it/**` | `[it]` `cargo test -p asc-it -- --ignored` | U27, U24/U25 |
| U29 | `asc-server` binary: server and cache modes, SQL registry, outbox (hooks and bridge export), Forgejo hook client, admin import and verify | `bins/asc-server/**`, `bins/asc-server/migrations/**` | `cargo test -p asc-server` (SQLite plus local filesystem; mock Forgejo hook endpoint; push, fetch, land, bridge export) | U16, U17, U08 |
| U30 | Deploy bundle (O01): compose with Garage or MinIO, Postgres, asc-server | `deploy/**` | `[it]` `docker compose -f deploy/compose.yml up -d --wait && cargo test -p asc-it --test deploy_smoke -- --ignored` | U29 |
| U31 | `asc-ci`: `asc ci checkout/affected/changed/selftest`, rules (`.asc/ci.toml`), GHA outputs | `crates/asc-ci/**` | `cargo test -p asc-ci` | U27, U29 |
| A01 | `asc/asc-actions` (`checkout`, `affected` composite actions) | `checkout/action.yml`, `affected/action.yml`, `.forgejo/workflows/selftest.yml` | `asc ci selftest` in a runner job (workflow `selftest.yml` green) | U31 |
| F01 | Fork baseline and CI | `.forgejo/workflows/asc-fork.yml` | `go build ./... && make test-backend` | — |
| F02 | Patch 1: path hiding | callers of `git.OpenRepository`, `modules/gitrepo/**`, `modules/git/{blame,diff,log_name_status}.go` | `make test-backend test-sqlite` | F01 |
| F03 | Patch 2: Seam A (`Backend`, `modules/git/gitcli`) | `modules/git/**` | `go test ./modules/git/... && make test-sqlite` | F02 |
| F04 | Patch 3: Seam B (`modules/repostore`, gitcli writer) | `modules/repostore/**`, `services/{pull,wiki,repository/files,repository}/**` (temp-repo call sites) | `go test ./modules/repostore/... ./services/pull/... ./services/wiki/... && make test-sqlite` | F03 |
| F05 | Patch 4: remove raw git calls, plus a checker | `build/check-raw-git/**`, call sites in `modules/indexer/code`, `services/*` | `go run ./build/check-raw-git && make test-sqlite` | F04 |
| F06 | Patch 5: `modules/ascproto` (buf-generated) | `modules/ascproto/**`, `buf.gen.yaml` | `go build ./modules/ascproto/...` | U15 |
| F07 | Patch 6a: `modules/git/ascclient` | `modules/git/ascclient/**` | `ASC_SERVER_BIN=<path> go test ./modules/git/ascclient/...` | F03, F06, U29 |
| F08 | Patch 6b: `repostore` asc writer | `modules/repostore/asc/**` | `ASC_SERVER_BIN=<path> go test ./modules/repostore/asc/...` | F04, F07 |
| F09 | Patch 6c: `storage_backend` column, migration, selection, `dual` backend | `models/repo/repo.go`, `models/forgejo_migrations/**`, `modules/gitrepo/**`, `modules/git/dual/**` | `go test ./models/... ./modules/gitrepo/... ./modules/git/dual/...` | F07 |
| F10 | Patch 7: asc authz, `asc-authenticate`, hooks without quarantine | `routers/private/asc.go`, `cmd/serv.go`, `modules/ssh/**`, `modules/private/**`, `routers/private/hook_*.go` | `go test ./routers/private/... ./cmd/... ./modules/ssh/...` | F09, U14 |
| F11 | Patch 8: `services/ascbridge`, bridge paths in githttp and serv, `ImportGitPack` on receive, LFS from asc, mirrors | `services/ascbridge/**`, `routers/web/repo/githttp.go`, `services/lfs/**`, `services/mirror/**` | `ASC_SERVER_BIN=<path> go test ./services/ascbridge/... ./services/lfs/...` | F10, U29 |
| F12 | Patch 9: Actions asc context and `paths:` through `ChangedPaths` | `services/actions/**`, `modules/actions/**` | `go test ./modules/actions/... ./services/actions/...` | F09 |
| F13 | Full Forgejo suite on the asc and dual backends | `tests/integration/asc_backend_test.go`, CI matrix | `FORGEJO_TEST_STORAGE_BACKEND=asc make test-sqlite test-pgsql` | F08, F10–F12 |
| F14 | Patch 10: ID mode `asc` (normalisation and DB rewrite) — phase 7 | `modules/git/object_format.go`, `models/**` (ID columns), `services/context/**` | `FORGEJO_TEST_OBJECT_FORMAT=asc make test-sqlite` | F13 |
| M01 | ASC migration runbook and tooling | `docs/runbooks/asc-cutover.md`, `bins/asc-server/src/admin/**` | `asc-server admin verify-import --repo asc/dds --repo asc/soma` exits 0 with `mismatches: 0` | U29, F13 |

Parallel tracks: F01–F05 can start on day one. U01–U08 come next, then U09–U14. U15–U18 can run alongside U19–U23.

---

## 19. Open questions and spikes (in order)

1. **ProjFS stat stability** (blocks D4 on Windows): with `Client-ProjFS` enabled, hydrate placeholders and confirm `CreationTime`, `ChangeTime` and `LastWriteTime` stay constant. Also confirm Defender scans don't hydrate whole files on open, and whether a junction redirection inside a virtualization root behaves. Run this first; it is part of U24.
2. **Import fidelity:** run the U08 round trip over all of ASC's objects. It tells us whether tree overrides are ever needed. The 4.6 GB cruft pack is unreachable and is not imported.
3. **Claude Code worktrees:** confirm Claude Code's worktree creation can be hooked to `asc worktree add/remove`. If it can't, ship a `git worktree`-shaped shim limited to agent paths.
4. **forgejo-runner host mounts** (`rshared` bind mount, host daemon socket). The fallback is sparse mode.
5. **S3 conditional writes** on Garage, which ASC already runs. Server mode doesn't need them (refs live in SQL). Only serverless-bucket mode does.
6. **Fold `soma` into `dds`?** If no other repo consumes `asc/soma`, a subtree import removes the composition feature from the critical path. This is the user's call.
7. **Artefacts in asc** (about 200 GiB of local corpus and model data): commit them as lazy large files, or keep `artefacts.ts`? This is the user's call. asc supports it through the sha256 alias with no copying.
8. **Quota semantics** for deduplicated pools (logical size vs pool-unique size).
9. **Turbo upstream:** propose `TURBO_SCM_CHANGED_FILES` and "comment-only `.gitattributes` = none". These are nice to have; the projection works without them.
