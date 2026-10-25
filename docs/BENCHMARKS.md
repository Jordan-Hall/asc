# Benchmarks: git vs asc

Produced by `cargo run -p asc-bench --release -- --suite quick --release <label>` (see `bench/asc-bench/README.md`).
The gate (SPEC 16.4): asc beats git on clone, worktree add, switch, merge-base, blame and log; it is within 10% on
status, diff and commit; no scenario regresses more than 20% against `bench/baseline.json`.
Each release adds one section below with the git version, the asc version, the machine and the table.

No release has been published yet.
