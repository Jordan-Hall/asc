# asc: final specification

Status: **authoritative**, v1.0, 2026-10-22. This document supersedes `docs/designs/*.md`. Where it is silent, the design documents are background only; they are not normative.

Inputs: `docs/designs/{simplicity,integration,performance}.md`, `docs/research/{asc-profile,forgejo,prior-art,rust-stack,turbo-ci}.md`, and the three judge reports. Base design: *simplicity* (two of three judges ranked it first). Grafts are marked **[graft: source]**. Fixes for judge-reported flaws are marked **[fix Jn]** and listed in Appendix A.

Conventions: MUST/MUST NOT are requirements. "ASC" means the `asc/dds` monorepo at `D:\dev\monorepo` and its Forgejo at `https://forge.example.org`. Sizes are binary (KiB, MiB, GiB).

---

## 0. The design on one page

**asc** is a source control system modelled on Scion: Bonsai-style changesets, content-addressed immutable objects in an S3-compatible or local object store, refs as compare-and-swap rows, derived data computed from parents, a server that does history, diffs, merges and landing, and clients that fetch only what they touch.

**Deployables**

| Deployable | What it is |
|---|---|
| `asc` | One Rust multi-call binary: CLI, `asc daemon`, `asc serve`, and, by argv[0], `git-remote-asc` and `asc-fsmonitor`. Cargo features `serve`, `vfs-projfs`, `vfs-fuse` trim builds. |
| Forgejo fork | Forgejo v16.0.6 plus an ordered patch stack (seams, asc client, hooks, bridge). Go, separate repo, GPLv3. Talks to `asc serve` only over gRPC. |
| `asc/asc-actions` | Composite actions `checkout` and `affected` that call `asc ci …`. |

**Ten decisions**

1. **A remote is a store or a server.** `Objects` (immutable, content-addressed) and `Refs` (CAS). One `Engine` runs over the machine store, an S3/local bucket, or a gRPC client. Local-only, serverless-bucket, git-remote and client/server modes share one code path.
2. **Identity is asc's.** BLAKE3 ids, typed per kind. Git SHA-1s are deterministic derived data. Import is lossless and canonical, so a commit exported from asc and re-imported has the same changeset id **[fix: determinism across modes]**.
3. **The changeset hashes its change list and its root tree.** Validated as `apply(parents[0].root, changes) == root` and `changes == leafdiff(parents[0].root, root)`. The v1 tree format has no size limit and is frozen. A tree-format change would be a history rewrite and is out of scope; this trade is accepted and documented **[fix J3: identity coupling]**.
4. **Tree entries carry the git blob/tree SHA-1 and size.** Export, the projection index, turbo hashing and index-vs-HEAD are metadata-only. Servers recompute and verify every client-supplied git field **[graft: integration] [fix J1]**.
5. **The machine cache is a directory.** Atomic rename, content-addressed names, BLAKE3 verified on every read **[graft: integration]**. `store/` (never evicted: unpushed and local-only data) is separate from `cache/` (evictable copies of remote objects).
6. **Every working copy has a real `.git` projection; the git index is asc's staging area.** Git, turbo, IDEs and husky work unchanged. Commits made by `git commit` (IDE, lint tools) are imported, not refused **[graft: judge 2]**. Unimplemented read-only git commands are delegated to git **[graft: integration D11]**.
7. **Lazy materialisation.** ProjFS (Windows) and FUSE (Linux) virtual working copies; plain mode (reflink/copy) everywhere else and as the CI default. Switch updates placeholders from metadata and downloads nothing. Placeholder timestamps are per-placeholder switch-epoch times that only move forward **[fix J1/J2/J3]**.
8. **Server-side history, API-first CI.** `Diff`, `MergeBase`, `Log`, `Blame` run on the server with no checkout. CI plan jobs need one RPC.
9. **Staged, reversible Forgejo cutover.** Stage 0 git-remote mode (no server) → Stage 1 mirror mode (no fork) → Stage 2 asc is truth, Forgejo reads a git bridge → Stage 3 Forgejo reads through asc (Seam A) → Stage 4 Forgejo writes through asc (Seam B, full replacement). `storage_backend` and `object_format_name` are independent switches; git SHAs stay visible until a separate, optional ID flip **[graft: integration D2/D9]**.
10. **One SQL transaction moves refs.** Ref CAS, ref log, idempotency record and outbox row (bridge promotion, post-receive) commit together; the outbox delivers at least once **[graft: integration] [fix J1]**.

**Not built in v1** (each is additive, gated on measurement): segment packing of small objects, zstd dictionaries, skip lists, a path-log table, regional cache tier, SSH `asc-authenticate`, jj-style first-class conflicts, workspace sync, macOS NFS/FSKit, `node_modules` snapshots, server-run `turbo ls`, daemon-side turbo cache tier, write-behind uploads, bootstrap snapshots, `WatchRefs`.

---

## 1. Requirements traceability

| Fixed requirement | Where met |
|---|---|
| New changeset model, content-addressed; git derived | §2, §5 (`gitcommit`), §14 |
| Full storage replacement in Forgejo | §13 stages 2–4 |
| Rust client, daemon, server | §11, §9, §8 (one binary) |
| `asc` mirrors git commands and flags | §11 |
| S3-compatible and local storage; local-only and client/server | §3, §6 |
| Lazy materialisation on checkout/switch/branch | §10 |
| Diffs server-side or from metadata | §4.3, §7 `Diff` |
| Cheap worktrees, shared object cache | §3.5, §9.3 |
| Faster Turborepo CI | §12 |

ASC workload (from `asc-profile.md`): 4,543 files / 119 MiB at HEAD, 5,502 commits, 104,800 reachable objects (311 MiB packed), 113 historical LFS objects (25.6 GiB), one submodule `packages/soma/sdk`, 50 worktrees, 26 commits/day mean (303 peak), median 4 files per commit.

**Targets**

| Operation | Target (ASC, warm machine cache) |
|---|---|
| `asc clone` (virtual) | < 2 s, < 5 MiB downloaded |
| `asc worktree add` | < 1 s, < 1 MiB written before builds |
| `asc status` clean, virtual | < 50 ms |
| `asc switch` between trunk tips | O(changed paths), 0 file downloads |
| CI plan job | < 10 s, no checkout |
| CI plain checkout from warm host cache | < 3 s |
| Turbo task hashes on clean virtual copy | equal to a git clone, 0 hydrations beyond manifests |
| Per-machine footprint replacing 58 GB `.git` | < 2 GiB plus hydrated content |

---

## 2. Data model (crate `asc-core`: pure, no I/O, no async)

### 2.1 Identifiers

All ids are 32-byte BLAKE3 outputs using **derive-key mode** with a per-kind context string:

| Type | Context string | Hashed bytes |
|---|---|---|
| `FileId` | `"asc v1 file"` | the raw file content (whole file, any size) |
| `ChunkId` | `"asc v1 chunk"` | the raw chunk bytes |
| `TreeId` | `"asc v1 tree"` | the canonical envelope of a `Tree` (§2.2) |
| `ChangesetId` | `"asc v1 changeset"` | the canonical envelope of a `Changeset` |
| `TagId` | `"asc v1 tag"` | the canonical envelope of a `TagRecord` |

`id = blake3::Hasher::new_derive_key(CONTEXT).update(bytes).finalize()`.

- Each is a distinct Rust newtype over `[u8; 32]` with no shared base type. `GitOid([u8; 20])` is a separate newtype and is never an identity inside asc.
- Display: 64 lowercase hex; abbreviated display is 12 hex. Input accepts full hex or a unique prefix of ≥ 4 hex characters. A 40-hex string is a git SHA. A prefix that matches both an asc id and a git SHA is an error that lists candidates. Explicit prefixes `asc:` and `git:` force one interpretation.
- The empty tree has a fixed `TreeId` = hash of the envelope of `Tree { entries: [] }`; golden value in `crates/asc-core/tests/golden/ids.txt`.

### 2.2 Canonical encoding

**Envelope:** `b"asc"` (0x63 0x67) ‖ `kind: u8` ‖ `version: u8` ‖ `postcard(payload)`.

| kind | payload | version |
|---|---|---|
| 1 | `Tree` | 1 |
| 2 | `Changeset` | 1 |
| 3 | `TagRecord` | 1 |
| 4 | `ChunkList` | 1 |
| 5 | `FileMeta` (server-side only) | 1 |
| 16–63 | derived values (§5) | per type |

Postcard 1.x rules (normative, golden-tested): unsigned ints are LEB128 varints, signed ints are zigzag varints, `u8` is one raw byte, `bool` is one byte 0/1, `Option` is a tag byte 0/1 then the value, `Vec<T>`/`Vec<u8>` is a varint length then elements, `[u8; N]` is N raw bytes with no length, enums are a varint variant index then fields, structs are fields in declaration order. Forbidden in encoded types: `HashMap`, `f32/f64`, `String` (use `Vec<u8>` except where noted), `#[serde(skip_serializing_if)]`, `flatten`, untagged enums. Maps are `Vec<(K, V)>` sorted strictly ascending by key bytes (no duplicates); decoders MUST reject unsorted or duplicate keys, trailing bytes, unknown kinds and unknown versions. Therefore `encode(decode(b)) == b` for every accepted `b`.

### 2.3 Names and paths

- `Name`: non-empty `Vec<u8>`; MUST NOT contain `/` or NUL; MUST NOT be `.` or `..`; MUST NOT equal `.git` under ASCII case folding. No other restriction at the data-model level (history may contain anything git allowed).
- `RepoPath`: one or more `Name`s joined by `/`. No leading or trailing `/`. Ordering is by raw bytes.
- `casefold(name)` = Unicode simple case folding of the UTF-8 decode, or ASCII lowercase of raw bytes when not valid UTF-8.
- `windows_valid(name)` is false when the name: ends in `.` or space; contains any of `< > : " \ | ? *` or bytes 0x01–0x1F; or its stem before the first `.` is (case-insensitively) `CON PRN AUX NUL COM1–COM9 LPT1–LPT9`.
- `checkout_safe(path)` applies git's `verify_path` rules with `core.protectNTFS=true` and `core.protectHFS=true` (rejects `.git` variants such as `GIT~1`, `.git::$INDEX_ALLOCATION`, `.git` with HFS-ignorable code points).

Where each rule applies is in §2.8.

### 2.4 File content

- A file's identity is `FileId` over its whole content. Chunking never affects identity.
- **Inline files** (size ≤ 4 MiB = 4,194,304 bytes) are one object.
- **Chunked files** (size > 4 MiB) are a `ChunkList` plus chunks. FastCDC v2020 from `fastcdc` 5.0.0, min 512 KiB, avg 1 MiB, max 4 MiB, `Normalization::Level1`.

```rust
pub struct ChunkList { pub size: u64, pub chunks: Vec<(ChunkId, u32)> } // u32 = chunk length; sum == size
```

`ChunkList` is not identity-bearing: it is stored under the file's `FileId`, and a reader accepts any `ChunkList` whose assembled bytes hash to that `FileId`. A reader verifies each chunk against its `ChunkId` while streaming, and the whole file against `FileId` at the end. On mismatch it discards the result and reports corruption.

- Bytes are stored exactly as committed. asc performs no EOL or filter conversion. Projections set `core.autocrlf=false` (§9.4).

### 2.5 Trees

```rust
pub struct Tree { pub entries: Vec<Entry> }       // strictly ascending by name bytes; root may be empty, others not
pub struct Entry {
    pub name: Name,
    pub kind: Kind,
    pub id: [u8; 32],          // File/Exec/Symlink: FileId; Dir: TreeId; Gitlink: all zero
    pub size: u64,             // File/Exec/Symlink: byte length of the git blob; Dir: sum of descendant blob sizes; Gitlink: 0
    pub git: GitOid,           // git blob oid / git tree oid / gitlink commit oid
    pub lfs: Option<LfsRef>,   // Some only for File/Exec entries imported as git-LFS pointers
}
pub enum Kind { File = 0, Exec = 1, Symlink = 2, Dir = 3, Gitlink = 4 }   // postcard variant index as listed
pub struct LfsRef { pub sha256: [u8; 32], pub size: u64 }                  // resolved content hash and size
```

- A tree depends only on content, never on path or history.
- `git` of a `Dir` entry is the git oid of that subtree **as git saw it**. For canonical trees this equals `sha1(render_git_tree(child))`. For imported non-canonical git trees (unusual modes, odd ordering) it is the original oid, and the original raw git tree object is stored as a **git tree override** object `o/<oid>` (§3.3). The exporter uses the override whenever `sha1(render) != git`.
- The git oid of the root tree is not stored in `Tree`. It is `sha1(render_git_tree(root))`, except for imported commits that keep raw bytes (§2.9), whose tree line is authoritative and whose root override is stored like any other.
- **LFS entries [fix J1: pointer identity]:** `id` is the `FileId` of the **pointer blob bytes**, `size` is the pointer blob's size, and `git` is the pointer blob's oid, exactly as git stored them. `lfs.size`/`lfs.sha256` describe the real content. Working copies show the real content (size `lfs.size`), resolved via the `x/sha256/<hex>` alias (§3.3). The projection index records `git` (the pointer oid) with stat size `lfs.size`, which is exactly what a git+LFS clone records. Import is deterministic whether or not LFS bytes are available.
- Large files committed natively are chunked content (§2.4). asc never creates new LFS pointers.

### 2.6 Changesets

```rust
pub struct Changeset {
    pub parents: Vec<ChangesetId>,               // first parent first; empty for a root changeset
    pub root: TreeId,
    pub author: Signature,
    pub committer: Signature,
    pub message: Vec<u8>,                        // raw bytes
    pub extra: Vec<(Vec<u8>, Vec<u8>)>,          // sorted map; keys are ASCII, namespaced (below)
    pub changes: Vec<(RepoPath, Change)>,        // sorted map; relative to parents[0] (or the empty tree)
}
pub struct Signature { pub name: Vec<u8>, pub email: Vec<u8>, pub time: i64 /*unix s*/, pub tz_min: i16 }
pub enum Change {
    Write { kind: Kind /* File|Exec|Symlink */, file: FileId, size: u64, git: GitOid,
            lfs: Option<LfsRef>, copy_from: Option<RepoPath> },
    Gitlink { commit: GitOid },
    Delete,
}
```

**Validation (MUST, on every ingest: local commit, push, import, server mutation):**

1. Every parent exists; `parents` has no duplicates.
2. `apply(tree(parents[0]) or empty, changes) == root`, where `apply` builds the new tree along changed spines (§4.1).
3. `changes == leafdiff(tree(parents[0]) or empty, root)` exactly: every changed leaf appears once, nothing unchanged appears, and a file replaced by a directory (or the reverse) appears as `Delete` of the old leaf plus `Write`s of the new leaves. Therefore `changes` is a pure function of the two roots plus `copy_from` annotations, and two producers of the same commit always agree.
4. Every `Write.file` exists, and its `size`, `git` (blob SHA-1) and `lfs` match the content (§8.4 verifies on the server).
5. `copy_from`, if present, names a path that exists in `tree(parents[0])`.
6. Merge changesets record changes relative to `parents[0]` only (Scion/Bonsai).

**`extra` keys** (others are rejected):

| Key | Value | Meaning |
|---|---|---|
| `git.commit` | raw git commit object body | Imported commit that is not canonical (§2.9). Export emits these bytes verbatim. |
| `git.gpgsig` | signature bytes (armored) | Signature over the canonical git commit bytes without the `gpgsig` header. Rendered as the `gpgsig` header. |
| `asc.snapshot` | empty | Snapshot changeset (stash); refused on protected refs. |

### 2.7 Tags and refs

```rust
pub struct TagRecord { pub target: ChangesetId, pub name: Vec<u8>, pub tagger: Option<Signature>,
                       pub message: Vec<u8>, pub extra: Vec<(Vec<u8>, Vec<u8>)> } // extra: "git.tag" raw, "git.gpgsig"
pub enum RefTarget { Changeset(ChangesetId), Tag(TagId) }   // stored as 33 bytes: 0x01|0x02 ‖ id
```

Ref names follow `git check-ref-format`. Namespaces: `refs/heads/*`, `refs/tags/*`, `refs/remotes/*` (local only), `refs/stash` (local only), `refs/pull/<n>/head` and `refs/for/*` (Forgejo), `refs/heads/agent/*` (may carry an expiry, §8.8).

### 2.8 Path rules by context

| Context | Rule |
|---|---|
| Decoding any object | `Name`/`RepoPath` validity (§2.3) |
| Importing git history | Nothing more. History is never rejected for Windows names or case collisions **[fix: missing-from-all]** |
| New native changesets (local commit, push, server mutation) in a repo with `windows_safe = true` (default) | For each path **added or changed** by the changeset: `windows_valid` for every component, `checkout_safe`, and no case-fold collision with a sibling in the resulting tree. Pre-existing violations elsewhere are not re-checked. |
| Materialising (VFS, plain) | `checkout_safe` always. On Windows, entries that are not `windows_valid`, or that case-collide with an earlier sibling (in byte order), are not projected. They are listed by `asc status` under "unrepresentable on this OS", keep their index entries with the skip-worktree bit, and are never reported as deleted. |

### 2.9 Git mapping (`asc-core::git`)

Git object rendering is exact and golden-tested against `git hash-object`, `git mktree` and `git commit-tree`.

- **Blob:** `sha1("blob " ‖ ascii(len) ‖ 0x00 ‖ bytes)`.
- **Tree render:** entries sorted by git order (name bytes, with directories compared as `name ‖ "/"`). Each entry is `mode ‖ " " ‖ name ‖ 0x00 ‖ oid[20]`, with modes `100644` File, `100755` Exec, `120000` Symlink, `40000` Dir, `160000` Gitlink. Header `tree <len>\0`.
- **Commit render (canonical):**

```
tree <root git oid hex>\n
parent <gitcommit(p) hex>\n                       (one per parent, in order)
author <name> <<email>> <time> <tz>\n             (tz = sign ‖ HH ‖ MM from tz_min, e.g. +0100, -0530, +0000)
committer <name> <<email>> <time> <tz>\n
asc <base64(postcard(AscGitExtra))>\n               (only when AscGitExtra is non-empty)
gpgsig <line 1>\n <line 2>\n …                    (only when extra has git.gpgsig; continuation lines start with one space)
\n
<message bytes verbatim>
```

```rust
pub struct AscGitExtra { pub copies: Vec<(RepoPath, RepoPath)>, pub snapshot: bool } // (path, copy_from), sorted
```

  The `asc` header carries asc-only data through git, so a native changeset exported to git and re-imported has the same id.

- **Import canonicalisation (deterministic, used by every importer):** parse the raw commit into tree, parents, author, committer, optional `asc` header, optional `gpgsig`, and message. Build the canonical changeset (with `git.gpgsig` and the copies from the `asc` header). If `render(canonical) == raw`, store it **without** `git.commit`. Otherwise store `extra["git.commit"] = raw`. Tags work the same way with `git.tag`. Trees: if `render(tree) != raw`, store the override.
- **`gitcommit(cs)`** (derived, §5): `sha1` of `extra["git.commit"]` if present, otherwise of the canonical render.
- **Signing:** `asc commit -S` renders the canonical commit without `gpgsig`, signs it with git's configured program (`gpg.format`, `gpg.program`, `gpg.ssh.program`, `user.signingkey`), and stores `git.gpgsig`. Forgejo and git verify these signatures unchanged.

---

## 3. Storage (crate `asc-store`)

### 3.1 Keys

```rust
pub enum ObjKey {
    File(FileId),            // f/   inline file bytes (size ≤ 4 MiB)
    ChunkList(FileId),       // l/   ChunkList envelope (size > 4 MiB)
    Chunk(ChunkId),          // k/
    Tree(TreeId),            // t/
    Changeset(ChangesetId),  // c/
    Tag(TagId),              // g/
    GitTreeOverride(GitOid), // o/   raw git tree object, "tree <len>\0…"
    Derived { name: &'static str, version: u32, id: [u8; 32] },   // d/<name>.v<version>/
    Sha256Alias([u8; 32]),   // x/sha256/   value = FileId (32 raw bytes)
}
```

`File` vs `ChunkList` is decided by size, which every referrer knows from its `Entry`/`Change`.

**Stored value** (bucket and wire): `codec: u8 ‖ data`, codec `0` = raw and `1` = one zstd frame (level 3, content size set). A writer uses zstd only if the frame is < 90% of raw. Readers accept both. The stored bytes are the wire bytes; only the consumer decodes.

### 3.2 Traits

```rust
#[async_trait]
pub trait Objects: Send + Sync {
    async fn get(&self, key: &ObjKey) -> Result<Option<Bytes>>;                  // decoded data (codec removed), verified
    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> Result<Option<Bytes>>; // File/Chunk only
    async fn put(&self, key: &ObjKey, data: Bytes) -> Result<()>;               // create-if-absent; AlreadyExists == Ok
    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>>;
    async fn delete(&self, key: &ObjKey) -> Result<()>;                          // GC only
    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, Result<(ObjKey, SystemTime)>>; // GC only
}

#[async_trait]
pub trait Refs: Send + Sync {
    async fn get(&self, repo: RepoKey, name: &RefName) -> Result<Option<(RefTarget, u64 /*version*/)>>;
    async fn list(&self, repo: RepoKey, prefix: &str) -> Result<Vec<(RefName, RefTarget, u64)>>;
    async fn update(&self, repo: RepoKey, txn: &RefTxn) -> Result<(), RefConflict>; // atomic where supported (§3.4)
    async fn log(&self, repo: RepoKey, name: &RefName, limit: u32) -> Result<Vec<RefLogEntry>>;
}
pub struct RefTxn { pub updates: Vec<RefUpdate>, pub actor: Actor, pub reason: String, pub op_id: Uuid }
pub struct RefUpdate { pub name: RefName, pub expect: Expect, pub new: Option<RefTarget> /* None = delete */ }
pub enum Expect { Any, Absent, Value(RefTarget) }
```

**Verify on read (MUST):** `get` verifies `File`, `Chunk`, `Tree`, `Changeset` and `Tag` values against their id before returning. A mismatch is `Error::Corrupt(key)`. The caller treats the object as missing, and caches delete the file **[graft: integration] [fix J1: cache poisoning]**.

### 3.3 Implementations

| Impl | Backing | Notes |
|---|---|---|
| `BucketObjects` | `object_store` 0.14.2: `AmazonS3` (S3, MinIO, R2, Garage, Ceph RGW), `LocalFileSystem`, `InMemory` | Stores codec-prefixed values. `put` uses `PutMode::Create`. `AlreadyExists` / 412 / 409 → `Ok`. On `NotImplemented`, 501, or a 400 that names the conditional header, the store remembers `create_unsupported` and uses `PutMode::Overwrite` from then on. That is safe because keys are content-addressed and immutable **[fix: missing-from-all]**. Multipart for values > 32 MiB. |
| `CacheDir` | plain files | Machine cache and machine store (§3.5). Stores **decoded** data so content files can be copied, reflinked or passed through. |
| `Tiered<A, B>` | read A, on miss read B, then fill A; writes go to A | Client view of a repo: `Tiered<StoreSet, Remote>` |
| `RemoteObjects` | gRPC `GetObjects`/`PutObjects`/`HasObjects` | In `asc-rpc` |
| `SqlRefs` | `sqlx` 0.9 `AnyPool` over SQLite or Postgres | Atomic multi-ref CAS in one transaction (§3.4) |
| `BucketRefs` | refs inside a bucket, using create-only puts | Single-ref atomic only; enabled only after the probe passes (§3.4) |

**Bucket layout** (one namespace per repository **[fix: missing-from-all, cross-fork reads]**):

```
<root>/ns/<ns>/f/<h2>/<hex64>          inline file
<root>/ns/<ns>/l/<h2>/<hex64>          chunk list
<root>/ns/<ns>/k/<h2>/<hex64>          chunk
<root>/ns/<ns>/t/<h2>/<hex64>          tree
<root>/ns/<ns>/c/<h2>/<hex64>          changeset
<root>/ns/<ns>/g/<h2>/<hex64>          tag
<root>/ns/<ns>/o/<h2>/<hex40>          git tree override
<root>/ns/<ns>/d/<name>.v<N>/<h2>/<hex64>   derived (recomputable)
<root>/ns/<ns>/x/sha256/<hex64>        alias → FileId
<root>/ns/<ns>/rl/<refname>/<v:020>    ref log entries (BucketRefs only)
<root>/ns/<ns>/r/<refname>             ref hint (BucketRefs only)
<root>/turbo/<repo-slug>/<scope>/<hash>[.meta]   turbo remote cache (§12.4)
```

`<h2>` is the first two hex characters. `<ns>` is a 16-byte random id (hex) assigned at repo creation. A bucket remote URL (`s3://…`, `file://…`) addresses one namespace directly: its URL prefix plays the role of `<root>/ns/<ns>`. There are no packfiles. Every object can be fetched alone, so transfers resume by asking again for whatever is missing.

**Namespaces and forks.** Objects are only ever read within the namespace of the repository named in the request. A Forgejo fork gets a new namespace. `Fork` copies the reachable closure server-side, using S3 `CopyObject` or a local reflink, as a background job. The fork is readable when the copy completes (minutes for ASC). Cross-namespace operations (PR merges from forks, `refs/pull/<n>/head`) first call `CopyClosure` (§7), authorised against both repositories. This closes the confirmation attack ("does this content exist in a private fork?").

### 3.4 Ref stores and their atomicity

**`SqlRefs`** (server; and machine-local refs in `state.db`):

```sql
CREATE TABLE refs (repo_id BIGINT NOT NULL, name TEXT NOT NULL, target BLOB/BYTEA NOT NULL, -- 33 bytes
                   version BIGINT NOT NULL, expires_at BIGINT NULL,                       -- unix s
                   PRIMARY KEY (repo_id, name));
CREATE TABLE ref_log (seq BIGINT PRIMARY KEY /*autoinc*/, repo_id BIGINT, name TEXT, old BLOB NULL, new BLOB NULL,
                      actor TEXT, via TEXT, op_id BLOB, at BIGINT);
```

Update: in one transaction, for each update, `UPDATE refs SET target=$new, version=version+1 WHERE repo_id=$r AND name=$n AND version=$v` (or `INSERT` for `Absent`, `DELETE … WHERE version=$v` for deletes). Any zero-row result rolls back and returns `RefConflict { name, actual }`. Server-only callers append more statements to the same transaction (§8.3).

**`BucketRefs`** (serverless shared bucket) **[fix J1/J2: serverless multi-writer, ABA, reflog]**:

- The **ref log is the truth.** Entry `rl/<ref>/<v>` (v zero-padded to 20 digits, starting at 1) holds `postcard(BucketRefEntry { old: Option<RefTarget>, new: Option<RefTarget>, op_id: [u8;16], actor: Vec<u8>, at: i64 })`. The current value is the entry with the highest `v`.
- **CAS = create-only put** of `rl/<ref>/<v+1>` after reading entry `v` and checking that `entry_v.new == expected`. Only one writer can create a given `v+1`. On `AlreadyExists`, read the entry: if its `op_id` equals mine, the earlier attempt succeeded (a retried create), so return success. Otherwise return `RefConflict`. Version numbers never repeat, so there is no ABA.
- `r/<ref>` holds the latest known `v` as a **hint**, written with a plain overwrite after a successful CAS. Readers start at the hint and probe `v+1, v+2, …` until a missing key. Listing refs lists `r/` and then resolves each one.
- Atomicity covers one ref per update. A multi-ref push applies refs in order and reports partial success explicitly.
- **Probe** (`asc doctor --store`, and automatically on `asc init --store s3://…` and on the first push to a bucket remote): put key `probe/<uuid>` with `Create` twice. The second put MUST fail with `AlreadyExists`. If it succeeds (header ignored) or the store errors with `NotImplemented`, `BucketRefs` is refused for that bucket with a message naming the store. The bucket can still hold objects, and refs then need a server or the single-writer mode below. Garage v2 is expected to fail the probe.
- **Single-writer fallback:** `asc init --store <bucket> --refs local` keeps refs in the machine's `state.db`. It is explicitly single-machine and is labelled as such in `asc remote -v`.

### 3.5 Machine layout (`$ASC_HOME`)

`$ASC_HOME` = env `ASC_HOME`, else global git config `asc.home`, else `%LOCALAPPDATA%\asc` (Windows) or `${XDG_DATA_HOME:-~/.local/share}/asc`. ASC sets `D:\asc`, on the same volume as bun's cache.

```
cache/{f,l,k,t,c,g,o,d}/<h2>/<hex>   evictable decoded copies of remote objects (CacheDir)
store/{f,l,k,t,c,g,o,d}/<h2>/<hex>   objects that exist only here: local-only repos, unpushed commits, staged content. Never evicted.
tmp/                                  staging for atomic writes (same volume)
state.db                              SQLite: repos, local refs, ref log, workspaces, journal, git map, entry times, intents
repos/<repo-id>/.git/                 projection common dir per repo (objects, refs mirror, packed-refs, config, logs)
wc/<ws-id>/git/                       per-workspace gitdir (HEAD, index, config.worktree, commondir, logs/HEAD)
wc/<ws-id>/overlay/                   FUSE only: modified file bodies
redirect/<ws-id>/<path>               FUSE only: redirected directories
hooks/                                asc-owned git hook dispatchers
tools/                                pinned helper tools (e.g. turbo for `asc ci affected`)
run/                                  daemon pid, Unix socket fallback
log/daemon.log
```

- **Atomic write:** write to `tmp/<uuid>`, fsync, then rename to the final path. If the final path exists, delete the temp file and succeed. Readers never see partial files. Files are opened for read with `FILE_SHARE_DELETE` on Windows, so eviction never blocks readers.
- **Content files** (`f/`, `k/`) hold raw bytes. A chunked file also gets an assembled `f/` copy when it is first hydrated, if it is ≤ 1 GiB.
- **LRU** uses file mtime. A read touches mtime when it is older than 24 h. The daemon (or `asc gc`) evicts `cache/` files with the oldest mtime once the cache exceeds `asc.cache.maxSize` (default 20 GiB), down to 80% of that.
- **Demotion:** after a successful push, pushed objects are renamed from `store/` to `cache/`.
- **Store set:** `StoreSet` reads `store/` then `cache/`, and writes new local objects to `store/`.

### 3.6 Garbage collection (server and bucket; unit O01, not needed for ASC v1)

- Objects are never deleted on the write path.
- **Two-mark rule [fix: missing-from-all, concurrent writers]:** a sweep deletes an object only if it was unreachable in two marks taken at least 24 h apart, and its `LastModified` is older than 14 days. Roots for each mark are: all refs; every changeset object whose `LastModified` is within 14 days; retained snapshot refs; turbo objects (by their own retention). A push in progress during mark 1 that re-references an old object uploads its changeset before mark 2, which makes the object reachable again. Pushes taking longer than 24 h are not supported. The client re-uploads on `FAILED_PRECONDITION: missing objects`.
- Server marks are stored in SQL (`gc_marks`). Bucket-mode GC (`asc gc --store <url>`) keeps its marks in the bucket under `gc/<run>/` and refuses to run concurrently, using a `gc/lock` create-only key with a 6 h lease.

---

## 4. Engine (crate `asc-engine`)

`Engine<O: Objects, R: Refs>` is the only implementation of repository logic. It runs inside the CLI (local operations, local-only and bucket remotes) and inside `asc serve`.

### 4.1 Trees (`tree.rs`)

- `apply(root, changes) -> TreeId`: group changes by parent directory, rebuild only the touched spines bottom-up, write new `Tree` objects, prune empty directories, and recompute each new `Entry`'s `size` and `git` (a `Dir`'s `git` = sha1 of its canonical render). Cost is O(changed paths × depth).
- `leafdiff(a, b, pathspec) -> Vec<(RepoPath, Change)>` with **hash pruning**: equal `TreeId`s are skipped. File-to-directory replacement produces a `Delete` plus `Write`s.
- `lookup(root, path)`, `walk(root, pathspec)`, glob matching with `globset`.
- Oracle property test: `apply(a, leafdiff(a, b)) == b`, and pruned diff equals a naive full-walk diff.

### 4.2 Commits and revisions (`commit.rs`, `revspec.rs`)

- `build_changeset(parents, index_entries | changes, sigs, message, extra)`: computes changes as `leafdiff(parent root, new root)`, attaches `copy_from` from rename records, writes the tree, and validates (§2.6).
- Revspec grammar (git subset, all supported): full or abbreviated asc id; full or abbreviated git SHA (through the git map); ref names with git's DWIM order (`<n>`, `refs/<n>`, `refs/tags/<n>`, `refs/heads/<n>`, `refs/remotes/<n>`, `refs/remotes/<n>/HEAD`); `HEAD`, `@`; `<rev>^`, `^N`, `~N`; `<rev>^{tree}`, `^{commit}`, `^{}`; `@{u}`, `@{upstream}`, `<branch>@{u}`; `@{N}` (reflog); `:/<regex>`; `<rev>:<path>`; ranges `A..B`, `A...B`, `^A B`, `<rev>^@`, `<rev>^!`.

### 4.3 Graph and history (`graph.rs`)

- An in-memory graph per repo, `ChangesetId -> (gen: u32, parents)`, where `gen = 1 + max(gen(parents))`. It is loaded lazily: from `cs_graph` on the server, from the `gen` derived value locally.
- `merge_base(a, b)`: a two-frontier walk in descending generation order (git's paint-down algorithm). `is_ancestor(a, b)`: walk from `b`, pruning nodes with `gen < gen(a)`.
- `log(range, pathspec, first_parent, limit, skip)`: generation-ordered walk. The path filter uses each changeset's `changes` keys (prefix match), so it costs no tree reads. `--follow` uses `copy_from`.
- Line diff (`diff/`): `imara-diff` 0.2 Myers with indent heuristic post-processing. Output formats: `--name-only`, `--name-status`, `--stat`, `--numstat`, `--shortstat`, `--raw`, unified patch (`-U<n>`, rename headers, mode lines, `Binary files … differ`, `\ No newline at end of file`), `--word-diff=plain`. **Contract:** every patch passes `git apply --check`, and the patch text equals `git diff` byte for byte on ≥ 99% of the golden corpus (pairs taken from ASC history). Forgejo depends only on the first property **[graft: integration]**.
- Renames: exact (equal `FileId`) first, then similarity among added/deleted files only, capped by `diff.renameLimit` (default 1000). Off unless `-M`, or `diff.renames=true` (git's default for porcelain).

### 4.4 Merge, rebase, land (`merge/`, `rebase.rs`, `land.rs`)

- **Tree merge** of base→ours and base→theirs is the union of two change sets. A path changed on one side takes that side. A path changed identically on both sides takes it. A path changed differently gets a content 3-way merge with the `gix-merge` (pinned to the `gix` 0.89 family) builtin text driver, which produces git's markers in `merge`, `diff3` and `zdiff3` styles. Only both-sides paths read content. File/directory and case-fold conflicts are detected on changed paths only. Binary conflicts take ours and record the conflict.
- **Conflicts** in a working copy: markers in the files, stages 1/2/3 in the index, `MERGE_HEAD`/`MERGE_MSG` (merge) or `CHERRY_PICK_HEAD` (cherry-pick, revert) in the gitdir, git-compatible. Rebase state lives in `state.db` (`ws_ops`).
- **Rebase of changeset `C` onto `T`:** if `paths(C)` is disjoint from `paths(base(C)..T)` (§4.5), the result is `C` with `parents = [T]`, the same `changes`, and `root = apply(T.root, C.changes)` (metadata-only, no content reads) **[graft: performance]**. Otherwise only the intersecting files are merged.
- `cherry_pick`, `revert` and `squash` are built on the same primitives.

### 4.5 Path-set disjointness (shared by land, metadata rebase and merge fast paths)

`paths(base..tip)` = the union of the `changes` keys along the first-parent chain from `tip` back to (excluding) `base`, when `base` is on that chain. Otherwise it is `leafdiff(base.root, tip.root)` keys. This is a superset of the real difference, so it is conservative.

Two path sets **conflict** when some `a ∈ A`, `b ∈ B` satisfy any of: `a == b`; `casefold(a) == casefold(b)`; `a` is a directory prefix of `b` or the reverse (also compared under casefold) **[fix J1/J2: prefix and case conflicts]**. Implementation: insert B's casefolded paths and all of their ancestor directories into a trie, then test each A path, and each ancestor of it, against the trie.

### 4.6 Blame (`blame.rs`)

`blame(rev, path)` walks back from `rev` over changesets whose `changes` touch `path` (following `copy_from`). It line-diffs successive versions and assigns each line its origin. Results are memoised per `(changeset, path)` as derived `blame.v1` (§5). Output: git porcelain, incremental and default formats; `-L` ranges.

---

## 5. Derived data

```rust
#[async_trait]
pub trait Derived: Sized + Send + Sync {
    const NAME: &'static str;   // "gen", "gitcommit", "blame"
    const VERSION: u32;
    const EAGER: bool;          // must exist before any ref may point at the changeset
    async fn derive(e: &EngineCtx, cs: &Changeset, id: ChangesetId, parents: &[Self]) -> Result<Self>;
    fn encode(&self) -> Vec<u8>; fn decode(b: &[u8]) -> Result<Self>;
}
```

- `get::<D>(id)` finds the underived ancestors iteratively (topological order, no recursion), derives the oldest first, and memoises at `d/<NAME>.v<VERSION>/<id>` (and in SQL for the server types below). Derivation is pure, so concurrent derivers write identical bytes.

| Type | Eager | Value | Also written to |
|---|---|---|---|
| `gen.v1` | yes | `{ gen: u32 }` | server `cs_graph`; local `state.db` |
| `gitcommit.v1` | yes | `{ commit: GitOid }` | server `git_map`; local `state.db.git_map` |
| `blame.v1` (per changeset+path; key = blake3(cs ‖ path)) | no | line runs `(start, len, origin ChangesetId, orig_line)` | – |

- **Warm-ref rule:** `UpdateRefs`, local commits and imports derive every eager type for new changesets before a ref moves. Readers never wait.
- On ingest the server also writes the **git map** for every new tree, file and changeset: `(git oid, kind) -> asc id`. Trees and files come from `Entry.git`. Changesets come from `gitcommit`.
- Adding a type means one `impl Derived` plus registration. The write path never changes.

---

## 6. Repositories, remotes and modes

### 6.1 Repository kinds on a machine

Every repository a machine knows has a row in `state.db.repos` (`id`, `name`, `ns`, `remote`, `default_branch`, `windows_safe`). Local branches and tags always live in `state.db.refs` (as git keeps local refs locally). Remote-tracking refs are `refs/remotes/<remote>/*` in the same table.

| Mode | Authoritative objects | Authoritative remote refs | How history ops run |
|---|---|---|---|
| **local-only** (`asc init`) | `store/` | none (local refs are the only refs) | local engine |
| **bucket remote** (`s3://`, `file://`) | the bucket | `BucketRefs` (shared) or local (single-writer) | local engine over `Tiered<StoreSet, BucketObjects>` |
| **git remote** (`git+https://`, or Forgejo without asc) | the git server | the git server | local engine; import on fetch; export + `git push` on push |
| **server remote** (`asc+https://`, or Forgejo with asc discovered) | the server's store | the server's SQL | local engine for local work; server RPCs for `Diff --remote`, blame, long-range log and CI (`asc.serverHistory=auto`) |

### 6.2 Remote URLs and discovery

| URL | Meaning |
|---|---|
| `https://host/owner/repo[.git]` | Discover. Use `remote.<name>.ascServer` if set. Otherwise `GET https://host/.well-known/asc`, which returns `{ "server": "https://asc.host:443", "repos": "*", "mode": {"<owner>/<repo>": "mirror"\|"primary"}, "min_client": "x.y.z" }`. If that is 404, use git-remote mode. |
| `asc+https://host[:port]/owner/repo` | server remote |
| `git+https://…`, `git+ssh://…`, `git@host:path` | git-remote mode, forced |
| `s3://bucket/prefix?endpoint=<url>&region=<r>&path_style=true` | bucket remote. Credentials from `AWS_*` env, or `git credential fill` with `protocol=s3`, `host=<bucket>`. |
| `file:///abs/path` or an absolute path to a bucket-layout dir | bucket remote on the local filesystem |

Before the Forgejo fork serves `/.well-known/asc`, ASC's reverse proxy routes that path to `asc serve`.

### 6.3 Fetch, push and clone (`asc-engine::sync`)

Negotiation follows Scion. There are no packfiles and no negotiation round trips:

- **fetch:** list remote refs. For each changed ref, walk new changesets from the tip until a known changeset, using batched `GetObjects` of `Changeset` objects. Trees and files are not fetched (`--prefetch <pathspec>` warms them). Update remote-tracking refs. Write `gen` and `gitcommit` locally.
- **push:**
  1. compute the closure of new local objects: changesets not on the remote; for each, its new trees (walk `changes` spines), files (and chunk lists, chunks, tags);
  2. `HasObjects` in batches of 4,096;
  3. upload missing objects in dependency order (files and chunks, trees bottom-up, then changesets) with `PutObjects`, using up to 8 concurrent streams of ≤ 16 MiB batches;
  4. update refs (`UpdateRefs` with `expect = Value(remote-tracking value)`, or `Any` with `--force`, or the lease given by `--force-with-lease`);
  5. on success, demote pushed objects from `store/` to `cache/`.

  A retry of an interrupted push re-runs `HasObjects` and so resumes. Nothing is uploaded before `asc push` **[fix J1: no write-behind]**.
- **clone:** register the repo, fetch refs and the head changeset and its root tree, create a workspace (§9). Downloads are O(1) in repository size.
- **Git-remote mode:** fetch = `git fetch` (credential helpers apply) into the projection common dir `repos/<id>/.git` (full objects, about 0.4 GB for ASC, once per machine), then an incremental import of new commits (§14.1) into `cache/`, which is evictable because it is rebuildable from git. Push = export new changesets into the common dir (§14.2), then `git push <url> <sha>:<ref> --force-with-lease=<ref>:<expected sha>`, then update remote-tracking refs. `asc clone <url> --reference <path-to-existing-.git>` first runs `git fetch <path>` (copying reachable objects only, with no alternates link), so the old checkout can be deleted afterwards.
- **Mirror-mode server (Stage 1):** fetch and clone use the server. Push uses the git-remote push path to Forgejo, and the server imports the result (§8.6). `UpdateRefs` from clients is rejected with `FAILED_PRECONDITION: mirror mode`.

### 6.4 Offline behaviour

These work offline: `status`, `add`, `commit`, `diff` of the working copy, `stash`, `branch`, `tag`, `switch` between cached revisions, `merge`/`rebase`/`cherry-pick` when the content is cached, `log`/`blame` over cached history, and reads of cached files. Everything else fails fast with `asc: offline (<remote> unreachable): <operation> needs <RPC>`. A virtual read of uncached content fails with `STATUS_FILE_IS_OFFLINE` (ProjFS) or `EIO` (FUSE), and the daemon log names the path. `asc prefetch <pathspec> [--history N]` warms the cache before going offline.

---

## 7. RPC surface (`proto/asc/v1/repo.proto`, `workspace.proto`)

gRPC over HTTP/2 (`tonic` 0.14), TLS on the network, `interprocess` local sockets for the daemon. The Go fork generates stubs from the same files (`buf generate`). Objects travel as stored bytes (codec byte plus data).

**Common metadata:** `authorization: Bearer <token>`; `x-asc-client: asc/<semver> <os>-<arch>`; `x-asc-op-id: <uuidv7>` on every mutating call (idempotency, §8.3); W3C `traceparent`.

**Errors** use gRPC status codes with an `asc.v1.ErrorDetail { string code; repeated string paths; bytes actual; }` detail. `code` values: `REF_CONFLICT`, `NOT_FAST_FORWARD`, `LAND_CONFLICT`, `LAND_NEEDS_CLIENT_REBASE`, `MISSING_OBJECTS`, `INVALID_OBJECT`, `HOOK_REJECTED`, `MIRROR_MODE`, `CLIENT_TOO_OLD`, `PATH_POLICY`.

```proto
syntax = "proto3";
package asc.v1;
option go_package = "forgejo.org/modules/ascproto/ascv1";

enum IdMode { ID_MODE_ASC = 0; ID_MODE_GIT = 1; }   // GIT: changeset/tree/file ids are git oids (20 bytes) in and out
message RepoRef { string name = 1; IdMode id_mode = 2; }   // name = "owner/repo"

enum ObjKind { OBJ_KIND_UNSPECIFIED = 0; OBJ_KIND_FILE = 1; OBJ_KIND_CHUNK_LIST = 2; OBJ_KIND_CHUNK = 3;
               OBJ_KIND_TREE = 4; OBJ_KIND_CHANGESET = 5; OBJ_KIND_TAG = 6; OBJ_KIND_GIT_TREE_OVERRIDE = 7;
               OBJ_KIND_SHA256_ALIAS = 8; }
message ObjKey { ObjKind kind = 1; bytes id = 2; }          // 32 bytes (20 for GIT_TREE_OVERRIDE)
message Object { ObjKey key = 1; bytes data = 2; }          // stored bytes: codec ‖ data

message RefTarget { oneof t { bytes changeset = 1; bytes tag = 2; } }
message Ref { string name = 1; RefTarget target = 2; uint64 version = 3; int64 expires_at = 4; }
message Signature { bytes name = 1; bytes email = 2; int64 time = 3; int32 tz_min = 4; }

service Repo {
  rpc Version(VersionRequest) returns (VersionResponse);
  rpc HasObjects(HasObjectsRequest) returns (HasObjectsResponse);
  rpc GetObjects(stream GetObjectsRequest) returns (stream GetObjectsResponse);
  rpc PutObjects(stream PutObjectsRequest) returns (PutObjectsResponse);
  rpc ListRefs(ListRefsRequest) returns (ListRefsResponse);
  rpc UpdateRefs(UpdateRefsRequest) returns (UpdateRefsResponse);
  rpc Resolve(ResolveRequest) returns (ResolveResponse);
  rpc GitMap(GitMapRequest) returns (GitMapResponse);
  rpc Log(LogRequest) returns (stream LogResponse);
  rpc MergeBase(MergeBaseRequest) returns (MergeBaseResponse);
  rpc IsAncestor(IsAncestorRequest) returns (IsAncestorResponse);
  rpc Diff(DiffRequest) returns (stream DiffResponse);
  rpc Blame(BlameRequest) returns (stream BlameResponse);
  rpc LastCommits(LastCommitsRequest) returns (LastCommitsResponse);
  rpc ReadChangeset(ReadChangesetRequest) returns (ReadChangesetResponse);
  rpc ReadTree(ReadTreeRequest) returns (stream ReadTreeResponse);
  rpc ReadFile(ReadFileRequest) returns (stream ReadFileResponse);
  rpc Stat(StatRequest) returns (StatResponse);
  rpc CheckAttr(CheckAttrRequest) returns (CheckAttrResponse);
  rpc Grep(GrepRequest) returns (stream GrepResponse);
  rpc Archive(ArchiveRequest) returns (stream ArchiveResponse);
  rpc Commit(CommitRequest) returns (CommitResponse);
  rpc Merge(MergeRequest) returns (MergeResponse);
  rpc Apply(ApplyRequest) returns (ApplyResponse);
  rpc CopyClosure(CopyClosureRequest) returns (CopyClosureResponse);
  rpc RepoAdmin(RepoAdminRequest) returns (RepoAdminResponse);
  rpc ImportGitPush(ImportGitPushRequest) returns (ImportGitPushResponse);   // Forgejo internal token only
}

message VersionRequest {}
message VersionResponse { string server_version = 1; string min_client = 2; repeated string features = 3; }

message HasObjectsRequest { RepoRef repo = 1; repeated ObjKey keys = 2; }           // ≤ 4096 keys
message HasObjectsResponse { bytes present = 1; }                                   // bitmap, LSB-first per byte
message GetObjectsRequest { RepoRef repo = 1; repeated ObjKey keys = 2; }           // ≤ 256 keys per message
message GetObjectsResponse { repeated Object objects = 1; repeated ObjKey missing = 2; } // ≤ 16 MiB per message
message PutObjectsRequest { RepoRef repo = 1; repeated Object objects = 2; }
message PutObjectsResponse { uint64 stored = 1; uint64 already_present = 2; }

message ListRefsRequest { RepoRef repo = 1; string prefix = 2; bytes contains = 3; string after = 4; uint32 limit = 5; }
message ListRefsResponse { repeated Ref refs = 1; string next = 2; string head = 3; }   // head = symbolic HEAD
message RefUpdate { string name = 1;
  oneof expect { bool any = 2; bool absent = 3; RefTarget value = 4; }
  RefTarget new = 5;  bool delete = 6; }
enum UpdateMode { UPDATE_MODE_FAST_FORWARD = 0; UPDATE_MODE_FORCE = 1; UPDATE_MODE_LAND = 2; }
enum HookMode { HOOK_MODE_RUN = 0; HOOK_MODE_SKIP_PRE = 1; }                        // SKIP_PRE: internal token only
message UpdateRefsRequest { RepoRef repo = 1; repeated RefUpdate updates = 2; UpdateMode mode = 3;
  repeated string push_options = 4; HookMode hooks = 5; int64 ttl_seconds = 6; }
message Rewrite { bytes old = 1; bytes new = 2; }
message RefResult { string name = 1; RefTarget old = 2; RefTarget new = 3; }
message UpdateRefsResponse { repeated RefResult results = 1; repeated Rewrite rewritten = 2; string message = 3; }

message ResolveRequest { RepoRef repo = 1; repeated string revspecs = 2; }
message ResolveResponse { repeated bytes ids = 1; }                                  // empty bytes = not found
message GitMapRequest { RepoRef repo = 1; repeated bytes asc = 2; repeated bytes git = 3; }
message GitMapResponse { repeated bytes asc_to_git = 1; repeated bytes git_to_asc = 2; }

message LogRequest { RepoRef repo = 1; repeated string include = 2; repeated string exclude = 3;
  repeated string paths = 4; bool first_parent = 5; bool follow = 6; uint32 skip = 7; uint32 limit = 8;
  string author = 9; string grep = 10; int64 since = 11; int64 until = 12; bool count_only = 13; }
message LogEntry { bytes id = 1; bytes git = 2; repeated bytes parents = 3; Signature author = 4;
  Signature committer = 5; bytes message = 6; uint32 gen = 7; }
message LogResponse { repeated LogEntry entries = 1; uint64 count = 2; }
message MergeBaseRequest { RepoRef repo = 1; bytes a = 2; repeated bytes b = 3; bool all = 4; }
message MergeBaseResponse { repeated bytes bases = 1; }
message IsAncestorRequest { RepoRef repo = 1; bytes ancestor = 2; bytes descendant = 3; }
message IsAncestorResponse { bool result = 1; }

enum DiffFormat { DIFF_FORMAT_NAMES = 0; DIFF_FORMAT_STATUS = 1; DIFF_FORMAT_STAT = 2; DIFF_FORMAT_NUMSTAT = 3;
                  DIFF_FORMAT_PATCH = 4; DIFF_FORMAT_RAW = 5; }
message DiffRequest { RepoRef repo = 1; string base = 2; string head = 3; bool merge_base = 4;
  repeated string paths = 5; DiffFormat format = 6; bool renames = 7; uint32 context = 8; bool binary = 9;
  bool null_terminated = 10; uint32 max_files = 11; }
message PathChange { bytes path = 1; string status = 2; bytes old_path = 3; uint32 old_mode = 4; uint32 new_mode = 5;
  bytes old_git = 6; bytes new_git = 7; bytes old_id = 8; bytes new_id = 9; uint64 old_size = 10; uint64 new_size = 11;
  uint32 similarity = 12; }
message DiffResponse { repeated PathChange changes = 1; bytes text = 2; bool truncated = 3; }  // text = git-format output

message BlameRequest { RepoRef repo = 1; string rev = 2; bytes path = 3; uint32 start_line = 4; uint32 end_line = 5;
  bool reverse = 6; }
message BlameHunk { bytes origin = 1; bytes orig_path = 2; uint32 orig_start = 3; uint32 start = 4; uint32 len = 5; }
message BlameResponse { repeated BlameHunk hunks = 1; repeated LogEntry commits = 2; }
message LastCommitsRequest { RepoRef repo = 1; string rev = 2; bytes dir = 3; repeated bytes names = 4; }
message LastCommitsResponse { map<string, LogEntry> by_name = 1; }

message ReadChangesetRequest { RepoRef repo = 1; string rev = 2; }
message ReadChangesetResponse { LogEntry header = 1; bytes root_tree = 2; bytes root_git = 3; repeated PathChange changes = 4;
  map<string, bytes> extra = 5; bytes raw_git_commit = 6; }
message ReadTreeRequest { RepoRef repo = 1; string rev = 2; bytes path = 3; bool recursive = 4; uint32 limit = 5;
  bytes after = 6; }
message TreeEntryInfo { bytes name = 1; bytes path = 2; uint32 mode = 3; bytes id = 4; bytes git = 5; uint64 size = 6;
  bool lfs = 7; uint64 lfs_size = 8; }
message ReadTreeResponse { repeated TreeEntryInfo entries = 1; bytes tree_id = 2; bytes tree_git = 3; }
message ReadFileRequest { RepoRef repo = 1; oneof at { string rev_path = 2; bytes file_id = 3; } uint64 offset = 4;
  uint64 length = 5; bool resolve_lfs = 6; }
message ReadFileResponse { bytes data = 1; uint64 total_size = 2; }
message StatRequest { RepoRef repo = 1; string rev = 2; repeated bytes paths = 3; repeated ObjKey objects = 4; }
message StatResponse { repeated TreeEntryInfo entries = 1; repeated bool exists = 2; }
message CheckAttrRequest { RepoRef repo = 1; string rev = 2; repeated bytes paths = 3; repeated string names = 4; }
message AttrValues { map<string, string> values = 1; }
message CheckAttrResponse { repeated AttrValues per_path = 1; }
message GrepRequest { RepoRef repo = 1; string rev = 2; string pattern = 3; bool fixed = 4; bool ignore_case = 5;
  repeated string paths = 6; uint32 max_results = 7; }
message GrepMatch { bytes path = 1; uint32 line = 2; bytes text = 3; }
message GrepResponse { repeated GrepMatch matches = 1; }
message ArchiveRequest { RepoRef repo = 1; string rev = 2; string format = 3; string prefix = 4; repeated string paths = 5; }
message ArchiveResponse { bytes data = 1; }

message FileOp { bytes path = 1;
  oneof op { bytes put_content = 2; bytes put_file_id = 3; bool delete = 4; bytes move_from = 5; }
  uint32 mode = 6; }
message CommitRequest { RepoRef repo = 1; string parent = 2; repeated FileOp ops = 3; bytes message = 4;
  Signature author = 5; Signature committer = 6; string update_ref = 7; RefTarget expect = 8; bool sign = 9; }
message CommitResponse { bytes id = 1; bytes git = 2; }
enum MergeStyle { MERGE_STYLE_MERGE = 0; MERGE_STYLE_SQUASH = 1; MERGE_STYLE_REBASE = 2; MERGE_STYLE_REBASE_MERGE = 3;
  MERGE_STYLE_FAST_FORWARD_ONLY = 4; }
message MergeRequest { RepoRef repo = 1; string base = 2; string head = 3; RepoRef head_repo = 4; MergeStyle style = 5;
  bytes message = 6; Signature author = 7; Signature committer = 8; bool dry_run = 9; string update_ref = 10;
  repeated string protected_globs = 11; bool sign = 12; }
message MergeResponse { bytes result = 1; repeated bytes conflicts = 2; repeated bytes changed_protected = 3;
  bool is_empty = 4; repeated Rewrite rewritten = 5; }
enum ApplyMode { APPLY_MODE_CHERRY_PICK = 0; APPLY_MODE_REVERT = 1; APPLY_MODE_PATCH = 2; }
message ApplyRequest { RepoRef repo = 1; string onto = 2; string changeset = 3; bytes patch = 4; ApplyMode mode = 5;
  bytes message = 6; Signature author = 7; Signature committer = 8; string update_ref = 9; }
message ApplyResponse { bytes result = 1; repeated bytes conflicts = 2; }
message CopyClosureRequest { RepoRef from = 1; RepoRef to = 2; repeated bytes changesets = 3; }
message CopyClosureResponse { uint64 copied = 1; }

message RepoAdminRequest { RepoRef repo = 1;
  oneof op { InitOp init = 2; ForkOp fork = 3; bool delete = 4; string rename_to = 5; bool size = 6; bool verify = 7; } }
message InitOp { string default_branch = 1; bool windows_safe = 2; string mode = 3; /* "primary"|"mirror" */
  string mirror_url = 4; repeated string land_branches = 5; }
message ForkOp { RepoRef from = 1; }
message RepoAdminResponse { uint64 logical_size = 1; uint64 unique_size = 2; string report = 3; }

message GitRefUpdate { string name = 1; bytes old_git = 2; bytes new_git = 3; }
message ImportGitPushRequest { RepoRef repo = 1; string git_dir = 2; string object_dir = 3;
  repeated string alternate_dirs = 4; repeated GitRefUpdate updates = 5; int64 pusher_id = 6; }
message ImportGitPushResponse { repeated RefResult results = 1; }
```

```proto
// workspace.proto — daemon IPC only (named pipe / Unix socket, current user only)
service Workspace {
  rpc Ping(PingRequest) returns (PingResponse);                 // version, pid, uptime
  rpc Mount(MountRequest) returns (MountResponse);              // ws_id → starts provider
  rpc Unmount(UnmountRequest) returns (UnmountResponse);
  rpc Switch(SwitchRequest) returns (SwitchResponse);           // ws_id, from_root, to_root, mode {SAFE|MERGE|FORCE}
  rpc Journal(JournalRequest) returns (JournalResponse);        // since token → paths + new token + fresh flag
  rpc ClearJournal(ClearJournalRequest) returns (ClearJournalResponse);
  rpc Dehydrate(DehydrateRequest) returns (DehydrateResponse);  // ws_id, pathspec
  rpc Prefetch(PrefetchRequest) returns (PrefetchResponse);     // ws_id, pathspec
  rpc Stats(StatsRequest) returns (StatsResponse);              // hydrations by path/process, bytes, journal size
  rpc Shutdown(ShutdownRequest) returns (ShutdownResponse);
}
```

**HTTP on the same port** (`axum`): `/v8/*` turbo remote cache (§12.4), `/hooks/forgejo` (mirror webhook), `/.well-known/asc`, `/healthz`, `/metrics`.

**Compatibility:** `asc.v1` evolves additively only (new fields, RPCs and enum values). A breaking change is `asc.v2`, served side by side. Envelope versions are independent: readers reject unknown versions with "upgrade asc".

---

## 8. Server (`asc serve`, crate `asc-serve`)

### 8.1 Process and config

One process, one port, stateless apart from caches. Several replicas may run behind a load balancer: every ref change is a SQL CAS, and in-memory graphs refresh on CAS miss. Config `asc-serve.toml`:

```toml
listen = "0.0.0.0:7890"
public_url = "https://asc.forge.example.org"
tls = { cert = "...", key = "..." }          # or terminate at the proxy
store = "s3://asc?endpoint=https://garage.internal:3900&region=garage&path_style=true"   # or file:///srv/asc
db = "postgres://asc_hub@db/forgejo?options=-csearch_path%3Dasc_hub"                              # or sqlite:///srv/asc/asc.db
cache = { memory = "4GiB", disk_dir = "/var/cache/asc-serve", disk = "50GiB" }
[forgejo]
url = "https://forge.example.org"
internal_token_file = "/etc/asc/forgejo-internal-token"
webhook_secret_file = "/etc/asc/webhook-secret"
repo_root = "/var/lib/forgejo/data/forgejo-repositories"   # needed from Stage 2 (bridge)
[turbo]
trusted_tokens_file = "/etc/asc/turbo-trusted"   # Stage 1 only (§12.4)
pr_retention_days = 14
trusted_retention_days = 60
```

### 8.2 SQL schema (`crates/asc-serve/migrations/{postgres,sqlite}`)

```sql
repos(id BIGINT PK, name TEXT UNIQUE, ns BLOB, mode TEXT /*mirror|primary*/, default_branch TEXT,
      windows_safe BOOL, forgejo_reads TEXT /*git|asc*/, bridge TEXT /*off|lazy|eager*/, land_branches TEXT /*json*/,
      created BIGINT)
refs, ref_log                                  -- §3.4
cs_graph(ns BLOB, cs BLOB, gen BIGINT, parents BLOB /*concat 32B ids*/, PRIMARY KEY(ns, cs))
git_map(ns BLOB, git BLOB, kind SMALLINT /*1 file,2 tree,3 changeset,4 tag*/, asc BLOB, PRIMARY KEY(ns, git, kind))
  INDEX git_map_asc (ns, asc)
file_meta(ns BLOB, file BLOB PK2, size BIGINT, git BLOB, sha256 BLOB)   -- server-computed, see 8.4
op_results(op_id BLOB PRIMARY KEY, repo_id BIGINT, result BLOB, at BIGINT)       -- retained 7 days
outbox(id BIGINT PK autoinc, repo_id BIGINT, op_id BLOB, effects BLOB /*json*/, attempts INT, next_at BIGINT,
       done_at BIGINT NULL, last_error TEXT)
mirror_state(repo_id BIGINT PK, last_import BIGINT, last_error TEXT)
gc_marks(run BIGINT, ns BLOB, key BLOB)          -- O01 only
```

### 8.3 `UpdateRefs` pipeline (normative order)

1. **Authorise:** write permission on the repo (§15.1). `hooks=SKIP_PRE` requires the Forgejo internal token.
2. **Idempotency:** if `op_results[op_id]` exists, return the stored result.
3. **Mode check:** a repo in `mirror` mode → `MIRROR_MODE`.
4. **Ingest closure** for every new target: walk new changesets until known in `cs_graph`. For each in topological order, check that all referenced objects exist (`HasObjects`), validate it (§2.6, §8.4), apply the path policy (§2.8), and upsert `cs_graph` and `git_map` rows (eager derivation). Missing objects → `MISSING_OBJECTS` with the keys.
5. **Policy:** for `FAST_FORWARD`, the new value must descend from the current value (or the ref must be absent). For `LAND`, run §8.5 to produce rewritten changesets, then ingest them as in step 4.
6. **Bridge quarantine** (only when `repos.forgejo_reads = git`, Stage 2): export the new git objects (§14.2) into `<bridge>/objects/incoming-asc-<op_id>/` (a fresh object directory) **before** pre-receive **[fix J1: Stage 2 ordering]**.
7. **Pre-receive** (unless `SKIP_PRE`, or no Forgejo is configured): `POST {forgejo}/api/internal/hook/pre-receive/{owner}/{repo}` with `HookOptions` (`OldCommitIDs`/`NewCommitIDs` as git SHAs while `object_format=sha1`; `RefFullNames`; `UserID`/`UserName`/`DeployKeyID` from authz; `GitPushOptions`; `PushTrigger = "asc"`; in Stage 2 `GitObjectDirectory` = quarantine, `GitAlternativeObjectDirectories` = `<bridge>/objects`, `GitQuarantinePath` = quarantine; from Stage 3 these are empty and Forgejo reads through Seam A). Rejection → delete the quarantine, return `HOOK_REJECTED` with Forgejo's message. For land, the hook sees the **rewritten** changesets **[fix: missing-from-all]**.
8. **One SQL transaction [graft: integration]:** CAS every ref (version check); insert `ref_log` rows (`via` = `asc-push`|`land`|`forgejo-web`|`git-push`|`mirror`); insert `op_results(op_id, response)`; insert `outbox(effects = [bridge_promote?, post_receive])`. A CAS miss rolls back. For `LAND`, retry from step 5 (at most 20 times); otherwise return `REF_CONFLICT`.
9. **Respond** with results and `rewritten` pairs.

**Outbox worker:** rows are processed per repo in id order. Effects are idempotent and run in listed order:

- `bridge_promote` (Stage 2–3, and Stage 4 with `bridge=eager`): move the quarantine's objects into `<bridge>/objects` (or, if the quarantine is gone, export missing objects for the new tips deterministically). Then `git update-ref --stdin` setting each bridge ref to **asc's current value** (not the request's new value, so the bridge converges on the truth). No git hooks run.
- `post_receive`: `POST /api/internal/hook/post-receive/{owner}/{repo}` with the old and new values and `PushTrigger="asc"`. Forgejo's `PushUpdates` is idempotent per (ref, new).

Retries use exponential backoff from 1 s to 5 min, indefinitely. The `asc_outbox_lag_seconds` and `asc_outbox_failures_total` metrics alert after 10 failures. A crash at any point after the transaction commits loses nothing **[fix J1: lost CI triggers]**. A crash before it leaves only orphan objects and a quarantine directory, which a janitor removes after 24 h.

### 8.4 Ingest verification **[graft: integration] [fix J2: client-supplied git fields]**

- `PutObjects` re-hashes every `File`, `Chunk`, `Tree`, `Changeset` and `Tag`. For each new `File` it computes `size`, `git` (blob SHA-1) and `sha256`, and records them in `file_meta`. Chunked files: when the `ChunkList` arrives, the server streams its chunks once to compute the same metadata.
- Every `Entry` and `Change::Write` is checked against `file_meta`: `size`, `git` and, for LFS entries, the pointer bytes parse to `lfs.sha256`/`lfs.size`. Every `Dir` entry's `git` must equal the canonical render hash of the child tree, or an override `o/<git>` whose raw bytes hash to `git` and decode to the same entries must exist. A mismatch → `INVALID_OBJECT`. Client-built trees are accepted only after this check.

### 8.5 Land (pushrebase) [graft: performance/simplicity]

Applies when the request mode is `LAND` (client `asc push` to a branch listed in `repos.land_branches`, or `asc push --land`):

1. The stack = new changesets from the pushed tip back to `B`, the first ancestor already in `cs_graph` that is an ancestor of the current tip `T`.
2. If `B == T`, it is a fast-forward.
3. Otherwise compute `A = ∪ paths(stack)` and `P = paths(B..T)` (§4.5). If they conflict → `LAND_CONFLICT` with the conflicting paths, and the client runs `asc pull --rebase`.
4. If any stack changeset has `git.gpgsig` and the branch requires signed commits (from authz, §15.1) → `LAND_NEEDS_CLIENT_REBASE` (signatures cannot survive a server rebase).
5. Metadata-rebase each changeset onto the new tip in order (§4.4). Committer is unchanged; there is no server signature.
6. Continue with pipeline step 6. The response's `rewritten` maps old → new ids. The client moves its local branch and remote-tracking ref to the new tip, rewrites local branches that pointed into the stack, and prints `landed <n> changesets as <new tip> (rebased onto <T>)`.

No queue and no leader: concurrent lands race on the CAS and retry (step 8). At ASC's rate (peak about 50 per minute) this is ample.

### 8.6 Mirror mode (Stage 1)

- `RepoAdmin.init { mode = "mirror", mirror_url }`. The server keeps a bare mirror at `<cache disk_dir>/mirrors/<repo-id>.git`.
- **Triggers:** a Forgejo push webhook to `/hooks/forgejo` (HMAC-SHA256 `X-Forgejo-Signature` verified with the configured secret), plus a poll every 60 s. Each trigger runs `git fetch --prune <mirror_url> +refs/heads/*:refs/heads/* +refs/tags/*:refs/tags/* +refs/pull/*:refs/pull/*` with a read token, then imports new commits incrementally (§14.1), then sets asc refs to match the git refs with `via = mirror`, all in one transaction. Imports are idempotent, so duplicate webhooks are harmless.
- LFS objects referenced by new pointers are copied from Forgejo's LFS API into the namespace (`x/sha256` alias plus content) by a background job.

### 8.7 Server-side mutations (`Commit`, `Merge`, `Apply`, `CopyClosure`, `RepoAdmin`)

- `Commit` builds a changeset from `FileOp`s on `parent` (content inline or by `FileId`), then runs the full `UpdateRefs` pipeline on `update_ref` with `expect`.
- `Merge` with `dry_run` is Forgejo's mergeability check: it returns conflicts and the changed paths that match `protected_globs`. Styles: `MERGE` (two-parent changeset), `SQUASH` (one changeset), `REBASE` (metadata rebase of each commit), `REBASE_MERGE` (rebase then a merge commit), `FAST_FORWARD_ONLY`. With `update_ref`, the result goes through `UpdateRefs` so Forgejo hooks run once. When `head_repo` differs, `CopyClosure` runs first.
- `RepoAdmin`: `init`, `fork` (new namespace plus closure copy job), `delete` (refs dropped; the namespace is deleted after 30 days), `rename` (registry only), `size` (logical = bytes reachable from refs; unique = namespace bytes), `verify` (closure of every ref exists; git map round-trip).

### 8.8 Expiring refs

`UpdateRefs.ttl_seconds` (from `asc push -o asc.ttl=7d`) sets `refs.expires_at`, and is accepted only for `refs/heads/agent/*` and `refs/snapshots/*`. A worker deletes expired refs every 10 minutes through the normal pipeline (outbox `post_receive` with a deletion), so Forgejo's branch table stays in sync **[graft: performance]**.

---

## 9. Machine side: daemon, workspaces, projection

### 9.1 The daemon (`asc daemon`)

One per user per machine. It exists because a virtual filesystem needs a long-lived process. It also hosts background work.

| Does | Does not |
|---|---|
| Host every virtual workspace (ProjFS root or FUSE mount) | Proxy history RPCs (the CLI calls the remote directly) |
| Keep each workspace's journal; reconcile on start | Own the cache (a shared directory) |
| Run switch for virtual workspaces (placeholder updates) | Upload anything before `asc push` |
| Prefetch, GC the cache, dehydrate idle files, expire ephemeral worktrees | |
| Serve `asc-fsmonitor` queries | |

- **Start:** the CLI starts it on first need. The installer registers a per-user logon task (Windows Task Scheduler: restart on failure 3 times, 10 s apart) or a systemd user unit (`Restart=on-failure`, `RestartSec=2`) **[fix: missing-from-all, watchdog]**. Stale pid files are detected by `Ping`.
- **IPC:** `\\.\pipe\asc-<user-SID>` with a DACL that allows only the user's SID and SYSTEM; Unix `$XDG_RUNTIME_DIR/asc/asc.sock` (or `$ASC_HOME/run/asc.sock`), mode 0600, in a 0700 directory.
- **Version skew:** the CLI compares `Ping.version` with its own. On mismatch it asks the daemon to `Shutdown` (which stops providers cleanly) and starts the new binary. ProjFS roots re-attach with the same instance GUID; FUSE remounts at the same path.
- **Daemon down while files are open:** hydrated and full files stay readable and writable (they are real NTFS/overlay files). Reads of virtual files fail until restart (≤ 10 s with the watchdog). Writes during downtime are found by reconcile (§10.2.7). `asc status` prints `daemon restarted at <t>; reconciled N paths`.
- **No daemon** (`ASC_NO_DAEMON=1`, CI, tests): the CLI does everything in-process. Only plain workspaces are available.

### 9.2 State database (`$ASC_HOME/state.db`, SQLite via sqlx)

```sql
repos(id TEXT PK /*uuid*/, name TEXT, ns BLOB, remote_url TEXT, remote_kind TEXT, default_branch TEXT,
      windows_safe BOOL, display_ids TEXT)
refs, ref_log                                         -- SqlRefs schema (§3.4), repo_id = rowid of repos
cs_gen(repo_id, cs BLOB, gen INT, parents BLOB, PRIMARY KEY(repo_id, cs))
git_map(repo_id, git BLOB, kind INT, asc BLOB, PRIMARY KEY(repo_id, git, kind)) + index (repo_id, asc)
workspaces(id TEXT PK, repo_id, path TEXT UNIQUE, mode TEXT /*virtual|plain*/, head TEXT /*ref name or ""*/,
           base_cs BLOB, epoch INT, epoch_time INT /*unix s*/, sparse TEXT, ephemeral_until INT NULL,
           locked TEXT NULL, created INT)
entry_times(ws_id, path BLOB, time INT, PRIMARY KEY(ws_id, path))   -- placeholder time overrides (§10.1)
journal(ws_id, seq INT, path BLOB, op INT, PRIMARY KEY(ws_id, seq))  -- op: 1 modified,2 created,3 deleted,4 renamed-from,5 dir-renamed-from,6 dir-renamed-to
switch_intent(ws_id PK, from_root BLOB, to_root BLOB, to_head TEXT, pending BLOB /*paths*/, started INT)
ws_ops(ws_id PK, kind TEXT /*rebase|cherry-pick|revert|merge*/, state BLOB)
inodes(ws_id, ino INT, path BLOB, PRIMARY KEY(ws_id, ino)) + UNIQUE(ws_id, path)   -- FUSE only
```

The journal is appended in batches (≤ 100 ms) from an in-memory queue. A crash can lose hints but never correctness, because reconcile recovers them (§10.2.7).

### 9.3 Workspaces and worktrees

A workspace is a `workspaces` row, a gitdir `wc/<id>/git/`, a `.git` gitfile at its root, and a mount (virtual) or materialised files (plain).

- `asc clone <url> [<dir>] [--mode virtual|plain] [--branch b] [--sparse <cone>…] [--no-checkout] [--reference <git-dir>]`: register the repo, fetch refs and the head changeset and root tree, create the workspace, write the projection (§9.4), mount or materialise.
- `asc worktree add <path> [-b <new>] [--detach] [<rev>] [--ephemeral --ttl 7d] [--mode …]`: the same with no network when the revision is cached. Cost: one index file (about 0.5 MB for ASC) plus rows.
- `asc worktree list [--porcelain]|remove [--force]|prune|lock|unlock|move` mirror git. `remove` unmounts, deletes the root (including redirections) and the gitdir, and refuses when dirty unless `--force`.
- **Ephemeral worktrees [graft: integration/performance]:** `--ephemeral --ttl <dur>` sets `ephemeral_until`. The daemon removes expired clean worktrees hourly. Expired dirty worktrees are kept, and `asc status` warns. Agent branches use `refs/heads/agent/<name>` and push with `-o asc.ttl=<dur>`.
- **Claude Code agents:** configure Claude Code's worktree hooks in ASC's `.claude/settings.json`: `WorktreeCreate` → `asc worktree add --ephemeral --ttl 7d "$path" -b "$branch"`, `WorktreeRemove` → `asc worktree remove --force "$path"`. If the installed Claude Code lacks these hook events, agents call `asc worktree add` through a project instruction instead. Spike S7 confirms which applies.

### 9.4 The git projection

```
<root>/.git                              gitfile: "gitdir: $ASC_HOME/wc/<ws>/git\n"
$ASC_HOME/wc/<ws>/git/
  commondir                              "../../../repos/<repo-id>/.git\n"
  HEAD                                   "ref: refs/heads/<b>\n" or the git sha
  index                                  written by asc (below); edited by asc and git
  config.worktree                        core.worktree=<root>, core.hooksPath=$ASC_HOME/hooks, core.fsmonitor=<asc-fsmonitor path> (virtual only)
  logs/HEAD
  MERGE_HEAD, MERGE_MSG, CHERRY_PICK_HEAD (when in progress)
  shallow (CI only)
$ASC_HOME/repos/<repo-id>/.git/           common dir (shared by all workspaces of the repo on the machine)
  config                                 core.repositoryformatversion=1, core.bare=false, core.autocrlf=false, core.eol=lf,
                                         core.symlinks=<os>, core.ignorecase=<true on Windows>, core.longpaths=true,
                                         core.filemode=<os>, extensions.worktreeConfig=true, extensions.partialClone=origin,
                                         remote.origin.url=asc::origin, remote.origin.promisor=true,
                                         remote.origin.partialclonefilter=blob:none, remote.origin.fetch=+refs/heads/*:refs/remotes/origin/*,
                                         user hooks chain: asc.userHooksPath (detected, see below)
  objects/                               commits, trees (rendered), blobs (seeded or fetched); git-remote mode: full packs
  refs/, packed-refs, logs/              mirror of asc refs as git SHAs (written by asc)
```

- **Why the common dir sits at `repos/<id>/.git`:** turbo treats a gitfile worktree as linked and puts its cache under the main worktree root, which is the parent of the common dir. All workspaces of a repo therefore share `$ASC_HOME/repos/<id>/.turbo/cache` (a machine-wide turbo cache). Contract-tested in U61.
- **Config precedence [fix: missing-from-all, husky and autocrlf]:** `core.hooksPath` and `core.fsmonitor` live in `config.worktree`, which overrides the local `config` that husky's `prepare` script writes. `core.autocrlf=false` in the local config overrides any global `autocrlf=true`. `asc doctor` flags a `--worktree` override of either key.
- **Hook chain:** `$ASC_HOME/hooks/<name>` dispatchers run asc's own action, then the user's hook from the first existing of: `core.hooksPath` in local or global config (for example husky's `.husky/_`), else `<common>/hooks`. Hooks with no asc action are pure pass-throughs. `asc commit` uses the same resolution to run `pre-commit`, `prepare-commit-msg`, `commit-msg` and `post-commit`, honouring `--no-verify`.
- **Index (the staging area):** v2 with a TREE (cache-tree) extension, written atomically with git's lock protocol: create `index.lock` with `O_EXCL`, write, fsync, rename. If the lock is held, asc waits up to 5 s with backoff and then fails with git's message (`Unable to create '…/index.lock': File exists`) **[fix: missing-from-all, concurrent index writers]**. asc writes the index on clone, switch, checkout, reset, restore, merge and rebase, and edits it on add/rm/mv. Each entry: path; mode; `git` oid; size (`lfs.size` for LFS entries); ctime = mtime = the placeholder time (virtual, §10.1) or the real stat (plain); dev/ino 0 on Windows, `inodes.ino` on FUSE, real on plain Unix; uid/gid 0 on virtual. No skip-worktree bits except unrepresentable entries (§2.8) and plain sparse mode. Gitlinks: mode 160000. After writing, asc sets the index file's mtime to `max(now, max_entry_time + 2 s)` so no entry is racy.
- **Objects:**
  - **Commits:** rendered locally from changesets (§2.9) for all ancestors of every local branch head and remote-tracking ref, written once per machine to the common dir. For ASC that is 5.5k commits, a few MB.
  - **Trees:** rendered for HEAD and the bases on demand; others arrive lazily through the promisor (git lazily fetches missing trees from promisor remotes).
  - **Blobs:** seeded at HEAD (and at the CI base) for `**/package.json`, `turbo.json`, `bun.lock`, `.gitignore` files and `**/Cargo.toml`, the files turbo reads. Everything else comes on demand through `git-remote-asc` (§9.5).
  - Every rendered tree adds `(entry.git → entry.id)` rows to `state.db.git_map`, so blob requests resolve without the server.
- **Refs mirror:** whenever asc moves a ref, it writes the git SHA into the common dir (`refs/heads/*`, `refs/tags/*`, `refs/remotes/*`, `refs/stash` plus `logs/refs/stash`) and `HEAD`/`logs/HEAD` into the workspace gitdir.

### 9.5 `git-remote-asc` (promisor and push helper)

Invoked by git for `asc::origin` URLs, with `GIT_DIR` set to the workspace gitdir.

- **Capabilities:** `stateless-connect`, `push`, `option`.
- **Fetch** (protocol v2 over stateless-connect): `ls-refs` returns the asc refs mirror. `fetch` accepts explicit `want` oids (no negotiation; `have` lines are ignored) and returns a pack of whole objects (no deltas). Objects resolve git oid → asc id through `state.db.git_map` (falling back to `GitMap` RPC on the remote), are read through `Tiered` (cache → remote), and are rendered to git form: blobs are the file bytes (pointer bytes for LFS entries), trees use the override or the render, commits use the render.
- **Push** (`push <src>:<dst>`): reconcile (§9.6) so `<src>` is imported, then run `asc push` semantics for that one ref, then reply `ok <dst>` or `error <dst> <message>`. So `git push` from an IDE pushes through asc.

### 9.6 Git-side edits are imported, not refused [graft: judge 2] [fix: missing-from-all, IDE workflows]

**Reconcile** (`asc internal reconcile <ws>`) is triggered in four ways:

1. the `reference-transaction` dispatcher, in the `committed` state;
2. `post-checkout`, `post-commit`, `post-merge` and `post-rewrite`;
3. the start of every `asc` command in that workspace;
4. a `notify` watch on the common dir's `refs/`, `packed-refs` and the workspace `HEAD`, held by the daemon.

Hooks only speed up detection. Correctness comes from (3) and (4), so `--no-verify` and a changed `core.hooksPath` cannot cause divergence.

Steps:

1. For each mirrored ref and `HEAD`, compare the git value with asc's value.
2. A git value already in the git map → move the asc ref to the mapped changeset (CAS on asc's old value).
3. An unknown git value → import commits from the projection ODB (gix: loose objects and packs), walking parents until known. The import is lossless and canonical (§14.1), so `git commit` from VS Code produces exactly the changeset `asc commit` would have produced. Then move the asc ref.
4. If asc's ref moved concurrently (CAS miss): asc wins. The git ref is reset to asc's value, and the git commit stays reachable as `refs/asc/orphans/<sha>`, with a warning printed by the next `asc` command.
5. `HEAD` moved by `git checkout` / `git switch`: adopt the new HEAD and base for the workspace. Files git wrote are real files that the journal sees (virtual) or the stat scan sees (plain). Status hashes them; those equal to the tree are clean, and the daemon dehydrates them later.

`git stash`, `git merge`, `git rebase`, `git cherry-pick` and `git reset` in a projection therefore all work. They are slower than the asc equivalents, because git materialises files. `asc doctor` reports git-written full files per workspace.

**Delegation [graft: integration D11]:** an `asc <cmd>` that asc does not implement and that git classifies as read-only (`shortlog`, `name-rev`, `describe --contains`, `grep --no-index`, `notes show`, `whatchanged`, `range-diff`, `cherry`, `log --graph` variants asc lacks) runs `git <cmd> …` with `GIT_DIR`/`GIT_WORK_TREE` set to the projection. History-rewriting commands asc lacks (`filter-branch`, `filter-repo`, `replace`) fail with `asc: '<cmd>' is not supported`.

### 9.7 `asc commit` (staging → changeset)

1. Read the index (holding `index.lock` while reading the stat-dirty set when `-a` is used).
2. For each index entry whose `(mode, oid)` differs from HEAD's tree: if the oid is in `git_map`, reuse its `FileId` (and LFS ref). Otherwise **read the blob from the projection ODB** (loose object written by `git add`, or a pack), verify `sha1("blob <len>\0" ‖ bytes) == oid`, ingest it into `store/` (FileId, chunks), and add a `git_map` row. The working file is never read for a staged oid **[fix J1/J2/J3: `git add -p`]**. `asc add` writes the loose object itself as it stages (gix `write_blob`), so asc and git staging are interchangeable.
3. Build the new root with `apply` and the changes with `leafdiff` (§4.2). Record `copy_from` from `asc mv` (kept in `ws_ops`'s pending-renames list until commit).
4. Run hooks, sign if `-S`/`commit.gpgSign`, write the changeset to `store/`, derive `gen` and `gitcommit`, and move the branch (CAS on the old value). Write the commit object into the common dir's ODB and update the refs mirror and the index's TREE extension.

---

## 10. Working copies and virtual filesystems (crates `asc-wc`, `asc-vfs`)

### 10.1 Stat times (all modes) [fix J1/J2/J3: committer-time mtimes]

- Each virtual workspace has an `epoch` counter and an `epoch_time`, both in `workspaces`.
- **Clone/worktree add:** `epoch = 1`, `epoch_time = floor_seconds(now)`. Every placeholder reports `CreationTime = LastWriteTime = ChangeTime = LastAccessTime = epoch_time`.
- **Switch/reset/restore** that changes a set of paths: `T = max(floor_seconds(now), previous epoch_time + 1)`. Then `epoch += 1`, `epoch_time = T`, and every **changed** placeholder is rewritten with time `T` and recorded in `entry_times(ws, path, T)`. Unchanged placeholders keep their times. Times therefore only move forward. A file changed by a switch is always newer than build outputs created before the switch, so mtime-based tools (cargo, `tsc -b`, make) rebuild.
- The index records exactly these times. A path's time = `entry_times[path]` if present, else the workspace's clone-time `epoch_time` (kept in `workspaces.created`). There is no history walk and no per-path derived data.
- Hydration never changes placeholder timestamps. ProjFS keeps `PRJ_FILE_BASIC_INFO`, which spike S1 confirms. FUSE `getattr` reports the same values. A file the user writes gets real times, so it is stat-dirty and is hashed, which is correct.
- `entry_times` is compacted on a clean switch to a new tree when it exceeds 50k rows: rewrite all placeholders with the new `T` and clear the table.

### 10.2 Windows: ProjFS (`asc-vfs::projfs`)

1. **Binding:** our own thin layer over `windows` 0.62.2 (`Win32_Storage_ProjectedFileSystem`). `ProjectedFSLib.dll` is loaded at runtime with `libloading`, so `asc.exe` runs where the feature is off and falls back to plain mode with a notice. The MSI enables `Client-ProjFS` (it runs elevated). `asc doctor --fix` prints the admin command otherwise.
2. **One virtualization root per workspace**, with a stable instance GUID (the workspace uuid). The root directory is marked with `FILE_ATTRIBUTE_NOT_CONTENT_INDEXED`.
3. **Placeholders:** `VersionInfo.ContentID` = 33 bytes `kind ‖ id` (kind 1 = file `FileId`, 2 = LFS `sha256`, 3 = tree `TreeId`). `ProviderID` = the 16-byte workspace uuid. Sizes come from tree metadata (`lfs.size` for LFS). Times follow §10.1. Symlinks use `PrjFillDirEntryBuffer2` with symlink extended info when Developer Mode is on. Otherwise they are projected as text files containing the target, as git does with `core.symlinks=false`, and the projection config says so.
4. **Callbacks:**
   - `StartDirectoryEnumeration`/`GetDirectoryEnumeration`/`EndDirectoryEnumeration` answer from the tree at the base, honouring the search expression with `PrjFileNameMatch` and sorting with `PrjFileNameCompare`. Gitlink directories are composed (§10.6).
   - `GetPlaceholderInfo` looks the path up case-insensitively.
   - `GetFileData` serves **the ContentID recorded in the placeholder, never the current HEAD**. It reads `cache/f/<id>` (or fetches through `Tiered`) and writes with `PrjWriteFileData` in 1 MiB aligned buffers (`PrjAllocateAlignedBuffer`). Chunked files stream chunk by chunk. A fetch that is not local returns `ERROR_IO_PENDING` and completes with `PrjCompleteCommand`. Cancellation is honoured.
   - `QueryFileName` and `Notification` are also implemented.
   - No ProjFS call is ever made from inside a callback while holding a lock.
5. **Hydration policy [fix: missing-from-all, indexer/AV storms]:** `GetFileData` receives `TriggeringProcessId` and `TriggeringProcessImageFileName`. Every hydration is counted per process image (metric and `asc debug hydrations`). `asc.vfs.denyHydration` (default `SearchProtocolHost.exe;SearchIndexer.exe;SearchFilterHost.exe`) returns `STATUS_CANNOT_ACCESS`-equivalent (`ERROR_ACCESS_DENIED`) for those images. The installer adds Defender exclusions for `$ASC_HOME`, and `asc doctor` checks Defender exclusions and Dev Drive status (Defender is ProjFS-aware and does not hydrate placeholders it does not open, which spike S1 measures). Directory enumeration and `ReadDirectoryChangesW` watchers (VS Code, turbo, vite) never hydrate. The documented VS Code settings exclude `node_modules`, `dist` and `.turbo` from search and watching.
6. **Notifications** (registered for the root with mappings that mask redirected directories and the `.git` gitfile): `NEW_FILE_CREATED`, `FILE_OVERWRITTEN`, `PRE_DELETE`, `PRE_RENAME`, `FILE_RENAMED`, `HARDLINK_CREATED`, `FILE_HANDLE_CLOSED_FILE_MODIFIED`, `FILE_HANDLE_CLOSED_FILE_DELETED` and `FILE_PRE_CONVERT_TO_FULL` append journal hints. They are **hints only**.
   - **Directory renames [fix: missing-from-all]:** on `PRE_RENAME` of a directory, the provider hydrates the whole subtree under the source first, so every descendant is a real file before the rename proceeds. It then journals `dir-renamed-from <src>` and `dir-renamed-to <dst>`. Status treats every tracked path under `src` (from the base tree) as deleted and walks `dst` on disk as new or untracked files. Directory renames are rare, so the hydration cost is accepted.
7. **Reconcile on start [graft: performance]:** for each workspace, enumerate only on-disk (non-virtual) entries. ProjFS keeps placeholders, hydrated, full and tombstone entries as real directory entries; a recursive walk that skips virtual-only directories is bounded by what was materialised. Each path's state comes from `PrjGetOnDiskFileState`: Full or DirtyPlaceholder → journal `modified`; Tombstone → journal `deleted`; full files not in the tree → `created`. Then, if a `switch_intent` exists, the switch is re-run (§10.2.8).
8. **Switch, reset --hard, restore** (the daemon executes `Switch`):
   1. Persist `switch_intent {from_root, to_root, to_head, pending = changed paths}` **[graft: performance]**.
   2. Compute `leafdiff(from_root, to_root)` from metadata, plus directory-level changes.
   3. Apply git's switch rules to dirty paths (journal ∪ staged): if the target changes a dirty path, refuse (SAFE), three-way merge the content (MERGE, `--merge`), or overwrite (FORCE, `-f`/`reset --hard`).
   4. For each changed path that is not dirty, in parallel batches:
      - virtual (never materialised) → nothing on disk; enumeration serves the new tree after the base changes;
      - placeholder or hydrated → `PrjUpdateFileIfNeeded(new placeholder info, PRJ_UPDATE_ALLOW_DIRTY_METADATA | PRJ_UPDATE_ALLOW_READ_ONLY)`; for a deletion → `PrjDeleteFile(…, same flags)`;
      - **tombstone** (the user deleted it locally and the target changes or re-adds it) → in FORCE mode, or when the target equals the deletion, `PrjDeleteFile(path, PRJ_UPDATE_ALLOW_TOMBSTONE)` removes the tombstone so the path becomes virtual and enumerates from the new tree. In SAFE mode a tombstone counts as a dirty path (git semantics) **[fix: missing-from-all, tombstones]**.
   5. **Files held open** (update fails with `ERROR_SHARING_VIOLATION` or the update-failure reason `PRJ_UPDATE_FAILURE_CAUSE_*`): retry 3 times at 100/300/900 ms, then leave the path in `switch_intent.pending` **[graft: performance]**. The switch completes. Pending paths get index entries for the target oid with stat values that cannot match, so git and turbo hash them. `asc status` shows them as `not updated (in use by <process>)`, and the daemon retries pending paths every 5 s until done, then clears them.
   6. Bump the epoch and record `entry_times` (§10.1). Rewrite the index and `HEAD` (holding `index.lock` from step 1), clear the negative path cache (`PrjClearNegativePathCache`), and delete the intent if nothing is pending.

   A crash at any step: on restart the intent is re-run idempotently (`PrjUpdateFileIfNeeded` to the target version is idempotent), then the index and HEAD are rewritten.
9. **Dehydration:** clean hydrated placeholders in a workspace are re-virtualised with `PrjDeleteFile(PRJ_UPDATE_ALLOW_READ_ONLY)` (only clean `HydratedPlaceholder` state is eligible) when either: they have not been read for 3 days (from the hydration log), or the workspace's hydrated bytes exceed `asc.vfs.hydrationBudget` (default 500 MiB), least recently hydrated first **[graft: performance]**. It runs during idle (no notifications for 60 s) and on demand with `asc sparse dehydrate <pathspec>`.
10. **Long paths:** all internal paths use `\\?\` verbatim form. `core.longpaths=true` is set. `asc doctor` checks `HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled=1` **[fix: missing-from-all]**.

### 10.3 Linux: FUSE (`asc-vfs::fuse`)

- `fuser` 0.18, `default_permissions`. **Inode table persisted** in `state.db.inodes` (allocated sequentially, never reused while the workspace exists), so inode numbers are stable across restarts and the git index's ino matches **[graft: performance] [fix J1]**.
- `getattr` returns tree metadata, with times from §10.1 and mode from `Kind`. The kernel caches attributes and entries with a 1 h TTL. Switch invalidates changed entries (`notify_inval_entry`, `notify_inval_inode`).
- Opening a clean file read-only uses **FUSE passthrough** (kernel ≥ 6.9) to the `cache/f/<id>` file, which gives zero copies and zero extra disk. Without passthrough it reads through.
- Open-for-write, create, truncate, unlink and rename copy up into `wc/<ws>/overlay/` (the overlay holds modified files and records deletions). Every write passes through the daemon, so the journal is exact and needs no reconcile.
- Redirected names are served as symlinks to `$ASC_HOME/redirect/<ws>/<path>`, created on first lookup.
- CI virtual mode (later, after measurement): the daemon runs on the runner host, and the job sees the mount through an `rshared` bind mount.

### 10.4 Plain mode (`asc-wc::plain`; all OSes; CI default; macOS v1)

- Materialise the tree, or a cone-only sparse set (`asc sparse set <dir>…`; non-materialised entries carry skip-worktree in the index), from the cache using `reflink-copy` (APFS `clonefile`, btrfs/XFS, ReFS Dev Drive) or a plain copy. **Never hardlinks**, so an editor can never corrupt the shared cache.
- Status is a git-style stat check of index entries against the files (4.5k files take milliseconds), kept warm by `notify` while the daemon runs. Untracked files come from a walk with `.gitignore` rules (`ignore` crate).
- Switch updates only changed files: write to a temp file in the same directory, then rename. Mtimes are real.

### 10.5 Redirections

`.asc/redirections` (checked in) lists directory names. ASC's default is `node_modules`, `dist`, `.turbo`, `.next`, `target`, `.venv`, `__pycache__`.

- **ProjFS:** these directories are ordinary NTFS directories inside the root (they are not in the tree, so they are never virtual). Notification mappings mask them, so they never reach the journal. They live on the same volume as bun's cache, so bun's hardlinks keep working. No junctions are used, which removes the need for the junction-inside-root spike **[fix: missing-from-all]**.
- **FUSE:** symlinks to `$ASC_HOME/redirect/<ws>/<path>` on the same filesystem as `$ASC_HOME`.
- **Plain:** ordinary directories, excluded from status by `.gitignore`.

### 10.6 Submodules (gitlinks)

- Gitlinks are kept losslessly (`Kind::Gitlink`, `Change::Gitlink`).
- **Read-only composition [graft: performance; decision per judge 2]:** each gitlink's repo is configured with `asc.submodule.<path>.repo` (default derived from `.gitmodules` `url`, e.g. `../soma.git` → `asc/soma`). The VFS and plain mode project the nested repo's tree at the pinned commit (git oid → changeset through the nested repo's git map) at that path, from the same shared cache. The composed subtree is **read-only**: writes fail with `EACCES`/`ERROR_ACCESS_DENIED`. `asc status` ignores it. The projection index keeps the gitlink entry (mode 160000), as git does for an initialised submodule.
- To change the sdk, work in a separate workspace of `asc/soma` and update the pin with `asc submodule set-pin <path> <rev>` (writes a `Change::Gitlink`). `asc submodule status` lists pins. `asc submodule update` is a no-op with a notice.
- This removes the 27 GB of per-worktree submodule git dirs and `submodules: recursive` from CI without merging repositories. **Folding `soma` into `dds` remains a user decision** (§19). If chosen, it is one changeset, and composition stops mattering.

---

## 11. CLI (`asc`)

### 11.1 Principles

- Same verbs, flags, revision syntax, exit codes (0, 1 for diff-like or "nothing to commit", 128 fatal, 129 usage) and output formats as git 2.55: `--porcelain[=v1|v2]`, `-z`, `--format`/`--pretty` placeholders (`%H %h %T %t %P %p %an %ae %ad %ar %at %cn %ce %cd %cr %ct %s %b %B %d %D %n %%`, colors), `--name-status`, `--stat`.
- **Unsupported flags fail loudly:** `asc: <command>: flag '--x' is not supported yet` (exit 129). They are never ignored.
- Global options: `-C <dir>`, `-c key=value`, `--no-pager`, `--git-dir`/`--work-tree` (read-only uses only). Plus `alias.*` expansion, `asc-<name>` plugins on `PATH`, git's pager rules (`core.pager`, `GIT_PAGER`, `PAGER`, `less -FRX`), `color.ui`, `NO_COLOR`.
- Config comes from git's files (`gix-config`): system, `~/.gitconfig`, XDG, the projection's local config and `config.worktree`. asc keys live under `asc.*`. Credentials use `git credential fill/approve/reject` (Git Credential Manager on Windows).
- **Ids in output:** `asc.displayIds = auto|git|asc|both`. `auto` = `git` for repos whose server `id_mode` is `sha1` and for git-remote repos, otherwise `asc`. Input always accepts both kinds of id.
- `asc help git` prints the git→asc cheat sheet (§11.3). `asc help <cmd>` shows asc-specific notes and then git's manual.

### 11.2 Command map

Where: **L** local; **L+f** local, fetching missing objects; **S** server RPC when the remote is a server (otherwise the same engine code runs locally).

| git command | asc behaviour | Where |
|---|---|---|
| `init [dir] [-b <b>] [--store <url> [--refs local]]` | local-only repo plus a plain workspace. `--store` puts objects (and BucketRefs if probed) in a bucket | L |
| `clone <url> [dir]` | §9.3. `--mode virtual\|plain` (default virtual where available), `-b`, `--sparse`, `--no-checkout`, `--reference`, `--origin`. `--depth`/`--filter` are accepted (always lazy) | L+f |
| `worktree add\|list\|remove\|prune\|lock\|unlock\|move` | §9.3; `--ephemeral --ttl` | L |
| `status` | `-s`, `-b`, `--porcelain[=v2]`, `-z`, `-u[no\|normal\|all]`, `--ignored`, `--ahead-behind` | L |
| `add`, `rm`, `mv` | edit the index (`-p`, `-u`, `-A`, `-N`, `--cached`, `-f`, `-n`); `mv` records `copy_from` | L |
| `restore`, `reset` | `--staged`, `--worktree`, `--source`; `reset --soft\|--mixed\|--hard [<rev>] [-- paths]`; virtual uses the daemon switch | L+f |
| `commit` | §9.7. `-m`, `-F`, `-a`, `--amend`, `--fixup`, `--squash`, `--no-verify`, `--allow-empty`, `-S`, `--author`, `--date`, `-v`, `-e`/`--no-edit` | L |
| `switch`, `checkout` | `-c`/`-C`, `--detach`, `--orphan`, `--merge`, `-f`, `-` ; `checkout <rev> -- <paths>` restores | L+f |
| `branch` | `-d`/`-D`, `-m`/`-M`, `-c`, `-u`/`--set-upstream-to`, `--unset-upstream`, `-r`, `-a`, `-v`/`-vv`, `--list <pat>`, `--contains`, `--merged`, `--format` | L |
| `tag` | lightweight and annotated (`-a`, `-m`, `-s`, `-d`, `-l`, `-f`, `-n`) | L |
| `fetch` | refs plus changesets only (§6.3). `--prune`, `--all`, `--tags`, `--prefetch <pathspec>` | L+f |
| `pull` | fetch then merge or rebase per `pull.rebase`/`pull.ff`; `--rebase`, `--ff-only`, `--no-rebase` | L+f |
| `push` | §6.3/§8.3; `-u`, `--force`, `--force-with-lease[=ref:expect]`, `--delete`, `--tags`, `--all`, `-o`, `--land`, `--dry-run`; `refs/for/<b>` (AGit) | S |
| `merge` | local engine; `--no-ff`, `--ff-only`, `--squash`, `--abort`, `--continue`, `-m`, `--no-commit`, `-X ours\|theirs`, `--conflict` style via `merge.conflictStyle` | L+f |
| `rebase` | local engine; `--onto`, `--continue`, `--abort`, `--skip`, `-i` (git's todo format and editor), `--autosquash`, `--exec`, `--update-refs` | L+f |
| `cherry-pick`, `revert` | local engine; `-n`, `-x`, `-m <n>`, `--continue/--abort/--skip` | L+f |
| `stash` | `push [-u] [-m] [-- paths]`, `pop`, `apply`, `list`, `show [-p]`, `drop`, `clear`, `branch`. git's stash structure (W with parents [HEAD, I], optional U) on `refs/stash`, plus reflog | L |
| `log` | `--oneline`, `--graph`, `--format`, `-p`, `--stat`, `--name-status`, `--follow`, `-- <path>`, `--since`/`--until`, `--author`, `--grep`, `-n`, `--first-parent`, `--all`, `--decorate`, `--reverse`, ranges | L / S |
| `show` | `<rev>`, `<rev>:<path>`, `--stat`, `--name-only`, `--format` | L+f / S |
| `diff` | worktree/index/HEAD (L); `<a> <b>`, `<a>...<b>` (L, or S with `--remote <repo>` and no checkout); all formats; `--cached`, `-M`, `--no-index` (delegated) | L+f / S |
| `blame` | `-L`, `--porcelain`, `--incremental`, `-w`, `-M`, `--reverse` (S when a server remote exists, else local). Dirty files are blamed against the working copy locally | S / L |
| `grep` | clean files: server `Grep` at base (no hydration); dirty files: local. Local-only repos: local over the tree | S+L |
| `remote` | `add`, `remove`, `rename`, `set-url`, `get-url`, `-v`, `show`, `prune` | L |
| `config` | git config files; `--global`, `--local`, `--worktree`, `--get`, `--list`, `--unset` | L |
| `rev-parse`, `rev-list`, `merge-base`, `show-ref`, `for-each-ref`, `symbolic-ref`, `update-ref`, `describe` | native. `rev-parse --git <rev>` / `--asc <rev>` print either id | L / S |
| `ls-files`, `ls-tree`, `cat-file`, `diff-tree`, `diff-index`, `hash-object`, `check-ignore`, `check-attr` | native, from metadata; `ls-tree --format` supports `%(objectname)` (git) and `%(ascid)` | L+f |
| `reflog` | `state.db` ref log; `--remote` shows the server log | L |
| `clean` | `-n`, `-f`, `-d`, `-x`, `-X`; virtual mode walks only materialised directories | L |
| `bisect` | `start/good/bad/skip/reset/run/log`; midpoints from the graph; the working copy switches each step | L+f |
| `archive` | server `Archive`, or local streaming | S / L |
| `sparse-checkout` | `set`/`add`/`list`/`disable` = cone in plain mode; in virtual mode = the prefetch profile | L |
| `submodule` | `status`, `set-pin`; `update`/`init` are no-ops with a notice (§10.6) | L |
| `gc`, `maintenance`, `prune`, `fsck`, `count-objects`, `repack` | `gc` = cache eviction plus dehydration; `fsck` verifies `store/`/`cache/` hashes and workspaces; `repack`/`prune` print "not needed in asc" and exit 0 | L |
| `format-patch`, `apply`, `am` | local engine | L |
| `shortlog`, `name-rev`, `whatchanged`, `range-diff`, `cherry`, `notes show` | delegated to git on the projection (§9.6) | L |
| `filter-branch`, `filter-repo`, `replace`, `svn`, `p4`, `send-email`, `daemon`, `instaweb`, `lfs` | not supported (`lfs`: "not needed, large files are native") | – |

asc-only operational commands: `asc daemon [start|stop|status]`, `asc serve`, `asc import`, `asc export`, `asc auth login|status|logout`, `asc doctor [--fix] [--store <url>] [--report-reclaimable]`, `asc prefetch`, `asc sparse dehydrate`, `asc debug stats|hydrations|trace`, `asc ci checkout|affected|changed|warm`, `asc upgrade`, `asc internal …` (hooks, fsmonitor, reconcile).

### 11.3 Cheat sheet (`asc help git`, also `docs/CHEATSHEET.md`)

The rule is to use your git commands with `asc` in front. Differences: `clone` and `worktree add` are instant and virtual; `fetch` downloads no files; `push` to `master` lands (it rebases on the server if paths don't overlap); `submodule update` is unnecessary; `lfs` is unnecessary; `git` itself still works inside an asc working copy (commits made with git are picked up).

---

## 12. Turborepo and CI

### 12.1 Hashing without hydration

Turbo 2.10's hot path reads `.git/index` with `gix-index`, stat-compares each entry, and trusts the stored SHA-1 when the stat matches. The projection index carries the git oid and the exact stat the VFS reports (§9.4, §10.1), and no entry is racy. On a clean virtual workspace, `turbo run --dry=json` therefore reads no tracked files except manifests (which turbo parses) and produces the same hashes as a git clone. Turbo's other git calls (`rev-parse`, `ls-files --others`, `diff-tree --merge-base`, `show <base>:bun.lock`) run the real git against the projection. The only content fetched is `bun.lock` at the base, and only when it changed. `.gitattributes` must not be tracked (ASC's is comment-only; delete it, §12.5).

### 12.2 Plan job (no checkout)

```yaml
plan:
  runs-on: docker
  steps:
    - run: asc ci changed --repo "$ASC_REPO" --base "$BASE" --head "$HEAD" --rules .asc/ci.toml >> "$GITHUB_OUTPUT"
    - run: asc ci affected --repo "$ASC_REPO" --base "$BASE" --head "$HEAD" --rules .asc/ci.toml >> "$GITHUB_OUTPUT"
```

- `asc ci changed`: one `Diff(NAMES, merge_base=true)` RPC, then glob rules from `.asc/ci.toml` (`[switches] rust = ["apps/analysis-pipeline/**", "**/*.rs", "**/Cargo.toml"]`, `docs_only = …`). It writes `key=value` lines. This replaces Forgejo `paths:` filters and the docs-only short-circuit.
- `asc ci affected`: (1) a plain sparse checkout into a temp dir of manifests only (`**/package.json`, `turbo.json`, `bun.lock`, `**/Cargo.toml`, `**/pyproject.toml`, `package.json` workspaces config), with the projection carrying the base refs and a shallow boundary at the merge base; (2) runs `turbo ls --affected --output=json` with `TURBO_SCM_BASE=<merge-base git sha>`, using the turbo version pinned in `.asc/ci.toml` (`turbo = "2.10.11"`), which asc downloads from the npm registry tarball `@turbo/<platform>`, verifies against the `integrity` in `bun.lock` (or a pinned sha512 in `ci.toml`), and caches in `$ASC_HOME/tools/turbo-<ver>`; (3) maps packages to switches with `.asc/ci.toml` `[packages]` rules; (4) writes outputs. No install, no build. If turbo fails, it outputs "all affected" and warns **[fix: integration's unspecified turbo binary]**.

### 12.3 Build/test jobs: `asc ci checkout`

```yaml
- uses: asc/asc-actions/checkout@v1     # mode: plain (default) | virtual
  with: { base: "${{ github.base_ref || github.event.before }}" }
- run: bun install --frozen-lockfile
- run: turbo run build check-types test --affected
```

`asc ci checkout --repo R --rev <head> --base <base> --dir . [--mode plain] [--sparse …]`:

1. authenticates with the Actions task token (`GITHUB_TOKEN`);
2. resolves head and base, and `MergeBase`;
3. builds `Tiered<HostCacheRO, JobCache, Remote>`. **`HostCacheRO`** is the runner host's `/var/cache/asc/cache`, bind-mounted **read-only** into the job container. **`JobCache`** is `$RUNNER_TEMP/asc-cache`, writable and discarded after the job. Every read is BLAKE3-verified **[fix J1: CI cache poisoning]**;
4. materialises the tree (plain, reflink or copy) and writes the projection (index with real stat; commits merge-base..head; `shallow` = {merge-base}; `refs/remotes/origin/<base>`; trees for base, merge-base and head; seeded manifest blobs at base);
5. exports `TURBO_SCM_BASE=<merge-base git sha>`, `TURBO_SCM_HEAD=HEAD`, `TURBO_API=<asc serve public_url>`, `TURBO_TOKEN=<task token>`, `TURBO_TEAM=<owner>.<repo>`, and `TURBO_CACHE=local:rw,remote:rw` (trusted) or `local:rw,remote:r` (PRs).

There is no submodule clone (sdk composed via the nested repo, §10.6) and no LFS smudge.

**Host cache warmer:** `asc ci warm --repo R --ref master` runs on each runner host as a systemd timer every minute, as a host-side user that owns `/var/cache/asc`. It fetches trunk's changesets, trees and files into the host cache. Only this trusted process writes the host cache. PR jobs never can.

**Virtual CI** (FUSE on the host, the job sees the mount through a bind mount) is built later, only if measurements show plain checkout (≈ 1–3 s for 119 MiB from a warm cache) is a bottleneck.

### 12.4 Turbo remote cache on the same store (`asc serve /v8`)

- Implements `GET /v8/artifacts/status`, `HEAD|GET|PUT /v8/artifacts/{hash}`, `POST /v8/artifacts` (batch query) and `POST /v8/artifacts/events`. Objects are stored at `turbo/<owner>.<repo>/<scope>/<hash>` with `.meta` JSON (`duration`, `tag`, `sha`, `dirty_hash`, `size`, `at`). `x-artifact-tag` (the HMAC signature) is stored opaquely and returned. ASC enables `remoteCache.signature: true`.
- **Scopes:** `trusted` and `pr-<n>`. Reads try the caller's scope, then `trusted`. Writes go only to the caller's scope.
  - Stage 2+ (fork authz available): the authz response says whether the token is an Actions task for a push to a protected branch (→ `trusted`) or for PR `n` (→ `pr-<n>`). Developer PATs are read-only on `trusted` and write to `dev-<user>`.
  - Stage 1 (no fork): writes to `trusted` require a token in `turbo.trusted_tokens` (a secret configured only on the trunk-deploy environment). Others write to `pr-<n>`, with `n` taken from the `x-asc-pr` header, which is untrusted and only isolates PRs from each other. Known limitation: same-repo PR workflows can read repository secrets, so the trusted token must live in a protected Forgejo environment.
- Retention: `pr-*` 14 days and `trusted` 60 days, by last access (a daily sweep).
- `OPTIONS` preflight returns presigned S3 URLs for artefacts > 8 MiB when the store is S3.
- Developers set `TURBO_API` to the same server, which gives one cache across all their worktrees and CI's trusted scope (read).

### 12.5 ASC changes independent of asc (do now)

Delete the comment-only `.gitattributes`. Replace `"env": ["*"]` on `build` with an explicit list. Add `check-types` to every package. Add `cargo-check`/`cargo-test` turbo tasks for `apps/analysis-pipeline` (inputs `**/*.rs`, `**/Cargo.toml`, `Cargo.lock`). Bind-mount the bun cache on runners. Merge `ci-intelligence.yml` into `ci.yml`. Use `UV_LINK_MODE=hardlink`.

---

## 13. Forgejo: full storage replacement

### 13.1 Stages

| Stage | Source of truth | Forgejo reads | Forgejo writes | Fork needed | Rollback |
|---|---|---|---|---|---|
| 0 git-remote | Forgejo git | git | git | no | uninstall asc |
| 1 mirror | Forgejo git | git | git | no (reverse proxy for `/.well-known/asc`) | stop asc serve |
| 2 asc truth | **asc** | git **bridge** (eager) | temp-repo pushes into the bridge, imported by pre-receive | patches 1–9 (K01–K09) | `storage_backend=git` (the bridge is current) |
| 3 Seam A | asc | **asc** (`ascbackend`, id_mode GIT) | as Stage 2 | + K10, K11 | `storage_backend=dual`/`git` |
| 4 Seam B | asc | asc | **asc** (`Commit`/`Merge`/`Apply`/`UpdateRefs`) | + K12, K13 | as above while the bridge is eager |
| 5 Actions | asc | asc | asc | + K14 | – |
| 6 IDs (optional, later) | asc | asc, `object_format=asc` | asc | + K16 | migration down script |

At Stage 4 no bare git repository is a source of truth. The bridge becomes `lazy`: it is built on the first legacy git request, for `git clone`, `actions/checkout`, push mirrors or external submodule consumers.

### 13.2 Two independent switches [graft: integration D2]

- `repository.storage_backend`: `git` | `dual` | `asc`. This is a new column (K08). Selection happens in one place, `modules/gitrepo.OpenRepository`. `dual` serves from `gitcli` and calls `ascbackend` on a 10% sample of read calls (configurable), comparing results and emitting `forgejo_asc_shadow_mismatch_total{method}` plus a log line. It never changes a response.
- `repository.object_format_name`: `sha1` (unchanged) or `asc` (Stage 6). In `sha1` mode, `ascbackend` sends `id_mode = GIT`, and `asc serve` translates ids at its boundary through `git_map`. No database rows change at cutover. Commit statuses, PR merge bases, comments, Actions runs and links keep working.

### 13.3 Patch stack (Go, `D:\dev\forgejo`, branch `asc/v16`, one commit per unit)

| Unit | Patch | Behaviour change |
|---|---|---|
| K01 | Fork baseline and CI (`make test-backend`, sqlite and pg integration) | none |
| K02 | Path hiding: 34 `git.OpenRepository(path)` callers → `gitrepo.OpenRepository`; `repoPath string` signatures (blame, diff, log-name-status) → `*Repository` | none (upstreamable) |
| K03 | Seam A: `Backend` interface (about 30 methods, as in `forgejo.md` §2) inside `git.Repository`; current code moves to `modules/git/gitcli` | none (upstreamable) |
| K04 | Seam B: `modules/repostore.Writer` (`CreateCommit`, `Merge` incl. dry run, `CherryPick`, `Revert`, `ApplyPatch`, `UpdateRefs`, `Init`, `Fork`, `Delete`, `Size`); temp-repo code moves to its `gitcli` implementation | none |
| K05 | Remove raw `git.NewCommand` outside `modules/git/gitcli` and `services/ascbridge`; add the `build/check-raw-git` linter | none (upstreamable) |
| K06 | `modules/ascproto` (buf-generated from `source-control/proto`, committed) plus `modules/ascclient` (connection, auth, retry, LRU by immutable id) | none |
| K07 | `routers/private/asc.go`: `POST /api/internal/asc/authz` → `{allowed, user_id, username, perm, deploy_key_id, is_actions_task, task: {run_id, event, ref, pr_index, is_fork_pr}, branch_rules: {require_signed: bool}}`; serve `/.well-known/asc` from `[asc] SERVER_URL` | behind `[asc] ENABLED` |
| K08 | `storage_backend` column, migration, `[asc]` settings, backend selection in `gitrepo.OpenRepository` (gitcli for `git`/`dual`) | behind the flag |
| K09 | Stage 2 hook glue: in `HookPreReceive`, for `storage_backend=asc` and git-origin pushes, after policy checks call `ImportGitPush(quarantine paths, updates)`; an asc error rejects the push. In `HookPostReceive`, for `storage_backend=asc`, process only `PushTrigger="asc"` calls (asc's outbox is the single fan-out) and return OK for git-origin calls | behind the flag |
| K10 | `modules/git/ascbackend`: the `Backend` over `asc.v1.Repo` (`ReadChangeset`, `ReadTree`, `ReadFile`, `Stat`, `ListRefs`, `Log`, `MergeBase`, `IsAncestor`, `LastCommits`, `Blame`, `Diff`, `Archive`, `Grep`, `CheckAttr`, `Resolve`); `services/gitdiff` parses `Diff(PATCH)` text unchanged; language stats computed in Go over `ReadTree` plus `ReadFile` | `storage_backend=asc` |
| K11 | `modules/git/dual` shadow backend | `storage_backend=dual` |
| K12 | `modules/repostore/asc`: one RPC per Writer method (`Commit`, `Merge`, `Apply`, `UpdateRefs`, `RepoAdmin`); `CopyClosure` for cross-repo PRs; `refs/pull/<n>/head` via `UpdateRefs` after `CopyClosure` | `storage_backend=asc` |
| K13 | `services/ascbridge`: `bridgePath(repo)` used by `githttp.go`, `cmd/serv.go`, `services/mirror`, migrations; `bridge=off|lazy|eager`; lazy build = `asc export --git <bridge>`; LFS batch API served from asc content via the `x/sha256` alias (presigned URLs); repo lifecycle (fork O(namespace copy), delete, size = logical) | `storage_backend=asc` |
| K14 | Actions: `paths:` filters via `Diff(NAMES)`; context gains `asc_server_url`, `asc_changeset`, `asc_base_merge_base`; `github.sha` stays the git SHA in `sha1` mode | `storage_backend=asc` |
| K15 | Integration suite matrix: `git`, `dual` (zero mismatches required) and `asc` | test only |
| K16 | ID mode `asc`: `object_format_name=asc`, a one-off migration rewriting stored SHAs through `GitMap`; every input accepts both forms | per repo, optional |

**Patch-stack upkeep [fix: missing-from-all]:** budget 2–4 engineer-days per upstream minor release (most conflicts are in `modules/git`, where K03 moved code verbatim to `gitcli`; `build/rebase-map.sh` lists upstream changes to `modules/git/X.go` against `gitcli/X.go`). K02, K03 and K05 are offered upstream as a "storage backend interface"; each one accepted shrinks the stack. The fork moves from v16 to v17 once K01–K05 are stable.

### 13.4 Forgejo lifecycle mapping

| Forgejo operation | Stage ≥ 4 behaviour |
|---|---|
| Create repo | `RepoAdmin.init` (new namespace) |
| Fork | `RepoAdmin.fork` (namespace copy job; the UI shows "preparing" until done) |
| Transfer/rename | `RepoAdmin.rename` |
| Delete | `RepoAdmin.delete` |
| Size/quota | `RepoAdmin.size` → logical size used for quota |
| Wiki | a second asc repo `<owner>/<name>.wiki` |
| Mirrors (pull) | git fetch into the bridge, `ImportGitPush`-equivalent import, `UpdateRefs` |
| Mirrors (push) | bridge forced `eager`, git push from the bridge |
| AGit `refs/for/*` | `UpdateRefs` to `refs/for/<b>`; asc calls Forgejo `proc-receive` instead of pre/post-receive for that ref |
| Archive download | `Archive` |
| Code indexer | `Diff(NAMES)` between indexed and new tip, plus `ReadFile` |
| Repo doctor | `RepoAdmin.verify` |

---

## 14. Migration and import (crate `asc-git`)

### 14.1 Import

`asc import --git <path|url> [--repo <name>] [--incremental]` (also used by git-remote mode, mirror mode, reconcile and `ImportGitPush`):

1. Use `gix` to list commits not yet in the git map, in topological order (parents first).
2. For each commit: diff its git tree against its first parent's tree (gix tree diff, no rename detection), giving the changed leaves. Ingest new blobs (BLAKE3, SHA-1 kept, chunking over 4 MiB). LFS pointer blobs (`version https://git-lfs.github.com/spec/v1`, `oid sha256:`, `size`) become entries with `lfs: Some(…)` and **pointer identity** (§2.5). Gitlinks become `Change::Gitlink`.
3. Build asc trees bottom-up from the git trees. Each `Entry.git` is the actual git oid. Any git tree whose canonical render differs from its raw bytes gets an override `o/<oid>`.
4. Build the changeset with import canonicalisation (§2.9) and validate it (§2.6).
5. Write `gen`, `gitcommit` and the git map rows. **Assert `gitcommit(changeset) == original commit oid`**, and abort on mismatch.
6. Tags: annotated tags become `TagRecord`s, canonical or raw.

LFS content: the server (mirror, Stage 1+) copies each referenced LFS object from Forgejo's LFS store into the namespace as content plus an `x/sha256` alias, in a resumable background job (25.6 GiB for ASC). Clients never need it unless a historical LFS file is read.

ASC scale: 5,502 commits and about 50k blobs (6.99 GiB raw) import in minutes. The 4.6 GB unreachable cruft pack is never read.

### 14.2 Export

`asc export --git <dir> [--refs <glob>]` and the bridge writer render blobs (file bytes, or pointer bytes for LFS entries), trees (override or render) and commits (raw or canonical) for every changeset not yet in the target. They write loose objects, or one undeltified pack when there are more than 1,000 objects, then index it with `git index-pack`. Refs are set with `git update-ref --stdin`. Export is deterministic, so the same changeset always yields the same SHA.

### 14.3 ASC cutover runbook (unit O03)

1. **Prep (no asc):** §12.5 hygiene. Optionally `git gc --prune=now` on the server mirror copy to drop cruft (not required).
2. **Stage 0 (week ≈ 6):** pilot users install asc (MSI). `asc clone https://forge.example.org/asc/dds D:\work\dds --reference D:\dev\monorepo\.git`. Agents use `asc worktree add --ephemeral`. `asc doctor --report-reclaimable` lists the old `.git` (58 GB), the 49 git worktrees and the submodule git dirs. **The user deletes them; asc never does.**
3. **Stage 1 (week ≈ 10):** deploy `asc serve` (Postgres schema `asc` on Forgejo's DB server; Garage bucket `asc`, or local disk). `RepoAdmin.init mode=mirror` for `asc/soma` and `asc/dds`. Add the webhook and the proxy route for `/.well-known/asc`. Run `asc serve admin verify-import` (every commit's export equals git, which is required). Client workspaces switch automatically by discovery: ids are identical, so nothing is re-fetched. CI moves to `asc-actions/checkout` and the plan job.
4. **Stage 2 (week ≈ 14):** deploy the fork with K01–K09. Freeze pushes for minutes, run a final mirror import, and flip `repos.mode=primary`, `forgejo_reads=git`, `bridge=eager`, `storage_backend=asc`. The existing bare repo is now the bridge. **Rollback drill on staging first:** set `storage_backend=git`, `repos.mode=mirror` (once the outbox has drained). The bridge is current and SHAs are identical.
5. **Stage 3 (week ≈ 18):** `storage_backend=dual` for a week (zero mismatches above noise), then `asc` with `forgejo_reads=asc`.
6. **Stage 4 (week ≈ 22):** K12–K13; bridge → `lazy`. Full storage replacement.
7. Later: Stage 6 IDs (optional); fold soma (user decision); artefacts into asc (user decision).

---

## 15. Security

### 15.1 Identity and authorisation

- **Identity is Forgejo's.** Clients present a Forgejo PAT, OAuth token or Actions task token (`git credential fill` with `host = <forge host>`; `asc auth login` stores a PAT via `git credential approve`).
- `asc serve` authorises every repo-scoped call. Stage 1 uses Forgejo's public `GET /api/v1/repos/{owner}/{repo}` with the token, checking `permissions.pull`/`push`. Stage 2+ uses the internal authz endpoint (K07). Positive answers are cached for 60 s and negative ones for 5 s, keyed by `(sha256(token), repo, mode)`.
- **Object access is per namespace** (one per repo, §3.3). Content ids are 256-bit, but the namespace rule, not unguessability, is the control.
- `HOOK_MODE_SKIP_PRE` and `ImportGitPush` require Forgejo's internal token, over the private network or mTLS.
- **CI:** a job's token is bound to its repo. With the host cache read-only and the job cache per job, cross-job poisoning is impossible. Every tier verifies BLAKE3.
- **Daemon:** a current-user-only pipe/socket. The daemon never serves another user.

### 15.2 Integrity and policy

- All object reads are verified (§3.2). The server re-derives every client-supplied git field (§8.4) and validates every changeset (§2.6).
- Path safety: `checkout_safe` on every materialisation (git's NTFS/HFS protections); `windows_safe` on new changesets (§2.8).
- Policy stays in Forgejo's pre-receive: branch protection, protected files, signed-commit rules and quota run on the exact changesets that will be referenced (rewritten ones for land).
- Signing: git-format signatures over canonical commit bytes (§2.9), verified by Forgejo and git unchanged. Turbo artefacts carry HMAC tags.
- Secrets in drafts never leave the machine before `asc push` (no write-behind).
- Supply chain: pinned crate versions, `cargo deny` (licences: MIT/Apache/BSD/ISC/Zlib/Unicode only; no GPL; Sapling, Mononoke and EdenFS are ideas only), `cargo audit` in CI, release binaries signed (Authenticode on Windows, minisign for `asc upgrade`).

### 15.3 Data at rest and backup/DR [fix: missing-from-all]

- Bucket server-side encryption. Logs carry no file content.
- **Backup:** Postgres PITR (WAL archiving, 14 days) plus bucket versioning or replication. Because objects are written before refs and never deleted on the write path, restoring Postgres to time T always yields refs whose closure exists in the bucket.
- **Restore drill** (quarterly, `docs/runbooks/restore.md`): restore Postgres to T on staging, point an `asc serve` at the bucket, run `asc serve admin verify --all` (every ref's closure exists; git map round-trips), then rebuild the bridge with `asc export`. The drill passes when verify exits 0.

---

## 16. Operations

### 16.1 Observability

- `tracing` everywhere; JSON logs; optional OTLP (`ASC_OTLP_ENDPOINT`); `traceparent` propagated from Forgejo through gRPC.
- **Server `/metrics`:** RPC latency and errors per method; bytes in and out; store latency per backend; ref CAS conflicts; land retries and conflicts; hook latency and failures; outbox lag and failures; mirror import lag; turbo hit ratio per scope; cache hit ratios.
- **Client:** `asc debug stats [--json]` (cache size and hit ratio, hydrations per workspace, journal size, daemon uptime); `asc debug hydrations [--since] [--by-process]`; `ASC_TRACE=1` (per-command RPC timings); `asc doctor`.

### 16.2 Distribution, upgrades and version skew [fix: missing-from-all]

- Release CI builds `x86_64-pc-windows-msvc`, `x86_64/aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`.
- **Windows:** an MSI (`cargo-wix`) installs `asc.exe`, hardlinks `git-remote-asc.exe` and `asc-fsmonitor.exe`, adds `PATH`, enables `Client-ProjFS`, adds Defender exclusions for `$ASC_HOME`, and registers the per-user logon task. A winget manifest is published on ASC's Forgejo releases. Fleet rollout is through winget or GPO.
- **Linux:** a tarball, `install.sh` (installs the systemd user unit), and a container image `asc:<ver>` for CI.
- `asc upgrade` downloads the latest signed release from the configured releases URL, verifies the minisign signature and replaces itself. The daemon restarts on the next CLI call (§9.1).
- **Skew policy:** the server's `Version.min_client` is enforced (`CLIENT_TOO_OLD`, with an upgrade hint). Servers support the current and previous minor clients. Envelope versions only change with a major release. The Forgejo fork checks `Version.features` at startup and refuses to enable `[asc]` when a required feature is missing.

---

## 17. Testing strategy

| Layer | How |
|---|---|
| `asc-core` | proptest round trips (`decode(encode(x)) == x`, `encode(decode(b)) == b`); frozen golden vectors for ids and envelopes; git rendering against `git hash-object`/`mktree`/`commit-tree`/`mktag` outputs |
| `asc-store` | one conformance suite for every `Objects` and `Refs` impl: InMemory and LocalFileSystem always; MinIO and Garage (`--features it-s3`, docker compose); Postgres (`--features it-pg`); a concurrent CAS race (100 tasks, one winner per round); BucketRefs retry and ABA tests with a fault-injecting store; the put-fallback test with a store that rejects `If-None-Match` |
| `asc-engine` | tree apply/diff vs naive oracle; merge vs `git merge-file` and `git merge-tree --write-tree` goldens; diff patches pass `git apply --check` and ≥ 99% byte-equal; `merge_base` vs `git merge-base` on generated DAGs; land disjointness truth table (same path, case-fold, file-vs-dir prefix) |
| `asc-git` | import→export byte identity on fixture repos (signed commits, odd modes, LFS, gitlinks, encodings); native→export→import id equality; `asc_roundtrip` (`#[ignore]`, reads `D:\dev\monorepo` read-only, asserts all 5,502 SHAs) |
| `asc-wc`, `asc-vfs` | per OS: mount, enumerate, hydrate (counter), write→journal, switch with dirty/open files/tombstones, directory rename, daemon kill and reconcile, crash mid-switch (intent replay), case-insensitivity, unrepresentable names; ProjFS on a Windows runner with ProjFS enabled; FUSE in Linux CI or WSL |
| CLI | `crates/asc/tests/scenarios/*.t`: each scenario runs the same script under `git` and `asc` against identical fixtures and compares porcelain output |
| Turbo contract | `tests/turbo/`: pinned turbo 2.10.11; `--dry=json` hashes equal a git clone's; hydrations ⊆ manifests; one edit changes only its tasks; `turbo ls --affected` equals a git clone's; racy run has 0 verified entries; worktrees share `repos/<id>/.turbo/cache`; run on every turbo minor |
| Server | in-process `asc serve` over InMemory plus SQLite with a fake Forgejo (authz, hooks); every RPC; auth-denied paths; land races; outbox crash injection (kill between CAS and effects); Stage 2 quarantine ordering |
| Forgejo | upstream unit and integration suites on `git`, `dual` and `asc` (K15) |
| End to end | `deploy/compose.yml` (Garage, Postgres, asc serve, Forgejo fork, forgejo-runner): clone, push, PR, merge, plan job, cached turbo build, bridge `git clone`, legacy `git push`, rollback flip |

---

## 18. Roadmap, effort and rollout

Team assumption: 2 Rust engineers, 1 Go engineer (from week 8), and implementation agents (run at effort ≤ medium). Weeks are calendar weeks from start.

| Phase | Weeks | Units | Ships | Exit criteria |
|---|---|---|---|---|
| 1 Local-only asc | 1–4 | U01–U28 | `asc init/add/commit/status/diff/log/branch/switch/merge/rebase/stash/blame` on local disk, plain mode | scenario suite green vs git |
| 2 Remotes and S3 | 4–5 | U29–U32 | bucket remotes (S3/MinIO/R2/local), shared BucketRefs | conformance on MinIO; two machines push/fetch one bucket |
| 3 Git interop | 5–7 | U33–U40 | import/export, projection, git-remote mode, `git-remote-asc`, IDE `git commit` import | ASC round trip 0 mismatches; turbo contract on plain mode |
| 4 Server | 7–10 | U41–U51 | `asc serve` (mirror, primary, land, reads, mutations, `/v8`) | Stage 1 live for pilot; plan job < 10 s |
| 5 Daemon and ProjFS | 8–12 | U52–U58 | virtual worktrees on Windows | Windows exit: worktree add < 1 s; turbo hashes equal; 0 hydrations beyond manifests |
| 6 CI | 10–12 | U61–U64 | `asc-actions`, host warmer | typical PR ≤ 8 runner-minutes |
| 7 FUSE and submodules | 12–14 | U59–U60 | Linux virtual, sdk composition | WSL contract green |
| 8 Forgejo Stage 2 | 8–15 | K01–K09 | asc is truth | rollback drill passed on staging |
| 9 Forgejo Stage 3–5 | 15–22 | K10–K15 | full storage replacement | Forgejo suite green on `asc`; a dual week with 0 mismatches |
| Ops | ongoing | O01–O05 | GC, release/MSI, runbooks, docs, bench | – |

**Pilot and rollout [fix: missing-from-all]:** (1) the agent fleet first (the heaviest worktree users; ephemeral worktrees), (2) 2–3 volunteer developers, (3) all developers, then (4) CI.

Success metrics per cohort: disk per worktree (target < 50 MiB before builds), `worktree add` p95 < 1 s, `status` p95 < 100 ms, hydrations per workspace per day, turbo cache hit rate unchanged or better, PR wall time, and support issues per week. A cohort advances when the metrics hold for one week.

---

## 19. Open decisions for the user, and spikes

**User decisions (defaults in force until decided):**

1. Fold `asc/soma` into `dds`? Default **no**: the sdk is composed read-only (§10.6).
2. Commit `ops/artefacts` (about 200 GiB, sha256-addressed in Garage) as asc large files? Default **no**. It is possible later via the `x/sha256` alias, with no copy.
3. Flip displayed ids to asc (Stage 6)? Default **no**.
4. Quota semantics: logical size (default) vs namespace-unique size.

**Spikes (each 1–2 days, in this order; owner = the unit that depends on it):**

| # | Spike | Blocks |
|---|---|---|
| S1 | Enable ProjFS here; a 1k-file provider. Confirm: hydration keeps `PRJ_FILE_BASIC_INFO` times; turbo's gix-index stat check matches with `CreationTime=ChangeTime=LastWriteTime=T`; Defender and the indexer behaviour with the deny list; `PRJ_UPDATE_ALLOW_TOMBSTONE` removal; directory-rename pre-hydration | U56/U57 |
| S2 | ASC round-trip import (all 5,502 commits) | U35 |
| S3 | MinIO, R2 and Garage `If-None-Match` behaviour (probe outcomes) | U29/U30 |
| S4 | Turbo with the common dir at `repos/<id>/.git` (shared cache path) | U61 |
| S5 | FUSE passthrough in WSL 6.18 | U59 |
| S6 | Dev Drive (ReFS) and ProjFS together | dehydration policy |
| S7 | Claude Code `WorktreeCreate`/`WorktreeRemove` hooks | U54 docs |
| S8 | git partial clone lazily fetching **trees** from a promisor with `blob:none` | U36/U38 |

---

## Appendix A. Judge flaws and how they are fixed

| Flaw (judge) | Fix |
|---|---|
| Placeholder mtime = committer time; needs per-path history; can be older than build outputs (J1–J3) | Per-placeholder switch-epoch time, monotonically increasing, stored in `entry_times` and the index (§10.1) |
| `asc commit` ingests unknown staged SHA-1 from the working file (J1–J3) | Read and verify the blob from the projection ODB (§9.7 step 2) |
| Stage 2 pre-receive ran before new commits existed in git (J1) | Export into a bridge quarantine before pre-receive; promote after the CAS (§8.3 steps 6–8) |
| No transactional outbox (J1) | Ref CAS, ref log, op result and outbox in one SQL transaction; at-least-once worker (§8.3) |
| CI host cache read-write; no verify-on-read (J1) | Host cache read-only in jobs, per-job writable cache, trusted warmer; BLAKE3 verification on every read (§12.3, §3.2) |
| Write-behind leaks secrets; GC can lose drafts (J1/perf) | No upload before push; `store/` never evicted (§6.3, §3.5) |
| Land request state not atomic with the CAS (perf) | `op_results` in the same transaction; no queue/leader (§8.5) |
| Importer renamed case-colliding paths (perf) | History imported as-is; colliding/invalid names are not projected on Windows (§2.8) |
| Client-supplied `git_oid`/size trusted (perf) | Server recomputes `file_meta` and verifies every entry and change (§8.4) |
| Too many crates and moving parts (perf/integration) | 10 crates, one binary, no land queue, no segments, no WatchRefs (§0) |
| Two storage formats in server vs serverless (perf) | One bucket layout everywhere (§3.3) |
| LFS content vs pointer identity ambiguity (integration) | Pointer identity; `size` = pointer, `lfs.size` = resolved; index uses resolved stat with pointer oid (§2.5) |
| `asc adopt` in place leaves Full files (integration) | No in-place adopt; `clone --reference` into a new workspace; the user deletes the old checkout (§6.3, §14.3) |
| Submodule composition with nested writes on the critical path (integration) | Read-only composition, after the MVP; folding is a user decision (§10.6) |
| Serverless refs single-machine (J2/J3) | `BucketRefs` with ref-log CAS (create-only); probe; explicit single-writer fallback (§3.4) |
| Linux FUSE and virtual CI deferred (J2/J3) | FUSE in phase 7 with plain mode as the measured CI default; CI meets "no fetch until accessed" for history, LFS and submodules, and virtual CI is gated on measurement (§12.3) |
| Forgejo shows asc ids at Stage 2 (J2/J3) | Separate `object_format_name` switch; `id_mode=GIT` at the boundary (§13.2) |
| Soma folded by fiat (J2) | Not folded; it is a user decision (§19) |
| Root/tree format coupling unstated (J3) | Stated and accepted; the v1 tree format has no size limit (§0 decision 3) |
| `git-remote-asc` off the critical path (J3) | U38 in phase 3, before the server (PLAN) |
| Optimistic schedule; units too coarse (J3) | 5-month plan; the CLI is split into command-family units; the server is split into core, forgejo, mirror, land, reads, mutations and turbo units (PLAN) |
| Missing: tombstones, directory renames, signatures after rebase, husky hooksPath, Windows-invalid names, PutMode fallback, bucket GC/ABA/reflog, indexer/AV storms, node_modules placement, cross-fork reads, IDE `git commit`, backup/DR, distribution and skew, watchers, daemon crash, patch-stack cost, onboarding, concurrent index writers, autocrlf, Claude Code worktrees, MAX_PATH, rolling upgrades | §10.2 items 6/8, §8.5 step 4, §9.4, §2.8, §3.3, §3.4/§3.6, §10.2.5, §10.5, §3.3, §9.6, §15.3, §16.2, §10.2.5, §9.1, §13.3, §11.3/§18, §9.4, §9.4, §9.3, §10.2.10, §7/§16.2 |
