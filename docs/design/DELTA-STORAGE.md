# Delta storage

Status: built behind `[delta] enabled` (off by default). Reads resolve deltas whether or not the pass
runs, so a server can turn the pass off at any time and keep every stored delta readable.

## Why

asc stores every version of a file whole (codec `1`, one zstd frame). Most commits change a few lines
of a large file, so a long history stores near-copies of the same bytes. A measurement over 5,638
modified file pairs of a real history (each new version against the previous version of the same path)
gave a 95.7% saving when the new version is stored as a zstd frame compressed with the old version as a
dictionary, against storing it as its own zstd frame.

Non-goals: changing object identity, the wire format, verification or what a client sees. A delta is a
storage detail below `Objects`, like a pack.

## Stored value

Codec `2` is added to the stored value (`codec u8 ‖ data`, SPEC §3.1):

```
2 ‖ base FileId (32 bytes) ‖ zstd frame (content size set), compressed with the base's data as a raw dictionary
```

- Only the server writes codec `2`, and only to loose `File` objects (never packed, never chunked: files
  above the 4 MiB chunk threshold are left alone).
- A delta is written only when it is under half of the stored value it replaces.
- A chain (a delta whose base is a delta) has at most 8 links (`MAX_DELTA_DEPTH`). The pass refuses a
  base whose chain is already 8 deep, and refuses a base whose chain contains the file (a cycle).
- The wire never carries codec `2`. `BucketObjects` resolves a delta when it reads a loose value: it
  reads the base (from a pack first, then loose, recursively), decodes the frame within the decode cap,
  verifies the result against the key and returns it as a codec `0` or `1` value. `get`, `get_stored`,
  `get_range` and every caller above them see only full values. Wire `check_codec` still refuses `2`.
- A missing or damaged base reads as `CodecError::BadBase(base)`, which `put` treats as damage: a put of
  the same file (a re-push, a fetch from a peer) overwrites the delta with the full value.

## The pass

`asc-serve/src/delta_service.rs`. One pass per repository, every `[delta] interval`, under the
collection lock (`GcService`, the same PostgreSQL advisory lock as GC), so a collection never runs
while a delta is being written.

1. For each land branch, read `mainline` rows after the branch's `delta_cursor` (at most
   `max_changesets` per pass).
2. For each `Change::Write` of a changeset that is not a symlink and has
   `min_size ≤ size ≤ 4 MiB`, the base is the entry at the same path, or at `copy_from` when the change
   records a copy, in the first parent's tree. A base that is not a file, is above 4 MiB, equals the
   new file, or is listed in `gc_purge` is skipped.
3. `plan_delta` builds the delta without writing. When it saves half, the pass upserts the
   `delta_bases (ns, file, base)` row first and then overwrites the loose value.
4. When a mainline changeset is a merge, the pass walks back from its other parents and treats each
   changeset it meets the same way (each against its own first parent), stopping at changesets on the
   branch's mainline. One merge's walk reads at most `max_changesets` changesets. A version written on
   a side branch therefore gets a base on that branch even when the merge only adds the file.
5. The cursor moves to the last row read.

## Keeping bases alive

- **GC.** `DeltaRoots` (a `GcRootSource`) returns every recorded base as a strict root, and is
  re-queried in the fenced re-mark, so a base is never swept while a delta names it. When a delta file
  is swept, the sweep observer deletes its `delta_bases` row; the base is then an ordinary object and
  goes at a later collection when nothing else reaches it.
- **Purge.** Before a collection that is not a dry run, `release_purged` finds every delta whose base
  is in `gc_purge`, rewrites it as a full value and drops its row, so the purge can delete the base.
- **Packs.** The pack job reads raw loose values and skips deltas, so deltas stay loose. Bases may be
  packed; reads look in packs first.
- **Forks.** A fork copies raw objects into a new namespace that has no `delta_bases` rows. After each
  copied batch, `expand_copied` rewrites the copied deltas in full in the fork. The fork's own pass
  makes deltas again.
- **Storage migration and backup** copy raw values within the same namespace; `delta_bases` and
  `delta_cursor` are namespace tables (`NS_TABLES`) and are backed up and restored with the database.

## Configuration

```toml
[delta]
enabled = false        # run the pass; reads resolve deltas regardless
interval = "1h"        # 1m to 7d
min_size = "1KiB"      # smaller files are left alone; 64B to 4MiB
max_changesets = 5000  # per repository per pass; 1 to 1000000
```

## Operation

- `asc serve admin delta run --repo R | --all` runs one pass now, whether or not `enabled` is set, and
  prints mainline changesets read, side-branch changesets read, candidates, deltas written, files without gain, skipped files and stored bytes
  before and after.
- `asc serve admin delta status --repo R | --all` prints the number of deltas, the distinct bases they
  name and the mainline changesets not yet read.
- `asc serve admin delta expand --repo R | --all` rewrites every delta in full and drops its row, under
  the collection lock. The cursors stay, so a later pass does not redo old history.
- Rollback: set `enabled = false`; existing deltas stay readable. Then `delta expand --all` if the
  stored values must be whole again (for example before a downgrade to a release without codec `2`).

## Schema

Migration `0611_delta_bases` (PostgreSQL and SQLite):

- `delta_bases (ns, file, base, PRIMARY KEY (ns, file))` with index `delta_bases_by_base (ns, base)`.
- `delta_cursor (ns, ref, pos, PRIMARY KEY (ns, ref))`.

## Tests

- `asc-store/src/codec.rs`: encode and decode, unrelated bases give no delta, the decode cap.
- `asc-store/src/delta.rs`: reads return the full value, a missing base is damage that a put repairs,
  chains stop at cycles and at the depth limit, the pack job leaves deltas loose.
- `asc-serve/src/delta_tests.rs`: the pass (copies, cursor, size bounds, batch limit, merged side
  branches), GC keeps a base
  until its last delta is swept, a purged base expands its dependents and takes no new ones, the admin
  command and the rollback.
- `asc-serve/src/admin.rs`: a fork holds full values where the source holds deltas.
