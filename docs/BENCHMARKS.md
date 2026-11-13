# Benchmarks: git vs asc

Produced by `cargo run -p asc-bench --release -- --suite quick --release <label>` (see `bench/asc-bench/README.md`).
The gate (SPEC 16.4): asc beats git on clone, worktree add, switch, merge-base, blame and log; it is within 10% on
status, diff and commit; no scenario regresses more than 20% against `bench/baseline.json`.
Each release adds one section below with the git version, the asc version, the machine and the table.

No release has been published yet.

## Import batching and packed objects (dash)

`cargo run -p asc-git --release --example import_bench -- <git-dir> <out-dir> loose|batched|packed`
imports every ref of a bare git repository into a store on a local disk and counts what it wrote. Source:
`Jordan-Hall/dash` (162 MB bare; 7,869 commits, 25,327 blobs, 37,269 trees). Windows 11, NTFS, release build,
64 GB RAM, one run of each (the machine was shared with other builds, so read the times as a ratio).

| mode | what it is | wall | files | bytes | bytes at 4 KiB blocks | git-map calls | peak RSS |
|---|---|---|---|---|---|---|---|
| loose | a write after every commit (`batch_commits = 1`), no packs: the shape before batching | 220 s | 75,957 | 605 MB | 849 MB | 7,870 | 435 MB |
| batched | 500 commits per batch (default), loose objects | 122 s | 75,957 | 605 MB | 849 MB | 17 | 437 MB |
| packed | batched, `[pack] enabled` | 93 s | 17,623 | 610 MB | 678 MB | 17 | 440 MB |

Batching alone halves the wall time (concurrent object writes and one map call per batch instead of per
commit). Packing leaves 18 packs holding 58,371 objects (128 MB) and 17,586 loose objects, the ones over
`pack.max_object` (64 KiB); files on disk fall by 77 % and block-rounded bytes by 20 %. Peak memory does not
change: the importer's caches were already bounded, and the batch buffer (at most 128 MiB) is small next to
them. A sample of every 50th object reads back in all three. On S3 the difference is larger than on a local
disk, because the cost there is a request per object (not measured here).

## Delta storage (dash)

`import_bench ... delta` imports as `batched`, then stores what the server's delta pass would
(docs/design/DELTA-STORAGE.md) over every branch: each modified or renamed file against its previous
version, each new file against up to four files of the same extension and similar size in its
directory, each new chunk of a file over 4 MiB against the old version's chunk at the same place, and
each changed tree (roots and subdirectories) against the same directory's old tree. Where an object has
several candidate bases the smallest delta is kept, and a delta is stored only when it is under half the
stored value. Same source and machine as above; bytes per object kind.

| store | files (`f`) | chunks (`k`) | trees (`t`) | total |
|---|---|---|---|---|
| no deltas (`batched`) | 441 MB | 130 MB | 28.2 MB | 605 MB |
| file deltas, zstd level 3, first-parent pairs | 359 MB | 130 MB | 28.2 MB | 523 MB |
| file deltas, zstd level 9, first-parent pairs | 299 MB | 130 MB | 28.2 MB | 463 MB |
| file, chunk and tree deltas, level 9, first-parent pairs | 299 MB | 113 MB | 7.9 MB | 426 MB |
| all branches, renames and new-file candidates (current) | 113.6 MB | 65.1 MB | 7.9 MB | 192.6 MB |

The git bare repository packs the same history in 161 MB (`size-pack` 165,242 KiB); git packs its trees
in 2.5 MB and its large-file versions in 50 MB. Most of the earlier gap was the bench pairing only
first-parent history (8,289 of the 22,704 file edits), not the delta format.

Deltas written: 19,172 of 23,501 file candidates, 254 of 489 chunk pairs and 17,786 of 25,964 tree
pairs. Planning (reads, decode, encode) took 43 s for files and 84 s for trees; storing took 74 s and
70 s, bound by one file write per delta (4–5 ms each on Windows). One cache of decoded values (64 MiB)
and delta links is shared by the plans of a pass, so a base that an earlier plan read or wrote is not
read again. A sample of every 50th object reads back (deltas resolved) in 2.1 s.

Tried and rejected, against the first-parent run (426 MB): falling back to a shallower base when the best one's chain is
full (432.8 MB, worse: shallow bases are older and less alike), a chain depth of 16 instead of 8
(413.9 MB, −2.7%, and older readers refuse the deeper chains) and storing deltas up to 75% of the value
instead of 50% (422.8 MB, within noise).

Choosing the delta level: over all 22,704 same-path pairs of every commit, the deltas stored 129 MB at
level 3, 56 MB at 6, 24.6 MB at 9, 23.7 MB at 12 and 21.6 MB at 19, with encode times of 5.8 s, 9.9 s,
16.5 s, 37.9 s and 198 s and decode under 1 s at every level. Level 9 is the knee.

## Cold read of a delta chain

`cargo run -p asc-store --example delta_cold_read` stores a file of 4 KiB, 64 KiB and 1 MiB whole
(depth 0), then stores each next version, a 48-byte edit of the one before, as a delta on it, up to
depth 7 (the pass refuses to extend a chain past `MAX_DELTA_DEPTH` = 8 links, so depth 7 is the
deepest the example can build). An in-memory object store is wrapped in a decorator that really sleeps
20 ms before every request and counts requests and bytes returned. Each read uses a fresh
`BucketObjects`, so nothing is cached in process; the table is the median of five reads of the top of
the chain. The example prints every depth from 0 to 7; this table shows depths 0, 1, 3 and 7. The
time is wall time and includes decode and verification, and the Windows timer rounds each sleep up, so
the 1 request rows read 11 to 22 ms above 20 ms.

| size | depth | requests | bytes fetched | time at 20 ms |
|---|---|---|---|---|
| 4 KiB | 0 | 1 | 1,356 | 32 ms |
| 4 KiB | 1 | 2 | 1,428 | 64 ms |
| 4 KiB | 3 | 4 | 1,585 | 125 ms |
| 4 KiB | 7 | 8 | 1,887 | 251 ms |
| 64 KiB | 0 | 1 | 18,598 | 31 ms |
| 64 KiB | 1 | 2 | 18,674 | 62 ms |
| 64 KiB | 3 | 4 | 18,846 | 124 ms |
| 64 KiB | 7 | 8 | 19,173 | 232 ms |
| 1 MiB | 0 | 1 | 285,036 | 42 ms |
| 1 MiB | 1 | 2 | 285,218 | 95 ms |
| 1 MiB | 3 | 4 | 285,601 | 206 ms |
| 1 MiB | 7 | 8 | 286,363 | 430 ms |

A full value is one request. A chain of depth N is N + 1 requests, one per link, made one after the
other because the base is only known once the delta is read, so a cold read costs N + 1 round trips
and the bytes fetched barely change (the deltas are small). A depth-seven chain took 232 to 430 ms where
the full value took 31 to 42 ms, about eight times as long.

For a cold checkout that reads many files one after another from a remote store, every chained file
adds up to seven round trips: at 20 ms, 1,000 files at depth seven is about 160 s of waiting that
parallel reads or a local cache must hide. Deltas are cheap in space and costly in latency, so the
decision to enable them should weigh this against the saving above.

