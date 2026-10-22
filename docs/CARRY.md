# Carry-forward obligations for later units

These are requirements that fixes in earlier units created for later units. Every build brief must read this file.

- **asc-engine ingest (U11 and every write path):** reject `git.commit` and `git.tag` extras on every ingest path except git import (`canonicalise_commit` / `canonicalise_tag`).
- **Callers of `export_commit` / `export_tag`** handle the `Result`. Raw bytes are verified against tree, parents, idents and message.
- **`Signature`** has private fields. Build it with `Signature::new`, which rejects LF, `<`, `>` and NUL.
- **Object decode** validates `Change`, `Changeset` and `ChunkList` invariants. Full apply/leafdiff validation stays in asc-engine ingest.
- **Paths:** `checkout_safe` rejects any backslash. `windows_valid` matches git's `is_valid_win32_path`.
- **`FileMeta` is dropped.** Envelope kind 5 is reserved.
- **No LFS anywhere:** no LFS fields, alias keys, pointer parsing, LFS API or LFS copy jobs. Pointer files are plain blobs.
- **asc-store reconcile, after U08/U09:**
  - `CacheDir` and `Tiered` `put` must verify on write and repair corrupt values, so they pass the extended conformance suite.
  - Migrate the callers to `decode_stored_capped(stored, max)` with per-class caps, then delete the 256 MiB `decode_stored` wrapper.
- **`canonicalise_tag`** must reject `type` other than `commit` at import, not only at export.
- **`parse_tag` rejects unknown headers.** This is stricter than git. If an imported history tag has extra headers, keep it raw instead of failing the import.
- **Import round-trip invariant (asc-git import, U33):** after building each changeset from a raw git commit or tag, call `export_commit`/`export_tag`. Store it only if the result equals the original raw bytes (same SHA-1). Otherwise fail the import of that object. An `asc` header whose copies do not map to `Change::Write` entries is rejected this way, so nothing unexportable is ever stored.
- **`ObjKey::ChangeMeta` (`m/`) and envelope kind 6 are reserved (SPEC §2.2, §3.1, §20.2).** `OBJ_KIND_CHANGE_META = 9` is reserved in `repo.proto` (U41). C01 adds them to the built `asc-core`/`asc-store` enums; no other unit may use kind 6, key prefix `m/` or ObjKind 9.
- **Graph boundary (U31):** the built `GraphSource::get` has no `Boundary` result. U31 owns adding `GraphSource::get -> Result<NodeOrBoundary>` (a `Boundary` variant for ids recorded in `history_boundary`) to `crates/asc-engine/src/graph.rs`. `Graph::merge_base`/`is_ancestor`/`log` hand a boundary to an optional `RemoteHistory` hook, which calls `MergeBase`/`IsAncestor`/`Log` with the boundary ids.
- **Skip pointers (U14b):** `JumpIndex::jump` is synchronous, so `graph_skip.rs` loads `skip.v1` for each node when `GraphSource` loads it (a wrapping `GraphSource`) and answers `jump` from memory.
- **Raw git in tests (U01b, U15):** the test-only `Command::new("git")` lines in asc-engine carry `// lint:allow-raw-git` (SPEC §11.4); asc-engine cannot use `RealGit` (crate cycle).
- **Local ref moves (every CLI unit):** go through `Ctx::update_local_refs` only (SPEC §9.4.1); the synthetic index follows set 0, not only HEAD (SPEC §9.4).
- **Read-only cache tier (U20, recorded amendment to built U08) [audit: object-stack-selector-single-owner-blocks-remote-tier]:** add `CacheDir::open_read_only(root)` in `crates/asc-store/src/cachedir.rs`. Its `put` returns `Ok` without writing, and a corrupt read returns `Error::Corrupt` without deleting the file (SPEC §3.5).
- **Ref lint (U20, recorded amendment to built U01b) [audit: engine-local-ref-moves-bypass-chokepoint]:** in `build/lint/no-raw-git.sh`, `check_refs_update crates/asc/src/cli` becomes `crates/asc/src`, excluding `crates/asc/src/refs_guard.rs`. Engine code never moves a local ref (SPEC §9.4.1).
- **S3 credentials (U32, recorded amendment to built U08) [audit: store-must-not-spawn-git]:** `asc-store` no longer runs `git credential fill`; `s3::open_with` and `Bucket::from_url_with` take optional `S3Credentials`. U32: S3 remotes get credentials from asc_git::credential::fill only when asc.s3.credentialHelper=true; libraries never spawn git.
- asc-graph: `canon::Value` derives plain `Deserialize`. Anything decoding untrusted bytes into a `Value` must go through `model::Parsed` (depth-bounded seed), never `postcard::from_bytes::<Value>` directly.
