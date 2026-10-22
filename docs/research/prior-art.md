# asc: prior-art research

Status: research input for the asc design. Date: 2026-10-22.
Scope: what ASC's new source control system, `asc`, should copy or avoid from Scion, Meta Sapling/Mononoke/EdenFS/Watchman, Microsoft VFS for Git/Scalar/ProjFS, Google Piper/CitC, Jujutsu, Pijul, modern git (partial clone, sparse, merge-ort), BLAKE3/bao, FastCDC and zstd. It also looks at how Turborepo talks to git, because CI speed is a main goal.

Fixed decisions this report works within: a Bonsai-style changeset model (not git object IDs), full storage replacement inside a Forgejo fork, Rust for client/daemon/server, a git-shaped `asc` CLI, storage on S3-compatible object stores or local disk with or without a server, lazy materialisation through a virtual filesystem, cheap worktrees, and faster Turborepo CI.

Each section ends with **Copy / Avoid**. Section 13 answers the specific design questions. Section 14 ranks the ideas.

Primary sources read: the Scion article; Mononoke docs `eden/mononoke/docs/*.md` and `blobstore/packblob/README.md`; EdenFS docs `eden/fs/docs/*.md` (Windows, macOS, Inodes, Caching, Checkout, Redirections, WindowsFsck, Glossary); Sapling internals docs (IndexedLog, MetaLog, ZstDelta, LineLog, visibility/mutation, axes of scale, differences from git). All of these come from a sparse clone of `facebook/sapling` at its October 2026 HEAD. Also read: the Jujutsu technical docs (architecture, concurrency, conflicts) and the `Backend`/`WorkingCopy` traits at jj HEAD (v0.46); `vercel/turborepo` `crates/turborepo-scm`; the Microsoft ProjFS docs; the VFS for Git protocol doc; GitHub's "Story of Scalar". Full source list at the end.

---

## 1. Scion (Anthropic, 2026)

**What it is.** An in-house replacement for git in a monorepo with roughly 100 GB of main-branch history, a 25 GB compressed working copy and about 25 M CI checkouts per day. Merge rate grew from 2.5 k/day (May 2026) to more than 30 k/day (Sept 2026). The server is built on Mononoke and its Bonsai model. Piper is cited as prior art.

**Key mechanisms.**
- **Always-online clients and client/server asymmetry.** The server owns history, blame, commits, merges and rebases. The client owns a bounded object cache, file layout and FS watching.
- **API-first.** CI asks the server for a diff against the merge base. It needs no checkout. Automated bisect and revert pipelines also run against the API.
- **Changeset = WAL entry.** It lists the files changed relative to the first parent, with content-addressed pointers to compressed blobs. Creating a changeset does not touch refs. Moving a ref is a compare-and-swap (CAS) on one Spanner row. Merges also record changes against the first parent (trunk-based by design).
- **Derived data.** Blame, path history, commit-graph skip lists, copy-detection indexes and path search. Each type is defined recursively from the same type on the parents, and is computed eagerly or deferred. Clients never see derived data, so its format can change without a client release.
- **No packfiles and no maintenance.** No full copy of the repo exists anywhere. All data is immutable and content-addressed, so any server can cache any of it.
- **Negotiation inverted.** The client does not tell the server what it has. It names the trees and blobs it wants: root tree, then missing subtrees recursively, then blobs. Every request is resumable. Sparse and virtual checkouts need no special support, because the client simply never asks for the unwanted trees.
- **Three tiers, one RPC surface.** `scion-daemon` runs one per node, serves local clients over a socket and boots from a periodically refreshed snapshot. `scion-cache` is regional, in-memory, streams objects in the daemon's on-disk format and does no delta compression. `scion-server` is authoritative and backed by Spanner and cloud storage. Tiers hold no long-lived state. Each tier has a cache hit rate above 99%.
- **Results.** CPU and memory footprint 200x smaller than the git mirror fleet. CI and research checkouts 100x faster (seconds instead of minutes). 25% of CI CPU capacity won back.
- **Merge queue without a global lock.** Correctness relies on post-merge and pre-deploy testing. An agent resolves races.

**Copy.** Every bullet above. The most important for ASC:
1. Separate immutable data (changesets, trees, blobs: put anywhere, cache anywhere) from the tiny mutable part (refs, by CAS).
2. Make diff and merge-base server RPCs so CI and Forgejo never need a checkout.
3. Use want-driven, per-object, resumable fetch.
4. Use the same RPC at every tier, so a per-machine daemon gives the shared cache that fixes the worktree disk problem.

**Avoid.** Scion assumes Spanner and a large fleet. ASC needs the same contracts on S3 plus SQLite/Postgres, or S3 alone. Scion also discourages refs. ASC's Forgejo users rely on branches and PRs, so asc keeps cheap branches as plain ref rows.

---

## 2. Mononoke (Meta's server, GPL-2.0)

### 2.1 Bonsai changesets (`2.1-bonsai-data-model.md`, `mononoke_types/src/bonsai_changeset.rs`)
- **Inherent (hashed) data:** parents; author and date; optional committer and date; message; `hg_extra` and `git_extra_headers`; and a **flat, sorted map `path -> FileChange`**.
- **`FileChange` variants:** tracked change `{content_id, file_type, size, copy_from: Option<(path, changeset)>, git_lfs flag}`, tracked deletion, untracked change and untracked deletion. The untracked variants are for working-copy *snapshots*.
- **File types:** regular, executable, symlink, git submodule.
- **IDs:** Blake2b-256 over the Thrift-compact serialisation. `ContentId` is a hash of the bytes. Large contents are chunked (`FileContents::Chunked`).
- **No root tree in the changeset.** Trees (fsnodes, unodes, skeleton manifests, git trees) are *derived data*. This keeps the write path minimal: store changeset and blobs, update the commit graph, move the bookmark. All other work happens off the critical path.
- **Copies are recorded explicitly** (`copy_from`), Mercurial-style, not guessed. Guessed copies live in a separate derived type, `inferred_copy_from`.
- **Git compatibility is derived.** Git commits and trees are a derived type (`MappedGitCommitId`). A SQL `bonsai_git_mapping` table holds the bidirectional mapping. Content metadata stores the git blob SHA-1, so a git blob ID never needs a re-read.
- **LFS handling:** Mononoke can *interpret* LFS pointers on push. It stores the real content in Bonsai and sets a `git_lfs` flag so the git export emits a pointer again.

### 2.2 Derived data framework (`2.3-derived-data.md`)
- Trait `BonsaiDerivable`: `derive_single(bonsai, parents' derived values)`, `store_mapping`, `fetch`, plus batch variants, `Dependencies` and `PredecessorDependencies` (for parallel backfill).
- Modes: derive on demand on the read path, batch derivation, backfill (topological, or sliced in parallel), and a remote derivation service with a dependency-aware queue that coalesces duplicate requests.
- **Warm bookmark cache:** bookmark moves become visible only once derived data for the target exists. Readers therefore never stall on derivation, at the cost of a few seconds of lag.
- Versioning: a new version is a new type (Blame -> BlameV2), backfilled and then switched. Mapping key prefixes allow a re-derivation side by side with the old one.
- **Manifest types worth knowing:**
  - *fsnodes*: a pure content tree. Each entry holds size, content hashes and file type. Each directory has a summary (counts, total size).
  - *unodes*: a tree with history linkage, so it depends on path and history.
  - *skeleton manifests*: paths only, for existence and case-conflict checks.
  - *basename-suffix skeleton*: for `prefix/**/*.suffix` search.
  - *deleted manifest*: answers "was this path ever deleted".
  - *fastlog*: per-file history.
  - *blame*.
  - *changeset_info*: metadata without the file list.
- **Sharded Map V2:** a trie with path compression and bounded node size. Huge directories stay cheap to load partially.

### 2.3 Blobstore stack (`2.4-storage-architecture.md`, `packblob/README.md`)
- One small trait (`get`, `put`, `is_present`) with interchangeable backends: Manifold, SQL (MySQL/SQLite), **S3**, file and memory.
- **Decorators**, stacked by a factory from config: `prefixblob` (namespace per repo), `cacheblob` (in-process plus memcache), `multiplexedblob`, `packblob`, `redactedblob`, `throttledblob`, `samplingblob`, `logblob`, `readonlyblob`, `chaosblob`/`delayblob` (tests), and a virtually-sharded cache that dedupes in-flight requests.
- **Multiplex:** write-all and read-any. A write succeeds on a quorum. Partial writes go to a WAL that a *healer* job repairs.
- **Packblob:** a storage envelope, a Thrift union of "single" or "packed", so a reader needs no side metadata. Related blobs (versions of one file) are packed with **zstd, using one blob as the dictionary for the rest**. It sits *below* multiplex, so different replicas can use different packing.
- **Rule for what goes where:** immutable, content-addressed or large data goes to the blobstore. Mutable, small, indexed or relational data goes to SQL: bookmarks plus the bookmark update log, VCS mappings, commit-graph index (generation numbers and skip-tree pointers), phases, mutable counters, and the pushrebase mutation map.
- Caches never need invalidation because data is immutable. TTLs exist only for capacity.

### 2.4 Pushrebase (`4.1-pushrebase.md`)
- The client pushes a stack based on an old main. The server computes the rebased set and checks for **path-level** conflicts between "files touched by the stack" and "files touched on main since root". Prefix (file vs directory) conflicts and case-folding conflicts also count.
- With no conflict, the server rebases (new changesets with new parents), runs hooks and moves the bookmark in one transaction. If the bookmark moved meanwhile it retries (up to about 100 times). It returns the old-to-new mapping so the client needs no fetch.
- Path-level conflict detection is deliberately conservative: the same file touched on both sides counts as a conflict even when the line ranges differ.

### 2.5 Services (`3.1-servers-and-services.md`)
- **SLAPI/EdenAPI:** HTTP batched endpoints for Sapling and EdenFS (trees, files, history, commit lookups). **SCS:** about 80 Thrift methods for tools (history, blame, file content, listings, diffs, commit creation, bookmark moves) with mTLS authentication and per-repo permission checks. **Git server:** smart HTTP, packfiles generated from delta-manifest derived data, bundle-URI support. **LFS server:** batch API over the same filestore.
- Separate *land service* and *bookmark service* serialize bookmark moves and keep the warm cache.

**Copy.**
- Flat path->change changesets with explicit copy-from, content metadata that carries the git SHA-1, and git objects as *derived* data.
- The `Derivable` trait with dependencies and the warm-ref rule.
- The blobstore trait with a decorator stack and packblob envelope (zstd-dictionary packing).
- The blob-vs-SQL placement rule.
- Server-side pushrebase with path-level conflict checks.
- An SCS-style programmatic API, which is what Forgejo and CI will call.

**Avoid.**
- Mononoke's breadth: Mercurial and SVN mappings, cross-repo sync, many manifest versions. Start with one content manifest, plus history and blame later.
- Thrift.
- Meta-only infrastructure (Manifold, memcache, CacheLib).
- **Licence:** Sapling, Mononoke and EdenFS are **GPL-2.0**. Copy *ideas*, not code, unless ASC accepts GPL for the copied component. The Forgejo fork is GPLv3+ in any case, but the asc server and client should stay separately licensed programs that talk RPC.

---

## 3. Sapling client (`sl`)

- **Lazy everything.** Clone and pull fetch only the main bookmarks and the *graph*. Commit messages, trees and files are fetched on demand. Remote bookmarks are fetched when a command names them (`sl goto release_123` fetches that bookmark itself).
- **Segmented changelog.** The whole commit graph, compressed into segments, takes a few MB and answers ancestry and merge-base queries in O(merges). Commit metadata is stored separately and fetched lazily. If the server is unreachable, approximate file history comes from bisecting segments.
- **Per-file history** gives O(changes to file) `log` and `blame`. For directory history the client asks the server.
- **IndexedLog:** an append-only log with xxhash checksums plus derived radix-tree indexes. Readers are lock-free (append-only big files, atomic-replace of the small root pointer). It needs no repack, and `repair` truncates to the last good entry. **RotateLog** keeps N logs and drops the oldest, giving a bounded cache with O(1) eviction. Sapling's shared per-machine cache (`scmstore`/hgcache) is built on these.
- **MetaLog:** a tiny content-addressed store for bookmarks, remote names and visible heads, with a log of roots. Updates are atomic. Concurrent writers are merged by an application-supplied merge function, and every past state is viewable (this powers `undo`). **Write order:** first blobs, then trees, then commits, then MetaLog. Orphaned objects are harmless, so no rollback is needed.
- **Visibility and mutation:** amended or rebased commits are hidden, not deleted. Mutation records ("B amends A") drive `restack`, smartlog obsolescence markers and sharing through commit cloud.
- **UX:** no staging area, no required branches, smartlog, `undo`/`uncommit`/`unamend`, `absorb` (built on **LineLog** interleaved deltas, which also make blame cheap), `goto`, and sparse **profiles** checked into the repo. `sl githelp` translates git commands.
- **Commit cloud:** draft commits and bookmarks sync between a user's machines through server workspaces.
- **Scale notes:** directories with more than a few thousand entries degrade. Files of 10 MB to 2 GB go through an LFS-protocol path with a separate cache budget so they don't evict small objects.

**Copy.**
- An IndexedLog/RotateLog-style shared cache: lock-free reads, bounded size, self-repairing.
- MetaLog-style atomic local ref state with an operation history (undo).
- The "write refs last; orphans are harmless" rule.
- Lazy commit metadata plus a compact graph index on the client.
- Bookmark auto-fetch on use.
- Sparse profiles in the repo, which suit Turborepo package scopes.
- Mutation records for amend and rebase.
- A separate cache budget for large blobs.

**Avoid.** Dropping the staging area and branch-centric UX: asc's adoption goal is git muscle memory. asc can still offer `undo` and `absorb` as extra commands.

---

## 4. EdenFS (Meta's virtual filesystem)

### 4.1 Model
- **Two parallel models:** immutable source-control objects (commit/tree/blob, with **tree entries carrying size and content hash**) and mutable **inodes** (`TreeInode`, `FileInode`).
- **Materialised vs non-materialised.** A non-materialised inode is just an object ID. Writing a file materialises it. Its contents move to the **overlay** (a per-checkout local store), and every ancestor directory is materialised up to the first one already materialised. Materialised does not mean "modified": renaming a non-materialised file keeps its object ID, while `reset` can make unmaterialised files show as modified.
- **Loaded/unloaded** is a separate axis: inodes load lazily on lookup, inode numbers are allocated on demand from a 64-bit counter, and allocation is recorded in the overlay.
- **Journal:** every mutating FS op is recorded with a sequence number (and the snapshot hash, after a checkout). `getFilesChangedSince(position)` serves Watchman and `status`. **Status is O(materialised and journaled paths), not O(repo).**
- **Content hashes without reads:** SHA-1/BLAKE3 of files come from tree metadata through Thrift or an xattr. Buck and Watchman hash sources this way **without hydrating them**. A million-entry in-memory `blob id -> (size, sha1)` LRU takes under 100 MB.
- **Caches:** the kernel page/dentry cache is primary (FUSE replies use infinite TTL plus explicit invalidation). Behind it sit a small blob LRU (40 MiB, minimum 16 entries, evicted once the kernel has read the whole file, tracked by a coverage set) and a local store (RocksDB) whose eviction "is an unsolved problem".

### 4.2 Checkout algorithm (`slides/Checkout.md`)
- Diff the current tree against the destination tree, **recursing only into directories whose IDs differ *and* which the OS has loaded**. Unloaded directories just get a new object ID.
- For each changed entry: conflict check, then update, then **invalidate** the kernel cache. FUSE uses `FUSE_NOTIFY_INVAL_ENTRY`/`INVAL_INODE`. ProjFS uses `PrjDeleteFile` and `PrjMarkDirectoryAsPlaceholder`. NFS uses a no-op `chmod` to bump the mtime.
- **Checkout cost is O(loaded inodes)**, so periodic unloading and GC of inodes keeps checkout fast.

### 4.3 Platforms
- **Linux: FUSE.** Every write pays a user-space round trip. This motivates **redirections**: `buck-out` and other generated-output directories are bind-mounted (or symlinked) to local disk and bypass the journal. Takeover passes FUSE fds to a new daemon for zero-downtime upgrades.
- **macOS: NFSv3 loopback** (kexts are deprecated). No xattrs, no client PID, unreliable invalidation, `readdir` without types (N extra lookups, so a Thrift `readdir` was added), no `forget` (so the inode count grows and needs background GC), and hung I/O if the daemon dies. FSKit is under evaluation.
- **Windows: ProjFS.** See section 5. EdenFS keeps the same inode model but treats ProjFS as the cache owner.

**Copy.**
- Trees that carry size, content hash and git-sha1, so tools hash without hydration.
- Materialisation propagating to the root, with an overlay holding only modified files.
- A journal with sequence numbers and a `changed-since` API.
- Checkout as hash-pruned tree diff plus targeted invalidation, O(loaded).
- Redirections for build outputs. For ASC that means `node_modules`, `.turbo`, `dist`, `.next`, `target`, `.venv` and `__pycache__`.
- Background inode/placeholder GC.
- A Thrift-like "readdir-with-types" and "glob" API for crawlers.

**Avoid.**
- RocksDB local store with no eviction. Use RotateLog-style or LRU-with-budget instead.
- NFS on macOS as the first target. Use FSKit or FUSE-T, or the non-virtual reflink mode (section 13.6), until FSKit proves out.
- C++/folly complexity. Use Rust: `fuser` for FUSE and the ProjFS bindings via the `windows` crate.

---

## 5. ProjFS (Windows) specifics and pitfalls

From Microsoft docs and EdenFS `Windows.md` / `WindowsFsck.md`:

- **States:** *virtual* (only projected during enumeration), *placeholder* (metadata on disk, no data), *hydrated placeholder* (full data cached on NTFS), *dirty placeholder* (metadata changed), *full* (content modified or locally created, no longer a cache), *tombstone* (hidden marker for a locally deleted projected item). A directory created as a placeholder never becomes hydrated or full, so the provider can still change its projection later.
- **Callbacks:** start/get/end directory enumeration, `GetPlaceholderInfo`, `GetFileData` and an optional notification callback. **ProjFS refers to files by path, not inode.** Directory listings are *not cached*: each enumeration calls back.
- **Notifications:** pre-operation and vetoable (`PRE_DELETE`, `PRE_RENAME`, `PRE_SET_HARDLINK`, `FILE_PRE_CONVERT_TO_FULL`) and post-operation (`FILE_OPENED`, `NEW_FILE_CREATED`, `FILE_OVERWRITTEN`, `FILE_RENAMED`, `HARDLINK_CREATED`, `FILE_HANDLE_CLOSED_{NO_MODIFICATION,FILE_MODIFIED,FILE_DELETED}`). Notification masks can be set per subtree, for example `SUPPRESS` under redirected dirs.
- **Pitfalls EdenFS hit:**
  1. Post-op notifications are **asynchronous and unordered** (a child can arrive before its parent). Treat each one as "something changed near this path" and re-stat the disk. Don't trust the event.
  2. Writes **cannot be refused**, and files **can change while the provider is not running**. So a **filesystem check on every start** is required (EdenFS rewrote it to "one FS op per file").
  3. **ProjFS never forgets hydrated files.** The working set grows and checkout slows, because every hydrated file must be invalidated. A background GC must call `PrjDeleteFile` on unmodified, not-recently-used files.
  4. **Invalidation fails when another process holds the file open.** The result is a stale file that shows as modified. Checkout must detect this and either fail or retry.
  5. `PrjMarkDirectoryAsPlaceholder` on an unpopulated directory recursed into callbacks and **deadlocked**. Never call into ProjFS from inside a callback thread holding locks.
  6. **The virtualization-instance GUID** must stay stable for the life of the checkout, or you get "provider temporarily unavailable".
  7. **Directory renames are unsupported.** Renamed files are still read under their *old* path, so the provider must serve reads from the committed tree only, never from the overlay.
  8. Symlink support was buggy until 2024 patches.
  9. ProjFS is **not on by default**. It is the optional feature `Client-ProjFS`, which needs admin rights once.
- **Hydration duplicates bytes on disk.** A hydrated placeholder is a real NTFS copy. A worktree whose tools read everything (indexers, `rg`, `tsc --build` over all packages, antivirus) ends up as a full copy. Mitigations: GC dehydration, Defender exclusions or Dev Drive *performance mode* (async scanning), and redirections for `node_modules`. Third-party reports say real-time scanners trigger full hydration on partial reads. To verify on ASC machines: whether the ProjFS minifilter (`PrjFlt`) must be explicitly allowed on Dev Drive volumes (`fsutil devdrv setFiltersAllowed`).
- **Upside vs FUSE:** after hydration, reads *and writes* are native NTFS speed with no user-mode hop, so redirections matter less on Windows than on Linux.
- Microsoft states that ProjFS assumes a *fast* backing store and recommends the Cloud Files API for slow stores (it has progress and offline states). For asc the backing store is the local daemon cache, which is fast, so ProjFS is right. Only cold misses go over the network.

**Copy.** EdenFS's rules: serve callbacks from committed trees only; reconcile notifications against disk; FSCK at start; GC of hydrated placeholders; stable GUID; no ProjFS calls from inside callbacks; per-subtree notification masks; and detect and enable the ProjFS feature at `asc clone`.

**Avoid.** Relying on notifications as an exact journal on Windows. Use them as hints and keep a reconcile pass, ideally USN-journal-assisted, to verify.

---

## 6. Microsoft VFS for Git (GVFS), Scalar and git's own scale features

- **GVFS protocol:**
  - `GET /gvfs/objects/{id}` returns one loose object.
  - `POST /gvfs/objects {objectIds, commitDepth}` returns a pack or a stream of loose objects.
  - `GET /gvfs/prefetch?lastPackTimestamp=` returns *timestamped packs of non-blob objects* with indexes. Packs arrive in timestamp order, so a client can resume from the last completed one.
  - `POST /gvfs/sizes` returns sizes without content, so `ls -l` and placeholders need no download.
  - `GET /gvfs/config` gives allowed client version ranges and **cache server** URLs.
  - Cache servers speak the same protocol. Scion repeats the "same RPC at every tier" idea.
- **Lessons (Scalar story):**
  - VFS for Git died on macOS when Apple removed kernel extensions. Portability of the virtual FS is the main risk.
  - Users "frequently get into bad states" by hydrating too much or by not enabling ProjFS.
  - Microsoft moved to **partial clone (`--filter=blob:none`) plus cone-mode sparse-checkout, `fsmonitor` built into git, and `git maintenance`**. That stack became Scalar and shipped in Git 2.38.
  - Cone mode (directory-prefix patterns only) took pattern evaluation from about 40 minutes to fast, and `git status` from slow to 3–4 s.
  - Removing the mount process simplified everything.
- **Git features worth mirroring:**
  - *Partial clone/promisor:* batched missing-object fetch.
  - *Sparse index:* the index stores collapsed directory entries outside the cone.
  - *fsmonitor daemon:* a token-based "changed since" query, in git itself on Windows and macOS.
  - *untracked cache.*
  - *commit-graph* with generation numbers and **changed-path Bloom filters**, which give fast `log -- path`.
  - *multi-pack-index.*
  - *bundle-uri:* clone bootstrap from static files on a CDN or S3.
  - `git worktree` shares the object DB but **materialises a full checkout per worktree**. This is ASC's pain: 50 worktrees, plus LFS stored twice (once in `.git/lfs/objects` and once smudged into each worktree).

**Copy.**
- The sizes endpoint, folded into tree entries.
- Timestamped, resumable bootstrap snapshots: asc's version is a periodic daemon snapshot, like Scion's.
- Cache-server discovery from server config, and client version gating from server config.
- **Cone-only sparse patterns.** Turborepo packages are directories, so this is a natural fit.
- A non-virtual fallback mode (sparse plus lazy) for platforms where the VFS is unavailable.

**Avoid.**
- Building the product on one OS's kernel extension.
- Per-object round trips (the old read-object hook).
- Packs as the transfer unit.

---

## 7. Google Piper and CitC

- **Piper:** a centralised, server-only repository on Spanner/Bigtable, replicated across data centres, with directory-level ACLs. The 2015 snapshot held about 2 B lines of code, 9 M source files, 35 M commits and 86 TB. Trunk-based, with `OWNERS` files.
- **CitC (Clients in the Cloud):** a cloud-hosted workspace store plus a FUSE filesystem. The workspace stores **only the files the developer changed**, overlaid on a Piper snapshot. Reported averages are under 10 files per workspace (secondary source). Workspaces are visible from any machine and to code review and build tools. Snapshots happen continuously, and review comments can be attached to workspace states.
- Build and test (Blaze/TAP) read sources through the same virtual layer and content hashes, so they never "check out".

**Copy.**
- **A workspace is a server-side object** (a base changeset plus an overlay of changed files) that any machine, CI or Forgejo can read. In asc terms, commit-cloud-style syncing of the overlay and draft changesets: an agent worktree can be "opened" on another machine or in CI with no push.
- Directory ACLs at the server.
- Building from content hashes.

**Avoid.** Assuming always-on Google-grade networking. asc needs a local-only mode, with the whole store on disk and no server.

---

## 8. Jujutsu (jj)

- **Library-first:** at HEAD the crate is `jj-core` (renamed from `jj-lib`), version 0.46, Apache-2.0, **MSRV 1.97.1** (newer than ASC's local Rust 1.95). Storage sits behind traits so Google can run it on a cloud backend:
  - `Backend`: async `read_file`/`write_file`, `read_tree(path, id)`/`write_tree`, `read_commit`/`write_commit`, copy-history objects, and `concurrency()` as a hint for cloud backends.
  - `OpStore` and `OpHeadsStore`.
  - `Index`.
  - `WorkingCopy`/`LockedWorkingCopy`: `snapshot`, `check_out`, `reset`, `recover`, `set_sparse_patterns`, `finish`.
  - Factories select the backend through `.jj/repo/store/type`.
- **Working copy as a commit:** each command first *snapshots* the working copy into a commit (`TreeState` with an mtime/size cache, optional Watchman fsmonitor). "All working copies are sparse."
- **Operation log:** operations are content-addressed objects that point to a *view* (heads, bookmarks, tags, the working-copy commit per workspace). There are **no locks**. A command loads the latest operation and always commits its own. Concurrent heads are 3-way merged on the next load, and conflicted bookmarks are recorded as such. Op heads are files named by ID: add the new one, then delete the old one. `undo` and `--at-op` come for free.
- **First-class conflicts:** a conflicted value is `Merge<T>`, an odd-length list `A + (C - B) + (E - D)…` of trees or file IDs stored in the commit. It is resolved lazily and only where needed (identical subtree IDs short-circuit). Algebraic `flatten`/`simplify` cancels terms, so rebasing a conflicted commit never nests conflicts (`D+((C+(B-A))-C) = D+(B-A)`). The "same-change rule" auto-resolves identical edits. Conflicts are materialised to markers in files and parsed back on snapshot.
- **StackedTable:** an immutable, hash-named, sorted key-value table with parent chaining (O(log N) amortised), used for lock-free indexes.
- **Workspaces** (jj's worktrees) share one repo. A workspace becomes "stale" when another workspace rewrites its commit, and it recovers on the next update.

**Should asc reuse jj-core?** Recommendation: **no as a foundation; yes as a source of ideas and possibly a small vendored module.**
- Its model is change IDs plus working-copy commits with no index. asc must mirror git's index and branch UX.
- It is pre-1.0 with API churn and has an MSRV above ASC's toolchain.
- Its `Backend` is path-addressed per tree object, which fits, but asc's server-side diff, merge and derived data do not map onto it.
- Coupling asc's core types to jj's release cadence adds risk without much code saved.

What to take:
1. The `Merge<T>` conflict algebra. It is small and Apache-2.0, so it can be ported with attribution.
2. The op-log/view design for client-side ref state and `asc undo`.
3. The `TreeState` snapshot approach for the non-virtual mode.
4. The rule "storage interfaces in plain Rust types, with a backend chosen by a type file".
5. Later, a **`jj` backend for asc** (implementing `Backend` over the asc RPC) lets jj users work on asc repos. This is cheap and optional.

---

## 9. Pijul

- A patch-based model: changes form a graph of lines (the "graggle"). Independent patches commute, merges never fail, conflicts are a state of the graph, and resolving a conflict is itself a patch.
- Mature for small repos only. It has had public performance problems on large repos, and no evidence was found that monorepo scale is solved.

**Copy.** The idea that conflicts are *data*, not an error state. jj's `Merge<T>` gives most of this benefit on top of snapshots.

**Avoid.** A patch-algebra core. It is too risky for a monorepo with high churn, and it conflicts with "simple" and with git interop.

---

## 10. Hashing, chunking and compression

- **BLAKE3:** fast, SIMD and multi-threaded, with an internal Merkle tree of 1 KiB chunks. **Bao** gives *verified streaming and range reads*: a client can verify any byte range of a large file against the root hash. iroh-blobs stores outboards at 16 KiB chunk-group granularity (`.obao4`), which cuts outboard size by 16x without changing the root hash. The `bao-tree` crate supports configurable chunk groups. The BLAKE3 crate now exposes a "hazmat" API for subtree hashes.
- **FastCDC:** content-defined chunking with a gear hash, normalised chunking, sub-minimum cut-point skipping and two-byte rolling. It runs about 10x faster than Rabin CDC with a similar dedup ratio (ATC'16; TPDS'20 reports 3–12x faster than the state of the art). The Rust crate is `fastcdc`, v3 / 2020 variant.
- **zstd:**
  - Per-repo trained **dictionaries** help small source files a lot. Train per file class: `.ts/.tsx`, `.json`, `.rs`, `.py`.
  - **`--patch-from`/reference-prefix compression** lets one version act as the dictionary for the next. This is what Mononoke packblob and Sapling's `zstdelta` do.
  - Keep deltas *inside the storage layer* (packing), never in the protocol. Scion's caches do no delta compression.
- **git SHA-1 as a derived content attribute.** Compute it once on ingest (Mononoke stores git SHA-1, and fsnodes store SHA-1 and SHA-256). It keeps the git bridge cheap and lets Turborepo, IDEs and LFS keep their hash expectations.

**Copy.**
- Content ID = BLAKE3 of raw bytes, giving verified range hydration for big files.
- Large files above a threshold (about 4 MiB) stored as a FastCDC chunk list (about 64 KiB–1 MiB target chunks), each chunk content-addressed.
- zstd with trained dictionaries for small objects.
- Version packing as a background storage optimisation.
- Precomputed git-sha1 and size in tree entries.

**Avoid.** SHA-1 or SHA-256 as primary IDs. Avoid git-style delta chains as the storage or transfer unit, and avoid LFS pointers as a separate system.

---

## 11. Object storage (S3-compatible) as the backbone

- AWS S3 now supports **conditional writes**: `If-None-Match: *` (create-only) and `If-Match: <etag>` (compare-and-swap) on PutObject and CompleteMultipartUpload. MinIO supports both. OVH supports PutObject. **Support in R2, Garage and Ceph RGW was not confirmed** in this research, so a startup probe is needed.
- This allows a **serverless mode**: immutable objects as content-addressed keys (put-if-absent), refs as small objects updated by `If-Match` CAS, and the ref log as append-only keys. Where CAS is unavailable, fall back to a single writer (the asc server, or a lock object with a lease).
- Content-addressed objects with `Cache-Control: immutable` let **any HTTP cache or CDN act as a regional cache tier**. The server can also hand out presigned GET URLs for large blobs, as the LFS batch API does, to take bandwidth off the asc server.
- Small-object economics: billions of tiny PUTs are expensive and slow on S3. Pack small objects into **segment files** (append-only, as in IndexedLog or Bonsai packblob) with an index, and fetch them with ranged GETs. Keep large chunks as standalone keys.

**Copy.** Use a `BlobStore` trait (get, get_range, put_if_absent, exists, list_prefix) with S3 and local-dir backends behind Mononoke-style decorators. Use a `RefStore` trait with CAS (SQLite, Postgres, or S3 conditional writes). Use a packed-segment layout for small objects.

---

## 12. Turborepo and git today (what CI depends on)

From `crates/turborepo-scm` at HEAD:
- `enum SCM { Git(GitRepo), Manual(FileHashMode) }`. "Currently we only support git."
- Hashing of tracked files uses **git blob OIDs (SHA-1)**. It reads `.git/index` **directly via `gix-index`** and stat-compares each entry. Racy entries are deferred, which avoids reading every file on a fresh checkout. It also shells out to `git ls-tree -r -z HEAD`, `git ls-files`, `git status --untracked-files --no-renames -z`, `git diff HEAD`, `git rev-parse`, `git merge-base`, `git show <rev>:<lockfile>` and `git hash-object`.
- Affected detection (`--affected`, `--filter=...[base]`) uses `git diff --name-only` against a base ref resolved from CI env (`GITHUB_BASE_REF` and similar).
- If git is not found, **`Manual` mode reads and hashes every file**. On a virtual checkout that hydrates the whole repo, which is the worst case.

**Implications for asc.** Turborepo needs three things from asc:
1. Per-file git-SHA-1 for tracked files, without reading them. asc has this as tree metadata (section 10).
2. A dirty-file list. asc has this from the journal.
3. Changed files between two revisions. asc has this from the server diff RPC.

There are two ways to provide them:
- **(a) A small upstream or forked patch** adding `SCM::Asc`, which calls `asc ls-tree -r -z --git-oid`, `asc status -z` and `asc diff --name-only base...head`.
- **(b) A `git`-compatible shim** for the handful of plumbing commands turbo uses. That alone is not enough, because turbo reads `.git/index` directly. Option (a) is cleaner and keeps **hash compatibility with today's remote cache**, since the git OIDs are identical.

Turborepo's remote cache is a simple HTTP API. Self-hosted servers can target S3, so the turbo cache can live in the same bucket under a different prefix.

---

## 13. Answers to the specific design questions

### 13.1 Changeset format
- **Bonsai-style, inherent only:** `{parents: Vec<ChangesetId>, author, author_date, committer, committer_date, message, extra: BTreeMap<String, Bytes>, changes: BTreeMap<RepoPath, Change>}`.
  - `Change = Write { content: ContentId, size: u64, kind: File|Exec|Symlink|Submodule, copy_from: Option<(RepoPath, ChangesetId)> } | Delete`.
  - Changes are relative to the **first parent** (Scion). For merges, also record paths whose resolution differs from both parents. Mononoke does this, so `p1 + changes` reconstructs the tree.
- **Canonical, versioned binary encoding** with a 1-byte format version and sorted maps, defined in one small `asc-types` crate. ID = `BLAKE3(domain_tag || bytes)`, with domain-separated typed IDs (`ChangesetId`, `TreeId`, `ContentId`, `ChunkId`) as distinct Rust newtypes.
- **Do not put the root tree in the hashed changeset.** Make the content manifest *eagerly derived* and required before a ref moves (warm-ref rule). Push stays O(change) and the manifest format can evolve. The alternative is to include `root_tree` in the changeset (git-like). That makes verification trivial but freezes the tree format. The Bonsai approach is recommended, as Scion shows it works at scale.
- **Snapshot changesets** (untracked changes allowed, not on refs) for workspace sync, stash and agent handoff.
- Git commits and trees are **derived data plus a mapping table**. They are not identity.

### 13.2 Tree representation: Merkle trees vs flat manifests
- **Merkle trees of content only** (fsnode-like), per directory: sorted entries `{name, kind, id, size, git_sha1}` plus a directory summary (file count, total bytes). Use **sharded nodes** (bounded-size trie, Sharded Map V2 style) only for directories above about 2–5 k entries.
- Why Merkle over flat:
  - Hash pruning makes diff, checkout and merge O(changed directories).
  - Sparse and virtual fetch can stop at any subtree.
  - Identical subtrees dedupe across branches.
  - The VFS can answer `readdir` and `stat` from one node.
  - Flat manifests (the Bonsai *changeset* is flat; Mercurial's old flat manifest) are O(repo) per revision.
- Keep **history out of tree IDs**. Unode-style path-history linkage is a separate derived type, so identical content means identical TreeId.

### 13.3 Lazy fetch negotiation
- **Want-driven, per-object, batched by level** (Scion/EdenAPI). The client holds a root `TreeId`, asks its daemon for missing tree IDs (a batch per BFS level, many in flight), and fetches blobs only on hydrate. Nothing is "have"-negotiated.
- Add two server-assisted shortcuts to remove round trips:
  1. **`TreeDiff(old_root, new_root, prefixes?)`** streams only the differing tree nodes, so `switch` is O(changed directories).
  2. **`Prefetch(root, cone_prefixes, max_bytes)`** streams the tree closure (and optionally small blobs) for a sparse cone. CI and "warm my package" use this.
- **Resumable by construction:** every unit is an independently verifiable object. Large files resume by chunk, or by byte range verified with Bao.
- Tiers: CLI and VFS -> **per-machine daemon** (socket or named pipe; shared cache) -> optional regional cache (any HTTP cache, since objects are immutable) -> server -> blobstore. Every tier runs the same RPC.
- **Local-only mode:** the "server" is a library linked into the daemon over a local dir or a bucket. The RPC is the same, just in-process.

### 13.4 Cheap diffs
- **Tree diff with hash pruning:** walk both trees and skip equal IDs. Cost is O(changed directories + changed entries). Sizes and git-sha1 in entries give `--stat` totals and rename candidates without reading blobs.
- **Merge-base and ancestry** from a commit-graph index with generation numbers and skip pointers (Mononoke `commit_graph`, Sapling segments).
- **Path-level CI diff** = `changes` maps of the changesets between base and head, unioned and cancelled out. There is no tree walk at all for linear history. This is the cheapest possible "affected packages" query and works without a checkout.
- **Renames:** explicit `copy_from` from the client (`asc mv`, or the VFS seeing a rename), plus a derived `inferred_copy` for git-imported history. Use git-like heuristics: exact-hash first, then same basename, then similarity limited to the changed set (merge-ort's lesson).
- **Line diffs** run server-side for review and API (`imara-diff`, histogram/Myers) and are cached as derived data by `(content_a, content_b)`.

### 13.5 Large files (replacing LFS)
- No pointer files. Every file is ordinary content.
- Above about 4 MiB, content becomes a FastCDC chunk list (`ChunkedContent { chunks: Vec<(ChunkId, len)> }`). ContentId stays BLAKE3 of the full bytes, so the identity does not depend on chunking parameters. Chunks dedupe across versions, which matters for ASC's ~7.6 GB of LFS.
- Hydration supports **range reads**: the VFS fetches only the chunks touched, with Bao verification.
- Separate cache budget for large chunks (Sapling) so they don't evict trees.
- Git bridge: content marked `lfs_on_export` is exported as a pointer and served by an LFS endpoint that reads the same chunks (the Mononoke pattern).

### 13.6 Cheap worktrees and workspaces
- **One object cache per machine** (daemon-owned, bounded, RotateLog/LRU), shared by every worktree of every repo. A worktree is a mount: base changeset, overlay of modified files, journal, small state file. **Cost = metadata plus modified files plus hydrated files** (Windows) or **plus modified files only** (FUSE, which serves reads from the shared cache).
- `asc worktree add` = write the state and mount. This takes milliseconds, independent of repo size.
- **Redirections** keep build outputs and `node_modules` out of the VFS, on local disk. Optionally they are shared via bun's global store and hardlinks.
- **Non-virtual fallback**, for CI containers without FUSE, macOS before FSKit, or users who opt out: materialise only the sparse cone, by **reflink/clone from the shared cache** where the FS supports it (ReFS Dev Drive block cloning, btrfs/XFS reflink, APFS `clonefile`), and by copy otherwise. Never hardlink, because an edit would corrupt the cache. Status in this mode uses a jj/git-style stat cache plus an fsmonitor.
- **Server-side workspace sync** (CitC / commit cloud): `asc workspace push` uploads the overlay as a snapshot changeset. Another machine or CI can mount it.

### 13.7 Fast status
- In the VFS, asc *is* the filesystem. The journal (sequence number + path + op) plus the materialised set give **O(changes) status**. Compare overlay files against the base tree by size and BLAKE3, and de-materialise when content matches again.
- On Windows, ProjFS notifications are unordered hints. Keep a dirty-path set fed by notifications and **reconcile by stat on query**, with **FSCK at daemon start**. Optionally use the NTFS USN journal as a cross-check.
- Expose **`changed-since(token)`** (the Watchman and git-fsmonitor-v2 protocol shape). Turbo's daemon, IDEs and file watchers then avoid crawling. A "fresh instance" flag tells clients when the journal was truncated or the daemon restarted.
- Untracked files: only in materialised (dirty) directories, because unmaterialised directories cannot contain untracked files. That removes the classic full-tree untracked walk.

### 13.8 Conflict representation
- **Port jj's `Merge<T>`** (odd-length adds/removes) as the in-memory and storable conflict value for trees and files. Use algebraic simplification and the same-change rule.
- Store conflicted states only in **draft or snapshot changesets**. Refs that require clean state (main, release) reject them.
- In the working copy, materialise git-style markers (`diff3`/`zdiff3` styles selectable via `merge.conflictStyle`) so editors and tools keep working. Parse them back on `asc add`.
- **Server-side merge and rebase** use merge-ort's approach:
  1. Tree-level trivial resolution by ID (if a subtree is unchanged on one side, take the other wholesale).
  2. Rename detection only where relevant, cached across a rebase sequence.
  3. Content 3-way merge only for files changed on both sides.
  4. Never touch a working copy.
- Landing to trunk uses **pushrebase with path-level conflict detection**. A real merge happens only on explicit `asc merge`/`asc rebase`.

### 13.9 Staying git-command-compatible
- Same verbs, flags and revision syntax (`HEAD~2`, `A..B`, `A...B`, `@{u}`, `:/msg`, `<rev>:<path>`). The same **porcelain and plumbing output formats** (`status --porcelain=v1|v2 -z`, `diff --name-only/--name-status/--stat`, `log --format=`, `rev-parse`, `ls-files`, `ls-tree`). Scripts, husky/lint-staged, changesets, IDE integrations and Turborepo parse these.
- Keep the **index** (staging) as a git user expects. Implement it as a *staged tree ID* plus a working-copy snapshot, not a binary index file: `add -p` edits the staged tree, and `commit` turns the staged tree into a changeset.
- Abbreviated hex IDs for display. Because asc IDs are BLAKE3, show a short prefix. `asc rev-parse --git <rev>` gives the derived git SHA-1 for tools that need it.
- `asc githelp`-style translation for the few places semantics differ (Sapling idea), and explicit errors for unsupported options.
- Extras behind new commands only: `asc undo`, `asc absorb`, `asc workspace`, `asc sparse` (cone profiles per Turborepo package).
- A **git bridge** (derived git objects plus smart-HTTP read) lets any leftover tool `git clone` from the asc server during migration.

---

## 14. Best ideas, ranked by value for ASC

1. **Shared per-machine daemon cache plus virtual worktrees** (Scion daemon, Sapling shared cache, EdenFS overlay). This solves the 58 GB `.git` and 50-worktree disk problem directly. A worktree costs its modified files.
2. **Server-side diff and merge-base RPCs from changeset metadata** (Scion API-first). Turbo affected-package detection and Forgejo PR diffs need no checkout.
3. **Git-SHA-1 and size precomputed in tree entries** (Mononoke content metadata, EdenFS hashes without reads). Turbo hashes without hydration and today's remote-cache keys stay valid.
4. **Immutable content-addressed store on S3 or a local dir, plus CAS refs** (Scion, Mononoke blob-vs-SQL split, S3 conditional writes). The same code runs local-only, serverless-on-bucket or client/server. Any HTTP cache becomes a tier.
5. **Merkle content trees with hash pruning** (fsnodes). Checkout, diff, merge and sparse fetch all cost O(change).
6. **Want-driven, resumable, per-object fetch with the same RPC at every tier** (Scion, GVFS cache servers). No packfiles and no repo maintenance.
7. **FastCDC chunks with BLAKE3/Bao verified ranges** replace LFS. That removes the 7.6 GB double storage and lets large files hydrate partially.
8. **Journal-based status and a `changed-since` API** (EdenFS and Watchman). Status is O(changes) and the watchers stop crawling.
9. **Redirections for `node_modules`, `.turbo`, `dist` and `target`** (EdenFS). They keep write-heavy and crawl-heavy directories off the VFS.
10. **Derived-data framework with the warm-ref rule** (Mononoke). Blame, history, git export and inferred copies are added later without touching the write path.
11. **Server-side pushrebase with path-level conflicts** (Mononoke, Scion merge queue). No push races on trunk.
12. **First-class conflicts via jj `Merge<T>` and the op-log for undo** (jj). Agents and humans can defer conflict resolution, and every local mutation is undoable.
13. **Workspace sync** (CitC, commit cloud). Agent worktrees can move between laptop and CI without a push.
14. **Non-virtual reflink fallback plus cone-only sparse profiles** (Scalar). A safety net when FUSE or ProjFS is unavailable.

### Things to avoid (summary)
- Building on GPL code (Sapling/Mononoke/EdenFS) unless the component is meant to be GPL.
- Building the core on jj-core (model mismatch, pre-1.0, MSRV).
- Patch theory (Pijul).
- Packfiles or delta chains on the wire.
- Per-object serial fetches.
- Unbounded local stores (EdenFS RocksDB).
- Trusting ProjFS notifications as exact.
- macOS via NFS first.
- Path-dependent tree IDs.
- LFS pointers.
- A staging-less UX.
- Kernel-extension dependence (VFS for Git on macOS).

---

## Sources

- Anthropic, "Redesigning source control" (Scion): https://www.anthropic.com/research/redesigning-source-control
- Sapling repo (GPL-2.0), docs read from a sparse clone of https://github.com/facebook/sapling:
  - Mononoke: `eden/mononoke/docs/{1.1..6.4}*.md`, especially 2.1 Bonsai, 2.3 Derived Data, 2.4 Storage, 3.1 Services, 4.1 Pushrebase, 5.1 Git Support; `eden/mononoke/blobstore/packblob/README.md`
  - EdenFS: `eden/fs/docs/{Overview,Data_Model,Inodes,Glossary,Caching,Windows,WindowsFsck,macOS,Redirections}.md`, `slides/Checkout.md`
  - Sapling: `website/docs/dev/internals/{indexedlog,metalog,zstdelta,linelog,visibility-and-mutation}.md`, `website/docs/scale/axes.md`, `website/docs/introduction/differences-git.md`
- Braindetox, "Scion vs Mononoke derived data" (secondary): https://braindetox.kr/en/posts/anthropic_scion_mononoke_derived_data_critique_2026.html
- Jujutsu (Apache-2.0), https://github.com/jj-vcs/jj: `docs/technical/{architecture,concurrency,conflicts}.md`, `core/src/backend.rs`, `lib/src/working_copy.rs`
- Turborepo, https://github.com/vercel/turborepo: `crates/turborepo-scm/src/{lib,git,repo_index,ls_tree,status}.rs`
- Microsoft ProjFS: https://learn.microsoft.com/en-us/windows/win32/projfs/cache-state ; https://learn.microsoft.com/en-us/windows/win32/projfs/file-system-operation-notifications ; https://learn.microsoft.com/en-us/windows/win32/projfs/projected-file-system
- VFS for Git protocol: https://github.com/microsoft/VFSForGit/blob/master/Protocol.md ; FAQ: https://github.com/microsoft/VFSForGit/blob/3790eb06e14d88d71af11958414b59fd3cb2e65d/docs/faq.md
- GitHub, "The Story of Scalar": https://github.blog/open-source/git/the-story-of-scalar/
- Huntress, "Inside the Windows ProjFS": https://huntress.com/blog/windows-projected-file-system-mechanics
- Resilio, progressive hydration and AV behaviour: https://docs.resilio.com/content/advanced-configuration/agents/file_streaming__progressive_hydration_in_tss/
- Potvin and Levenberg, "Why Google Stores Billions of Lines of Code in a Single Repository", CACM 2016: https://cacm.acm.org/research/why-google-stores-billions-of-lines-of-code-in-a-single-repository/ ; Piper summary: https://en.wikipedia.org/wiki/Piper_(source_control_system)
- Pijul: https://pijul.com ; https://pijul.org/posts/2022-01-07-monorepos
- git merge-ort performance series (Elijah Newren), e.g. "merge-ort: skip rename detection entirely if possible": https://patchwork.kernel.org/comment/23882643
- FastCDC: USENIX ATC'16 https://www.usenix.org/conference/atc16/technical-sessions/presentation/xia ; IEEE TPDS 2020 https://ranger.uta.edu/~jiang/publication/Journals/2020/2020-IEEE-TPDS(Wen%20Xia).pdf
- BLAKE3/Bao/iroh: https://iroh.computer/blog/blob-store-design-challenges ; https://iroh.computer/blog/blake3-hazmat-api ; https://docs.rs/bao-tree
- S3 conditional writes: https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.md ; https://aws.amazon.com/about-aws/whats-new/2024/08/amazon-s3-conditional-writes ; MinIO: https://docs.min.io/aistor/developers/s3-api-compatibility/ ; OVH: https://docs.ovhcloud.com/es/guides/storage-and-backup/object-storage/s3-conditional-writes.md

Open items to verify: S3 conditional-write support in R2, Garage and Ceph RGW; ProjFS (`PrjFlt`) on Dev Drive volumes and Defender performance-mode behaviour; FSKit maturity for macOS; whether Turborepo upstream would accept an `SCM` extension point.
