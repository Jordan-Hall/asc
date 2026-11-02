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
- **`GitTreeOverride` value** is the git tree object body without the `tree <len>\0` header, verified by `object_oid(Tree, body) == oid`. SPEC §3.3 must say so.
- **`canonicalise_tag`** must reject `type` other than `commit` at import, not only at export.
- **`parse_tag` rejects unknown headers.** This is stricter than git. If an imported history tag has extra headers, keep it raw instead of failing the import.
- **Import round-trip invariant (asc-git import, U33):** after building each changeset from a raw git commit or tag, call `export_commit`/`export_tag`. Store it only if the result equals the original raw bytes (same SHA-1). Otherwise fail the import of that object. An `asc` header whose copies do not map to `Change::Write` entries is rejected this way, so nothing unexportable is ever stored.
