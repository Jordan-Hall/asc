# asc: Rust crate stack research

Status: research / recommendation. Date: 2026-10-22. Author: research agent for ASC (`asc` project).

Every version below was checked against the crates.io API on 2026-10-22, using `max_stable_version`. API claims marked **[src]** were checked against the published crate source for that exact version. Claims marked **[doc]** come from vendor documentation. Claims marked **[verify]** are unconfirmed and need a spike.

---

## 0. TL;DR

| Concern | Choice | Version | Notes |
|---|---|---|---|
| Object storage (S3/MinIO/R2/Ceph/local/memory) | `object_store` | 0.14.2 | `PutMode::Create` / `PutMode::Update` give CAS on S3-compatible stores. The local FS backend does **not** support `Update` **[src]** |
| Ref / metadata DB (server) | `sqlx` (postgres + sqlite) | 0.9.0 | Refs are compare-and-swap on one row (the Scion/Spanner pattern). Postgres for HA, SQLite for a single node |
| Ref / metadata DB (daemon, local-only) | `redb` | 4.4.0 | Pure Rust, ACID, MVCC, single file, low read latency for VFS callbacks |
| Content hash | `blake3` | 1.8.7 | Typed 32-byte IDs with domain separation |
| Git-compat hash (derived) | `sha1` | 0.11.0 | Stores the git blob SHA-1 per file so turbo and the git bridge can work without hydrating files |
| Compression | `zstd` | 0.14.3 (zstd-safe 8.2.0) | Trained dictionaries for small text blobs |
| Chunking (large files only) | `fastcdc` (v2020) | 5.0.0 | Only for files over ~4 MiB, which replaces LFS |
| Canonical object encoding | `postcard` + `serde` | 1.1.3 / 1.0.229 | Stable, specified wire format, so hashes stay reproducible |
| RPC (server, cache, daemon, and Forgejo in Go) | `tonic` + `tonic-prost` + `prost` | 0.14.6 / 0.14.6 / 0.14.4 | gRPC streaming. Go stubs for the Forgejo fork come from the same `.proto` |
| HTTP side-APIs (turbo remote cache, git smart-HTTP bridge, health) | `axum` | 0.8.9 | Shares the port and router with tonic |
| Daemon IPC | `interprocess` (tokio feature) | 2.4.6 | Unix socket on Unix, named pipe on Windows, carrying the same gRPC |
| Windows VFS | `windows` (`Win32_Storage_ProjectedFileSystem`) + `libloading` | 0.62.2 / 0.9.0 | Our own thin wrapper. `windows-projfs` 0.1.8 is a reference only |
| Linux VFS | `fuser` | 0.18.0 | Supports FUSE passthrough (`BackingId`) **[src]** |
| macOS VFS | `nfs3_server` (NFSv3 loopback, the EdenFS approach) | 0.11.1 | fuse-t / macFUSE through `fuser`'s `libfuse` feature as an alternative |
| Non-virtual mode | `notify` + `notify-debouncer-full`, `reflink-copy`, `ignore` | 8.2.0 / 0.7.0 / 0.1.30 / 0.4.33 | Clone-on-write materialisation where the filesystem supports it |
| Git import / bridge | `gix` | 0.89.0 | Import: gix ODB/pack reading. Export: generate a `git fast-import` stream |
| Line diff | `imara-diff` | 0.2.0 | Histogram/Myers, the fastest option |
| Inline / word diff display | `similar` | 3.2.0 | Display niceties only |
| 3-way text merge | port of `gix-merge` `blob::builtin_driver::text` onto `imara-diff` (MIT/Apache) | ref: gix-merge 0.22.0 | diff3/zdiff3 conflict styles that match git. `diffy` 0.5.2 is the fallback |
| CLI | `clap` (derive) + `anstream` + `indicatif` | 4.6.7 / 1.0.0 / 0.18.6 | Git-shaped subcommands and flags |
| Read the user's git config | `gix-config` | 0.62.1 | Reuses `user.name`/`user.email`/aliases from `~/.gitconfig` |
| In-memory caches | `quick_cache` | 0.7.0 | Tree and manifest caches in the daemon |

**Environment checks on this machine (Windows 11 Pro 10.0.26300, build 26100 servicing):**

- **ProjFS:** `Client-ProjFS` is **present but Disabled**. `Win32_OptionalFeature.InstallState = 2`, and `Get-WindowsOptionalFeature` needs elevation. The payload is staged in WinSxS (`microsoft-windows-projfs-api/driver 10.0.26100.9711`). `ProjectedFSLib.dll` and `drivers\prjflt.sys` are not yet in System32. To enable it, run this from an elevated shell: `Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart`. A reboot may be needed.
- **WSL2:** available. Distros are `Ubuntu` (default, Ubuntu 24.04), `docker-desktop` and `podman-machine-default`, all stopped at the time of the check. The WSL kernel is **6.18.54.1-microsoft-standard-WSL2**, with `CONFIG_FUSE_FS=y`, **`CONFIG_FUSE_PASSTHROUGH=y`** and `CONFIG_FUSE_IO_URING=y`. `/dev/fuse` is world-rw and `fusermount3` is installed. `CONFIG_NFS_V3=m`. This means FUSE and FUSE passthrough can be tested locally in WSL today.
- **Docker:** the CLI is installed, but the Docker Desktop engine was not running (`dockerDesktopLinuxEngine` pipe missing). Start Docker Desktop for MinIO/Garage testcontainers.
- **Disks:** `C:` is NTFS, 444 GB, 29 GB free. `D:` is NTFS, 1385 GB, 144 GB free. Neither is ReFS / Dev Drive, so NTFS has no block-clone. See section 5.5.
- **Unrelated:** `C:\Users\Jordan\.wslconfig` line 5 has an unknown key `wsl2.autoMemoryReclaim`, and WSL warns about it.

---

## 1. Storage layer

### 1.1 `object_store` 0.14.2 (recommended)

- This is the Apache Arrow project's crate, now released from `apache/arrow-rs-object-store`. It has about 25M recent downloads.
- Backends: `aws` (also covers MinIO, R2, Ceph RGW, Garage, Wasabi and any S3 API), `gcp`, `azure`, `http`, `fs` (the default feature, `LocalFileSystem`) and `InMemory`.
- It is one async trait (`ObjectStore`) with `put_opts`, `get_opts`/`get_range(s)`, `head`, `list(_with_offset)`, multipart upload, `copy_if_not_exists` and `delete_stream`.
- **Conditional put [src]:**
  ```rust
  pub enum PutMode { Overwrite, Create, Update(UpdateVersion) }
  pub struct UpdateVersion { pub e_tag: Option<String>, pub version: Option<String> }
  ```
  - On S3, `S3ConditionalPut::ETagMatch` is the **default**. It sends `If-None-Match: *` for `Create` and `If-Match: <etag>` for `Update`. It can be turned off with `aws_conditional_put=disabled`.
  - `InMemory` supports `Create` and `Update`, which is useful for tests.
  - **`LocalFileSystem` supports `Create`** (staged file plus hard-link publish) **but returns `NotImplemented` for `PutMode::Update`** (local.rs:399). Local refs therefore cannot use object_store CAS. They go to redb (section 1.3).
  - `S3CopyIfNotExists` supports the R2 header (`cf-copy-destination-if-none-match`) and a native-S3 multipart trick.
- Content-addressed blob writes do not need CAS at all. Use `PutMode::Create` and treat `AlreadyExists` as success, because writes are idempotent. This makes the blob path work on **every** S3-compatible store, including ones without conditional writes.

### 1.2 S3-compatible conditional write support (for ref CAS on bare buckets)

| Store | `If-None-Match: *` on PUT | `If-Match: <etag>` on PUT | Source |
|---|---|---|---|
| AWS S3 | Yes (PutObject, CompleteMultipartUpload, CopyObject), since Aug 2024 | Yes (same APIs), since Nov 2024 | [doc] AWS "conditional writes". Concurrent races can return `409 Conflict` (retry PutObject) as well as `412` |
| MinIO | Yes | Yes (since about 2023) | [doc] MinIO blog |
| Cloudflare R2 | Yes on PutObject | Yes on PutObject. CopyObject only has `x-amz-copy-source-if-*`. CompleteMultipartUpload lists no conditionals | [doc] R2 S3 API compatibility table |
| Ceph RGW | Yes | Yes. A versioned-bucket `If-Match` bug was fixed in ceph 20.1 (Tentacle) | [doc] Red Hat BZ 2380738 |
| Garage (v2.0) | **Not documented** | **Not documented** | [verify]. Assume unsupported |
| OVH, Scaleway, CoreWeave, Selectel | Yes | Yes (CoreWeave returns 409 on races) | [doc] |

**Conclusion:**

- Treat ref CAS on a bare bucket as a capability that asc probes for at `asc init`. The probe writes a test key with `Create` twice and expects `AlreadyExists`. It then does an `Update` with a stale ETag and expects `Precondition`.
- If the probe fails (Garage, old Ceph), a bare bucket can still hold all immutable data, but refs need either the asc server (Postgres/SQLite) or a local redb.
- Handle the retry hazard: a retried conditional create can return 412 even when the first attempt succeeded. Ref objects must therefore hold the new changeset ID, so that "412 and the current value equals my value" is treated as success.

### 1.3 Where refs live: three deployment modes, one `RefStore` trait

```rust
#[async_trait] // or native async fn in trait (Rust 1.75+)
pub trait RefStore: Send + Sync {
    async fn get(&self, name: &RefName) -> Result<Option<(ChangesetId, RefVersion)>>;
    async fn cas(&self, name: &RefName, expected: Option<RefVersion>, new: Option<ChangesetId>) -> Result<CasOutcome>;
    async fn list(&self, prefix: &RefPrefix) -> Result<Vec<(RefName, ChangesetId)>>;
}
```

| Mode | Immutable objects | Refs + changeset index + derived data | Use |
|---|---|---|---|
| **local-only** (no server, no network) | `object_store::local::LocalFileSystem` under `~/.asc/store` (or a shared machine cache) | `redb` file | Solo / offline / tests |
| **serverless shared bucket** | `object_store` S3 | `refs/<name>` objects using `PutMode::Update` CAS. The changeset log is the set of immutable changeset objects. Derived data is computed client-side and cached in the bucket under content-addressed keys | Small teams, CI caches, disaster recovery. Requires conditional-write support (see 1.2) |
| **client/server** (ASC default) | `object_store` S3/MinIO, or local disk for an on-prem single node | `sqlx` Postgres (HA) or SQLite (single node). Refs are a CAS on a single row: `UPDATE refs SET cs=$new, ver=ver+1 WHERE name=$n AND ver=$old` | Forgejo-backed. Matches Scion (Spanner row CAS + blob storage) |

Why **sqlx 0.9.0** for the server:

- One async API covers Postgres and SQLite, and migrations are built in.
- Forgejo already needs Postgres (or SQLite), so the same database server can host an `asc` schema.
- `tokio-postgres` 0.7.18 / `deadpool-postgres` 0.14.2 would also work, but they cover Postgres only.

**Pitfall:** do not put `rusqlite` and `sqlx-sqlite` in the same workspace. Both link `libsqlite3-sys` with different version ranges, and Cargo refuses two `links = "sqlite3"` crates. The choice is sqlx (with SQLite) on the server and redb in the daemon, so `rusqlite` is not used.

**Bold option to keep in view:** `slatedb` 0.17.0 is an LSM built *on object storage*. It uses a single writer with manifest fencing that relies on conditional puts. It could hold all server metadata in the same bucket, so a deployment would be one bucket plus a stateless-ish server. It is still 0.x and needs CAS support from the store. Revisit it after v1 and do not depend on it now.

### 1.4 `opendal` 0.59.5 (alternative, not chosen)

- Covers far more services (50+, including HDFS, WebDAV, GDrive, IPFS), and has layers for retry, timeout, concurrency limits and logging.
- It has been split into `opendal-core` plus service crates.
- Recent versions support conditional writes (`if_not_exists`, `if_match`) on S3-like services [verify the exact option names for 0.59].
- Rejected because:
  1. We only need S3 and the local filesystem.
  2. `object_store` has a smaller surface, has well-specified `PutMode` semantics, is used by DataFusion, delta-rs and SlateDB, and comes with an `InMemory` backend for tests.
  3. A second abstraction would duplicate ours.
- Keep `opendal` as the escape hatch if Azure Blob, GCS or HDFS ever matter. Our own `asc-store` trait hides the choice.

`aws-sdk-s3` 1.154.0 is not needed. `object_store` speaks SigV4 itself, which avoids the large SDK dependency tree.

### 1.5 Object layout in the bucket (proposal)

```
blobs/<hh>/<blake3-hex>        zstd frame (optionally dictionary-compressed), header byte = codec/dict id
chunks/<hh>/<blake3-hex>       fastcdc chunk (large files only)
trees/<hh>/<blake3-hex>        postcard-encoded tree (sorted entries)
changesets/<hh>/<blake3-hex>   postcard-encoded changeset (Bonsai: parents + changed paths vs first parent)
derived/<kind>/<key>           derived data (git-sha1 maps, skip lists, path history) - recomputable
dicts/<id>                     zstd dictionaries
refs/<name>                    (serverless mode only) small JSON/postcard {changeset, ver}
```

- There are no packfiles. Each object is idempotent and independently fetchable, so fetches resume per object.
- The `<hh>` prefix shards keys across S3 partitions.
- The cache tiers (daemon, regional cache) absorb per-object latency, as in Scion.

---

## 2. Hashing, compression, chunking, encoding

- **`blake3` 1.8.7:** SIMD, multithreaded with `rayon`, and around 5 to 10x faster than SHA-256.
  - Use `blake3::Hasher::new_derive_key("asc 2026 blob v1")` and similar for each object kind, so a blob can never collide with a tree.
  - Expose typed newtypes in `asc-types`: `BlobId`, `TreeId`, `ChangesetId`, `ChunkId`, each a `[u8; 32]`. This follows the strong-types preference.
  - `bao-tree` 0.17.0 (verified BLAKE3 range streaming) is optional later, for verifiable partial reads of huge files.
- **`sha1` 0.11.0:** at ingest, also compute the git blob SHA-1 (`"blob <len>\0" + bytes`) as **derived data** stored in the tree entry metadata. This gives three things:
  1. The git bridge can export without re-reading content.
  2. Turborepo-compatible file hashes can be produced **without hydrating files**. Turbo hashes inputs with git object hashes [verify in the turbo research task].
  3. `git hash-object` parity can be checked during migration.
- **`zstd` 0.14.3:** level 3 on the write path, plus optional background recompaction at level 19 for cold blobs.
  - Train dictionaries per extension class (`.ts/.tsx`, `.json`, `.rs`, `.py`, lockfiles) with `zstd::dict::from_continuous` over a sample of the existing repo. Store them as `dicts/<id>` and record the dict id in a 1-byte header.
  - Small source files typically compress 2 to 4x better with a dictionary.
  - tonic can also apply zstd to the RPC stream. Disable it for already-compressed blob payloads.
- **`fastcdc` 5.0.0 (`v2020` module, `tokio` feature for `AsyncStreamCDC`):**
  - Use it only for files over about 4 MiB, with min/avg/max of 512 KiB / 1 MiB / 4 MiB.
  - The file entry then points to a `ChunkList` object instead of a blob.
  - This covers the current 7.6 GB of git-LFS content natively: dedupe across versions, ranged hydration, and no LFS server.
- **`postcard` 1.1.3** is the canonical on-disk and hashed encoding.
  - It has a published, stable wire-format spec (1.0) and is compact (varints).
  - It is deterministic as long as the code uses structs and `Vec`s sorted by path (no `HashMap`).
  - Protobuf is used only at the RPC boundary, and blob/tree/changeset bytes travel as opaque `bytes` fields, so the bytes hashed are exactly the bytes stored and sent.
  - `rkyv` 0.8.18 (zero-copy) was considered and rejected: it adds complexity and its format is tied to alignment.

---

## 3. RPC and IPC

### 3.1 Options

| Option | Crate(s) | Streaming | Go client for Forgejo | Verdict |
|---|---|---|---|---|
| gRPC | `tonic` 0.14.6, `tonic-prost` 0.14.6, `prost` 0.14.4, `tonic-prost-build` 0.14.6, `tonic-health` 0.14.6 | Bidirectional, HTTP/2 flow control, built-in gzip/zstd | `protoc-gen-go-grpc` / `buf generate` from the same `.proto`. This is the Gitaly-style pattern Forgejo needs | **Chosen** |
| ConnectRPC | `connectrpc` 0.9.1 (Tower-based, connect-rust) | Yes, and it also works over HTTP/1.1 and is curl-able | `connect-go` | Good future swap. It uses the same `.proto` but is young (0.x) |
| HTTP + postcard/JSON | `axum` 0.8.9 | Manual (chunked bodies) | Hand-written | Used only for side APIs (turbo remote cache `/v8/artifacts`, git smart-HTTP bridge, metrics) |
| Cap'n Proto | `capnp` 0.27.2, `capnp-rpc` 0.27.1 | Promise pipelining | Weak Go ecosystem, small community | Rejected |
| tarpc | `tarpc` 0.38.0 | Yes | Rust-only | Rejected, because Forgejo is Go |

tonic 0.14 routes are built on axum, so `tonic::service::Routes::into_axum_router()` lets one port serve both gRPC and plain HTTP endpoints.

### 3.2 API shape (Scion-style: same RPCs on every tier)

- **Objects:** `FindMissing(ids) -> ids` (Bazel REAPI-style), `GetObjects(stream ids) -> stream (id, bytes)`, `PutObjects(stream) -> ack`.
  - Resumability comes from idempotent, content-addressed batches: after a drop, the client re-asks only for the IDs it does not have. No session state is needed.
  - Large blobs travel as chunks, so no single message is larger than a few MiB. Raise `max_decoding_message_size` to about 16 MiB.
- **Trees:** `GetTree(id, depth)` supports Scion's root-down negotiation. It returns child trees the client lacks (the client sends a compact "have" bloom or the generation of its cache snapshot).
- **History / derived (server-side, no checkout):** `Diff(a, b, paths?)` returns changed paths, sizes, ids and git-sha1. Also `MergeBase(a, b)`, `Log(path, range)`, `Blame(path, cs)`, `Affected(base, head, globs)`.
- **Mutations:** `CreateChangeset(parent, file changes)`, `MoveRef(name, expected, new)`, `Rebase`, `Merge`, `Land`. Commits, rebases and merges happen on the server, as in Scion.
- The daemon implements the same service traits. It answers from its cache, forwards misses upstream, and runs the VFS in-process. Regional caches use the same binary as the server in `--cache` mode.

### 3.3 Daemon IPC

- **`interprocess` 2.4.6** with features `tokio`: `local_socket` abstracts a Unix domain socket on Linux/macOS and a named pipe on Windows (`\\.\pipe\asc-<user>`).
  - Wrap its tokio stream in a newtype implementing `tonic::transport::server::Connected`, and feed it to `Server::serve_with_incoming`.
  - On the client, use `Endpoint::connect_with_connector(service_fn(...))`. This is the standard tonic UDS pattern.
- Alternative: `tokio::net::windows::named_pipe` plus `tokio::net::UnixListener` under cfg gates. `interprocess` saves about 150 lines of cfg code.
- Security: on Windows, set a pipe DACL restricting access to the current user's SID (`windows` crate `Win32_Security`). On Unix, use socket file mode 0600 in `$XDG_RUNTIME_DIR`.
- The hot path (VFS callbacks) does **not** cross IPC, because the VFS lives inside `ascd`. IPC is only for `asc` CLI commands, which stay low-latency.

---

## 4. Local metadata DB (daemon / local-only)

| | `redb` 4.4.0 | `fjall` 3.1.12 | `rusqlite` 0.40.2 | `heed` 0.23.0 (LMDB) |
|---|---|---|---|---|
| Model | Copy-on-write B-tree, MVCC, 1 writer / N readers | LSM, keyspaces, cross-keyspace transactions, compression | SQL | mmap B-tree |
| Pure Rust | Yes | Yes | No (C, bundled) | No (C) |
| Read latency | Excellent, predictable | Good (bloom filters) | Good | Excellent |
| Write throughput | Good (fsync per commit; batch) | Excellent | Good | Good |
| Format stability | Stable since 2.0 | Stable since 3.0 | Stable | Stable |
| Windows | Fine | Fine | Fine | Map-size quirks |

**Choice: `redb`.**

- VFS callbacks (ProjFS `GetPlaceholderInfo`, FUSE `lookup`/`getattr`) are latency-bound point reads, which a B-tree with MVCC serves best.
- It is a single file, pure Rust, with no compaction pauses.
- Write volume is moderate if blob **bodies live as files** in the cache directory (needed anyway for FUSE passthrough and block clone). redb then holds only indexes:
  - `blob id -> (size, cache-path, last-access)`
  - `path -> inode`
  - working-copy dirty set
  - per-worktree base changeset
  - refs in local-only mode
- `fjall` is a fine second choice if the profile shows write-bound workloads, such as a huge import. Keep storage access behind a `MetaStore` trait in `asc-cache` so the backend can change.

In-memory: `quick_cache` 0.7.0 (weighted LRU/CLOCK-Pro, very fast) for decoded trees. `moka` 0.12.16 is the alternative if async loaders are needed. `foyer` 0.22.6 (hybrid memory+disk) is overkill for now.

---

## 5. Virtual filesystem

Shared design (crate `asc-vfs`):

- A working copy is a `(base ChangesetId, overlay of local changes)`.
- Each tree entry carries `size`, `mode`, `blob id` and `git-sha1`, so directory listings and `stat` never need content.
- Content is fetched on first read from the per-machine shared cache (`ascd`), then from upstream.
- Each OS backend implements a small trait (`list_dir`, `entry`, `read_range`, `on_change`) over this shared core.

### 5.1 Windows: ProjFS

**Bindings:**

- Use `windows` 0.62.2 with feature `Win32_Storage_ProjectedFileSystem` **[src]**. It exposes `PrjStartVirtualizing`, `PrjStopVirtualizing`, `PrjMarkDirectoryAsPlaceholder`, `PrjFillDirEntryBuffer(2)`, `PrjWritePlaceholderInfo(2)`, `PrjWriteFileData`, `PrjAllocateAlignedBuffer`, `PrjFreeAlignedBuffer`, `PrjGetVirtualizationInstanceInfo`, `PrjUpdateFileIfNeeded`, `PrjDeleteFile`, `PrjGetOnDiskFileState`, `PrjCompleteCommand`, `PrjClearNegativePathCache`, `PrjFileNameMatch`, `PrjFileNameCompare` and `PrjDoesNameContainWildCards`.
- **Load `ProjectedFSLib.dll` dynamically** (`libloading` 0.9.0, or `LoadLibraryExW`+`GetProcAddress`). The `windows` crate uses raw-dylib imports, which the loader resolves at process start. A statically linked `ascd.exe` would therefore **fail to start** on machines where the optional feature is disabled, as on this one.
- `windows-projfs` 0.1.8 (WolverinDEV, about 500 recent downloads, synchronous `ProjectedFileSystemSource` trait, `dynamic-import` feature using libloading) is a good, readable reference implementation of all five callbacks. It is too small and synchronous to depend on, because we need async completion (`ERROR_IO_PENDING` + `PrjCompleteCommand`) and ContentID-versioned placeholders. Write `asc-vfs-projfs` at about 1.5 kLOC, borrowing its structure.
- Reference designs: Microsoft VFSForGit / Scalar (C#, archived) and the `ProjFS-Managed-API` sample "SimpleProviderManaged". Meta's EdenFS `PrjfsChannel.cpp` is the best production reference.

**Callbacks (`PRJ_CALLBACKS`):**

1. **`StartDirectoryEnumerationCallback(data, enumId)`:**
   - Create an enumeration session keyed by the GUID.
   - Resolve the directory in the tree **of the version recorded for this placeholder**: `data->VersionInfo->ContentID` holds our tree id, and `ProviderID` holds the working-copy epoch.
   - Snapshot the entries sorted with `PrjFileNameCompare`, which is case-insensitive and matches NTFS order.
2. **`GetDirectoryEnumerationCallback(data, enumId, searchExpression, dirEntryBufferHandle)`:**
   - Honour `PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN`. Store the search expression on the first call and filter with `PrjFileNameMatch`.
   - Fill entries with `PrjFillDirEntryBuffer2`, which supports symlink extended info, until it returns `HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER)`. Then return S_OK and resume from the cursor on the next call.
   - `PRJ_FILE_BASIC_INFO` needs `FileSize`, `IsDirectory`, timestamps and attributes. All of these come from tree metadata, so nothing is downloaded.
3. **`EndDirectoryEnumerationCallback`:** drop the session.
4. **`GetPlaceholderInfoCallback(data)`:**
   - Look up `data->FilePathName` (case-insensitive; repos with case-colliding paths must be rejected at ingest or mapped).
   - Call `PrjWritePlaceholderInfo2` with `PRJ_PLACEHOLDER_INFO`. `FileBasicInfo` is set from metadata. `VersionInfo.ContentID` (128 bytes) gets the 32-byte BlobId (or TreeId for a directory) plus a kind byte. `VersionInfo.ProviderID` gets the base changeset id.
   - Return `ERROR_FILE_NOT_FOUND` for unknown paths. ProjFS caches these in its negative-path cache.
5. **`GetFileDataCallback(data, byteOffset, length)`:**
   - Read the **ContentID from `data->VersionInfo`, not the current HEAD**. This keeps hydration consistent even if a switch happened after the placeholder was created.
   - Get bytes from the daemon cache, fetching upstream on a miss.
   - For slow fetches, return `HRESULT_FROM_WIN32(ERROR_IO_PENDING)` and later call `PrjCompleteCommand(commandId, hr)`.
   - Write with `PrjAllocateAlignedBuffer` + `PrjWriteFileData` in chunks aligned to the volume sector size (from `PrjGetVirtualizationInstanceInfo`).
   - Large chunked files can be streamed by range.
6. **`QueryFileNameCallback` (optional):** a fast existence check.
7. **`CancelCommandCallback`:** cancels a pending async fetch. Requires async support.
8. **`NotificationCallback(data, isDirectory, notification, destinationFileName, params)`:**
   - Register `PRJ_NOTIFICATION_MAPPING` at the root, with the `.asc\` directory excluded. Subscribe to `NEW_FILE_CREATED`, `FILE_OVERWRITTEN`, `PRE_DELETE`, `PRE_RENAME`, `PRE_SET_HARDLINK`, `FILE_RENAMED`, `HARDLINK_CREATED`, `FILE_HANDLE_CLOSED_FILE_MODIFIED`, `FILE_HANDLE_CLOSED_FILE_DELETED` and `FILE_PRE_CONVERT_TO_FULL`.
   - Append each affected path (and rename destination) to the **modified-paths journal** in redb.
   - `PRE_*` notifications can veto operations, for example to make a CI checkout read-only, or to block writes into `.asc`.

**Tracking modified files (status without a scan):**

- On-disk ProjFS states are Virtual (not on disk), Placeholder, HydratedPlaceholder, DirtyPlaceholder, Full and Tombstone.
- Only paths in the **modified-paths journal** can differ from the base, which is the VFSForGit "ModifiedPaths" approach. `asc status` therefore:
  1. Reads the journal.
  2. Confirms each path's state with `PrjGetOnDiskFileState`.
  3. Hashes only Full or DirtyPlaceholder files, and treats tombstones as deletions.
- Hydrated-but-unmodified placeholders (`HydratedPlaceholder`) are clean by definition. Status cost is O(changed files), not O(repo).
- **`asc switch` / `checkout`:**
  1. Compute `Diff(old, new)` from metadata (local or server RPC).
  2. For each changed path that is not in the journal, call `PrjUpdateFileIfNeeded` with the new placeholder info and `PRJ_UPDATE_ALLOW_DIRTY_METADATA`. Use `PrjDeleteFile` for removed paths, and ignore not-yet-materialised ones.
  3. Paths that are in the journal are conflicts and follow git's switch rules (refuse, or carry the local change).
  4. Call `PrjClearNegativePathCache` and bump `ProviderID`.
  - Cost is O(changed paths) with no downloads.
- **Disk reclaim:** `asc sparse --dehydrate <paths>` (and an idle GC) can turn clean hydrated files back into placeholders with `PrjDeleteFile` + re-placement, which frees disk space.

**Limits:**

- ProjFS hydration writes a **full copy** into the working copy on NTFS. Two worktrees that read the same file both pay for it, although each pays only for files actually read.
- On a **Dev Drive (ReFS)**, hydration could block-clone from the shared cache (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`), so hydrated files share extents across worktrees. ProjFS must be allowed on Dev Drive with `fsutil devdrv setfiltersallowed PrjFlt` [verify that ProjFS works on ReFS Dev Drive]. This machine has no Dev Drive, since C: and D: are NTFS.
- One virtualization root per worktree. The provider (`ascd`) must be running, or reads of unhydrated files fail. Run `ascd` as a per-user auto-start task, or as a service with `windows-service` 0.8.1.

### 5.2 Linux: FUSE

- **`fuser` 0.18.0** is pure Rust and needs no libfuse by default. Features: `libfuse2/3`, `experimental` (async-trait + tokio) and `macfuse-4-compat`.
  - **Passthrough is supported [src]:** `BackingId`, `ReplyOpen::opened_passthrough`, and `KernelConfig::set_max_stack_depth` must be at least 1.
  - With kernel 6.9+ passthrough, once a file is hydrated into the shared cache directory, **reads and mmap of that file go straight from the kernel to the backing file**. That means zero user-space copies and **zero extra disk per worktree**, which is the best possible outcome for both CI and worktrees.
  - The WSL kernel here (6.18) has `CONFIG_FUSE_PASSTHROUGH=y`.
- Writes go to a per-worktree **overlay directory** (copy-up on open-for-write, the EdenFS overlay approach). Every write passes through our handlers, so the dirty set is exact and needs no notifications.
- Inode table: `path <-> inode`, persisted in redb so mounts survive daemon restart.
- Alternatives:
  - `fuse3` 0.9.0 is async-native (tokio) but has no passthrough.
  - `fuse-backend-rs` 0.14.0 adds virtio-fs, which helps if CI ever runs in Firecracker or cloud-hypervisor VMs.
  - FUSE-over-io_uring (`CONFIG_FUSE_IO_URING`, kernel 6.14+) is not yet in `fuser` [verify]. It is a later optimisation.
- **CI containers:** need `--device /dev/fuse --cap-add SYS_ADMIN`, or rootless `fusermount3`.
  - If the Forgejo Actions runner cannot grant these, use **non-virtual sparse mode** (5.4), which materialises only the affected packages and their dependencies, as reflinks/hardlinks from the runner's shared cache.

### 5.3 macOS

| Option | Kext? | Rust path | Notes |
|---|---|---|---|
| **NFSv3 loopback** (what EdenFS uses on macOS) | No | `nfs3_server` 0.11.1 (active, updated 2026-10-21), `nfsserve` 0.11.0, `zerofs_nfsserve` 0.19.1 | Mount with `mount_nfs -o locallocks,vers=3,tcp,port=N localhost:/ <dir>`. No user approval prompts. Slightly higher latency. **Recommended** |
| FSKit (macOS 15.4+) | No (app extension) | Swift extension plus Rust core over FFI | Needs a signed app bundle and user enablement. Best long-term native option. Later |
| macFUSE 5 (FSKit backend) / fuse-t (NFS-based, libfuse-compatible) | No (with FSKit / fuse-t) | `fuser` with the `libfuse` feature linking fuse-t's libfuse | Licensing: fuse-t is free but closed source, macFUSE has a commercial licence. Fallback |
| Non-virtual mode | No | `reflink-copy` (APFS `clonefile`) | Always works. With APFS clones, worktrees are nearly free anyway |

Since ASC developers are on Windows and CI is on Linux, macOS can ship last, as non-virtual + APFS clonefile, with NFS virtual mode after that.

### 5.4 Non-virtual mode (fallback everywhere, and CI without FUSE)

- `asc clone --no-virtual` / `asc sparse set <globs>` materialises files into the working copy, using `reflink-copy` 0.1.30 from the shared cache where the filesystem supports it. Supported: ReFS / Dev Drive, Btrfs, XFS, APFS, bcachefs. Otherwise hardlink read-only plus copy-on-write-on-first-write, or plain copy.
- Change detection uses a git-index-style stat cache (`mtime`, `size`, `inode`/file id) stored in redb. `notify` 8.2.0 + `notify-debouncer-full` 0.7.0 (ReadDirectoryChangesW / inotify / FSEvents) keeps the dirty set warm while `ascd` runs, so status rarely re-stats the tree.
- `ignore` 0.4.33 + `globset` 0.4.20 (ripgrep's crates) for `.gitignore`/`.ascignore` semantics and sparse globs. Read existing `.gitignore` files unchanged, for adoption.

### 5.5 Cheap worktrees (main disk pain)

- `asc worktree add <path> <rev>` creates a new virtualization root (ProjFS/FUSE/NFS) that points at a changeset. Cost: a few KB of metadata plus a redb row.
- All worktrees on a machine share one blob cache (`%LOCALAPPDATA%\asc\cache` or a configured `D:\asc-cache`), so content is stored once per machine.
- Linux FUSE passthrough means hydrated reads cost no extra disk. Windows NTFS ProjFS costs a copy of only the files actually read. Dev Drive block-clone would remove even that [verify].
- Compare today: 50 git worktrees, each with a full checkout, plus a 58 GB `.git` on a 90%-full D:.

---

## 6. Git import and the git bridge

- **`gix` 0.89.0** (gitoxide; the gix-* crates move in lock-step, so pin exactly):
  - **Import:** open the existing repo, walk `rev_walk` from `main` (first-parent plus merges), and decode trees/blobs from packs in parallel (`gix-pack` 0.77.0, `gix-odb` 0.87.0).
  - For each commit, emit a Bonsai changeset: the changed paths versus the first parent, from `gix-diff` 0.70.0 tree diff.
  - Resolve LFS pointer files through the Forgejo LFS API into native asc chunked blobs.
  - Map the submodule `packages/soma/sdk` (gitlink) to a `Submodule{url, commit}` tree entry in v1. Possibly vendor it as a subtree later.
  - Record the `git commit sha1 <-> ChangesetId` mapping as derived data, so links and CI history keep working.
- **Export / bridge:**
  - Generating a **`git fast-import` stream** and piping it to `git fast-import` is the simplest robust way to produce a mirror. It uses no gix write-path dependency and is incremental with marks.
  - For a live read-only git endpoint (tools that require git, Renovate, external mirrors), serve git smart-HTTP from derived git objects later. gix has no push/server side, so this would be our own `upload-pack` over axum. Defer it.
- `gix-config` 0.62.1: read `~/.gitconfig`/`.git/config` for `user.name`, `user.email`, `core.editor`, `alias.*` and `diff.*` settings, so `asc` feels configured on day one. `asc` writes its own `~/.ascconfig` in the same INI format.
- **Reference, not dependency:** `jj-lib` 0.46.0 (Jujutsu) has a clean `Backend` trait, a working-copy abstraction, an operation log, and git interop through gix. It is a good design reference for changeset/working-copy separation and git-like UX. Its data model and ownership do not fit Scion-style server asymmetry, so do not build on it.

---

## 7. Diff and merge

- **Text diff: `imara-diff` 0.2.0.**
  - Histogram (the default, like git's `--histogram`) and Myers, with interning and a unified-diff printer (`unified_diff` feature).
  - In benchmarks it is 10 to 100x faster than `similar`.
  - Note that gitoxide uses its own fork `gix-imara-diff` 0.3.2. Track the upstream that git output parity needs, and run golden tests against `git diff --histogram`.
- **Display niceties: `similar` 3.2.0** for word/char inline highlighting (`asc diff --word-diff`). Its `TextDiff::iter_inline_changes` is convenient.
- **3-way text merge:**
  - **Recommended:** port `gix-merge` 0.22.0's `blob::builtin_driver::text` (MIT/Apache-2.0) into `asc-merge`. Its signature is `merge(out, &mut InternedInput, Labels{ancestor,current,other}, current, ancestor, other, Options{diff_algorithm, conflict}) -> Resolution`.
    - It implements git's `merge`/`diff3`/`zdiff3` conflict styles and the ours/theirs/union favour modes on imara-diff. That gives git-identical conflict markers, which matters for adoption.
    - Taking the `gix-merge` crate directly would pull in about 20 gix crates and git object types.
  - **Fallback / v0:** `diffy` 0.5.2 `merge`/`merge_bytes` (Myers-based). Simpler, but slower, and the conflict output differs from git.
- **Tree merge (port merge-ort ideas, but simpler):**
  - Changesets already list changed paths versus the first parent. A 3-way merge of `base..ours` and `base..theirs` is therefore a **union of two path-change sets**, and only paths touched on both sides need content merging.
  - Rename/copy handling uses the derived copy-detection index computed at commit time, instead of merge-ort's O(n) rename scan.
  - This is what makes server-side merge/rebase cheap enough for Scion's 30k merges/day.
- **Blame:** use derived data computed recursively from the parent's blame plus the diff of this changeset (Mononoke-style). Store it under `derived/blame/<blob>/<cs>` and compute it lazily on first request.

---

## 8. CLI

- **`clap` 4.6.7** (`derive`, `env`, `wrap_help`):
  - One enum per git porcelain command with git flag spellings. For example `asc switch -c <branch>`, `asc restore --staged --source=<rev>`, `asc log --oneline --graph -n 20`, `asc diff --stat --name-only --cached`, `asc reset --hard|--soft|--mixed`, `asc stash push -m`, `asc worktree add|list|remove`.
  - Global flags `-C <path>` and `-c key=value`, as in git.
  - `#[command(external_subcommand)]` dispatches `asc-foo` plugins on PATH, as git does.
  - Expand `alias.*` from config before parsing.
- Output uses `anstream` 1.0.0 / `anstyle` (clap's own color stack, respects `NO_COLOR` / `--color`) and `indicatif` 0.18.6 for progress. Paging spawns `$GIT_PAGER`/`$PAGER`/`less -FRX`, like git.
- Errors use `thiserror` 2.0.21 in libraries and `anyhow` 1.0.104 in binaries. `miette` 7.6.0 is optional for rich diagnostics.
- Commands such as `asc add` and the index:
  - Keep git's mental model (a staging area) so muscle memory works.
  - Internally the "index" is a pending changeset delta in redb.
  - `asc commit` sends `CreateChangeset` to the server, or the local engine in local-only mode.
- New commands that have no git equivalent are named as extensions: `asc affected`, `asc land`, `asc sparse`, `asc prefetch` and `asc daemon`.

---

## 9. Supporting crates (pinned)

| Purpose | Crate | Version |
|---|---|---|
| Async runtime | `tokio` | 1.54.1 |
| Tokio utils (codec, `CancellationToken`) | `tokio-util` | 0.7.20 |
| Streams | `tokio-stream` / `futures` | 0.1.19 / 0.3.34 |
| HTTP | `hyper` / `hyper-util` / `tower` / `tower-http` | 1.12.0 / 0.1.21 / 0.5.3 / 0.7.1 |
| TLS | `rustls` / `tokio-rustls` | 0.23.46 / 0.26.6 |
| Bytes | `bytes` | 1.12.1 |
| Logging / tracing | `tracing` / `tracing-subscriber` / `opentelemetry` | 0.1.44 / 0.3.23 / 0.33.1 |
| Paths | `camino` (UTF-8 paths), `bstr` | 1.2.6 / 1.13.1 |
| Small strings / vecs | `compact_str` / `smallvec` | 0.10.0 / 1.16.3 |
| Time | `jiff` | 0.2.38 |
| Parallelism | `rayon` | 1.12.0 |
| mmap | `memmap2` | 0.9.12 |
| Locks | `parking_lot` / `arc-swap` | 0.13.0 / 1.9.3 |
| Retry | `backon` | 1.6.0 |
| Config | `toml` | 1.1.8 |
| Dirs | `etcetera` | 0.11.0 |
| Encoding | `data-encoding` (hex/base32 ids) | 2.11.1 |
| IDs for ops | `uuid` | 1.28.0 |
| Windows service | `windows-service` | 0.8.1 |
| Unix syscalls | `rustix` / `nix` | 1.1.5 / 0.31.3 |
| Tests | `insta` / `proptest` / `tempfile` / `testcontainers` + `testcontainers-modules` (MinIO, Postgres) | 1.49.0 / 1.12.0 / 3.27.0 / 0.28.0 / 0.15.0 |

---

## 10. Recommended Cargo workspace

Use edition 2024 and toolchain 1.95 (pinned in `rust-toolchain.toml`). Single workspace at `D:\dev\source-control`. Commit `Cargo.lock`.

```
source-control/
  Cargo.toml                 # [workspace] + [workspace.dependencies] (below)
  rust-toolchain.toml        # channel = "1.95.0"
  proto/asc/v1/*.proto        # single source of truth for RPC; Go stubs generated for the Forgejo fork via buf
  crates/
    asc-types        # IDs (BlobId/TreeId/ChangesetId/ChunkId newtypes), RepoPath (normalised, case rules),
                    #   Changeset/Tree/Entry structs, postcard canonical encode/decode, blake3 domain hashing. No IO.
    asc-store        # ContentStore over object_store: put-if-absent, get/range, zstd+dict codec, fastcdc chunking,
                    #   verify-on-read, bucket layout. Backends: local fs, S3-compatible, memory.
    asc-refs         # RefStore trait + impls: redb (local), s3-cas (PutMode::Update), sql (sqlx pg/sqlite). Capability probe.
    asc-derive       # Derived-data framework (Mononoke-style): generation numbers, skip lists, path history, blame,
                    #   git-sha1 map, copy detection; eager or deferred, keyed by (kind, changeset).
    asc-diff         # Tree/changeset diff from metadata only; imara-diff line diff + unified printer; similar for inline.
    asc-merge        # 3-way text merge (ported gix-merge text driver), path-set tree merge, rebase/cherry-pick planner.
    asc-engine       # The repository API (create changeset, move ref, diff, merge-base, log, blame, merge, rebase,
                    #   affected) implemented over store+refs+derive. Embedded by the server AND by local-only mode.
    asc-proto        # tonic-prost generated code + conversions to asc-types (prost types never leak past this crate).
    asc-rpc          # Client + server glue: tonic services wrapping asc-engine; RemoteEngine client implementing the
                    #   same Rust trait; FindMissing/GetObjects streaming with resumable batches.
    asc-ipc          # interprocess local_socket <-> tonic Connected/connector adapters; pipe DACL / socket perms.
    asc-cache        # Per-machine shared cache: blob files on disk, redb index, quick_cache tree cache, LRU eviction,
                    #   prefetch, upstream fall-through (implements the same API as a tier).
    asc-wc           # Working-copy state: base changeset, staging delta, modified-paths journal, status, switch planner,
                    #   non-virtual materialiser (reflink-copy) + notify watcher + ignore rules.
    asc-vfs          # OS-neutral VFS core: inode/path table, placeholder versioning, overlay, read-through to asc-cache.
    asc-vfs-projfs   # [cfg(windows)] ProjFS provider (windows crate + libloading dynamic load).
    asc-vfs-fuse     # [cfg(linux)] fuser provider with passthrough + overlay.
    asc-vfs-nfs      # [cfg(macos)] (and optional Linux-unprivileged) NFSv3 loopback via nfs3_server.
    asc-git          # gix-based importer (history, LFS resolution, submodules) + fast-import exporter + sha1 mapping.
    asc-ci           # Turborepo/CI helpers: affected packages from server Diff, turbo-compatible file hash manifest
                    #   from metadata (git-sha1), sparse CI checkout plans. Consumed by `asc affected` and Forgejo Actions.
    asc-config       # Layered config (system/user/repo/-c), reads git config via gix-config, aliases.
    asc-testkit      # In-memory store/refs, MinIO/Postgres testcontainers, golden git-diff/merge fixtures.
  bins/
    asc              # CLI (clap). Talks to ascd over asc-ipc; in local-only mode can embed asc-engine directly.
    ascd             # Per-machine daemon: asc-cache + VFS mounts + IPC server; Windows service / systemd user unit / launchd.
    asc-server       # tonic+axum server: asc-engine over Postgres/SQLite + object_store; `--cache` mode = regional tier;
                    #   also serves turbo remote cache (/v8/artifacts) on the same object store.
```

Dependency direction runs strictly downward:

```
types <- store, refs <- derive <- diff, merge <- engine <- rpc <- (cache, server)
cache <- vfs <- vfs-* <- ascd
wc    <- asc
```

`asc-proto` is used only by `asc-rpc`. The engine, cache and remote client all implement **one Rust trait (`RepoApi`)**. This is the Scion "same RPCs at every tier" idea in code, and it is the main complexity reducer: local-only mode is just `RepoApi` backed by the embedded engine.

### `[workspace.dependencies]` (pinned to versions verified 2026-10-22)

```toml
[workspace.package]
edition = "2024"
rust-version = "1.95"
license = "MIT OR Apache-2.0"

[workspace.dependencies]
# storage
object_store   = { version = "0.14.2", default-features = false, features = ["fs", "aws"] }
redb           = "4.4.0"
sqlx           = { version = "0.9.0", default-features = false, features = ["runtime-tokio", "tls-rustls", "postgres", "sqlite", "migrate", "macros"] }
# hashing / compression / chunking / encoding
blake3         = { version = "1.8.7", features = ["rayon"] }
sha1           = "0.11.0"
zstd           = "0.14.3"
fastcdc        = { version = "5.0.0", features = ["tokio"] }
postcard       = { version = "1.1.3", features = ["use-std"] }
serde          = { version = "1.0.229", features = ["derive"] }
serde_json     = "1.0.154"
# rpc / ipc / http
tonic          = { version = "0.14.6", features = ["zstd"] }
tonic-prost    = "0.14.6"
prost          = "0.14.4"
tonic-health   = "0.14.6"
tonic-prost-build = "0.14.6"   # build-dependency of asc-proto
axum           = "0.8.9"
tower          = "0.5.3"
tower-http     = "0.7.1"
hyper          = "1.12.0"
hyper-util     = "0.1.21"
interprocess   = { version = "2.4.6", features = ["tokio"] }
rustls         = "0.23.46"
# async
tokio          = { version = "1.54.1", features = ["full"] }
tokio-util     = "0.7.20"
tokio-stream   = "0.1.19"
futures        = "0.3.34"
bytes          = "1.12.1"
backon         = "1.6.0"
# vfs (target-specific in member crates)
windows        = { version = "=0.62.2", features = ["Win32_Storage_ProjectedFileSystem", "Win32_Foundation", "Win32_Security", "Win32_System_IO"] }
libloading     = "0.9.0"
windows-service = "0.8.1"
fuser          = { version = "0.18.0", default-features = false }
nfs3_server    = "0.11.1"
rustix         = { version = "1.1.5", features = ["fs"] }
# working copy
notify         = "8.2.0"
notify-debouncer-full = "0.7.0"
ignore         = "0.4.33"
globset        = "0.4.20"
reflink-copy   = "0.1.30"
memmap2        = "0.9.12"
# git interop (gix family moves in lock-step: pin exactly)
gix            = { version = "=0.89.0", default-features = false, features = ["max-performance-safe", "revision", "blob-diff"] }
gix-config     = "=0.62.1"
# diff / merge
imara-diff     = "0.2.0"
similar        = { version = "3.2.0", features = ["inline"] }
diffy          = "0.5.2"       # v0 merge fallback only
# cli / ux
clap           = { version = "4.6.7", features = ["derive", "env", "wrap_help"] }
anstream       = "1.0.0"
indicatif      = "0.18.6"
# caches / util
quick_cache    = "0.7.0"
camino         = "1.2.6"
bstr           = "1.13.1"
compact_str    = "0.10.0"
smallvec       = "1.16.3"
jiff           = "0.2.38"
rayon          = "1.12.0"
parking_lot    = "0.13.0"
arc-swap       = "1.9.3"
data-encoding  = "2.11.1"
uuid           = { version = "1.28.0", features = ["v7"] }
toml           = "1.1.8"
etcetera       = "0.11.0"
thiserror      = "2.0.21"
anyhow         = "1.0.104"
tracing        = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter", "json"] }
# test
insta          = "1.49.0"
proptest       = "1.12.0"
tempfile       = "3.27.0"
testcontainers = "0.28.0"
testcontainers-modules = { version = "0.15.0", features = ["minio", "postgres"] }
```

Feature names for `gix`, `similar`, `tonic` (`zstd`) and `sqlx` must be confirmed with `cargo add` in the first spike. They are taken from each crate's documented feature set and may have been renamed between versions [verify]. `object_store` features (`fs`, `aws`) and `fastcdc`/`interprocess` `tokio` features were confirmed from source.

---

## 11. Risks and spikes (in order)

1. **ProjFS spike (Windows, needs admin once):**
   - Enable `Client-ProjFS` and build a minimal `asc-vfs-projfs` that projects a tree from an in-memory store.
   - Measure `git status`-equivalent and turbo `--dry` over a 100k-file projection.
   - Verify async `ERROR_IO_PENDING` completion and `PrjUpdateFileIfNeeded` switch behaviour.
2. **FUSE passthrough spike (WSL 6.18):** `fuser` + `BackingId` read-through from the cache directory. Measure `bun install` / `turbo run build` on a virtual checkout.
3. **Conditional-write probe** against MinIO and R2, and Garage if it is used. Run `PutMode::Create` / `Update` races under load. Confirm handling of 409 versus 412.
4. **Import throughput:** use gix to read the 58 GB repo and write a Bonsai changeset stream to local `object_store`. Confirm the LFS and submodule mapping. Target: the full history in hours, not days.
5. **Turbo hashing without hydration:** confirm turbo's file-hash algorithm (git blob SHA-1 vs. other) and how to feed it (`--affected` with `TURBO_SCM_BASE`, or a pre-computed hash manifest). This belongs to the turbo/CI research task, but decides whether `git-sha1` derived data is mandatory. Recommendation: make it mandatory either way, because it is cheap at ingest.
6. **Case-insensitivity:** Windows ProjFS is case-insensitive. Reject or normalise case-colliding paths at ingest, and add a lint in `asc commit`.
