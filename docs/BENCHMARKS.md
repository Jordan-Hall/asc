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

