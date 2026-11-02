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

