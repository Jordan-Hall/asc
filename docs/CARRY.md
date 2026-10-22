# Carry-forward obligations for later units

These are requirements that fixes in earlier units created for later units. Every build brief must read this file.

- **asc-engine ingest (U11 and every write path):** reject `git.commit` and `git.tag` extras on every ingest path except git import (`canonicalise_commit` / `canonicalise_tag`).
- **Callers of `export_commit` / `export_tag`** handle the `Result`. Raw bytes are verified against tree, parents, idents and message.
- **`Signature`** has private fields. Build it with `Signature::new`, which rejects LF, `<`, `>` and NUL.
- **Object decode** validates `Change`, `Changeset` and `ChunkList` invariants. Full apply/leafdiff validation stays in asc-engine ingest.
- **Paths:** `checkout_safe` rejects any backslash. `windows_valid` matches git's `is_valid_win32_path`.
- **`FileMeta` is dropped.** Envelope kind 5 is reserved.
- **No LFS anywhere:** no LFS fields, alias keys, pointer parsing, LFS API or LFS copy jobs. Pointer files are plain blobs.
