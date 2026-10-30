# Packed objects

Status: built behind `[pack] enabled` (off by default) and `[import]` batching (on). The sections below
are the design; "As built" at the end lists where the code differs from it and how to run it.

## Why

asc stores one stored value per object: `f/ab/abcd...`, `t/..`, `c/..`. A git import of a 163 MB
repository (Jordan-Hall/dash, public) made about 93,000 files on disk, 124,000 of them under 1 KiB
across two imports, 1.8 GB on disk and an asc peak RSS of 1.1 GB. A local file system rounds every
small file up to a block (4 KiB or 64 KiB); an S3 bucket charges a request, a key and a round trip for
every one; `has_many` is one `HEAD` per key; a GC sweep lists and deletes key by key. The number of
objects, not their bytes, is the cost. Trees, changesets, change metadata and most source files are a
few hundred bytes.

Goal: small objects live many to a file, large objects stay as they are, every reader keeps working
without knowing which form an object is in, and a repository can move between the forms at any time.

Non-goals: changing object identity, the codec byte, verification, the key space or the SQL schema of
refs. A pack is a storage detail below `Objects`.

## What is packed

Only objects whose stored value (`codec || data`) is at most `pack.max_object` bytes, default 64 KiB,
and only classes `File`, `Tree`, `Changeset`, `Tag`, `GitTreeOverride`, `ChangeMeta`, `ChunkList`.
`Chunk` objects are 64 KiB to 1 MiB and stay loose; `Derived` values are recomputable, small, many and
rewritten, and stay loose (they may get their own pack kind later). Anything larger stays loose.

## The pack file

Per repository namespace, under the namespace root: `pack/<id>.pack`, with `<id>` a 32-hex random id.
A pack is immutable once written; it is never appended to and never edited.

```
header   : "ASCPACK1" (8 bytes) | count u32 LE | reserved u32 (0)
entries  : repeated  [ key class u8 | key id 32 bytes (20 for GitTreeOverride, zero padded) |
                       stored length u32 LE | stored bytes (codec byte || data) ]
trailer  : blake3 over everything before it (32 bytes)
```

An entry is exactly what `put` is given, so the bytes read back are the loose bytes: the codec byte, the
zstd frame, `verify` against the key. The trailer lets a reader or `asc serve admin verify` detect a
damaged pack without parsing it. The size of a pack is at most `pack.max_size`, default 64 MiB, or
`pack.max_objects`, default 100,000. Entries are sorted by (class, id) so a pack is deterministic for a
given set of objects and two jobs that pack the same set make the same file.

The pack carries no index of its own that a reader must trust: the SQL index below is the index, and the
entry header (class, id, length) is repeated in the pack so that the index can be rebuilt from the packs
alone (`asc serve admin pack reindex`).

## The index

A table in the server database (the bucket mode keeps the same rows in `pack/<id>.idx`, one sorted file
per pack, see "Bucket mode"):

```sql
CREATE TABLE pack_objects (
    ns     BLOB   NOT NULL,
    class  BIGINT NOT NULL,      -- KeyClass as a number
    id     BLOB   NOT NULL,      -- the key bytes
    pack   BLOB   NOT NULL,      -- 16 bytes of the pack id
    offset BIGINT NOT NULL,      -- of the stored bytes, after the entry header
    len    BIGINT NOT NULL,      -- length of the stored bytes
    PRIMARY KEY (ns, class, id)
);
CREATE TABLE packs (
    ns      BLOB   NOT NULL,
    pack    BLOB   NOT NULL,
    objects BIGINT NOT NULL,
    bytes   BIGINT NOT NULL,
    created BIGINT NOT NULL,
    state   BIGINT NOT NULL,     -- 0 writing, 1 live, 2 retired
    PRIMARY KEY (ns, pack)
);
```

Both tables are namespace tables: they are named in `admin.rs::NS_TABLES` so a deleted repository drops
them. Migrations take the next free number at the time (sqlite and postgres).

## Reading

`Objects::get`, `get_stored`, `get_range` and `has_many` of a new `PackedObjects<Inner>` wrapper:

1. `get_stored(key)`: look the key up in `pack_objects`. A row: `GET` the range
   `[offset, offset+len)` of `pack/<id>.pack` (one range request on S3, one `pread` locally), return the
   bytes. No row: ask `Inner` for the loose object as today. A pack that is missing or shorter than the
   row says is `Error::Corrupt`-like: the reader falls back to a loose copy if one exists (a repack
   keeps loose copies until the pack is verified, see "Writing"), else reports the object missing and
   logs the pack id.
2. `get` decodes and verifies as now; `verify(key, data)` is unchanged, so a pack cannot make a wrong
   object look right.
3. `get_range` of a packed object decodes the whole stored value (it is at most 64 KiB) and slices.
4. `has_many(keys)`: one `SELECT ... WHERE (class, id) IN (...)` for the batch, then `Inner::has_many`
   only for the keys the index did not answer. This replaces most `HEAD` storms.
5. A small read-through cache of recently used packs' index pages is not needed: the index is SQL.
   The tiered store's local cache keeps whole packs (see "Tiered store").

Lookups in `pack_objects` cost one indexed read each. Callers that read many objects (tree walks, GC
mark) already batch; `get_many` gets a default implementation that groups keys by pack and reads each
pack once with a single ranged read of the covering span when the keys are close together.

## Writing

Three writers, all using one `PackWriter`.

1. **End of an import.** After the last commit of a batch is stored, the importer calls
   `pack_new(ns, since)`: every loose object written since the import started and small enough is added
   to a new pack. The importer already knows the set it wrote; it passes the keys instead of listing.
2. **Background repack job** (`pack.repack_interval`, default 1 h, one per repository, takes the
   repository's lease so two servers do not run it): lists loose small objects older than
   `pack.min_age` (default 10 min, so a push in flight is not touched), packs them, and merges packs
   smaller than `pack.merge_below` (default 4 MiB) into one when there are at least
   `pack.merge_count` (default 8) of them. Thresholds: a repository with fewer than
   `pack.min_loose` (default 2,000) loose small objects is left alone.
3. **Push ingest does not pack.** A push is latency sensitive; its objects stay loose and the job
   packs them later.

Crash safety. The order is: (a) write `pack/<id>.pack` under a temporary key and move it to its final
key (local: write to `.tmp`, fsync, rename; S3: a single `PUT` is atomic, use `If-None-Match`); (b)
read it back and verify the trailer and a sample of entries; (c) in one SQL transaction insert the
`pack_objects` rows and set `packs.state = 1`; (d) only after the commit, delete the loose copies
(`Inner::delete`, batched), and only those that were older than `pack.min_age`. A crash before (c)
leaves an unreferenced pack file (`state` 0 or no row): the next job deletes packs that have no live row
and are older than `pack.orphan_age` (1 h). A crash between (c) and (d) leaves both copies: readers
prefer the pack, the next job deletes the loose copies. No step makes an object unreadable. If (c) is
rejected for a duplicate row (another writer packed the same object), the row stays and the new pack's
copy is simply unused until a merge drops it.

Concurrency: `pack_objects` rows are only added in (c) and removed when a pack is retired; readers
never see a row whose pack is not durable.

## Garbage collection

GC (`asc-store/src/gc.rs`) marks reachable keys and sweeps unreachable ones older than `min_age`. With
packs:

- The mark phase reads through `Objects::get` and needs no change.
- The sweep lists loose keys as now and deletes them as now. For a packed key it deletes the
  `pack_objects` row (this makes the object unreadable and absent for `has_many`, as a delete does).
  The bytes stay in the pack until the pack is rewritten.
- A pack whose live rows are fewer than `pack.compact_below` (default 50 %) of its entries is
  *compacted* by the repack job: live entries are copied into a new pack (steps a to c), the old pack
  is set to `state = 2` and deleted after `pack.retire_age` (24 h, so a reader that already holds its
  range finishes). A pack with no live rows is retired directly.
- `Purge` (operator purge of an object) does the same as a sweep delete and also schedules the pack for
  compaction at once, because purge means the bytes must go.
- The fence (`Fence`) already excludes concurrent sweeps; the repack job takes the same exclusive lock
  while it deletes loose copies and retires packs, and none while it only writes a new pack.

## Tiered store and cache

`tiered.rs` reads through a local directory cache in front of the bucket. For a packed object the cache
unit is the pack: the first read of any object of a pack fetches the whole pack when it is at most
`pack.cache_fetch_max` (default 8 MiB) and otherwise only the range; the pack is cached under
`pack/<id>.pack`. Cache eviction treats a pack like one object. The loose-object path is unchanged.
`seed.rs` (seeding a cache from a bucket) learns to copy `pack/*.pack` and to rebuild `pack_objects`
from the packs through `reindex`.

## Bucket mode

Bucket mode has no SQL. Each pack gets a sibling `pack/<id>.idx`: a sorted array of fixed-size records
(class u8, id 32, offset u64, len u32) with a blake3 trailer, written before the pack becomes visible
and read with one range request per lookup by binary search over a cached fan-out table (the first
bytes of the id, as git does). `has_many` consults the cached fan-out of every live pack, newest first.
The set of live packs is `pack/MANIFEST`, a small file replaced with a conditional `PUT` (the same
mechanism `BucketRefs` and the `gc/freeze` key use). The server mode and the bucket mode share the
`PackWriter`, the entry format and the tests; only the index is different (`PackIndex` trait with
`SqlPackIndex` and `BucketPackIndex`).

## Migration of existing loose objects

None is forced. A repository with loose objects keeps working: reads fall back to loose. The repack job
packs old loose objects on its schedule. `asc serve admin pack run <repo>` runs the job once;
`asc serve admin pack status <repo>` prints loose small objects, packs, pack bytes and dead bytes;
`asc serve admin pack verify <repo>` checks trailers and the index against the packs;
`asc serve admin pack reindex <repo>` rebuilds `pack_objects` from the packs, which is also the disaster
recovery: losing the SQL rows loses nothing that the packs hold.

## Rollout

`[store.pack] enabled = false` by default for one release. Phases:

1. Ship the reader (`PackedObjects` wrapping the store, index tables, `reindex`), with the writer off. A
   server that has it reads packs; servers of the previous version ignore `pack/` and `pack_objects`
   and keep working on loose objects until a pack exists, so the reader goes to every server first.
2. Enable the end-of-import packing, then the background job, on one repository, then all.
3. Make `enabled = true` the default and remove loose-only paths from the importer's write batches.

A repository that has packs must not be served by a version without the reader: the first pack write
sets `repos.features` (or a row in a `store_features` table) to `pack1`, and an old server refuses to
open a repository with a feature it does not know, as it does for other store features.

## Import: batches and fewer writes (the cheaper part, built first)

Independent of packs and built before them:

- The importer commits to the store in batches of commits (`import.batch_commits`, default 500, and
  `import.batch_bytes`, default 128 MiB of new stored bytes) instead of one transaction for the run: it
  writes the objects of a batch, then in one SQL transaction the graph rows, git map rows and change
  rows of the batch, then the next. A crash leaves complete batches; re-running the import skips what is
  there (`has_many`) and continues.
- Peak memory is bounded by the batch, not by the history: trees and blobs of finished batches are
  dropped from the importer's caches (`CACHE_ITEMS` stays the ceiling).
- `put` of small objects goes through a per-batch buffer that dedups by key and issues `has_many` once
  per batch.

## Tests

- Format: a pack round-trips every class; a flipped byte anywhere is found by the trailer or by
  `verify`; entries sorted; the same set gives the same bytes.
- Reader: loose-only, pack-only and both; a missing pack falls back to loose or reports missing; a
  short pack is detected; `has_many` mixes both; `get_range` on a packed file.
- Writer: crash points (kill after each of steps a to d) leave a readable repository and no
  unreferenced rows; two jobs at once; a duplicate object in two packs.
- GC: sweep removes the row; compaction keeps live bytes; purge schedules compaction; the fence.
- Bucket mode: conditional manifest replace, fan-out lookup, concurrent writers.
- Scale: an import of the 163 MB repository before and after: files on disk, bytes on disk, peak RSS,
  wall time, `has_many` requests; the numbers go in `docs/BENCHMARKS.md`.

## Open questions for the implementer

- Whether `Derived` values deserve a pack kind (they dominate object counts in a repository with code
  intelligence on).
- The right `pack.max_object` for S3: bigger packs mean fewer requests but larger ranged reads for a
  lone object; measure with the 163 MB repository.
- Whether the index in SQL becomes the bottleneck for very large repositories (hundreds of millions of
  rows); the bucket-mode index is the fallback design if so.

## As built

Where the code is: `crates/asc-store/src/pack/` (`format`, `index`, `bucket_index`, `layer`, `job`, `tests`),
`crates/asc-store/src/batch.rs` (the import write buffer), `crates/asc-serve/src/pack_sql.rs` (the SQL index and
the write gate), `crates/asc-serve/src/pack_service.rs` (`[pack]`, `[import]`, the job), migration `0596_packs`.

Differences from the design above, and why:

- **The layer lives inside `BucketObjects`**, not in a `PackedObjects<Inner>` wrapper. `BucketObjects` is a concrete
  type in dozens of signatures; an optional `PackLayer` field gives every reader and writer of a namespace the
  packs without changing one of them. `BucketObjects::from_url` and `local` attach the bucket-mode index (reading
  only), so every bucket-mode tool reads packs.
- **A bulk write packs directly.** `Objects::put_many` (new; the default puts concurrently) writes the small objects
  of one call as one pack when `[pack] enabled`, instead of writing loose files and packing them at the end of the
  import. The importer calls it once per batch. The index commit still follows the durable pack file.
- **Import batching** is a `BatchedObjects` buffer in the importer: a batch's objects are flushed first, then its
  git-map and change rows in one call each (`[import] batch_commits` = 500, `batch_bytes` = 128 MiB; `1` is a write
  after every commit). `SqlGitMap::put` is one multi-row statement per 500 rows. Objects are durable before any row
  names them, so a crash re-imports the last batch and produces identical bytes.
- **Index lookups are cheap for repositories without packs.** "No packs here" is trusted for `negative_ttl` (5 s), so
  a read costs no query and no pooled connection. The cost is a window in which a server that has not yet noticed the
  first pack would miss an object that exists only in a pack; so the first pack of a namespace keeps the loose
  copies for `negative_ttl`, and bulk writes pack directly only once this process has known of packs that long.
- **Bucket mode index.** `pack/<id>.idx` is read whole once and cached (not a ranged binary search); the manifest is
  cached for 2 s. A GC delete writes `pack/<id>.del`, the sorted keys that pack no longer holds. A store without
  conditional writes (a local directory) gets a plain manifest write.
- **GC.** `list` includes packed keys (with their pack's time, so packing restarts an object's age: conservative), and
  `delete` forgets the row. A sweep that purged objects compacts the packs at once (`pack_compact_all`).
- **Features.** The first pack write records `pack1` in `store_features`; opening a namespace that lists a feature
  this build does not know is refused. Older builds do not read the table: the reader ships first, as above.
- **Not done:** whole-pack caching in the tiered store (a packed object is fetched by range, as the design's fallback
  says), and a packed form for `Derived` values.

Operation: `[pack] enabled = true` turns on the job (every `repack_interval`) and the direct bulk packing;
`asc serve admin pack run|status|verify|reindex --repo R | --all` do it by hand. Rollback: set `enabled = false`;
packs stay readable, nothing new is packed, and `pack run` still deletes retired packs.

