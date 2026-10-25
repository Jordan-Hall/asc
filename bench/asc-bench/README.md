# asc-bench (PLAN O05)

Runs git and asc on the same scenarios against deterministic synthetic repositories
(`src/synth.rs`, the `bench/gen` generator), prints a table and exits non-zero when the SPEC 16.4 gate fails.

    cargo build -p asc --release
    ASC_BIN=target/release/asc cargo run -p asc-bench --release -- --suite quick
    cargo run -p asc-bench --release -- --suite quick --only merge-base

Gate: asc must be faster at the Scion-model scenarios (clone-local, worktree-add, switch, merge-base,
blame, log-path), within 10% elsewhere (status, diff, commit) and within 20% of `bench/baseline.json`.
Differences under 5 ms do not count for the percentage gates. An asc command that is not implemented yet
fails the gate unless `--allow-unsupported` is given (local runs only). The asc binary comes from `--asc`,
`ASC_BIN` or the executable next to `asc-bench`; it is never searched on PATH.

`--update-baseline` rewrites the baseline from the run. `--release v1.0 ` prints the `docs/BENCHMARKS.md` section.

Not yet scenarios: push/fetch, CI checkout plus affected, turbo `--dry` (need the remote/CI wiring of
B25 and U62 consumers), the medium and HUGE workloads (nightly, `--suite full` adds 100x), peak RSS,
server CPU and cold/warm cache columns. `bench/git-baseline` (BENCH0/BENCH1) stays as the node harness.
