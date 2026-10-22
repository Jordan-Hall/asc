# asc architecture: performance and scale first

Status: design proposal (performance/scale angle). Date: 2026-10-22.
Inputs: `docs/research/{forgejo,prior-art,turbo-ci,rust-stack,asc-profile}.md`. Those reports hold the evidence; this document makes decisions.

Fixed user decisions (not reopened here): Bonsai-style changesets with content-addressed immutable data; full storage replacement inside a Forgejo fork; Rust client, daemon and server; an `asc` CLI that mirrors git; storage on S3-compatible stores and local disk, with and without a server; lazy materialisation through a virtual filesystem; cheap worktrees; faster Turborepo CI.

---

## 0. Summary

asc is Scion's shape on ASC's infrastructure. Immutable changesets, trees and file contents live in an object store (Garage/S3/MinIO/R2/Ceph or a local directory). The only mutable state is refs, moved by a compare-and-swap on one row. The server owns history: diff, merge-base, log, blame, merge, rebase and land run there as RPCs. One daemon per machine owns a bounded shared cache, the virtual working copies, and a local copy of the engine for offline work. The CLI, the daemon, a regional cache and the server all implement the same `RepoApi`. Local-only mode is that same API over the engine embedded in the daemon.

The performance stance is one rule: **every hot operation costs O(size of the change), never O(size of the repo or its history).**

| Operation | Why it is O(change) |
|---|---|
| `asc status` | The VFS journal lists the only paths that can differ |
| `asc switch` | Hash-pruned tree diff, then placeholder updates for changed paths only |
| `asc diff --name-only A..B`, `asc affected` | Union of changesets' `changes` maps, or a hash-pruned tree diff on the server |
| `asc worktree add` | Writes a state row and mounts; no file is written |
| `asc land` (pushrebase) | Path-set intersection against an indexed path log; rebase is metadata-only |
| `asc log -- <path>` | Indexed path-change table, O(results) |
| turbo hashing on a virtual checkout | Git blob SHA-1 and stat data come from tree metadata; zero file reads |

Twelve decisions carry most of the performance:

1. **Storage bytes = wire bytes = cache bytes.** One envelope format (§3.7). No tier decodes, re-compresses or re-packs anything. Verification happens once, at the consumer.
2. **Metadata answers content questions.** Every tree entry carries `size`, `git_oid` and a descendant file count. Every changeset `Write` carries `size` and `git_oid`. Tree derivation, the git bridge, turbo hashing, `--stat` totals, VFS `stat` and `readdir` never read file bytes.
3. **Rebase is metadata-only.** Rebasing a changeset whose paths do not intersect the new base's changes reuses its `changes` map unchanged. Only the parent pointer and the derived root tree change. This makes server pushrebase cheap enough for tens of thousands of lands a day.
4. **Indexed path log.** At land, each changed path and its ancestor directories are written to `path_changes(repo, path_id, generation)`. `log -- dir`, pushrebase conflict checks, affected-since and blame seeds become index range scans.
5. **Commit graph with generation numbers and skip pointers** in SQL. Merge-base and ancestry are O(log n).
6. **Want-driven fetch with two shortcuts:** `Prefetch(root, cones)` streams a whole tree closure in one response, and `TreeDiff(old, new)` streams only the differing tree nodes. Clone and switch each take one round trip.
7. **Segments on the object store, an object index in the database.** Each upload batch becomes one segment object with a footer index. Reads are ranged GETs. Existence checks are SQL lookups, not S3 HEADs.
8. **A per-machine cache in two parts.** A bounded append-only segment log for trees and small objects, with O(1) eviction. A decompressed file store that backs FUSE passthrough and reflinks.
9. **Warm trunk.** The daemon subscribes to ref updates (`WatchRefs`). When trunk moves it pulls the `TreeDiff` and, for small repositories, the new content too. All of ASC's live history is 311 MiB compressed, so by default the daemon keeps all of trunk's content local. Laziness then governs the *working copy*, not the network.
10. **Write-behind upload.** `asc commit` writes a draft changeset in the daemon's local engine in milliseconds and streams its objects upstream in the background. By the time the user types `asc push`, the push is one `Land` or `UpdateRefs` call with nothing left to send.
11. **A git projection for turbo and other git readers.** Each working copy gets a blobless `.git` whose index stat data equals what the VFS reports, so upstream `git` and `turbo` run unmodified with zero hydration of clean files.
12. **A land queue per (repo, ref).** Lands are serialised by one leader per ref, batched, and committed with one CAS per batch.

Simplicity notes. One metadata database per deployment (Postgres or SQLite) holds refs, the commit graph, the object index, the path log, queues and derived-data mappings. There is no extra queue, cache or coordination service: queues are tables using `SKIP LOCKED`, and leader election is a Postgres advisory lock. One server binary has three roles (`serve`, `worker`, `--cache`). ASC's whole repository fits in RAM on one node; horizontal scale is designed in (§8.4) but not deployed on day one.

---

## 1. Workload and targets

### 1.1 ASC today (from `asc-profile.md`)

| Quantity | Value | Design consequence |
|---|---|---|
| HEAD | 4,543 files + 1 submodule, 994 dirs, 119 MiB | All HEAD trees are about 0.5 MB raw. One `Prefetch` covers them |
| File sizes | median 4.2 KiB, p99 126 KiB; 12 files are 59% of bytes | Small-object path dominates request count; large-file path dominates bytes |
| Live history | 5,502 commits, 104,800 objects, 311 MiB compressed | Whole history fits in a daemon cache and in server RAM |
| `.git` | 58 GB, of which about 0.4 GB is live | Removing per-worktree submodule copies, LFS and cruft is the disk win |
| Changes per commit | median 4 files, p95 35, p99 352, max 4,710 | Changesets are tiny; the tail needs batched RPCs |
| Packages per commit | median 1, p95 6 | Affected sets are small; caching wins dominate CI |
| Commit rate | 26/day mean, 303/day peak; 1,592 in 30 days | Land throughput is not a problem today; agent growth is |
| Worktrees | 50, about 25 agent worktrees, 1 human + about 20 agents | Worktree creation and per-worktree disk are the main pain |
| Hottest file | `bun.lock`, 1.1 MB, in 7.5% of commits | Lockfile diff at base must be one blob fetch, never a checkout |

### 1.2 Design points

- **D1 (ASC at 10x):** 100k files and 5 GiB per snapshot, files of several GiB, 1M commits, 10k branches, 50 commits/minute bursts on one branch, single commits touching 50k files, 200 working copies per machine.
- **D2 (agent scale, Scion-like):** 30k lands per day per repository (peaks about 5 per second), 1,000 concurrent agent worktrees across a fleet, 25M object reads per hour on the server tier.

The architecture must meet D1 on one server node and reach D2 by adding stateless server nodes and runner daemons only (§8.4).

### 1.3 Latency and cost targets

All targets are measured by `asc-bench` scenarios (§20.4) on the ASC import and on a synthetic D1 repository. "Warm" means the daemon cache holds trunk trees. "LAN" means the asc server is on the local network.

| Operation | Target (ASC, warm) | Target (D1) | Mechanism |
|---|---|---|---|
| `asc clone` (virtual) | < 2 s | < 5 s | One `Prefetch` of trunk trees; mount; write git projection |
| `asc worktree add` | < 300 ms | < 1 s | State row + mount + index write from cached trees |
| Per-worktree disk, Linux | < 5 MiB + modified files | < 20 MiB | FUSE passthrough from the shared file store |
| Per-worktree disk, Windows | < 5 MiB + modified + hydrated files | < 50 MiB at rest | ProjFS placeholders + dehydration GC |
| `asc status`, clean | < 30 ms | < 50 ms | Journal is empty; no tree walk |
| `asc status`, 50 modified | < 80 ms | < 100 ms | Hash only journaled paths |
| `asc switch`, 20 changed paths | < 150 ms | < 300 ms | `TreeDiff` + `PrjUpdateFileIfNeeded` / FUSE invalidations |
| First read of an unhydrated file, cache hit | < 1 ms | < 1 ms | Daemon segment log, in-process VFS |
| First read, cache miss, LAN | < 20 ms | < 30 ms | Server RAM/NVMe cache, ranged segment read |
| `asc commit` (5 files) | < 50 ms | < 50 ms | Local draft; upload is write-behind |
| `asc push` / `asc land` to trunk | p50 < 250 ms, p99 < 2 s | same at 5 lands/s | Land queue, batched CAS |
| `asc log -n 50` | < 50 ms | < 100 ms | Graph rows + changeset headers |
| `asc log -- <dir> -n 50` | < 80 ms | < 150 ms | `path_changes` index |
| `asc merge-base A B` | < 10 ms | < 20 ms | Generation numbers + skip pointers |
| `asc diff --name-only A...B` | < 50 ms | < 200 ms | Changeset maps or hash-pruned tree diff |
| `asc blame <file>` | < 300 ms (derived, warm) | < 1 s | Derived blame from parent's blame |
| `asc affected` (CI plan job, no checkout) | < 2 s | < 3 s | `ChangedPaths` + cached package graph |
| `turbo run build --dry` on a virtual checkout | same as a real clone, 0 content reads for clean files | same | Git projection |
| Import of monorepo full history | < 30 min | < 6 h for 1M commits | Parallel gix decode, batched uploads |

### 1.4 Capacity check

- Changeset objects: median about 600 bytes. 1M changesets is about 1 GB, which fits in server RAM.
- `path_changes`: about 4 files x about 4 path levels per commit, so 16 rows per commit; 16M rows at D1. Postgres handles this with one composite index.
- Commit graph rows: about 100 bytes each, 100 MB at D1.
- Land CPU: a non-conflicting pushrebase of a 4-file changeset rewrites about 16 tree nodes (each a few hundred bytes) and does one indexed range scan. That is well under 5 ms of CPU. A single land leader therefore sustains more than 100 lands/s; D2 needs 5/s at peak.
- Read fan-out: Scion reports more than 99% hit rates per tier. Runner daemons and the server's RAM cache absorb almost all reads; the object store sees mostly cold misses and writes.

---

## 2. Architecture overview

```
  developer machine / CI runner host                                   server site
 ┌────────────────────────────────────────────┐            ┌─────────────────────────────────────────┐
 │ asc (CLI) ─┐   git / turbo / IDE / bun       │            │  Forgejo fork (Go)                       │
 │           │        │ (read projected .git)  │            │   modules/git/ascclient ──┐               │
 │       IPC (pipe/UDS, gRPC)                  │            │   modules/repostore ─────┤ gRPC          │
 │           ▼        ▼                        │   gRPC     │   routers/private/asc ◄───┼── authz/hooks │
 │ ascd ──────────────────────────────────┐    │  (TLS)     │                          ▼               │
 │  RepoApi tier: local engine + cache   ├────┼──────────► │  asc-server (N stateless frontends)       │
 │  VFS: ProjFS | FUSE | NFS | non-virt  │    │            │   RepoApi over engine; land leaders;     │
 │  shared cache: segment log + files    │    │            │   RAM/NVMe object cache; /v8 turbo cache │
 │  /v8 turbo cache tier (127.0.0.1)     │    │            │   git bridge endpoint                    │
 │  worktrees: W1..Wn (mounts + overlay) │    │            │  asc-server --role worker (derivation)    │
 └────────────────────────────────────────┘   │            │        │                  │              │
                                              │            │   Postgres/SQLite     object store       │
          optional regional tier: asc-server --cache        │   refs, graph, index  segments, chunks   │
                                                           └─────────────────────────────────────────┘
```

**Where an operation runs.** The daemon's `RepoApi` implementation decides per call with one rule:

- Working-copy operations (status, add, restore, commit, stash, switch planning) run in the daemon.
- History operations (log, blame, merge-base, diff of committed ranges, grep over history) run on the server, because the server has every object hot and the derived indexes. If the daemon is offline, it answers from its local engine and cache, and says so (`(offline: partial history)`).
- Mutations of shared refs (push, land, branch delete on the remote) always go to the server. Local drafts are created locally.

---

## 3. Data model

All types live in `asc-types`, a crate with no IO. Every ID is a distinct Rust newtype, so a `TreeId` can never be passed where a `ContentId` is expected.

### 3.1 IDs and hashing

| ID | Definition | Notes |
|---|---|---|
| `ContentId` | `BLAKE3(bytes)` (plain hash of the full file bytes) | Plain BLAKE3 keeps Bao verified range reads available. Identity does not depend on chunking |
| `ChunkId` | `BLAKE3(chunk bytes)` | Same function; a separate type |
| `TreeId` | `BLAKE3::derive_key("asc.v1.tree", canonical bytes)` | |
| `ChangesetId` | `BLAKE3::derive_key("asc.v1.changeset", canonical bytes)` | |
| `GitOid` | 20-byte SHA-1 | Derived attribute, never an identity |

Display: 64 lowercase hex characters, abbreviated to the shortest unique prefix of at least 12. This looks like a SHA-256 git object ID, which Forgejo already stores (`VARCHAR(64)`) and parses through its per-repo object-format switch (`forgejo.md` finding 3). `asc rev-parse --git <rev>` prints the bridge SHA-1.

### 3.2 File content

- **Small content (< 4 MiB):** one object whose payload is the file bytes, zstd-compressed (§3.7).
- **Large content (>= 4 MiB):** split with FastCDC v2020 (min 256 KiB, avg 1 MiB, max 4 MiB). The object stored under the `ContentId` is a `ChunkList { total_size, chunks: Vec<(ChunkId, u32 len)> }`. Chunks are stored as standalone objects, outside segments, so they can be range-read and cached on a separate budget. Content-defined chunking matters for ASC: its 45-176 MiB JSONL training sets are appended to and rewritten often, and fixed-size or whole-file storage would cost about 20x more.
- **Bao outboards** (16 KiB chunk groups) are derived data for chunked contents. They let the VFS verify any byte range without reading the whole file. Phase 6.
- **`ContentMeta`** is derived at ingest from the bytes the writer already has: `size`, `git_oid` (SHA-1 of `"blob <len>\0" + bytes`), `is_binary` (NUL in the first 8 KiB), and `sha256` only for contents over 4 MiB (needed for LFS pointers on export). It is stored in the object index row, and `size` and `git_oid` are also copied into changesets and trees.
- There are no LFS pointer files. On import, pointers are resolved into native content and the change is flagged `lfs_on_export` in `extra`, so the git bridge can emit the pointer again.

### 3.3 Trees (derived, content-only, fsnode-style)

```rust
pub struct Tree {
    pub version: u8,                 // 1
    pub entries: Vec<TreeEntry>,     // sorted by name bytes; unique; no case-fold duplicates
}
pub struct TreeEntry {
    pub name: PathComponent,         // validated UTF-8, no '/', no NUL, not "." or ".."
    pub kind: EntryKind,             // File | Exec | Symlink | Dir | Submodule
    pub id: ObjectRef,               // ContentId | TreeId | ChangesetId (submodule)
    pub size: u64,                   // file size; for Dir, total bytes below
    pub git_oid: GitOid,             // blob, tree or commit SHA-1 in the git bridge
    pub files: u32,                  // 1 for files; for Dir, number of files below
}
```

- History is not in tree IDs. The same content always has the same `TreeId`, so unchanged subtrees are shared across branches, worktrees and repositories in one namespace.
- `size`, `files` and `git_oid` let the VFS answer `stat` and `readdir`, let `--stat` and prefetch budgets be computed, and let the git projection write an index with a TREE extension, all without content.
- Directories with more than 4,096 entries are stored as a sharded tree: the node holds `Vec<(name_prefix, TreeId)>` of sub-nodes, each under 4,096 entries. Logical lookup is unchanged. ASC has no such directory today; `node_modules` is never tracked.
- Trees are **derived data** (§5). They are never part of a changeset's hash. A future tree format is a new derived type, not a history rewrite.

### 3.4 Changesets (Bonsai)

```rust
pub struct Changeset {
    pub version: u8,                              // 1
    pub kind: ChangesetKind,                      // Commit | Snapshot
    pub parents: SmallVec<[ChangesetId; 2]>,      // first parent is the trunk-side parent
    pub author: Signature,                        // name, email, time (unix secs), tz offset
    pub committer: Signature,
    pub message: BString,
    pub extra: BTreeMap<CompactString, BString>,  // e.g. "git-sig", "lfs_on_export:<path>", "sshsig"
    pub changes: BTreeMap<RepoPath, Change>,      // relative to parents[0]
}
pub enum Change { Write(FileWrite), Delete }
pub struct FileWrite {
    pub content: ContentId,       // for Submodule: the nested repo's ChangesetId bytes
    pub kind: FileKind,           // Regular | Exec | Symlink | Submodule
    pub size: u64,
    pub git_oid: GitOid,
    pub copy_from: Option<(RepoPath, ChangesetId)>,
}
```

- `changes` is relative to the first parent. For merges it also lists every path whose merged result differs from the first parent, so `tree(p1) + changes = tree(self)` always holds (Mononoke rule). Merge resolution never needs to be recomputed to rebuild a tree.
- Because `size` and `git_oid` are in each `Write`, tree derivation and git-bridge derivation never fetch content.
- `copy_from` is explicit (`asc mv`, or a rename seen by the VFS). Guessed copies for imported history are a separate derived type.
- `ChangesetKind::Snapshot` allows untracked-file changes and conflicted contents. Snapshots back `stash`, workspace sync and agent handoff. They can never be the target of a protected ref.

### 3.5 Conflicts as data

- Port jj's `Merge<T>` algebra (Apache-2.0, with attribution) into `asc-merge`. A conflicted file is stored in a Snapshot as a `Merge<ContentId>` object (`kind = conflict` in the envelope), with adds and removes.
- Working files get git-style markers (`merge`, `diff3`, `zdiff3`, chosen by `merge.conflictStyle`). `asc add` parses them back.
- Rebasing a conflicted snapshot simplifies terms algebraically, so conflicts never nest.

### 3.6 Submodules

- v1: a `Submodule` entry points at a `ChangesetId` in another asc repository (`extra` holds its repository name). The VFS shows it as a **nested virtual mount** of that repository at that changeset. It costs nothing until read and shares the machine cache. That alone removes the 27 GB of per-worktree submodule git directories and the 3.3 GB main-checkout copy.
- `git_oid` holds the git commit SHA in the nested repo's bridge, so the git projection writes a normal gitlink entry.
- Recommendation for ASC, Phase 5 or later: import `packages/soma/sdk` with its history as a subtree of the monorepo, if no other repository consumes `soma.git`. Then the nested mount, the second LFS store and `submodules: recursive` all disappear. The nested-mount mechanism remains for external dependencies.

### 3.7 Canonical encoding and the envelope

- Hashed structures are encoded with `postcard` 1.x from structs and sorted `Vec`/`BTreeMap` only. A property test re-encodes every decoded object and compares bytes.
- Every stored object is an **envelope**:

  ```
  byte 0      kind     (content, chunklist, tree, changeset, conflict, meta)
  byte 1      codec    (0 raw, 1 zstd, 2 zstd+dict)
  bytes 2-3   dict id  (u16, 0 if none)
  bytes 4..   payload
  ```

- The ID is computed over the decoded canonical bytes. Tiers move envelopes untouched: object store, server cache, regional cache, daemon segment log and RPC responses all hold the same bytes. Only the consumer decompresses and verifies (one BLAKE3 pass, about 3 GB/s per core).
- zstd level 3 on write. The server's compactor may rewrite cold objects at level 19 with trained dictionaries per file class (`.ts/.tsx`, `.json`, `.rs`, `.py`, lockfiles). Dictionaries are immutable objects (`dicts/<id>`) that every tier caches forever. gRPC-level compression is off for object streams, because the payloads are already compressed.

### 3.8 Paths and case

- `RepoPath` is validated UTF-8, `/`-separated, with no empty, `.`, `..` or `.git` components.
- The eager tree derivation rejects any tree with two names that differ only by case (Unicode simple case fold), because ProjFS and NTFS are case-insensitive. The server refuses lands that create a collision; `asc commit` checks first with a clear error. Import maps existing collisions by renaming with a suffix and records the mapping in `extra`.

---

## 4. Storage

### 4.1 `ContentStore` trait and decorators

```rust
pub trait ContentStore: Send + Sync {
    async fn get(&self, key: &ObjectKey) -> Result<Option<Bytes>>;
    async fn get_range(&self, key: &ObjectKey, range: Range<u64>) -> Result<Bytes>;
    async fn put_if_absent(&self, key: &ObjectKey, body: Bytes) -> Result<PutOutcome>; // Created | Existed
    async fn delete(&self, key: &ObjectKey) -> Result<()>;   // GC only
    async fn list(&self, prefix: &KeyPrefix) -> BoxStream<Result<ObjectKey>>;
}
```

- Backends over `object_store` 0.14: `LocalFileSystem`, S3-compatible (`aws` feature), `InMemory`.
- Decorators, stacked from config (Mononoke style, each about 100 lines): `Prefix` (per-namespace), `Verify` (hash on read, on in clients), `Cache` (RAM + NVMe, server tier), `Coalesce` (deduplicates in-flight reads of the same key), `Metrics`, `Throttle`, and `Chaos`/`Delay` for tests.
- Writes are `PutMode::Create`, and `AlreadyExists` counts as success. Immutable writes therefore need no conditional-write support and work on Garage.

### 4.2 Bucket layout and segments

```
<ns>/seg/<yyyymmdd>/<uuidv7>      segment: envelopes back to back + footer index
<ns>/chunk/<hh>/<chunk-id>        large-file chunks, standalone
<ns>/obj/<hh>/<id>                loose objects (serverless and local-only modes only)
<ns>/dicts/<id>                   zstd dictionaries
<ns>/snap/<changeset-id>/<tree-closure-id>   bootstrap snapshots (§9.5)
turbo/<repo>/<scope>/<hash>[.meta]          turbo remote cache (§13.4)
refs/<name>                       serverless mode only
```

- **Segments solve S3 small-object cost and latency.** ASC's median object is 4 KiB; one PUT per object is slow and expensive. Each `PutObjects` batch (one commit, one import batch) becomes one segment PUT, plus standalone chunks. The footer is a sorted `(id, offset, len)` table, so a segment can be read without the database.
- **Compaction** (worker role, Phase 6): merge small segments of the same week into 64 MiB segments, group objects by path so the versions of one file sit together, and re-compress cold objects with dictionaries. Compaction writes the new segment, swaps index rows in one transaction, and deletes the old segment after a grace period. Readers holding an old location retry through the index.
- **Serverless and local-only modes** keep loose objects (`obj/`), because they have no shared index. That is simpler and fast enough at single-user or small-team scale. Local-only mode can opt into segments, using redb as the index.

### 4.3 Object index

`objects(ns, id, kind, size, git_oid, location)` in the metadata database, where `location` is `(segment key, offset, len)` or `chunk`.

- `FindMissing(ids)` is one `SELECT ... WHERE id = ANY($1)`, not N S3 HEADs.
- The index row is written only after the segment PUT succeeds (write order: bytes, then index, then refs). An orphan segment from a crash is harmless and is removed by GC.
- `git_oid -> ContentId` lookup is an index on the same table. The git bridge and `git-remote-asc` need it.

### 4.4 Refs

```rust
pub trait RefStore: Send + Sync {
    async fn get(&self, name: &RefName) -> Result<Option<RefValue>>;           // (ChangesetId, RefVersion)
    async fn list(&self, prefix: &RefPrefix, after: Option<&RefName>, limit: u32) -> Result<Vec<(RefName, RefValue)>>;
    async fn transact(&self, txn: RefTxn) -> Result<TxnOutcome>;                // atomic multi-ref CAS
    fn watch(&self, prefix: &RefPrefix) -> BoxStream<RefEvent>;
}
pub struct RefTxn { pub updates: Vec<RefUpdate>, pub reason: RefReason }       // RefUpdate { name, expected: Expect, new: Option<ChangesetId> }
pub enum Expect { Any, Absent, Version(RefVersion), Value(ChangesetId) }
```

| Mode | Implementation | Multi-ref transaction | Watch |
|---|---|---|---|
| Client/server (ASC) | `refs(repo, name, cs, version, expires_at)` in Postgres or SQLite; `UPDATE ... WHERE version = $old` | Yes, one SQL transaction | `LISTEN/NOTIFY` (Postgres), in-process channel (SQLite) |
| Serverless bucket | `refs/<name>` objects, `PutMode::Update` with ETag. Enabled only if the `asc init` probe passes (AWS S3, MinIO, R2, Ceph >= 20.1; not Garage) | No; single-ref only | Polling with `If-None-Match` |
| Local-only | redb table | Yes | In-process |

- `ref_log(repo, name, seq, old, new, actor, reason, at)` is append-only, written in the same transaction. It serves `reflog`, audit and the Forgejo push feed.
- **Expiring refs:** refs under `refs/agents/*` and `refs/snapshots/*` carry `expires_at` (default 14 days after last update). A worker deletes expired refs. ASC has 79 `worktree-agent-*` branches today, and D1 assumes 10k branches.
- Serverless retry hazard: a retried create may return 412 after the first attempt succeeded. The ref object stores the new value, so "412 and current value equals mine" is success.

### 4.5 Namespaces and the dedup scope

The object namespace (`ns`) is a **fork network**: a repository and all its forks share one namespace (the GitHub model). Deduplication happens inside a namespace, never across. This closes the content-addressing leak where a user who can read repo A asks for an object ID that only exists in private repo B. The server authorises every object read against the repository and serves from that repository's namespace only. ASC has one main repository, so cross-repo dedup has no value worth the risk.

### 4.6 Garbage collection and retention

- Objects are never deleted on the write path.
- A weekly worker job marks objects reachable from live refs, unexpired snapshot refs and changesets younger than 30 days. It marks by walking `changes` maps and derived trees through SQL, not by listing the bucket. Unmarked index rows older than the grace period are deleted. Segments whose live fraction is below 50% are compacted, and empty segments are deleted.
- ASC's total today (about 105k objects) makes this a seconds-long job. At D1 it is incremental: mark from changesets added since the last run, plus a full pass monthly.

### 4.7 Mode matrix

| | Local-only | Serverless bucket | Client/server |
|---|---|---|---|
| Objects | local dir (loose; optional segments) | S3-compatible (loose) | S3-compatible or local dir (segments) |
| Refs and indexes | redb in the daemon | bucket CAS objects + daemon-local indexes | Postgres / SQLite |
| History RPCs run in | daemon engine | each client's engine | server |
| Derived data | computed locally, stored in the store | computed by clients, stored under `derived/` by content key | server workers |
| Typical use | solo, offline, tests, `asc init` | small teams, backup mirror | ASC default, Forgejo |

All three run the same `asc-engine` code. Only `ContentStore`, `RefStore` and `IndexStore` implementations differ.

---

## 5. Derived data

```rust
pub trait Derivable: Sized + Send + Sync + 'static {
    const NAME: &'static str;              // "fsnode.v1", "gitmap.v1", "blame.v1", ...
    const MODE: DeriveMode;                // Eager | Deferred
    type Value: Encode + Decode;
    /// Compute from this changeset and its parents' values of the same type (plus declared deps).
    async fn derive(ctx: &DeriveCtx, cs: &Changeset, parents: &[Self::Value]) -> Result<Self::Value>;
}
```

- Mappings are stored as `derived(type, changeset) -> value or object key` in the index database. Values are immutable, so any tier can cache them.
- **Warm-ref rule:** a ref move becomes visible (in `ListRefs`, `WatchRefs` and Forgejo) only after every Eager type exists for the target. Readers never wait on derivation. Land latency includes eager derivation; that is why the eager set is kept small and O(change).
- Deferred types are derived on first request (with in-flight coalescing) or by workers from `derive_queue` (`SELECT ... FOR UPDATE SKIP LOCKED`). Backfill runs in topological order, sliced by generation for parallelism.
- Versioning: a new format is a new type (`blame.v2`), backfilled alongside the old one, then switched.

| Type | Mode | Cost per changeset | Used by |
|---|---|---|---|
| `fsnode` (root `TreeId`) | Eager | O(changed paths x depth) tree nodes; no content reads | everything |
| `graph` (generation, skip pointers) | Eager | O(1) row | merge-base, ancestry, log |
| `path_changes` | Eager | one row per changed path and ancestor dir | log -- path, pushrebase, affected, blame seeds |
| `case_check` | Eager (inside fsnode) | O(changed dirs) | ProjFS safety |
| `gitmap` (git commit + tree SHA-1s, bidirectional mapping) | Eager if `git_bridge = eager`, else Deferred | O(changed dirs); uses `git_oid` from changes | git bridge, git projection, Forgejo SHA display, turbo |
| `changeset_info` (header without the file map) | Eager | tiny | fast `log` lists |
| `last_modified` (per directory entry: last changeset that touched it) | Deferred, eager for trunk | O(changed paths x depth) | Forgejo tree view, `asc ls-tree --last-commit` |
| `blame` | Deferred | O(lines of changed files) | `asc blame`, Forgejo |
| `inferred_copies` | Deferred | O(changed files), limited to the change set | log --follow, merges of imported history |
| `pkg_graph` (turbo package graph) | Deferred, eager on trunk; keyed by the TreeId of manifest inputs | sandboxed `turbo ls` on a virtual checkout, run only when a manifest changes | `asc affected`, CI plan job |
| `line_diff` | Deferred, keyed by `(ContentId, ContentId, algo)`, not by changeset | O(file) | Forgejo PR diffs, `asc diff` of committed ranges |
| `bao` outboard | Deferred, chunked contents only | O(file) | verified range reads |
| `bootstrap` snapshot (§9.5) | Deferred, trunk only, at most every 10 min | O(tree closure) | clone, daemon cold start |

---

## 6. History engine

All of this is `asc-engine`, embedded in the server and in the daemon.

### 6.1 Commit graph

`graph(repo, cs, gen, p1, other_parents[], skip)` where `gen = 1 + max(gen(parents))` and `skip` is a skew-binary skip pointer along first parents (the Mononoke/Sapling idea; Myers' skew-binary list gives O(log n) jumps to any ancestor generation).

- `is_ancestor(a, b)`: jump from `b` along skip pointers to `gen(a)`, then compare; merges fall back to a bounded BFS ordered by generation.
- `merge_base(a, b)`: walk both frontiers by descending generation using skip pointers to close the gap. O(log n) for trunk-based history, which is nearly all of ASC's.
- Rows are immutable after insert, so read replicas and caches can serve them. A miss on a replica retries on the primary; replica lag can only cause a miss, never a wrong answer.

### 6.2 Path log

`path_changes(repo, path_hash, gen, cs)` with primary key `(repo, path_hash, gen DESC)`. `path_hash` is a 64-bit hash of the normalised path; collisions are filtered by checking the changeset. Each changed path writes rows for itself and each ancestor directory, up to the root (excluded).

- `log -- <path>` on a linear range: one index range scan, O(results). On non-linear ranges: scan, then filter by ancestry.
- Pushrebase conflict check: "did any of my paths or their ancestors change on the target since my base's generation?" is one scan per path prefix in a batch query.
- `ChangedPaths(base, head)` for a linear range is the union of the range's `changes` maps (no tree walk). For non-linear ranges, or ranges longer than 500 changesets, the engine uses a hash-pruned tree diff of the two root trees instead. The planner picks the cheaper of the two using the generation gap.

### 6.3 Diff

- **Tree diff** walks both trees and skips equal `TreeId`s. O(changed directories + changed entries). Entries carry `size` and `git_oid`, so `--stat` totals, `--name-status` and exact-rename detection (equal content IDs) need no content.
- **Line diff** uses `imara-diff` (histogram by default, Myers on request) with a unified printer tested byte for byte against `git diff` (§20.2). Results for committed content pairs are cached as `line_diff` derived data, so Forgejo PR pages and repeated `asc show` calls are cache hits.
- Rename detection after exact matches: same basename, then similarity limited to the added/deleted sets of this diff (merge-ort's lesson), capped by `diff.renameLimit`.

### 6.4 Merge, rebase, land

- **Tree merge** of `base..ours` and `base..theirs` is a union of two path-change sets. Paths changed on one side take that side; paths changed identically on both sides auto-resolve (same-change rule); paths changed differently on both sides get a 3-way text merge (the `gix-merge` text driver ported onto `imara-diff`, giving git-identical markers). Directory/file and case conflicts are checked with the skeleton of the changed paths only. Only both-sides paths fetch content.
- **Rebase** of a changeset `C` onto `T`: if `paths(C)` and their ancestors do not intersect `paths(base(C)..T)`, the new changeset is `C` with `parents = [T]` and the same `changes`. No content is read. Otherwise only the intersecting files are merged. This is decision 3, and it is why server-side land is cheap.
- **Land (pushrebase) to a trunk ref:**
  1. The client's objects are already uploaded (write-behind). `Land { stack: [cs...], target: "main", expected_base: B }` is enqueued in `land_queue`.
  2. The leader for `(repo, main)`, chosen by a Postgres advisory lock, takes up to 64 queued requests.
  3. For each request in queue order: path-log conflict check against the current tip (including earlier requests in this batch), metadata rebase, eager derivation of new changesets (fsnode, graph, path_changes, gitmap if eager), Forgejo `pre-receive` hook call (§15.4).
  4. One `RefTxn` moves `main` from its tip to the last accepted changeset. Rejected requests get a conflict result listing the paths.
  5. Forgejo `post-receive` is called once per batch with all updates; it fans out webhooks and Actions.
  6. Each client receives its old-to-new changeset mapping, so it needs no fetch.

  Batching amortises the hook round trips and the CAS. Requests are never reordered across conflicts: a conflicting request fails without blocking the rest.
- Path-level conflicts are conservative, as in Mononoke: the same file on both sides is a conflict even if the lines differ. `asc land --merge-content` opts into a server-side content merge for non-overlapping hunks, recorded as a merge in the changeset metadata. Off by default for trunk.

### 6.5 Blame and history queries

- `blame.v1` for `(path, cs)` is derived from the parent's blame of the same path plus the line diff of this changeset. Most requests touch recently changed files, whose parents' blames are already derived.
- `last_modified` gives Forgejo's directory listings ("last commit per entry") in one RPC.

---

## 7. RPC surface

One `.proto` package, `asc.v1`, under `proto/`. Rust uses `tonic`/`prost`; the Forgejo fork generates Go stubs from the same files. Object payloads are opaque `bytes` envelopes. Every RPC takes a `repo` and an auth token in metadata.

| Service | RPC | Shape | Notes |
|---|---|---|---|
| Objects | `FindMissing(ids)` | unary, up to 10k ids | SQL index lookup |
| | `GetObjects(stream ids)` | bidi stream | Resumable: re-request what is missing. Responses up to 4 MiB |
| | `PutObjects(stream envelopes)` | client stream | One segment per call; returns index ack |
| | `GetChunkRange(content, range)` | server stream | Large files; Bao-verifiable |
| Trees | `GetTrees(ids, depth)` | server stream | Level-batched BFS |
| | `Prefetch(root, cones[], blob_max_size, max_bytes)` | server stream | Tree closure of cones plus small blobs; served from the bootstrap snapshot when possible |
| | `TreeDiff(old_root, new_root, cones[])` | server stream | Only differing tree nodes, plus new small blobs on request |
| History | `Resolve(revspec[])` | unary | git revision syntax, server-side |
| | `Log(range, paths[], first_parent, limit, cursor)` | server stream | headers from `changeset_info` |
| | `MergeBase(a, b[])`, `IsAncestor(a, b)` | unary | |
| | `ChangedPaths(base, head, merge_base: bool, paths[])` | server stream | path, status, old/new id, size, git_oid |
| | `Diff(base, head, paths[], format)` | server stream | `unified`, `stat`, `numstat`, `name-status`; byte-identical to git |
| | `Blame(path, cs, ranges)` | server stream | |
| | `LastModified(cs, dir)` | unary | Forgejo tree view |
| | `Grep(cs, pattern, paths[])` | server stream | Bulk blob read on the server |
| Mutations | `CreateChangeset(parent, file_ops[], meta)` | unary | Server builds the changeset from file ops (web editor, API, agents without a mount) |
| | `UpdateRefs(RefTxn)` | unary | Runs Forgejo hooks around the CAS |
| | `Land(stack, target, expected_base, options)` | unary | §6.4 |
| | `Merge(base_ref, head, style, dry_run)` | unary | All Forgejo merge styles; `dry_run` = mergeability check |
| | `Rebase`, `CherryPick`, `Revert`, `ApplyPatch` | unary | |
| Refs | `ListRefs(prefix, cursor, limit)` | server stream | |
| | `WatchRefs(prefixes)` | server stream | Push notifications; daemons keep trunk warm |
| Workspaces | `PutSnapshot(cs)`, `ListSnapshots(owner)` | unary | Snapshot changesets under `refs/snapshots/<user>/<name>` |
| CI | `Affected(base, head, options)` | unary | Changed paths mapped to turbo packages via `pkg_graph` |
| | `PackageGraph(cs)` | unary | |
| Git bridge | `GitMap(ids or oids)` | unary | both directions |
| | `GitBridgePack(have[], want[], blobless)` | server stream | Commits + trees for the projection (§13.2) |
| Admin | `RepoCreate`, `RepoFork`, `RepoDelete`, `RepoSize`, `Health`, `Version` | unary | Forgejo lifecycle |

Protocol rules:

- Every RPC is idempotent or carries a client-generated `op_id` (UUIDv7). Retrying a `Land` with the same `op_id` returns the original result.
- Clients keep up to 8 concurrent `GetObjects` streams with batches of 256 IDs, so cold reads are bandwidth-bound, not latency-bound.
- The server advertises `{min_client_version, cache_tiers[], features[]}` in `Version`, for upgrades and tier discovery (the GVFS `config` idea).
- The daemon serves the same services over IPC. A regional `asc-server --cache` serves read RPCs and forwards mutations.
- `axum` shares the port for `/v8/*` (turbo cache), `/git/*` (git bridge smart HTTP), `/healthz` and `/metrics`.

---

## 8. Server

### 8.1 Process roles

`asc-server` is one binary:

- `serve`: gRPC + HTTP frontend. Stateless apart from caches.
- `worker`: derivation queue, compaction, GC, expiring refs, bootstrap snapshots.
- `--cache <upstream>`: a regional tier, read-through, with no database.

ASC day one: one `serve` plus `worker` process on the Forgejo host, Postgres (Forgejo's own server, separate `asc` schema), Garage as the object store. SQLite is supported for single-node and test deployments.

### 8.2 Server caches

- **Object cache:** RAM (`quick_cache`, weighted by bytes, default 4 GiB) in front of local NVMe (default 50 GiB) in front of the object store. Keys are immutable, so there is no invalidation. ASC's entire live history (311 MiB) stays in RAM.
- **Decoded tree cache:** a separate RAM budget for decoded `Tree` values, because tree walks dominate diff and derivation CPU.
- **Request coalescing:** concurrent misses on the same key share one fetch, which removes thundering herds when many CI jobs start on the same commit.

### 8.3 Land queue

`land_queue(repo, target, op_id, stack, expected_base, state, result, enqueued_at)`. Any frontend accepts a `Land`, inserts it and `NOTIFY`s. The frontend that holds `pg_try_advisory_lock(hash(repo, target))` drains it in batches (§6.4). If that frontend dies, the lock is released with its session and another frontend takes over; requests are idempotent by `op_id`. The submitting frontend waits on `LISTEN` for the result and returns it to the client.

### 8.4 Horizontal scale path (D2)

1. More `serve` frontends behind a load balancer. Each object is cached on the frontend chosen by rendezvous hashing of its ID (frontends forward misses to the owner), so the cluster's cache is the sum of its RAM instead of N copies.
2. Postgres read replicas for immutable rows (graph, path log, object index, derived mappings), with primary fallback on miss. Refs and queues stay on the primary. A row CAS on Postgres sustains thousands of updates per second; D2 needs 5/s per hot ref.
3. More `worker` processes; the queue is already `SKIP LOCKED`.
4. Regional `--cache` tiers near runner fleets, and daemons on every runner host. Per Scion, these absorb more than 99% of reads.
5. Repository sharding (by `repo` across Postgres clusters) only if a single primary saturates on ref and index writes. Nothing in the schema crosses repositories, so this is a routing change.

---

## 9. Daemon (`ascd`)

One daemon per user per machine (Windows: per-user scheduled task, or a service with a per-user pipe; Linux: systemd user unit; CI hosts: a system service shared by the runner user).

### 9.1 Responsibilities

- `RepoApi` tier: answer from cache, forward misses upstream, run the local engine for drafts and offline work.
- Shared cache for every repository, worktree and CI job on the machine.
- VFS providers for all mounts (in-process, so file reads never cross IPC).
- Working-copy state, journals and git projections.
- Turbo `/v8` cache tier on `127.0.0.1`.
- Write-behind uploads and ref watching.

### 9.2 Cache layout

```
<cache-root>/                         default: %LOCALAPPDATA%\asc  |  $XDG_CACHE_HOME/asc; configurable (e.g. D:\asc)
  log/seg-000123.ascl ...              segment log: envelopes, append-only, 256 MiB files
  files/<hh>/<content-id>             decompressed contents (FUSE passthrough, reflink source, ProjFS fast path)
  chunks/<hh>/<chunk-id>              large-file chunks (separate budget)
  meta.redb                           index: id -> (segment, offset, len, last_access_bucket); wc state; local refs; journal
  wc/<wc-id>/overlay/ ...             per-worktree modified files (FUSE, non-virtual)
  wc/<wc-id>/git/ ...                 git projection gitdir (§13.2)
  gitbridge/<repo>/objects/           shared blobless git objects for projections
```

- **Segment log** (Sapling RotateLog idea): new objects are appended to the current segment. When the log exceeds its budget (default 8 GiB), the oldest segment is dropped after objects accessed in the last 7 days are copied forward to the head. Eviction is O(1) per segment, readers need no locks (segments are immutable once sealed), and a torn tail is truncated at startup by checking envelope hashes.
- **File store:** decompressed contents, created on demand when a FUSE file is opened (for passthrough), when a non-virtual checkout needs a reflink source, or when ProjFS hydrates a file over 64 KiB. LRU budget, default 20 GiB. Files are read-only, and never hardlinked into worktrees.
- **Chunks:** separate budget (default 20 GiB) so large files never evict trees and source files.

### 9.3 Fetch and prefetch policy

- **Warm trunk (default for repositories whose trunk content is under 2 GiB compressed; ASC is 311 MiB for all of history):** on each `WatchRefs` event for trunk, fetch `TreeDiff(old, new)` with blobs. All of trunk's content is local, so hydration never waits on the network, `asc log -p` on recent history is local, and offline works.
- **Large repositories:** trees of trunk only; blobs on demand.
- **Sibling read-ahead:** when the VFS hydrates a file, the daemon fetches the other small files (under 64 KiB) in the same directory into the cache, not into the worktree, capped at 2 MB per event. Tools like `tsc` read whole packages; this turns hundreds of misses into one batch.
- **Explicit:** `asc prefetch <pathspec|--package name>` and the CI action's `hydrate` globs batch into one `Prefetch` call.
- Large chunks are never prefetched; they are fetched by range on read.

### 9.4 IPC

`interprocess` local sockets: `\\.\pipe\asc-<user-sid>` with a DACL for the user's SID on Windows; `$XDG_RUNTIME_DIR/asc.sock` with mode 0600 on Unix. The CLI speaks the same gRPC services as the server. The CLI starts the daemon on demand if it is not running. Local-only mode with no daemon (`asc --no-daemon`, used in tests and minimal containers) embeds the engine in the CLI process and supports only non-virtual working copies.

### 9.5 Bootstrap snapshots and cold start

A worker writes, at most every 10 minutes for trunk, a `snap/` object holding the tree closure of trunk (and small blobs for repositories below the warm-trunk threshold) as one segment. `asc clone` and a fresh CI host daemon fetch it with one ranged GET via `Prefetch`, then apply a `TreeDiff` from the snapshot to the current tip. This is Scion's "daemon boots from a periodically refreshed snapshot".

### 9.6 Upgrades

The daemon persists all mount state in redb. On upgrade it stops providers cleanly, restarts, and re-attaches mounts: ProjFS instances keep their virtualization GUID; FUSE mounts are re-established at the same paths (fd takeover is a later optimisation). The CLI checks the daemon version and requests a restart when they differ.

---

## 10. Working copies and the VFS

### 10.1 Working-copy state

```rust
pub struct WorkingCopy {
    pub id: WcId,
    pub repo: RepoRef,
    pub root: Utf8PathBuf,
    pub mode: WcMode,                    // Virtual | Sparse { cones } | Full
    pub head: Head,                      // Branch(RefName) | Detached(ChangesetId)
    pub base_tree: TreeId,
    pub epoch: u64,                      // bumped on switch; placeholder timestamps derive from it
    pub journal_seq: u64,
    pub redirections: Vec<Redirection>,  // node_modules, dist, .turbo, target, .venv, ...
}
```

- **Staging area:** the git projection's index file is asc's staging area (`turbo-ci.md` 2.3). `git add` and `asc add` write the same file. asc rewrites the index only on `switch`, `checkout`, `reset` and `clone`. This keeps one staging area and lets `git add -p` and IDE staging work unchanged.
- **Journal:** `(seq, path, op)` rows in redb, appended by the VFS. `ChangedSince(token)` returns paths after `seq`, or `fresh_instance` if the journal was truncated or the daemon restarted.

### 10.2 Windows: ProjFS

Follow the rules in `rust-stack.md` 5.1 and `prior-art.md` 5:

- One virtualization root per worktree; stable instance GUID; `ProjectedFSLib.dll` loaded at runtime. `asc clone` detects a disabled `Client-ProjFS` and prints the one-time admin command, then offers non-virtual mode.
- Placeholders record `ContentID = (kind, ContentId|TreeId)` and `ProviderID = epoch`. `GetFileData` serves the version recorded in the placeholder, never the current head. Slow fetches return `ERROR_IO_PENDING` and complete with `PrjCompleteCommand`.
- **Stat stability** (critical for turbo): every placeholder created in epoch `e` reports `CreationTime = ChangeTime = LastWriteTime = epoch_time(e)`, a fixed value stored in the WC state, and the size from tree metadata. Hydration does not change these. The git projection writes exactly these values into the index and sets the index timestamp 2 s later. A contract test asserts zero `verify_candidate` reads by turbo (§20.3).
- **Notifications are hints.** `NEW_FILE_CREATED`, `FILE_OVERWRITTEN`, `FILE_HANDLE_CLOSED_FILE_MODIFIED`, `FILE_RENAMED`, `HARDLINK_CREATED`, `FILE_HANDLE_CLOSED_FILE_DELETED` and `FILE_PRE_CONVERT_TO_FULL` append the path (and the rename target) to the journal. `asc status` confirms each journaled path with `PrjGetOnDiskFileState` and hashes only Full or Dirty files. A file whose content again equals the base is dropped from the journal.
- **Startup check:** after a daemon restart, walk only on-disk (non-virtual) entries of each root, which ProjFS keeps small, and compare them with the journal. A cheap form of EdenFS's per-start FSCK.
- **Switch:** `TreeDiff(old, new)`; for each changed path not in the journal, `PrjUpdateFileIfNeeded` with the new placeholder info (or `PrjDeleteFile`); journaled paths follow git's switch rules (refuse, or carry the change). Files held open by another process make the update fail; asc retries three times, then reports the paths and leaves the worktree consistent (old content, marked for re-sync). Then `PrjClearNegativePathCache` and bump the epoch.
- **Dehydration GC:** clean hydrated placeholders not read for 3 days (or above a per-worktree budget, default 500 MiB) are reverted to placeholders with `PrjDeleteFile` + re-projection, during idle time. This bounds the NTFS copy that ProjFS makes of every read file.
- Notifications are suppressed under redirected directories, which are not part of the virtual root.
- **Dev Drive:** if the worktree is on ReFS with `PrjFlt` allowed, hydration of files over 64 KiB block-clones from the file store (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`) instead of copying. To verify in the ProjFS spike.

### 10.3 Linux: FUSE

- `fuser` with passthrough (kernel 6.9+): opening a clean file for read returns a backing file from `files/`, so reads and mmap go straight to the kernel with no copy and no extra disk per worktree.
- Open-for-write copies up into `wc/<id>/overlay/`. Every write goes through the daemon, so the dirty set is exact and the journal is authoritative (no reconciliation needed).
- `path <-> inode` table persisted in redb, so inode numbers are stable across daemon restarts. That matters because the git index compares inodes on Unix.
- Attributes are cached by the kernel with long TTLs and explicitly invalidated on switch (`notify_inval_entry` / `notify_inval_inode`), so `stat` storms (turbo, bun, tsc) do not reach user space.
- `readdir` returns directory listings from tree metadata in one reply (`readdirplus`).
- CI containers: the daemon runs on the host and the mount is bind-mounted into the job (§14.2), so jobs need no FUSE privileges.

### 10.4 macOS

Phase 6. Non-virtual mode with APFS `clonefile` first (always works, near-free worktrees). Then NFSv3 loopback (`nfs3_server`), the EdenFS approach, with FSKit evaluated later. No kernel extensions.

### 10.5 Non-virtual mode (fallback on every OS)

- `asc clone --no-virtual` or `asc sparse set <cones>` materialises files in the cones (cone-only patterns, which match turbo packages) by reflink from `files/` where the filesystem supports it (ReFS Dev Drive, Btrfs, XFS, APFS), otherwise by copy. Never by hardlink.
- Status uses a stat cache in redb plus a `notify` watcher while the daemon runs. Without the daemon, status stats the materialised cones only.
- The git projection works the same way, using the real stat data of materialised files.

### 10.6 Redirections

Directories that are written heavily or crawled (`node_modules`, `dist`, `.next`, `.turbo`, `target`, `.venv`, `__pycache__`) are **redirections**: plain directories on local disk, outside the virtual root, linked in (Windows: a directory junction to `<cache-root>/redir/<wc-id>/<path>`; Linux: a bind mount inside the FUSE root). The list comes from a checked-in `.asc/redirections` file, defaulting to ignored build-output globs. Redirected paths are never journaled or virtualised. Bun's global cache stays on the same drive as the redirections, so its hardlinks keep working (`asc-profile.md` 8.1).

Phase 6: a **node_modules snapshot** redirection, keyed by the hash of `bun.lock` + all workspace `package.json` + `patches/**`. The daemon stores an installed tree once per key and mounts it read-only (FUSE) or reflink-clones it (Dev Drive) into each worktree. Identical lockfiles across 25 agent worktrees then share one install.

### 10.7 Worktree cost

`asc worktree add <path> [<rev>]`: insert a WC row, create the virtualization root or mount, write the git projection index from cached trees (about 0.5 MB for ASC), create redirection targets. Nothing else. 200 worktrees per machine cost about 200 x (index + metadata) plus what each one modifies (and, on Windows, what it reads until dehydrated). Today the 49 linked worktrees report 157.7 GiB, of which about 79 GiB is source-control data that asc removes by design.

---

## 11. CLI command map

Columns: **Runs** = D (daemon/local), S (server RPC), D+S (local work then a server call). **Offline** = behaviour without the server. Flags and output formats follow git, including `--porcelain=v1|v2`, `-z`, `--format`, revision syntax (`HEAD~2`, `A..B`, `A...B`, `@{u}`, `:/msg`, `<rev>:<path>`), `-C`, `-c key=value`, `alias.*` and the pager.

| git command | asc behaviour | Runs | Offline |
|---|---|---|---|
| `init` | Local-only repo (`--store <dir|s3://...>` to choose the object store) | D | full |
| `clone <url>` | Virtual clone: fetch bootstrap snapshot, mount, write projection. `--no-virtual`, `--sparse`, `--depth` accepted (`--depth` is a no-op: history is never copied) | D+S | no |
| `status` | Journal-based; untracked files only from journaled directories | D | full |
| `add`, `rm`, `mv` | Update the projection index; `mv` records `copy_from` | D | full |
| `restore`, `checkout -- <path>` | Re-project placeholders from the source tree | D | needs cached content |
| `reset [--soft|--mixed|--hard]` | Move head; rewrite index; re-project changed paths | D | full for cached revs |
| `commit [-a] [--amend] [-m]` | Local draft changeset; write-behind upload | D | full (upload queued) |
| `switch`, `checkout <branch>` | `TreeDiff` + placeholder update; fetches the ref on use | D+S | cached revs only |
| `branch` | Local branch = remote ref with tracking; `-d/-D` local; `--remote` deletion via `push --delete` | D (+S for `-r`) | local refs |
| `tag` | Ref under `refs/tags/`; annotated tags are changeset-like tag objects | D+S | local |
| `log`, `shortlog`, `rev-list` | Server for ranges and paths (`path_changes`); local for drafts | S (D for drafts) | partial (cached graph) |
| `show`, `diff <rev>...` | Committed ranges: server `Diff` (cached line diffs); working copy: local | S / D | partial |
| `diff`, `diff --cached` | Working copy vs index vs head, local | D | full |
| `blame` | Server derived blame | S | local slow path on cached history |
| `grep` | Working tree: local over hydrated + cached content; `<rev>`: server | D / S | partial |
| `merge-base`, `rev-parse`, `describe` | Server graph (local for drafts and cached) | S | partial |
| `fetch` | Ref update only (`ListRefs`); objects stay lazy. The daemon already watches refs, so this is usually a no-op | S | no |
| `pull [--rebase]` | Fetch + local metadata rebase of drafts or merge | D+S | no |
| `push` | Trunk with `push.land = true`: `Land`. Other refs: `UpdateRefs` CAS. `--force-with-lease` is the native CAS; `--force` = `Expect::Any` | S | queued (`asc push --queue`) |
| `merge` | Local engine (path-set merge); conflicts as markers + `Merge<T>` in a snapshot | D | full for cached content |
| `rebase [-i] [--onto]` | Local metadata rebase; content merge only for overlapping paths. `-i` with a git-style todo list | D | full for cached content |
| `cherry-pick`, `revert` | Local engine | D | as above |
| `stash [push|pop|list|apply|drop]` | Snapshot changesets under `refs/stash` (local; synced with `--sync`) | D | full |
| `worktree add|list|remove|prune|move` | §10.7. `remove` unmounts and deletes state | D | full |
| `sparse-checkout` | Cone patterns; in virtual mode controls prefetch and visibility | D | full |
| `submodule` | Nested mounts; `update` is a no-op; `status` reports nested heads | D | cached |
| `remote` | Remotes are asc server URLs (`asc://`, `https://`), plus `git+https://` for a git bridge remote during migration | D | full |
| `config` | Layered config; reads `~/.gitconfig` for `user.*`, `alias.*`, `core.editor`, `diff.*`, `merge.*` | D | full |
| `clean` | Untracked files from the journal only | D | full |
| `reflog` | Local op log (every local mutation, undoable) + server `ref_log` for remote refs | D/S | local |
| `bisect` | Client-driven; `--server-run` hands the range to CI via API | D | cached |
| `archive` | Server `Archive` stream | S | no |
| `cat-file`, `ls-tree`, `ls-files` | From metadata; `ls-tree --git-oid` prints bridge oids | D | cached |
| `format-patch`, `am`, `apply` | Local; `apply --server` uses `ApplyPatch` | D | full |
| `gc`, `maintenance`, `repack`, `prune` | No-ops with a message (no local history to maintain); `asc gc` trims the cache | D | full |
| `fsck` | Verify cache envelopes and working-copy consistency | D | full |
| `notes`, `filter-branch`, `replace` | Not supported; explicit error naming the alternative | – | – |
| **asc extras** | `land`, `affected`, `prefetch`, `sparse`, `workspace push|pull|list`, `undo`, `absorb` (Phase 6), `daemon start|stop|status`, `doctor`, `githelp`, `rev-parse --git`, `debug hydration|trace` | | |

---

## 12. Offline and local-only behaviour

- **Local-only repositories** (`asc init`, or `asc clone --local-only` from a bucket) run the whole engine in the daemon: commits, refs (redb), history queries and derived data. The store can be a local directory or a bucket in serverless mode. Nothing needs a server.
- **Online clones going offline:** the daemon keeps working from its cache. With warm trunk (ASC default) every trunk file and all of trunk's recent history are local, so reads, commits, branches, rebases and merges work offline. Reads of uncached content fail fast: ProjFS returns `STATUS_FILE_IS_OFFLINE` (shown to tools as an I/O error) and FUSE returns `EIO`, with a daemon-side message naming `asc prefetch`. History queries answer from the cached graph and mark results as partial.
- **Queued writes:** commits upload when the connection returns. `asc push --queue` records the intended `Land`/`UpdateRefs` and submits it on reconnect, then reports the result in `asc status`.
- `asc prefetch --offline-kit <cones>` pins content for travel.

---

## 13. Turborepo integration

### 13.1 What turbo needs

From `turbo-ci.md`: per-file git blob SHA-1s and stat data (via `.git/index`), changed files between base and head with a merge base, untracked and modified files, and `git show <base>:bun.lock` when the lockfile changed. None of it needs the bytes of an unmodified file.

### 13.2 The git projection

Each working copy has `W/.git` as a gitfile pointing to `<cache-root>/wc/<id>/git`, which contains:

- `index` written by asc: path, mode, `git_oid`, size and the exact stat values the VFS reports (§10.2, §10.3); TREE extension from `gitmap`; index timestamp 2 s after the newest entry; no sparse or skip-worktree flags.
- `HEAD`, branch refs and, in CI, `refs/remotes/origin/<base>`, all as bridge SHA-1s.
- `objects/info/alternates` pointing to the machine-wide blobless `gitbridge/<repo>/objects`, filled by `GitBridgePack` with commits and trees only. Shared by every worktree on the machine; ASC's whole commit and tree history is a few tens of MB.
- `config`: `extensions.partialClone = asc`, a promisor remote `asc::<repo>` served by `git-remote-asc` (single-object fetch through the daemon by `git_oid -> ContentId`), `core.fsmonitor` pointing at an asc hook that returns `ChangedSince` paths, and `core.hooksPath` hooks that refuse `git commit` and `git push` with "use `asc commit` / `asc push`".
- CI: `shallow` = the merge base, and bridge objects for `merge_base..head` only.

Result: upstream `git` and upstream `turbo` run unmodified. Turbo's index path sees every clean file as clean and reads nothing. `git show base:bun.lock` fetches one blob through the promisor. Biome, `git ls-files` in `check-schema-instances.ts`, and IDEs work too.

### 13.3 Affected detection

- Inside a job, `turbo run <tasks> --affected` with `TURBO_SCM_BASE` set by `asc-checkout` to the merge base, so turbo never falls back to "everything changed".
- Before any job, `asc affected --base B --head H --format=gha` calls `Affected`: `ChangedPaths(merge_base, head)` mapped onto `pkg_graph` (computed once per manifest-tree by sandboxed `turbo ls --output=json` on a virtual checkout, so turbo stays the oracle). It applies turbo's rules: `turbo.json` changes mark everything; lockfile changes run turbo's own lockfile diff in the same sandbox; dependents are added. It outputs package lists and job switches (`any`, `rust`, `images`, `e2e`). The plan job only needs to be conservative; turbo inside the job is exact.

### 13.4 Remote cache

`asc-turbo-cache` implements `/v8/artifacts/status`, `HEAD|GET|PUT /v8/artifacts/{hash}`, `POST /v8/artifacts` and `POST /v8/artifacts/events` on the same `ContentStore`, in the server and in the daemon.

- Scopes: `trusted` (protected-branch jobs) and `pr-<n>`. PR jobs read `pr-<n>` then `trusted`, and write only `pr-<n>`. Signature verification (`remoteCache.signature: true`) is enabled for trusted writers.
- Tiering: the daemon answers `GET` from its cache, then upstream; on `PUT` it stores locally, replies 202 at once, and uploads in the background (artifacts are immutable and keyed by hash). Developers get one turbo cache per machine across all worktrees instead of one `.turbo` per worktree.
- Preflight redirects to presigned URLs for artifacts over 8 MiB when the server is remote from the runner (optional).
- Retention: LRU by last access in `.meta`; 14 days for `pr-*`, 60 days for `trusted`; per-repo size cap.
- Works in local-only mode too (daemon-only), which is useful for solo developers.

ASC prerequisite (no asc needed): fix `"env": ["*"]` on `build` in `turbo.json`, which makes keys differ per machine and destroys cache hits.

---

## 14. CI with Forgejo Actions

### 14.1 Flow

1. **Plan job, no checkout:** `asc/asc-affected@v1` calls `Affected`. Outputs switch jobs on and off. `deploy-affected` becomes this job plus one blob read of `deploy-targets.json`.
2. **Work jobs:** `asc/asc-checkout@v1` with `mode: virtual` (default on hosts with a daemon), `sparse` (no daemon or no mount sharing), or `full`.
3. Turbo runs with `--affected` against the projection and the daemon's `/v8` tier.

### 14.2 Runner topology

- `ascd` runs on each runner host as a system service, with one shared cache for all jobs. Jobs see their working copy through a bind mount with `rshared` propagation (`container.valid_volumes` / `options` in the forgejo-runner config; to be confirmed with the runner operator).
- `asc-checkout` steps: authenticate with the Actions task token (§18); ask the host daemon to create a WC at the changeset; `MergeBase(base, head)`; write the projection with `shallow` and base refs; one batched `Prefetch` of `**/package.json`, `bun.lock`, `turbo.json` and the action's `hydrate` globs; export `TURBO_SCM_BASE`, `TURBO_SCM_HEAD`, `TURBO_API=http://127.0.0.1:<port>`, `TURBO_TOKEN`, `TURBO_TEAM`, `TURBO_CACHE`.
- Job end: the WC is removed (metadata only). The host cache keeps everything for the next job.
- Sparse fallback: materialise the plan job's affected packages and their dependencies by reflink or copy from the daemon (or from an in-container daemon), with the same projection.

### 14.3 Throughput

| Item | Today (estimate) | With asc | Driver |
|---|---|---|---|
| Checkouts per PR | about 9 x 15-45 s + server pack generation | 3-5 x 1-2 s | virtual WC, no history, host cache |
| `deploy-affected` | full history fetch, 2-5 min | about 5 s | server `Affected`, no checkout |
| Builds | 3-4 cold overlapping builds | affected only, mostly cache hits | `--affected` + tiered `/v8` |
| Typecheck/test | 25 fixed steps | affected packages, cached | turbo task graph |
| Rust jobs | every PR | only when Rust is affected | plan job |
| Typical PR | about 25-45 runner-min | about 4-8 runner-min | |

Agent scale: with 1,000 agent PRs a day, server load per PR is one `Affected` call, a few `Prefetch` calls served from the bootstrap snapshot, and cache-tier traffic that runner daemons absorb. Forgejo no longer generates packs for checkouts at all.

Do-now items that need no asc (from `turbo-ci.md`): host bun cache, merge `ci-intelligence.yml` into `ci.yml`, `check-types` aliases, cargo turbo tasks, delete the comment-only `.gitattributes`, any `/v8` server on Garage.

---

## 15. Forgejo fork

Base: tag `v16.0.6` (ASC's running version), kept as a patch stack with one commit per seam. New code in new packages. Details and file lists are in `forgejo.md`.

### 15.1 Seam A: read backend

`git.Repository` keeps its concrete type and API; a private `backend Backend` field dispatches about 30 methods. Today's code moves into `modules/git/gitcli`; `modules/git/ascclient` implements the interface over gRPC stubs generated from `proto/` into `modules/ascproto`. The backend is chosen in `modules/gitrepo.OpenRepository` from a per-repo `storage_backend` column (`git` | `asc`). `ObjectFormatName = "asc"` with 64-hex IDs.

Performance requirements for `ascclient`:

- An LRU per process keyed by immutable IDs (commits, trees, blobs under 1 MiB, line diffs). No invalidation is needed. Ref lookups are not cached beyond one request.
- **Batch methods for page renders:** the repo home page and tree view call `LastModified(cs, dir)` once instead of one `log -1 -- path` per entry; commit lists use `Log` with headers; PR pages use `ChangedPaths` + `Diff` (cached `line_diff`). Target: at most 5 RPCs per page render.
- `UnifiedDiff` output is byte-identical to git's so `services/gitdiff` parses it unchanged.
- Connection: one HTTP/2 channel per Forgejo process with keepalive; deadlines on every call.

### 15.2 Seam B: write operations

`modules/repostore.Writer`: `CreateCommit(file ops)`, `Merge(style, dry_run)`, `CherryPick`, `Revert`, `ApplyPatch`, `UpdateRefs`, `Init`, `Fork`, `Delete`, `Size`. Under asc each is one RPC with no temporary clone. This replaces the "temp clone + git push" pattern for the web editor, uploads, the contents API, every PR merge style, update-branch, the wiki (a second asc repo per repository), templates and sync-fork. PR mergeability checks become `Merge(dry_run)`: a path-set intersection, usually with no content reads.

### 15.3 Seam C: transport and auth

`/api/internal/asc/authz/{owner}/{repo}` returns the caller's permission for a token. `asc-authenticate` over SSH (modelled on `git-lfs-authenticate` in `cmd/serv.go`) issues a short-lived JWT. The asc server caches authz answers for 30 s and drops them on a Forgejo permission-change notification.

### 15.4 Hooks

`UpdateRefs` and `Land` call `/api/internal/hook/pre-receive` before the CAS (branch protection, protected files, signed-commit rules, quota) and `post-receive` after it (webhooks, Actions triggers, branch table, PR re-tests). Lands are batched into one `post-receive` call. No quarantine is needed: objects are uploaded before the ref changes and are harmless if the ref never moves.

### 15.5 Actions

Add `asc` fields to the Actions context (changeset ID, merge-base), run workflow `paths:` filters through `ChangedPaths` (already funnelled through `commit.GetFilesChangedSinceCommit`), and ship `asc-checkout` and `asc-affected` actions.

### 15.6 Per-repo flags

`storage_backend` (`git`|`asc`), `git_bridge` (`off`|`lazy`|`eager`; any mirror forces at least `lazy`), `lfs_compat`, `server_side_merge_only`.

---

## 16. Git bridge

- `gitmap` derived data gives deterministic git commits and trees; imported commits keep their original bytes and signatures in `extra`, so export reproduces the original SHA-1s.
- **Read path:** a derived bare repository per asc repo, filled incrementally by a `git fast-import` stream from `gitmap` (a deletable cache). Stock `git upload-pack` serves it, so `githttp.go`, `cmd/serv.go`, mirrors and migrations keep working by pointing at `bridgePath(repo)`. Large files flagged `lfs_on_export` are written as LFS pointers and served by Forgejo's LFS endpoint from asc chunks.
- **Write path (legacy `git push`):** receive into a temporary bare repo, import new commits with `asc-git` into changesets, then `UpdateRefs` (which runs the hooks). Under `server_side_merge_only`, pushes to protected branches are refused with a message pointing at `asc land`.
- `eager` keeps the bridge in sync on every land (for ASC during migration and for the git projection); `lazy` builds on first git access; `off` disables it.

---

## 17. Migration and import

`asc import git <path-or-url> --repo <name>` (crate `asc-git`):

1. Open with `gix`. Walk all refs in topological order. Decode objects in parallel (`rayon`, one pack reader per thread).
2. For each commit, compute `changes` versus the first parent with gix tree diff. For each new blob: BLAKE3, `git_oid` (already known from git), zstd, chunk if large.
3. Resolve LFS pointers through Forgejo's LFS batch API (or the local `.git/lfs/objects`) into native content, and flag `lfs_on_export`.
4. Submodule gitlinks become `Submodule` entries pointing at the imported nested repository's changeset (imported first), or, with `--subtree packages/soma/sdk=<repo>`, the nested history is grafted under that prefix.
5. Upload in batches of about 8 MiB per `PutObjects` call (one segment each), 8 calls in flight. Write graph, path log and `gitmap` rows in bulk (`COPY`).
6. Set refs last, with `extra["git-commit"]` and signatures preserved so the bridge reproduces original SHA-1s. Verify: for every ref, the bridge export's commit SHA-1 must equal the git SHA-1.

ASC estimate: 104,800 reachable objects, 6.99 GiB raw (311 MiB compressed in git), plus LFS history. Hashing and compressing 7 GiB at about 500 MB/s across cores takes well under a minute; the bound is LFS download and upload, which should finish within 30 minutes on the LAN. The 4.6 GB cruft pack and the unreachable objects are not imported.

Cut-over for asc/dds: import with `git_bridge = eager`; run CI against both for a week; switch `storage_backend` to `asc`; developers `asc clone` and delete their git worktrees (the 58 GB `.git` goes away); after the stability window, set the bridge to `lazy`.

During migration, `asc clone git+https://forge.../dds.git` (Phase 2) runs import locally against the existing remote and pushes back through the bridge. This lets developers get virtual worktrees before the Forgejo fork ships.

---

## 18. Security

- **Authentication:** Forgejo PATs and OAuth tokens, SSH-issued JWTs (`asc-authenticate`), and Actions task tokens, all validated through Forgejo's internal authz endpoint. TLS (`rustls`) for every remote connection.
- **Authorisation:** per-repository read or write on every RPC; per-namespace object access (§4.5); protected refs and file rules enforced by Forgejo's `pre-receive`.
- **Integrity:** content addressing plus verify-on-read at the consumer; segment footers are checked on load; the daemon truncates torn segments at startup.
- **Signing:** changesets may carry `sshsig` over their canonical bytes in `extra`; imported git signatures are preserved for the bridge. Forgejo's signed-commit rules verify either.
- **Daemon:** IPC limited to the user's SID or socket mode 0600; ProjFS `PRE_*` notifications block writes inside the projection's gitdir; in CI, the WC can be mounted read-only except for redirections.
- **Turbo cache:** PR scopes, signatures for trusted writers, tokens scoped to one repository.
- **Sandboxing:** `pkg_graph` runs `turbo ls` in a container with no network, a read-only virtual checkout, a CPU and time limit, and only manifest files hydrated. It never runs package scripts.
- **Licences:** no code is copied from Sapling, Mononoke or EdenFS (GPL-2.0). jj's `Merge<T>` and the `gix-merge` text driver are Apache-2.0/MIT, ported with attribution. asc is MIT OR Apache-2.0; the Forgejo fork stays GPLv3+ and talks to asc over RPC.

---

## 19. Observability

- **Tracing:** `tracing` + OpenTelemetry in every Rust process; trace context propagated over gRPC metadata from CLI to daemon to server to Forgejo, so one `asc push` is one trace. Each response carries `served-by: daemon|regional|server|store` for tier attribution.
- **Metrics** (Prometheus at `/metrics`, daemon too):
  - per RPC: rate, latency histogram, bytes, errors;
  - per tier: hit ratio for objects, trees and turbo artifacts;
  - VFS: hydrations per second, hydration bytes, callback latency, journal length, dehydrated bytes;
  - land: queue depth, batch size, latency, conflicts, retries;
  - derivation: lag per type (time from changeset creation to derived), queue depth;
  - storage: segment count and size, S3 request rate and latency, GC reclaimed bytes;
  - disk: per-worktree overlay and hydrated bytes, cache usage per budget.
- **SLOs** for the Phase 5 cut-over: land p99 < 2 s, clone p95 < 5 s, derivation lag p99 < 10 s for eager types, daemon cache hit ratio > 95% on CI hosts.
- **Developer tools:** `asc doctor` (ProjFS state, daemon health, cache budgets, clock skew, redirections, stat-stability self-test), `asc debug hydration --since 10m` (which process hydrated which file; ProjFS supplies the PID), `asc debug trace -- <command>`.
- Turbo `/v8/artifacts/events` are stored as metrics for cache hit rates per task.

---

## 20. Testing and benchmarking

### 20.1 Layers

- **Unit and property tests** per crate: encoding round-trips and byte stability (proptest); tree derivation equals a naive full rebuild; tree diff equals naive diff; merge-base equals BFS on random DAGs; rebase metadata shortcut equals full merge when paths are disjoint.
- **RepoApi conformance suite** (`asc-testkit`): one test suite run against the embedded engine, the engine over gRPC, the daemon over IPC, and a cache tier in front of a server. This is how "same API at every tier" is enforced.
- **Storage matrix:** `ContentStore` and `RefStore` suites against InMemory, LocalFileSystem, MinIO (testcontainers), and Garage; the CAS probe against each; Postgres and SQLite for `sql` refs.
- **Fault injection:** `Chaos` and `Delay` decorators (dropped writes, partial segments, 409/412 responses, slow reads), crash-restart of the daemon during switch and hydration, land leader failover.
- **Land linearizability:** many concurrent `Land` calls with random path sets against an in-memory store; check that the resulting history is a valid serial order and every accepted stack's paths were conflict-free.

### 20.2 Git parity

Golden tests against real `git` 2.55 on fixture repositories and on monorepo: `diff` (unified, stat, numstat, name-status, `-z`), `status --porcelain=v1|v2 -z`, `log --format`, `ls-files`, `ls-tree`, `rev-parse` revision syntax, merge conflict markers (`merge`, `diff3`, `zdiff3`), and bridge export SHA-1 equality after import.

### 20.3 Turbo and VFS contract tests

On Windows (ProjFS), Linux (FUSE) and the non-virtual mode, with a hydration counter in the provider, for every turbo minor release:

1. `turbo run build --dry=json` on a clean WC: zero hydrations except manifests; task hashes equal those from a real git clone.
2. Modify one file: only dependent task hashes change; hydrations +1.
3. `turbo ls --affected` with `TURBO_SCM_BASE`: equals a full clone's result; only `bun.lock` at base is fetched, and only if changed.
4. Run within 1 s of checkout: turbo logs show `verified entries = 0`.
5. `git status`, `git ls-files`, `git diff HEAD`, `git log -5` work with no hydration of clean files.

### 20.4 Benchmarks (`asc-bench`)

Scenarios replay real workloads and gate CI with regression thresholds (fail on > 10% p50 regression):

- **asc-import:** import monorepo; measure time, bytes, objects.
- **asc-replay:** replay the last 2,000 ASC commits as lands from 20 simulated agents; measure land latency and throughput.
- **agent-swarm:** 200 worktrees on one machine, each switching, editing 4 files, committing and landing; measure disk per worktree and daemon memory.
- **d1-synthetic:** generated 100k-file, 1M-changeset repository with ASC's file-size and change-size distributions; measure every row of §1.3.
- **ci-sim:** 50 concurrent CI jobs on one runner host doing clone + prefetch + turbo dry run.

Benchmarks run on Linux in CI and on a Windows machine (nightly) for ProjFS.

---

## 21. Phased roadmap

Each phase ends with a measurable gate from §1.3.

| Phase | Content | Gate |
|---|---|---|
| 0 (now, no asc) | ASC CI hygiene: host bun cache, merge duplicate workflow, `check-types` aliases, cargo turbo tasks, delete `.gitattributes`, fix `"env": ["*"]`, stand-in `/v8` cache on Garage. Optional git cleanup (about 57 GB). Spikes: ProjFS, FUSE passthrough, S3 CAS probe, gix import throughput, ProjFS stat stability | Spike reports; CI wall time measured before/after |
| 1: core, local-only | `asc-types`, `asc-store`, `asc-refs`, `asc-index`, `asc-graph`, `asc-derive`, `asc-diff`, `asc-merge`, `asc-engine`, `asc-config`, `asc-git` import; `asc` CLI with `--no-daemon` non-virtual WCs | Import monorepo; git parity suite green; merge-base/log/diff targets on D1 synthetic |
| 2: daemon and VFS, still local-first | `asc-cache`, `asc-ipc`, `asc-wc`, `asc-vfs`, `asc-vfs-fuse`, `asc-vfs-projfs`, `asc-gitproj`, `ascd`; `asc clone git+https://...` with bridge push-back | Worktree add < 300 ms; per-worktree disk targets; turbo contract tests 1-5 green on Windows and Linux. Jordan's machine runs agent worktrees on asc |
| 3: server and tiers | `asc-proto`, `asc-rpc`, `asc-server` (SQLite then Postgres), land queue, derivation workers, bootstrap snapshots, `asc-turbo-cache` | RepoApi conformance over all tiers; land p99 < 2 s at 5/s; asc-replay passes |
| 4: Forgejo read side and CI | Fork phases 0-2 and 5 (seams A, ascclient, `storage_backend`), git bridge `eager`, `asc-ci`, `asc-affected` and `asc-checkout` actions (sparse mode first, then virtual on hosts with a daemon) | Forgejo integration suite green on both backends (read paths); typical PR < 8 runner-min |
| 5: Forgejo write side, cut-over | Fork phases 3, 4, 6-8 (seam B, raw `NewCommand` removal, auth, hooks, lifecycle); import asc/dds; switch to asc; bridge to `lazy` after stability window | Full integration suite green on asc backend; SLOs in §19 met for 2 weeks |
| 6: optimisation and reach | Segment compaction + dictionaries, Bao range verification, node_modules snapshots, workspace sync, `absorb`/`undo` polish, macOS (clonefile, then NFS), regional cache, horizontal scale-out tests at D2 | agent-swarm and d1-synthetic at D2; macOS parity suite |

---

## 22. Implementation units

Each unit is one crate, binary, Go package or action, with owned files, a single acceptance command and its dependencies. Units are ordered so that each can be verified before the next starts. Paths are relative to `D:\dev\source-control` unless they start with `forgejo/` (the fork at `D:\dev\forgejo`). Every Rust acceptance command assumes the workspace root as the working directory.

| # | Unit | Owned files | Acceptance command | Depends on |
|---|---|---|---|---|
| U00 | Workspace skeleton | `Cargo.toml`, `rust-toolchain.toml`, `deny.toml`, `.cargo/config.toml`, `crates/` and `bins/` dirs | `cargo check --workspace` | – |
| U01 | `asc-types`: IDs, `RepoPath`, `Changeset`, `Tree`, `ChunkList`, `Merge<T>` value type, envelope, postcard encoding, BLAKE3/SHA-1 helpers | `crates/asc-types/**` | `cargo test -p asc-types` | U00 |
| U02 | `asc-config`: layered config, git config reading, aliases | `crates/asc-config/**` | `cargo test -p asc-config` | U00 |
| U03 | `asc-store`: `ContentStore`, local/S3/memory backends, decorators, codec (zstd, dicts), FastCDC chunking, segment writer/reader | `crates/asc-store/**` | `cargo test -p asc-store` (MinIO tests behind `--features it-s3`) | U01 |
| U04 | `asc-refs`: `RefStore`, redb, sql (sqlite/postgres), S3-CAS, capability probe, ref log, expiry | `crates/asc-refs/**` | `cargo test -p asc-refs` | U01 |
| U05 | `asc-index`: `IndexStore` trait (object index, graph rows, path log, derived mappings, queues) with redb and sqlx impls, plus migrations | `crates/asc-index/**`, `crates/asc-index/migrations/**` | `cargo test -p asc-index` | U01 |
| U06 | `asc-graph`: generation numbers, skip pointers, ancestry, merge-base, path-log queries | `crates/asc-graph/**` | `cargo test -p asc-graph` | U05 |
| U07 | `asc-derive`: `Derivable`, scheduler, warm-ref gate, eager types (`fsnode`, `graph`, `path_changes`, `changeset_info`, `case_check`) | `crates/asc-derive/**` | `cargo test -p asc-derive` | U03, U04, U06 |
| U08 | `asc-diff`: tree diff with pruning, range path diff, rename detection, `imara-diff` line diff, git-compatible printers | `crates/asc-diff/**`, `tests/fixtures/diff/**` | `cargo test -p asc-diff` | U07 |
| U09 | `asc-merge`: `Merge<T>` algebra, ported text merge, path-set tree merge, metadata rebase, pushrebase planner | `crates/asc-merge/**`, `tests/fixtures/merge/**` | `cargo test -p asc-merge` | U08 |
| U10 | `asc-engine`: `RepoApi` trait + `LocalEngine` (create changeset, refs, log, diff, blame, merge, rebase, land, affected stubs) | `crates/asc-engine/**` | `cargo test -p asc-engine` | U09 |
| U11 | `asc-testkit`: RepoApi conformance suite, fixtures, MinIO/Postgres containers, git parity harness | `crates/asc-testkit/**` | `cargo test -p asc-testkit` (runs the suite against `LocalEngine`) | U10 |
| U12 | `asc-git`: gix importer (history, LFS, submodule/subtree), `gitmap` derivation, fast-import exporter | `crates/asc-git/**` | `cargo test -p asc-git` | U10, U11 |
| U13 | `asc-wc`: WC state, projection-index staging, journal, status, switch planner, non-virtual materialiser, stat cache, ignore rules, redirections | `crates/asc-wc/**` | `cargo test -p asc-wc` | U10, U02 |
| U14 | `asc-gitproj`: gitfile/gitdir writer, index writer (stat-exact, TREE ext), alternates store, hooks, `git-remote-asc` helper binary | `crates/asc-gitproj/**`, `bins/git-remote-asc/**` | `cargo test -p asc-gitproj` (calls the real `git` binary) | U12, U13 |
| U15 | `asc` CLI, no-daemon mode: clap command tree with git spellings, output formats, pager, local-only engine | `bins/asc/**` | `cargo test -p asc` (assert_cmd tests incl. git parity goldens) | U13, U14 |
| U16 | `asc-cache`: segment log, file store, chunk store, budgets/eviction, tiered `RepoApi` (cache then upstream), prefetch policies | `crates/asc-cache/**` | `cargo test -p asc-cache` | U10, U11 |
| U17 | `asc-ipc`: local socket/pipe transport for tonic, permissions | `crates/asc-ipc/**` | `cargo test -p asc-ipc` | U00 |
| U18 | `asc-proto`: `.proto` files + generated Rust; conversions to `asc-types` | `proto/asc/v1/*.proto`, `crates/asc-proto/**` | `cargo test -p asc-proto` | U01 |
| U19 | `asc-rpc`: tonic services over `RepoApi`; `RemoteEngine` client implementing `RepoApi`; resumable object streams | `crates/asc-rpc/**` | `cargo test -p asc-rpc` (conformance suite over loopback gRPC) | U16, U17, U18 |
| U20 | `asc-vfs`: OS-neutral VFS core (inode/path table, placeholder versions, overlay, read-through, journal feed, dehydration policy) | `crates/asc-vfs/**` | `cargo test -p asc-vfs` | U13, U16 |
| U21 | `asc-vfs-fuse`: fuser provider with passthrough and invalidation | `crates/asc-vfs-fuse/**` | `cargo test -p asc-vfs-fuse` (Linux/WSL; skipped elsewhere) | U20 |
| U22 | `asc-vfs-projfs`: ProjFS provider with dynamic load, async completion, notifications, switch, dehydration | `crates/asc-vfs-projfs/**` | `cargo test -p asc-vfs-projfs` (Windows with Client-ProjFS enabled) | U20 |
| U23 | `ascd`: daemon binary (IPC server, cache, mounts, write-behind, WatchRefs, `/v8` tier) | `bins/ascd/**` | `cargo test -p ascd` | U19, U21, U22, U14 |
| U24 | `asc` CLI, daemon mode: route commands through `ascd`; `clone`, `worktree`, `prefetch`, `doctor`, `debug` | `bins/asc/src/daemon/**` | `cargo test -p asc --features daemon-it` | U15, U23 |
| U25 | Turbo/VFS contract tests | `tests/turbo-contract/**` (crate `asc-e2e`) | `cargo test -p asc-e2e --test turbo_contract` | U24 |
| U26 | `asc-turbo-cache`: `/v8` API over `ContentStore`, scopes, signatures, retention | `crates/asc-turbo-cache/**` | `cargo test -p asc-turbo-cache` | U03 |
| U27 | `asc-server`: serve/worker/cache roles, land queue, derivation workers, bootstrap snapshots, GC, authz client, hook client, axum routes | `bins/asc-server/**` | `cargo test -p asc-server` (Postgres + MinIO containers) | U19, U26, U12 |
| U28 | `asc-ci`: `Affected`, `pkg_graph` derivation (sandboxed turbo), GHA output formats | `crates/asc-ci/**` | `cargo test -p asc-ci` | U27 |
| U29 | `asc-bench`: scenarios from §20.4 with thresholds | `crates/asc-bench/**` | `cargo run -p asc-bench --release -- check` | U27, U24 |
| G01 | Fork CI and patch-stack tooling | `forgejo/.forgejo/workflows/asc-*.yml`, `forgejo/contrib/asc/**` | `make -C D:/dev/forgejo lint-go` | – |
| G02 | Hide repo paths (fork phase 1) | `forgejo/modules/gitrepo/**`, call sites listed in `forgejo.md` | `go test ./modules/gitrepo/... ./services/repository/...` | G01 |
| G03 | `modules/git` `Backend` interface + `gitcli` move (seam A) | `forgejo/modules/git/backend.go`, `forgejo/modules/git/gitcli/**` | `go test ./modules/git/...` | G02 |
| G04 | `modules/ascproto` generated stubs + `modules/git/ascclient` with immutable-ID LRU and batch calls | `forgejo/modules/ascproto/**`, `forgejo/modules/git/ascclient/**` | `go test ./modules/git/ascclient/...` (against an `asc-server` test binary) | G03, U27 |
| G05 | `storage_backend` column, migration, backend selection | `forgejo/models/repo/**`, `forgejo/models/migrations/**`, `forgejo/modules/gitrepo/gitrepo.go` | `go test ./models/repo/... ./modules/gitrepo/...` | G04 |
| G06 | `modules/repostore.Writer` (seam B) with gitcli and asc implementations; callers in editor, pull, wiki, init, fork, branch | `forgejo/modules/repostore/**`, call sites in `forgejo/services/{pull,wiki,repository}/**` | `go test ./modules/repostore/... ./services/pull/... ./services/wiki/...` | G05 |
| G07 | Remove remaining raw `NewCommand` calls (fork phase 4) | `forgejo/modules/indexer/code/**`, gitgraph, contributors, release, asymkey, doctor | `go test ./modules/indexer/... ./services/...` | G06 |
| G08 | asc authz endpoint, `asc-authenticate` SSH verb, hook bridging (seam C) | `forgejo/routers/private/asc.go`, `forgejo/cmd/serv.go` (patch), `forgejo/modules/private/asc.go` | `go test ./routers/private/... ./cmd/...` | G05 |
| G09 | `services/ascbridge`: bridge path, LFS from asc, mirror wiring | `forgejo/services/ascbridge/**`, `forgejo/services/lfs/**` (patch) | `go test ./services/ascbridge/...` | G08, U12 |
| G10 | Actions: asc context fields, `paths:` via `ChangedPaths` | `forgejo/modules/actions/**`, `forgejo/services/actions/**` | `go test ./modules/actions/... ./services/actions/...` | G05 |
| G11 | Dual-backend integration suite | `forgejo/tests/integration/asc_*_test.go`, `forgejo/Makefile` target | `make -C D:/dev/forgejo test-sqlite-asc` | G06-G10 |
| A01 | `asc-affected` and `asc-checkout` actions | `actions/asc-affected/**`, `actions/asc-checkout/**` | `bun test --cwd actions` | U28, U24 |

Ordering logic: U01-U11 give a verified local engine with no network or filesystem tricks. U12-U15 make it usable as a non-virtual CLI on monorepo. U16-U25 add the per-machine cache and the virtual working copies, ending with the turbo contract gate, which is the main disk and CI risk. U26-U29 add the server and CI. G01-G11 can start in parallel with U01 (G01-G03 do not depend on asc) and join at G04.

---

## 23. Risks and open questions

| Risk | Impact | Mitigation |
|---|---|---|
| ProjFS stat values change on hydration or by other tools | Turbo sees every file as dirty and hydrates the whole tree | Contract test 4 in CI on Windows; `asc doctor` self-test; fall back to non-virtual mode per WC |
| Antivirus or indexers hydrate everything on Windows | Worktrees approach full-copy cost | Dehydration GC with budget; Defender exclusion or Dev Drive performance mode; measure with `asc debug hydration` |
| ProjFS on Dev Drive (ReFS) unverified | No block-clone hydration | Spike in Phase 0; NTFS works without it |
| Forgejo runner config may not allow host daemon mounts | CI falls back to sparse mode | Sparse mode is the Phase 4 default; virtual mode after operator sign-off |
| Garage lacks conditional writes | No serverless refs on Garage | Client/server mode uses Postgres refs; immutable writes need no CAS |
| Git-output parity (diff, status) drifts | Scripts and Forgejo parsing break | Golden tests against git 2.55 in CI |
| Turbo changes its SCM layer | Projection stops avoiding hydration | Contract tests per turbo minor; optional upstream `TURBO_SCM_CHANGED_FILES` proposal |
| Bridge SHA-1s vs asc IDs confuse users and tools | Wrong links, broken commit statuses | Forgejo shows asc IDs with bridge SHA-1 as an alias; `GitMap` RPC; statuses accept either |
| Land leader is a single point per hot ref | Latency spikes on failover | Advisory-lock failover, idempotent `op_id`, batching; measured by asc-replay |
| Many small RPCs per Forgejo page | Slow UI | Batch RPCs (`LastModified`, `Log` headers), immutable-ID LRU, 5-RPC page budget tested in G11 |
| Case-colliding paths in history | ProjFS cannot project them | Ingest check + mapping; land-time rejection |
| Submodule consumers outside the monorepo | Subtree import may not be possible | Nested-mount `Submodule` entries remain supported |

Open questions to settle in Phase 0 spikes: ProjFS async completion behaviour under `tsc --build` load; FUSE passthrough with bind mounts into containers; actual import throughput on the 58 GB repository (bounded by LFS transfer); real CI timings from Forgejo runner logs to replace the estimates in §14.3.
