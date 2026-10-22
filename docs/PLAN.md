# asc implementation plan

Normative design: `docs/SPEC.md`. This file orders the work into **implementation units**. Each unit is small (one crate module or one Go package), owns a disjoint set of files, names its dependencies, and has **one acceptance command**. A unit is done when its acceptance command passes on a clean checkout, `cargo clippy --workspace --all-targets -- -D warnings` stays green, and no file outside the unit's owned list was changed (except `Cargo.lock`, the one-line `mod`/`pub use` registration in the crate's `lib.rs`/`main.rs`, and adding its own crate to `members` in the root `Cargo.toml`, which every unit may do).

**Rules for implementers (people or agents)**

- Agents run at effort ≤ medium. Read the SPEC sections cited in the brief before coding; do not re-decide anything the SPEC fixes.
- Strong types: every id is a newtype; no `String` for paths or refs past the CLI boundary; errors via `thiserror` per crate, `anyhow` only in `crates/asc/src/main.rs`.
- No `unsafe` outside `asc-vfs::projfs::ffi` and `asc-vfs::fuse::passthrough`.
- Tests live next to the code (`#[cfg(test)]`) or in the unit's owned `tests/` files. Integration tests needing Docker, ProjFS, FUSE, turbo or ASC's repo are `#[ignore]` and gated behind the features named in the acceptance command.
- Pinned dependency versions are in SPEC §3/§4 and `docs/research/rust-stack.md` §10. Add each dependency to `[workspace.dependencies]` in U01 only; units reference `workspace = true`.

**Workspace layout (fixed by U01)**

```
Cargo.toml  rust-toolchain.toml  deny.toml  .cargo/config.toml  docker-compose.it.yml
proto/asc/v1/{repo,workspace}.proto   proto/buf.yaml   proto/buf.gen.yaml
crates/asc-core     pure data model, codec, git rendering, content ingest
crates/asc-store    Objects/Refs traits, bucket, cache dir, tiered, SQL refs, bucket refs, GC
crates/asc-engine   trees, commits, revspec, graph, derive, diff, merge, rebase, land, blame, sync
crates/asc-git      index, odb, projection, import, export, remote helper, git-remote mode
crates/asc-rpc      proto codegen, conversions, gRPC server/client, auth interceptor, IPC transport
crates/asc-wc       $ASC_HOME, state.db, workspaces, journal, status, switch planner, plain materialiser
crates/asc-vfs      provider core, ProjFS (cfg windows), FUSE (cfg linux), submodule composition
crates/asc-serve    asc serve: registry, ingest, UpdateRefs, outbox, Forgejo client, mirror, land, reads, mutations, /v8
crates/asc-ci       asc ci checkout/affected/changed/warm
crates/asc          the binary: CLI, daemon, argv[0] dispatch (asc, git-remote-asc, asc-fsmonitor)
tests/turbo/       turbo contract suite        actions/       composite actions        deploy/   compose bundle
```

Dependency direction (strict): `asc-core ← asc-store ← asc-engine ← asc-git ← asc-wc ← asc-vfs ← asc`; `asc-rpc` depends on `asc-engine`; `asc-serve` on `asc-rpc`, `asc-git`; `asc-ci` on `asc-wc`, `asc-git`, `asc-rpc`; `asc` on everything (features `serve`, `vfs-projfs`, `vfs-fuse`).

**Parallelism notation:** units in the same `‖ group` have disjoint files and no dependency on each other, so they can run concurrently once their dependencies are done.

---

## Unit index

| ID | Title | Crate | Depends on | ‖ group |
|---|---|---|---|---|
| U01 | Workspace skeleton | all | – | – |
| U02 | Typed ids | asc-core | U01 | A |
| U03 | Names and paths | asc-core | U01 | A |
| U04 | Objects and canonical codec | asc-core | U02, U03 | – |
| U05 | Git object rendering | asc-core | U04 | B |
| U06 | Content ingest and chunking | asc-core | U02 | B |
| U07 | Objects trait, keys, bucket (memory/fs) | asc-store | U04, U06 | C |
| U09 | Refs trait and SqlRefs | asc-store | U04 | C |
| U08 | CacheDir, StoreSet, Tiered | asc-store | U07 | – |
| U10 | Tree apply/diff | asc-engine | U05, U08 | – |
| U11 | Changeset build and validation | asc-engine | U10 | – |
| U12 | Commit graph and log | asc-engine | U11, U09 | D |
| U14 | Derived data (gen, gitcommit) | asc-engine | U11, U05 | D |
| U15 | Line diff and output formats | asc-engine | U10 | D |
| U13 | Revspec resolution | asc-engine | U12, U14 | – |
| U16 | Git index and ODB access | asc-git | U05, U06 | D |
| U17 | $ASC_HOME and state.db | asc-wc | U09 | D |
| U19 | Projection layout and config | asc-git | U16, U17 | – |
| U18 | Plain working copy | asc-wc | U16, U17, U10 | – |
| U20 | CLI frame, init, config | asc | U13, U18, U19 | – |
| U21 | CLI status/add/rm/mv/restore/reset | asc | U20, U15 | E |
| U22 | CLI commit | asc | U20, U14 | E |
| U23 | CLI branch/switch/checkout/tag | asc | U20 | E |
| U24 | CLI history and plumbing verbs | asc | U20, U15 | E |
| U25 | Scenario harness, local-only milestone | asc | U21–U24 | – |
| U26 | Merge, rebase, cherry-pick, land check | asc-engine | U15, U13 | F |
| U28 | Blame engine and CLI | asc-engine, asc | U25, U15 | F |
| U27 | CLI merge/rebase/cherry-pick/revert/stash | asc | U25, U26 | – |
| U29 | S3 bucket objects and put fallback | asc-store | U07 | G |
| U30 | BucketRefs and probe | asc-store | U07, U09 | G |
| U31 | Sync engine (push/fetch) | asc-engine | U08, U09, U14 | G |
| U32 | CLI remote/fetch/push/pull/clone (bucket remotes) | asc | U27, U29–U31 | – |
| U33 | Git import | asc-git | U14, U16 | H |
| U34 | Git export | asc-git | U14, U16 | H |
| U35 | ASC round-trip milestone | asc-git | U33, U34 | – |
| U36 | Projection objects and refs mirror | asc-git | U19, U34 | – |
| U37 | Hooks dispatcher and reconcile | asc-git, asc | U36, U33, U22 | I |
| U38 | git-remote-asc helper | asc-git, asc | U36, U31 | I |
| U39 | Git-remote mode | asc-git, asc | U32, U33, U34, U36 | – |
| U40 | fsmonitor, doctor, reclaimable report | asc | U39 | – |
| U41 | Proto and conversions | asc-rpc | U04 | J (may start after U04) |
| U42 | gRPC server/client, auth interceptor | asc-rpc | U41, U31, U26, U28 | – |
| U43 | Server core: registry, ingest, UpdateRefs, outbox | asc-serve | U42, U34 | – |
| U44 | Forgejo client (authz, hooks) | asc-serve | U43 | K |
| U47 | Decoded read RPCs | asc-serve | U43 | K |
| U49 | Turbo /v8 cache | asc-serve | U43 | K |
| U45 | Mirror mode | asc-serve | U44, U33 | L |
| U46 | Land | asc-serve | U44 | L |
| U48 | Mutations and admin | asc-serve | U44, U47 | L |
| U50 | CLI server wiring | asc | U42, U39, U46 | – |
| U51 | Deploy bundle and smoke | deploy | U45–U50 | – |
| U52 | IPC transport and Workspace service | asc-rpc | U42 | M |
| U54 | Journal, virtual status, switch intent, worktrees | asc-wc | U25 | M |
| U55 | VFS provider core | asc-vfs | U54 | – |
| U53 | Daemon | asc | U52, U54, U55 | – |
| U56 | ProjFS binding and callbacks | asc-vfs | U55 | N |
| U59 | FUSE provider | asc-vfs | U55 | N |
| U57 | ProjFS switch, tombstones, reconcile, dehydration | asc-vfs | U56, U53 | – |
| U58 | Windows virtual end-to-end | asc, tests | U57, U40 | – |
| U60 | Submodule read-only composition | asc-vfs, asc-wc | U55, U18 | – |
| U61 | Turbo contract suite | tests/turbo | U40 (plain), U58/U59 (virtual) | – |
| U62 | asc ci checkout | asc-ci | U50 | O |
| U63 | asc ci changed/affected/warm | asc-ci | U50 | O |
| U64 | Actions and ASC workflows | actions | U62, U63, U49 | – |
| K01–K16 | Forgejo fork | Go | see §K | own track |
| O01 | GC (two-mark) | asc-store, asc-serve | U43 | P |
| O02 | Release and distribution | release | U53 | P |
| O03 | Migration runbook and admin verify | asc-serve, docs | U45, K09 | P |
| O04 | Developer docs and cheat sheet | docs, asc | U40 | P |
| O05 | Benchmarks | asc-bench | U58 | P |

Critical path to the **local-only milestone (U25)**: U01 → U02/U03 → U04 → U05/U06 → U07/U09 → U08 → U10 → U11 → U12/U14/U15/U16/U17 → U13 → U18/U19 → U20 → U21–U24 → U25.
Critical path to **disk relief on Windows (U58)**: U25 → U27 → U32 → U33/U34 → U36 → U39 → U40 → U54 → U55 → U53 → U56 → U57 → U58.
The **Forgejo track** K01–K05 needs nothing from asc and starts in week 1 if a Go engineer is available. K06 needs U41.

---

## Phase 1: local-only asc (no VFS, no network)

### U01 Workspace skeleton
- **Crate:** all. **Owns:** `Cargo.toml`, `rust-toolchain.toml` (`1.95.0`, components rustfmt, clippy), `deny.toml`, `.cargo/config.toml`, `docker-compose.it.yml` (MinIO, Garage, Postgres), `crates/*/Cargo.toml`, `crates/*/src/lib.rs` (empty), `crates/asc/src/main.rs` (prints version), `proto/buf.yaml`, `proto/buf.gen.yaml`, `.forgejo/workflows/asc-ci.yml`, `.gitignore`, `LICENSE-MIT`, `LICENSE-APACHE`.
- **Depends:** –
- **Accept:** `cargo clippy --workspace --all-targets -- -D warnings && cargo deny check && cargo run -p asc -- --version`
- **Brief:** Edition 2024, `rust-version = "1.95"`, license `MIT OR Apache-2.0`. `[workspace.dependencies]` exactly as `rust-stack.md` §10, plus `gix-index`/`gix-merge` (same gix family pin), `async-trait`, `uuid` (v7), `base64`, `regex`, `globset`, `filetime`, `sha2`. Crate features on `asc`: `default = ["serve", "vfs-projfs", "vfs-fuse"]`, each forwarding to the owning crate; `vfs-projfs` is target-gated to Windows and `vfs-fuse` to Linux. CI workflow runs fmt, clippy, deny and `cargo test --workspace` on Linux and Windows. `deny.toml` denies GPL/AGPL/LGPL and unknown registries.

### U02 Typed ids (`asc-core::id`)
- **Owns:** `crates/asc-core/src/id.rs`, `crates/asc-core/tests/golden/ids.txt`
- **Depends:** U01. **Accept:** `cargo test -p asc-core id::`
- **Brief:** Newtypes `FileId`, `ChunkId`, `TreeId`, `ChangesetId`, `TagId` over `[u8; 32]`, and `GitOid([u8; 20])`. A macro generates `from_bytes`, `as_bytes`, `Display` (64 lowercase hex), `short()` (12 hex), `FromStr` (full hex), `Ord`, `Hash`, and serde as raw arrays (postcard writes 32 bytes, no length). Hashing functions: `FileId::of(&[u8])`, `ChunkId::of`, plus a streaming `FileHasher` using `blake3::Hasher::new_derive_key` with the SPEC §2.1 context strings. `HexPrefix` (≥ 4 hex) with `matches(&[u8])`. `RefTarget` enum with the 33-byte encoding (`to_bytes`/`from_bytes`). Tests: context strings produce the golden values in `ids.txt` (generate them once, then freeze); a prefix shorter than 4 is rejected; serde round trip.

### U03 Names and paths (`asc-core::path`)
- **Owns:** `crates/asc-core/src/path.rs`
- **Depends:** U01. **Accept:** `cargo test -p asc-core path::`
- **Brief:** `Name(Vec<u8>)` and `RepoPath(Vec<u8>)` with validated constructors (SPEC §2.3), `components()`, `parent()`, `join()`, `starts_with_dir()`, byte ordering. Functions `casefold(&[u8]) -> Vec<u8>` (Unicode simple folding via `char::to_lowercase` limited to simple mappings, or ASCII lowercase for non-UTF-8), `windows_valid(&Name) -> bool`, `checkout_safe(&RepoPath) -> bool` (port git's `verify_path` NTFS/HFS rules: `.git`, `git~1`, trailing dots and spaces, `::$INDEX_ALLOCATION`, HFS ignorable code points). `RefName` validated per `git check-ref-format`. Tests: table tests copied from git's `t0060-path-utils.sh` and `t1402-check-ref-format.sh` cases; proptest that valid paths round-trip.

### U04 Objects and canonical codec (`asc-core::object`, `codec`)
- **Owns:** `crates/asc-core/src/{object.rs,codec.rs,sig.rs}`, `crates/asc-core/tests/golden/envelopes/*`
- **Depends:** U02, U03. **Accept:** `cargo test -p asc-core -- object:: codec::`
- **Brief:** The structs exactly as SPEC §2.4–§2.7 (`Tree`, `Entry`, `Kind`, `LfsRef`, `Changeset`, `Signature`, `Change`, `TagRecord`, `ChunkList`, `AscGitExtra` (kind 5 is reserved; no `FileMeta`)). `SortedMap<K, V>` (a `Vec<(K, V)>` whose constructor sorts, and whose deserialiser rejects unsorted or duplicate keys). `encode_envelope(kind, version, &T) -> Vec<u8>`; `decode_envelope::<T>(&[u8]) -> Result<T>` rejects bad magic, unknown kind/version and trailing bytes. `Tree::id()`, `Changeset::id()`, `TagRecord::id()`. `Tree` invariants checked in `Tree::new` (sorted, unique, non-empty unless root, Gitlink id zero, `lfs` only on File/Exec). Tests: proptest `decode(encode(x)) == x` and `encode(decode(b)) == b`; golden envelope bytes for one value of each type (frozen hex files); rejection tests for each malformed case.

### U05 Git object rendering (`asc-core::git`)
- **Owns:** `crates/asc-core/src/git.rs`, `crates/asc-core/tests/golden/git/*`
- **Depends:** U04. **Accept:** `cargo test -p asc-core git::`
- **Brief:** `blob_oid(&[u8])`, `BlobOidHasher` (streaming), `render_tree(&Tree) -> Vec<u8>` (git sort order, modes per SPEC §2.9) and `tree_oid`, `render_commit(&Changeset, root_git, parent_gits) -> Vec<u8>` (canonical, with the optional `asc` header and `gpgsig` header with continuation lines), `parse_commit(raw) -> ParsedCommit`, `canonicalise_commit(raw, ...) -> (Changeset fields, Option<raw>)` (SPEC §2.9 rule), the same for tags, `parse_lfs_pointer(&[u8]) -> Option<LfsRef>`, `tz_to_string(i16)`/`parse_tz`. Golden tests: fixtures created with `git hash-object`, `git mktree`, `git commit-tree` (including a signed commit, a merge, a non-UTF-8 author, `+0530`, an `encoding` header, which must stay raw) committed as files with expected SHAs. Property: canonical render → parse → canonicalise is the identity.

### U06 Content ingest and chunking (`asc-core::content`)
- **Owns:** `crates/asc-core/src/content.rs`
- **Depends:** U02. **Accept:** `cargo test -p asc-core content::`
- **Brief:** `pub const CHUNK_THRESHOLD: u64 = 4 * 1024 * 1024`. `ingest(reader) -> Ingested { file: FileId, size, git: GitOid, sha256: [u8; 32], body: Body }`, where `Body::Inline(Bytes)` or `Body::Chunked { list: ChunkList, chunks: Vec<(ChunkId, Bytes)> }`. One pass computes BLAKE3, SHA-1 (blob header needs the size: use a two-pass read for a seekable source, or buffer ≤ 4 MiB, then stream) and SHA-256. FastCDC v2020 with the SPEC parameters. `ChunkAssembler` verifies each chunk and the final `FileId`. Tests: chunk boundaries are stable for a fixed pseudo-random 20 MiB input (golden chunk ids); inline/chunked threshold edges (4 MiB exactly is inline); a corrupt chunk is detected.

### U07 Objects trait, keys, bucket backend (`asc-store::{key,objects,bucket,conformance}`)
- **Owns:** `crates/asc-store/src/{key.rs,objects.rs,bucket.rs,codec.rs,conformance.rs}`
- **Depends:** U04, U06. **Accept:** `cargo test -p asc-store -- objects:: bucket::`
- **Brief:** `ObjKey` (SPEC §3.1) with `path(&self) -> String` producing the layout keys (`f/<h2>/<hex>`, …), and `KeyPrefix`. The `Objects` trait (SPEC §3.2). `codec::{encode_stored, decode_stored}` (codec byte; zstd level 3 only if < 90%). `verify(key, &data)` for verifiable kinds. `BucketObjects::new(store: Arc<dyn ObjectStore>, root: Path)` with `InMemory` and `LocalFileSystem` constructors (`from_url("file:///…")`, `memory()`). `put` = `PutMode::Create` with `AlreadyExists` → Ok (the S3 fallback is added in U29). `conformance::run_objects_suite(impl Objects)`, a public async fn behind feature `testkit`: put/get/has_many/range/idempotent put/verify-corrupt detection (tamper with the underlying store and expect `Error::Corrupt`).

### U08 CacheDir, StoreSet, Tiered (`asc-store::{cachedir,tiered}`)
- **Owns:** `crates/asc-store/src/{cachedir.rs,tiered.rs,storeset.rs}`
- **Depends:** U07. **Accept:** `cargo test -p asc-store -- cachedir:: tiered::`
- **Brief:** `CacheDir::open(root, tmp)` stores **decoded** data at `<root>/<letter>/<h2>/<hex>`. Atomic write: temp file, fsync, rename; an existing destination counts as success. Opens with `FILE_SHARE_DELETE` on Windows (`std::os::windows::fs::OpenOptionsExt::share_mode`). `get` verifies and deletes corrupt files. Touches mtime on read if it is older than 24 h (`filetime`). `content_path(FileId) -> Option<PathBuf>` for reflink, ProjFS and passthrough. `evict_to(max_bytes)` evicts by oldest mtime down to 80%. `StoreSet { store: CacheDir, cache: CacheDir }`: reads store then cache, writes new objects to the store, `demote(keys)` renames store → cache. `Tiered<A, B>`: read-through with fill and write-to-A. Run the U07 conformance suite against `CacheDir` and `Tiered<CacheDir, InMemory>`. Tests: concurrent writers of the same key (16 tasks) all succeed with one final file; eviction keeps `store/` intact.

### U09 Refs trait and SqlRefs (`asc-store::{refs,sqlrefs}`)
- **Owns:** `crates/asc-store/src/{refs.rs,sqlrefs.rs}`, `crates/asc-store/migrations/{sqlite,postgres}/0001_refs.sql`
- **Depends:** U04. **Accept:** `cargo test -p asc-store refs::` (Postgres: `cargo test -p asc-store --features it-pg -- --ignored refs`)
- **Brief:** The `Refs` trait, `RefTxn`, `RefUpdate`, `Expect`, `RefConflict`, `RefLogEntry`, `Actor`, `RepoKey(i64)`. `SqlRefs::new(AnyPool)` with the SPEC §3.4 schema and `migrate()`. `update` runs one transaction: per update a versioned `UPDATE`/`INSERT`/`DELETE`, and on a zero-row result rolls back with `RefConflict{name, actual}`, plus `ref_log` inserts. Expose `update_in_tx(&mut Transaction, …)` so `asc-serve` can append statements. `list` by prefix (`LIKE prefix || '%'` with escaping). `expires_at` stored and returned. `conformance::run_refs_suite` (behind `testkit`): absent→create, CAS mismatch, multi-ref atomicity, delete, log order, and a race (100 tasks doing CAS increments; exactly one winner per version).

### U10 Tree apply and diff (`asc-engine::tree`)
- **Owns:** `crates/asc-engine/src/tree.rs`, `crates/asc-engine/src/lib.rs` (Engine struct skeleton)
- **Depends:** U05, U08. **Accept:** `cargo test -p asc-engine tree::`
- **Brief:** `Engine<O: Objects, R: Refs> { objects: Arc<O>, refs: Arc<R>, repo: RepoKey, trees: quick_cache::sync::Cache<TreeId, Arc<Tree>> }`. `get_tree`, `put_tree`. `apply(root, &[(RepoPath, Change)]) -> TreeId` (SPEC §4.1: groups by directory, bottom-up rebuild, recomputes `size` and `git` for directories, prunes empty dirs, errors on writes through an existing file without a `Delete`). `leafdiff(a, b, Option<&Pathspec>) -> Vec<(RepoPath, Change)>` with hash pruning. `lookup(root, path) -> Option<Entry>`. `walk(root, pathspec) -> Stream<(RepoPath, Entry)>`. `Pathspec` wraps `globset` with git pathspec magic `:(glob)`, `:(exclude)` and literal prefixes. Tests: proptest random trees, `apply(a, leafdiff(a, b)) == b`, pruned diff == naive diff, file↔dir replacement.

### U11 Changeset build and validation (`asc-engine::commit`)
- **Owns:** `crates/asc-engine/src/commit.rs`
- **Depends:** U10. **Accept:** `cargo test -p asc-engine commit::`
- **Brief:** `NewChangeset { parents, author, committer, message, extra, root: TreeId, copies: Vec<(RepoPath, RepoPath)> }` → `Engine::create_changeset(..) -> ChangesetId`, which computes `changes = leafdiff(parent root, root)`, attaches `copy_from`, encodes and stores it. `Engine::validate_changeset(&Changeset, PathPolicy) -> Result<()>` implements every rule of SPEC §2.6, with `PathPolicy::{Import, Native { windows_safe }}` per SPEC §2.8 (the new-path checks look at the resulting tree's siblings). `get_changeset` with a cache. Tests: each validation rule has a failing case; a merge changeset relative to p1; a native windows_safe rejection for `CON.txt` and for `A.ts` next to an existing `a.ts`; the import policy accepts both.

### U12 Commit graph and log (`asc-engine::graph`)
- **Owns:** `crates/asc-engine/src/graph.rs`
- **Depends:** U11, U09. **Accept:** `cargo test -p asc-engine graph::`
- **Brief:** A `GraphSource` trait (`get(id) -> (gen, parents)` and batch get) with implementations for "derived via Engine" (U14 supplies `gen`; until then compute recursively and memoise in memory) and, later, SQL. `Graph` keeps an in-memory `HashMap<ChangesetId, Node>`. `merge_base(a, &[b], all: bool)` (paint-down by descending generation), `is_ancestor`, `log(LogQuery) -> Stream<ChangesetId>` (`include`/`exclude` sets, `first_parent`, path filter over `changes` prefixes, `--follow` through `copy_from`, skip/limit, author/grep/since/until filters on decoded headers), `count`. Tests: generated DAGs compared with `git merge-base --all` and `git rev-list` on the same DAG built via `git commit-tree` (test helper builds both).

### U13 Revspec resolution (`asc-engine::revspec`)
- **Owns:** `crates/asc-engine/src/revspec.rs`
- **Depends:** U12, U14. **Accept:** `cargo test -p asc-engine revspec::`
- **Brief:** A parser for the full grammar in SPEC §4.2 into `RevExpr`. `Resolver` trait: `resolve_ref(name) -> Option<RefTarget>` (DWIM order), `reflog(name, n)`, `upstream(branch)`, `git_to_asc(prefix) -> Vec<ChangesetId>`, `asc_prefix(prefix)`. `resolve(expr) -> Resolved { Single(ChangesetId) | Range{include, exclude} | TreePath(ChangesetId, RepoPath) }`. Ambiguity errors list candidates. Tests: a table of 40 expressions against a fixture graph, including `@{u}`, `:/fix`, `HEAD~2^2`, `A...B`, `x^!`, and a 40-hex git SHA.

### U14 Derived data (`asc-engine::derive`)
- **Owns:** `crates/asc-engine/src/derive/{mod.rs,gen.rs,gitcommit.rs}`
- **Depends:** U11, U05. **Accept:** `cargo test -p asc-engine derive::`
- **Brief:** The `Derived` trait (SPEC §5). `get::<D>(id)`: an iterative topological walk to find underived ancestors (an explicit stack, no recursion), derive oldest first, store at `ObjKey::Derived`. `GitMapSink` trait (implemented later by state.db and the server) receives `(git, kind, asc)` rows; `gitcommit` emits rows for the changeset, and `derive::index_tree_git(tree)` emits file and tree rows. `gen` value `{gen: u32}`. `gitcommit`: raw bytes if `extra["git.commit"]` is present, otherwise the canonical render with `root_git = tree_oid(root)` and parents mapped through `gitcommit`. `Engine::warm(id)` derives all eager types. Tests: a 10k-changeset linear chain derives without stack overflow; a native commit's `gitcommit` equals `git commit-tree` output for the same tree, parents and identity.

### U15 Line diff and output formats (`asc-engine::diff`)
- **Owns:** `crates/asc-engine/src/diff/{mod.rs,line.rs,format.rs,rename.rs}`, `crates/asc-engine/tests/diff_golden/**`
- **Depends:** U10. **Accept:** `cargo test -p asc-engine diff::`
- **Brief:** `TreeChange { path, old: Option<Entry>, new: Option<Entry>, status: A|M|D|R(score)|C|T }` from `leafdiff`. Renames per SPEC §4.3 (`rename.rs`). Line diff: `imara-diff` Myers plus indent-heuristic post-processing, with binary detection (NUL in the first 8 KiB, as git does). Formatters writing to `impl Write`: name-only, name-status, raw, stat (git's width rules, `--stat=<w>`), numstat, shortstat, unified patch (headers `diff --git`, `index <short>..<short> <mode>`, `new file mode`, `deleted file mode`, `rename from/to`, `similarity index`, `Binary files … differ`, `\ No newline at end of file`, `-U<n>`, function-context header lines like git's default `funcname` regex), word-diff plain. A content source trait for fetching file bytes lazily. Golden tests: 200 file pairs extracted from ASC history (script `tests/diff_golden/extract.sh` checked in; fixtures committed) compared with `git diff --no-index` output. Assert `git apply --check` succeeds for all and byte equality for ≥ 99%.

### U16 Git index and ODB access (`asc-git::{index,odb}`)
- **Owns:** `crates/asc-git/src/{index.rs,odb.rs}`
- **Depends:** U05, U06. **Accept:** `cargo test -p asc-git index:: odb::`
- **Brief:** `IndexFile` read/write over `gix-index` (v2 plus TREE extension), with `IndexEntry { path, mode, oid, size, ctime, mtime, dev, ino, uid, gid, flags: {stage, skip_worktree, intent_to_add} }`. `IndexLock::acquire(gitdir, timeout = 5 s)` uses git's `index.lock` protocol and git's error text; `commit()` fsyncs and renames; `set_index_mtime(t)`. Build the cache-tree from an asc tree (`git` fields). `Odb::open(common_dir)` over gix ODB (loose, packs, alternates): `read_blob_verified(oid) -> Result<Bytes>` (re-hashes), `write_loose(kind, bytes) -> GitOid`, `contains(oid)`. Tests: an index written by asc is accepted by `git ls-files -s` and `git status` (using a temporary real git repo); a git-written index round-trips through asc unchanged; a held lock produces git's message after the timeout.

### U17 $ASC_HOME and state.db (`asc-wc::{home,state}`)
- **Owns:** `crates/asc-wc/src/{home.rs,state.rs}`, `crates/asc-wc/migrations/0001_state.sql`
- **Depends:** U09. **Accept:** `cargo test -p asc-wc state:: home::`
- **Brief:** `AscHome::locate()` (`ASC_HOME` env → global git config `asc.home` → platform default) creates the SPEC §3.5 directories. `StateDb::open(home)` is an sqlx SQLite pool (WAL mode, `busy_timeout = 5000`) with the SPEC §9.2 schema. Typed accessors: `repos` (insert/get/by_name), `workspaces` (insert/get/by_path/update_head_base/epoch bump/list), `git_map` (`insert_many`, `git_to_asc`, `asc_to_git`, implementing `GitMapSink`), `cs_gen`, `entry_times`, `journal` (append batch / since / clear), `switch_intent`, `ws_ops`, `inodes`. Local refs reuse `SqlRefs` over the same pool. Tests: migrations apply twice idempotently; concurrent CLI processes (spawn 4) can write without `SQLITE_BUSY` failures.

### U18 Plain working copy (`asc-wc::{plain,status,switch}`)
- **Owns:** `crates/asc-wc/src/{plain.rs,status.rs,switch.rs,ignore.rs,redirect.rs}`
- **Depends:** U16, U17, U10. **Accept:** `cargo test -p asc-wc plain:: status:: switch::`
- **Brief:** `Materialiser` writes files from `StoreSet` content paths using `reflink_copy::reflink_or_copy` (never hardlinks), applying `checkout_safe`, exec bits and symlinks (Windows: a symlink if permitted, else a text file per `core.symlinks`). `plain::checkout(ws, from_root, to_root, mode: Safe|Merge|Force) -> SwitchReport` updates only changed paths (write to a temp file, rename) and rewrites the index from metadata with real stat. `status::compute(ws) -> Status` (staged = index vs HEAD tree via oids plus cache-tree pruning; unstaged = stat check, hashing only stat-dirty files with `BlobOidHasher`; untracked via the `ignore` crate honouring `.gitignore`, `.git/info/exclude`, `core.excludesFile`). Sparse cones set skip-worktree. Redirection list loader (`.asc/redirections`). `switch::plan(from, to, dirty) -> Plan { updates, deletes, conflicts }` is OS-neutral and reused by the VFS. Tests: switch refuses on a dirty conflict in Safe mode; Force overwrites; untracked files survive; status equals `git status --porcelain=v2` on the same materialised tree.

### U19 Projection layout and config (`asc-git::projection::{layout,config}`)
- **Owns:** `crates/asc-git/src/projection/{mod.rs,layout.rs,config.rs}`
- **Depends:** U16, U17. **Accept:** `cargo test -p asc-git projection::layout projection::config`
- **Brief:** `Projection::create(home, repo, ws, root)` writes the `.git` gitfile, `wc/<ws>/git/{commondir,HEAD,config.worktree,logs/}`, and the common dir `repos/<id>/.git` with the SPEC §9.4 config keys (via `gix-config` writes). `write_head(Head)`. `detect_user_hooks_path()` reads `core.hooksPath` from local and global config but **not** worktree config. `Projection::open(root)` discovers by walking up to a `.git` gitfile whose target is under `$ASC_HOME/wc/`. Tests: `git rev-parse --git-dir --git-common-dir --show-toplevel` returns the expected paths; `git config core.hooksPath` from inside returns asc's value even after `git config core.hooksPath .husky/_` (worktree config wins); `core.autocrlf` is false even with a global true (set `HOME` to a temp dir with a global config).

### U20 CLI frame, init, config (`asc` binary)
- **Owns:** `crates/asc/src/{main.rs,dispatch.rs}`, `crates/asc/src/cli/{mod.rs,common.rs,output.rs,pager.rs,init.rs,config.rs}`
- **Depends:** U13, U18, U19. **Accept:** `cargo test -p asc --test cli_frame`, with `crates/asc/tests/cli_frame.rs` owned by this unit
- **Brief:** argv[0] dispatch (`asc`, `git-remote-asc`, `asc-fsmonitor`, case-insensitive and ignoring `.exe`). A clap derive tree with every command in SPEC §11.2 declared; unimplemented ones return `asc: '<cmd>' is not implemented yet` (exit 1) until their unit lands. Global `-C`, `-c k=v`, `--no-pager`. Alias expansion (`alias.*`, including `!shell` aliases), and `asc-<name>` plugin lookup. Pager and colour policy as git. `Ctx` loads `AscHome`, `StateDb`, the projection, the git config snapshot, and an `Engine` over `Tiered<StoreSet, NoRemote>`. Error policy: an unknown flag → exit 129 with `flag '<x>' is not supported yet`. `asc init [dir] [-b]` creates a local-only repo (ns = random), a plain workspace, the projection and the unborn branch. `asc config` delegates to gix-config with git's flags. Tests: `init`, then `git rev-parse --show-toplevel` works; `-C`; an alias; an unsupported flag gives 129.

### U21 CLI status/add/rm/mv/restore/reset
- **Owns:** `crates/asc/src/cli/{status.rs,add.rs,rm.rs,mv.rs,restore.rs,reset.rs}`
- **Depends:** U20, U15. **Accept:** `cargo test -p asc --test cli_stage`, with `crates/asc/tests/cli_stage.rs` owned
- **Brief:** `status` in all porcelain forms (v1, v2, `-s`, `-b`, `-z`, `-u` modes, ahead/behind against the upstream). `add` writes loose objects (U16 `write_loose`), ingests into `store/` and adds a `git_map` row; flags `-p` (interactive hunk selection on the file-vs-index diff, git's prompt letters `y n q a d s e ?`), `-u`, `-A`, `-N`, `-f`, `-n`. `rm` (`--cached`, `-r`, `-f`). `mv` (records a pending rename in `ws_ops` for `copy_from`). `restore` (`--staged`, `--worktree`, `--source`). `reset` (`--soft/--mixed/--hard`, paths). Plain mode only at this point. Tests: compare with git on identical fixtures (porcelain v2 output), `add -p` driven by stdin.

### U22 CLI commit
- **Owns:** `crates/asc/src/cli/commit.rs`, `crates/asc/src/hooks_runner.rs`
- **Depends:** U20, U14. **Accept:** `cargo test -p asc --test cli_commit`, with `crates/asc/tests/cli_commit.rs` owned
- **Brief:** SPEC §9.7 exactly: index vs HEAD; unknown oids read **from the ODB with verification** (never the working file); apply and leafdiff; the hook chain (`pre-commit`, `prepare-commit-msg`, `commit-msg`, `post-commit`) resolved via U19; `-m`, `-F`, `-a`, `--amend`, `--fixup`, `--squash`, `--no-verify`, `--allow-empty`, `--author`, `--date`, `-e`, `-v`, `-S` (git's signing program; signature into `git.gpgsig`), message cleanup like git (`commit.cleanup`, default strip). Writes the changeset to `store/`, warms derived data, CAS-moves the branch and writes the reflog. Writing the commit into the git ODB and refs mirror is a hook point filled by U36 (no-op until then). Tests: **`git add` a file, edit it again, `asc commit` → the committed blob equals the staged version**; `--amend`; a hook rejecting the commit; a merge commit from `MERGE_HEAD`.

### U23 CLI branch/switch/checkout/tag
- **Owns:** `crates/asc/src/cli/{branch.rs,switch.rs,checkout.rs,tag.rs}`
- **Depends:** U20. **Accept:** `cargo test -p asc --test cli_branch`, with `crates/asc/tests/cli_branch.rs` owned
- **Brief:** `branch` with all flags in SPEC §11.2 (`--contains`/`--merged` via the graph, `--format` with `%(refname)`, `%(objectname)`, `%(upstream)`). `switch`/`checkout` over U18 plain checkout (`-c/-C/--detach/--orphan/--merge/-f/-`); `checkout <rev> -- <paths>`; `checkout <branch>` DWIM to a remote-tracking branch. `tag` lightweight and annotated (`TagRecord`), `-s` signing, `-d`, `-l` with patterns, `-n`. Reflog entries for HEAD and branches. Tests vs git: switch with an untracked file in the way; `-` toggles; tag listing order.

### U24 CLI history and plumbing verbs
- **Owns:** `crates/asc/src/cli/{log.rs,show.rs,diff.rs,plumbing.rs,reflog.rs,format.rs}`
- **Depends:** U20, U15. **Accept:** `cargo test -p asc --test cli_history`, with `crates/asc/tests/cli_history.rs` owned
- **Brief:** `log` (flags in SPEC §11.2; the `--graph` renderer ports git's graph lane algorithm; `--format` placeholders as listed in SPEC §11.1; `--decorate`), `show`, `diff` (worktree/index/HEAD/rev pairs, all formats), `rev-parse` (incl. `--git`/`--asc`, `--show-toplevel`, `--abbrev-ref`, `--verify`, `--short`), `rev-list` (`--count`, ranges), `merge-base` (`--all`, `--is-ancestor`), `ls-files` (`-s`, `-o`, `--exclude-standard`, `-z`), `ls-tree` (`-r`, `-l`, `--name-only`, `--format`), `cat-file` (`-t`, `-s`, `-p`, `--batch`), `diff-tree`, `diff-index`, `hash-object`, `show-ref`, `for-each-ref`, `symbolic-ref`, `update-ref`, `describe`, `reflog`. Ids are displayed per `asc.displayIds`. Tests: output equality with git on a fixture repo created by both tools with identical timestamps (`GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` honoured by asc).

### U25 Scenario harness and local-only milestone
- **Owns:** `crates/asc/tests/scenario.rs`, `crates/asc/tests/scenarios/*.t`
- **Depends:** U21, U22, U23, U24. **Accept:** `cargo test -p asc --test scenario`
- **Brief:** A runner for `.t` files: each is a shell script (bash on Unix, run through Git Bash on Windows) using a `$VCS` variable. The runner executes it twice, with `VCS=git` and `VCS=asc`, in fresh temp dirs with fixed env (`GIT_AUTHOR_*`, `GIT_COMMITTER_*`, `TZ=UTC`, `HOME`), captures stdout and exit codes per command, normalises ids (replacing each distinct 40/64-hex id with `<id1>`, …), and diffs. Scenarios: `init-add-commit.t`, `status-variants.t`, `branch-switch.t`, `diff-log.t`, `reset-restore.t`, `tag.t`, `rename.t`, `ignore.t`. **Milestone:** a local-only asc handles init/add/commit/status/diff/log/branch/switch end to end.

### U26 Merge, rebase, cherry-pick, land check (`asc-engine::{merge,rebase,land}`)
- **Owns:** `crates/asc-engine/src/merge/{mod.rs,tree.rs,text.rs}`, `crates/asc-engine/src/{rebase.rs,land.rs}`, `crates/asc-engine/tests/merge_golden/**`
- **Depends:** U15, U13. **Accept:** `cargo test -p asc-engine merge:: rebase:: land::`
- **Brief:** `merge_trees(base, ours, theirs, opts) -> MergeOutcome { root: TreeId, conflicts: Vec<Conflict{path, kind: Content|AddAdd|ModifyDelete|FileDir|CaseFold|Binary, stages}> }` per SPEC §4.4; the text merge via `gix-merge` `blob::builtin_driver::text` with `merge`/`diff3`/`zdiff3` and `-X ours|theirs`. `cherry_pick(cs, onto)`, `revert(cs, onto)`. `rebase_metadata(cs, onto) -> Option<Changeset>` (succeeds only when disjoint). `rebase_stack(stack, onto) -> Vec<Result<..>>`. `land::paths_since(base, tip) -> PathSet` (first-parent union, or the tree-diff fallback); `PathSet::conflicts(&other) -> Vec<(RepoPath, RepoPath)>` using the casefold trie of SPEC §4.5. Tests: goldens vs `git merge-tree --write-tree` and `git merge-file` (conflict markers byte-equal); the disjointness truth table (`a/b` vs `a`, `A.ts` vs `a.ts`, `x/` vs `x/y/z`, unrelated paths).

### U27 CLI merge/rebase/cherry-pick/revert/stash
- **Owns:** `crates/asc/src/cli/{merge.rs,rebase.rs,pick.rs,stash.rs}`, `crates/asc/tests/scenarios/{merge,rebase,stash,pick}.t`
- **Depends:** U25, U26. **Accept:** `cargo test -p asc --test scenario -- merge rebase stash pick`
- **Brief:** Conflict materialisation (markers in files, index stages 1/2/3, `MERGE_HEAD`/`MERGE_MSG`/`CHERRY_PICK_HEAD`); `--continue/--abort/--skip`; rebase state in `ws_ops`; `rebase -i` with git's todo grammar (`pick reword edit squash fixup drop exec break`, `--autosquash`, `--update-refs`) using `core.editor`/`GIT_SEQUENCE_EDITOR`; `pull` is added in U32. `stash` with git's commit structure, `refs/stash` and its reflog, `-u`, paths, `pop` conflicts kept. Scenarios as listed must match git.

### U28 Blame engine and CLI (`asc-engine::blame`, `asc` blame)
- **Owns:** `crates/asc-engine/src/blame.rs`, `crates/asc/src/cli/blame.rs`, `crates/asc/tests/scenarios/blame.t`
- **Depends:** U25, U15. **Accept:** `cargo test -p asc-engine blame:: && cargo test -p asc --test scenario -- blame`
- **Brief:** SPEC §4.6, with derived `blame.v1` memoisation keyed by `blake3(cs ‖ path)`. Output formats: default, `--porcelain`, `--line-porcelain`, `--incremental`, `-L a,b`, `-w`, `--reverse`. Dirty working files are blamed with uncommitted lines as `Not Committed Yet`. Tests: line origins equal `git blame --porcelain` on fixture histories (ties are allowed to differ only where git's `-M` heuristics differ; no `-M` by default).

---

## Phase 2: remotes and S3

### U29 S3 bucket objects and put fallback (`asc-store::bucket` S3 part)
- **Owns:** `crates/asc-store/src/s3.rs`, `crates/asc-store/tests/it_s3.rs`
- **Depends:** U07. **Accept:** `cargo test -p asc-store s3:: && docker compose -f docker-compose.it.yml up -d minio garage && cargo test -p asc-store --features it-s3 --test it_s3 -- --ignored`
- **Brief:** `BucketObjects::from_url("s3://bucket/prefix?endpoint=&region=&path_style=")` builds `AmazonS3Builder` (credentials from env, or `git credential fill` with `protocol=s3`). Put fallback per SPEC §3.3: an `AtomicBool create_unsupported` per store instance, set on `NotImplemented`/501/400-with-conditional-message, then `PutMode::Overwrite`. Multipart above 32 MiB. Retries via `backon` (exponential, 5 tries) for 5xx and timeouts only. Conformance on MinIO and Garage. Test the fallback with a wrapper store that returns `NotImplemented` for `Create`.

### U30 BucketRefs and probe (`asc-store::{bucketrefs,probe}`)
- **Owns:** `crates/asc-store/src/{bucketrefs.rs,probe.rs}`, `crates/asc-store/tests/it_bucketrefs.rs`
- **Depends:** U07, U09. **Accept:** `cargo test -p asc-store bucketrefs:: probe:: && cargo test -p asc-store --features it-s3 --test it_bucketrefs -- --ignored`
- **Brief:** SPEC §3.4 exactly: `BucketRefEntry`, the `rl/<ref>/<v:020>` create-only CAS, retry success iff `op_id` matches, `r/<ref>` hint and forward probing, deletes as entries with `new = None`, `list` via the `r/` prefix, `log` from entries. `probe(store) -> ProbeResult { create_exclusive: bool, detail }`. Tests: race (32 writers, one ref) with exactly one winner per version; a retried create after a simulated lost response returns success (fault-injecting store that performs the put and then returns a timeout); ABA (A→B→A) is impossible because versions only increase; the probe fails on a store that ignores `Create`.

### U31 Sync engine (`asc-engine::sync`)
- **Owns:** `crates/asc-engine/src/sync.rs`
- **Depends:** U08, U09, U14. **Accept:** `cargo test -p asc-engine sync::`
- **Brief:** A `Remote` trait: `list_refs`, `has_many`, `get_many` (stream), `put_many` (stream), `update_refs`, and optional `history: Option<&dyn HistoryApi>`. `BucketRemote` implements it over `BucketObjects` and `BucketRefs` or `SqlRefs`. `fetch(remote, refspecs) -> FetchReport` per SPEC §6.3 (changesets only; `--prefetch` pathspec walks trees and files). `push(remote, updates, mode) -> PushReport`: closure computation, `has_many` batches of 4,096, dependency-ordered `put_many` with 8 concurrent streams of ≤ 16 MiB, ref update, demotion. `clone_minimal(remote, branch)`. Tests: two in-memory "machines" push and fetch one `file://` bucket; an interrupted push (fault after N puts) resumes and uploads only the rest; a push uploads nothing for unchanged trees.

### U32 CLI remote/fetch/push/pull/clone for bucket remotes
- **Owns:** `crates/asc/src/cli/{remote.rs,fetch.rs,push.rs,pull.rs,clone.rs}`, `crates/asc/src/remote/{mod.rs,url.rs,bucket.rs}`, `crates/asc/tests/scenarios/remote-bucket.t`
- **Depends:** U27, U29, U30, U31. **Accept:** `cargo test -p asc --test scenario -- remote-bucket && cargo test -p asc --test cli_remote`, with `crates/asc/tests/cli_remote.rs` owned
- **Brief:** URL parsing per SPEC §6.2 into `RemoteSpec { Bucket{url, refs: Shared|Local}, Git{url}, Server{url}, Discover{url} }` (discovery is stubbed to Git until U50). Remotes are stored in git config `remote.<n>.url` and `.fetch`, plus `remote.<n>.ascRefs=local` for single-writer buckets. `fetch` (`--prune`, `--all`, `--tags`, `--prefetch`), `push` (flags in SPEC §11.2 except `--land`), `pull` (merge or rebase per config), `clone` for bucket URLs (plain mode). `asc init --store <url> [--refs local]`. Test: two clones of the same `file://` bucket exchange commits; a non-fast-forward push is rejected with git's message.

---

## Phase 3: git interop

### U33 Git import (`asc-git::import`)
- **Owns:** `crates/asc-git/src/import.rs`, `crates/asc-git/tests/fixtures/**` (fixture-builder scripts), `crates/asc-git/tests/import.rs`
- **Depends:** U14, U16. **Accept:** `cargo test -p asc-git --test import`
- **Brief:** `Importer::new(engine, odb, map: &dyn GitMapSink, policy)`, with `import_commits(tips) -> ImportReport` per SPEC §14.1: topological order of unknown commits; first-parent tree diff via gix; blob ingest; LFS pointer detection to `LfsRef` with pointer identity; gitlinks; asc tree building with actual git oids and overrides for non-canonical trees; commit canonicalisation; validation with `PathPolicy::Import`; derive; **assert `gitcommit == oid`**; tags. Incremental (skips oids in the map). Fixtures built by scripts with real git: signed commits, odd modes (`100664`), an unsorted tree (written with `git hash-object -t tree --literally`), LFS pointers, a gitlink, a non-UTF-8 author, the `encoding` header, octopus merges, `A.ts`/`a.ts` collisions, and a `CON` file.

### U34 Git export (`asc-git::export`)
- **Owns:** `crates/asc-git/src/export.rs`, `crates/asc-git/tests/export.rs`
- **Depends:** U14, U16. **Accept:** `cargo test -p asc-git --test export`
- **Brief:** `Exporter::export(changesets | ref tips, target: &Odb) -> ExportReport`: renders blobs (pointer bytes for LFS), trees (override or render), commits and tags; skips oids present in the target (`contains`); writes loose objects, or one pack (built with gix pack writing, no deltas) plus `git index-pack` when there are more than 1,000 objects; `set_refs(&[(name, oid)])` via `git update-ref --stdin`. Tests: export of a native history produces commits identical to `git commit-tree` outputs; native → export → import yields identical `ChangesetId`s, including `copy_from` through the `asc` header and a `-S` signature.

### U35 ASC round-trip milestone
- **Owns:** `crates/asc-git/examples/roundtrip.rs`, `crates/asc-git/tests/asc_roundtrip.rs`
- **Depends:** U33, U34. **Accept:** `cargo run -p asc-git --release --example roundtrip -- D:\dev\monorepo` prints `mismatches: 0` (and `cargo test -p asc-git --test asc_roundtrip -- --ignored`)
- **Brief:** Opens the repo **read-only** (never writes to it), imports all refs' history into an `InMemory`/temp store, exports to a temp bare repo, and compares every commit, tree, tag and blob oid. Prints counts, elapsed time, the number of overrides needed and the LFS pointers found. Also imports `D:\dev\soma` when present. Report-only output; no ASC files modified.

### U36 Projection objects and refs mirror (`asc-git::projection::{objects,refs,seed}`)
- **Owns:** `crates/asc-git/src/projection/{objects.rs,refs.rs,seed.rs,shallow.rs}`
- **Depends:** U19, U34. **Accept:** `cargo test -p asc-git projection::objects projection::refs`
- **Brief:** `ensure_commits(tips)` renders all missing ancestor commits into the common ODB (stops at commits already present). `ensure_tree(root)` renders one tree level, `ensure_trees_recursive(root)`, and adds `git_map` rows for entries. `seed_blobs(root, globs)` writes the manifest blobs (SPEC §9.4). `mirror_refs(&StateDb)` writes `refs/heads`, `refs/tags`, `refs/remotes`, `refs/stash` plus its reflog, packed-refs for large sets, and the workspace `HEAD`. `write_shallow(set)`. Hooks are filled into U22's commit path and U23's switch path (`on_ref_moved`, `on_head_moved`). Tests: after an asc commit and switch, `git log --oneline -5`, `git rev-parse HEAD^{tree}` and `git status` (clean) work in the projection with no blob reads except seeded ones (count via the ODB loose directory).

### U37 Hooks dispatcher and reconcile (`asc-git::projection::{hooks,reconcile}`, `asc internal`)
- **Owns:** `crates/asc-git/src/projection/{hooks.rs,reconcile.rs}`, `crates/asc/src/cli/internal.rs`, `crates/asc/tests/scenarios/git-in-projection.t`
- **Depends:** U36, U33, U22. **Accept:** `cargo test -p asc-git projection::reconcile && cargo test -p asc --test scenario -- git-in-projection`
- **Brief:** Install the dispatcher scripts in `$ASC_HOME/hooks/` (POSIX sh, run by git's bundled sh on Windows) that call `asc internal hook <name> "$@"` and then chain to the user's hook (SPEC §9.4). `reconcile(ws)` per SPEC §9.6 (CAS, orphan refs, HEAD adoption). Every `asc` command start calls `reconcile_fast` (compares the ref mirror's mtime and value snapshot; a no-op in < 1 ms when unchanged). Scenario: `git commit -m x` in a projection followed by `asc log -1` shows the commit with the same git SHA; `git commit --no-verify` with `core.hooksPath` changed is still picked up by the next `asc status`; `git checkout -b y` is adopted; husky's `pre-commit` runs both for `asc commit` and for `git commit`.

### U38 git-remote-asc helper (`asc-git::remote_helper`)
- **Owns:** `crates/asc-git/src/remote_helper.rs`, `crates/asc/src/cli/remote_helper_main.rs`
- **Depends:** U36, U31. **Accept:** `cargo test -p asc-git remote_helper::`
- **Brief:** The helper protocol: `capabilities` (`stateless-connect`, `push`, `option`), `stateless-connect git-upload-pack` speaking protocol v2 (`ls-refs`, `fetch` with explicit wants only; pkt-line framing; a response pack of whole objects, no deltas, no thin pack). Object source: `git_map` → asc id → `Tiered` read → git rendering. `push` lines → reconcile → sync push of that ref → `ok`/`error`. Tests: in a projected repo with no blobs, `git show HEAD:<file>` fetches exactly one blob; `git diff HEAD~1` fetches only changed blobs; `git push origin HEAD:refs/heads/x` from git lands the branch in the asc remote (a `file://` bucket).

### U39 Git-remote mode (`asc-git::gitremote`)
- **Owns:** `crates/asc-git/src/gitremote.rs`, `crates/asc/src/remote/git.rs`, `crates/asc/tests/scenarios/git-remote.t`
- **Depends:** U32, U33, U34, U36. **Accept:** `cargo test -p asc --test scenario -- git-remote`
- **Brief:** A `GitRemote` that implements U31's `Remote` by shelling out to `git fetch`/`git push` in the common dir (credential helpers apply; `GIT_TERMINAL_PROMPT=0` in non-interactive mode), with import after fetch (objects into `cache/`) and export before push (`--force-with-lease`). `asc clone <git url> --reference <git-dir>` runs `git fetch <path>` first, with no alternates. Status and ahead/behind work. Scenario against a local bare repo standing in for Forgejo: clone, commit, push, a second clone fetches, and the ids are equal on both machines.

### U40 fsmonitor, doctor, reclaimable report
- **Owns:** `crates/asc/src/cli/{doctor.rs,fsmonitor.rs,gc.rs}`
- **Depends:** U39. **Accept:** `cargo test -p asc --test cli_doctor`, with `crates/asc/tests/cli_doctor.rs` owned
- **Brief:** `asc-fsmonitor` (argv[0]) implements hook protocol v2 (`2 <token>` → token NUL paths). In plain mode with no daemon it answers `/` (everything). Wiring to the daemon journal comes in U53. `asc doctor` checks: ProjFS feature state (Windows), FUSE availability, daemon reachability, `$ASC_HOME` free space, `LongPathsEnabled`, Defender exclusions, `core.hooksPath`/`fsmonitor` worktree overrides, global `autocrlf`, git version ≥ 2.40, credentials present, remote reachability, and `--store <url>` running the U30 probe. `--report-reclaimable <path>…` reports sizes of the old `.git`, `worktrees/`, `modules/`, `lfs/` and cruft packs **without deleting anything**. `asc gc` evicts the cache. Tests: the reclaimable report on a fixture repo with worktrees; fsmonitor protocol framing.

---

## Phase 4: server

### U41 Proto and conversions (`asc-rpc` codegen)
- **Owns:** `proto/asc/v1/repo.proto`, `proto/asc/v1/workspace.proto`, `crates/asc-rpc/build.rs`, `crates/asc-rpc/src/{lib.rs,convert.rs,errors.rs}`
- **Depends:** U04. **Accept:** `cargo test -p asc-rpc convert:: && buf lint proto` (buf optional locally; CI requires it)
- **Brief:** The proto files exactly as SPEC §7. `tonic-prost-build` codegen in `build.rs`. `convert.rs` provides `TryFrom` between prost types and `asc-core`/`asc-store` types (ids by length and mode, `RefTarget`, `Signature`, `PathChange`, `ObjKey`). Prost types never leave this crate's public API except through the generated service traits. `errors.rs` maps `ErrorDetail` ↔ a Rust `RpcError` enum. Tests: round trip of every message type that has an asc-core equivalent; bad lengths are rejected.

### U42 gRPC server/client, auth interceptor (`asc-rpc::{server,client,auth}`)
- **Owns:** `crates/asc-rpc/src/{server.rs,client.rs,auth.rs,history.rs}`
- **Depends:** U41, U31, U26, U28. **Accept:** `cargo test -p asc-rpc server:: client::`
- **Brief:** `HistoryApi` trait (log, merge_base, is_ancestor, diff, blame, resolve, git_map) implemented by `Engine` (in `asc-engine` through a blanket adapter in this crate) and by `RpcClient`. `RepoService<B: RepoBackend>`: a tonic server implementing object, ref and history RPCs over a `RepoBackend` trait (the server crate implements that trait for the real pipeline; this unit ships a simple `EngineBackend` for tests and local use). `RpcClient` implements `Remote` (U31) and `HistoryApi`: 8 pipelined `GetObjects` streams, batching, resumable `PutObjects`, `x-asc-op-id`, version header, TLS via rustls. `AuthInterceptor` extracts the bearer token into request extensions; an `Authorizer` trait is supplied by the server. Tests: the U07/U09 conformance suites run through an in-process server and client; a history equality check (engine vs RPC) on fixture repos.

### U43 Server core (`asc-serve::{config,registry,ingest,update,outbox,http}`)
- **Owns:** `crates/asc-serve/src/{lib.rs,config.rs,registry.rs,ingest.rs,update.rs,outbox.rs,http.rs,graph_sql.rs}`, `crates/asc-serve/migrations/{postgres,sqlite}/**`, `crates/asc/src/cli/serve.rs`
- **Depends:** U42, U34. **Accept:** `cargo test -p asc-serve update:: ingest:: outbox::`
- **Brief:** The `asc-serve.toml` loader (SPEC §8.1). The SPEC §8.2 schema with migrations for both databases. A registry with per-repo `Engine` construction (`BucketObjects` namespace + `SqlRefs` + SQL `GraphSource` + SQL `GitMapSink`). Ingest verification (SPEC §8.4) with `file_meta`. The `UpdateRefs` pipeline steps 1–5, 8 and 9 (steps 6–7 are hook points with a no-op default; Forgejo arrives in U44). FAST_FORWARD and FORCE modes; `op_results` idempotency; outbox table and worker (per-repo ordering, backoff, metrics) with an `Effect` trait (implementations added by U44 and K-units). axum routes `/healthz`, `/metrics` (prometheus) and `/.well-known/asc`. The `asc serve` subcommand. Tests: in-process server over InMemory + SQLite; push of a stack; a duplicate `op_id` returns the stored result; **crash injection** (panic after commit, before effects) followed by restart delivers the effects; a client lying about a `git` oid or size is rejected with `INVALID_OBJECT`.

### U44 Forgejo client: authz and hooks (`asc-serve::forgejo`)
- **Owns:** `crates/asc-serve/src/forgejo.rs`, `crates/asc-serve/tests/fake_forgejo.rs`
- **Depends:** U43. **Accept:** `cargo test -p asc-serve forgejo:: --test fake_forgejo`
- **Brief:** `Authorizer` implementations: `PublicApiAuthz` (Stage 1: `GET /api/v1/repos/{o}/{r}` with the user's token, reading `permissions`) and `InternalAuthz` (Stage 2+: `POST /api/internal/asc/authz`, response as in K07). Cache: 60 s positive / 5 s negative, keyed by `sha256(token)`. `HookClient::pre_receive(HookOptions)`, `post_receive`, `proc_receive` against Forgejo's internal API with `INTERNAL_TOKEN`; `HookOptions` JSON matches `modules/private/hook.go` field names exactly. Effects: `PostReceive`. Pipeline step 7 wired in. The `fake_forgejo` test server (axum) records calls and can reject. Tests: a pre-receive rejection propagates the message as `HOOK_REJECTED`; `SKIP_PRE` without the internal token is refused; post-receive is delivered exactly once even after a forced retry (idempotent by op_id on the fake).

### U45 Mirror mode (`asc-serve::mirror`)
- **Owns:** `crates/asc-serve/src/mirror.rs`
- **Depends:** U44, U33. **Accept:** `cargo test -p asc-serve mirror::`
- **Brief:** SPEC §8.6: the webhook handler (HMAC verification), a poller, `git fetch` into the mirror dir, incremental import, ref sync in one transaction (`via = mirror`), the LFS copy job (Forgejo LFS batch API download → content + alias, resumable via a `mirror_lfs` progress table added in this unit's migration), `mirror_state`. `UpdateRefs` returns `MIRROR_MODE` for mirror repos. Tests: a local bare repo as upstream; a push to it plus a webhook → asc refs equal git refs within one call; duplicate webhooks are harmless; ids equal those of a client-side import (U39) of the same history.

### U46 Land (`asc-serve::land`)
- **Owns:** `crates/asc-serve/src/land.rs`
- **Depends:** U44. **Accept:** `cargo test -p asc-serve land::`
- **Brief:** SPEC §8.5 inside the pipeline: stack discovery, disjointness via U26, the signed-commit refusal (`branch_rules.require_signed` from authz), metadata rebase, ingest of rewritten changesets, pre-receive on the rewritten ids, CAS retry (≤ 20), `rewritten` in the response, and `op_results` in the same transaction. Tests: 20 concurrent lands of disjoint changes all succeed and produce a linear history; an overlapping pair → one `LAND_CONFLICT` naming the paths; a file-vs-directory prefix conflict and a case-fold conflict are detected; a retried land with the same `op_id` returns the original mapping and does not rebase again.

### U47 Decoded read RPCs (`asc-serve::read`)
- **Owns:** `crates/asc-serve/src/read.rs`, `crates/asc-serve/src/idmode.rs`
- **Depends:** U43. **Accept:** `cargo test -p asc-serve read:: idmode::`
- **Brief:** `ReadChangeset`, `ReadTree` (paged), `ReadFile` (ranges, LFS resolution), `Stat`, `LastCommits` (a first-parent walk over `changes`, memoised per (tip, dir) in a bounded LRU), `CheckAttr` (gitattributes via `gix-attributes`), `Grep` (`regex`, streaming over the tree, skipping binaries and files > 1 MiB), `Archive` (tar and zip via the `tar`/`zip` crates; prefix; pathspec), `Diff` with `merge_base`. `idmode.rs` translates every id field to and from git oids through `git_map` when `id_mode = GIT`. Tests: each RPC against fixtures; in `GIT` mode, results equal `git` CLI outputs (`ls-tree`, `cat-file`, `diff --name-status`, `log --format=%H`).

### U48 Mutations and admin (`asc-serve::{mutations,admin}`)
- **Owns:** `crates/asc-serve/src/{mutations.rs,admin.rs,expire.rs}`
- **Depends:** U44, U47. **Accept:** `cargo test -p asc-serve mutations:: admin:: expire::`
- **Brief:** `Commit` (FileOps → changeset → pipeline), `Merge` (all styles and `dry_run`; protected-glob detection; cross-repo via `CopyClosure`), `Apply` (cherry-pick, revert, patch via a `git apply`-compatible parser in `asc-engine::diff`), `CopyClosure`, `RepoAdmin` (init, fork with a background copy job and a readiness flag, delete with a delayed namespace drop, rename, size, verify), `ImportGitPush` (reads the quarantine with gix, imports, `UpdateRefs(SKIP_PRE)`), and the expiring-refs worker (SPEC §8.8). Tests: the merge styles produce the expected parents and trees; a dry run reports conflicts equal to `git merge-tree`; a fork is readable only after the copy completes; `ImportGitPush` with a stale old value → `REF_CONFLICT`; an expired agent ref is deleted with a post-receive effect.

### U49 Turbo /v8 cache (`asc-serve::turbo`)
- **Owns:** `crates/asc-serve/src/turbo.rs`, `crates/asc-serve/tests/turbo_it.rs`
- **Depends:** U43. **Accept:** `cargo test -p asc-serve turbo:: && cargo test -p asc-serve --features it-turbo --test turbo_it -- --ignored`
- **Brief:** SPEC §12.4: all endpoints, `.meta` storage, scopes and fallback reads, Stage 1 trusted-token rule, Stage 2 authz-driven scopes, presigned `OPTIONS` for S3, a retention sweep, events counted as metrics. `slug = <owner>.<repo>`. The integration test runs real `turbo` 2.10.11 (`bunx turbo@2.10.11`) against a fixture monorepo twice: the second run is a full remote hit, and a signature mismatch is rejected by turbo.

### U50 CLI server wiring
- **Owns:** `crates/asc/src/remote/{server.rs,discover.rs}`, `crates/asc/src/cli/push_land.rs`, `crates/asc/tests/cli_server.rs`
- **Depends:** U42, U39, U46. **Accept:** `cargo test -p asc --test cli_server`
- **Brief:** Discovery per SPEC §6.2 (`remote.<n>.ascServer`, `/.well-known/asc`, fallback to git-remote) and caching of the discovery result in `state.db` for 1 h. `ServerRemote` = `RpcClient` + credentials from `git credential fill`. Mirror-mode push uses the U39 git path. Primary-mode push uses `UpdateRefs` (`--land` and configured land branches → `LAND`; prints the rewritten mapping and moves local refs). `diff --remote`, and server-backed `log`/`blame` per `asc.serverHistory`. `asc.displayIds=auto` resolution. `asc auth login|status|logout`. `CLIENT_TOO_OLD` handling. Tests against an in-process server: clone, commit, push, fetch on a second client; land rewrite handling; `diff --remote` with no workspace.

### U51 Deploy bundle and smoke
- **Owns:** `deploy/{compose.yml,asc-serve.toml.example,Dockerfile,README.md}`, `crates/asc/tests/deploy_smoke.rs`
- **Depends:** U45–U50. **Accept:** `docker compose -f deploy/compose.yml up -d --wait && cargo test -p asc --test deploy_smoke -- --ignored`
- **Brief:** A multi-stage Dockerfile building `asc --no-default-features --features serve`; compose with Garage (single node, bucket bootstrap script), Postgres and asc serve. The smoke test: init a primary repo via admin, clone, push, land, `diff --remote`, turbo cache put/get, metrics scrape.

---

## Phase 5: daemon, workspaces, ProjFS

### U52 IPC transport and Workspace service (`asc-rpc::ipc`)
- **Owns:** `crates/asc-rpc/src/ipc.rs`
- **Depends:** U42. **Accept:** `cargo test -p asc-rpc ipc::`
- **Brief:** `interprocess` local sockets adapted to tonic (`Connected` stream plus a connector). Windows: pipe name `asc-<user SID>`, a security descriptor allowing only the user SID and SYSTEM (built with the `windows` crate). Unix: `$XDG_RUNTIME_DIR/asc/asc.sock` (or `$ASC_HOME/run`), 0700 dir, 0600 socket, stale-socket cleanup. Generated `Workspace` service traits are re-exported. Tests: a client from the same user connects; on Unix a different-uid connection is refused (run with `sudo -u nobody` only in CI); on Windows the DACL is asserted by reading it back.

### U53 Daemon (`asc::daemon`)
- **Owns:** `crates/asc/src/daemon/{mod.rs,lifecycle.rs,service.rs,scheduler.rs,install.rs}`, `crates/asc/src/cli/daemon.rs`
- **Depends:** U52, U54, U55. **Accept:** `cargo test -p asc daemon::`
- **Brief:** `asc daemon start|stop|status|run`. Autostart from the CLI (spawn detached, wait for `Ping` ≤ 3 s). Single instance via the pipe/socket name. Version-skew restart (SPEC §9.1). `Workspace` service implementation dispatching to mounted providers (plain mode uses U18; virtual uses the U55 provider-core trait with OS backends from U56/U59 behind features). Scheduler: hourly cache GC, idle dehydration hook, ephemeral worktree expiry, pending-path retries every 5 s, reconcile at start, notify watches for the reconcile triggers (U37), and fsmonitor answers from the journal (wires U40's `asc-fsmonitor`). `install.rs`: register/unregister the Windows logon task (Task Scheduler COM via the `windows` crate, restart on failure ×3 at 10 s) and the systemd user unit. Tests: the start/stop/restart cycle; kill -9 and auto-restart by the CLI; a version mismatch triggers restart; journal-backed fsmonitor answers.

### U54 Journal, virtual status, switch intent, worktrees (`asc-wc::{journal,vstatus,intent,worktree}`)
- **Owns:** `crates/asc-wc/src/{journal.rs,vstatus.rs,intent.rs,worktree.rs,times.rs}`, `crates/asc/src/cli/worktree.rs`
- **Depends:** U25. **Accept:** `cargo test -p asc-wc journal:: vstatus:: intent:: worktree:: times::`
- **Brief:** `Journal` with an in-memory queue, 100 ms batch flush to `state.db`, sequence tokens (`<epoch>:<seq>`), `since(token) -> (paths, fresh)`, `clear(paths)`. `vstatus::compute(ws, disk: &dyn DiskState)`: the dirty candidates (journal ∪ staged), the `DiskState` trait (`state(path) -> Virtual|Placeholder|Hydrated|Full|Tombstone|Missing`, `stat`, `read`) implemented by the OS providers, dir-rename expansion (SPEC §10.2.6), and untracked only in materialised dirs. `times.rs`: SPEC §10.1 (`epoch_time`, `next_time`, `entry_times` lookups, compaction). `intent.rs`: persist, replay and clear `switch_intent`. `worktree.rs` and `asc worktree add|list|remove|prune|lock|unlock|move` with `--ephemeral --ttl` (plain mode now; virtual through the daemon later). Tests with a fake `DiskState`: status for every on-disk state; dir-rename expansion; times strictly increase across switches; intent replay is idempotent; `worktree remove` refuses when dirty.

### U55 VFS provider core (`asc-vfs::core`)
- **Owns:** `crates/asc-vfs/src/{lib.rs,core.rs,policy.rs,stats.rs}`
- **Depends:** U54. **Accept:** `cargo test -p asc-vfs core::`
- **Brief:** The `Provider` trait the OS backends implement (`mount`, `unmount`, `apply_switch(plan, times) -> SwitchOutcome{pending}`, `dehydrate(paths)`, `disk_state()`). `CoreFs`, shared logic: path → entry resolution against the base tree with a `quick_cache` of trees; case-insensitive lookup mode (Windows); the unrepresentable-name filter (SPEC §2.8); `read_content(ContentRef) -> ContentStream` (`cache/f` path or fetch through `Tiered`, LFS via alias, chunk streaming with verification); a gitlink composition hook (filled by U60); `HydrationPolicy` (deny list by process image); `Stats` counters per path and process. An in-memory OS harness implementing `Provider` for tests. Tests: lookup, unrepresentable filtering, the deny list, stats.

### U56 ProjFS binding and callbacks (`asc-vfs::projfs`)
- **Owns:** `crates/asc-vfs/src/projfs/{mod.rs,ffi.rs,provider.rs,enumerate.rs,data.rs,notify.rs}`
- **Depends:** U55. **Accept:** `cargo test -p asc-vfs --features projfs projfs::basic -- --ignored` (Windows with Client-ProjFS enabled)
- **Brief:** Runtime loading of `ProjectedFSLib.dll` (`libloading`; every function pointer is resolved at start; a missing DLL → `ProviderUnavailable`). `PrjMarkDirectoryAsPlaceholder` for the root with the workspace GUID, then `PrjStartVirtualizing` with callbacks and notification mappings (redirections and `.git` masked). Callbacks per SPEC §10.2 items 3–6: enumeration sessions keyed by GUID, `PrjFileNameMatch`/`PrjFileNameCompare`, placeholder info with times from U54 `times`, `GetFileData` reading the placeholder's ContentID with aligned buffers, async completion (`ERROR_IO_PENDING` + `PrjCompleteCommand`, tracked per command id, cancellation), notifications appending to the journal, and the hydration policy. No locks are held across ProjFS calls. Tests (ignored unless ProjFS is available): mount a 1k-file tree; `dir` enumerates without hydrating; reading one file hydrates exactly one; `stat` times equal the epoch time before and after hydration; a write journals the path; a denied process gets access-denied.

### U57 ProjFS switch, tombstones, reconcile, dehydration (`asc-vfs::projfs`)
- **Owns:** `crates/asc-vfs/src/projfs/{switch.rs,reconcile.rs,dehydrate.rs,rename.rs}`
- **Depends:** U56, U53. **Accept:** `cargo test -p asc-vfs --features projfs projfs::lifecycle -- --ignored`
- **Brief:** SPEC §10.2 items 6–9: directory-rename pre-hydration on `PRE_RENAME`; the switch algorithm with intent, `PrjUpdateFileIfNeeded`/`PrjDeleteFile` flags, tombstone handling (`PRJ_UPDATE_ALLOW_TOMBSTONE`), held-open retry then pending, `PrjClearNegativePathCache`, epoch times; reconcile at start via an on-disk walk plus `PrjGetOnDiskFileState`; dehydration (idle, budget, on demand) for clean hydrated placeholders only. Tests: switch with a file held open by another process (spawn a child that holds the handle) → pending, then cleared after release; delete a placeholder, then `reset --hard` restores it; kill the daemon mid-switch, restart → consistent; rename a directory with virtual children → status shows deletes plus untracked correctly; dehydration returns hydrated bytes to 0 for clean files and never touches dirty ones.

### U58 Windows virtual end-to-end
- **Owns:** `crates/asc/tests/windows_virtual.rs`, `crates/asc/tests/scenarios/virtual-*.t`
- **Depends:** U57, U40. **Accept:** `cargo test -p asc --test windows_virtual -- --ignored` (Windows with ProjFS)
- **Brief:** `asc clone` (virtual) from a local `asc serve` and from a `file://` bucket; `worktree add` < 1 s; `status` clean < 50 ms; edit, add and commit; switch between branches with 0 downloads (download counter); `git status`/`git log`/`git diff` in the projection; IDE-style `git commit` imported; `asc worktree remove`. The virtual scenario files re-run selected U25 scenarios in virtual mode and require identical output.

---

## Phase 6–7: FUSE, submodules, turbo and CI

### U59 FUSE provider (`asc-vfs::fuse`)
- **Owns:** `crates/asc-vfs/src/fuse/{mod.rs,fs.rs,inodes.rs,overlay.rs,passthrough.rs}`
- **Depends:** U55. **Accept:** `cargo test -p asc-vfs --features fuse fuse:: -- --ignored` (Linux/WSL with `/dev/fuse`)
- **Brief:** SPEC §10.3: `fuser` filesystem; persisted inode table (`state.db.inodes`); `getattr` with epoch times; `readdirplus`; passthrough via `BackingId` for clean files when supported; overlay copy-up for writes, creates, unlinks and renames (exact journal); redirection symlinks; switch via entry and inode invalidation; `DiskState` from the overlay. Tests: inode stability across remount; passthrough read with zero overlay bytes; overlay exactness; switch invalidation visible to `stat`.

### U60 Submodule read-only composition (`asc-vfs::compose`, plain support)
- **Owns:** `crates/asc-vfs/src/compose.rs`, `crates/asc-wc/src/compose_plain.rs`, `crates/asc/src/cli/submodule.rs`
- **Depends:** U55, U18. **Accept:** `cargo test -p asc-vfs compose:: && cargo test -p asc-wc compose_plain::`
- **Brief:** SPEC §10.6: resolve `asc.submodule.<path>.repo` (default from `.gitmodules`); map the gitlink commit → nested changeset via the nested repo's `git_map` (fetching from its remote if needed); project its root tree read-only (writes are denied); plain mode materialises read-only files. `asc submodule status|set-pin|update(no-op)`. Tests: a fixture parent and child repo; a composed read; a denied write; a pin change via `set-pin` produces a `Change::Gitlink`.

### U61 Turbo contract suite
- **Owns:** `tests/turbo/{Cargo.toml,src/lib.rs,tests/contract.rs,fixture/**}`
- **Depends:** U40 (plain), U58/U59 (virtual). **Accept:** `cargo test -p asc-turbo-contract -- --ignored` (needs bun and network for `bunx turbo@2.10.11`)
- **Brief:** SPEC §17 turbo row: a fixture monorepo with 20 packages, a `bun.lock` and `turbo.json`; compare `turbo run build --dry=json` task hashes between a real git clone and an asc workspace (plain always, virtual where available) with hydration counters; a one-file edit; `turbo ls --affected` equality with `TURBO_SCM_BASE`; a racy run (immediately after checkout) showing 0 verified entries (turbo debug log); worktrees share `$ASC_HOME/repos/<id>/.turbo/cache` (spike S4).

### U62 asc ci checkout (`asc-ci::checkout`)
- **Owns:** `crates/asc-ci/src/{lib.rs,checkout.rs,env.rs}`, `crates/asc/src/cli/ci.rs`
- **Depends:** U50. **Accept:** `cargo test -p asc-ci checkout::`
- **Brief:** SPEC §12.3 steps 1–5: `HostCacheRO` (a `CacheDir` opened read-only: writes go to the next tier) layered over `JobCache` and the remote; materialise (plain, sparse optional); projection with `shallow` at the merge base, base refs, commits merge-base..head, base and head trees, seeded manifests at the base; env exports written to `$GITHUB_ENV`; task-token auth; trusted vs PR cache mode decided from the event. Tests: a read-only host cache is never written (permissions test); a corrupted host cache file is detected and re-fetched; `git diff-tree --merge-base` inside the checkout works within the shallow boundary.

### U63 asc ci changed/affected/warm (`asc-ci::{changed,affected,warm,rules,tools}`)
- **Owns:** `crates/asc-ci/src/{changed.rs,affected.rs,warm.rs,rules.rs,tools.rs}`
- **Depends:** U50. **Accept:** `cargo test -p asc-ci changed:: affected:: warm:: rules::`
- **Brief:** `.asc/ci.toml` schema (`turbo`, optional `turbo_sha512`, `[switches]` globs, `[packages]` mapping rules, `[outputs]` names). `changed`: one `Diff(NAMES, merge_base)` → switches. `affected`: manifest-only sparse checkout in a temp dir → `tools::turbo(version)` (download the npm tarball for the platform, verify integrity from `bun.lock` or `ci.toml`, cache in `$ASC_HOME/tools`) → `turbo ls --affected --output=json` → mapping → outputs; on failure, "all" plus a warning. `warm`: fetch trunk changesets, trees and files into the host cache (a systemd timer unit file template is included in `deploy/`). Tests: rules evaluation; the turbo download verification rejects a tampered tarball; the affected fallback path.

### U64 Actions and ASC workflows
- **Owns:** `actions/checkout/action.yml`, `actions/affected/action.yml`, `actions/changed/action.yml`, `actions/test/selftest.sh`, `deploy/runner/{asc-warm.service,asc-warm.timer,runner-config-snippet.yaml}`, `docs/ci/asc-workflows.patch` (proposed changes to `D:\dev\monorepo\.forgejo\workflows\*`, applied by ASC as a PR, not by this unit)
- **Depends:** U62, U63, U49. **Accept:** `bash actions/test/selftest.sh` (starts a local asc serve, runs the actions' commands in a container with a read-only host cache mount, asserts the projection, `TURBO_SCM_BASE` and outputs)
- **Brief:** Composite actions that install `asc` (cached by version) and call `asc ci …`. Runner config for the host cache read-only bind mount and the warm timer. The workflow patch: a plan job (`changed` + `affected`), verify/rust/images/e2e jobs gated on outputs, no `fetch-depth: 0`, no `submodules: recursive`, `TURBO_API` to asc serve.

---

## §K Forgejo fork track (Go, `D:\dev\forgejo`, branch `asc/v16`)

Each unit is one commit on the patch stack (SPEC §13.3). Commands run from `D:\dev\forgejo`. A Go engineer owns this track. It needs Rust units only where noted (as an `ASC_SERVER_BIN` built from this repo).

| ID | Title | Owns | Depends | Accept |
|---|---|---|---|---|
| K01 | Baseline and CI | `.forgejo/workflows/asc-fork.yml`, `docs/asc/BASELINE.md` (recorded passing tests) | – | `go build ./... && make test-backend` |
| K02 | Path hiding | the 34 `git.OpenRepository(path)` callers, `modules/gitrepo/**`, `modules/git/{blame,diff,log_name_status}.go` signatures | K01 | `make test-backend test-sqlite` |
| K03 | Seam A `Backend` + `gitcli` | `modules/git/backend.go`, `modules/git/gitcli/**`, `modules/git/*.go` dispatch | K02 | `go test ./modules/git/... && make test-sqlite` |
| K04 | Seam B `repostore` + gitcli writer | `modules/repostore/**`, temp-repo call sites in `services/{repository/files,pull,wiki,repository}` | K03 | `go test ./modules/repostore/... ./services/pull/... ./services/wiki/... && make test-sqlite` |
| K05 | Raw git removal + checker | `build/check-raw-git/**`, call sites in `modules/indexer/code`, `services/*` | K04 | `go run ./build/check-raw-git && make test-sqlite` |
| K06 | `ascproto` + `ascclient` | `modules/ascproto/**` (buf-generated, committed), `modules/ascclient/**`, `buf.gen.yaml` | U41 | `go test ./modules/ascclient/...` (with `ASC_SERVER_BIN` set: in-process server tests) |
| K07 | Internal authz endpoint + well-known | `routers/private/asc.go`, route registration in `routers/private/internal.go`, `modules/private/asc.go`, `routers/web/wellknown_asc.go`, `modules/setting/asc.go` | K01 | `go test ./routers/private/... -run TestAscAuthz` |
| K08 | `storage_backend` column + selection | `models/repo/repo.go`, `models/forgejo_migrations/v*_asc_storage_backend.go`, `modules/gitrepo/backend_select.go` | K03, K06 | `go test ./models/... ./modules/gitrepo/...` |
| K09 | Stage 2 hook glue | `routers/private/hook_pre_receive.go` (asc branch), `routers/private/hook_post_receive.go` (`PushTrigger="asc"` handling), `services/ascsync/**` | K07, K08, U48 | `ASC_SERVER_BIN=… go test ./services/ascsync/... ./routers/private/...` |
| K10 | `ascbackend` reads | `modules/git/ascbackend/**` | K08, U47 | `ASC_SERVER_BIN=… go test ./modules/git/ascbackend/...` |
| K11 | `dual` shadow backend | `modules/git/dual/**` | K10 | `go test ./modules/git/dual/...` |
| K12 | `repostore` asc writer | `modules/repostore/asc/**` | K04, K10, U48 | `ASC_SERVER_BIN=… go test ./modules/repostore/asc/...` |
| K13 | Bridge, LFS, lifecycle | `services/ascbridge/**`, `routers/web/repo/githttp.go`, `cmd/serv.go`, `services/lfs/**`, `services/mirror/**`, `services/repository/{create,fork,delete,adopt,check}.go` | K12 | `ASC_SERVER_BIN=… go test ./services/ascbridge/... ./services/lfs/...` |
| K14 | Actions integration | `services/actions/**`, `modules/actions/**` | K10 | `go test ./modules/actions/... ./services/actions/...` |
| K15 | Integration matrix | `tests/integration/asc_backend_test.go`, CI matrix | K11, K13, K14 | `FORGEJO_TEST_STORAGE_BACKEND=asc make test-sqlite test-pgsql` and the same with `dual` (0 mismatches) |
| K16 | ID mode `asc` (optional, later) | `modules/git/object_format.go`, `models/**` id columns migration, `services/context/**` | K15 | `FORGEJO_TEST_OBJECT_FORMAT=asc make test-sqlite` |

**Briefs:**

- **K02–K05:** follow `docs/research/forgejo.md` §4.2 phases 1–4 exactly. No behaviour change. The full existing test suite must stay green at every commit.
- **K03:** `Backend` is the method set in `forgejo.md` §2 Seam A. The `gitcli` code moves mostly verbatim. Add `build/rebase-map.sh` (SPEC §13.3).
- **K06:** an `ascclient.Client` with a connection pool, bearer auth (the user token is forwarded for user-initiated calls; the internal token for system calls), retries on `UNAVAILABLE`, and an LRU of decoded objects keyed by immutable id (default 256 MiB).
- **K07:** the response schema is exactly SPEC §13.3 K07. It reuses the `/serv/command` permission logic. The Actions task token resolves the run and event (`is_fork_pr` from the run's PR). `branch_rules.require_signed` comes from the protected-branch settings.
- **K09:** pre-receive asc branch: after the existing checks, call `ImportGitPush` with `GIT_QUARANTINE_PATH`, `GIT_OBJECT_DIRECTORY` and `GIT_ALTERNATE_OBJECT_DIRECTORIES` from the hook env; on error, reject with asc's message. Post-receive: for `storage_backend=asc` repos, ignore calls whose `PushTrigger != "asc"`, because asc's outbox is the single fan-out. `HookOptions` gains nothing new; `PushTrigger` already exists.
- **K10:** map every `Backend` method to the RPCs in SPEC §13.3 K10 with `id_mode=GIT` while `object_format=sha1`. Batch `LastCommits` and `Stat`. Language stats are computed in Go over `ReadTree` plus `ReadFile`.
- **K11:** sampled comparison (`[asc] SHADOW_SAMPLE=0.1`) of `GetCommit`, `GetTree`, `ListRefs`, `Log` pages, `GetDiff` name-status, `GetMergeBase`, blame and `Merge(dry_run)` vs `git merge-tree`. Metrics plus logs; the response always comes from gitcli.
- **K12:** each `Writer` method = one RPC. PR head refs use `CopyClosure` then `UpdateRefs refs/pull/<n>/head`. The AGit flow maps to `UpdateRefs refs/for/<b>`, after which asc calls `proc-receive`.
- **K13:** `bridgePath(repo)` everywhere a repo path served git. A lazy bridge build calls `asc export --git <bridge>` (the `asc` binary on the host) on the first legacy request (a request-scoped lock). The LFS batch API answers from asc via the `x/sha256` alias with presigned URLs. Fork, delete and size use `RepoAdmin`.
- **K14:** `paths:` matching uses `Diff(NAMES)` between before and after (push) or merge base and head (PR). The context gains the SPEC fields.

---

## Ops units

### O01 GC (two-mark)
- **Owns:** `crates/asc-store/src/gc.rs`, `crates/asc-serve/src/gc.rs`, `crates/asc/src/cli/gc_store.rs`
- **Depends:** U43. **Accept:** `cargo test -p asc-store gc:: && cargo test -p asc-serve gc::`
- **Brief:** SPEC §3.6: marks from refs plus recent changesets, the two-mark and 14-day rule, the server's `gc_marks` table, bucket marks under `gc/<run>/` with a `gc/lock` lease, and dry-run output. Tests: an in-flight push across marks keeps re-referenced old objects; unreachable old objects are deleted only after the second mark; concurrent GC runs are refused.

### O02 Release and distribution
- **Owns:** `.forgejo/workflows/release.yml`, `packaging/windows/{main.wxs,register-task.ps1,defender.ps1}`, `packaging/winget/**`, `packaging/linux/{install.sh,asc-daemon.service}`, `crates/asc/src/cli/upgrade.rs`
- **Depends:** U53. **Accept:** `cargo test -p asc upgrade:: && cargo wix --nocapture --package asc` (on Windows CI)
- **Brief:** SPEC §16.2: release builds for 4 targets; the MSI (installs, hardlinks the helper names, enables Client-ProjFS, adds Defender exclusions, registers the logon task); a winget manifest; the Linux installer and user unit; `asc upgrade` with minisign verification; `Version.min_client` enforcement messages. Test: upgrade refuses a tampered artefact.

### O03 Migration runbook and admin verify
- **Owns:** `docs/runbooks/{asc-cutover.md,rollback.md,restore.md}`, `crates/asc-serve/src/admin_cli.rs`
- **Depends:** U45, K09. **Accept:** `cargo test -p asc-serve admin_cli::` and, on staging, `asc serve admin verify-import --repo asc/dds --repo asc/soma` exits 0 with `mismatches: 0`
- **Brief:** SPEC §14.3 as an executable runbook with exact commands; `asc serve admin verify-import` (every git commit's export equals git), `verify --all` (closure and git map), `outbox drain --wait`, and `set-mode`. The rollback and restore drills are scripted.

### O04 Developer docs and cheat sheet
- **Owns:** `docs/CHEATSHEET.md`, `docs/ONBOARDING.md`, `docs/agents/claude-code-worktrees.md`, `crates/asc/src/cli/help_git.rs`
- **Depends:** U40. **Accept:** `cargo test -p asc help_git::` (the cheat sheet embedded in `asc help git` matches `docs/CHEATSHEET.md`)
- **Brief:** The git → asc mapping (SPEC §11.3); onboarding (install, `clone --reference`, reclaim disk, VS Code settings for search and watcher excludes); Claude Code `WorktreeCreate`/`WorktreeRemove` hook configuration (outcome of spike S7); a troubleshooting section keyed by `asc doctor` findings.

### O05 Benchmarks (`asc-bench`)
- **Owns:** `crates/asc-bench/**`
- **Depends:** U58. **Accept:** `cargo run -p asc-bench --release -- --suite quick` (prints a table and fails on regression > 20% vs `crates/asc-bench/baseline.json`)
- **Brief:** The SPEC §1 targets: clone, worktree add, status (clean and 10 dirty), switch between trunk tips, commit, push, `diff --remote`, plan job, import throughput. Uses a synthetic 100k-file repo plus an optional ASC mirror.

---

## Parallel execution summary

| Wave | Units that can run concurrently |
|---|---|
| 1 | U01 |
| 2 | U02, U03 (+ K01, K02 on the Go track; U41 after U04) |
| 3 | U04 |
| 4 | U05, U06 |
| 5 | U07, U09 |
| 6 | U08 |
| 7 | U10 |
| 8 | U11 |
| 9 | U12, U14, U15, U16, U17 |
| 10 | U13, U18, U19 |
| 11 | U20 |
| 12 | U21, U22, U23, U24 |
| 13 | U25 |
| 14 | U26, U28, U29, U30, U31, U33, U34, U41, U54 |
| 15 | U27, U35, U55 |
| 16 | U32, U36, U42, U56, U59 |
| 17 | U37, U38, U43, U52, U60 |
| 18 | U39, U44, U47, U49, U53 |
| 19 | U40, U45, U46, U48, U57 |
| 20 | U50, U58, O01 |
| 21 | U51, U61, U62, U63, O02, O04 |
| 22 | U64, O03, O05 |

The Go track (K01–K16) runs alongside: K01–K05 from wave 2; K06 after U41; K07 and K08 next; K09 after U48; K10–K15 after U47 and U48.
