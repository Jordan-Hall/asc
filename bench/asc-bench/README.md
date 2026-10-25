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

Commit-cloud scenarios (PLAN B25) live in `bench/scenarios/branchless.toml`: commit latency with the daemon
uploading (cloud on against off), time from commit to visible on the server (p50 < 2 s, p95 < 10 s) and push of
an already uploaded stack. They run with `--only branchless` (or whenever `ASC_BENCH_REMOTE` is set). The last two
need `ASC_BENCH_REMOTE`, an https or ssh URL without a password, and a running asc daemon; otherwise they fail the
gate as unsupported (skipped with `--allow-unsupported`). `--update-baseline` stores them as `cloud/<name>`.

Not yet scenarios: push/fetch, CI checkout plus affected, turbo `--dry` (need the remote/CI wiring of
B25 and U62 consumers), the medium and HUGE workloads (nightly, `--suite full` adds 100x), peak RSS,
server CPU and cold/warm cache columns. `bench/git-baseline` (BENCH0/BENCH1) stays as the node harness.
