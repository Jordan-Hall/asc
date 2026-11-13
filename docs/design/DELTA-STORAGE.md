# Delta storage

Status: built behind `[delta] enabled` (off by default). Reads resolve deltas whether or not the pass
runs, so a server can turn the pass off at any time and keep every stored delta readable.

## Why

asc stores every version of a file whole (codec `1`, one zstd frame). Most commits change a few lines
of a large file, so a long history stores near-copies of the same bytes. A measurement over 5,638
modified file pairs of a real history (each new version against the previous version of the same path)
gave a 95.7% saving when the new version is stored as a zstd frame compressed with the old version as a
dictionary, against storing it as its own zstd frame.

Directory trees and large-file chunks repeat the same way: a commit that edits one file rewrites
every tree on the path to it, each a near-copy of the old tree, and an edit to a large file changes a
few of its chunks. On one public history imported for the benchmark, git's packs hold all 26,512
trees in 2.5 MB and the large-file versions in 50 MB, where whole values took 28 MB and 130 MB.

Non-goals: changing object identity, the wire format, verification or what a client sees. A delta is a
storage detail below `Objects`, like a pack.

## Stored value

Codec `2` is added to the stored value (`codec u8 ‖ data`, SPEC §3.1):

```
2 ‖ base id (32 bytes) ‖ zstd frame (content size set), compressed with the base's data as a raw dictionary
```

- Only the server writes codec `2`, and only to loose `File`, `Tree` and `Chunk` objects (never packed).
  The base is always an object of the delta's own kind: a file's base is a file, a tree's a tree and a
  chunk's a chunk. The id in the value is read under the delta's kind, so it can never name an object
  of another kind. A chunk list, a changeset or any other kind is never a delta.
- A delta is written only when it is under half of the stored value it replaces.
- A chain (a delta whose base is a delta) has at most 8 links (`MAX_DELTA_DEPTH`). The pass refuses a
  base whose chain is already 8 deep, and refuses a base whose chain contains the file (a cycle).
- The wire never carries codec `2`. `BucketObjects` resolves a delta when it reads a loose value: it
  reads the base (from a pack first, then loose, recursively), decodes the frame within the decode cap,
  verifies the result against the key and returns it as a codec `0` or `1` value. `get`, `get_stored`,
  `get_range` and every caller above them see only full values. Wire `check_codec` still refuses `2`.
- A missing or damaged base reads as `CodecError::BadBase(base key)`, which `put` treats as damage: a put of
  the same file (a re-push, a fetch from a peer) overwrites the delta with the full value.

## The pass

`asc-serve/src/delta_service.rs`. One pass per repository, every `[delta] interval`, under the
collection lock (`GcService`, the same PostgreSQL advisory lock as GC), so a collection never runs
while a delta is being written.

1. For each land branch, read `mainline` rows after the branch's `delta_cursor` (at most
   `max_changesets` per pass).
2. For each `Change::Write` of a changeset that is not a symlink and has `size ≥ min_size`, the
   candidate bases are the entries at the same path, or at `copy_from` when the change records a copy,
   in every parent's tree (a merge has several). A file equal to one of them needs nothing. A
   candidate that is not a file or is listed in `gc_purge` is skipped.
   - When the new version is at most 4 MiB, every candidate of at most 4 MiB is planned and the
     smallest delta is kept, as git's window search does.
   - A new file (no candidate in any parent) is planned against up to four files (`MAX_SIBLINGS`) of
     the first parent's same directory with the same extension and a size between half and double
     its own, nearest sizes first.
   - When both are above 4 MiB (chunked), the pass reads both chunk lists. A chunk both lists share
     needs nothing. Each other new chunk takes as its base the next old chunk that the new version
     dropped, counting from the last chunk both share: content-defined boundaries line up again after
     an edit, so the chunk an edit replaced is the one at the same place.
   - When one is chunked and the other is not, nothing is paired.
3. Trees: when the changeset's root differs from the parent's, the new root takes the old root as its
   base. Every directory above a changed path takes the tree the same directory had in the parent,
   when both are directories and differ.
4. `plan_best` builds the deltas without writing. When the best saves half, the pass upserts the
   `delta_bases (ns, file, base, kind)` row first, checks that the base still exists (a purge between
   the plan and the row would otherwise strand the delta; the row is deleted and the file stays full),
   and then overwrites the loose value. The chain rule bounds read cost: a root tree that changes in
   every commit is stored whole every ninth commit.
   - One `PlanCache` serves the whole pass: decoded values (verified against their keys, 64 MiB,
     oldest dropped first) and each key's delta link. Chains are rebuilt from the links on every
     plan, so a value that became a delta later in the pass lengthens every chain through it, and a
     link that the stored value contradicts makes the chain unreadable (the base is refused) rather
     than wrong.
5. When a mainline changeset is a merge, the pass walks back from its other parents and treats each
   changeset it meets the same way (each against its own first parent), stopping at changesets on the
   branch's mainline. Side changesets count against the same `max_changesets` budget as mainline ones
   and are read once per pass. When the budget runs out inside a walk, the cursor stays before that
   merge and the next pass starts with it; a merge that is the first changeset of a pass is read as
   far as the whole budget goes and then passed, so one huge merge cannot stall the pass. A version
   written on a side branch therefore gets a base on that branch even when the merge only adds the
   file.
6. The cursor moves to the last row read.

## Keeping bases alive

- **GC.** `DeltaRoots` (a `GcRootSource`) returns every recorded base as a strict root, and is
  re-queried in the fenced re-mark, so a base is never swept while a delta names it. When a delta file,
  tree or chunk is swept, the sweep observer deletes its `delta_bases` row; the base is then an ordinary object and
  goes at a later collection when nothing else reaches it.
- **Purge.** Before a collection that is not a dry run, `release_purged` finds every delta (of any
  kind) whose base is in `gc_purge`, rewrites it as a full value and drops its row, so the purge can delete the base.
- **Packs.** The pack job reads raw loose values and skips deltas, so deltas stay loose. Bases may be
  packed; reads look in packs first.
- **Forks.** A fork copies raw objects into a new namespace that has no `delta_bases` rows. After each
  copied batch, `expand_copied` rewrites the copied deltas in full in the fork. The fork's own pass
  makes deltas again.
- **Storage migration and backup** copy raw values within the same namespace; `delta_bases` and
  `delta_cursor` are namespace tables (`NS_TABLES`) and are backed up and restored with the database.

## Deltas are never served

The rule: every path that hands object bytes to a client, a cache tier or another namespace hands a
full, self-contained value (codec `0` or `1`), never codec `2`. A delta exists only as a loose value
inside the bucket of the namespace that has its `delta_bases` row.

How it holds. The only function that returns raw stored bytes is `BucketObjects::get_loose_raw`, and it
is `pub(crate)` to `asc-store`. Its callers are the resolver (`get_loose_stored`, which decodes a delta
against its base and re-encodes the result), the delta code in `delta.rs` (planning, expanding, base
checks) and the pack job. Everything that leaves the crate goes through `get`, `get_many`, `get_stored`
or `get_range`, which resolve.

| Outward path | How it complies |
| --- | --- |
| `GetObjects` (`asc-rpc` server, behind `ServerObjects`) | Reads with `get_stored`, so each value is resolved and verified before it is sent. |
| Fetch, clone and sync clients | They use `GetObjects`. A client also refuses codec `2` (`check_codec`). |
| Git-compat serving and the git projection | Read through `Objects::get`, which returns decoded content. |
| Cache role | It relays `GetObjects` answers from its upstream and stores what it received, so it only ever holds full values. Its own reads of the origin use the same RPC. |
| Pack job | Reads raw values, skips every delta and leaves it loose, so a pack never holds codec `2`. |
| `CopyClosure` between repositories | Uses `get_stored` and `put`, so the destination gets full values and re-hashes them. |

Within-namespace exceptions, where raw bytes move but never outward:

- Storage migration copies a repository's objects, byte for byte, from one bucket to another for the
  same namespace. The `delta_bases` and `delta_cursor` rows stay with that namespace, so the deltas keep
  their bases.
- Backup copies the bucket files and a database snapshot. The snapshot includes `delta_bases`
  (`NS_TABLES`), and a restore brings both back together.

Transient exception: a fork copies raw loose objects into the fork's namespace, in the same bucket by
server-side copy or across buckets through the server. The fork has no `delta_bases` rows, so
`expand_copied` rewrites every copied delta in full before the fork is made ready. Nothing reads the
fork until then.

Tests: `asc-serve/src/delta_tests.rs` makes the pass (or `store_delta`) turn a file, a tree and a chunk
into deltas and then reads them through the real `GetObjects` handler. Each served value must not start
with codec `2`, must pass `check_codec` and must decode to the original bytes under its key. The fork
case is `a_fork_gets_full_values_where_the_source_holds_deltas` in `admin.rs`, and the pack case is
`the_pack_job_leaves_deltas_loose_and_their_bases_may_be_packed` in `delta.rs`.

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
  prints mainline changesets read, side-branch changesets read, candidates, deltas written (and how
  many of them are trees and chunks), objects without gain, skipped objects and stored bytes before
  and after.
- `asc serve admin delta status --repo R | --all` prints the number of deltas, the distinct bases they
  name and the mainline changesets not yet read.
- `asc serve admin delta expand --repo R | --all` rewrites every delta in full and drops its row, under
  the collection lock. The cursors stay, so a later pass does not redo old history.
- Rollback: set `enabled = false`; existing deltas stay readable. Then `delta expand --all` if the
  stored values must be whole again (for example before a downgrade to a release without codec `2`).

## Schema

Migrations `0611_delta_bases` and `0612_delta_kinds` (PostgreSQL and SQLite):

- `delta_bases (ns, file, base, kind, PRIMARY KEY (ns, file))` with index
  `delta_bases_by_base (ns, base)`. `kind` is 0 for a file, 1 for a tree and 2 for a chunk; rows from
  before `0612` are files. Ids of different kinds never collide (each kind hashes under its own
  blake3 key), so `file` stays the key whatever the kind.
- `delta_cursor (ns, ref, pos, PRIMARY KEY (ns, ref))`.

## Tests

- `asc-store/src/codec.rs`: encode and decode, unrelated bases give no delta, the decode cap.
- `asc-store/src/delta.rs`: reads return the full value, a missing base is damage that a put repairs,
  chains stop at cycles and at the depth limit, the pack job leaves deltas loose, a chunk takes only a
  chunk as its base and keys keep their kind.
- `asc-serve/src/delta_tests.rs`: the pass (copies, cursor, size bounds, batch limit, merged side
  branches), a changed directory stored as a tree delta whose base is a GC root and which the
  rollback expands, chunk pairing, row kinds, GC keeps a base
  until its last delta is swept, a purged base expands its dependents and takes no new ones, the admin
  command and the rollback.
- `asc-serve/src/delta_tests.rs`: `GetObjects` serves a file, a tree and a chunk delta in full.
- `asc-serve/src/admin.rs`: a fork holds full values where the source holds deltas.
